//! Pi-style visual-row editing with typed, atomic paste/image attachments.
//!
//! TextArea supplies editing/selection, not drawing. Private-use tokens are
//! internal references; incoming private-use text is escaped as literal content.
//! A displayed chip is never parsed back out of user text.
//!
//! Ctrl+Z undoes/recovers an input edit; redo is deliberately unsupported.
//! Each editor change is atomic, including replacement selections, completion,
//! and paste expansion. Cursor movement does not create an undo record. Undo
//! restores text, attachments and cursor, cancelling selection. History/queue
//! restoration and reset start a fresh undo boundary without losing the draft.
//! Undo retains at most 100 snapshots and a conservative 32 MiB budget.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_textarea::{CursorMove, TextArea};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::message::Content;
use crate::text::terminal_text;
use crate::theme::Palette;

const MAX_BYTES: usize = 24 * 1024 * 1024;
const HISTORY_BYTES: usize = 32 * 1024 * 1024;
const TOKEN_FIRST: u32 = 0xf0000;
const TOKEN_LAST: u32 = 0xffffd;

/// Bindings shared by modal fields and skill creation; /help has the full list.
pub const EDITING_HINT: &str = "Shift+Enter/Ctrl+J newline · Ctrl+U/K delete line · Ctrl+D delete forward · Ctrl+Z undo/recover";

pub fn is_newline(key: KeyEvent) -> bool {
    !key.modifiers.contains(KeyModifiers::ALT)
        && (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT)
            || key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
}

#[derive(Clone)]
enum Attachment {
    Paste {
        number: usize,
        body: Arc<str>,
    },
    Image {
        number: usize,
        content: Arc<Content>,
    },
    Literal(char),
}

impl Attachment {
    fn bytes(&self) -> usize {
        match self {
            Self::Paste { body, .. } => body.len(),
            Self::Image { content, .. } => match content.as_ref() {
                Content::Image { data, .. } => data.len(),
                Content::Text { text } => text.len(),
            },
            Self::Literal(c) => c.len_utf8(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Paste { number, body } => {
                let lines = body.lines().count().max(1);
                if lines > 3 {
                    format!("[Pasted text #{number} +{} lines]", lines - 1)
                } else {
                    format!("[Pasted text #{number} {} chars]", body.chars().count())
                }
            }
            Self::Image { number, .. } => format!("[Image #{number}]"),
            Self::Literal(c) => c.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct Draft {
    lines: Vec<String>,
    cursor: (usize, usize),
    attachments: Arc<BTreeMap<char, Attachment>>,
}

impl Draft {
    pub fn content(&self) -> Vec<Content> {
        let mut content = Vec::new();
        let mut text = String::new();
        for c in self.lines.join("\n").chars() {
            match self.attachments.get(&c) {
                Some(Attachment::Paste { body, .. }) => text.push_str(body),
                Some(Attachment::Literal(original)) => text.push(*original),
                Some(Attachment::Image { content: image, .. }) => {
                    if !text.is_empty() {
                        content.push(Content::text(std::mem::take(&mut text)));
                    }
                    content.push(image.as_ref().clone());
                }
                None => text.push(c),
            }
        }
        if !text.is_empty() {
            content.push(Content::text(text));
        }
        content
    }

    pub fn text(&self) -> String {
        crate::message::content_text(&self.content())
    }

    pub fn has_images(&self) -> bool {
        self.attachments
            .values()
            .any(|a| matches!(a, Attachment::Image { .. }))
    }

    pub fn is_empty(&self) -> bool {
        !self.has_images() && self.text().trim().is_empty()
    }

    pub fn bytes(&self) -> usize {
        expanded_bytes(&self.lines, &self.attachments)
    }

    pub fn summary(&self) -> String {
        self.lines
            .iter()
            .flat_map(|line| line.chars())
            .map(|c| {
                self.attachments
                    .get(&c)
                    .map_or_else(|| c.to_string(), Attachment::label)
            })
            .collect()
    }
}

fn expanded_bytes(lines: &[String], attachments: &BTreeMap<char, Attachment>) -> usize {
    lines
        .iter()
        .map(String::len)
        .sum::<usize>()
        .saturating_add(lines.len().saturating_sub(1))
        .saturating_add(
            lines
                .iter()
                .flat_map(|line| line.chars())
                .filter_map(|token| attachments.get(&token))
                .map(Attachment::bytes)
                .fold(0usize, usize::saturating_add),
        )
}

#[derive(Clone)]
struct Unit {
    text: String,
    row: usize,
    column: usize,
    end_column: usize,
    chip: bool,
}

struct VisualRow {
    units: Vec<Unit>,
}

pub struct Editor {
    area: TextArea<'static>,
    attachments: Arc<BTreeMap<char, Attachment>>,
    next_token: u32,
    next_paste: usize,
    next_image: usize,
    history: VecDeque<Draft>,
    history_index: Option<usize>,
    history_draft: Option<Draft>,
    width: usize,
    scroll: usize,
    preferred_column: Option<usize>,
    last_paste: Option<char>,
    undo: VecDeque<Draft>,
    changing: bool,
    undo_boundary: u64,
}

impl Default for Editor {
    fn default() -> Self {
        let mut area = TextArea::default();
        // TextArea records selection removal separately from insertion. Shared
        // editor snapshots instead make the entire input action one undo unit.
        area.set_max_histories(0);
        Self {
            area,
            attachments: Arc::new(BTreeMap::new()),
            next_token: TOKEN_FIRST,
            next_paste: 0,
            next_image: 0,
            history: VecDeque::new(),
            history_index: None,
            history_draft: None,
            width: 70,
            scroll: 0,
            preferred_column: None,
            last_paste: None,
            undo: VecDeque::new(),
            changing: false,
            undo_boundary: 0,
        }
    }
}

impl Editor {
    pub fn lines(&self) -> &[String] {
        self.area.lines()
    }
    pub fn cursor(&self) -> (usize, usize) {
        self.area.cursor()
    }

    pub fn draft(&self) -> Draft {
        let live = self
            .area
            .lines()
            .iter()
            .flat_map(|s| s.chars())
            .collect::<BTreeSet<_>>();
        Draft {
            lines: self.area.lines().to_vec(),
            cursor: self.area.cursor(),
            attachments: Arc::new(
                self.attachments
                    .iter()
                    .filter(|(c, _)| live.contains(c))
                    .map(|(c, a)| (*c, a.clone()))
                    .collect(),
            ),
        }
    }

    /// Restore a saved/history/queued draft, starting a fresh undo boundary.
    pub fn restore(&mut self, draft: Draft) {
        self.apply_draft(draft);
        self.undo.clear();
        self.undo_boundary = self.undo_boundary.wrapping_add(1);
    }

    fn apply_draft(&mut self, draft: Draft) {
        self.area = TextArea::from(draft.lines);
        self.area.set_max_histories(0);
        self.attachments = draft.attachments;
        self.next_token = self
            .attachments
            .keys()
            .map(|c| *c as u32 + 1)
            .max()
            .unwrap_or(TOKEN_FIRST);
        self.next_paste = self
            .attachments
            .values()
            .filter_map(|a| match a {
                Attachment::Paste { number, .. } => Some(*number),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        self.next_image = self
            .attachments
            .values()
            .filter_map(|a| match a {
                Attachment::Image { number, .. } => Some(*number),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        self.jump(draft.cursor.0, draft.cursor.1);
        self.history_index = None;
        self.history_draft = None;
        self.scroll = 0;
        self.preferred_column = None;
        self.last_paste = None;
    }

    pub fn reset(&mut self) {
        let history = std::mem::take(&mut self.history);
        *self = Self::default();
        self.history = history;
    }

    pub fn clear(&mut self) {
        let _ = self.change(|editor| {
            editor.area.select_all();
            editor.area.delete_next_char();
            editor.edited();
            Ok(())
        });
    }

    /// Perform one atomic edit, including nested paste/completion operations.
    fn change<T>(
        &mut self,
        edit: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        if self.changing {
            return edit(self);
        }
        let before = self.draft();
        let boundary = self.undo_boundary;
        self.changing = true;
        let result = edit(self).and_then(|value| self.validate().map(|()| value));
        self.changing = false;
        if result.is_err() {
            self.apply_change(before);
            return result;
        }
        if self.undo_boundary == boundary && self.area.lines() != before.lines {
            self.undo.push_back(before);
            self.trim_changes();
        }
        result
    }

    fn validate(&self) -> Result<(), String> {
        if expanded_bytes(self.area.lines(), &self.attachments) > MAX_BYTES {
            return Err("Input exceeds the 24 MiB editor limit".into());
        }
        if self.area.lines().len() > usize::from(u16::MAX)
            || self
                .area
                .lines()
                .iter()
                .any(|line| line.chars().count() > usize::from(u16::MAX))
        {
            return Err("Input is too large to expand inline; keep it folded".into());
        }
        Ok(())
    }

    fn trim_changes(&mut self) {
        while self.undo.len() > 100
            || self.undo.iter().map(Draft::bytes).sum::<usize>() > HISTORY_BYTES
        {
            self.undo.pop_front();
        }
    }

    // Undo should not erase the existing readline yank buffer. Its internal
    // attachment references must remain registered even when absent in text.
    fn apply_change(&mut self, draft: Draft) {
        let yank = self.area.yank_text();
        let attachments = self.attachments.clone();
        self.apply_draft(draft);
        for token in yank.chars() {
            if let Some(attachment) = attachments.get(&token) {
                Arc::make_mut(&mut self.attachments).insert(token, attachment.clone());
                self.next_token = self.next_token.max(token as u32 + 1);
                match attachment {
                    Attachment::Paste { number, .. } => {
                        self.next_paste = self.next_paste.max(*number);
                    }
                    Attachment::Image { number, .. } => {
                        self.next_image = self.next_image.max(*number);
                    }
                    Attachment::Literal(_) => {}
                }
            }
        }
        self.area.set_yank_text(yank);
    }

    /// Undo one complete change, preserving folded bodies and image content.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop_back() else {
            return false;
        };
        self.apply_change(previous);
        true
    }

    fn register(&mut self, attachment: Attachment) -> Result<char, String> {
        let bytes = self
            .attachments
            .values()
            .map(Attachment::bytes)
            .sum::<usize>()
            + self.area.lines().iter().map(String::len).sum::<usize>();
        if bytes.saturating_add(attachment.bytes()) > MAX_BYTES {
            let live = self.draft();
            let yank = self.area.yank_text().chars().collect::<BTreeSet<_>>();
            let retained = self
                .attachments
                .iter()
                .filter(|(token, _)| live.attachments.contains_key(token) || yank.contains(token))
                .map(|(token, attachment)| (*token, attachment.clone()))
                .collect::<BTreeMap<_, _>>();
            let retained_bytes = retained.values().map(Attachment::bytes).sum::<usize>()
                + self.area.lines().iter().map(String::len).sum::<usize>();
            if retained_bytes.saturating_add(attachment.bytes()) > MAX_BYTES {
                return Err("Input attachments exceed the 24 MiB editor limit".into());
            }
            // Snapshots own their attachment maps. Retire only unreachable
            // active references, keeping the yank buffer and undo snapshots.
            self.attachments = Arc::new(retained);
        }
        if self.next_token > TOKEN_LAST {
            return Err("Editor attachment token limit reached".into());
        }
        let token = char::from_u32(self.next_token).expect("private-use token range");
        self.next_token += 1;
        Arc::make_mut(&mut self.attachments).insert(token, attachment);
        Ok(token)
    }

    pub fn insert_str(&mut self, text: impl AsRef<str>) -> bool {
        self.insert_text(text.as_ref()).is_ok()
    }

    pub fn insert_text(&mut self, text: &str) -> Result<(), String> {
        self.change(|editor| editor.insert_text_inner(text))
    }

    fn insert_text_inner(&mut self, text: &str) -> Result<(), String> {
        if text.len() > MAX_BYTES {
            return Err("Input exceeds the 24 MiB editor limit".into());
        }
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        if clean
            .lines()
            .any(|line| line.chars().count() > usize::from(u16::MAX))
            || self
                .area
                .lines()
                .len()
                .saturating_add(clean.lines().count())
                > usize::from(u16::MAX)
        {
            return Err("Input is too large to expand inline; keep it folded".into());
        }
        let mut escaped = String::new();
        for c in clean.chars() {
            if (TOKEN_FIRST..=TOKEN_LAST).contains(&(c as u32)) {
                escaped.push(self.register(Attachment::Literal(c))?);
            } else {
                escaped.push(c);
            }
        }
        self.area.insert_str(escaped);
        self.edited();
        Ok(())
    }

    pub fn paste(&mut self, text: &str) -> Result<(), String> {
        self.change(|editor| editor.paste_inner(text))
    }

    fn paste_inner(&mut self, text: &str) -> Result<(), String> {
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(token) = self.last_paste
            && self.area.cursor().1 > 0
            && self.area.lines()[self.area.cursor().0]
                .chars()
                .nth(self.area.cursor().1 - 1)
                == Some(token)
            && let Some(Attachment::Paste { body, .. }) = self.attachments.get(&token)
            && body.as_ref() == clean
        {
            if clean
                .lines()
                .any(|line| line.chars().count() > usize::from(u16::MAX))
            {
                return Err("Paste is too large to expand inline; it remains folded".into());
            }
            let saved = self.area.clone();
            self.area.delete_char();
            let result = self.insert_text(&clean);
            if result.is_err() {
                self.area = saved;
            }
            return result;
        }
        if clean.chars().count() <= 800 && clean.split('\n').count() <= 3 {
            return self.insert_text(&clean);
        }
        self.next_paste += 1;
        let token = self.register(Attachment::Paste {
            number: self.next_paste,
            body: Arc::from(clean),
        })?;
        self.area.insert_char(token);
        self.edited();
        self.last_paste = Some(token);
        Ok(())
    }

    pub fn image(&mut self, content: Content) -> Result<(), String> {
        self.change(|editor| editor.image_inner(content))
    }

    fn image_inner(&mut self, content: Content) -> Result<(), String> {
        self.next_image += 1;
        let token = self.register(Attachment::Image {
            number: self.next_image,
            content: Arc::new(content),
        })?;
        self.area.insert_char(token);
        self.edited();
        Ok(())
    }

    pub fn from_content(content: &[Content]) -> Result<Self, String> {
        let mut editor = Self::default();
        for part in content {
            match part {
                Content::Text { text } => editor.paste(text)?,
                Content::Image { .. } => editor.image(part.clone())?,
            }
        }
        Ok(editor)
    }

    pub fn remember(&mut self, draft: Draft) {
        if draft.is_empty() {
            return;
        }
        let text = draft.text();
        if let Some(index) = self
            .history
            .iter()
            .position(|old| old.text() == text && !old.has_images() && !draft.has_images())
        {
            self.history.remove(index);
        }
        self.history.push_front(draft);
        while self.history.len() > 100
            || self.history.iter().map(Draft::bytes).sum::<usize>() > HISTORY_BYTES
        {
            self.history.pop_back();
        }
    }

    fn history(&mut self, previous: bool) {
        let next = if previous {
            Some(self.history_index.map_or(0, |i| i + 1))
        } else {
            self.history_index.and_then(|i| i.checked_sub(1))
        };
        if next.is_some_and(|i| i >= self.history.len()) {
            return;
        }
        if self.history_index.is_none() && previous {
            self.history_draft = Some(self.draft());
        }
        let saved = self.history_draft.take();
        let draft = next
            .map(|i| self.history[i].clone())
            .or_else(|| saved.clone());
        if let Some(draft) = draft {
            self.restore(draft);
            self.history_index = next;
            self.history_draft = saved;
            if previous {
                self.jump(0, 0);
            }
        }
    }

    fn edited(&mut self) {
        self.history_index = None;
        self.history_draft = None;
        self.last_paste = None;
        self.preferred_column = None;
    }

    fn jump(&mut self, row: usize, column: usize) {
        self.area.move_cursor(CursorMove::Jump(
            u16::try_from(row).unwrap_or(u16::MAX),
            u16::try_from(column).unwrap_or(u16::MAX),
        ));
    }

    pub fn insert_newline(&mut self) -> bool {
        self.change(|editor| {
            editor.area.insert_newline();
            editor.edited();
            Ok(())
        })
        .is_ok()
    }

    pub fn before_cursor(&self) -> String {
        let (row, column) = self.area.cursor();
        self.area.lines()[row].chars().take(column).collect()
    }

    pub fn complete(&mut self, count: usize, value: &str) -> Result<(), String> {
        self.change(|editor| {
            let (row, column) = editor.area.cursor();
            editor.jump(row, column.saturating_sub(count));
            editor.area.delete_str(count);
            editor.insert_text(value)
        })
    }

    pub fn input(&mut self, key: KeyEvent) -> bool {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return false;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if !alt && control && key.code == KeyCode::Char('z') {
            return self.undo();
        }
        if control && key.code == KeyCode::Char('r') {
            return false; // Redo is deliberately unsupported.
        }
        // Keep former OS-conflicting bindings inactive.
        if key.code == KeyCode::Char('z') && alt
            || matches!(key.code, KeyCode::Char('_' | '-')) && control
        {
            return false;
        }
        if is_newline(key) {
            return self.insert_newline();
        }
        self.change(|editor| Ok(editor.input_inner(key)))
            .unwrap_or(false)
    }

    fn input_inner(&mut self, mut key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('b' | 'f' | 'd'))
        {
            key.code = match key.code {
                KeyCode::Char('b') => KeyCode::Left,
                KeyCode::Char('f') => KeyCode::Right,
                _ => KeyCode::Delete,
            };
            key.modifiers.remove(KeyModifiers::CONTROL);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if control && !alt && matches!(key.code, KeyCode::Char('u' | 'k')) {
            let changed = if key.code == KeyCode::Char('u') {
                self.area.delete_line_by_head()
            } else {
                self.area.delete_line_by_end()
            };
            self.edited();
            return changed;
        }
        if shift {
            self.area.start_selection();
        } else if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
        ) {
            self.area.cancel_selection();
        }
        if matches!(key.code, KeyCode::Up | KeyCode::Down) && !control && !alt {
            let previous = key.code == KeyCode::Up;
            let rows = self.layout(self.width);
            let (visual, col) = self.visual_cursor(&rows);
            if previous && visual == 0 && !shift {
                if self.area.cursor().1 == 0
                    || self.history_index.is_some()
                    || self.draft().is_empty()
                {
                    self.history(true);
                } else {
                    self.jump(0, 0);
                }
            } else if !previous
                && visual + 1 == rows.len()
                && self.history_index.is_some()
                && !shift
            {
                self.history(false);
            } else {
                let target = if previous {
                    visual.saturating_sub(1)
                } else {
                    (visual + 1).min(rows.len().saturating_sub(1))
                };
                let preferred = self.preferred_column.unwrap_or(col);
                let units = &rows[target].units;
                if let Some(unit) = units
                    .iter()
                    .scan(0, |used, u| {
                        let start = *used;
                        *used += UnicodeWidthStr::width(u.text.as_str());
                        Some((start, u))
                    })
                    .min_by_key(|(start, _)| start.abs_diff(preferred))
                    .map(|(_, u)| u)
                {
                    self.jump(unit.row, unit.column);
                }
                self.preferred_column = Some(preferred);
            }
            self.last_paste = None;
            return true;
        }
        if matches!(key.code, KeyCode::Left | KeyCode::Right) && !control && !alt {
            let (row, column) = self.area.cursor();
            let line = &self.area.lines()[row];
            let mut offsets = vec![0];
            let mut chars = 0;
            for g in line.graphemes(true) {
                chars += g.chars().count();
                offsets.push(chars);
            }
            if key.code == KeyCode::Left {
                if let Some(col) = offsets.into_iter().filter(|c| *c < column).max() {
                    self.jump(row, col);
                } else {
                    self.area.move_cursor(CursorMove::Back);
                }
            } else if let Some(col) = offsets.into_iter().find(|c| *c > column) {
                self.jump(row, col);
            } else {
                self.area.move_cursor(CursorMove::Forward);
            }
            self.preferred_column = None;
            self.last_paste = None;
            return true;
        }
        if key.code == KeyCode::Backspace && !control && !alt && !self.area.is_selecting() {
            let (row, col) = self.area.cursor();
            let text = self.area.lines()[row].chars().take(col).collect::<String>();
            let count = text
                .graphemes(true)
                .next_back()
                .map_or(1, |g| g.chars().count());
            let changed = if col == 0 {
                self.area.delete_char()
            } else {
                self.jump(row, col.saturating_sub(count));
                self.area.delete_str(count)
            };
            self.edited();
            return changed;
        }
        if let KeyCode::Char(c) = key.code
            && !control
            && !alt
        {
            return self.insert_text(&c.to_string()).is_ok();
        }
        if key.code == KeyCode::Delete && !control && !alt && !self.area.is_selecting() {
            let (row, col) = self.area.cursor();
            let tail = self.area.lines()[row].chars().skip(col).collect::<String>();
            let count = tail.graphemes(true).next().map_or(1, |g| g.chars().count());
            let changed = self.area.delete_str(count);
            self.edited();
            return changed;
        }
        let changed = self.area.input(key);
        self.last_paste = None;
        self.preferred_column = None;
        if changed {
            self.history_index = None;
            self.history_draft = None;
        }
        changed
    }

    fn layout(&self, width: usize) -> Vec<VisualRow> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for (row, line) in self.area.lines().iter().enumerate() {
            let mut units = Vec::new();
            let mut column = 0;
            for grapheme in line.graphemes(true) {
                let end_column = column + grapheme.chars().count();
                let attachment = grapheme
                    .chars()
                    .next()
                    .and_then(|c| self.attachments.get(&c));
                let label = attachment.map_or_else(|| grapheme.to_owned(), Attachment::label);
                let safe = if label == "\t" {
                    "    ".to_owned()
                } else {
                    terminal_text(&label, false)
                };
                for part in safe.graphemes(true) {
                    units.push(Unit {
                        text: part.to_owned(),
                        row,
                        column,
                        end_column,
                        chip: attachment.is_some(),
                    });
                }
                column = end_column;
            }
            // A real cursor cell is reserved, including at the end of a full row.
            units.push(Unit {
                text: " ".into(),
                row,
                column,
                end_column: column,
                chip: false,
            });
            let mut current: Vec<Unit> = Vec::new();
            let mut used = 0;
            for unit in units {
                let cell_width = UnicodeWidthStr::width(unit.text.as_str()).min(width);
                if used + cell_width > width && !current.is_empty() {
                    let break_at = current
                        .iter()
                        .rposition(|u| u.text.chars().all(char::is_whitespace) && !u.chip)
                        .map(|i| i + 1);
                    if let Some(at) = break_at.filter(|at| *at < current.len()) {
                        let rest = current.split_off(at);
                        rows.push(VisualRow {
                            units: std::mem::replace(&mut current, rest),
                        });
                        used = current
                            .iter()
                            .map(|u| UnicodeWidthStr::width(u.text.as_str()).min(width))
                            .sum();
                    } else {
                        rows.push(VisualRow {
                            units: std::mem::take(&mut current),
                        });
                        used = 0;
                    }
                }
                if used + cell_width > width && !current.is_empty() {
                    rows.push(VisualRow {
                        units: std::mem::take(&mut current),
                    });
                    used = 0;
                }
                used += cell_width;
                current.push(unit);
            }
            rows.push(VisualRow { units: current });
        }
        rows
    }

    fn visual_cursor(&self, rows: &[VisualRow]) -> (usize, usize) {
        let cursor = self.area.cursor();
        for (index, row) in rows.iter().enumerate() {
            let mut col = 0;
            for unit in &row.units {
                if (unit.row, unit.column) == cursor {
                    return (index, col);
                }
                col += UnicodeWidthStr::width(unit.text.as_str());
            }
        }
        (rows.len().saturating_sub(1), 0)
    }

    pub fn first_visual_row(&self) -> bool {
        self.visual_cursor(&self.layout(self.width)).0 == 0
    }
    pub fn last_visual_row(&self) -> bool {
        let rows = self.layout(self.width);
        self.visual_cursor(&rows).0 + 1 == rows.len()
    }

    pub fn click(&mut self, row: u16, column: u16) {
        let rows = self.layout(self.width);
        if let Some(row) = rows.get(self.scroll + usize::from(row)) {
            let target = usize::from(column.saturating_sub(2));
            let mut used = 0;
            for unit in &row.units {
                let width = UnicodeWidthStr::width(unit.text.as_str());
                if used + width > target {
                    self.jump(unit.row, unit.column);
                    break;
                }
                used += width;
            }
        }
    }

    pub fn height(&self, width: u16, maximum: u16) -> u16 {
        u16::try_from(self.layout(usize::from(width.saturating_sub(2))).len())
            .unwrap_or(u16::MAX)
            .clamp(1, maximum.max(1))
            .saturating_add(2)
    }

    /// Reusable questionnaire/search/skill field: label outside plain input rules.
    pub fn render_labeled(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        palette: &Palette,
        label: &str,
        focused: bool,
    ) {
        let [label_area, input] = super::chrome::label_input(area);
        super::chrome::label(frame, label_area, palette, label);
        self.render(frame, input, palette, focused);
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, palette: &Palette, focused: bool) {
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(palette.border_accent))
            .style(Style::default().fg(palette.text).bg(palette.background));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        self.width = usize::from(inner.width.saturating_sub(2)).max(1);
        let rows = self.layout(self.width);
        let (cursor_row, cursor_col) = self.visual_cursor(&rows);
        if cursor_row < self.scroll {
            self.scroll = cursor_row;
        }
        if cursor_row >= self.scroll + usize::from(inner.height) {
            self.scroll = cursor_row + 1 - usize::from(inner.height);
        }
        self.scroll = self
            .scroll
            .min(rows.len().saturating_sub(usize::from(inner.height)));
        let selected = self.area.selection_range();
        let visible = rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(usize::from(inner.height))
            .map(|(index, row)| {
                let mut spans = vec![Span::styled(
                    if index == 0 { "> " } else { "  " },
                    Style::default().fg(palette.accent),
                )];
                for unit in &row.units {
                    let mut style = Style::default()
                        .fg(if unit.chip {
                            palette.accent
                        } else {
                            palette.text
                        })
                        .bg(palette.background);
                    if selected.is_some_and(|(start, end)| {
                        (unit.row, unit.column) >= start
                            && (unit.row, unit.end_column) <= end
                            && unit.end_column > unit.column
                    }) {
                        style = style.bg(palette.selected_bg);
                    }
                    spans.push(Span::styled(unit.text.clone(), style));
                }
                Line::from(spans)
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().bg(palette.background)),
            inner,
        );
        if focused {
            let x = inner
                .x
                .saturating_add(2)
                .saturating_add(u16::try_from(cursor_col).unwrap_or(u16::MAX))
                .min(inner.right().saturating_sub(1));
            let y = inner
                .y
                .saturating_add(
                    u16::try_from(cursor_row.saturating_sub(self.scroll)).unwrap_or(u16::MAX),
                )
                .min(inner.bottom().saturating_sub(1));
            frame.buffer_mut()[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
            frame.set_cursor_position((x, y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn folding_is_atomic_undoable_and_never_parses_literal_chip_text() {
        let mut editor = Editor::default();
        let paste = (0..20).map(|i| format!("line {i}\n")).collect::<String>();
        editor.paste(&paste).unwrap();
        assert_eq!(editor.draft().text(), paste);
        assert_eq!(editor.lines()[0].chars().count(), 1);
        editor.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(editor.draft().is_empty());
        assert!(editor.undo());
        assert_eq!(editor.draft().text(), paste);
        editor.insert_text("[Pasted text #1 +19 lines]").unwrap();
        assert!(
            editor
                .draft()
                .text()
                .ends_with("[Pasted text #1 +19 lines]")
        );
        editor.insert_text("\u{f0000}").unwrap();
        assert!(editor.draft().text().ends_with('\u{f0000}'));
    }

    #[test]
    fn removed_bindings_do_nothing_and_ctrl_z_recovers_atomic_changes() {
        for removed in [
            KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('-'), KeyModifiers::CONTROL),
            KeyEvent::new(
                KeyCode::Char('-'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT),
        ] {
            let mut editor = Editor::default();
            editor.insert_str("你好e\u{301}");
            let cursor = editor.cursor();
            editor.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            assert!(!editor.input(removed));
            assert_eq!(editor.draft().text(), "你好");
            assert!(editor.input(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)));
            assert_eq!(editor.draft().text(), "你好e\u{301}");
            assert_eq!(editor.cursor(), cursor);
            editor.input(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
            assert!(!editor.input(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
            assert_eq!(editor.draft().text(), "你好e\u{301}");
            editor.insert_str("!");
            assert_eq!(editor.draft().text(), "你好!e\u{301}");
        }
    }

    #[test]
    fn grapheme_selection_replacement_completion_and_newline_are_atomic_changes() {
        let mut editor = Editor::default();
        let original = "A👨‍👩‍👧‍👦Z";
        editor.insert_str(original);
        editor.input(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        editor.input(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        editor.input(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        let cursor = editor.cursor();
        editor.paste("你好").unwrap();
        assert_eq!(editor.draft().text(), "A你好Z");
        assert!(editor.undo());
        assert_eq!(editor.draft().text(), original);
        assert_eq!(editor.cursor(), cursor);
        assert!(!editor.area.is_selecting());
        assert!(!editor.input(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert_eq!(editor.draft().text(), original);
        editor.area.select_all();
        editor.insert_newline();
        assert_eq!(editor.draft().text(), "\n");
        assert!(editor.undo());
        assert_eq!(editor.draft().text(), original);
        editor.reset();
        editor.insert_str("/comp");
        editor.complete(5, "/compact").unwrap();
        assert_eq!(editor.draft().text(), "/compact");
        assert!(editor.undo());
        assert_eq!(editor.draft().text(), "/comp");
        assert!(!editor.input(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert_eq!(editor.draft().text(), "/comp");
    }

    #[test]
    fn folded_paste_expansion_and_image_replacement_are_atomically_recoverable() {
        let mut editor = Editor::default();
        let paste = "line 中文\n".repeat(20);
        editor.paste(&paste).unwrap();
        let folded = editor.draft().summary();
        editor.paste(&paste).unwrap();
        assert!(editor.lines().len() > 3);
        assert!(editor.undo());
        assert_eq!(editor.draft().summary(), folded);
        assert_eq!(editor.draft().text(), paste);
        assert!(!editor.input(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert_eq!(editor.draft().summary(), folded);
        assert_eq!(editor.draft().text(), paste);
        editor
            .image(Content::Image {
                data: "controlled-image".into(),
                mime_type: "image/png".into(),
            })
            .unwrap();
        let content = serde_json::to_value(editor.draft().content()).unwrap();
        editor.area.select_all();
        editor.insert_str("replacement [Image #1] \u{f0000}");
        assert!(!editor.draft().has_images());
        assert!(editor.undo());
        assert_eq!(
            serde_json::to_value(editor.draft().content()).unwrap(),
            content
        );
        assert!(!editor.input(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert!(editor.draft().has_images());
    }

    #[test]
    fn restore_history_and_queue_boundaries_keep_content_without_old_undo_leaks() {
        let mut editor = Editor::default();
        editor.paste(&"queued paste\n".repeat(20)).unwrap();
        editor
            .image(Content::Image {
                data: "fixture".into(),
                mime_type: "image/png".into(),
            })
            .unwrap();
        let queued = editor.draft();
        let expected = serde_json::to_value(queued.content()).unwrap();
        editor.remember(queued.clone());
        editor.restore(queued.clone());
        assert!(!editor.undo());
        editor.insert_str(" revision");
        assert!(editor.undo());
        assert_eq!(
            serde_json::to_value(editor.draft().content()).unwrap(),
            expected
        );
        assert!(!editor.input(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        editor.reset();
        editor.insert_str("unsent");
        editor.input(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        editor.input(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            serde_json::to_value(editor.draft().content()).unwrap(),
            expected
        );
        assert!(!editor.undo());
        editor.jump(0, editor.lines()[0].chars().count());
        editor.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(editor.draft().text(), "unsent");
        assert!(!editor.undo());
        editor.restore(queued);
        assert_eq!(
            serde_json::to_value(editor.draft().content()).unwrap(),
            expected
        );
    }

    #[test]
    fn undo_retains_readline_yank_text_and_typed_attachment_references() {
        let mut editor = Editor::default();
        let paste = "kept body\n".repeat(20);
        editor.paste(&paste).unwrap();
        editor.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(editor.draft().is_empty());
        assert!(editor.undo());
        editor.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(editor.draft().text(), paste.repeat(2));
        editor.reset();
        editor
            .image(Content::Image {
                data: "fixture".into(),
                mime_type: "image/png".into(),
            })
            .unwrap();
        editor.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(editor.undo());
        editor.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(editor.draft().content().len(), 2);
        assert!(editor.draft().content().iter().all(|part| matches!(part,
            Content::Image { data, .. } if data == "fixture")));
    }

    #[test]
    fn undo_storage_is_bounded_and_failed_edits_preserve_content() {
        let mut editor = Editor::default();
        for _ in 0..130 {
            editor.insert_str("x");
        }
        let mut undone = 0;
        while editor.undo() {
            undone += 1;
        }
        assert_eq!(undone, 100);
        assert_eq!(editor.draft().text(), "x".repeat(30));
        assert!(
            editor
                .insert_text(&"a".repeat(usize::from(u16::MAX) + 1))
                .is_err()
        );
        assert!(!editor.input(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        editor.reset();
        editor.paste(&"x".repeat(HISTORY_BYTES / 4 + 1)).unwrap();
        for _ in 0..8 {
            editor.insert_str("!");
        }
        assert!(editor.undo.iter().map(Draft::bytes).sum::<usize>() <= HISTORY_BYTES);
        assert_eq!(editor.area.max_histories(), 0);
    }

    #[test]
    fn labeled_inputs_keep_plain_prompt_rules_and_conditional_spacing_on_tiny_sizes() {
        let mut editor = Editor::default();
        editor.insert_str("你好");
        for width in [1, 2, 20] {
            for height in [1, 2, 3, 4, 5, 12] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        editor.render_labeled(
                            frame,
                            frame.area(),
                            &Palette::default(),
                            "Notes",
                            true,
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert!(
                    !buffer
                        .content
                        .iter()
                        .any(|cell| ["│", "┌", "┐", "└", "┘", "╭"].contains(&cell.symbol()))
                );
                if width == 20 && height >= 5 {
                    assert_eq!(buffer[(0, 0)].symbol(), "N");
                    for x in 0..width {
                        assert_eq!(buffer[(x, 1)].symbol(), " ");
                        assert_eq!(buffer[(x, 2)].symbol(), "─");
                        assert_eq!(buffer[(x, height - 1)].symbol(), "─");
                    }
                    assert_eq!(buffer[(0, 3)].symbol(), ">");
                }
                assert_eq!(editor.draft().text(), "你好");
            }
        }
    }

    #[test]
    fn transactions_bound_repeated_yanks_combined_lines_and_completion() {
        let mut editor = Editor::default();
        let body = "x".repeat(MAX_BYTES / 2 + 1);
        editor.paste(&body).unwrap();
        editor.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(editor.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)));
        assert!(!editor.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)));
        assert_eq!(editor.draft().text(), body);
        assert!(editor.draft().bytes() <= MAX_BYTES);
        editor.reset();
        let line = "x".repeat(usize::from(u16::MAX) - 1);
        editor.insert_text(&line).unwrap();
        let before = editor.draft();
        assert!(editor.complete(1, "longer").is_err());
        assert_eq!(editor.draft().text(), line);
        assert_eq!(editor.cursor(), before.cursor);
        assert!(editor.insert_text("too long").is_err());
        assert_eq!(editor.draft().text(), line);
        editor.reset();
        editor
            .insert_text(&"\n".repeat(usize::from(u16::MAX) - 1))
            .unwrap();
        assert!(!editor.insert_newline());
        assert_eq!(editor.lines().len(), usize::from(u16::MAX));
    }

    #[test]
    fn ctrl_u_uses_reference_readline_deletion_not_textarea_undo() {
        let mut editor = Editor::default();
        editor.insert_str("first\n\nsecond");
        for _ in 0..6 {
            editor.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        }
        assert!(editor.draft().is_empty());
        editor.insert_str("replacement");
        assert_eq!(editor.draft().text(), "replacement");
    }

    #[test]
    fn requested_deletion_and_newline_bindings_share_grapheme_safe_behavior() {
        let mut editor = Editor::default();
        editor.insert_str("kept\nprefix👨‍👩‍👧‍👦suffix");
        editor.input(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        for _ in 0..6 {
            editor.input(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        editor.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.draft().text(), "kept\n👨‍👩‍👧‍👦suffix");
        editor.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(editor.draft().text(), "kept\nsuffix");
        editor.input(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(editor.draft().text(), "kept\n");
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            assert!(editor.input(key));
        }
        assert_eq!(editor.draft().text(), "kept\n\n\n");
    }

    #[test]
    fn repeated_paste_expands_without_duplication_and_failures_keep_the_body() {
        let mut editor = Editor::default();
        let paste = "line\n".repeat(20);
        editor.paste(&paste).unwrap();
        editor.paste(&paste).unwrap();
        assert_eq!(editor.draft().text(), paste);
        assert!(editor.lines().len() > 3);
        editor.reset();
        let huge = "x\n".repeat(usize::from(u16::MAX) + 1);
        editor.paste(&huge).unwrap();
        assert!(editor.paste(&huge).is_err());
        assert_eq!(editor.draft().text(), huge);
    }

    #[test]
    fn image_chips_and_full_pastes_survive_draft_restoration() {
        let mut editor = Editor::default();
        editor.insert_str("before ");
        editor
            .image(Content::Image {
                data: "fixture".into(),
                mime_type: "image/png".into(),
            })
            .unwrap();
        editor.insert_str(" after");
        let draft = editor.draft();
        let mut other = Editor::default();
        other.restore(draft.clone());
        assert_eq!(other.draft().content().len(), 3);
        assert!(
            matches!(&other.draft().content()[1], Content::Image { data, .. } if data == "fixture")
        );
        assert_eq!(draft.summary(), "before [Image #1] after");
    }

    #[test]
    fn visual_wrapping_resize_unicode_and_native_rules_keep_cursor_visible() {
        let mut editor = Editor::default();
        editor.insert_str(format!("BEGIN {}END", "word 中文🙂e\u{301} ".repeat(5)));
        for width in [1, 2, 20, 40, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
            terminal
                .draw(|f| editor.render(f, f.area(), &Palette::default(), true))
                .unwrap();
            let p = terminal.get_cursor_position().unwrap();
            assert!(p.x < width && p.y < 18);
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(0, 0)].symbol(), "─");
            assert!(
                !buffer
                    .content
                    .iter()
                    .any(|c| ["╭", "╮", "│"].contains(&c.symbol()))
            );
        }
        assert!(!editor.draft().text().contains('\n'));
        assert!(editor.height(40, 10) > 3);
    }

    #[test]
    fn history_restores_unsent_draft_and_grapheme_deletion_is_atomic() {
        let mut editor = Editor::default();
        editor.insert_str("old prompt");
        let old = editor.draft();
        editor.remember(old);
        editor.reset();
        editor.insert_str("draft");
        editor.input(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        editor.input(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(editor.draft().text(), "old prompt");
        editor.jump(0, 10);
        editor.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(editor.draft().text(), "draft");
        editor.reset();
        editor.insert_str("👨‍👩‍👧‍👦");
        editor.input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(editor.draft().is_empty());
    }
}
