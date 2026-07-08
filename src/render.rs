//! Native GIF export — rasterizes ratatui buffers into pixels and encodes
//! an animated GIF, so `r` in the wrapped screen produces a shareable file
//! with no external tooling. Text is drawn with an embedded JetBrains Mono
//! (OFL, see assets/fonts/OFL.txt); bars, box borders, and geometric marks
//! are drawn procedurally so gradients stay pixel-perfect at any scale.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

const FONT_REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");

type Rgb = (u8, u8, u8);

pub struct Rasterizer {
    regular: fontdue::Font,
    bold: fontdue::Font,
    px: f32,
    pub cell_w: usize,
    pub cell_h: usize,
    baseline: usize,
    /// Fallbacks for cells whose style has no explicit RGB color.
    fg: Rgb,
    bg: Rgb,
    cache: HashMap<(char, bool), (fontdue::Metrics, Vec<u8>)>,
}

impl Rasterizer {
    /// `px` is the font size in pixels — it directly sets the output
    /// resolution (one terminal cell ≈ 0.6px × 1.2px of it).
    pub fn new(px: f32, fg: Rgb, bg: Rgb) -> io::Result<Self> {
        let load = |bytes: &[u8]| {
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                .map_err(|e| io::Error::other(format!("embedded font: {e}")))
        };
        let regular = load(FONT_REGULAR)?;
        let bold = load(FONT_BOLD)?;
        let line = regular
            .horizontal_line_metrics(px)
            .ok_or_else(|| io::Error::other("font has no horizontal metrics"))?;
        let cell_w = regular.metrics('M', px).advance_width.round() as usize;
        Ok(Self {
            regular,
            bold,
            px,
            cell_w,
            cell_h: line.new_line_size.round() as usize,
            baseline: line.ascent.round() as usize,
            fg,
            bg,
            cache: HashMap::new(),
        })
    }

    pub fn frame_size(&self, buffer: &Buffer) -> (usize, usize) {
        (buffer.area.width as usize * self.cell_w, buffer.area.height as usize * self.cell_h)
    }

    /// Rasterize one buffer into an RGB pixel frame (row-major, 3 bytes/px).
    pub fn rasterize(&mut self, buffer: &Buffer) -> Vec<u8> {
        let (img_w, img_h) = self.frame_size(buffer);
        let mut img = vec![0u8; img_w * img_h * 3];
        let area = buffer.area;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &buffer[(x, y)];
                let style = cell.style();
                let fg = rgb_or(style.fg, self.fg);
                let bg = rgb_or(style.bg, self.bg);
                let bold = style.add_modifier.contains(Modifier::BOLD);
                let cx = (x - area.left()) as usize * self.cell_w;
                let cy = (y - area.top()) as usize * self.cell_h;
                fill(&mut img, img_w, cx, cy, self.cell_w, self.cell_h, bg);
                let ch = cell.symbol().chars().next().unwrap_or(' ');
                if ch != ' ' {
                    self.draw_glyph(&mut img, img_w, cx, cy, ch, fg, bg, bold);
                }
            }
        }
        img
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_glyph(
        &mut self,
        img: &mut [u8],
        img_w: usize,
        cx: usize,
        cy: usize,
        ch: char,
        fg: Rgb,
        bg: Rgb,
        bold: bool,
    ) {
        let (w, h) = (self.cell_w, self.cell_h);
        let eighth_w = |k: usize| w * k / 8;
        let eighth_h = |k: usize| h * k / 8;
        let stroke = (h / 14).max(1);
        match ch {
            // Block elements: procedural rectangles keep bar gradients and
            // heat cells crisp at any scale.
            '█' => fill(img, img_w, cx, cy, w, h, fg),
            '▉' => fill(img, img_w, cx, cy, eighth_w(7), h, fg),
            '▊' => fill(img, img_w, cx, cy, eighth_w(6), h, fg),
            '▋' => fill(img, img_w, cx, cy, eighth_w(5), h, fg),
            '▌' => fill(img, img_w, cx, cy, eighth_w(4), h, fg),
            '▍' => fill(img, img_w, cx, cy, eighth_w(3), h, fg),
            '▎' => fill(img, img_w, cx, cy, eighth_w(2), h, fg),
            '▏' => fill(img, img_w, cx, cy, eighth_w(1), h, fg),
            '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' => {
                let k = (ch as u32 - '▁' as u32 + 1) as usize;
                fill(img, img_w, cx, cy + h - eighth_h(k), w, eighth_h(k), fg);
            }
            '▔' => fill(img, img_w, cx, cy, w, eighth_h(1), fg),
            '▀' => fill(img, img_w, cx, cy, w, h / 2, fg),
            // Box drawing (strokes meet at the cell center).
            '─' => fill(img, img_w, cx, cy + (h - stroke) / 2, w, stroke, fg),
            '│' => fill(img, img_w, cx + (w - stroke) / 2, cy, stroke, h, fg),
            '╭' | '╮' | '╰' | '╯' => {
                let (mx, my) = ((w - stroke) / 2, (h - stroke) / 2);
                let (h_left, v_up) = match ch {
                    '╭' => (false, false),
                    '╮' => (true, false),
                    '╰' => (false, true),
                    _ => (true, true),
                };
                if h_left {
                    fill(img, img_w, cx, cy + my, mx + stroke, stroke, fg);
                } else {
                    fill(img, img_w, cx + mx, cy + my, w - mx, stroke, fg);
                }
                if v_up {
                    fill(img, img_w, cx + mx, cy, stroke, my + stroke, fg);
                } else {
                    fill(img, img_w, cx + mx, cy + my, stroke, h - my, fg);
                }
            }
            // Geometric marks the font may not cover.
            '●' | '○' | '·' => {
                let r = if ch == '·' { h as f64 * 0.09 } else { h as f64 * 0.21 };
                let (ox, oy) = (w as f64 / 2.0, h as f64 / 2.0);
                for py in 0..h {
                    for pxl in 0..w {
                        let d = ((pxl as f64 + 0.5 - ox).powi(2)
                            + (py as f64 + 0.5 - oy).powi(2))
                        .sqrt();
                        let inside = if ch == '○' {
                            d <= r && d >= r - stroke as f64
                        } else {
                            d <= r
                        };
                        if inside {
                            put(img, img_w, cx + pxl, cy + py, fg);
                        }
                    }
                }
            }
            '◂' | '▸' => {
                let hh = h as f64 * 0.22;
                let ww = w as f64 * 0.72;
                let cyf = h as f64 / 2.0;
                for py in 0..h {
                    let dy = (py as f64 + 0.5 - cyf).abs();
                    if dy > hh {
                        continue;
                    }
                    let run = (ww * (1.0 - dy / hh)) as usize;
                    let x0 = if ch == '▸' {
                        (w as f64 * 0.14) as usize
                    } else {
                        w.saturating_sub((w as f64 * 0.14) as usize + run)
                    };
                    for pxl in 0..run {
                        put(img, img_w, cx + x0 + pxl, cy + py, fg);
                    }
                }
            }
            // Everything else: font glyph blended fg-over-bg by coverage.
            _ => {
                let px = self.px;
                let font = if bold { &self.bold } else { &self.regular };
                if font.lookup_glyph_index(ch) == 0 {
                    return; // unmapped glyph: leave the cell as background
                }
                let (metrics, coverage) = self
                    .cache
                    .entry((ch, bold))
                    .or_insert_with(|| font.rasterize(ch, px));
                let top = self.baseline as i32 - metrics.ymin - metrics.height as i32;
                for gy in 0..metrics.height {
                    let py = cy as i32 + top + gy as i32;
                    for gx in 0..metrics.width {
                        let pxx = cx as i32 + metrics.xmin + gx as i32;
                        if py < 0 || pxx < 0 {
                            continue;
                        }
                        let cov = coverage[gy * metrics.width + gx];
                        if cov > 0 {
                            put(img, img_w, pxx as usize, py as usize, blend(bg, fg, cov));
                        }
                    }
                }
            }
        }
    }
}

fn rgb_or(color: Option<Color>, fallback: Rgb) -> Rgb {
    match color {
        Some(Color::Rgb(r, g, b)) => (r, g, b),
        _ => fallback,
    }
}

fn blend(bg: Rgb, fg: Rgb, cov: u8) -> Rgb {
    let t = cov as u32;
    let c = |b: u8, f: u8| ((b as u32 * (255 - t) + f as u32 * t) / 255) as u8;
    (c(bg.0, fg.0), c(bg.1, fg.1), c(bg.2, fg.2))
}

fn put(img: &mut [u8], img_w: usize, x: usize, y: usize, (r, g, b): Rgb) {
    let i = (y * img_w + x) * 3;
    if i + 2 < img.len() && x < img_w {
        img[i] = r;
        img[i + 1] = g;
        img[i + 2] = b;
    }
}

fn fill(img: &mut [u8], img_w: usize, x: usize, y: usize, w: usize, h: usize, c: Rgb) {
    for py in y..y + h {
        for px in x..x + w {
            put(img, img_w, px, py, c);
        }
    }
}

/// Encode frames (delay in centiseconds, buffer) into an animated GIF.
pub fn write_gif(
    path: &Path,
    px: f32,
    frames: &[(u16, Buffer)],
    fg: Rgb,
    bg: Rgb,
) -> io::Result<()> {
    let Some((_, first)) = frames.first() else {
        return Err(io::Error::other("no frames to encode"));
    };
    let mut raster = Rasterizer::new(px, fg, bg)?;
    let (img_w, img_h) = raster.frame_size(first);
    if img_w > u16::MAX as usize || img_h > u16::MAX as usize {
        return Err(io::Error::other("frame too large for GIF"));
    }

    let file = std::fs::File::create(path)?;
    let mut encoder = gif::Encoder::new(std::io::BufWriter::new(file), img_w as u16, img_h as u16, &[])
        .map_err(|e| io::Error::other(format!("gif: {e}")))?;
    encoder
        .set_repeat(gif::Repeat::Infinite)
        .map_err(|e| io::Error::other(format!("gif: {e}")))?;
    for (delay, buffer) in frames {
        let rgb = raster.rasterize(buffer);
        let mut frame = gif::Frame::from_rgb_speed(img_w as u16, img_h as u16, &rgb, 10);
        frame.delay = *delay;
        encoder
            .write_frame(&frame)
            .map_err(|e| io::Error::other(format!("gif: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    fn pixel(img: &[u8], img_w: usize, x: usize, y: usize) -> Rgb {
        let i = (y * img_w + x) * 3;
        (img[i], img[i + 1], img[i + 2])
    }

    #[test]
    fn embedded_font_covers_every_text_glyph_the_ui_draws() {
        let r = Rasterizer::new(16.0, (255, 255, 255), (0, 0, 0)).unwrap();
        let ui_text: String = ('a'..='z')
            .chain('A'..='Z')
            .chain('0'..='9')
            .chain("$%().,:—…·Σ[]/↔‹›- ▔".chars())
            .collect();
        for ch in ui_text.chars() {
            if ch == ' ' || "▔".contains(ch) {
                continue; // procedural
            }
            assert_ne!(r.regular.lookup_glyph_index(ch), 0, "regular missing {ch:?}");
            assert_ne!(r.bold.lookup_glyph_index(ch), 0, "bold missing {ch:?}");
        }
    }

    #[test]
    fn rasterizes_blocks_text_and_background() {
        let mut raster = Rasterizer::new(16.0, (200, 200, 200), (10, 10, 10)).unwrap();
        let mut buffer = Buffer::with_lines(vec!["█x "]);
        buffer.set_style(Rect::new(0, 0, 3, 1), Style::new().bg(Color::Rgb(1, 2, 3)));
        buffer.set_style(Rect::new(0, 0, 1, 1), Style::new().fg(Color::Rgb(250, 100, 0)));

        let img = raster.rasterize(&buffer);
        let (img_w, img_h) = raster.frame_size(&buffer);
        assert_eq!(img.len(), img_w * img_h * 3);

        // Full block: every pixel of cell 0 is the fg color.
        let (cw, chh) = (raster.cell_w, raster.cell_h);
        assert_eq!(pixel(&img, img_w, cw / 2, chh / 2), (250, 100, 0));
        assert_eq!(pixel(&img, img_w, 0, 0), (250, 100, 0));
        // Space cell: pure background.
        assert_eq!(pixel(&img, img_w, cw * 2 + cw / 2, chh / 2), (1, 2, 3));
        // Text cell: some non-background pixels exist ('x' is drawn).
        let cell1: Vec<Rgb> = (0..chh)
            .flat_map(|y| (0..cw).map(move |x| (cw + x, y)))
            .map(|(x, y)| pixel(&img, img_w, x, y))
            .collect();
        assert!(cell1.iter().any(|&p| p != (1, 2, 3)), "glyph pixels rendered");
    }

    #[test]
    fn writes_an_animated_gif() {
        let dir = std::env::temp_dir().join(format!("scorchtop-gif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.gif");

        let a = Buffer::with_lines(vec!["ab", "██"]);
        let b = Buffer::with_lines(vec!["cd", "▌ "]);
        write_gif(&path, 16.0, &[(3, a), (150, b)], (200, 200, 200), (10, 10, 10)).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"GIF89a"));
        assert!(bytes.len() > 100);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
