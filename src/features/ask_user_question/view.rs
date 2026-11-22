//! Native questionnaire UI with one focused editor and retained per-question drafts.

use std::sync::Arc;

use crate::ui::editor::Editor;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use super::{Answers, Intent, Params, Questionnaire};
use crate::text::terminal_text;
use crate::theme::Palette;
use crate::ui::editor::EDITING_HINT;
use crate::ui::{chrome, markdown};

#[derive(Clone, Copy)]
enum Focus {
    Options,
    Custom,
    Notes(usize),
}

pub struct Dialog {
    pub questionnaire: Questionnaire,
    pub collapsed: bool,
    tab: usize,
    row: usize,
    rows: Vec<usize>,
    preview_scroll: u16,
    review_scroll: u16,
    focus: Focus,
    custom: Vec<Editor>,
    notes: Vec<Editor>,
    notice: Option<String>,
}

impl Dialog {
    pub fn new(params: Arc<Params>) -> Self {
        let count = params.questions.len();
        Self {
            questionnaire: Questionnaire::new(params),
            collapsed: false,
            tab: 0,
            row: 0,
            rows: vec![0; count + 1],
            preview_scroll: 0,
            review_scroll: 0,
            focus: Focus::Options,
            custom: (0..count).map(|_| Editor::default()).collect(),
            notes: (0..=count).map(|_| Editor::default()).collect(),
            notice: None,
        }
    }

    fn sync(&mut self) {
        for (index, area) in self.custom.iter().enumerate() {
            self.questionnaire.drafts[index].custom = area.draft().text();
            self.questionnaire.drafts[index].notes = self.notes[index].draft().text();
        }
        self.questionnaire.global_note = self.notes.last().expect("global note").draft().text();
    }

    fn edit_field(&mut self, key: KeyEvent) {
        let field = match self.focus {
            Focus::Custom => &mut self.custom[self.tab],
            Focus::Notes(index) => &mut self.notes[index],
            Focus::Options => return,
        };
        field.input(key);
        let total = self
            .custom
            .iter()
            .chain(&self.notes)
            .map(|editor| editor.draft().bytes())
            .sum::<usize>();
        if total > 2 * 1024 * 1024 {
            match self.focus {
                Focus::Custom => {
                    self.custom[self.tab].undo();
                }
                Focus::Notes(index) => {
                    self.notes[index].undo();
                }
                Focus::Options => unreachable!(),
            }
            self.notice = Some("Question answers exceed the 2 MiB input limit".into());
        } else {
            self.notice = None;
        }
    }

    pub fn paste(&mut self, text: &str) {
        let total = self
            .custom
            .iter()
            .chain(&self.notes)
            .map(|editor| editor.draft().bytes())
            .sum::<usize>();
        if total.saturating_add(text.len()) > 2 * 1024 * 1024 {
            self.notice = Some("Question answers exceed the 2 MiB input limit".into());
            return;
        }
        let result = match self.focus {
            Focus::Custom if self.tab < self.custom.len() => self.custom[self.tab].paste(text),
            Focus::Notes(index) => self.notes[index].paste(text),
            _ => return,
        };
        self.notice = result.err();
        self.sync();
    }

    pub fn scroll(&mut self, delta: isize) {
        let scroll = if self.tab == self.questionnaire.params.questions.len() {
            &mut self.review_scroll
        } else {
            &mut self.preview_scroll
        };
        let rows = u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);
        *scroll = if delta < 0 {
            scroll.saturating_sub(rows)
        } else {
            scroll.saturating_add(rows)
        };
    }

    fn advance(&mut self) -> Option<Answers> {
        self.sync();
        if self.questionnaire.params.questions.len() == 1 {
            return Some(self.questionnaire.finish(false));
        }
        self.rows[self.tab] = self.row;
        self.tab += 1;
        self.row = self.rows[self.tab];
        self.focus = Focus::Options;
        None
    }

    pub fn key(&mut self, mut key: KeyEvent) -> Option<Answers> {
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
            return None;
        }
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.kind != KeyEventKind::Release
        {
            self.sync();
            return Some(self.questionnaire.finish(true));
        }
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(']') {
            if key.kind == KeyEventKind::Repeat {
                return None;
            }
            self.collapsed = !self.collapsed;
            return None;
        }
        if self.collapsed {
            if key.code == KeyCode::Esc {
                self.sync();
                return Some(self.questionnaire.finish(true));
            }
            return None;
        }
        match self.focus {
            Focus::Notes(_) => {
                match key.code {
                    KeyCode::Esc => {
                        self.focus = Focus::Options;
                        self.sync();
                    }
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.edit_field(key);
                    }
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.edit_field(key);
                    }
                    KeyCode::Enter => {
                        self.focus = Focus::Options;
                        self.sync();
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        self.switch_tab(key.code == KeyCode::BackTab)
                    }
                    _ => {
                        self.edit_field(key);
                    }
                }
                self.sync();
                return None;
            }
            Focus::Custom => {
                match key.code {
                    KeyCode::Esc => {
                        self.sync();
                        return Some(self.questionnaire.finish(true));
                    }
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.edit_field(key);
                    }
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.edit_field(key);
                    }
                    KeyCode::Enter => {
                        self.sync();
                        if self.questionnaire.commit_custom(self.tab) {
                            self.focus = Focus::Options;
                            return self.advance();
                        }
                    }
                    KeyCode::Up if self.custom[self.tab].first_visual_row() => {
                        self.focus = Focus::Options;
                        self.row = self.row.saturating_sub(1);
                    }
                    KeyCode::Down if self.custom[self.tab].last_visual_row() => {
                        self.focus = Focus::Options;
                        self.row += 1;
                        if !self.questionnaire.params.questions[self.tab].multi_select {
                            self.row = 0;
                        }
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        self.switch_tab(key.code == KeyCode::BackTab);
                    }
                    _ => {
                        self.edit_field(key);
                    }
                }
                self.sync();
                return None;
            }
            Focus::Options => {}
        }
        if crate::ui::editor::is_newline(key) {
            return None; // Newline keys never confirm a non-editor action.
        }
        if key.code == KeyCode::Esc {
            self.sync();
            return Some(self.questionnaire.finish(true));
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.switch_tab(key.code == KeyCode::BackTab);
            return None;
        }
        if key.code == KeyCode::Char('n') {
            self.focus = Focus::Notes(self.tab);
            return None;
        }
        let count = self.questionnaire.params.questions.len();
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let scroll = if self.tab == count {
                &mut self.review_scroll
            } else {
                &mut self.preview_scroll
            };
            *scroll = if key.code == KeyCode::PageUp {
                scroll.saturating_sub(8)
            } else {
                scroll.saturating_add(8)
            };
            return None;
        }
        if self.tab == count {
            match key.code {
                KeyCode::Up | KeyCode::Down => self.row = 1 - self.row.min(1),
                KeyCode::Enter => {
                    self.sync();
                    return Some(self.questionnaire.finish(self.row == 1));
                }
                _ => {}
            }
            return None;
        }
        let question = &self.questionnaire.params.questions[self.tab];
        let options = question.options.len();
        let rows = options + 1 + usize::from(question.multi_select);
        if matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Char('1'..='9')
        ) {
            self.preview_scroll = 0;
        }
        match key.code {
            KeyCode::Up => self.row = (self.row + rows - 1) % rows,
            KeyCode::Down => self.row = (self.row + 1) % rows,
            KeyCode::Char(digit) if ('1'..='9').contains(&digit) => {
                let index = digit as usize - '1' as usize;
                if index <= options {
                    self.row = index;
                    return self.activate();
                }
            }
            KeyCode::Enter => return self.activate(),
            KeyCode::Char(' ') if question.multi_select && self.row < options => {
                self.questionnaire.toggle(self.tab, self.row)
            }
            _ => {}
        }
        if self.row == options {
            self.focus = Focus::Custom;
        }
        None
    }

    fn switch_tab(&mut self, previous: bool) {
        self.sync();
        let questions = self.questionnaire.params.questions.len();
        if questions == 1 {
            self.focus = Focus::Options;
        }
        if questions > 1 {
            let tabs = questions + 1;
            self.rows[self.tab] = self.row;
            self.tab = (self.tab + if previous { tabs - 1 } else { 1 }) % tabs;
            self.row = self.rows[self.tab];
            self.preview_scroll = 0;
            self.review_scroll = 0;
            self.focus = Focus::Options;
        }
    }

    fn activate(&mut self) -> Option<Answers> {
        let question = &self.questionnaire.params.questions[self.tab];
        let options = question.options.len();
        if self.row == options {
            self.focus = Focus::Custom;
            return None;
        }
        if self.row > options {
            self.questionnaire.drafts[self.tab].intent = Some(Intent::Multi);
            return self.advance();
        }
        if question.multi_select {
            self.questionnaire.toggle(self.tab, self.row);
            None
        } else {
            self.questionnaire.choose(self.tab, self.row);
            self.advance()
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
        let inner = chrome::modal(frame, area, palette, "Ask user");
        if inner.is_empty() {
            return;
        }
        if self.collapsed {
            frame.render_widget(
                Paragraph::new("Questionnaire collapsed; Ctrl+] to expand, Esc to cancel")
                    .style(Style::default().fg(palette.muted)),
                inner,
            );
            return;
        }
        let hint = match self.focus {
            Focus::Notes(_) => format!("Enter/Esc done · Shift+Enter/Ctrl+J newline · Tab questions · {EDITING_HINT}"),
            Focus::Custom => format!("Enter confirm · Shift+Enter/Ctrl+J newline · Tab questions · Esc cancel · {EDITING_HINT}"),
            Focus::Options => "Enter select · ↑/↓ options · Tab questions · PgUp/PgDn preview/review · n notes · Esc cancel · Ctrl+] collapse".into(),
        };
        let actions = self.notice.clone().unwrap_or(hint);
        let maximum_actions = inner.height.saturating_sub(7).clamp(1, 3);
        let action_height = chrome::actions_height(&actions, inner.width, maximum_actions);
        let [body, footer] = chrome::body_actions(inner, action_height);
        let parts = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).split(body);
        let tabs = self
            .questionnaire
            .params
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                Span::styled(
                    format!(
                        " {}{} ",
                        if self.questionnaire.drafts[index].intent.is_some() {
                            "[x] "
                        } else {
                            "[ ] "
                        },
                        terminal_text(&question.header, false)
                    ),
                    Style::default().fg(if index == self.tab {
                        palette.accent
                    } else {
                        palette.muted
                    }),
                )
            })
            .chain((self.questionnaire.params.questions.len() > 1).then(|| {
                Span::styled(
                    " Submit ",
                    Style::default().fg(if self.tab == self.questionnaire.params.questions.len() {
                        palette.accent
                    } else {
                        palette.muted
                    }),
                )
            }))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(Line::from(tabs)), parts[0]);
        match self.focus {
            Focus::Notes(index) => {
                self.notes[index].render_labeled(frame, parts[1], palette, "Notes", true);
            }
            Focus::Custom => {
                self.custom[self.tab].render_labeled(
                    frame,
                    parts[1],
                    palette,
                    "Type something.",
                    true,
                );
            }
            Focus::Options => {
                if self.tab == self.questionnaire.params.questions.len() {
                    self.render_submit(frame, parts[1], palette);
                } else {
                    self.render_options(frame, parts[1], palette);
                }
            }
        }
        chrome::actions(frame, footer, palette, &actions);
    }

    fn render_submit(&mut self, frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
        let mut lines = vec![Line::styled(
            "Review your answers",
            Style::default().fg(palette.accent),
        )];
        let answers = self.questionnaire.finish(false);
        for (index, question) in self.questionnaire.params.questions.iter().enumerate() {
            let answered = self.questionnaire.drafts[index].intent.is_some();
            lines.push(Line::styled(
                format!(
                    "{} {}",
                    if answered { "[x]" } else { "[ ]" },
                    terminal_text(&question.question, false)
                ),
                Style::default().fg(if answered {
                    palette.text
                } else {
                    palette.warning
                }),
            ));
            if let Some(answer) = answers
                .answers
                .iter()
                .find(|answer| answer.question_index == index)
            {
                let value = answer
                    .selected
                    .iter()
                    .flatten()
                    .cloned()
                    .chain(answer.answer.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(Line::styled(
                    format!("  {}", terminal_text(&value, false)),
                    Style::default().fg(palette.muted),
                ));
            }
            let notes = self.questionnaire.drafts[index].notes.trim();
            if !notes.is_empty() {
                lines.push(Line::styled(
                    format!("  notes: {}", terminal_text(notes, false)),
                    Style::default().fg(palette.muted),
                ));
            }
        }
        if let Some(note) = &answers.global_note {
            lines.push(Line::styled(
                format!("Global note: {}", terminal_text(note, false)),
                Style::default().fg(palette.muted),
            ));
        }
        let parts = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(area);
        let review = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max_scroll = u16::try_from(review.line_count(parts[0].width))
            .unwrap_or(u16::MAX)
            .saturating_sub(parts[0].height);
        self.review_scroll = self.review_scroll.min(max_scroll);
        frame.render_widget(review.scroll((self.review_scroll, 0)), parts[0]);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!("{} Submit answers", if self.row == 0 { "❯" } else { " " }),
                    Style::default().fg(palette.accent),
                ),
                Line::styled(
                    format!("{} Cancel", if self.row == 1 { "❯" } else { " " }),
                    Style::default().fg(palette.muted),
                ),
            ]),
            parts[1],
        );
    }

    fn render_options(&mut self, frame: &mut Frame<'_>, area: Rect, palette: &Palette) {
        let question = &self.questionnaire.params.questions[self.tab];
        let heading = Paragraph::new(terminal_text(&question.question, true))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(palette.text));
        let rows = heading.line_count(area.width.max(1));
        let height = rows
            .min(usize::from((area.height / 3).max(1)))
            .min(usize::from(area.height)) as u16;
        let maximum = rows.saturating_sub(usize::from(height));
        let scroll = usize::from(self.preview_scroll)
            .min(maximum)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(heading.scroll((scroll, 0)), Rect { height, ..area });
        let start = height.saturating_add(1).min(area.height);
        let body = Rect {
            y: area.y + start,
            height: area.height - start,
            ..area
        };
        let preview = !question.multi_select
            && question.options.iter().any(|option| {
                option
                    .preview
                    .as_ref()
                    .is_some_and(|preview| !preview.is_empty())
            });
        let columns = if preview && body.width >= 100 {
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(body)
        } else {
            Layout::vertical([
                Constraint::Percentage(if preview { 55 } else { 100 }),
                Constraint::Min(0),
            ])
            .split(body)
        };
        let mut items = question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let draft = &self.questionnaire.drafts[self.tab];
                let selected = if question.multi_select {
                    draft.selected.contains(&index)
                } else {
                    matches!(draft.intent, Some(Intent::Option(selected)) if selected == index)
                };
                let marker = if selected { "[x] " } else { "[ ] " };
                let mut lines = vec![
                    Line::from(format!(
                        "{}. {marker}{}",
                        index + 1,
                        terminal_text(&option.label, false)
                    )),
                    Line::styled(
                        format!("   {}", terminal_text(&option.description, false)),
                        Style::default().fg(palette.muted),
                    ),
                ];
                if columns[0].height >= 8 {
                    lines.push(Line::default());
                }
                Text::from(lines)
            })
            .collect::<Vec<_>>();
        let custom = self.custom[self.tab].draft().summary();
        items.push(Text::from(format!(
            "{}. {}",
            items.len() + 1,
            if custom.trim().is_empty() {
                "Type something.".into()
            } else {
                format!("Type something.: {}", terminal_text(&custom, false))
            }
        )));
        if question.multi_select {
            items.push(Text::from(
                if matches!(
                    self.questionnaire.drafts[self.tab].intent,
                    Some(Intent::Multi)
                ) {
                    "Next [confirmed]"
                } else {
                    "Next"
                },
            ));
        }
        let heights = items
            .iter()
            .map(|text| {
                Paragraph::new(text.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(columns[0].width)
                    .min(usize::from(u16::MAX)) as u16
            })
            .collect::<Vec<_>>();
        let mut first = 0;
        while first < self.row
            && heights[first..=self.row.min(heights.len() - 1)]
                .iter()
                .map(|height| usize::from(*height))
                .sum::<usize>()
                > usize::from(columns[0].height)
        {
            first += 1;
        }
        let mut y = columns[0].y;
        for (index, text) in items.into_iter().enumerate().skip(first) {
            if y >= columns[0].bottom() {
                break;
            }
            let height = heights[index].min(columns[0].bottom() - y);
            let style = Style::default().fg(if index == self.row {
                palette.accent
            } else {
                palette.text
            });
            frame.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }).style(style),
                Rect {
                    y,
                    height,
                    ..columns[0]
                },
            );
            y += height;
        }
        if preview {
            let block = chrome::section(palette, "Preview");
            let inner = block.inner(columns[1]);
            frame.render_widget(block, columns[1]);
            let text = question
                .options
                .get(self.row)
                .and_then(|option| option.preview.as_deref())
                .unwrap_or("No preview available");
            let preview =
                Paragraph::new(markdown::render(text, palette)).wrap(Wrap { trim: false });
            let max_scroll = u16::try_from(preview.line_count(inner.width))
                .unwrap_or(u16::MAX)
                .saturating_sub(inner.height);
            self.preview_scroll = self.preview_scroll.min(max_scroll);
            frame.render_widget(preview.scroll((self.preview_scroll, 0)), inner);
        }
    }
}
