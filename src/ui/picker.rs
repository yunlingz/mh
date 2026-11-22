//! Reference-style searchable select lists: heading, search box, rows, sticky actions.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use super::chrome;
use super::completion::Item;
use super::editor::{EDITING_HINT, Editor};
use crate::text::terminal_text;
use crate::theme::Palette;

pub enum Action {
    Selected(String),
    Cancelled,
}

pub struct Picker {
    title: &'static str,
    items: Vec<Item>,
    query: Editor,
    matches: Vec<usize>,
    selected: usize,
    error: Option<String>,
}
impl Picker {
    pub fn new(title: &'static str, items: Vec<Item>, query: &str) -> Self {
        let mut search = Editor::default();
        search.insert_str(query);
        let mut picker = Self {
            title,
            items,
            query: search,
            matches: Vec::new(),
            selected: 0,
            error: None,
        };
        picker.filter();
        picker
    }
    pub fn error(&mut self, error: String) {
        self.error = Some(error);
    }
    fn filter(&mut self) {
        self.error = None;
        let query = self.query.draft().text().to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.label.to_lowercase().contains(&query)
                    || item.value.to_lowercase().contains(&query)
                    || item.description.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }
    pub fn navigate(&mut self, delta: isize) {
        if !self.matches.is_empty() {
            self.selected =
                (self.selected as isize + delta).rem_euclid(self.matches.len() as isize) as usize;
        }
    }
    pub fn paste(&mut self, text: &str) {
        match self.query.paste(text) {
            Ok(()) => self.filter(),
            Err(error) => self.error = Some(error),
        }
    }
    pub fn key(&mut self, mut key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Char('m') && key.modifiers == KeyModifiers::CONTROL {
            key.code = KeyCode::Enter;
            key.modifiers = KeyModifiers::NONE;
        }
        if key.kind == crossterm::event::KeyEventKind::Release
            || key.kind == crossterm::event::KeyEventKind::Repeat
                && matches!(key.code, KeyCode::Enter | KeyCode::Esc)
        {
            return None;
        }
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return Some(Action::Cancelled);
        }
        match key.code {
            KeyCode::Enter if !super::editor::is_newline(key) => {
                return self
                    .matches
                    .get(self.selected)
                    .map(|i| Action::Selected(self.items[*i].value.clone()));
            }
            KeyCode::BackTab => self.navigate(-1),
            KeyCode::Tab => self.navigate(1),
            KeyCode::Up if key.modifiers.is_empty() => self.navigate(-1),
            KeyCode::Down if key.modifiers.is_empty() => self.navigate(1),
            KeyCode::PageUp if key.modifiers.is_empty() => self.navigate(-5),
            KeyCode::PageDown if key.modifiers.is_empty() => self.navigate(5),
            _ => {
                let before = self.query.draft().text();
                self.query.input(key);
                if self.query.draft().text() != before {
                    self.filter();
                }
            }
        }
        None
    }
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
        let inner = chrome::modal(frame, area, palette, self.title);
        if inner.is_empty() {
            return;
        }
        let hint =
            format!("Type to search · ↑/↓ select · Enter select · Esc close · {EDITING_HINT}");
        let actions = self.error.as_deref().unwrap_or(&hint);
        let maximum_actions = inner.height.saturating_sub(6).clamp(1, 3);
        let action_height = chrome::actions_height(actions, inner.width, maximum_actions);
        let [body, footer] = chrome::body_actions(inner, action_height);
        let search_height = self
            .query
            .height(body.width, 3)
            .saturating_add(2)
            .min(body.height);
        let separator = u16::from(body.height > search_height.saturating_add(1));
        let parts = Layout::vertical([
            Constraint::Length(search_height),
            Constraint::Length(separator),
            Constraint::Min(0),
        ])
        .split(body);
        self.query
            .render_labeled(frame, parts[0], palette, "Search", true);
        let stride = if parts[2].height >= 6 { 3 } else { 2 };
        let capacity = usize::from(parts[2].height / stride).max(1);
        let start = self.selected.saturating_sub(capacity - 1);
        let mut rows = Vec::new();
        for (index, item) in self.matches.iter().enumerate().skip(start).take(capacity) {
            let item = &self.items[*item];
            let style = Style::default()
                .fg(palette.text)
                .bg(if index == self.selected {
                    palette.selected_bg
                } else {
                    palette.background
                });
            rows.push(Line::styled(
                format!(
                    "{} {}",
                    if index == self.selected { ">" } else { " " },
                    terminal_text(&item.label, false)
                ),
                style,
            ));
            rows.push(Line::styled(
                format!("  {}", terminal_text(&item.description, false)),
                Style::default().fg(palette.muted),
            ));
            if stride == 3 {
                rows.push(Line::default());
            }
        }
        if rows.is_empty() {
            rows.push(Line::styled(
                "No matching entries",
                Style::default().fg(palette.muted),
            ));
        }
        frame.render_widget(
            Paragraph::new(rows).style(Style::default().bg(palette.background)),
            parts[2],
        );
        chrome::actions(frame, footer, palette, actions);
    }
}

pub fn sessions(sessions: Vec<crate::session::SessionInfo>) -> Vec<Item> {
    sessions
        .into_iter()
        .map(|session| {
            let age = crate::storage::timestamp().saturating_sub(session.updated_at) / 60_000;
            let age = if age < 60 {
                format!("{age}m ago")
            } else if age < 1440 {
                format!("{}h ago", age / 60)
            } else {
                format!("{}d ago", age / 1440)
            };
            Item {
                value: session.id.clone(),
                label: session.first_message,
                description: format!(
                    "{} · {age} · {} messages{}",
                    session.id,
                    session.message_count,
                    session
                        .model
                        .map_or(String::new(), |model| format!(" · {}", model.qualified))
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn items() -> Vec<Item> {
        [("a", "edit editor"), ("b", "check sessions")]
            .into_iter()
            .map(|(id, label)| Item {
                value: id.into(),
                label: label.into(),
                description: "Description".into(),
            })
            .collect()
    }

    #[test]
    fn picker_search_reuses_grapheme_selection_recovery_and_modal_first_keys() {
        let mut picker = Picker::new("Resume session", items(), "");
        picker.paste("session");
        picker.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        picker.key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT));
        picker.paste("editor");
        assert_eq!(picker.query.draft().text(), "editor");
        assert_eq!(picker.matches, [0]);
        assert!(picker.query.undo());
        picker.filter();
        assert_eq!(picker.query.draft().text(), "session");
        assert_eq!(picker.matches, [1]);
        picker.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(picker.query.draft().text(), "session");
        picker.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        picker.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        picker.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(picker.query.draft().text(), "se");
        assert!(
            matches!(picker.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)),
            Some(Action::Selected(value)) if value == "b")
        );
        assert!(matches!(
            picker.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Cancelled)
        ));
        let mut picker = Picker::new("Resume session", items(), "");
        picker.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        picker.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            picker.selected, 1,
            "cursor movement must not reset list selection"
        );
        picker.paste("👨‍👩‍👧‍👦");
        picker.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(picker.query.draft().is_empty());
    }

    #[test]
    fn picker_is_opaque_rectangular_and_search_is_labelled_outside_plain_rules() {
        use ratatui::widgets::Wrap;
        use ratatui::{Terminal, backend::TestBackend};
        let palette = Palette::default();
        for (width, height) in [(1, 1), (2, 3), (8, 6), (40, 16), (80, 24)] {
            let mut picker = Picker::new("Resume session", items(), "你好");
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("▒".repeat(2000)).wrap(Wrap { trim: false }),
                        frame.area(),
                    );
                    picker.render(frame, frame.area(), &palette);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for cell in &buffer.content {
                assert_ne!(cell.symbol(), "▒");
                assert!(!["╭", "╮", "╰", "╯"].contains(&cell.symbol()));
            }
            if width >= 40 {
                let row = |y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                };
                assert!(row(0).contains("Resume session"));
                assert!(row(1).contains("Search"));
                for x in 1..width - 1 {
                    assert_eq!(buffer[(x, 2)].symbol(), " ");
                }
                for x in 1..width - 1 {
                    assert_eq!(buffer[(x, 3)].symbol(), "─");
                }
                assert_eq!(buffer[(1, 4)].symbol(), ">");
                let cursor = terminal.get_cursor_position().unwrap();
                assert_eq!(cursor.x, 7);
                assert_eq!(cursor.y, 4);
            }
            assert_eq!(picker.query.draft().text(), "你好");
        }
    }

    #[test]
    fn newline_and_removed_undo_keys_never_select_or_erase_search() {
        let mut picker = Picker::new("Select model", items(), "draft");
        for key in [
            KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT),
        ] {
            assert!(picker.key(key).is_none());
            assert_eq!(picker.query.draft().text(), "draft");
        }
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            assert!(picker.key(key).is_none());
        }
        assert_eq!(picker.query.draft().text(), "draft\n\n");
    }

    #[test]
    fn search_selection_and_cancellation_are_deterministic() {
        let items = [("a", "edit editor"), ("b", "check sessions")]
            .into_iter()
            .map(|(id, label)| Item {
                value: id.into(),
                label: label.into(),
                description: String::new(),
            })
            .collect();
        let mut picker = Picker::new("Resume session", items, "");
        picker.paste("session");
        assert!(
            matches!(picker.key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),Some(Action::Selected(id)) if id=="b")
        );
        assert!(matches!(
            picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Action::Cancelled)
        ));
    }
}
