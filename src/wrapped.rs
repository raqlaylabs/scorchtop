//! `scorchtop wrapped` — pure view-model for the monthly shareable summary.
//! Everything the wrapped screen shows (totals, top projects, biggest
//! session, heatmap cells, blur pseudonyms) is computed here and unit-tested;
//! the rendering code just paints the result.

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, Days, Local, NaiveDate};

use crate::aggregate::{Cube, Totals};
use crate::pricing::pricing_for;
use crate::source::UsageRecord;

/// Calendar month in the local timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Month {
    pub year: i32,
    /// 1-based, like `chrono`.
    pub month: u32,
}

impl Month {
    pub fn of(date: NaiveDate) -> Self {
        Self { year: date.year(), month: date.month() }
    }

    pub fn current() -> Self {
        Self::of(Local::now().date_naive())
    }

    pub fn contains(&self, date: NaiveDate) -> bool {
        date.year() == self.year && date.month() == self.month
    }

    pub fn first_day(&self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.year, self.month, 1).expect("valid month")
    }

    pub fn day_count(&self) -> u32 {
        self.next().first_day().pred_opt().expect("not year 0").day()
    }

    pub fn prev(&self) -> Self {
        if self.month == 1 {
            Self { year: self.year - 1, month: 12 }
        } else {
            Self { year: self.year, month: self.month - 1 }
        }
    }

    pub fn next(&self) -> Self {
        if self.month == 12 {
            Self { year: self.year + 1, month: 1 }
        } else {
            Self { year: self.year, month: self.month + 1 }
        }
    }

    /// "July 2026"
    pub fn label(&self) -> String {
        self.first_day().format("%B %Y").to_string()
    }
}

/// Stable screenshot-safe stand-in for a project name, assigned by rank:
/// `project-a` is the month's top project, then `project-b`, … `project-aa`.
pub fn pseudonym(rank: usize) -> String {
    // Bijective base-26 so 26 -> "aa" with no leading-zero digit.
    let mut n = rank + 1;
    let mut letters = Vec::new();
    while n > 0 {
        n -= 1;
        letters.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    letters.reverse();
    format!("project-{}", String::from_utf8(letters).expect("ascii"))
}

#[derive(Debug, Clone)]
pub struct ProjectStat {
    pub name: String,
    /// Blur stand-in, stable within the view (assigned by rank).
    pub pseudonym: String,
    pub tokens: u64,
    pub est_cost: Option<f64>,
    /// Bar length relative to the top project (0.0..=1.0).
    pub frac: f64,
}

impl ProjectStat {
    pub fn display(&self, blur: bool) -> &str {
        if blur {
            &self.pseudonym
        } else {
            &self.name
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStat {
    /// Project display name.
    pub project: String,
    /// Matches the project's pseudonym in `WrappedModel::projects`.
    pub pseudonym: String,
    pub tokens: u64,
    pub est_cost: Option<f64>,
    /// Local day the session started.
    pub date: NaiveDate,
}

impl SessionStat {
    pub fn display(&self, blur: bool) -> &str {
        if blur {
            &self.pseudonym
        } else {
            &self.project
        }
    }
}

/// One day of the GitHub-style heatmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeatCell {
    pub date: NaiveDate,
    pub tokens: u64,
    /// Week column, 0-based from the week containing the 1st.
    pub col: u16,
    /// Weekday row, 0 = Monday .. 6 = Sunday.
    pub row: u16,
    /// Intensity 0..=4: 0 = no usage, 4 = the month's busiest day.
    pub level: u8,
}

#[derive(Debug, Default)]
pub struct WrappedModel {
    pub label: String,
    pub totals: Totals,
    pub active_days: usize,
    /// Projects in the month, most expensive first (ties broken by tokens).
    pub projects: Vec<ProjectStat>,
    pub session_count: usize,
    /// Session with the most tokens. Sessions come from live transcripts
    /// only (history has no session detail), so this is a floor for months
    /// Claude Code has partially pruned.
    pub biggest_session: Option<SessionStat>,
    pub busiest_day: Option<(NaiveDate, u64)>,
    /// One cell per day of the month.
    pub heatmap: Vec<HeatCell>,
}

/// Earliest and latest month with any usage, for month navigation bounds.
pub fn month_bounds(cube: &Cube) -> Option<(Month, Month)> {
    let mut bounds: Option<(Month, Month)> = None;
    for (key, entry) in cube {
        if entry.tokens.total() == 0 {
            continue;
        }
        let m = Month::of(key.date);
        bounds = Some(match bounds {
            None => (m, m),
            Some((lo, hi)) => (lo.min(m), hi.max(m)),
        });
    }
    bounds
}

/// Build the wrapped view for one month from the history-merged cube plus
/// already-deduplicated records (for session stats).
pub fn wrapped(cube: &Cube, records: &[&UsageRecord], month: Month) -> WrappedModel {
    let mut model = WrappedModel { label: month.label(), ..Default::default() };

    type ProjectAcc = (String, Totals);
    let mut projects: BTreeMap<String, ProjectAcc> = BTreeMap::new();
    let mut by_day: BTreeMap<NaiveDate, u64> = BTreeMap::new();

    for (key, entry) in cube {
        if entry.tokens.total() == 0 || !month.contains(key.date) {
            continue;
        }
        model.totals.add_entry(entry);
        *by_day.entry(key.date).or_insert(0) += entry.tokens.total();
        let (name, totals) = projects.entry(key.project.clone()).or_default();
        if !entry.display_name.is_empty() {
            *name = entry.display_name.clone();
        }
        totals.add_entry(entry);
    }
    model.active_days = by_day.len();
    model.busiest_day = by_day.iter().max_by_key(|(_, t)| **t).map(|(d, t)| (*d, *t));
    model.heatmap = heatmap(month, &by_day);

    let mut ranked: Vec<(String, Totals)> = projects
        .into_values()
        .map(|(name, totals)| {
            let name = if name.is_empty() { "?".to_string() } else { name };
            (name, totals)
        })
        .collect();
    // "Most expensive" headline order: known cost, then tokens for unpriced.
    ranked.sort_by(|(_, a), (_, b)| {
        b.known_cost
            .partial_cmp(&a.known_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.tokens.total().cmp(&a.tokens.total()))
    });
    let max_tokens = ranked.iter().map(|(_, t)| t.tokens.total()).max().unwrap_or(0);
    // Pseudonyms are keyed by display name so session stats blur identically.
    let mut alias: HashMap<String, String> = HashMap::new();
    model.projects = ranked
        .into_iter()
        .enumerate()
        .map(|(rank, (name, t))| {
            let pseudonym =
                alias.entry(name.clone()).or_insert_with(|| pseudonym(rank)).clone();
            ProjectStat {
                name,
                pseudonym,
                tokens: t.tokens.total(),
                est_cost: cost_of(&t),
                frac: if max_tokens == 0 {
                    0.0
                } else {
                    t.tokens.total() as f64 / max_tokens as f64
                },
            }
        })
        .collect();

    let sessions = session_stats(records, month, &alias);
    model.session_count = sessions.len();
    model.biggest_session = sessions.into_iter().max_by_key(|s| s.tokens);

    model
}

/// Group month-window records by session id. Records without a session id
/// are skipped (they cannot be attributed).
fn session_stats(
    records: &[&UsageRecord],
    month: Month,
    alias: &HashMap<String, String>,
) -> Vec<SessionStat> {
    #[derive(Default)]
    struct Acc {
        project: String,
        tokens: u64,
        known_cost: f64,
        has_unknown_model: bool,
        started: Option<NaiveDate>,
    }
    let mut by_session: HashMap<&str, Acc> = HashMap::new();
    for r in records {
        let Some(sid) = r.session_id.as_deref() else { continue };
        let date = r.timestamp.with_timezone(&Local).date_naive();
        if !month.contains(date) {
            continue;
        }
        let acc = by_session.entry(sid).or_default();
        acc.tokens += r.usage.total();
        match pricing_for(&r.model) {
            Some(p) => acc.known_cost += p.cost(&r.usage),
            None => acc.has_unknown_model = true,
        }
        acc.started = Some(acc.started.map_or(date, |d| d.min(date)));
        if let Some(name) = r.cwd.as_deref().and_then(|c| c.rsplit('/').find(|s| !s.is_empty()))
        {
            acc.project = name.to_string();
        } else if acc.project.is_empty() {
            acc.project = crate::source::claude_code::display_name_from_key(&r.project_key);
        }
    }
    by_session
        .into_values()
        .filter(|a| a.tokens > 0)
        .map(|a| SessionStat {
            pseudonym: alias.get(&a.project).cloned().unwrap_or_else(|| "project-?".into()),
            project: a.project,
            tokens: a.tokens,
            est_cost: if a.has_unknown_model && a.known_cost == 0.0 {
                None
            } else {
                Some(a.known_cost)
            },
            date: a.started.expect("set on first record"),
        })
        .collect()
}

/// Lay the month out GitHub-style: columns are weeks, rows are weekdays
/// (Monday first). Intensity is relative to the month's busiest day.
pub fn heatmap(month: Month, by_day: &BTreeMap<NaiveDate, u64>) -> Vec<HeatCell> {
    let first = month.first_day();
    let offset = first.weekday().num_days_from_monday();
    let max = by_day.values().max().copied().unwrap_or(0);
    (0..month.day_count())
        .map(|i| {
            let date = first + Days::new(i as u64);
            let tokens = by_day.get(&date).copied().unwrap_or(0);
            let slot = offset + i;
            let level = if tokens == 0 || max == 0 {
                0
            } else {
                (tokens as f64 / max as f64 * 4.0).ceil() as u8
            };
            HeatCell { date, tokens, col: (slot / 7) as u16, row: (slot % 7) as u16, level }
        })
        .collect()
}

fn cost_of(t: &Totals) -> Option<f64> {
    if t.has_unknown_model && t.known_cost == 0.0 {
        None
    } else {
        Some(t.known_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::{CubeEntry, CubeKey};
    use crate::source::TokenUsage;
    use chrono::{TimeZone, Utc};

    const JULY: Month = Month { year: 2026, month: 7 };

    fn insert(cube: &mut Cube, project: &str, date: &str, model: &str, input: u64, cost: Option<f64>) {
        cube.insert(
            CubeKey { project: project.into(), date: date.parse().unwrap(), model: model.into() },
            CubeEntry {
                display_name: project.trim_start_matches('-').into(),
                tokens: TokenUsage { input, output: 0, cache_create: 0, cache_read: 0 },
                records: 1,
                est_cost: cost,
            },
        );
    }

    fn record(session: &str, project: &str, day: u32, input: u64) -> UsageRecord {
        UsageRecord {
            project_key: format!("-u-{project}"),
            cwd: Some(format!("/u/{project}")),
            session_id: Some(session.into()),
            // Noon UTC stays on the same local calendar day in every
            // timezone the tests run in.
            timestamp: Utc.with_ymd_and_hms(2026, 7, day, 12, 0, 0).unwrap(),
            model: "claude-opus-4-8".into(),
            message_id: None,
            request_id: None,
            usage: TokenUsage { input, output: 0, cache_create: 0, cache_read: 0 },
            turn: None,
            lines_written: 0,
        }
    }

    #[test]
    fn month_arithmetic() {
        assert_eq!(JULY.day_count(), 31);
        assert_eq!(JULY.prev(), Month { year: 2026, month: 6 });
        assert_eq!(JULY.next(), Month { year: 2026, month: 8 });
        assert_eq!(Month { year: 2026, month: 1 }.prev(), Month { year: 2025, month: 12 });
        assert_eq!(Month { year: 2025, month: 12 }.next(), Month { year: 2026, month: 1 });
        assert_eq!(Month { year: 2024, month: 2 }.day_count(), 29);
        assert_eq!(JULY.label(), "July 2026");
        assert!(JULY.contains("2026-07-31".parse().unwrap()));
        assert!(!JULY.contains("2026-08-01".parse().unwrap()));
    }

    #[test]
    fn pseudonyms_are_stable_and_unbounded() {
        assert_eq!(pseudonym(0), "project-a");
        assert_eq!(pseudonym(25), "project-z");
        assert_eq!(pseudonym(26), "project-aa");
        assert_eq!(pseudonym(27), "project-ab");
    }

    #[test]
    fn month_window_and_totals() {
        let mut cube = Cube::new();
        insert(&mut cube, "-a", "2026-07-01", "m", 100, Some(1.0));
        insert(&mut cube, "-a", "2026-07-15", "m", 200, Some(2.0));
        insert(&mut cube, "-a", "2026-06-30", "m", 999, Some(9.0)); // out of month
        insert(&mut cube, "-b", "2026-07-15", "<synthetic>", 0, None); // hidden

        let model = wrapped(&cube, &[], JULY);
        assert_eq!(model.label, "July 2026");
        assert_eq!(model.totals.tokens.input, 300);
        assert_eq!(model.active_days, 2);
        assert_eq!(model.busiest_day, Some(("2026-07-15".parse().unwrap(), 200)));
        assert_eq!(model.projects.len(), 1, "zero-token project hidden");
    }

    #[test]
    fn projects_ranked_by_cost_with_matching_pseudonyms() {
        let mut cube = Cube::new();
        insert(&mut cube, "-big-tokens", "2026-07-01", "m", 4000, Some(1.0));
        insert(&mut cube, "-expensive", "2026-07-01", "m", 100, Some(5.0));

        let model = wrapped(&cube, &[], JULY);
        assert_eq!(model.projects[0].name, "expensive");
        assert_eq!(model.projects[0].pseudonym, "project-a");
        assert_eq!(model.projects[0].display(true), "project-a");
        assert_eq!(model.projects[0].display(false), "expensive");
        assert_eq!(model.projects[1].pseudonym, "project-b");
        // Bar fractions stay token-relative even under cost ordering.
        assert!((model.projects[1].frac - 1.0).abs() < 1e-9);
        assert!((model.projects[0].frac - 0.025).abs() < 1e-9);
    }

    #[test]
    fn biggest_session_wins_by_tokens_and_blurs_like_its_project() {
        let mut cube = Cube::new();
        insert(&mut cube, "-alpha", "2026-07-01", "claude-opus-4-8", 50_000, Some(9.0));
        insert(&mut cube, "-beta", "2026-07-02", "claude-opus-4-8", 10, Some(0.1));

        let records = [
            record("s1", "alpha", 1, 100),
            record("s1", "alpha", 2, 300),
            record("s2", "beta", 2, 250),
            record("s3", "alpha", 1, 0), // zero-token session hidden
        ];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let model = wrapped(&cube, &refs, JULY);

        assert_eq!(model.session_count, 2);
        let s = model.biggest_session.as_ref().unwrap();
        assert_eq!(s.project, "alpha");
        assert_eq!(s.tokens, 400);
        assert_eq!(s.date, "2026-07-01".parse().unwrap());
        assert!(s.est_cost.unwrap() > 0.0);
        assert_eq!(s.pseudonym, "project-a", "matches the project list alias");
        assert_eq!(s.display(true), "project-a");
    }

    #[test]
    fn sessions_outside_the_month_are_excluded() {
        let records = [record("s1", "alpha", 1, 100)];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let june = Month { year: 2026, month: 6 };
        let model = wrapped(&Cube::new(), &refs, june);
        assert_eq!(model.session_count, 0);
        assert!(model.biggest_session.is_none());
    }

    #[test]
    fn heatmap_lays_out_weeks_as_columns() {
        // July 2026 starts on a Wednesday and ends on a Friday.
        let mut by_day = BTreeMap::new();
        by_day.insert("2026-07-01".parse().unwrap(), 100u64);
        by_day.insert("2026-07-31".parse().unwrap(), 400u64);

        let cells = heatmap(JULY, &by_day);
        assert_eq!(cells.len(), 31);
        assert_eq!((cells[0].col, cells[0].row), (0, 2), "Jul 1 is Wednesday");
        assert_eq!((cells[30].col, cells[30].row), (4, 4), "Jul 31 is Friday");
        assert_eq!(cells[0].level, 1, "quarter of max rounds up to lowest hot level");
        assert_eq!(cells[30].level, 4, "busiest day is hottest");
        assert_eq!(cells[10].level, 0, "no usage stays cold");
        assert!(cells.iter().all(|c| c.col <= 4 && c.row <= 6));
    }

    #[test]
    fn month_bounds_ignore_zero_token_entries() {
        let mut cube = Cube::new();
        assert!(month_bounds(&cube).is_none());
        insert(&mut cube, "-a", "2026-05-10", "m", 100, Some(1.0));
        insert(&mut cube, "-a", "2026-07-01", "m", 100, Some(1.0));
        insert(&mut cube, "-a", "2026-09-01", "<synthetic>", 0, None);
        let (lo, hi) = month_bounds(&cube).unwrap();
        assert_eq!(lo, Month { year: 2026, month: 5 });
        assert_eq!(hi, JULY);
    }
}
