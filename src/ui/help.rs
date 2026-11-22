//! Local command/shortcut reference, outside conversation and model history.
//! Uses the existing opaque dialog hierarchy rather than a second UI layout.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::{chrome, completion::COMMANDS};
use crate::theme::Palette;

const GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Composer and shared editing",
        &[
            (
                "Enter / Ctrl+M",
                "Submit input or a command; during main work, queue steering input",
            ),
            (
                "Shift+Enter / Ctrl+J",
                "Insert a newline, never submit or advance a field",
            ),
            (
                "Backspace / Ctrl+H",
                "Delete backward; Backspace deletes a complete grapheme",
            ),
            (
                "Ctrl+D / Delete",
                "Delete forward; Ctrl+D never exits, even on empty input",
            ),
            ("Ctrl+U / Ctrl+K", "Delete to line start / end"),
            ("Ctrl+W / Alt+Backspace / Alt+H", "Delete the previous word"),
            ("Alt+D / Alt+Delete", "Delete the next word"),
            ("Ctrl+Y", "Yank the editor's last deleted/copied text"),
            (
                "Ctrl+X",
                "Cut the editor selection into its internal yank buffer",
            ),
            (
                "Ctrl+Z",
                "Undo/recover one complete input change; never suspend the fullscreen app",
            ),
            (
                "Left / Right / Ctrl+B / Ctrl+F",
                "Move the input cursor backward / forward",
            ),
            (
                "Home / End / Ctrl+A / Ctrl+E",
                "Move to the logical line start / end",
            ),
            ("Ctrl+Left / Ctrl+Right / Alt+B / Alt+F", "Move by word"),
            (
                "Up / Down",
                "Move by visual input row; recall prompt history at its boundaries",
            ),
            ("Ctrl+N", "Move down a logical input row"),
            (
                "Alt+P / Alt+[ / Alt+N / Alt+]",
                "Move to the previous / next input paragraph",
            ),
            ("Alt+< / Alt+>", "Move to the beginning / end of the input"),
            (
                "Shift + cursor movement",
                "Extend an input selection (transcript focus uses transcript selection)",
            ),
            (
                "Ctrl+V / Alt+V",
                "Read text/image from the OS clipboard in the main composer",
            ),
            (
                "Bracketed paste",
                "Paste literally; large text folds, repeating the same paste expands it",
            ),
            (
                "Ctrl+C",
                "Copy selected transcript text; otherwise clear input, then repeat within 1s to quit",
            ),
            (
                "Esc",
                "Dismiss completion first; otherwise cancel main work / clear an idle selection",
            ),
            ("Alt+Enter", "Queue a follow-up while main work is active"),
            (
                "Alt+Up / Alt+Q",
                "Restore queued messages and the current draft for editing",
            ),
            (
                "Ctrl+P / Ctrl+Shift+P",
                "Cycle fixed model entries forward / backward; stage during main work",
            ),
            ("Ctrl+T", "Expand / collapse the bounded todo panel"),
        ],
    ),
    (
        "Completion",
        &[
            (
                "Tab",
                "Open completion, or apply the selected command/skill/model/file candidate",
            ),
            (
                "Up / Down / PageUp / PageDown",
                "Choose a candidate; page keys move five candidates",
            ),
            (
                "Enter",
                "Apply the candidate; commands and exact command arguments also submit",
            ),
            ("Esc", "Dismiss without cancelling main work"),
        ],
    ),
    (
        "Transcript and selection",
        &[
            (
                "Ctrl+O",
                "Open / close detailed transcript and startup information",
            ),
            ("Up / Down", "Scroll one row while reviewing the transcript"),
            (
                "PageUp / PageDown / mouse wheel",
                "Scroll transcript output",
            ),
            (
                "Ctrl+Up / Ctrl+Down",
                "Jump between messages when completion is closed",
            ),
            (
                "Ctrl+Home / Ctrl+End",
                "Jump to the beginning / resume following output",
            ),
            (
                "Shift+arrows / Shift+Home / Shift+End",
                "Extend transcript selection in review or after mouse focus",
            ),
            (
                "Ctrl+Shift+Home / Ctrl+Shift+End",
                "Extend transcript selection to the beginning / end",
            ),
            (
                "Mouse drag",
                "Select transcript text; dragging outside the viewport scrolls",
            ),
            (
                "Ctrl+C / Alt+C",
                "Copy the selected transcript text (modals handle their own controls first)",
            ),
            (
                "Esc / Q / Ctrl+C",
                "Return from review to input; Ctrl+C copies if a selection exists",
            ),
            (
                "Click input / jump-to-bottom",
                "Focus input / resume following output",
            ),
        ],
    ),
    (
        "Model and session pickers",
        &[
            (
                "Typing / paste",
                "Filter labels, values and descriptions using the shared editor",
            ),
            ("Up / Down / Tab / Shift+Tab", "Choose an entry"),
            (
                "PageUp / PageDown / mouse wheel",
                "Move five entries / move with wheel",
            ),
            ("Enter / Ctrl+M", "Select an entry"),
            (
                "Shift+Enter / Ctrl+J",
                "Insert a search newline, not select an entry",
            ),
            ("Esc / Ctrl+C", "Close the picker"),
        ],
    ),
    (
        "Questionnaire",
        &[
            (
                "Up / Down",
                "Choose an option or review action; custom-input edge rows return to options",
            ),
            (
                "1–9",
                "Activate the corresponding option (including Other when numbered)",
            ),
            ("Space", "Toggle the highlighted multi-select option"),
            (
                "Enter / Ctrl+M",
                "Confirm an option/custom answer or the final Submit/Cancel action",
            ),
            (
                "Tab / Shift+Tab",
                "Switch questions; leave notes/custom focus for a single question",
            ),
            ("N", "Edit notes for the current question"),
            (
                "Shift+Enter / Ctrl+J",
                "Insert a newline in notes/custom input",
            ),
            ("Enter / Esc in notes", "Finish notes and return to options"),
            (
                "PageUp / PageDown / mouse wheel",
                "Scroll option previews and final answer review",
            ),
            ("Ctrl+]", "Collapse / expand the question dialog"),
            (
                "Esc / Ctrl+C",
                "Decline the questionnaire (Esc in notes only finishes notes)",
            ),
        ],
    ),
    (
        "Local skill creation",
        &[
            (
                "Up / Down / Tab / Shift+Tab",
                "Choose scope, invocation policy, or review action",
            ),
            (
                "Enter / Ctrl+M",
                "Advance a field or confirm the chosen review action",
            ),
            ("Tab / Shift+Tab in a field", "Next field / previous step"),
            (
                "Shift+Enter / Ctrl+J",
                "Insert a field newline, never advance or publish",
            ),
            ("PageUp / PageDown", "Scroll the instructions preview"),
            ("Esc", "Previous step; close at the first step"),
            ("Ctrl+C", "Cancel without publishing"),
        ],
    ),
    (
        "Side question (/btw)",
        &[
            (
                "Up / Down / PageUp / PageDown / mouse wheel",
                "Scroll the side answer",
            ),
            ("Esc", "Close the view without cancelling the side request"),
            ("Ctrl+C", "Cancel only the side request"),
            ("X", "Clear side history and cancel its active request"),
        ],
    ),
    (
        "Full todo list (/todos)",
        &[
            (
                "Up / Down / PageUp / PageDown / mouse wheel",
                "Scroll tasks",
            ),
            ("Home", "Jump to the beginning"),
            ("Esc / Q / Ctrl+C", "Close the task list"),
        ],
    ),
    (
        "Help",
        &[
            (
                "Up / Down / PageUp / PageDown / mouse wheel",
                "Scroll this complete reference",
            ),
            ("Home / End", "Jump to the beginning / end of help"),
            (
                "Esc / Q / Ctrl+C / Enter",
                "Close help without cancelling main work",
            ),
        ],
    ),
    (
        "Shared editor inside modal fields",
        &[
            (
                "Ctrl+P / Ctrl+N",
                "Move up / down a logical row (main Ctrl+P cycles models)",
            ),
            ("Ctrl+Alt+Left / Ctrl+Alt+Right", "Move to line start / end"),
            ("Ctrl+Alt+P / Ctrl+Alt+N", "Move to input beginning / end"),
            (
                "Ctrl+Up / Ctrl+Down",
                "Move by paragraph unless the modal handles option navigation",
            ),
            (
                "Ctrl+V / Alt+V",
                "Page the field viewport, not OS clipboard paste",
            ),
            (
                "Other shared editing keys above",
                "Modal navigation/confirmation/cancellation takes precedence",
            ),
        ],
    ),
];

#[derive(Default)]
pub struct Help {
    scroll: usize,
    page: usize,
}

impl Help {
    /// Return true only to close the view. No key reaches underlying input.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page.max(1)),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(self.page.max(1)),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = usize::MAX,
            _ => {}
        }
        false
    }

    pub fn wheel(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }

    fn lines(palette: &Palette) -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled(
            "Commands",
            Style::default().fg(palette.accent),
        )];
        for &(name, description) in COMMANDS {
            let arguments = match name {
                "btw" => " [question]",
                "compact" => " [focus]",
                "model" => " [provider/alias]",
                "resume" => " [session-id]",
                "skills" => " create",
                _ => "",
            };
            lines.push(Line::from(format!("/{name}{arguments} — {description}")));
        }
        lines.push(Line::from(
            "/skill:<name> [args] — Invoke a discovered skill",
        ));
        lines.push(Line::from(
            "Terminal shortcuts depend on terminal/OS support; fullscreen Ctrl+Z recovers input without suspending.",
        ));
        for &(heading, entries) in GROUPS {
            lines.push(Line::default());
            lines.push(Line::styled(heading, Style::default().fg(palette.accent)));
            lines.extend(
                entries
                    .iter()
                    .map(|(keys, action)| Line::from(format!("{keys} — {action}"))),
            );
        }
        lines
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
        let inner = chrome::modal(frame, area, palette, "Help · commands and all shortcuts");
        if inner.is_empty() {
            return;
        }
        let hint = "↑/↓ scroll · PgUp/PgDn page · Home/End · Esc close";
        let height =
            chrome::actions_height(hint, inner.width, 3).min(inner.height.saturating_sub(1));
        let [body, footer] = chrome::body_actions(inner, height);
        let content = Paragraph::new(Self::lines(palette))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(palette.text).bg(palette.background));
        let total = content.line_count(body.width.max(1));
        self.page = usize::from(body.height).saturating_sub(1).max(1);
        self.scroll = self
            .scroll
            .min(total.saturating_sub(usize::from(body.height)));
        frame.render_widget(
            content.scroll((self.scroll.min(u16::MAX as usize) as u16, 0)),
            body,
        );
        chrome::actions(frame, footer, palette, hint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn reference_covers_all_commands_groups_and_corrected_bindings() {
        let text = Help::lines(&Palette::default())
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        for &(name, _) in COMMANDS {
            assert!(text.contains(&format!("/{name}")));
        }
        for &(heading, _) in GROUPS {
            assert!(text.contains(heading));
        }
        for key in [
            "Ctrl+U",
            "Ctrl+K",
            "Ctrl+Z",
            "Backspace",
            "Ctrl+D",
            "Shift+Enter",
            "Ctrl+J",
            "Ctrl+]",
            "Alt+Q",
            "Ctrl+Shift+Home",
        ] {
            assert!(text.contains(key), "{key}");
        }
        assert!(text.contains("Undo/recover"));
        assert!(
            !GROUPS
                .iter()
                .flat_map(|(_, entries)| *entries)
                .any(|(keys, _)| *keys == "Ctrl+R")
        );
        for removed in ["Alt+Z", "Ctrl+_", "Ctrl+-"] {
            assert!(!text.contains(removed));
        }
    }

    #[test]
    fn help_is_opaque_scrollable_and_bounded_on_small_terminals() {
        let palette = Palette::default();
        for (width, height) in [(1, 1), (2, 3), (8, 6), (40, 16), (80, 24)] {
            let mut help = Help::default();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("▒".repeat(4000)).wrap(Wrap { trim: false }),
                        frame.area(),
                    );
                    help.render(frame, frame.area(), &palette);
                })
                .unwrap();
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| cell.symbol() != "▒" && cell.bg == palette.background)
            );
            if width >= 40 {
                assert!(!help.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
                terminal
                    .draw(|frame| help.render(frame, frame.area(), &palette))
                    .unwrap();
                assert!(help.scroll > 0 && help.scroll < u16::MAX as usize);
                help.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
                assert_eq!(help.scroll, 0);
            }
            assert!(help.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        }
    }
}
