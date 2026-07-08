//! asciinema cast v2 export — lets the wrapped screen record its own
//! entrance animation without any screen-capture tooling. Frames are
//! rendered into an off-screen ratatui buffer, serialized to ANSI here, and
//! written as a `.cast` file (JSON lines). Convert with `agg out.cast
//! out.gif` or share the cast directly.

use std::io::Write;
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

/// Serialize one full-repaint frame: home the cursor, then every cell with
/// a reset-based SGR emitted only where the style changes.
pub fn frame_to_ansi(buffer: &Buffer) -> String {
    let area = buffer.area();
    let mut out = String::from("\x1b[H");
    for y in area.top()..area.bottom() {
        if y > area.top() {
            out.push_str("\r\n");
        }
        let mut current: Option<Style> = None;
        for x in area.left()..area.right() {
            let cell = &buffer[(x, y)];
            let style = cell.style();
            if current != Some(style) {
                out.push_str(&sgr(&style));
                current = Some(style);
            }
            out.push_str(cell.symbol());
        }
    }
    out
}

/// Reset-based SGR for a cell style: truecolor fg/bg plus bold. Non-RGB
/// colors fall back to the terminal defaults (the dashboard theme only uses
/// RGB), so nothing here can panic on an unexpected style.
fn sgr(style: &Style) -> String {
    let mut code = String::from("\x1b[0");
    if style.add_modifier.contains(Modifier::BOLD) {
        code.push_str(";1");
    }
    if let Some(Color::Rgb(r, g, b)) = style.fg {
        code.push_str(&format!(";38;2;{r};{g};{b}"));
    }
    if let Some(Color::Rgb(r, g, b)) = style.bg {
        code.push_str(&format!(";48;2;{r};{g};{b}"));
    }
    code.push('m');
    code
}

/// Write an asciinema cast v2 file: a JSON header line, then one output
/// event per frame at its timestamp (seconds).
pub fn write_cast(
    path: &Path,
    width: u16,
    height: u16,
    frames: &[(f64, String)],
) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let header = serde_json::json!({
        "version": 2,
        "width": width,
        "height": height,
        "title": "agentop wrapped",
    });
    writeln!(file, "{header}")?;
    for (i, (time, frame)) in frames.iter().enumerate() {
        // Hide the cursor for the whole recording via the first frame.
        let data = if i == 0 { format!("\x1b[?25l{frame}") } else { frame.clone() };
        writeln!(file, "{}", serde_json::json!([time, "o", data]))?;
    }
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn frame_emits_truecolor_runs_once_per_style_change() {
        let mut buffer = Buffer::with_lines(vec!["ab", "cd"]);
        buffer.set_style(
            Rect::new(0, 0, 1, 1),
            Style::new().fg(Color::Rgb(1, 2, 3)).bg(Color::Rgb(9, 8, 7)),
        );
        buffer.set_style(Rect::new(1, 0, 1, 1), Style::new().add_modifier(Modifier::BOLD));

        let ansi = frame_to_ansi(&buffer);
        assert!(ansi.starts_with("\x1b[H"), "homes the cursor");
        assert!(ansi.contains("\x1b[0;38;2;1;2;3;48;2;9;8;7ma"));
        assert!(ansi.contains("\x1b[0;1mb"), "bold without color resets color");
        assert!(ansi.contains("\r\n"), "explicit line breaks");
        // Second row shares one default style: exactly one SGR for "cd".
        let row2 = ansi.split("\r\n").nth(1).unwrap();
        assert_eq!(row2.matches("\x1b[0").count(), 1);
        assert!(row2.ends_with("cd"));
    }

    #[test]
    fn cast_file_is_valid_json_lines_with_header() {
        let dir = std::env::temp_dir().join(format!("agentop-cast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.cast");

        let frames = vec![(0.0, "one".to_string()), (0.5, "two".to_string())];
        write_cast(&path, 80, 24, &frames).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        let header: serde_json::Value =
            serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 80);
        assert_eq!(header["height"], 24);

        let first: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first[0], 0.0);
        assert_eq!(first[1], "o");
        assert!(first[2].as_str().unwrap().starts_with("\u{1b}[?25l"), "cursor hidden once");
        let second: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(second[2], "two");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
