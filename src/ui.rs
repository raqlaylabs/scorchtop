//! Ratatui dashboard. Dumb by design: draws a `ViewModel` and forwards
//! keystrokes — every number it shows was computed in `view.rs`.

use std::time::Duration;

use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::aggregate::Cube;
use crate::source::ScanStats;
use crate::view::{fmt_cost, fmt_tokens, view, Period, ViewModel};

const ACCENT: Color = Color::Cyan;
const MONEY: Color = Color::Green;
const DIM: Color = Color::DarkGray;
/// Cyan -> blue ramp for bars, by project rank.
const BAR_RAMP: [Color; 6] = [
    Color::Indexed(51),
    Color::Indexed(45),
    Color::Indexed(39),
    Color::Indexed(33),
    Color::Indexed(27),
    Color::Indexed(61),
];

pub struct App {
    cube: Cube,
    stats: ScanStats,
    period: Period,
    vm: ViewModel,
}

impl App {
    pub fn new(cube: Cube, stats: ScanStats) -> Self {
        let period = Period::Day;
        let vm = view(&cube, period, Local::now().date_naive());
        Self { cube, stats, period, vm }
    }

    fn set_period(&mut self, period: Period) {
        self.period = period;
        self.vm = view(&self.cube, period, Local::now().date_naive());
    }
}

pub fn run(cube: Cube, stats: ScanStats) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(cube, stats);
    let result = loop {
        if let Err(e) = terminal.draw(|frame| draw(frame, &app)) {
            break Err(e);
        }
        // Static dashboard: block briefly for input; live updates arrive in
        // Milestone 3 via the watcher channel.
        match event::poll(Duration::from_millis(250)) {
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
                        KeyCode::Char('d') => app.set_period(Period::Day),
                        KeyCode::Char('w') => app.set_period(Period::Week),
                        KeyCode::Char('m') => app.set_period(Period::Month),
                        _ => {}
                    }
                }
            }
            Ok(false) => {}
            Err(e) => break Err(e),
        }
    };
    ratatui::restore();
    result
}

fn draw(frame: &mut Frame, app: &App) {
    let [header, main, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)])
            .areas(frame.area());

    draw_header(frame, header, app);
    // Wide terminals get a models side panel; narrow ones keep full width
    // for the project bars.
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
    let title = Line::from(vec![
        Span::styled(" agentop ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("v{} ", env!("CARGO_PKG_VERSION")), Style::new().fg(DIM)),
        Span::styled(
            format!(
                "· claude code · {} files{}",
                app.stats.files_scanned,
                if app.stats.malformed_lines > 0 {
                    format!(" · {} malformed", app.stats.malformed_lines)
                } else {
                    String::new()
                }
            ),
            Style::new().fg(DIM),
        ),
    ]);
    let badge = Line::from(vec![
        Span::styled("[ ", Style::new().fg(DIM)),
        Span::styled(vm.period_label, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" ] ", Style::new().fg(DIM)),
    ])
    .right_aligned();

    let today_cost = fmt_cost(if vm.today.records == 0 && !vm.today.has_unknown_model {
        Some(0.0)
    } else if vm.today.has_unknown_model && vm.today.known_cost == 0.0 {
        None
    } else {
        Some(vm.today.known_cost)
    });
    let stats = Line::from(vec![
        Span::styled(" today ", Style::new().fg(DIM)),
        Span::styled(today_cost, Style::new().fg(MONEY).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {} tok", fmt_tokens(vm.today.tokens.total())), Style::new().fg(ACCENT)),
        Span::styled("   burn ", Style::new().fg(DIM)),
        Span::styled("—/min", Style::new().fg(DIM)),
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
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [row1, row2] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(Paragraph::new(title), row1);
    frame.render_widget(Paragraph::new(badge), row1);
    frame.render_widget(Paragraph::new(stats), row2);
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
                format!("no usage in {}", vm.period_label),
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
    let bar_w = (inner.width as usize).saturating_sub(name_w + tokens_w + cost_w + 6);

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
        let color = BAR_RAMP[i.min(BAR_RAMP.len() - 1)];
        lines.push(Line::from(vec![
            Span::styled(format!("{name:<name_w$}"), Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(bar(p.frac, bar_w), Style::new().fg(color)),
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
