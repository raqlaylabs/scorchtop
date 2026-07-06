//! Dedup + aggregation. Pure functions over `UsageRecord`s — the UI renders
//! the output of this module and contains no business logic of its own.
//!
//! The core structure is the *cube*: usage keyed by (project, day, model).
//! The history store persists the cube; rollups and period views are derived
//! from it.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use chrono::{Local, NaiveDate};

use crate::pricing::pricing_for;
use crate::source::{TokenUsage, UsageRecord};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CubeKey {
    /// Stable project key (encoded directory name).
    pub project: String,
    /// Local-timezone calendar day.
    pub date: NaiveDate,
    pub model: String,
}

#[derive(Debug, Clone, Default)]
pub struct CubeEntry {
    /// Human-friendly project name (last component of cwd when known).
    pub display_name: String,
    pub tokens: TokenUsage,
    pub records: u64,
    /// Est. API value. `None` when the model has no pricing entry — shown as
    /// `—`, never guessed. Uniform per key since the key includes the model.
    pub est_cost: Option<f64>,
}

/// Usage bucketed by (project, day, model).
pub type Cube = BTreeMap<CubeKey, CubeEntry>;

#[derive(Debug, Default)]
pub struct DedupCube {
    pub cube: Cube,
    pub duplicates_skipped: u64,
}

/// Deduplicate on (`message.id`, `requestId`) and bucket by project, local
/// day, and model. Records missing either id are kept as-is (a dedup key
/// exists only when both are present).
///
/// Streaming writes the same message several times with *growing*
/// `output_tokens`; the final occurrence carries the true count, so among
/// duplicates the record with the largest output wins.
pub fn build_cube<'a>(records: impl IntoIterator<Item = &'a UsageRecord>) -> DedupCube {
    let mut keyed: HashMap<(String, String), &UsageRecord> = HashMap::new();
    let mut survivors: Vec<&UsageRecord> = Vec::new();
    let mut duplicates_skipped = 0;

    for record in records {
        if let (Some(mid), Some(rid)) = (&record.message_id, &record.request_id) {
            match keyed.entry((mid.clone(), rid.clone())) {
                Entry::Occupied(mut e) => {
                    duplicates_skipped += 1;
                    if record.usage.output > e.get().usage.output {
                        e.insert(record);
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(record);
                }
            }
        } else {
            survivors.push(record);
        }
    }
    survivors.extend(keyed.into_values());

    let mut cube = Cube::new();
    for record in survivors {
        let key = CubeKey {
            project: record.project_key.clone(),
            date: record.timestamp.with_timezone(&Local).date_naive(),
            model: record.model.clone(),
        };
        let pricing = pricing_for(&record.model);
        let entry = cube.entry(key).or_default();
        entry.tokens.add(&record.usage);
        entry.records += 1;
        entry.est_cost = pricing.map(|p| entry.est_cost.unwrap_or(0.0) + p.cost(&record.usage));
        if let Some(name) = record
            .cwd
            .as_deref()
            .and_then(|c| c.rsplit('/').find(|s| !s.is_empty()))
        {
            entry.display_name = name.to_string();
        } else if entry.display_name.is_empty() {
            entry.display_name =
                crate::source::claude_code::display_name_from_key(&record.project_key);
        }
    }

    DedupCube { cube, duplicates_skipped }
}

/// Rolled-up usage for one bucket (a day, a project, a model, or the total).
#[derive(Debug, Default, Clone)]
pub struct Totals {
    pub tokens: TokenUsage,
    /// Sum of est. API value over entries whose model has a known price.
    pub known_cost: f64,
    /// True when at least one entry's model had no pricing — display cost as
    /// incomplete, never guess.
    pub has_unknown_model: bool,
    pub records: u64,
}

impl Totals {
    pub fn add_entry(&mut self, entry: &CubeEntry) {
        self.tokens.add(&entry.tokens);
        self.records += entry.records;
        match entry.est_cost {
            Some(c) => self.known_cost += c,
            None => self.has_unknown_model = true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ProjectTotals {
    pub display_name: String,
    pub totals: Totals,
}

#[derive(Debug, Default)]
pub struct Aggregates {
    pub totals: Totals,
    pub by_day: BTreeMap<NaiveDate, Totals>,
    pub by_project: BTreeMap<String, ProjectTotals>,
    pub by_model: BTreeMap<String, Totals>,
    /// Records dropped as duplicates of an already-seen (message.id, requestId).
    pub duplicates_skipped: u64,
}

/// Derive day/project/model rollups from a cube.
pub fn rollup(cube: &Cube, duplicates_skipped: u64) -> Aggregates {
    let mut agg = Aggregates { duplicates_skipped, ..Default::default() };
    for (key, entry) in cube {
        agg.totals.add_entry(entry);
        agg.by_day.entry(key.date).or_default().add_entry(entry);
        agg.by_model.entry(key.model.clone()).or_default().add_entry(entry);
        let project = agg.by_project.entry(key.project.clone()).or_default();
        project.totals.add_entry(entry);
        if !entry.display_name.is_empty() {
            project.display_name = entry.display_name.clone();
        }
    }
    agg
}

/// Convenience: dedup + bucket + roll up in one call.
pub fn aggregate<'a>(records: impl IntoIterator<Item = &'a UsageRecord>) -> Aggregates {
    let d = build_cube(records);
    rollup(&d.cube, d.duplicates_skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn record(mid: Option<&str>, rid: Option<&str>, input: u64) -> UsageRecord {
        UsageRecord {
            project_key: "-Users-x-proj".into(),
            cwd: Some("/Users/x/proj".into()),
            session_id: Some("s".into()),
            timestamp: Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, 0).unwrap(),
            model: "claude-opus-4-8".into(),
            message_id: mid.map(String::from),
            request_id: rid.map(String::from),
            usage: TokenUsage { input, output: 10, cache_create: 0, cache_read: 0 },
        }
    }

    #[test]
    fn dedups_on_message_and_request_id() {
        let records = vec![
            record(Some("m1"), Some("r1"), 100),
            record(Some("m1"), Some("r1"), 100), // duplicate
            record(Some("m1"), Some("r2"), 100), // same msg, different request: kept
        ];
        let agg = aggregate(&records);
        assert_eq!(agg.duplicates_skipped, 1);
        assert_eq!(agg.totals.tokens.input, 200);
    }

    #[test]
    fn dedup_keeps_the_largest_output_among_duplicates() {
        // Streaming writes the same message with growing output_tokens;
        // the final (largest) value is the true count.
        let mut early = record(Some("m1"), Some("r1"), 100);
        early.usage.output = 1;
        let mut done = record(Some("m1"), Some("r1"), 100);
        done.usage.output = 257;
        let records = vec![early, done];
        let agg = aggregate(&records);
        assert_eq!(agg.duplicates_skipped, 1);
        assert_eq!(agg.totals.tokens.output, 257);
    }

    #[test]
    fn records_without_ids_are_never_deduped() {
        let records = vec![record(None, None, 100), record(None, None, 100)];
        let agg = aggregate(&records);
        assert_eq!(agg.duplicates_skipped, 0);
        assert_eq!(agg.totals.tokens.input, 200);
    }

    #[test]
    fn unknown_model_counts_tokens_but_flags_cost() {
        let mut r = record(Some("m1"), Some("r1"), 100);
        r.model = "mystery-model-9".into();
        let agg = aggregate(std::iter::once(&r));
        assert_eq!(agg.totals.tokens.input, 100);
        assert!(agg.totals.has_unknown_model);
        assert_eq!(agg.totals.known_cost, 0.0);
    }

    #[test]
    fn buckets_by_local_day_project_model() {
        let records = vec![record(Some("m1"), Some("r1"), 100)];
        let agg = aggregate(&records);
        assert_eq!(agg.by_day.len(), 1);
        assert_eq!(agg.by_project.len(), 1);
        assert_eq!(agg.by_model.len(), 1);
        let project = agg.by_project.values().next().unwrap();
        assert_eq!(project.display_name, "proj");
        assert!(agg.totals.known_cost > 0.0);
    }

    #[test]
    fn cube_accumulates_cost_within_a_key() {
        let records = vec![
            record(Some("m1"), Some("r1"), 100),
            record(Some("m2"), Some("r2"), 300),
        ];
        let d = build_cube(&records);
        assert_eq!(d.cube.len(), 1); // same project/day/model
        let entry = d.cube.values().next().unwrap();
        assert_eq!(entry.records, 2);
        assert_eq!(entry.tokens.input, 400);
        // 400 input * $5/M + 20 output * $25/M
        let expected = (400.0 * 5.0 + 20.0 * 25.0) / 1_000_000.0;
        assert!((entry.est_cost.unwrap() - expected).abs() < 1e-12);
    }
}
