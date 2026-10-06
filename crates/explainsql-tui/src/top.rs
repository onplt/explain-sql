//! The list of a database's costliest statements, from pg_stat_statements:
//! pick one to see its plan, or to try values for its parameters.

use explainsql_core::format;
use explainsql_core::top::Entry;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::Key;
use crate::theme::Theme;

/// What the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    /// Show the plan of the statement at this index.
    Plan(usize),
    /// Try values for its parameters (`--params`).
    Values(usize),
    Quit,
}

/// The list and where the user is in it.
pub struct List {
    pub entries: Vec<Entry>,
    /// Where the statements come from: `user@host:port/db, PostgreSQL 16`.
    pub source: String,
    /// The server plans statements with parameters without their values
    /// (`EXPLAIN (GENERIC_PLAN)`, PostgreSQL 16 or later). Before, Enter
    /// tries values for them.
    pub generic: bool,
    /// Trying values runs the statement (`--measure`), in a transaction
    /// that is rolled back.
    pub measure: bool,
    pub selected: usize,
    /// The first row on screen.
    pub offset: usize,
    /// A one-line notice in the status bar.
    pub message: Option<String>,
    /// Rows of the list on screen at the last frame, for paging.
    pub height: usize,
}

impl List {
    pub fn new(entries: Vec<Entry>, source: String, generic: bool) -> Self {
        List {
            entries,
            source,
            generic,
            measure: false,
            selected: 0,
            offset: 0,
            message: None,
            height: 10,
        }
    }

    /// What a key does; `Some` when the list should close.
    pub fn handle(&mut self, key: Key) -> Option<Picked> {
        self.message = None;
        let last = self.entries.len().saturating_sub(1);
        let page = self.height.saturating_sub(1).max(1);
        match key {
            Key::Char('q') | Key::Esc => return Some(Picked::Quit),
            Key::Down | Key::Char('j') => self.selected = (self.selected + 1).min(last),
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::PageDown => self.selected = (self.selected + page).min(last),
            Key::PageUp => self.selected = self.selected.saturating_sub(page),
            Key::Home | Key::Char('g') => self.selected = 0,
            Key::End | Key::Char('G') => self.selected = last,
            Key::Enter | Key::Char('l') | Key::Right => return self.plan(),
            Key::Char('p') => return self.values(),
            _ => {}
        }
        None
    }

    fn plan(&mut self) -> Option<Picked> {
        let entry = self.entries.get(self.selected)?;
        if let Some(reason) = &entry.unplannable {
            self.message = Some(format!("This statement cannot be planned: {reason}."));
            return None;
        }
        // Without values, $1 can be planned from PostgreSQL 16 only.
        if !self.generic && parameters(entry) > 0 {
            return Some(Picked::Values(self.selected));
        }
        Some(Picked::Plan(self.selected))
    }

    fn values(&mut self) -> Option<Picked> {
        let entry = self.entries.get(self.selected)?;
        if let Some(reason) = &entry.unplannable {
            self.message = Some(format!("This statement cannot be planned: {reason}."));
            return None;
        }
        if parameters(entry) == 0 {
            self.message =
                Some("This statement has no parameters: Enter shows its plan.".to_owned());
            return None;
        }
        Some(Picked::Values(self.selected))
    }
}

/// How many `$n` parameters a statement takes.
fn parameters(entry: &Entry) -> usize {
    explainsql_core::params::placeholders(&entry.query).count
}

const TOTAL_WIDTH: usize = 9;
const SHARE_WIDTH: usize = 6;
const CALLS_WIDTH: usize = 11;
const MEAN_WIDTH: usize = 9;
const PAGES_WIDTH: usize = 13;
const TEMP_WIDTH: usize = 9;

pub fn draw(frame: &mut Frame, list: &mut List, theme: &Theme) {
    let area = frame.area();
    if area.width < 40 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("The terminal is too small for the list. Press q to quit.")
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let details = if area.height >= 30 { 9 } else { 6 };
    let [top, body, bottom, status] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(details),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!("The costliest statements in {}", list.source),
                theme.title,
            ),
            Line::styled(
                if list.measure {
                    "By total execution time (pg_stat_statements). Measured runs are rolled back."
                } else {
                    "By total execution time (pg_stat_statements). Enter plans it without running it."
                },
                theme.dim,
            ),
        ]),
        top,
    );
    draw_list(frame, list, theme, body);
    draw_details(frame, list, theme, bottom);
    let line = match &list.message {
        Some(message) => Line::raw(message.clone()),
        None => {
            let mut spans = Vec::new();
            for (key, what) in [
                ("j/k", "move"),
                ("Enter", "plan"),
                (
                    "p",
                    if list.measure {
                        "measure values"
                    } else {
                        "try values"
                    },
                ),
                ("q", "quit"),
            ] {
                spans.push(Span::styled(key, theme.key));
                spans.push(Span::styled(format!(" {what}  "), theme.dim));
            }
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(line), status);
}

fn draw_list(frame: &mut Frame, list: &mut List, theme: &Theme, area: Rect) {
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(theme.focused_border)
        .title(Span::styled(
            format!(" Statements ({}) ", list.entries.len()),
            theme.title,
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if list.entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No statements yet: pg_stat_statements has counted none in this database.",
                theme.dim,
            )),
            inner,
        );
        return;
    }
    let height = usize::from(inner.height).saturating_sub(1).max(1);
    list.height = height;
    if list.selected < list.offset {
        list.offset = list.selected;
    } else if list.selected >= list.offset + height {
        list.offset = list.selected + 1 - height;
    }
    let width = usize::from(inner.width);
    let fixed = 2
        + TOTAL_WIDTH
        + 1
        + SHARE_WIDTH
        + 1
        + CALLS_WIDTH
        + 1
        + MEAN_WIDTH
        + 1
        + PAGES_WIDTH
        + 1
        + TEMP_WIDTH
        + 2;
    let wide = width >= fixed + 30;
    let header = if wide {
        format!(
            "  {:>TOTAL_WIDTH$} {:>SHARE_WIDTH$} {:>CALLS_WIDTH$} {:>MEAN_WIDTH$} {:>PAGES_WIDTH$} {:>TEMP_WIDTH$}  Statement",
            "Total", "Share", "Calls", "Mean", "Pages", "Temp"
        )
    } else {
        format!(
            "  {:>TOTAL_WIDTH$} {:>CALLS_WIDTH$} {:>MEAN_WIDTH$}  Statement",
            "Total", "Calls", "Mean"
        )
    };
    let used = header.chars().count() - "Statement".len();
    let mut lines = vec![Line::styled(header, theme.dim)];
    for (index, entry) in list
        .entries
        .iter()
        .enumerate()
        .skip(list.offset)
        .take(height)
    {
        let share = entry.share;
        let marker = if entry.unplannable.is_some() {
            "– "
        } else {
            "  "
        };
        let figures = if wide {
            format!(
                "{:>TOTAL_WIDTH$} {:>SHARE_WIDTH$} {:>CALLS_WIDTH$} {:>MEAN_WIDTH$} {:>PAGES_WIDTH$} {:>TEMP_WIDTH$}  ",
                format::duration(entry.total_ms),
                format::percent(share),
                format::grouped(entry.calls),
                format::duration(entry.mean_ms),
                format::grouped(entry.pages()),
                if entry.temp_written > 0 {
                    format::grouped(entry.temp_written)
                } else {
                    String::new()
                },
            )
        } else {
            format!(
                "{:>TOTAL_WIDTH$} {:>CALLS_WIDTH$} {:>MEAN_WIDTH$}  ",
                format::duration(entry.total_ms),
                format::grouped(entry.calls),
                format::duration(entry.mean_ms),
            )
        };
        let text = fit(&entry.one_line(), width.saturating_sub(used));
        let line = Line::from(vec![
            Span::styled(marker, theme.dim),
            Span::styled(figures, theme.share(share)),
            Span::styled(
                text,
                if entry.unplannable.is_some() {
                    theme.dim
                } else {
                    theme.text
                },
            ),
        ]);
        lines.push(if index == list.selected {
            line.style(theme.selected)
        } else {
            line
        });
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_details(frame: &mut Frame, list: &List, theme: &Theme, area: Rect) {
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(theme.border)
        .title(Span::styled(" Statement ", theme.title));
    let Some(entry) = list.entries.get(list.selected) else {
        frame.render_widget(block, area);
        return;
    };
    let pages = entry.pages();
    let cached = if pages > 0 {
        format!(
            ", {} from cache",
            format::percent(entry.shared_hit as f64 / pages as f64)
        )
    } else {
        String::new()
    };
    let mut facts = format!(
        "{} calls · {} each · {} rows · {} pages{cached}",
        format::grouped(entry.calls),
        format::duration(entry.mean_ms),
        format::grouped(entry.rows),
        format::grouped(pages),
    );
    if entry.temp_written > 0 {
        facts.push_str(&format!(
            " · {} pages written to temporary files",
            format::grouped(entry.temp_written)
        ));
    }
    if let Some(id) = entry.queryid {
        facts.push_str(&format!(" · query id {id}"));
    }
    let mut lines = vec![Line::styled(facts, theme.dim)];
    if let Some(reason) = &entry.unplannable {
        lines.push(Line::styled(
            format!("Cannot be planned: {reason}."),
            theme.warm,
        ));
    }
    lines.push(Line::raw(entry.one_line()));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Shortens text to `width` characters, ending with an ellipsis.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut short: String = text.chars().take(width - 1).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(query: &str, total_ms: f64) -> Entry {
        Entry {
            queryid: Some(1),
            query: query.to_owned(),
            calls: 10,
            total_ms,
            share: total_ms / 1000.0,
            mean_ms: total_ms / 10.0,
            rows: 10,
            shared_hit: 90,
            shared_read: 10,
            temp_written: 0,
            unplannable: explainsql_core::top::unplannable(query, Some(1024)),
        }
    }

    #[test]
    fn picks_what_can_be_planned() {
        let entries = vec![
            entry("SELECT * FROM orders WHERE customer_id = $1", 900.0),
            entry("VACUUM orders", 50.0),
            entry("SELECT count(*) FROM orders", 40.0),
        ];
        let mut list = List::new(entries.clone(), "db".to_owned(), true);
        assert_eq!(list.handle(Key::Enter), Some(Picked::Plan(0)));
        assert_eq!(list.handle(Key::Char('p')), Some(Picked::Values(0)));
        list.handle(Key::Char('j'));
        assert_eq!(list.handle(Key::Enter), None);
        assert!(list.message.as_deref().unwrap().contains("VACUUM"));
        assert_eq!(list.handle(Key::Char('p')), None);
        list.handle(Key::Char('G'));
        assert_eq!(list.selected, 2);
        // No parameters: nothing to try, and planned as it is.
        assert_eq!(list.handle(Key::Char('p')), None);
        assert!(list.message.as_deref().unwrap().contains("no parameters"));
        assert_eq!(list.handle(Key::Enter), Some(Picked::Plan(2)));
        assert_eq!(list.handle(Key::Char('q')), Some(Picked::Quit));

        // Before PostgreSQL 16, $1 needs values.
        let mut list = List::new(entries, "db".to_owned(), false);
        assert_eq!(list.handle(Key::Enter), Some(Picked::Values(0)));
        assert_eq!(list.handle(Key::Char('p')), Some(Picked::Values(0)));
        list.handle(Key::Char('G'));
        assert_eq!(list.handle(Key::Enter), Some(Picked::Plan(2)));
    }
}
