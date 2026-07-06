//! Dedup + aggregation. Pure functions over `UsageRecord`s — the UI renders
//! the output of this module and contains no business logic of its own.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use chrono::{Local, NaiveDate};

use crate::pricing::pricing_for;
use crate::source::{TokenUsage, UsageRecord};

/// Rolled-up usage for one bucket (a day, a project, a model, or the total).
#[derive(Debug, Default, Clone)]
pub struct Totals {
    pub tokens: TokenUsage,
    /// Sum of est. API value over records whose model has a known price.
    pub known_cost: f64,
    /// True when at least one record's model had no pricing entry —
    /// display cost as `—`-suffixed / approximate, never guess.
    pub has_unknown_model: bool,
    pub records: u64,
}

impl Totals {
    fn add(&mut self, usage: &TokenUsage, cost: Option<f64>) {
        self.tokens.add(usage);
        self.records += 1;
        match cost {
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

/// Deduplicate on (`message.id`, `requestId`) and roll up by day (local
/// timezone), project, and model. Records missing either id are kept as-is
/// (a dedup key exists only when both are present).
///
/// Streaming writes the same message several times with *growing*
/// `output_tokens`; the final occurrence carries the true count, so among
/// duplicates the record with the largest output wins.
pub fn aggregate<'a>(records: impl IntoIterator<Item = &'a UsageRecord>) -> Aggregates {
    let mut agg = Aggregates::default();
    let mut keyed: HashMap<(String, String), &UsageRecord> = HashMap::new();
    let mut survivors: Vec<&UsageRecord> = Vec::new();

    for record in records {
        if let (Some(mid), Some(rid)) = (&record.message_id, &record.request_id) {
            match keyed.entry((mid.clone(), rid.clone())) {
                Entry::Occupied(mut e) => {
                    agg.duplicates_skipped += 1;
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

    for record in survivors {
        let cost = pricing_for(&record.model).map(|p| p.cost(&record.usage));
        let day = record.timestamp.with_timezone(&Local).date_naive();

        agg.totals.add(&record.usage, cost);
        agg.by_day.entry(day).or_default().add(&record.usage, cost);
        agg.by_model
            .entry(record.model.clone())
            .or_default()
            .add(&record.usage, cost);

        let project = agg
            .by_project
            .entry(record.project_key.clone())
            .or_default();
        project.totals.add(&record.usage, cost);
        // Prefer the real cwd's last component for display; fall back to
        // decoding the directory-name key.
        if let Some(name) = record
            .cwd
            .as_deref()
            .and_then(|c| c.rsplit('/').find(|s| !s.is_empty()))
        {
            project.display_name = name.to_string();
        } else if project.display_name.is_empty() {
            project.display_name =
                crate::source::claude_code::display_name_from_key(&record.project_key);
        }
    }

    agg
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
}
