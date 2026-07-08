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

/// Crash guard for the working flag: a session that died mid-turn (so no
/// `end_turn` ever arrives) stops counting as live once its file has been
/// silent this long. Long tool runs gap ~3-4 min, so this stays generous.
const STALE_WORK_SECS: i64 = 600;

/// Compute sparkline/burn from records of roughly the last hour, and the
/// active set from per-project turn state: live = an unfinished turn (see
/// `ProjectActivity::working`) with non-stale writes. `now` is injected for
/// testability.
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

    let stale_cutoff = now - chrono::Duration::seconds(STALE_WORK_SECS);
    let mut active: std::collections::BTreeSet<String> = Default::default();
    for a in activity {
        if a.working && a.last_write >= stale_cutoff {
            active.insert(a.name.clone());
        }
    }

    let last5: u64 = stats.minute_tokens[55..].iter().sum();
    stats.burn_per_min = last5 as f64 / 5.0;
    stats.is_active = !active.is_empty();
    stats.active_projects = active.into_iter().collect();
    stats
}

/// One prompt→reply turn for the turns panel: what a typed prompt cost.
/// `lines_written` is Write/Edit output, deliberately not "surviving code".
#[derive(Debug, Clone)]
pub struct TurnRow {
    pub prompt_chars: u64,
    /// Total tokens of this turn's deduped records (subagent transcripts in
    /// separate files are not attributable, so this is a floor).
    pub tokens: u64,
    pub est_cost: Option<f64>,
    pub lines_written: u64,
    pub started: chrono::DateTime<chrono::Utc>,
    /// Still streaming: unfinished with non-stale writes (crash-guarded).
    pub active: bool,
}

/// Turns of one project, newest first.
#[derive(Debug, Clone)]
pub struct TurnGroup {
    /// Project display name.
    pub project: String,
    pub turns: Vec<TurnRow>,
}

/// Newest turns shown in the panel (across all groups).
pub const TURN_ROW_LIMIT: usize = 15;

/// Join per-file turn metadata with deduplicated records (matched on
/// `UsageRecord::turn`) into display rows: the newest `TURN_ROW_LIMIT` turns,
/// grouped by project. Groups are ordered by their newest turn, turns within
/// a group newest first. Each meta arrives as (project display name, that
/// file's last write time, meta).
pub fn turn_rows(
    turns: &[(String, chrono::DateTime<chrono::Utc>, crate::watch::TurnMeta)],
    records: &[&crate::source::UsageRecord],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<TurnGroup> {
    #[derive(Default)]
    struct Acc {
        tokens: u64,
        known_cost: f64,
        has_unknown_model: bool,
        lines_written: u64,
    }
    let mut by_turn: std::collections::HashMap<u32, Acc> = Default::default();
    for r in records {
        let Some(id) = r.turn else { continue };
        let acc = by_turn.entry(id).or_default();
        acc.tokens += r.usage.total();
        acc.lines_written += r.lines_written;
        match crate::pricing::pricing_for(&r.model) {
            Some(p) => acc.known_cost += p.cost(&r.usage),
            None => acc.has_unknown_model = true,
        }
    }

    let stale_cutoff = now - chrono::Duration::seconds(STALE_WORK_SECS);
    let mut rows: Vec<(u32, &str, TurnRow)> = turns
        .iter()
        .map(|(project, last_write, meta)| {
            let acc = by_turn.remove(&meta.id).unwrap_or_default();
            (meta.id, project.as_str(), TurnRow {
                prompt_chars: meta.prompt_chars,
                tokens: acc.tokens,
                est_cost: if acc.has_unknown_model && acc.known_cost == 0.0 {
                    None
                } else {
                    Some(acc.known_cost)
                },
                lines_written: acc.lines_written,
                started: meta.started,
                active: meta.ended.is_none() && *last_write >= stale_cutoff,
            })
        })
        .collect();
    // Timestamps are second-resolution, so same-second prompts tie; within a
    // file the turn id is chronological and breaks the tie.
    rows.sort_by_key(|(id, _, r)| std::cmp::Reverse((r.started, *id)));
    rows.truncate(TURN_ROW_LIMIT);

    // Group by project, groups ordered by their newest turn. `rows` is
    // already newest-first, so first appearance fixes the group order and
    // pushes keep turns newest-first within each group.
    let mut groups: Vec<TurnGroup> = Vec::new();
    for (_, project, row) in rows {
        match groups.iter_mut().find(|g| g.project == project) {
            Some(g) => g.turns.push(row),
            None => groups.push(TurnGroup { project: project.to_string(), turns: vec![row] }),
        }
    }
    groups
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
            turn: None,
            lines_written: 0,
        };

        let act = |name: &str, working: bool, secs_ago: i64| crate::watch::ProjectActivity {
            name: name.into(),
            working,
            last_write: now - chrono::Duration::seconds(secs_ago),
        };

        let records = vec![rec(1, 500), rec(30, 200), rec(90, 999)];
        // alpha is mid-turn; beta finished its reply (user is reading) and
        // gamma died mid-turn 20 min ago (crash guard).
        let activity =
            vec![act("alpha", true, 170), act("beta", false, 5), act("gamma", true, 1200)];
        let stats = live_stats(&records, &activity, now);

        assert_eq!(stats.minute_tokens.len(), 60);
        assert_eq!(stats.minute_tokens[58], 500); // 1 min ago
        assert_eq!(stats.minute_tokens[29], 200); // 30 min ago
        assert_eq!(stats.minute_tokens.iter().sum::<u64>(), 700); // 90-min-old excluded
        assert!((stats.burn_per_min - 100.0).abs() < 1e-9); // 500 over last 5 min
        assert!(stats.is_active);
        assert_eq!(stats.active_projects, vec!["alpha".to_string()]);

        let idle = live_stats(&[rec(30, 200)], &[act("alpha", false, 30)], now);
        assert!(!idle.is_active);
        assert!(idle.active_projects.is_empty());

        // recent_project_totals groups by display name from cwd.
        let totals = recent_project_totals(&records);
        assert_eq!(totals.len(), 1);
        assert_eq!(totals["alpha"], 1699);
    }

    #[test]
    fn turn_rows_join_group_and_guard() {
        use crate::source::UsageRecord;
        use crate::watch::TurnMeta;
        use chrono::{TimeZone, Utc};

        let now = Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 0).unwrap();
        let rec = |turn: u32, total: u64, model: &str| UsageRecord {
            project_key: "-u-alpha".into(),
            cwd: Some("/u/alpha".into()),
            session_id: Some("s".into()),
            timestamp: now,
            model: model.into(),
            message_id: None,
            request_id: None,
            usage: TokenUsage { input: total, output: 0, cache_create: 0, cache_read: 0 },
            turn: Some(turn),
            lines_written: 3,
        };
        let meta = |id: u32, mins_ago: i64, ended: bool| TurnMeta {
            id,
            prompt_chars: 10 * id as u64,
            started: now - chrono::Duration::minutes(mins_ago),
            ended: ended.then_some(now),
        };

        let records = [
            rec(1, 100, "claude-opus-4-8"),
            rec(1, 50, "claude-opus-4-8"),
            rec(2, 999, "mystery-model"),
        ];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let turns = vec![
            ("alpha".to_string(), now, meta(1, 30, true)),
            // Open turn with fresh writes: active.
            ("alpha".to_string(), now, meta(2, 5, false)),
            // Open turn whose file went silent 20 min ago: crash-guarded.
            ("beta".to_string(), now - chrono::Duration::minutes(20), meta(3, 15, false)),
        ];
        let groups = turn_rows(&turns, &refs, now);

        // Groups ordered by newest turn: alpha (5 min ago) before beta.
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].project, "alpha");
        assert_eq!(groups[0].turns.len(), 2, "alpha's turns grouped together");
        assert_eq!(groups[1].project, "beta");

        let open = &groups[0].turns[0];
        assert_eq!(open.prompt_chars, 20, "newest first within the group");
        assert!(open.active);
        assert_eq!(open.est_cost, None, "unpriced model shows — not a guess");
        assert_eq!(open.tokens, 999);

        let done = &groups[0].turns[1];
        assert_eq!(done.tokens, 150, "records grouped by turn id");
        assert_eq!(done.lines_written, 6);
        assert!(!done.active);
        assert!(done.est_cost.unwrap() > 0.0);

        let stale = &groups[1].turns[0];
        assert!(!stale.active, "stale mid-turn file must not read as live");
        assert_eq!(stale.tokens, 0);

        // Turns are capped at the display limit across all groups.
        let many: Vec<_> =
            (10..40).map(|i| ("alpha".to_string(), now, meta(i, i as i64, true))).collect();
        let capped = turn_rows(&many, &[], now);
        assert_eq!(capped.iter().map(|g| g.turns.len()).sum::<usize>(), TURN_ROW_LIMIT);
    }

    #[test]
    fn formats_token_counts() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(4_560_000), "4.6M");
        assert_eq!(fmt_tokens(2_100_000_000), "2.1B");
    }
}
