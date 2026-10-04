//! Colors for dark and light terminals, at whatever depth the terminal
//! supports. Color is never the only signal: severities and misestimates
//! also have words and symbols.

use std::env;

use ratatui::style::{Color, Modifier, Style};

/// The terminal's background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Background {
    #[default]
    Dark,
    Light,
}

/// How many colors the terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 24-bit color.
    TrueColor,
    /// The 256-color palette.
    Palette,
    /// The 16 ANSI colors.
    Basic,
    /// `NO_COLOR` or a dumb terminal: bold, dim and reverse video only.
    None,
}

impl Depth {
    /// From `NO_COLOR`, `TERM` and `COLORTERM`.
    pub fn detect() -> Self {
        let set = |name: &str| env::var_os(name).is_some_and(|value| !value.is_empty());
        let term = env::var("TERM").unwrap_or_default();
        let colorterm = env::var("COLORTERM").unwrap_or_default();
        if set("NO_COLOR") || term == "dumb" {
            Depth::None
        } else if colorterm == "truecolor" || colorterm == "24bit" {
            Depth::TrueColor
        } else if term.contains("256color") {
            Depth::Palette
        } else {
            Depth::Basic
        }
    }
}

/// The styles the viewer uses.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub text: Style,
    pub dim: Style,
    pub title: Style,
    pub border: Style,
    pub focused_border: Style,
    pub selected: Style,
    pub hot: Style,
    pub warm: Style,
    pub bar: Style,
    pub high: Style,
    pub medium: Style,
    pub low: Style,
    pub matched: Style,
    pub key: Style,
}

impl Theme {
    pub fn new(background: Background, depth: Depth) -> Self {
        // (true color, 256-color palette, basic) for each role.
        let pick = |rgb: (u8, u8, u8), indexed: u8, basic: Color| match depth {
            Depth::TrueColor => Some(Color::Rgb(rgb.0, rgb.1, rgb.2)),
            Depth::Palette => Some(Color::Indexed(indexed)),
            Depth::Basic => Some(basic),
            Depth::None => None,
        };
        let fg = |color: Option<Color>| match color {
            Some(color) => Style::new().fg(color),
            None => Style::new(),
        };
        let dark = background == Background::Dark;
        let red = pick(
            if dark { (240, 98, 98) } else { (190, 30, 30) },
            if dark { 203 } else { 160 },
            Color::Red,
        );
        let yellow = pick(
            if dark { (229, 192, 90) } else { (150, 100, 0) },
            if dark { 221 } else { 136 },
            Color::Yellow,
        );
        let blue = pick(
            if dark { (110, 160, 230) } else { (30, 90, 170) },
            if dark { 75 } else { 25 },
            Color::Blue,
        );
        let gray = pick(
            if dark {
                (130, 130, 140)
            } else {
                (110, 110, 120)
            },
            if dark { 245 } else { 243 },
            Color::DarkGray,
        );
        let selection = pick(
            if dark { (50, 60, 85) } else { (205, 220, 245) },
            if dark { 237 } else { 189 },
            Color::Blue,
        );
        let selected = match (depth, selection) {
            (Depth::None, _) | (_, None) => Style::new().add_modifier(Modifier::REVERSED),
            (Depth::Basic, Some(color)) => Style::new().bg(color).fg(Color::White),
            (_, Some(color)) => Style::new().bg(color),
        };
        let dim = match depth {
            Depth::None => Style::new().add_modifier(Modifier::DIM),
            _ => fg(gray),
        };
        Theme {
            text: Style::new(),
            dim,
            title: Style::new().add_modifier(Modifier::BOLD),
            border: dim,
            focused_border: fg(blue).add_modifier(Modifier::BOLD),
            selected,
            hot: fg(red),
            warm: fg(yellow),
            bar: fg(blue),
            high: fg(red).add_modifier(Modifier::BOLD),
            medium: fg(yellow),
            low: dim,
            matched: fg(yellow).add_modifier(Modifier::UNDERLINED),
            key: fg(blue).add_modifier(Modifier::BOLD),
        }
    }

    /// The style for a share of the runtime.
    pub fn share(&self, fraction: f64) -> Style {
        if fraction >= 0.5 {
            self.hot
        } else if fraction >= 0.2 {
            self.warm
        } else {
            self.bar
        }
    }
}
