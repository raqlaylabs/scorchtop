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

    let mut projects: std::collections::BTreeMap<String, (String, Totals)> = Default::default();
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
        let (name, totals) = projects.entry(key.project.clone()).or_default();
        if !entry.display_name.is_empty() {
            *name = entry.display_name.clone();
        }
        totals.add_entry(entry);
        models.entry(key.model.clone()).or_default().add_entry(entry);
    }
    vm.active_days = days.len();

    vm.projects = projects
        .into_values()
        .map(|(name, t)| ProjectRow {
            name,
            tokens: t.tokens.total(),
            est_cost: cost_of(&t),
            frac: 0.0,
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
        let vm = view(&cube, Period::Day, day("2026-07-06"));
        assert_eq!(vm.projects[0].name, "big");
        assert!((vm.projects[0].frac - 1.0).abs() < 1e-9);
        assert!((vm.projects[1].frac - 0.25).abs() < 1e-9);
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
    fn formats_token_counts() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(4_560_000), "4.6M");
        assert_eq!(fmt_tokens(2_100_000_000), "2.1B");
    }
}
