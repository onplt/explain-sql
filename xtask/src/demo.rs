//! `cargo xtask demo`: the README's demo, an animated SVG of the viewer on
//! the bundled demo plan. Each frame is the viewer as the tests draw it,
//! after a scripted key, so the recording is deterministic and needs no
//! terminal recorder.

use std::fmt::Write as _;
use std::fs;

use explainsql_tui::{App, Background, Depth, Key, Theme};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use crate::fixtures::workspace_root;

const WIDTH: u16 = 104;
const HEIGHT: u16 = 36;
/// Pixels per terminal cell.
const CELL_WIDTH: f64 = 8.4;
const CELL_HEIGHT: f64 = 18.0;
const FONT_SIZE: f64 = 14.0;
/// Room for the window's title bar.
const TOP: f64 = 34.0;
const PADDING: f64 = 12.0;
const FOREGROUND: &str = "#d4d4d8";
const BACKGROUND: &str = "#18181b";

/// The keys pressed, the caption shown, and how long each frame stays.
const SCRIPT: [(&[Key], &str, f64); 7] = [
    (&[], "explainsql --demo", 3.0),
    (
        &[Key::Char('j'), Key::Char('j')],
        "j j  move to the slowest node",
        2.5,
    ),
    (&[Key::Tab], "Tab  browse the findings", 2.5),
    (&[Key::Char('j')], "j  the next finding", 2.5),
    (&[Key::Enter], "Enter  go to its node", 2.0),
    (&[Key::Char('i')], "i  the suggested index", 3.5),
    (&[Key::Char('?')], "?  all the keys", 3.0),
];

pub fn demo(check: bool) -> Result<(), String> {
    let root = workspace_root();
    let source = root.join("crates/explainsql/demo/plan.txt");
    let text = fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    let plan = explainsql_core::parse(&text).map_err(|e| e.to_string())?;
    let analysis = explainsql_core::analyze(&plan);
    let mut app = App::new(plan, analysis);
    let theme = Theme::new(Background::Dark, Depth::TrueColor);

    let mut frames = Vec::new();
    for (keys, caption, seconds) in SCRIPT {
        for &key in keys {
            app.handle(key, 10);
        }
        let buffer = explainsql_tui::render(&mut app, &theme, WIDTH, HEIGHT);
        frames.push((frame(&buffer), caption, seconds));
    }
    let svg = svg(&frames);

    let path = root.join("docs/demo.svg");
    if check {
        let current = fs::read_to_string(&path).unwrap_or_default();
        if current != svg {
            return Err(format!(
                "{} is out of date (run cargo xtask demo)",
                path.display()
            ));
        }
        println!("{} is up to date", path.display());
    } else {
        fs::write(&path, &svg).map_err(|e| format!("{}: {e}", path.display()))?;
        println!(
            "wrote {} ({} frames, {} bytes)",
            path.display(),
            frames.len(),
            svg.len()
        );
    }
    Ok(())
}

/// One frame as SVG elements: background rectangles, then text runs.
fn frame(buffer: &Buffer) -> String {
    let mut out = String::new();
    let area = buffer.area;
    for y in 0..area.height {
        let top = TOP + PADDING + f64::from(y) * CELL_HEIGHT;
        // Background runs.
        let mut x = 0;
        while x < area.width {
            let background = cell_colors(buffer, x, y).1;
            let start = x;
            while x < area.width && cell_colors(buffer, x, y).1 == background {
                x += 1;
            }
            if let Some(background) = background {
                let _ = write!(
                    out,
                    r#"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" fill="{background}"/>"#,
                    PADDING + f64::from(start) * CELL_WIDTH,
                    top,
                    f64::from(x - start) * CELL_WIDTH,
                    CELL_HEIGHT
                );
            }
        }
        // Text runs of one style, without trailing blanks.
        let mut x = 0;
        while x < area.width {
            let style = text_style(buffer, x, y);
            let start = x;
            let mut text = String::new();
            while x < area.width && text_style(buffer, x, y) == style {
                text.push_str(buffer[(x, y)].symbol());
                x += 1;
            }
            let trimmed = text.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let leading = trimmed.chars().take_while(|c| *c == ' ').count();
            let shown = &trimmed[leading..];
            let columns = shown.chars().count();
            let _ = write!(
                out,
                r#"<text x="{:.1}" y="{:.1}" textLength="{:.1}" lengthAdjust="spacingAndGlyphs"{}>{}</text>"#,
                PADDING + (f64::from(start) + leading as f64) * CELL_WIDTH,
                top + CELL_HEIGHT * 0.75,
                columns as f64 * CELL_WIDTH,
                style,
                escape(shown)
            );
        }
    }
    out
}

/// Foreground and background of a cell, with reverse video applied.
fn cell_colors(buffer: &Buffer, x: u16, y: u16) -> (Option<String>, Option<String>) {
    let cell = &buffer[(x, y)];
    let (fg, bg) = (hex(cell.fg), hex(cell.bg));
    if cell.modifier.contains(Modifier::REVERSED) {
        (
            Some(bg.unwrap_or_else(|| BACKGROUND.to_owned())),
            Some(fg.unwrap_or_else(|| FOREGROUND.to_owned())),
        )
    } else {
        (fg, bg)
    }
}

/// The SVG attributes of a cell's text.
fn text_style(buffer: &Buffer, x: u16, y: u16) -> String {
    let cell = &buffer[(x, y)];
    let mut style = String::new();
    if let Some(color) = cell_colors(buffer, x, y).0 {
        let _ = write!(style, r#" fill="{color}""#);
    }
    if cell.modifier.contains(Modifier::BOLD) {
        style.push_str(r#" font-weight="bold""#);
    }
    if cell.modifier.contains(Modifier::DIM) {
        style.push_str(r#" opacity="0.6""#);
    }
    if cell.modifier.contains(Modifier::UNDERLINED) {
        style.push_str(r#" text-decoration="underline""#);
    }
    style
}

fn hex(color: Color) -> Option<String> {
    let rgb = match color {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (229, 229, 229),
        Color::DarkGray => (102, 102, 102),
        Color::White => (255, 255, 255),
        _ => (212, 212, 216),
    };
    Some(format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The frames in a terminal window, each shown in turn, forever.
fn svg(frames: &[(String, &str, f64)]) -> String {
    let width = PADDING * 2.0 + f64::from(WIDTH) * CELL_WIDTH;
    let height = TOP + PADDING * 2.0 + f64::from(HEIGHT) * CELL_HEIGHT;
    let total: f64 = frames.iter().map(|(_, _, seconds)| seconds).sum();
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.0} {height:.0}" fill="{FOREGROUND}" font-family="ui-monospace, 'SFMono-Regular', Menlo, Consolas, 'DejaVu Sans Mono', monospace" font-size="{FONT_SIZE}">
<title>ExplainSQL showing the demo plan: the verdict, the plan tree, the findings and a suggested index</title>
<style>
.frame {{ opacity: 0; animation: show {total}s step-end infinite; }}
text {{ white-space: pre; }}
"#
    );
    let mut start = 0.0;
    for (index, (_, _, seconds)) in frames.iter().enumerate() {
        let from = start / total * 100.0;
        let to = (start + seconds) / total * 100.0;
        let _ = writeln!(
            out,
            ".f{index} {{ animation-name: f{index}; }}\n@keyframes f{index} {{ 0% {{ opacity: 0; }} {from:.3}% {{ opacity: 1; }} {to:.3}% {{ opacity: 0; }} }}"
        );
        start += seconds;
    }
    out.push_str("</style>\n");
    let _ = writeln!(
        out,
        r##"<rect width="{width:.0}" height="{height:.0}" rx="10" fill="{BACKGROUND}"/>
<circle cx="20" cy="17" r="6" fill="#ff5f57"/><circle cx="40" cy="17" r="6" fill="#febc2e"/><circle cx="60" cy="17" r="6" fill="#28c840"/>"##
    );
    for (index, (body, caption, _)) in frames.iter().enumerate() {
        let _ = writeln!(
            out,
            r#"<g class="frame f{index}"><text x="{:.1}" y="22" text-anchor="middle" opacity="0.7">{}</text>{body}</g>"#,
            width / 2.0,
            escape(caption)
        );
    }
    out.push_str("</svg>\n");
    out
}
