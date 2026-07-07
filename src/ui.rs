//! Ratatui dashboard. Dumb by design: it renders `Snapshot`s delivered over
//! the mpsc channel plus pure `view()` / `live_stats()` results — every
//! number it shows was computed in the aggregation layer. The only state it
//! owns is presentation: the selected period and bar-easing positions.

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

use crate::view::{fmt_cost, fmt_tokens, live_stats, view, LiveStats, Period, ViewModel};
use crate::watch::Snapshot;

const ACCENT: Color = Color::Cyan;
const MONEY: Color = Color::Green;
const DIM: Color = Color::DarkGray;
const LIVE: Color = Color::LightGreen;
/// Cyan -> blue ramp for bars, by project rank.
const BAR_RAMP: [Color; 6] = [
    Color::Indexed(51),
    Color::Indexed(45),
    Color::Indexed(39),
    Color::Indexed(33),
    Color::Indexed(27),
    Color::Indexed(61),
];

const TICK: Duration = Duration::from_millis(80);
const REFRESH: Duration = Duration::from_secs(1);

struct App {
    snapshot: Option<Snapshot>,
    period: Period,
    vm: ViewModel,
    live: LiveStats,
    /// Eased bar fraction per project name (presentation state only).
    eased: HashMap<String, f64>,
}

impl App {
    fn new() -> Self {
        Self {
            snapshot: None,
            period: Period::Day,
            vm: ViewModel::default(),
            live: LiveStats::default(),
            eased: HashMap::new(),
        }
    }

    fn recompute(&mut self) {
        if let Some(snap) = &self.snapshot {
            self.vm = view(&snap.cube, self.period, Local::now().date_naive());
            self.live = live_stats(&snap.recent, chrono::Utc::now());
        }
    }

    /// Move eased bar positions toward their targets; true while animating.
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
        if got_new || last_refresh.elapsed() >= REFRESH {
            // Periodic recompute keeps the sparkline sliding and day
            // rollover correct even with no new data.
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
    let [header, main, footer] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(3), Constraint::Length(1)])
            .areas(frame.area());

    draw_header(frame, header, app);
    if main.width >= 140 && !app.vm.models.is_empty() {
        let [left, right] =
            Layout::horizontal([Constraint::Min(60), Constraint::Length(46)]).areas(main);
        draw_projects(frame, left, app);
        draw_models(frame, right, app);
    } else {
        draw_projects(frame, main, app);
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
        title_spans.push(Span::styled("● live ", Style::new().fg(LIVE).add_modifier(Modifier::BOLD)));
        title_spans.push(Span::styled(
            app.live.active_projects.join(", "),
            Style::new().fg(LIVE),
        ));
    } else {
        title_spans.push(Span::styled("○ idle", Style::new().fg(DIM)));
    }
    let title = Line::from(title_spans);
    let badge = Line::from(vec![
        Span::styled("[ ", Style::new().fg(DIM)),
        Span::styled(vm.period_label, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" ] ", Style::new().fg(DIM)),
    ])
    .right_aligned();

    let today_cost = if vm.today.has_unknown_model && vm.today.known_cost == 0.0 && vm.today.records > 0 {
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
    let stats = Line::from(vec![
        Span::styled(" today ", Style::new().fg(DIM)),
        Span::styled(fmt_cost(today_cost), Style::new().fg(MONEY).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {} tok", fmt_tokens(vm.today.tokens.total())),
            Style::new().fg(ACCENT),
        ),
        Span::styled("   burn ", Style::new().fg(DIM)),
        burn,
        Span::styled("   Σ ", Style::new().fg(DIM)),
        Span::styled(fmt_cost(cost_display(vm)), Style::new().fg(MONEY)),
        Span::styled(
            format!(
                " · {} tok · {}d active",
                fmt_tokens(vm.period.tokens.total()),
                vm.active_days
            ),
            Style::new().fg(DIM),
        ),
    ]);

    let block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::new().fg(DIM));
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
        Layout::horizontal([Constraint::Length(14), Constraint::Min(10)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(" tok/min · 1h ", Style::new().fg(DIM)))),
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

fn draw_projects(frame: &mut Frame, area: Rect, app: &App) {
    let vm = &app.vm;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .title(Line::from(vec![
            Span::styled(" projects ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(format!("({}) ", vm.projects.len()), Style::new().fg(DIM)),
        ]))
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
        let color = BAR_RAMP[i.min(BAR_RAMP.len() - 1)];
        lines.push(Line::from(vec![
            dot,
            Span::styled(
                format!("{name:<name_w$}"),
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(bar(frac, bar_w), Style::new().fg(color)),
            Span::raw("  "),
            Span::styled(format!("{:>tokens_w$}", fmt_tokens(p.tokens)), Style::new().fg(ACCENT)),
            Span::raw("  "),
            Span::styled(format!("{:>cost_w$}", fmt_cost(p.est_cost)), Style::new().fg(MONEY)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_models(frame: &mut Frame, area: Rect, app: &App) {
    let vm = &app.vm;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
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
                Span::styled(format!("{:<24}", m.name), Style::new().fg(Color::White)),
                Span::styled(format!("{:>8}", fmt_tokens(m.tokens)), Style::new().fg(ACCENT)),
                Span::styled(format!("{:>9}", fmt_cost(m.est_cost)), Style::new().fg(MONEY)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let mut spans = Vec::new();
    for (key, label) in [("d", "today"), ("w", "7 days"), ("m", "30 days"), ("q", "quit")] {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
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

/// Unicode block bar with 1/8-cell resolution.
fn bar(frac: f64, width: usize) -> String {
    const PARTIALS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (frac.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let mut s = "█".repeat(full);
    if rem > 0 && full < width {
        s.push(PARTIALS[rem]);
    }
    let pad = width.saturating_sub(s.chars().count());
    s.push_str(&" ".repeat(pad));
    s
}
