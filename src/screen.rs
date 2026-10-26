//! Cell canvas for the terminal UI. Panels, meters and braille graphs are drawn into a
//! grid, then written as one frame that repaints only the rows that changed, which keeps
//! a 10 Hz spectrum cheap over `adb shell`.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const TEXT: Rgb = Rgb(220, 222, 228);
pub const DIM: Rgb = Rgb(112, 118, 132);
pub const BORDER: Rgb = Rgb(72, 80, 96);
pub const TITLE: Rgb = Rgb(236, 238, 244);
pub const ACCENT: Rgb = Rgb(30, 215, 96);
pub const KEY: Rgb = Rgb(240, 150, 90);
pub const WARN: Rgb = Rgb(236, 196, 72);
pub const ERROR: Rgb = Rgb(240, 92, 84);
pub const INFO: Rgb = Rgb(96, 176, 240);
pub const AIRPLAY: Rgb = Rgb(186, 132, 246);
pub const YOUTUBE: Rgb = Rgb(255, 72, 112);
/// Low to high, as btop colours its meters.
pub const HEAT: [Rgb; 3] = [Rgb(30, 215, 96), Rgb(236, 204, 64), Rgb(240, 84, 72)];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Colors {
    True,
    Ansi256,
}
impl Colors {
    /// adb does not forward the desktop environment, so the host wrapper passes COLORTERM.
    pub fn detect() -> Self {
        match std::env::var("COLORTERM").as_deref() {
            Ok("truecolor" | "24bit") => Self::True,
            _ => Self::Ansi256,
        }
    }
    fn foreground(self, c: Rgb, out: &mut String) {
        use std::fmt::Write;
        let _ = match self {
            Self::True => write!(out, "\x1b[38;2;{};{};{}m", c.0, c.1, c.2),
            Self::Ansi256 => {
                let level = |v: u8| (u16::from(v) * 5 + 127) / 255;
                write!(
                    out,
                    "\x1b[38;5;{}m",
                    16 + 36 * level(c.0) + 6 * level(c.1) + level(c.2)
                )
            }
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Rgb,
    pub bold: bool,
}
impl Style {
    pub const fn new(fg: Rgb) -> Self {
        Self { fg, bold: false }
    }
    pub const fn bold(fg: Rgb) -> Self {
        Self { fg, bold: true }
    }
}

/// Columns a character takes; 0 for characters that must not reach the terminal.
pub fn width(c: char) -> usize {
    let v = c as u32;
    if c.is_control() || (0x300..0x370).contains(&v) || (0x200b..0x2010).contains(&v) {
        0
    } else if (0x1100..0x1160).contains(&v)
        || (0x2e80..0xa4d0).contains(&v)
        || (0xac00..0xd7a4).contains(&v)
        || (0xf900..0xfb00).contains(&v)
        || (0xfe30..0xfe50).contains(&v)
        || (0xff00..0xff61).contains(&v)
        || (0xffe0..0xffe7).contains(&v)
        || (0x1f300..0x1f650).contains(&v)
        || (0x1f900..0x1fa00).contains(&v)
        || (0x20000..0x3fffe).contains(&v)
    {
        2
    } else {
        1
    }
}
pub fn text_width(text: &str) -> usize {
    text.chars().map(width).sum()
}

/// Breaks `text` into lines of at most `limit` columns at spaces; a word longer than a line,
/// such as a login URL, is split between characters.
pub fn wrap(text: &str, limit: usize) -> Vec<String> {
    // A line that fits keeps its spacing, which menus use to align columns.
    if text_width(text) <= limit {
        return vec![text.to_owned()];
    }
    let mut lines = vec![];
    let mut line = String::new();
    let mut used = 0;
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let w = text_width(word);
        if used > 0 && used + 1 + w <= limit {
            line.push(' ');
            line.push_str(word);
            used += 1 + w;
            continue;
        }
        if used > 0 {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for c in word.chars() {
            let cw = width(c);
            if used + cw > limit && used > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += cw;
        }
    }
    if used > 0 || lines.is_empty() {
        lines.push(line);
    }
    lines
}
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let channel = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb(channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
}
pub fn gradient(stops: &[Rgb], t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let i = (t.floor() as usize).min(stops.len() - 2);
    mix(stops[i], stops[i + 1], t - i as f32)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    /// '\0' marks the second column of a wide character.
    ch: char,
    style: Style,
}

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    cells: Vec<Cell>,
}
impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![
                Cell {
                    ch: ' ',
                    style: Style::new(TEXT)
                };
                width * height
            ],
        }
    }
    pub fn put(&mut self, x: usize, y: usize, ch: char, style: Style) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = Cell { ch, style };
        }
    }
    /// Writes `text` from `x`, clipped to `limit` columns; returns the columns used.
    pub fn text(&mut self, x: usize, y: usize, limit: usize, text: &str, style: Style) -> usize {
        let limit = limit.min(self.width.saturating_sub(x));
        let mut used = 0;
        for c in text.chars() {
            let w = width(c);
            if w == 0 {
                continue;
            }
            if used + w > limit {
                break;
            }
            self.put(x + used, y, c, style);
            if w == 2 {
                self.put(x + used + 1, y, '\0', style);
            }
            used += w;
        }
        used
    }
    /// Text that ends at column `right` (exclusive).
    pub fn text_right(&mut self, right: usize, y: usize, text: &str, style: Style) {
        let w = text_width(text).min(right);
        self.text(right - w, y, w, text, style);
    }
    /// A rounded panel with a title in the top edge and key hints in the bottom edge.
    pub fn panel(&mut self, area: Area, title: &str, keys: &[(&str, &str)]) {
        let Area { x, y, w, h } = area;
        if w < 4 || h < 2 {
            return;
        }
        let border = Style::new(BORDER);
        for i in 1..w - 1 {
            self.put(x + i, y, '─', border);
            self.put(x + i, y + h - 1, '─', border);
        }
        for j in 1..h - 1 {
            self.put(x, y + j, '│', border);
            self.put(x + w - 1, y + j, '│', border);
        }
        self.put(x, y, '╭', border);
        self.put(x + w - 1, y, '╮', border);
        self.put(x, y + h - 1, '╰', border);
        self.put(x + w - 1, y + h - 1, '╯', border);
        let room = w.saturating_sub(6);
        if !title.is_empty() && room > 0 {
            let used = self.text(x + 3, y, room, title, Style::bold(TITLE));
            self.put(x + 2, y, ' ', border);
            self.put(x + 3 + used, y, ' ', border);
        }
        let mut cursor = x + 2;
        let end = x + w - 2;
        for (key, label) in keys {
            let need = text_width(key) + text_width(label) + 2;
            if cursor + need + 1 > end {
                break;
            }
            self.put(cursor, y + h - 1, ' ', border);
            cursor += 1;
            cursor += self.text(cursor, y + h - 1, end - cursor, key, Style::bold(KEY));
            self.put(cursor, y + h - 1, ' ', border);
            cursor += 1;
            cursor += self.text(cursor, y + h - 1, end - cursor, label, Style::new(DIM));
            self.put(cursor, y + h - 1, ' ', border);
            cursor += 1;
        }
    }
    /// A horizontal bar of `glyph` cells, coloured along `stops`; the rest uses `empty`.
    pub fn bar(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        fraction: f32,
        glyphs: (char, char),
        stops: &[Rgb],
    ) {
        let filled = ((fraction.clamp(0.0, 1.0) * w as f32).round() as usize).min(w);
        for i in 0..w {
            if i < filled {
                let t = if w > 1 {
                    i as f32 / (w - 1) as f32
                } else {
                    0.0
                };
                self.put(x + i, y, glyphs.0, Style::new(gradient(stops, t)));
            } else {
                self.put(x + i, y, glyphs.1, Style::new(BORDER));
            }
        }
    }
    /// A filled braille graph of `samples` (0..=100, newest last), two samples per cell and
    /// four dots per row, coloured from the bottom up along `stops`.
    pub fn graph(&mut self, area: Area, samples: &[u8], stops: &[Rgb]) {
        let Area { x, y, w, h } = area;
        if w == 0 || h == 0 {
            return;
        }
        let dots = h * 4;
        let start = samples.len().saturating_sub(w * 2);
        let visible = &samples[start..];
        // Right-align so the newest sample sits at the right edge.
        let offset = w * 2 - visible.len();
        let height = |column: usize| -> usize {
            column
                .checked_sub(offset)
                .and_then(|i| visible.get(i))
                .map_or(0, |v| (usize::from(*v.min(&100)) * dots).div_ceil(100))
        };
        for cx in 0..w {
            let (left, right) = (height(cx * 2), height(cx * 2 + 1));
            for row in 0..h {
                let base = (h - 1 - row) * 4;
                let mut bits = 0u32;
                for dot in 0..4 {
                    let level = base + dot;
                    // Dots from the bottom of a braille cell: 7,3,2,1 left and 8,6,5,4 right.
                    if level < left {
                        bits |= [0x40, 0x04, 0x02, 0x01][dot];
                    }
                    if level < right {
                        bits |= [0x80, 0x20, 0x10, 0x08][dot];
                    }
                }
                let t = if h > 1 {
                    (h - 1 - row) as f32 / (h - 1) as f32
                } else {
                    0.0
                };
                let (ch, style) = if bits == 0 && row == h - 1 {
                    // Silence still shows as a dim baseline, so a pause between tracks
                    // does not look like a hole in the graph.
                    ('⣀', Style::new(BORDER))
                } else if bits == 0 {
                    (' ', Style::new(TEXT))
                } else {
                    (
                        char::from_u32(0x2800 + bits).unwrap_or(' '),
                        Style::new(gradient(stops, t)),
                    )
                };
                self.put(x + cx, y + row, ch, style);
            }
        }
    }
    /// The visible characters of one row.
    #[cfg(test)]
    pub fn row(&self, y: usize) -> String {
        self.cells[y * self.width..(y + 1) * self.width]
            .iter()
            .filter(|c| c.ch != '\0')
            .map(|c| c.ch)
            .collect()
    }
    fn encode_row(&self, y: usize, colors: Colors) -> String {
        let mut out = String::new();
        let mut current: Option<Style> = None;
        for cell in &self.cells[y * self.width..(y + 1) * self.width] {
            if cell.ch == '\0' {
                continue;
            }
            if current != Some(cell.style) {
                out.push_str(if cell.style.bold {
                    "\x1b[0;1m"
                } else {
                    "\x1b[0m"
                });
                colors.foreground(cell.style.fg, &mut out);
                current = Some(cell.style);
            }
            out.push(cell.ch);
        }
        out.push_str("\x1b[0m");
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Area {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}
impl Area {
    pub fn inner(self) -> Self {
        Self {
            x: self.x + 2,
            y: self.y + 1,
            w: self.w.saturating_sub(4),
            h: self.h.saturating_sub(2),
        }
    }
}

/// Remembers the last frame so only changed rows are sent.
pub struct Screen {
    colors: Colors,
    rows: Vec<String>,
    size: (usize, usize),
}
impl Screen {
    pub fn new(colors: Colors) -> Self {
        Self {
            colors,
            rows: vec![],
            size: (0, 0),
        }
    }
    pub fn frame(&mut self, canvas: &Canvas) -> String {
        let mut out = String::new();
        if self.size != (canvas.width, canvas.height) {
            out.push_str("\x1b[0m\x1b[2J");
            self.rows = vec![String::new(); canvas.height];
            self.size = (canvas.width, canvas.height);
        }
        for y in 0..canvas.height {
            let row = canvas.encode_row(y, self.colors);
            if self.rows[y] != row {
                out.push_str(&format!("\x1b[{};1H", y + 1));
                out.push_str(&row);
                self.rows[y] = row;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_is_clipped_by_columns_and_drops_control_characters() {
        let mut canvas = Canvas::new(8, 1);
        assert_eq!(canvas.text(0, 0, 8, "a\x1b[2Jb日本語", Style::new(TEXT)), 7);
        assert_eq!(canvas.row(0), "a[2Jb日 ");
        let mut canvas = Canvas::new(4, 1);
        canvas.text(0, 0, 4, "abc日", Style::new(TEXT));
        assert_eq!(canvas.row(0), "abc ");
    }
    #[test]
    fn wrapping_breaks_at_spaces_and_splits_words_longer_than_a_line() {
        assert_eq!(
            wrap("Saved Spotify login was rejected", 12),
            ["Saved", "Spotify", "login was", "rejected"]
        );
        assert_eq!(
            wrap("Open https://spotify.com/pair", 10),
            ["Open", "https://sp", "otify.com/", "pair"]
        );
        assert_eq!(wrap("日本語の歌", 4), ["日本", "語の", "歌"]);
        assert_eq!(wrap("", 5), [""]);
        assert_eq!(wrap("fits", 4), ["fits"]);
        assert_eq!(wrap("  a   [b]", 9), ["  a   [b]"]);
    }
    #[test]
    fn panels_show_titles_and_key_hints_inside_their_borders() {
        let mut canvas = Canvas::new(30, 3);
        canvas.panel(
            Area {
                x: 0,
                y: 0,
                w: 30,
                h: 3,
            },
            "now playing",
            &[("n", "next"), ("p", "prev")],
        );
        assert_eq!(canvas.row(0), format!("╭─ now playing {}╮", "─".repeat(14)));
        assert_eq!(canvas.row(1), format!("│{}│", " ".repeat(28)));
        assert_eq!(
            canvas.row(2),
            format!("╰─ n next  p prev {}╯", "─".repeat(11))
        );
    }
    #[test]
    fn graph_fills_from_the_bottom_with_the_newest_sample_on_the_right() {
        let mut canvas = Canvas::new(2, 2);
        canvas.graph(
            Area {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            &[0, 100, 50, 12],
            &HEAT,
        );
        // Columns: 0 | 100 in the first cell, 50 (four dots) | 12 (one dot) in the second.
        assert_eq!(canvas.row(0), "⢸ ");
        assert_eq!(canvas.row(1), "⢸⣇");
        let mut canvas = Canvas::new(3, 1);
        canvas.graph(
            Area {
                x: 0,
                y: 0,
                w: 3,
                h: 1,
            },
            &[100],
            &HEAT,
        );
        // Missing and silent samples sit on a baseline.
        assert_eq!(canvas.row(0), "⣀⣀⢸");
    }
    #[test]
    fn frames_repaint_only_changed_rows_and_clear_on_resize() {
        let mut screen = Screen::new(Colors::Ansi256);
        let mut canvas = Canvas::new(4, 2);
        canvas.text(0, 0, 4, "ab", Style::new(TEXT));
        let first = screen.frame(&canvas);
        assert!(first.starts_with("\x1b[0m\x1b[2J") && first.contains("\x1b[2;1H"));
        canvas.text(0, 1, 4, "cd", Style::new(ERROR));
        let second = screen.frame(&canvas);
        assert!(!second.contains("\x1b[1;1H") && second.contains("\x1b[2;1H"));
        assert_eq!(screen.frame(&canvas), "");
        assert!(
            screen
                .frame(&Canvas::new(5, 2))
                .starts_with("\x1b[0m\x1b[2J")
        );
    }
    #[test]
    fn colours_fall_back_to_the_256_colour_cube() {
        let mut out = String::new();
        Colors::Ansi256.foreground(Rgb(255, 0, 0), &mut out);
        assert_eq!(out, "\x1b[38;5;196m");
        out.clear();
        Colors::True.foreground(Rgb(1, 2, 3), &mut out);
        assert_eq!(out, "\x1b[38;2;1;2;3m");
        assert_eq!(gradient(&HEAT, 0.0), HEAT[0]);
        assert_eq!(gradient(&HEAT, 1.0), HEAT[2]);
    }
}
