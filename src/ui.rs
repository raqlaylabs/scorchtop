//! Ratatui dashboard. Dumb by design: it renders `Snapshot`s delivered over
//! the mpsc channel plus pure `view()` / `live_stats()` results — every
//! number it shows was computed in the aggregation layer. The only state it
//! owns is presentation: the selected period, bar easing, and the equalizer's
//! energy/peak animation.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Sparkline};
use ratatui::Frame;

use crate::view::{
    fmt_cost, fmt_tokens, live_stats, recent_project_totals, view, LiveStats, Period, ViewModel,
};
use crate::watch::Snapshot;

// ---------------------------------------------------------------------------
// Palette: agentop paints its own dark theme (truecolor), btop-style, instead
// of inheriting the terminal's colors.

const BG: Color = Color::Rgb(9, 12, 18);
const FG: Color = Color::Rgb(196, 205, 216);
const DIM: Color = Color::Rgb(100, 110, 126);
const BORDER: Color = Color::Rgb(42, 52, 68);
const ACCENT: Color = Color::Rgb(56, 214, 240);
const MONEY: Color = Color::Rgb(74, 222, 128);
const LIVE: Color = Color::Rgb(94, 250, 154);
/// Off-phase of a blinking live dot.
const LIVE_DIM: Color = Color::Rgb(32, 96, 60);

/// Blink phase for live dots: 600ms on/off on the wall clock, so no tick
/// state is needed — live sessions keep the animation loop redrawing anyway.
fn blink_on() -> bool {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() / 600 % 2 == 0)
        .unwrap_or(true)
}
const BADGE: Color = Color::Rgb(250, 204, 21);
const PEAK: Color = Color::Rgb(226, 232, 240);

/// Horizontal bars fade from this deep blue at their base…
const BAR_BASE: (u8, u8, u8) = (26, 50, 86);
/// …to a tip color that cools with project rank (top project burns hottest).
fn bar_tip(rank: usize) -> (u8, u8, u8) {
    lerp_rgb((0, 231, 255), (92, 116, 168), (rank as f64 / 6.0).min(1.0))
}

/// Model-family colors for the split-by-model bar mode and the models panel.
fn model_color(model: &str) -> Color {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        Color::Rgb(244, 114, 182) // pink
    } else if m.contains("opus") {
        Color::Rgb(192, 132, 252) // violet
    } else if m.contains("sonnet") {
        Color::Rgb(56, 214, 240) // cyan
    } else if m.contains("haiku") {
        Color::Rgb(74, 222, 128) // green
    } else {
        Color::Rgb(125, 135, 150) // unknown/other
    }
}

/// Family label for the legend ("opus", "sonnet", ...).
fn model_family(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") {
        "fable"
    } else if m.contains("opus") {
        "opus"
    } else if m.contains("sonnet") {
        "sonnet"
    } else if m.contains("haiku") {
        "haiku"
    } else {
        "other"
    }
}

/// Equalizer columns: dim teal base -> cyan -> near-white hot tip.
fn eq_color(t: f64) -> Color {
    if t < 0.72 {
        rgb(lerp_rgb((16, 62, 88), (0, 224, 255), t / 0.72))
    } else {
        rgb(lerp_rgb((0, 224, 255), (235, 250, 255), (t - 0.72) / 0.28))
    }
}

fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    (c(a.0, b.0), c(a.1, b.1), c(a.2, b.2))
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

// ---------------------------------------------------------------------------
// Animation timing.

const TICK: Duration = Duration::from_millis(80);
const REFRESH: Duration = Duration::from_secs(1);
/// Equalizer energy half-life ~1.8s (0.97^tick), peaks fall ~10%/s.
const ENERGY_DECAY: f64 = 0.97;
const PEAK_FALL: f64 = 0.008;
/// Adaptive full-scale floor for the equalizer, in tokens of burst energy.
const SCALE_FLOOR: f64 = 150_000.0;
/// Live-but-quiet sessions groove at this baseline height instead of dying —
/// the panel keeps dancing while anything is active; spikes still mean tokens.
const LIVE_FLOOR: f64 = 0.18;

/// Per-project equalizer state, all presentation: normalized height 0..1,
/// falling peak cap, and a phase that makes the column tops ripple.
#[derive(Default)]
struct BandState {
    energy: f64,
    height: f64,
    peak: f64,
    phase: f64,
}

struct App {
    snapshot: Option<Snapshot>,
    period: Period,
    vm: ViewModel,
    live: LiveStats,
    /// Eased horizontal-bar fraction per project name.
    eased: HashMap<String, f64>,
    /// Equalizer bands per project name.
    bands: HashMap<String, BandState>,
    /// Last per-project token totals over the recent window, to turn
    /// snapshot-to-snapshot deltas into band energy.
    last_totals: HashMap<String, u64>,
    primed: bool,
    /// Adaptive equalizer full-scale (max burst seen, slowly decaying).
    scale: f64,
    /// Color project bars by model share instead of the rank gradient.
    split_by_model: bool,
    /// Show recent turns instead of models in the right-side panel.
    show_turns: bool,
}

impl App {
    fn new() -> Self {
        Self {
            snapshot: None,
            period: Period::Day,
            vm: ViewModel::default(),
            live: LiveStats::default(),
            eased: HashMap::new(),
            bands: HashMap::new(),
            last_totals: HashMap::new(),
            primed: false,
            scale: SCALE_FLOOR,
            split_by_model: false,
            show_turns: false,
        }
    }

    fn recompute(&mut self) {
        if let Some(snap) = &self.snapshot {
            self.vm = view(&snap.cube, self.period, Local::now().date_naive());
            self.live = live_stats(&snap.recent, &snap.activity, chrono::Utc::now());
        }
    }

    /// Feed snapshot-to-snapshot token deltas into band energy. The first
    /// snapshot only primes the baseline — startup must not read as a burst.
    fn ingest(&mut self) {
        let Some(snap) = &self.snapshot else { return };
        let totals = recent_project_totals(&snap.recent);
        if self.primed {
            for (name, total) in &totals {
                let prev = self.last_totals.get(name).copied().unwrap_or(0);
                let delta = total.saturating_sub(prev);
                if delta > 0 {
                    self.bands.entry(name.clone()).or_default().energy += delta as f64;
                }
            }
        }
        self.last_totals = totals;
        self.primed = true;
    }

    /// Advance all animation state one tick; true while anything is moving.
    fn animate(&mut self) -> bool {
        let mut moving = false;
        for p in &self.vm.projects {
            let slot = self.eased.entry(p.name.clone()).or_insert(0.0);
            let delta = p.frac - *slot;
            if delta.abs() < 0.004 {
                *slot = p.frac;
            } else {
                *slot += delta * 0.25;
                moving = true;
            }
        }

        // Every live project keeps a band, so the groove floor applies even
        // before its first token burst of the session.
        for name in &self.live.active_projects {
            self.bands.entry(name.clone()).or_default();
        }
        let mut max_energy: f64 = 0.0;
        for (name, band) in self.bands.iter_mut() {
            band.energy *= ENERGY_DECAY;
            max_energy = max_energy.max(band.energy);
            let mut target = (band.energy / self.scale).powf(0.6).min(1.0);
            if self.live.active_projects.contains(name) {
                target = target.max(LIVE_FLOOR);
            }
            // Fast attack, eased release.
            band.height = if target > band.height {
                target
            } else {
                band.height + (target - band.height) * 0.2
            };
            band.peak = (band.peak - PEAK_FALL).max(band.height);
            if band.height > 0.01 {
                band.phase += 0.38;
                moving = true;
            }
        }
        self.scale = (self.scale * 0.999).max(max_energy).max(SCALE_FLOOR);
        self.bands.retain(|name, b| {
            b.energy > 1.0 || b.peak > 0.02 || self.live.active_projects.contains(name)
        });
        moving
    }
}

pub fn run(rx: Receiver<Snapshot>) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new();
    let mut dirty = true;
    let mut last_refresh = Instant::now();

    let result = loop {
        // Drain the channel; keep only the newest snapshot.
        let mut got_new = false;
        while let Ok(snap) = rx.try_recv() {
            app.snapshot = Some(snap);
            got_new = true;
        }
        if got_new {
            app.ingest();
        }
        if got_new || last_refresh.elapsed() >= REFRESH {
            // Periodic recompute keeps the sparkline sliding, activity
            // expiring, and day rollover correct even with no new data.
            app.recompute();
            last_refresh = Instant::now();
            dirty = true;
        }
        if app.animate() {
            dirty = true;
        }

        if dirty {
            if let Err(e) = terminal.draw(|frame| draw(frame, &app)) {
                break Err(e);
            }
            dirty = false;
        }

        match event::poll(TICK) {
            Ok(true) => {
                if let Ok(Event::Key(key)) = event::read() {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break Ok(())
                        }
                        KeyCode::Char('d') => set_period(&mut app, Period::Day, &mut dirty),
                        KeyCode::Char('w') => set_period(&mut app, Period::Week, &mut dirty),
                        KeyCode::Char('m') => set_period(&mut app, Period::Month, &mut dirty),
                        KeyCode::Char('x') => {
                            app.split_by_model = !app.split_by_model;
                            dirty = true;
                        }
                        KeyCode::Char('t') => {
                            app.show_turns = !app.show_turns;
                            dirty = true;
                        }
                        _ => {}
                    }
                } else {
                    dirty = true; // resize etc.
                }
            }
            Ok(false) => {}
            Err(e) => break Err(e),
        }
    };
    ratatui::restore();
    result
}

fn set_period(app: &mut App, period: Period, dirty: &mut bool) {
    app.period = period;
    app.recompute();
    *dirty = true;
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, Style::new().bg(BG).fg(FG));

    // The equalizer gets a third of tall terminals, nothing below 20 rows.
    let eq_h = if area.height >= 20 { (area.height / 3).clamp(7, 12) } else { 0 };
    let [header, eq, main, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(eq_h),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, header, app);
    if eq_h > 0 {
        draw_equalizer(frame, eq, app);
    }
    if main.width >= 110 && !app.vm.models.is_empty() {
        let [left, right] =
            Layout::horizontal([Constraint::Min(60), Constraint::Length(46)]).areas(main);
        draw_projects(frame, left, app, false);
        if app.show_turns {
            draw_turns(frame, right, app);
        } else {
            draw_models(frame, right, app);
        }
    } else {
        // Tell the user the models panel exists — it needs more columns.
        let widen_hint = !app.vm.models.is_empty();
        draw_projects(frame, main, app, widen_hint);
    }
    draw_footer(frame, footer);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let vm = &app.vm;
    let scanning = app.snapshot.is_none();

    let mut title_spans = vec![
        Span::styled(" agentop ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("v{} ", env!("CARGO_PKG_VERSION")), Style::new().fg(DIM)),
    ];
    if scanning {
        title_spans.push(Span::styled("· scanning ~/.claude …", Style::new().fg(DIM)));
    } else if app.live.is_active {
        let dot = Style::new().fg(if blink_on() { LIVE } else { LIVE_DIM });
        title_spans.push(Span::styled("● ", dot.add_modifier(Modifier::BOLD)));
        title_spans
            .push(Span::styled("live ", Style::new().fg(LIVE).add_modifier(Modifier::BOLD)));
        title_spans
            .push(Span::styled(app.live.active_projects.join(", "), Style::new().fg(LIVE)));
    } else {
        title_spans.push(Span::styled("○ idle", Style::new().fg(DIM)));
    }
    let title = Line::from(title_spans);
    let badge = Line::from(vec![
        Span::styled("[ ", Style::new().fg(DIM)),
        Span::styled(vm.period_label, Style::new().fg(BADGE).add_modifier(Modifier::BOLD)),
        Span::styled(" ] ", Style::new().fg(DIM)),
    ])
    .right_aligned();

    let today_cost =
        if vm.today.has_unknown_model && vm.today.known_cost == 0.0 && vm.today.records > 0 {
            None
        } else {
            Some(vm.today.known_cost)
        };
    let burn = if app.live.burn_per_min >= 1.0 {
        Span::styled(
            format!("{}/min", fmt_tokens(app.live.burn_per_min as u64)),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("—/min", Style::new().fg(DIM))
    };
    let mut stats_spans = vec![
        Span::styled(" today ", Style::new().fg(DIM)),
        Span::styled(fmt_cost(today_cost), Style::new().fg(MONEY).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {} tok", fmt_tokens(vm.today.tokens.total())),
            Style::new().fg(ACCENT),
        ),
    ];
    // Most "tokens" are cache re-reads (billed at 0.1x input); surface the
    // share so the big totals aren't mistaken for fresh context.
    let today_total = vm.today.tokens.total();
    if today_total > 0 {
        let pct = vm.today.tokens.cache_read as f64 / today_total as f64 * 100.0;
        stats_spans.push(Span::styled(
            format!(" ({pct:.0}% cache)"),
            Style::new().fg(DIM),
        ));
    }
    stats_spans.push(Span::styled("   burn ", Style::new().fg(DIM)));
    stats_spans.push(burn);
    // On the day view the period totals equal the today stat; only show Σ
    // when it adds information.
    if app.period != Period::Day {
        stats_spans.push(Span::styled("   Σ ", Style::new().fg(DIM)));
        stats_spans.push(Span::styled(fmt_cost(cost_display(vm)), Style::new().fg(MONEY)));
        stats_spans.push(Span::styled(
            format!(
                " · {} tok · {}d active",
                fmt_tokens(vm.period.tokens.total()),
                vm.active_days
            ),
            Style::new().fg(DIM),
        ));
    }
    let stats = Line::from(stats_spans);

    let block = Block::new().borders(Borders::BOTTOM).border_style(Style::new().fg(BORDER));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [row1, row2, row3] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(title), row1);
    frame.render_widget(Paragraph::new(badge), row1);
    frame.render_widget(Paragraph::new(stats), row2);
    draw_sparkline(frame, row3, app);
}

fn draw_sparkline(frame: &mut Frame, area: Rect, app: &App) {
    let [label_area, spark_area] =
        Layout::horizontal([Constraint::Length(24), Constraint::Min(10)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " tok/min · past hour → ",
            Style::new().fg(DIM),
        ))),
        label_area,
    );
    let data = &app.live.minute_tokens;
    let width = spark_area.width as usize;
    let slice = if data.len() > width { &data[data.len() - width..] } else { &data[..] };
    frame.render_widget(
        Sparkline::default().data(slice).style(Style::new().fg(ACCENT)),
        spark_area,
    );
}

fn cost_display(vm: &ViewModel) -> Option<f64> {
    if vm.period.has_unknown_model && vm.period.known_cost == 0.0 {
        None
    } else {
        Some(vm.period.known_cost)
    }
}

// ---------------------------------------------------------------------------
// Equalizer: one dancing column cluster per project with recent throughput.

const V_EIGHTHS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];

fn draw_equalizer(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![
            Span::styled(" streaming ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled("tok ", Style::new().fg(DIM)),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 4 || inner.width < 12 {
        return;
    }
    let rows = (inner.height - 1) as usize; // last row holds the labels

    // Bands: anything with visible motion plus every currently-live project
    // (idle-but-live sessions keep a faint ember so the panel never dies).
    let mut names: Vec<&String> = app
        .bands
        .iter()
        .filter(|(name, b)| b.peak > 0.015 || app.live.active_projects.contains(name))
        .map(|(name, _)| name)
        .collect();
    for name in &app.live.active_projects {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort_by(|a, b| {
        let h = |n: &str| app.bands.get(n).map(|b| b.height).unwrap_or(0.0);
        h(b).partial_cmp(&h(a)).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(b))
    });

    if names.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "· · ·  waiting for tokens  · · ·",
            Style::new().fg(DIM),
        )))
        .centered();
        let mid = Rect { y: inner.y + inner.height / 2, height: 1, ..inner };
        frame.render_widget(msg, mid);
        return;
    }

    let n = names.len().min((inner.width as usize / 8).max(1));
    let names = &names[..n];
    let slot_w = inner.width as usize / n;
    let band_w = slot_w.saturating_sub(2).clamp(3, 14);

    let empty = BandState::default();
    let mut lines: Vec<Line> = Vec::with_capacity(rows + 1);
    for row in 0..rows {
        let from_bottom = (rows - 1 - row) as f64;
        let mut spans: Vec<Span> = Vec::new();
        for name in names {
            let band = app.bands.get(*name).unwrap_or(&empty);
            let base = band.height;
            let peak_cell = (band.peak * rows as f64).round();
            let left_pad = (slot_w - band_w) / 2;
            spans.push(Span::raw(" ".repeat(left_pad)));
            for sub in 0..band_w {
                // Each sub-column ripples around the band height; the floor
                // term keeps ~1 cell of motion even at the groove baseline.
                let wobble = (0.05 + 0.08 * base) * (band.phase + sub as f64 * 1.9).sin();
                let h = (base + wobble).clamp(0.0, 1.0) * rows as f64;
                let filled = h - from_bottom;
                let (ch, style) = if filled >= 1.0 {
                    ('█', Style::new().fg(eq_color(from_bottom / rows as f64)))
                } else if filled > 0.06 {
                    (
                        V_EIGHTHS[((filled * 8.0) as usize).clamp(1, 7)],
                        Style::new().fg(eq_color(from_bottom / rows as f64)),
                    )
                } else if from_bottom == peak_cell && band.peak > band.height + 0.02 {
                    ('▔', Style::new().fg(PEAK))
                } else {
                    (' ', Style::new())
                };
                spans.push(Span::styled(ch.to_string(), style));
            }
            spans.push(Span::raw(" ".repeat(slot_w - left_pad - band_w)));
        }
        lines.push(Line::from(spans));
    }

    // Label row: centered project names, live dot for active ones.
    let mut labels: Vec<Span> = Vec::new();
    for name in names {
        let active = app.live.active_projects.contains(*name);
        let max_name = slot_w.saturating_sub(3).max(1);
        let mut label: String = name.chars().take(max_name).collect();
        if name.chars().count() > max_name {
            label.pop();
            label.push('…');
        }
        let display_len = label.chars().count() + 2; // dot + space
        let left = (slot_w.saturating_sub(display_len)) / 2;
        labels.push(Span::raw(" ".repeat(left)));
        labels.push(if active {
            Span::styled("● ", Style::new().fg(LIVE))
        } else {
            Span::styled("○ ", Style::new().fg(DIM))
        });
        labels.push(Span::styled(
            label,
            Style::new().fg(if active { FG } else { DIM }).add_modifier(Modifier::BOLD),
        ));
        labels.push(Span::raw(" ".repeat(slot_w.saturating_sub(left + display_len))));
    }
    lines.push(Line::from(labels));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_projects(frame: &mut Frame, area: Rect, app: &App, widen_hint: bool) {
    let vm = &app.vm;
    let mut title_spans = vec![
        Span::styled(" projects ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("({}) ", vm.projects.len()), Style::new().fg(DIM)),
    ];
    if widen_hint {
        title_spans.push(Span::styled(
            "· ↔ widen to 110+ cols for the models panel ",
            Style::new().fg(DIM),
        ));
    }
    if app.split_by_model {
        // Legend: one dot per model family present in the period.
        let mut seen: Vec<&'static str> = Vec::new();
        for m in &vm.models {
            let fam = model_family(&m.name);
            if !seen.contains(&fam) {
                seen.push(fam);
                title_spans.push(Span::styled("· ", Style::new().fg(DIM)));
                title_spans.push(Span::styled("■ ", Style::new().fg(model_color(&m.name))));
                title_spans.push(Span::styled(format!("{fam} "), Style::new().fg(DIM)));
            }
        }
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(title_spans))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if vm.projects.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if app.snapshot.is_none() {
                    "scanning…".to_string()
                } else {
                    format!("no usage in {}", vm.period_label)
                },
                Style::new().fg(DIM),
            ))),
            inner,
        );
        return;
    }

    let name_w = vm
        .projects
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 20);
    let tokens_w = 8usize;
    let cost_w = 9usize;
    // name + activity dot + gaps + tokens + cost
    let bar_w = (inner.width as usize).saturating_sub(name_w + 2 + tokens_w + cost_w + 6);

    let visible = inner.height as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (i, p) in vm.projects.iter().enumerate() {
        if lines.len() + 1 == visible && vm.projects.len() > visible {
            lines.push(Line::from(Span::styled(
                format!("… {} more", vm.projects.len() - i),
                Style::new().fg(DIM),
            )));
            break;
        }
        let name: String = if p.name.chars().count() > name_w {
            let truncated: String = p.name.chars().take(name_w - 1).collect();
            format!("{truncated}…")
        } else {
            p.name.clone()
        };
        let active = app.live.active_projects.iter().any(|a| a == &p.name);
        let dot = if active {
            Span::styled("● ", Style::new().fg(LIVE))
        } else {
            Span::raw("  ")
        };
        let frac = app.eased.get(&p.name).copied().unwrap_or(p.frac);
        let mut spans = vec![
            dot,
            Span::styled(
                format!("{name:<name_w$}"),
                Style::new().fg(FG).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
        ];
        if app.split_by_model {
            spans.extend(stacked_bar(frac, bar_w, &p.models, p.tokens));
        } else {
            spans.extend(gradient_bar(frac, bar_w, i));
        }
        spans.extend([
            Span::raw("  "),
            Span::styled(format!("{:>tokens_w$}", fmt_tokens(p.tokens)), Style::new().fg(ACCENT)),
            Span::raw("  "),
            Span::styled(format!("{:>cost_w$}", fmt_cost(p.est_cost)), Style::new().fg(MONEY)),
        ]);
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_models(frame: &mut Frame, area: Rect, app: &App) {
    let vm = &app.vm;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(Span::styled(
            " models ",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = vm
        .models
        .iter()
        .take(inner.height as usize)
        .map(|m| {
            Line::from(vec![
                Span::styled("■ ", Style::new().fg(model_color(&m.name))),
                Span::styled(format!("{:<22}", m.name), Style::new().fg(FG)),
                Span::styled(format!("{:>8}", fmt_tokens(m.tokens)), Style::new().fg(ACCENT)),
                Span::styled(format!("{:>9}", fmt_cost(m.est_cost)), Style::new().fg(MONEY)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_turns(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![
            Span::styled(" turns ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled("· prompt → tok → lines ", Style::new().fg(DIM)),
        ]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let turns = app.snapshot.as_ref().map(|s| s.turns.as_slice()).unwrap_or_default();
    if turns.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no prompts seen yet",
                Style::new().fg(DIM),
            ))),
            inner,
        );
        return;
    }

    let lines: Vec<Line> = turns
        .iter()
        .take(inner.height as usize)
        .enumerate()
        .map(|(i, t)| {
            let dot = if t.active {
                Span::styled("● ", Style::new().fg(if blink_on() { LIVE } else { LIVE_DIM }))
            } else {
                Span::raw("  ")
            };
            let name: String = if t.project.chars().count() > 9 {
                format!("{}…", t.project.chars().take(8).collect::<String>())
            } else {
                format!("{:<9}", t.project)
            };
            // Runs of the same project read as one group: only the first row
            // of a run gets a bright name, so project changes pop.
            let repeated = i > 0 && turns[i - 1].project == t.project;
            let name_style = if t.active {
                Style::new().fg(LIVE).add_modifier(Modifier::BOLD)
            } else if repeated {
                Style::new().fg(DIM)
            } else {
                Style::new().fg(FG).add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                dot,
                Span::styled(name, name_style),
                Span::styled(
                    format!("{:>7}", format!("{}ch", fmt_tokens(t.prompt_chars))),
                    Style::new().fg(DIM),
                ),
                Span::styled(format!("{:>8}", fmt_tokens(t.tokens)), Style::new().fg(ACCENT)),
                Span::styled(
                    format!("{:>7}", format!("{}ln", fmt_tokens(t.lines_written))),
                    Style::new().fg(PEAK),
                ),
                Span::styled(format!("{:>9}", fmt_cost(t.est_cost)), Style::new().fg(MONEY)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let mut spans = Vec::new();
    for (key, label) in [
        ("d", "today"),
        ("w", "7 days"),
        ("m", "30 days"),
        ("x", "models"),
        ("t", "turns"),
        ("q", "quit"),
    ] {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(BG).bg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}   "), Style::new().fg(DIM)));
    }
    let hints = Line::from(spans);
    let note = Line::from(Span::styled(
        "$ = est. API value, not your subscription price ",
        Style::new().fg(DIM),
    ))
    .right_aligned();
    frame.render_widget(Paragraph::new(hints), area);
    if area.width >= 100 {
        frame.render_widget(Paragraph::new(note), area);
    }
}

/// Horizontal bar partitioned into model-colored segments (cell resolution,
/// eighth-precision on the trailing edge).
fn stacked_bar(
    frac: f64,
    width: usize,
    models: &[(String, u64)],
    total: u64,
) -> Vec<Span<'static>> {
    const PARTIALS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (frac.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;

    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    if total > 0 && full > 0 {
        // Whole-cell segments per model, largest first; last one absorbs
        // rounding so the bar length always matches the gradient mode.
        let mut cells: Vec<(usize, Color)> = models
            .iter()
            .map(|(m, t)| {
                ((*t as f64 / total as f64 * full as f64).round() as usize, model_color(m))
            })
            .collect();
        let assigned: usize = cells.iter().map(|(c, _)| *c).sum();
        if let Some(last) = cells.last_mut() {
            last.0 = (last.0 + full).saturating_sub(assigned);
        }
        for (count, color) in cells {
            let count = count.min(full - used);
            if count > 0 {
                spans.push(Span::styled("█".repeat(count), Style::new().fg(color)));
                used += count;
            }
        }
        if used < full {
            let color = models.first().map(|(m, _)| model_color(m)).unwrap_or(DIM);
            spans.push(Span::styled("█".repeat(full - used), Style::new().fg(color)));
            used = full;
        }
    }
    if rem > 0 && used < width {
        let color = models.last().map(|(m, _)| model_color(m)).unwrap_or(DIM);
        spans.push(Span::styled(PARTIALS[rem].to_string(), Style::new().fg(color)));
        used += 1;
    }
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

/// Horizontal unicode-block bar with 1/8-cell resolution and a per-cell
/// base->tip color gradient.
fn gradient_bar(frac: f64, width: usize, rank: usize) -> Vec<Span<'static>> {
    const PARTIALS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let tip = bar_tip(rank);
    let eighths = (frac.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;

    let mut spans: Vec<Span> = Vec::with_capacity(full + 2);
    let color_at = |cell: usize| {
        let t = if width <= 1 { 1.0 } else { cell as f64 / (width - 1) as f64 };
        rgb(lerp_rgb(BAR_BASE, tip, t))
    };
    for cell in 0..full {
        spans.push(Span::styled("█", Style::new().fg(color_at(cell))));
    }
    let mut used = full;
    if rem > 0 && full < width {
        spans.push(Span::styled(PARTIALS[rem].to_string(), Style::new().fg(color_at(full))));
        used += 1;
    }
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}
