//! Local skill-creation presentation, separate from publication and resource refresh.
//!
//! The wizard is an mh adaptation, not a Pi/Claude feature claim. It reuses the
//! inspected questionnaire/picker heading, selection, input and sticky-action
//! hierarchy. Inputs retain Pi's plain prompt/rules; Ctrl+J follows the documented
//! portable multiline convention. No filesystem, provider or subprocess work lives here.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::chrome;
use super::editor::{EDITING_HINT, Editor};
use crate::context::{SkillDraft, SkillScope};
use crate::text::{display_path, terminal_text};
use crate::theme::Palette;

const NAME_INPUT_BYTES: usize = 256;
const DESCRIPTION_INPUT_BYTES: usize = 4096;
const BODY_INPUT_BYTES: usize = 1024 * 1024;
const REVIEW_PREVIEW_BYTES: usize = 4096;

pub enum Action {
    None,
    Cancel,
    Create {
        scope: SkillScope,
        draft: SkillDraft,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Scope,
    Name,
    Description,
    ExplicitOnly,
    Body,
    Review,
}

impl Step {
    fn index(self) -> usize {
        match self {
            Self::Scope => 1,
            Self::Name => 2,
            Self::Description => 3,
            Self::ExplicitOnly => 4,
            Self::Body => 5,
            Self::Review => 6,
        }
    }
}

pub struct Wizard {
    project_preview: PathBuf,
    global_preview: PathBuf,
    scope: SkillScope,
    step: Step,
    name: Editor,
    description: Editor,
    body: Editor,
    explicit_only: bool,
    review_choice: usize,
    review_scroll: u16,
    review_text: String,
    draft: Option<SkillDraft>,
    notice: Option<String>,
    creating: bool,
}

impl Wizard {
    /// Previews are the two roots returned by SkillStore::root, not user-entered paths.
    pub fn new(project_preview: PathBuf, global_preview: PathBuf) -> Self {
        Self {
            project_preview,
            global_preview,
            scope: SkillScope::Project,
            step: Step::Scope,
            name: Editor::default(),
            description: Editor::default(),
            body: Editor::default(),
            explicit_only: false,
            review_choice: 0,
            review_scroll: 0,
            review_text: String::new(),
            draft: None,
            notice: None,
            creating: false,
        }
    }

    pub fn creating(&self) -> bool {
        self.creating
    }

    /// A failed worker retains all fields and the validated review draft for retry/edit.
    /// Callers supply redacted errors; terminal controls and excessive text are removed here.
    pub fn failed(&mut self, message: String) {
        self.creating = false;
        self.notice = Some(terminal_text(&message, false).chars().take(512).collect());
    }

    fn field(&mut self) -> Option<(&mut Editor, usize)> {
        match self.step {
            Step::Name => Some((&mut self.name, NAME_INPUT_BYTES)),
            Step::Description => Some((&mut self.description, DESCRIPTION_INPUT_BYTES)),
            Step::Body => Some((&mut self.body, BODY_INPUT_BYTES)),
            _ => None,
        }
    }

    fn limit_notice(&mut self) {
        self.notice = Some(
            match self.step {
                Step::Name => "Skill name input exceeds 256 bytes",
                Step::Description => "Skill description input exceeds 4 KiB",
                _ => "Skill instructions exceed the 1 MiB input limit",
            }
            .into(),
        );
    }

    pub fn paste(&mut self, text: &str) {
        if self.creating {
            return;
        }
        let Some((field, limit)) = self.field() else {
            return;
        };
        if text.len() > limit {
            self.limit_notice();
            return;
        }
        let result = field.paste(text);
        let exceeds = field.draft().text().len() > limit;
        if exceeds {
            // Shared Editor changes are atomic, including replacement selections/paste chips.
            field.undo();
            self.limit_notice();
        } else {
            self.notice = result.err();
        }
        self.draft = None;
    }

    fn input(&mut self, key: KeyEvent, newline: bool) {
        let Some((field, limit)) = self.field() else {
            return;
        };
        if newline {
            field.insert_str("\n");
        } else {
            field.input(key);
        }
        if field.draft().text().len() > limit {
            field.undo();
            self.limit_notice();
        } else {
            self.notice = None;
        }
        self.draft = None;
    }

    fn back(&mut self) -> Action {
        self.notice = None;
        self.step = match self.step {
            Step::Scope => return Action::Cancel,
            Step::Name => Step::Scope,
            Step::Description => Step::Name,
            Step::ExplicitOnly => Step::Description,
            Step::Body => Step::ExplicitOnly,
            Step::Review => Step::Body,
        };
        Action::None
    }

    fn validate(
        &self,
        include_description: bool,
        include_body: bool,
    ) -> Result<SkillDraft, String> {
        SkillDraft::new(
            self.name.draft().text(),
            if include_description {
                self.description.draft().text()
            } else {
                "description".into()
            },
            if include_body {
                self.body.draft().text()
            } else {
                "instructions".into()
            },
            self.explicit_only,
        )
        .map_err(|error| error.to_string())
    }

    fn advance(&mut self) -> Action {
        let validation = match self.step {
            Step::Name => self.validate(false, false).map(|_| ()),
            Step::Description => self.validate(true, false).map(|_| ()),
            Step::Body => self.validate(true, true).map(|draft| {
                self.draft = Some(draft);
                self.prepare_review();
            }),
            _ => Ok(()),
        };
        if let Err(error) = validation {
            self.notice = Some(error);
            return Action::None;
        }
        self.notice = None;
        self.step = match self.step {
            Step::Scope => Step::Name,
            Step::Name => Step::Description,
            Step::Description => Step::ExplicitOnly,
            Step::ExplicitOnly => Step::Body,
            Step::Body => Step::Review,
            Step::Review => return self.review_action(),
        };
        Action::None
    }

    fn prepare_review(&mut self) {
        let body = self.body.draft().text();
        let mut end = body.len().min(REVIEW_PREVIEW_BYTES);
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        let root = match self.scope {
            SkillScope::Project => &self.project_preview,
            SkillScope::Global => &self.global_preview,
        };
        let destination = root.join(self.name.draft().text()).join("SKILL.md");
        self.review_text = format!(
            "Scope: {}\nDestination: {}\n\nDescription:\n{}\n\nInvocation: {}\n\nInstructions ({} bytes):\n{}{}",
            if self.scope == SkillScope::Project {
                "Project"
            } else {
                "Global"
            },
            display_path(&destination),
            self.description.draft().text(),
            if self.explicit_only {
                "Explicit /skill command only; hidden from the model catalog"
            } else {
                "Available to the model and explicit /skill commands"
            },
            body.len(),
            &body[..end],
            if end < body.len() {
                "\n\n[Preview truncated; full instructions are retained. Choose Edit instructions to revise them.]"
            } else {
                ""
            },
        );
        self.review_choice = 0;
        self.review_scroll = 0;
    }

    fn review_action(&mut self) -> Action {
        match self.review_choice {
            1 => {
                self.step = Step::Body;
                Action::None
            }
            2 => Action::Cancel,
            _ => {
                let Some(draft) = self.draft.clone() else {
                    self.notice = Some("Review the skill instructions before creating it".into());
                    return Action::None;
                };
                // Set synchronously, before App can start its filesystem worker.
                self.creating = true;
                Action::Create {
                    scope: self.scope,
                    draft,
                }
            }
        }
    }

    fn select(&mut self, delta: isize) {
        match self.step {
            Step::Scope => {
                self.scope = match self.scope {
                    SkillScope::Project => SkillScope::Global,
                    SkillScope::Global => SkillScope::Project,
                }
            }
            Step::ExplicitOnly => self.explicit_only = !self.explicit_only,
            Step::Review => {
                self.review_choice = (self.review_choice as isize + delta).rem_euclid(3) as usize
            }
            _ => (),
        }
    }

    pub fn key(&mut self, mut key: KeyEvent) -> Action {
        if self.creating || key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if key.code == KeyCode::Char('m') && key.modifiers == KeyModifiers::CONTROL {
            key.code = KeyCode::Enter;
            key.modifiers = KeyModifiers::NONE;
        }
        if key.kind == KeyEventKind::Repeat
            && matches!(
                key.code,
                KeyCode::Enter | KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab
            )
        {
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Cancel;
        }
        if key.code == KeyCode::Esc {
            return self.back();
        }
        let choice = matches!(self.step, Step::Scope | Step::ExplicitOnly | Step::Review);
        if super::editor::is_newline(key) {
            if !choice {
                self.input(key, true);
            }
            return Action::None;
        }
        match key.code {
            KeyCode::Enter => return self.advance(),
            KeyCode::Tab if choice => self.select(1),
            KeyCode::BackTab if choice => self.select(-1),
            KeyCode::Tab => return self.advance(),
            KeyCode::BackTab => return self.back(),
            KeyCode::Up if choice && key.modifiers.is_empty() => self.select(-1),
            KeyCode::Down if choice && key.modifiers.is_empty() => self.select(1),
            KeyCode::PageUp if self.step == Step::Review => {
                self.review_scroll = self.review_scroll.saturating_sub(5)
            }
            KeyCode::PageDown if self.step == Step::Review => {
                self.review_scroll = self.review_scroll.saturating_add(5)
            }
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => self.input(key, true),
            _ if !choice => self.input(key, false),
            _ => (),
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
        let title = format!("Create skill · {}/6", self.step.index());
        let inner = chrome::modal(frame, area, palette, &title);
        if inner.is_empty() {
            return;
        }
        let hint = if self.creating {
            "Creating skill…".to_owned()
        } else if matches!(self.step, Step::Name | Step::Description | Step::Body) {
            format!("Enter/Tab next · Shift+Tab/Esc back · Ctrl+C cancel · {EDITING_HINT}")
        } else if self.step == Step::Review {
            "↑/↓/Tab select · Enter confirm · PgUp/PgDn preview · Esc edit instructions · Ctrl+C cancel".into()
        } else {
            "↑/↓/Tab select · Enter next · Esc back/close · Ctrl+C cancel".into()
        };
        let actions = self.notice.as_deref().unwrap_or(&hint);
        let action_height = chrome::actions_height(actions, inner.width, 4);
        let [body, footer] = chrome::body_actions(inner, action_height);
        match self.step {
            Step::Scope => self.render_choices(frame, body, palette, &[
                ("Project", format!("Only this workspace: {}", display_path(&self.project_preview))),
                ("Global", format!("All workspaces: {}", display_path(&self.global_preview))),
            ], usize::from(self.scope == SkillScope::Global)),
            Step::ExplicitOnly => self.render_choices(frame, body, palette, &[
                ("Automatic and explicit", "Shown in the model catalog and available through /skill:<name>".into()),
                ("Explicit only", "Hidden from the model catalog; /skill:<name> still works. This is not a filesystem restriction.".into()),
            ], usize::from(self.explicit_only)),
            Step::Name => self.name.render_labeled(frame, body, palette, "Name (lowercase letters, digits, single hyphens)", true),
            Step::Description => self.description.render_labeled(frame, body, palette, "Description (what it does and when to use it)", true),
            Step::Body => self.body.render_labeled(frame, body, palette, "Instructions (Ctrl+J inserts a newline)", true),
            Step::Review => {
                let parts = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(body);
                let preview = Paragraph::new(terminal_text(&self.review_text, true))
                    .wrap(Wrap { trim: false }).style(Style::default().fg(palette.text).bg(palette.background));
                let maximum = preview.line_count(parts[0].width.max(1)).saturating_sub(usize::from(parts[0].height));
                self.review_scroll = self.review_scroll.min(u16::try_from(maximum).unwrap_or(u16::MAX));
                frame.render_widget(preview.scroll((self.review_scroll, 0)), parts[0]);
                let rows = ["Create skill", "Edit instructions", "Cancel"].iter().enumerate().map(|(index, text)| {
                    Line::styled(format!("{} {}", if index == self.review_choice { ">" } else { " " }, text),
                        Style::default().fg(palette.text).bg(if index == self.review_choice { palette.selected_bg } else { palette.background }))
                }).collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(rows), parts[1]);
            }
        }
        chrome::actions(frame, footer, palette, actions);
    }

    fn render_choices(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        palette: &Palette,
        choices: &[(&str, String)],
        selected: usize,
    ) {
        let stride = if area.height >= 6 { 3 } else { 2 };
        let mut rows = Vec::new();
        for (index, (name, description)) in choices.iter().enumerate() {
            rows.push(Line::styled(
                format!("{} {}", if index == selected { ">" } else { " " }, name),
                Style::default().fg(palette.text).bg(if index == selected {
                    palette.selected_bg
                } else {
                    palette.background
                }),
            ));
            rows.push(Line::styled(
                format!("  {}", terminal_text(description, false)),
                Style::default().fg(palette.muted),
            ));
            if stride == 3 {
                rows.push(Line::default());
            }
        }
        frame.render_widget(
            Paragraph::new(rows).style(Style::default().bg(palette.background)),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn wizard() -> Wizard {
        Wizard::new(
            PathBuf::from("project/.mh/skills"),
            PathBuf::from("mh/config/skills"),
        )
    }

    fn press(wizard: &mut Wizard, code: KeyCode) -> Action {
        wizard.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn complete(wizard: &mut Wizard) {
        press(wizard, KeyCode::Enter);
        wizard.paste("local-workflow");
        press(wizard, KeyCode::Enter);
        wizard.paste("Use when reviewing local changes.");
        press(wizard, KeyCode::Enter);
        press(wizard, KeyCode::Enter);
        wizard.paste("Inspect tests before modifying code.");
        press(wizard, KeyCode::Enter);
        assert_eq!(wizard.step, Step::Review);
    }

    #[test]
    fn wizard_validates_fields_preserves_backtracking_and_explicit_only() {
        let mut wizard = wizard();
        press(&mut wizard, KeyCode::Tab);
        assert_eq!(wizard.scope, SkillScope::Global);
        press(&mut wizard, KeyCode::Enter);
        wizard.paste("../outside");
        press(&mut wizard, KeyCode::Enter);
        assert_eq!(wizard.step, Step::Name);
        assert!(wizard.notice.is_some());
        assert!(wizard.name.undo());
        wizard.paste("safe");
        press(&mut wizard, KeyCode::Tab);
        wizard.paste("Review a project");
        wizard.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        wizard.paste("with local checks.");
        assert!(wizard.description.draft().text().contains('\n'));
        press(&mut wizard, KeyCode::Enter);
        press(&mut wizard, KeyCode::Tab);
        assert!(wizard.explicit_only);
        press(&mut wizard, KeyCode::Enter);
        wizard.paste("你好\nInstructions");
        press(&mut wizard, KeyCode::Esc);
        assert_eq!(wizard.step, Step::ExplicitOnly);
        press(&mut wizard, KeyCode::Enter);
        assert_eq!(wizard.body.draft().text(), "你好\nInstructions");
        press(&mut wizard, KeyCode::Enter);
        assert!(wizard.review_text.contains("Explicit /skill command only"));
        assert!(
            matches!(press(&mut wizard, KeyCode::Enter), Action::Create { scope: SkillScope::Global, draft } if draft.name() == "safe")
        );
    }

    #[test]
    fn publication_is_exactly_once_until_failure_and_failed_drafts_can_retry_or_edit() {
        let mut wizard = wizard();
        complete(&mut wizard);
        assert!(matches!(
            press(&mut wizard, KeyCode::Enter),
            Action::Create { .. }
        ));
        assert!(wizard.creating());
        for key in [KeyCode::Enter, KeyCode::Esc, KeyCode::Tab] {
            assert!(matches!(press(&mut wizard, key), Action::None));
        }
        wizard.paste("MUST_NOT_ENTER_DRAFT");
        wizard.failed("Cannot publish the skill".into());
        assert!(!wizard.creating());
        assert!(!wizard.body.draft().text().contains("MUST_NOT_ENTER_DRAFT"));
        assert!(matches!(
            press(&mut wizard, KeyCode::Enter),
            Action::Create { .. }
        ));
        wizard.failed("Destination already exists".into());
        press(&mut wizard, KeyCode::Tab);
        press(&mut wizard, KeyCode::Enter);
        assert_eq!(wizard.step, Step::Body);
        assert!(wizard.body.draft().text().contains("Inspect tests"));
    }

    #[test]
    fn bounded_paste_preserves_fields_and_removed_redo_is_inactive() {
        let mut wizard = wizard();
        complete(&mut wizard);
        press(&mut wizard, KeyCode::Esc);
        let original = wizard.body.draft().text();
        wizard.paste(&"x".repeat(BODY_INPUT_BYTES + 1));
        assert_eq!(wizard.body.draft().text(), original);
        assert!(wizard.notice.is_some());
        wizard.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        wizard.key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT));
        let body = "x".repeat(BODY_INPUT_BYTES);
        wizard.paste(&body);
        assert_eq!(wizard.body.draft().text(), body);
        wizard.paste("extra");
        assert_eq!(wizard.body.draft().text(), body);
        wizard.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(wizard.body.draft().text(), body);
        press(&mut wizard, KeyCode::Enter);
        assert!(wizard.review_text.contains("Preview truncated"));
        assert!(wizard.review_text.len() < REVIEW_PREVIEW_BYTES + 4096);
        assert_eq!(wizard.body.draft().text().len(), BODY_INPUT_BYTES);
    }

    #[test]
    fn metadata_fields_are_bounded_and_failures_are_terminal_safe_without_erasing_drafts() {
        let mut wizard = wizard();
        press(&mut wizard, KeyCode::Enter);
        wizard.paste(&"x".repeat(NAME_INPUT_BYTES + 1));
        assert!(wizard.name.draft().is_empty());
        assert!(wizard.notice.is_some());
        wizard.paste("safe");
        press(&mut wizard, KeyCode::Enter);
        wizard.paste(&"x".repeat(DESCRIPTION_INPUT_BYTES + 1));
        assert!(wizard.description.draft().is_empty());
        wizard.paste("A workflow");
        wizard.failed(format!("failure\u{1b}[31m{}", "x".repeat(1000)));
        assert_eq!(wizard.name.draft().text(), "safe");
        assert_eq!(wizard.description.draft().text(), "A workflow");
        assert!(wizard.notice.as_ref().unwrap().chars().count() <= 512);
        assert!(!wizard.notice.as_ref().unwrap().contains('\u{1b}'));
    }

    #[test]
    fn newlines_and_removed_undo_keys_never_advance_or_publish() {
        let mut wizard = wizard();
        for step in [
            Step::Scope,
            Step::Name,
            Step::Description,
            Step::Body,
            Step::Review,
        ] {
            wizard.step = step;
            for key in [
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT),
            ] {
                assert!(matches!(wizard.key(key), Action::None));
                assert_eq!(wizard.step, step);
                assert!(!wizard.creating);
            }
        }
        assert_eq!(wizard.body.draft().text(), "\n\n");
    }

    #[test]
    fn repeat_release_ctrl_m_and_cancel_controls_are_modal_first() {
        let mut wizard = wizard();
        let mut key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        key.kind = KeyEventKind::Repeat;
        assert!(matches!(wizard.key(key), Action::None));
        assert_eq!(wizard.step, Step::Scope);
        key.kind = KeyEventKind::Release;
        assert!(matches!(wizard.key(key), Action::None));
        wizard.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL));
        assert_eq!(wizard.step, Step::Name);
        assert!(matches!(
            wizard.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Cancel
        ));
        press(&mut wizard, KeyCode::Esc);
        assert!(matches!(press(&mut wizard, KeyCode::Esc), Action::Cancel));
    }

    #[test]
    fn wizard_is_opaque_rectangular_and_inputs_use_labelled_plain_rules_at_all_sizes() {
        for (width, height) in [(1, 1), (2, 2), (8, 6), (48, 20), (90, 26)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let palette = Palette::default();
            let mut wizard = wizard();
            for step in [
                Step::Scope,
                Step::Name,
                Step::Description,
                Step::ExplicitOnly,
                Step::Body,
                Step::Review,
            ] {
                wizard.step = step;
                terminal
                    .draw(|frame| {
                        frame.render_widget(
                            Paragraph::new("█".repeat(1000)).wrap(Wrap { trim: false }),
                            frame.area(),
                        );
                        wizard.render(frame, frame.area(), &palette);
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(0, 0)].symbol(), "┌");
                assert!(buffer.content.iter().all(|cell| cell.bg == palette.background || cell.bg == palette.selected_bg));
                assert!(
                    buffer
                        .content
                        .iter()
                        .all(|cell| !["█", "╭", "╮", "╰", "╯"].contains(&cell.symbol()))
                );
                if width >= 48 && height >= 20 && step == Step::Name {
                    let text = buffer
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(text.contains("Name (lowercase"));
                    assert!(text.contains(">"));
                    assert!(!text.contains("─Name"));
                    assert_eq!(buffer[(1, 3)].symbol(), "─");
                    assert_eq!(buffer[(width - 2, 3)].symbol(), "─");
                    assert_eq!(buffer[(1, 4)].symbol(), ">");
                    assert_eq!(buffer[(width - 2, 4)].symbol(), " ");
                }
            }
        }
    }
}
