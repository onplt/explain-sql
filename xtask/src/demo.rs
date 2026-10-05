//! `cargo xtask demo`: the README's demo, an animated SVG of a real session
//! with explainsql.
//!
//! `cargo xtask demo --record` records the session. It builds the release
//! binary and runs it in a tmux pane against the database named by
//! `EXPLAINSQL_TEST_DATABASE_URL` (the fixture schema, with HypoPG). It
//! types each step below, waits for the result, and saves the screen as
//! tmux shows it, colors included, to `xtask/demo/recording.json`.
//!
//! `cargo xtask demo` draws `docs/demo.svg` from that recording. Drawing
//! needs no database and gives the same SVG every time, so CI checks that
//! the SVG is current.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde_json::{Value, json};

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

/// The statement the session runs, the recording, and the drawing.
const STATEMENT: &str = "xtask/demo/slow.sql";
const RECORDING: &str = "xtask/demo/recording.json";
const DRAWING: &str = "docs/demo.svg";

/// How long a step may take to show its result.
const TIMEOUT: Duration = Duration::from_secs(30);

/// What a step types.
#[derive(Clone, Copy)]
enum Input {
    /// Text, typed as is.
    Type(&'static str),
    /// A key, by its tmux name.
    Press(&'static str),
    /// Waits until the text is on the screen.
    Wait(&'static str),
}

/// One frame: what is typed, what must be on the screen before it is
/// captured, the caption, and how long the frame stays.
struct Step {
    input: &'static [Input],
    until: &'static str,
    caption: &'static str,
    seconds: f64,
}

/// The session. `{postgres}` in a caption is the server's version. The
/// texts waited for follow `xtask/demo/slow.sql` and the viewer's screens.
const STEPS: [Step; 7] = [
    Step {
        input: &[
            Input::Type("cat slow.sql"),
            Input::Press("Enter"),
            // The end of the file, then the prompt.
            Input::Wait("GROUP BY c.name;\n$"),
            Input::Type("explainsql -d \"$DATABASE_URL\" -f slow.sql"),
        ],
        until: "-f slow.sql",
        caption: "The query: a customer's order summary",
        seconds: 4.0,
    },
    Step {
        input: &[Input::Press("Enter")],
        until: "measured with EXPLAIN ANALYZE",
        caption: "Enter  EXPLAIN ANALYZE on PostgreSQL {postgres}, rolled back",
        seconds: 3.5,
    },
    Step {
        input: &[Input::Type("1")],
        until: "Removed by filter",
        caption: "1  go to the slowest node",
        seconds: 3.5,
    },
    Step {
        input: &[Input::Type("y")],
        until: "UNUSABLE",
        caption: "y  ask the planner why it uses no index",
        seconds: 4.5,
    },
    Step {
        input: &[Input::Type("i")],
        until: "Press t to test it",
        caption: "i  the suggested index",
        seconds: 3.5,
    },
    Step {
        input: &[Input::Type("t")],
        until: "Before → after",
        caption: "t  test it with a hypothetical index (HypoPG)",
        seconds: 4.5,
    },
    Step {
        input: &[Input::Type("?")],
        until: "Any key closes this help",
        caption: "?  all the keys",
        seconds: 3.0,
    },
];

/// Draws `docs/demo.svg` from the recording, after recording it again
/// with `--record`, or checks that it is current with `--check`.
pub fn demo(option: Option<&str>) -> Result<(), String> {
    let root = workspace_root();
    let recording_path = root.join(RECORDING);
    let recording = match option {
        Some("--record") => {
            let recording = record(&root)?;
            let text = serde_json::to_string_pretty(&recording).map_err(|e| e.to_string())?;
            fs::write(&recording_path, text + "\n")
                .map_err(|e| format!("{}: {e}", recording_path.display()))?;
            println!("wrote {}", recording_path.display());
            recording
        }
        None | Some("--check") => {
            let text = fs::read_to_string(&recording_path).map_err(|e| {
                format!(
                    "{}: {e} (record it with cargo xtask demo --record)",
                    recording_path.display()
                )
            })?;
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", recording_path.display()))?
        }
        Some(other) => return Err(format!("unknown option `{other}` (--check or --record)")),
    };
    let svg = draw(&recording)?;

    let path = root.join(DRAWING);
    if option == Some("--check") {
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
        println!("wrote {} ({} bytes)", path.display(), svg.len());
    }
    Ok(())
}

/// Runs explainsql in tmux against the test database, types the steps and
/// returns the screens.
fn record(root: &Path) -> Result<Value, String> {
    let url = env::var("EXPLAINSQL_TEST_DATABASE_URL").map_err(|_| {
        "recording needs EXPLAINSQL_TEST_DATABASE_URL: a database with the fixture schema and HypoPG"
            .to_owned()
    })?;
    run(Command::new("tmux").arg("-V")).map_err(|e| format!("recording needs tmux: {e}"))?;

    println!("building explainsql");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    run(Command::new(cargo).current_dir(root).args([
        "build",
        "--release",
        "--locked",
        "--package",
        "explainsql",
    ]))?;
    let bin = root
        .join(env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()))
        .join("release");
    let explainsql = run(Command::new(bin.join("explainsql")).arg("--version"))?;
    // "16.14 (Ubuntu 16.14-1)": the version alone. Without psql, the
    // caption goes without it.
    let postgres = run(Command::new("psql").args([url.as_str(), "-XAtc", "SHOW server_version"]))
        .ok()
        .and_then(|version| version.split_whitespace().next().map(str::to_owned));

    let session = Session::start(root, &bin, &url)?;
    println!("recording");
    let mut frames = Vec::new();
    for step in &STEPS {
        for input in step.input {
            match *input {
                Input::Type(text) => session.tmux(&["send-keys", "-t", "demo", "-l", text])?,
                Input::Press(key) => session.tmux(&["send-keys", "-t", "demo", key])?,
                Input::Wait(text) => session.wait_for(text)?,
            };
        }
        session.wait_for(step.until)?;
        let screen = session.settled()?;
        let caption = match &postgres {
            Some(version) => step.caption.replace("{postgres}", version),
            None => step.caption.replace(" on PostgreSQL {postgres}", ""),
        };
        let mut lines: Vec<&str> = screen.lines().take(usize::from(HEIGHT)).collect();
        lines.resize(usize::from(HEIGHT), "");
        frames.push(json!({
            "caption": caption,
            "seconds": step.seconds,
            "screen": lines,
        }));
    }
    Ok(json!({
        "explainsql": explainsql.trim(),
        "postgres": postgres,
        "width": WIDTH,
        "height": HEIGHT,
        "frames": frames,
    }))
}

/// A tmux server running an interactive shell in a pane of the demo's
/// size, and the directory the shell starts in, which holds the server's
/// socket and the database URL. Dropping it stops the server and removes
/// the directory.
struct Session {
    dir: PathBuf,
}

impl Session {
    fn start(root: &Path, bin: &Path, url: &str) -> Result<Self, String> {
        let session = Session {
            dir: env::temp_dir().join(format!("explainsql-demo-{}", std::process::id())),
        };
        fs::create_dir_all(&session.dir).map_err(|e| format!("{}: {e}", session.dir.display()))?;
        fs::copy(root.join(STATEMENT), session.dir.join("slow.sql"))
            .map_err(|e| format!("{STATEMENT}: {e}"))?;
        let rc = session.dir.join("rc");
        fs::write(
            &rc,
            format!(
                "PS1='$ '\nexport PATH={}:\"$PATH\"\nexport COLORTERM=truecolor\nexport DATABASE_URL={}\nunset NO_COLOR HISTFILE\n",
                quote(&bin.display().to_string()),
                quote(url)
            ),
        )
        .map_err(|e| format!("{}: {e}", rc.display()))?;
        let shell = format!(
            "bash --noprofile --rcfile {} -i",
            quote(&rc.display().to_string())
        );
        let (width, height) = (WIDTH.to_string(), HEIGHT.to_string());
        let dir = session.dir.display().to_string();
        session.tmux(&[
            "-f",
            "/dev/null",
            "-u",
            "new-session",
            "-d",
            "-s",
            "demo",
            "-x",
            &width,
            "-y",
            &height,
            "-c",
            &dir,
            &shell,
            ";",
            "set-option",
            "-g",
            "status",
            "off",
        ])?;
        session.wait_for("$")?;
        Ok(session)
    }

    fn tmux(&self, args: &[&str]) -> Result<String, String> {
        run(Command::new("tmux")
            .arg("-S")
            .arg(self.dir.join("tmux"))
            .args(args))
    }

    /// The screen as text, or with its escape sequences and trailing
    /// blanks, so that colors and backgrounds survive.
    fn screen(&self, colors: bool) -> Result<String, String> {
        if colors {
            self.tmux(&["capture-pane", "-p", "-e", "-N", "-t", "demo"])
        } else {
            self.tmux(&["capture-pane", "-p", "-t", "demo"])
        }
    }

    fn wait_for(&self, text: &str) -> Result<String, String> {
        let start = Instant::now();
        loop {
            let screen = self.screen(false)?;
            if screen.contains(text) {
                return Ok(screen);
            }
            if start.elapsed() > TIMEOUT {
                return Err(format!(
                    "waited {} s for {text:?}; the screen shows:\n{screen}",
                    TIMEOUT.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// The screen, once it stops changing.
    fn settled(&self) -> Result<String, String> {
        let start = Instant::now();
        let mut last = self.screen(true)?;
        loop {
            thread::sleep(Duration::from_millis(300));
            let screen = self.screen(true)?;
            if screen == last {
                return Ok(screen);
            }
            if start.elapsed() > TIMEOUT {
                return Err(format!(
                    "the screen kept changing for {} s",
                    TIMEOUT.as_secs()
                ));
            }
            last = screen;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Runs a command and returns its standard output.
fn run(command: &mut Command) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Quotes a word for the shell.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The demo as an animated SVG, from its recording.
fn draw(recording: &Value) -> Result<String, String> {
    let size = (recording["width"].as_u64(), recording["height"].as_u64());
    if size != (Some(u64::from(WIDTH)), Some(u64::from(HEIGHT))) {
        return Err(format!(
            "the recording is not {WIDTH}×{HEIGHT}: record it again"
        ));
    }
    let explainsql = recording["explainsql"].as_str().unwrap_or("explainsql");
    let source = match recording["postgres"].as_str() {
        Some(version) => {
            format!("Recorded from a real run of {explainsql} against PostgreSQL {version}.")
        }
        None => format!("Recorded from a real run of {explainsql}."),
    };
    let mut frames = Vec::new();
    for recorded in recording["frames"].as_array().into_iter().flatten() {
        let caption = recorded["caption"]
            .as_str()
            .ok_or("a frame has no caption")?;
        let seconds = recorded["seconds"]
            .as_f64()
            .ok_or("a frame has no duration")?;
        let screen: Vec<&str> = recorded["screen"]
            .as_array()
            .ok_or("a frame has no screen")?
            .iter()
            .map(|line| line.as_str().unwrap_or_default())
            .collect();
        frames.push((frame(&buffer(&screen)), caption.to_owned(), seconds));
    }
    if frames.is_empty() {
        return Err("the recording has no frames".to_owned());
    }
    Ok(svg(&frames, &source))
}

/// A screen captured with its escape sequences, as cells. The style
/// carries over from one line to the next, as on the terminal.
fn buffer(screen: &[&str]) -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut style = Style::default();
    for (y, line) in (0..HEIGHT).zip(screen) {
        let mut x = 0;
        let mut rest = *line;
        while !rest.is_empty() {
            if let Some(sequence) = rest.strip_prefix("\x1b[") {
                let end = sequence
                    .find(|c: char| c.is_ascii_alphabetic())
                    .unwrap_or(sequence.len());
                if sequence[end..].starts_with('m') {
                    style = sgr(style, &sequence[..end]);
                }
                rest = sequence.get(end + 1..).unwrap_or_default();
                continue;
            }
            if let Some(after) = rest.strip_prefix('\x1b') {
                // Not a style: skip the escape and the character after it.
                let mut chars = after.chars();
                chars.next();
                rest = chars.as_str();
                continue;
            }
            let end = rest.find('\x1b').unwrap_or(rest.len());
            if x < WIDTH {
                x = buffer
                    .set_stringn(x, y, &rest[..end], usize::from(WIDTH - x), style)
                    .0;
            }
            rest = &rest[end..];
        }
    }
    buffer
}

/// Applies a Select Graphic Rendition sequence, such as `1;38;2;110;160;230`.
fn sgr(mut style: Style, parameters: &str) -> Style {
    let mut codes = parameters
        .split(';')
        .map(|code| code.parse::<u16>().unwrap_or(0));
    while let Some(code) = codes.next() {
        match code {
            0 => style = Style::default(),
            1 => style.add_modifier.insert(Modifier::BOLD),
            2 => style.add_modifier.insert(Modifier::DIM),
            3 => style.add_modifier.insert(Modifier::ITALIC),
            4 => style.add_modifier.insert(Modifier::UNDERLINED),
            7 => style.add_modifier.insert(Modifier::REVERSED),
            22 => style.add_modifier.remove(Modifier::BOLD | Modifier::DIM),
            23 => style.add_modifier.remove(Modifier::ITALIC),
            24 => style.add_modifier.remove(Modifier::UNDERLINED),
            27 => style.add_modifier.remove(Modifier::REVERSED),
            30..=37 | 90..=97 => style.fg = Some(Color::Indexed(basic(code % 10, code >= 90))),
            40..=47 | 100..=107 => style.bg = Some(Color::Indexed(basic(code % 10, code >= 100))),
            38 | 48 => {
                let color = match codes.next() {
                    Some(5) => codes.next().map(|index| Color::Indexed(index as u8)),
                    Some(2) => match (codes.next(), codes.next(), codes.next()) {
                        (Some(r), Some(g), Some(b)) => Some(Color::Rgb(r as u8, g as u8, b as u8)),
                        _ => None,
                    },
                    _ => None,
                };
                if code == 38 {
                    style.fg = color;
                } else {
                    style.bg = color;
                }
            }
            39 => style.fg = None,
            49 => style.bg = None,
            _ => {}
        }
    }
    style
}

/// The palette index of one of the eight basic colors, or its bright form.
fn basic(color: u16, bright: bool) -> u8 {
    color as u8 + if bright { 8 } else { 0 }
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
    if cell.modifier.contains(Modifier::ITALIC) {
        style.push_str(r#" font-style="italic""#);
    }
    if cell.modifier.contains(Modifier::UNDERLINED) {
        style.push_str(r#" text-decoration="underline""#);
    }
    style
}

fn hex(color: Color) -> Option<String> {
    let index = match color {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => return Some(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(index) => index,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
    };
    let (r, g, b) = palette(index);
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

/// The xterm 256-color palette, with the sixteen basic colors of a dark
/// terminal theme.
fn palette(index: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => BASIC[usize::from(index)],
        16..=231 => {
            let cube = index - 16;
            (
                LEVELS[usize::from(cube / 36)],
                LEVELS[usize::from(cube / 6 % 6)],
                LEVELS[usize::from(cube % 6)],
            )
        }
        _ => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The frames in a terminal window, each shown in turn, forever.
fn svg(frames: &[(String, String, f64)], source: &str) -> String {
    let width = PADDING * 2.0 + f64::from(WIDTH) * CELL_WIDTH;
    let height = TOP + PADDING * 2.0 + f64::from(HEIGHT) * CELL_HEIGHT;
    let total: f64 = frames.iter().map(|(_, _, seconds)| seconds).sum();
    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.0} {height:.0}" fill="{FOREGROUND}" font-family="ui-monospace, 'SFMono-Regular', Menlo, Consolas, 'DejaVu Sans Mono', monospace" font-size="{FONT_SIZE}">
<title>ExplainSQL on a slow query: the verdict, the slowest node, why the planner uses no index, and the suggested index tested with HypoPG</title>
<desc>{}</desc>
<style>
.frame {{ opacity: 0; animation: show {total}s step-end infinite; }}
text {{ white-space: pre; }}
"#,
        escape(source)
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
