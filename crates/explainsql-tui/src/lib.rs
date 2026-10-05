//! The interactive viewer: the plan tree with where the time went, the
//! details of each node and the findings, in the terminal.

mod app;
mod theme;
mod ui;

use std::io::{self, Write};

use explainsql_core::Analysis;
use explainsql_core::ir::Plan;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

pub use app::{App, Key, Outcome};
pub use theme::{Background, Depth, Theme};

/// How the viewer looks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub background: Background,
    /// Detected from the environment when `None`.
    pub depth: Option<Depth>,
}

/// Shows a plan until the user quits. Keys are read from the terminal even
/// when the plan came from standard input.
pub fn run(plan: Plan, analysis: Analysis, options: Options) -> io::Result<()> {
    let theme = Theme::new(
        options.background,
        options.depth.unwrap_or_else(Depth::detect),
    );
    let mut app = App::new(plan, analysis);
    // Restores the terminal on panic as well.
    let mut terminal = ratatui::try_init()?;
    let result = (|| loop {
        terminal.draw(|frame| ui::draw(frame, &mut app, &theme))?;
        let Event::Key(key) = event::read()? else {
            // Resizes and other events just redraw.
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Ok(());
        }
        let key = match key.code {
            KeyCode::Char(c) => Key::Char(c),
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            KeyCode::Tab | KeyCode::BackTab => Key::Tab,
            KeyCode::Backspace => Key::Backspace,
            _ => continue,
        };
        let page = app.tree_height.saturating_sub(1);
        match app.handle(key, page) {
            Outcome::Quit => return Ok(()),
            Outcome::Copy(text) => copy(&text)?,
            Outcome::Continue => {}
        }
    })();
    ratatui::try_restore()?;
    result
}

/// Puts text on the clipboard with the OSC 52 escape sequence, which
/// terminals support over SSH and inside tmux.
fn copy(text: &str) -> io::Result<()> {
    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &byte)| n | u32::from(byte) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Draws one frame into a buffer, for tests and benchmarks.
pub fn render(app: &mut App, theme: &Theme, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("a test backend never fails");
    terminal
        .draw(|frame| ui::draw(frame, app, theme))
        .expect("a test backend never fails");
    terminal.backend().buffer().clone()
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_base64() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
