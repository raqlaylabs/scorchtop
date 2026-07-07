//! Pure view-model layer: turns the cube into exactly what the dashboard
//! draws. All period filtering, sorting, and formatting decisions live here
//! (unit-tested); the rendering code just paints the result.

use chrono::NaiveDate;

use crate::aggregate::{Cube, Totals};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    pub fn label(&self) -> &'static str {
        match self {
            Period::Day => "today",
            Period::Week => "last 7 days",
            Period::Month => "last 30 days",
        }
    }

    /// First day included in the window ending at `today`.
    fn start(&self, today: NaiveDate) -> NaiveDate {
        let days = match self {
            Period::Day => 0,
            Period::Week => 6,
            Period::Month => 29,
        };
        today - chrono::Days::new(days)
    }
}

#[derive(Debug, Clone)]
pub struct ProjectRow {
    pub name: String,
    pub tokens: u64,
    pub est_cost: Option<f64>,
    /// Bar length relative to the largest project in the view (0.0..=1.0).
    pub frac: f64,
    /// Per-model tokens within this project, sorted descending.
    pub models: Vec<(String, u64)>,
}

#[derive(Debug, Clone)]
pub struct ModelRow {
    pub name: String,
    pub tokens: u64,
    pub est_cost: Option<f64>,
}

#[derive(Debug, Default)]
pub struct ViewModel {
    pub period_label: &'static str,
    /// Totals over the selected period.
    pub period: Totals,
    /// Today's totals, independent of the selected period (header stat).
    pub today: Totals,
    /// Projects in the period, sorted by tokens descending.
    pub projects: Vec<ProjectRow>,
    /// Models in the period, sorted by tokens descending.
    pub models: Vec<ModelRow>,
    /// Days with any usage in the period.
    pub active_days: usize,
}

/// Build the dashboard view for a period ending at `today` (injected for
/// testability). Zero-token buckets (e.g. `<synthetic>` records) are hidden.
pub fn view(cube: &Cube, period: Period, today: NaiveDate) -> ViewModel {
    let start = period.start(today);
    let mut vm = ViewModel {
        period_label: period.label(),
        ..Default::default()
    };

    type ProjectAcc = (String, Totals, std::collections::BTreeMap<String, u64>);
    let mut projects: std::collections::BTreeMap<String, ProjectAcc> = Default::default();
    let mut models: std::collections::BTreeMap<String, Totals> = Default::default();
    let mut days: std::collections::BTreeSet<NaiveDate> = Default::default();

    for (key, entry) in cube {
        if entry.tokens.total() == 0 {
            continue;
        }
        if key.date == today {
            vm.today.add_entry(entry);
        }
        if key.date < start || key.date > today {
            continue;
        }
        vm.period.add_entry(entry);
        days.insert(key.date);
        let (name, totals, by_model) = projects.entry(key.project.clone()).or_default();
        if !entry.display_name.is_empty() {
            *name = entry.display_name.clone();
        }
        totals.add_entry(entry);
        *by_model.entry(key.model.clone()).or_insert(0) += entry.tokens.total();
        models.entry(key.model.clone()).or_default().add_entry(entry);
    }
    vm.active_days = days.len();

    vm.projects = projects
        .into_values()
        .map(|(name, t, by_model)| {
            let mut models: Vec<(String, u64)> = by_model.into_iter().collect();
            models.sort_by_key(|(_, tokens)| std::cmp::Reverse(*tokens));
            ProjectRow {
                name,
                tokens: t.tokens.total(),
                est_cost: cost_of(&t),
                frac: 0.0,
                models,
            }
        })
        .collect();
    vm.projects.sort_by_key(|p| std::cmp::Reverse(p.tokens));
    let max = vm.projects.first().map_or(0, |p| p.tokens);
    for p in &mut vm.projects {
        p.frac = if max == 0 { 0.0 } else { p.tokens as f64 / max as f64 };
    }

    vm.models = models
        .into_iter()
        .map(|(name, t)| ModelRow { name, tokens: t.tokens.total(), est_cost: cost_of(&t) })
        .collect();
    vm.models.sort_by_key(|m| std::cmp::Reverse(m.tokens));

    vm
}

/// Cost to display for a rollup: `None` (render `—`) only when nothing in the
/// bucket is priced; a partially priced bucket shows the known part.
fn cost_of(t: &Totals) -> Option<f64> {
    if t.has_unknown_model && t.known_cost == 0.0 {
        None
    } else {
        Some(t.known_cost)
    }
}

/// Live activity derived from recent records and per-project write times.
#[derive(Debug, Default)]
pub struct LiveStats {
    /// Total tokens per minute over the last hour, oldest bucket first.
    pub minute_tokens: Vec<u64>,
    /// Average tokens/min over the last 5 minutes.
    pub burn_per_min: f64,
    /// Projects (display names) whose files were written recently.
    pub active_projects: Vec<String>,
    pub is_active: bool,
}

/// A session mid-task can go minutes between writes (one long tool run emits
/// nothing until it finishes), so "live" tolerates a 3-minute silence.
const ACTIVE_WINDOW_SECS: i64 = 180;

/// Compute sparkline/burn from records of roughly the last hour, and the
/// active set from per-project last-write times (file mtimes — see
/// `ProjectActivity`). `now` is injected for testability.
pub fn live_stats(
    recent: &[crate::source::UsageRecord],
    activity: &[crate::watch::ProjectActivity],
    now: chrono::DateTime<chrono::Utc>,
) -> LiveStats {
    let mut stats = LiveStats { minute_tokens: vec![0; 60], ..Default::default() };

    for r in recent {
        let age_min = (now - r.timestamp).num_seconds() as f64 / 60.0;
        if (0.0..60.0).contains(&age_min) {
            let bucket = 59 - age_min.floor() as usize;
            stats.minute_tokens[bucket] += r.usage.total();
        }
    }

    let active_cutoff = now - chrono::Duration::seconds(ACTIVE_WINDOW_SECS);
    let mut active: std::collections::BTreeSet<String> = Default::default();
    for a in activity {
        if a.last_write >= active_cutoff {
            active.insert(a.name.clone());
        }
    }

    let last5: u64 = stats.minute_tokens[55..].iter().sum();
    stats.burn_per_min = last5 as f64 / 5.0;
    stats.is_active = !active.is_empty();
    stats.active_projects = active.into_iter().collect();
    stats
}

/// Total tokens per project (display name) over the recent window. The UI
/// diffs consecutive results to feed the equalizer's animation energy.
pub fn recent_project_totals(
    recent: &[crate::source::UsageRecord],
) -> std::collections::HashMap<String, u64> {
    let mut totals = std::collections::HashMap::new();
    for r in recent {
        let name = r
            .cwd
            .as_deref()
            .and_then(|c| c.rsplit('/').find(|s| !s.is_empty()))
            .map(str::to_string)
            .unwrap_or_else(|| crate::source::claude_code::display_name_from_key(&r.project_key));
        *totals.entry(name).or_insert(0) += r.usage.total();
    }
    totals
}

/// "12.3M", "986k", "1.2B" — compact token counts for tight columns.
pub fn fmt_tokens(n: u64) -> String {
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.1}B", f / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", f / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", f / 1e3)
    } else {
        n.to_string()
    }
}

/// Est. API value column: `$12.34` or `—` for unpriced.
pub fn fmt_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${c:.2}"),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::{CubeEntry, CubeKey};
    use crate::source::TokenUsage;

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

    fn day(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn period_windows_filter_by_date() {
        let mut cube = Cube::new();
        insert(&mut cube, "-a", "2026-07-06", "m", 100, Some(1.0)); // today
        insert(&mut cube, "-a", "2026-07-01", "m", 200, Some(2.0)); // 5 days ago
        insert(&mut cube, "-a", "2026-06-10", "m", 400, Some(4.0)); // 26 days ago

        let today = day("2026-07-06");
        assert_eq!(view(&cube, Period::Day, today).period.tokens.input, 100);
        assert_eq!(view(&cube, Period::Week, today).period.tokens.input, 300);
        assert_eq!(view(&cube, Period::Month, today).period.tokens.input, 700);
        assert_eq!(view(&cube, Period::Month, today).active_days, 3);
    }

    #[test]
    fn today_stat_is_independent_of_period() {
        let mut cube = Cube::new();
        insert(&mut cube, "-a", "2026-07-06", "m", 100, Some(1.0));
        insert(&mut cube, "-a", "2026-07-01", "m", 200, Some(2.0));
        let vm = view(&cube, Period::Month, day("2026-07-06"));
        assert_eq!(vm.today.tokens.input, 100);
        assert_eq!(vm.period.tokens.input, 300);
    }

    #[test]
    fn projects_sorted_desc_with_fractions() {
        let mut cube = Cube::new();
        insert(&mut cube, "-small", "2026-07-06", "m", 100, Some(1.0));
        insert(&mut cube, "-big", "2026-07-06", "m", 400, Some(4.0));
        insert(&mut cube, "-big", "2026-07-06", "m2", 100, Some(1.0));
        let vm = view(&cube, Period::Day, day("2026-07-06"));
        assert_eq!(vm.projects[0].name, "big");
        assert!((vm.projects[0].frac - 1.0).abs() < 1e-9);
        assert!((vm.projects[1].frac - 0.2).abs() < 1e-9);
        // Per-project model breakdown, sorted by tokens descending.
        assert_eq!(
            vm.projects[0].models,
            vec![("m".to_string(), 400), ("m2".to_string(), 100)]
        );
    }

    #[test]
    fn zero_token_buckets_are_hidden() {
        let mut cube = Cube::new();
        insert(&mut cube, "-a", "2026-07-06", "<synthetic>", 0, None);
        insert(&mut cube, "-a", "2026-07-06", "m", 100, Some(1.0));
        let vm = view(&cube, Period::Day, day("2026-07-06"));
        assert_eq!(vm.models.len(), 1);
        assert!(!vm.period.has_unknown_model, "zero-token unknown must not taint cost");
    }

    #[test]
    fn unpriced_cost_renders_as_dash() {
        let mut cube = Cube::new();
        insert(&mut cube, "-a", "2026-07-06", "mystery", 100, None);
        let vm = view(&cube, Period::Day, day("2026-07-06"));
        assert_eq!(vm.projects[0].est_cost, None);
        assert_eq!(fmt_cost(vm.projects[0].est_cost), "—");
    }

    #[test]
    fn live_stats_buckets_by_minute_and_flags_activity() {
        use crate::source::UsageRecord;
        use chrono::{TimeZone, Utc};

        let now = Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 0).unwrap();
        let rec = |mins_ago: i64, total: u64| UsageRecord {
            project_key: "-u-alpha".into(),
            cwd: Some("/u/alpha".into()),
            session_id: None,
            timestamp: now - chrono::Duration::minutes(mins_ago),
            model: "m".into(),
            message_id: None,
            request_id: None,
            usage: TokenUsage { input: total, output: 0, cache_create: 0, cache_read: 0 },
        };

        let act = |name: &str, secs_ago: i64| crate::watch::ProjectActivity {
            name: name.into(),
            last_write: now - chrono::Duration::seconds(secs_ago),
        };

        let records = vec![rec(1, 500), rec(30, 200), rec(90, 999)];
        // alpha wrote 170s ago (inside the 3-min window even though its last
        // usage record could be older); beta went quiet 4 min ago.
        let activity = vec![act("alpha", 170), act("beta", 240)];
        let stats = live_stats(&records, &activity, now);

        assert_eq!(stats.minute_tokens.len(), 60);
        assert_eq!(stats.minute_tokens[58], 500); // 1 min ago
        assert_eq!(stats.minute_tokens[29], 200); // 30 min ago
        assert_eq!(stats.minute_tokens.iter().sum::<u64>(), 700); // 90-min-old excluded
        assert!((stats.burn_per_min - 100.0).abs() < 1e-9); // 500 over last 5 min
        assert!(stats.is_active);
        assert_eq!(stats.active_projects, vec!["alpha".to_string()]);

        let idle = live_stats(&[rec(30, 200)], &[act("alpha", 240)], now);
        assert!(!idle.is_active);
        assert!(idle.active_projects.is_empty());

        // recent_project_totals groups by display name from cwd.
        let totals = recent_project_totals(&records);
        assert_eq!(totals.len(), 1);
        assert_eq!(totals["alpha"], 1699);
    }

    #[test]
    fn formats_token_counts() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(4_560_000), "4.6M");
        assert_eq!(fmt_tokens(2_100_000_000), "2.1B");
    }
}
