//! Grapheme-safe transcript selection anchored to logical text, not screen rows.
//! Only the hovered/visible lines are mapped; history is never cloned in full.

use std::collections::VecDeque;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

use super::viewport::Viewport;
use super::wrapping::{self, Row};
use crate::theme::Palette;

const COPY_LIMIT: usize = 2 * 1024 * 1024;
const MAP_LIMIT: usize = 2 * 1024 * 1024;
const CACHE_LINES: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Point {
    pub line: usize,
    pub byte: usize,
}

pub struct Document<'a> {
    pub saved: &'a [Line<'static>],
    pub live: &'a [Line<'static>],
    pub saved_rows: &'a Viewport,
    pub live_rows: &'a Viewport,
    pub width: u16,
}

impl Document<'_> {
    fn len(&self) -> usize {
        self.saved.len() + self.live.len()
    }
    fn line(&self, index: usize) -> Option<&Line<'static>> {
        if index < self.saved.len() {
            self.saved.get(index)
        } else {
            self.live.get(index - self.saved.len())
        }
    }
    fn row(&self, row: usize) -> Option<(usize, usize)> {
        if row < self.saved_rows.height() {
            self.saved_rows.locate_row(row)
        } else {
            self.live_rows
                .locate_row(row - self.saved_rows.height())
                .map(|(line, within)| (self.saved.len() + line, within))
        }
    }
    fn start_row(&self, line: usize) -> usize {
        if line < self.saved.len() {
            self.saved_rows.row_of_line(line)
        } else {
            self.saved_rows.height() + self.live_rows.row_of_line(line - self.saved.len())
        }
    }
    pub fn height(&self) -> usize {
        self.saved_rows.height() + self.live_rows.height()
    }
}

#[derive(Clone, Copy)]
pub enum Movement {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Start,
    Finish,
}

struct Cached {
    index: usize,
    width: u16,
    source: Line<'static>,
    rows: Vec<Row>,
    bytes: usize,
}

#[derive(Default)]
pub struct Selection {
    anchor: Option<Point>,
    caret: Option<Point>,
    dragging: bool,
    cache: VecDeque<Cached>,
    cache_bytes: usize,
}

impl Selection {
    pub fn clear(&mut self) {
        self.anchor = None;
        self.caret = None;
        self.dragging = false;
    }
    pub fn selected(&self) -> bool {
        self.anchor.zip(self.caret).is_some_and(|(a, b)| a != b)
    }
    pub fn focused(&self) -> bool {
        self.anchor.is_some()
    }
    pub fn dragging(&self) -> bool {
        self.dragging
    }
    pub fn finish_drag(&mut self) {
        self.dragging = false;
    }
    pub fn retain_saved(&mut self, saved: usize) {
        if self
            .anchor
            .into_iter()
            .chain(self.caret)
            .any(|point| point.line >= saved)
        {
            self.clear();
        }
    }
    fn bounds(&self) -> Option<(Point, Point)> {
        let (a, b) = self.anchor.zip(self.caret)?;
        (a != b).then_some((a.min(b), a.max(b)))
    }
    fn layout<'a>(&'a mut self, doc: &Document<'_>, index: usize) -> Option<&'a [Row]> {
        let source = doc.line(index)?;
        if let Some(at) = self.cache.iter().position(|cached| {
            cached.index == index && cached.width == doc.width && &cached.source == source
        }) {
            let cached = self.cache.remove(at)?;
            self.cache.push_back(cached);
            return Some(&self.cache.back()?.rows);
        }
        let bytes = source
            .spans
            .iter()
            .map(|span| span.content.len())
            .sum::<usize>();
        if bytes > MAP_LIMIT {
            return None;
        }
        while !self.cache.is_empty()
            && (self.cache.len() >= CACHE_LINES || self.cache_bytes + bytes > MAP_LIMIT)
        {
            self.cache_bytes -= self.cache.pop_front()?.bytes;
        }
        self.cache_bytes += bytes;
        self.cache.push_back(Cached {
            index,
            width: doc.width,
            source: source.clone(),
            rows: wrapping::rows(source, doc.width),
            bytes,
        });
        Some(&self.cache.back()?.rows)
    }
    fn point(
        &mut self,
        doc: &Document<'_>,
        row: usize,
        column: usize,
        after: bool,
    ) -> Option<Point> {
        let (line, within) = doc.row(row.min(doc.height().saturating_sub(1)))?;
        let row = self.layout(doc, line)?.get(within)?;
        let glyph = row.glyphs.iter().find(|glyph| glyph.columns.end > column);
        Some(Point {
            line,
            byte: glyph.map_or(row.end, |glyph| {
                if after {
                    glyph.bytes.end
                } else {
                    glyph.bytes.start
                }
            }),
        })
    }
    pub fn begin(&mut self, doc: &Document<'_>, row: usize, column: usize) -> bool {
        let Some(point) = self.point(doc, row, column, false) else {
            return false;
        };
        self.anchor = Some(point);
        self.caret = Some(point);
        self.dragging = true;
        true
    }
    pub fn drag(&mut self, doc: &Document<'_>, row: usize, column: usize) {
        if !self.dragging {
            return;
        }
        let Some(before) = self.point(doc, row, column, false) else {
            return;
        };
        self.caret = if self.anchor.is_some_and(|a| a <= before) {
            self.point(doc, row, column, true)
        } else {
            Some(before)
        };
    }
    fn position(&mut self, doc: &Document<'_>, point: Point) -> Option<(usize, usize)> {
        let base = doc.start_row(point.line);
        let rows = self.layout(doc, point.line)?;
        for (index, row) in rows.iter().enumerate() {
            if let Some(glyph) = row.glyphs.iter().find(|glyph| glyph.bytes.end > point.byte) {
                return Some((base + index, glyph.columns.start));
            }
            if point.byte <= row.end {
                return Some((
                    base + index,
                    row.glyphs.last().map_or(0, |glyph| glyph.columns.end),
                ));
            }
        }
        rows.last().map(|row| {
            (
                base + rows.len() - 1,
                row.glyphs.last().map_or(0, |glyph| glyph.columns.end),
            )
        })
    }
    pub fn extend(
        &mut self,
        doc: &Document<'_>,
        movement: Movement,
        first_row: usize,
    ) -> Option<usize> {
        if self.anchor.is_none() {
            self.begin(doc, first_row, 0);
        }
        self.dragging = false;
        let point = self.caret?;
        let text = doc.line(point.line)?.to_string();
        let (row, column) = self.position(doc, point)?;
        let next = match movement {
            Movement::Start => Point::default(),
            Movement::Finish => {
                let line = doc.len().checked_sub(1)?;
                Point {
                    line,
                    byte: doc
                        .line(line)?
                        .spans
                        .iter()
                        .map(|span| span.content.len())
                        .sum(),
                }
            }
            Movement::Left => {
                if let Some((byte, _)) = text
                    .grapheme_indices(true)
                    .take_while(|(byte, _)| *byte < point.byte)
                    .last()
                {
                    Point { byte, ..point }
                } else if point.line > 0 {
                    Point {
                        line: point.line - 1,
                        byte: doc
                            .line(point.line - 1)?
                            .spans
                            .iter()
                            .map(|span| span.content.len())
                            .sum(),
                    }
                } else {
                    point
                }
            }
            Movement::Right => {
                if let Some((byte, glyph)) = text
                    .grapheme_indices(true)
                    .find(|(byte, _)| *byte >= point.byte)
                {
                    Point {
                        byte: byte + glyph.len(),
                        ..point
                    }
                } else if point.line + 1 < doc.len() {
                    Point {
                        line: point.line + 1,
                        byte: 0,
                    }
                } else {
                    point
                }
            }
            Movement::Up => self.point(doc, row.saturating_sub(1), column, false)?,
            Movement::Down => self.point(doc, row.saturating_add(1), column, false)?,
            Movement::Home => self.point(doc, row, 0, false)?,
            Movement::End => self.point(doc, row, usize::from(doc.width), false)?,
        };
        self.caret = Some(next);
        self.position(doc, next).map(|(row, _)| row)
    }
    pub fn copy(&self, doc: &Document<'_>) -> Result<String, &'static str> {
        let (start, end) = self.bounds().ok_or("Select transcript text first")?;
        if end.line >= doc.len() {
            return Err("The selected transcript changed; select it again");
        }
        let mut output = String::new();
        for index in start.line..=end.line {
            let text = doc
                .line(index)
                .ok_or("The selected transcript changed")?
                .to_string();
            let lo = if index == start.line { start.byte } else { 0 };
            let hi = if index == end.line {
                end.byte
            } else {
                text.len()
            };
            let selected = text
                .get(lo..hi)
                .ok_or("The selected transcript changed; select it again")?;
            let separator = usize::from(index != start.line);
            if output
                .len()
                .saturating_add(selected.len())
                .saturating_add(separator)
                > COPY_LIMIT
            {
                return Err("Selection exceeds the 2 MiB clipboard limit");
            }
            if separator != 0 {
                output.push('\n');
            }
            output.push_str(selected);
        }
        Ok(output)
    }
    pub fn highlight(
        &mut self,
        doc: &Document<'_>,
        buffer: &mut Buffer,
        area: Rect,
        scroll: usize,
        palette: &Palette,
    ) {
        let Some((start, end)) = self.bounds() else {
            return;
        };
        for screen_row in 0..area.height {
            let Some((line, within)) = doc.row(scroll + usize::from(screen_row)) else {
                break;
            };
            if line < start.line || line > end.line {
                continue;
            }
            let Some(rows) = self.layout(doc, line) else {
                continue;
            };
            let Some(row) = rows.get(within) else {
                continue;
            };
            for glyph in &row.glyphs {
                let point = Point {
                    line,
                    byte: glyph.bytes.start,
                };
                if point < start || point >= end {
                    continue;
                }
                for column in glyph
                    .columns
                    .clone()
                    .filter(|column| *column < usize::from(area.width))
                {
                    let cell = &mut buffer[(area.x + column as u16, area.y + screen_row)];
                    cell.bg = palette.selected_bg;
                    cell.fg = palette.text;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_preserves_soft_wrapped_source_and_resize_graphemes() {
        let lines = vec![
            Line::raw("hello 世界 e\u{301} 🙂 end"),
            Line::raw("second line"),
        ];
        let mut saved = Viewport::default();
        let live = Viewport::default();
        saved.prepare(&lines, 8);
        let mut selection = Selection::default();
        let doc = Document {
            saved: &lines,
            live: &[],
            saved_rows: &saved,
            live_rows: &live,
            width: 8,
        };
        selection.begin(&doc, 0, 0);
        selection.extend(&doc, Movement::Finish, 0);
        assert_eq!(
            selection.copy(&doc).unwrap(),
            "hello 世界 e\u{301} 🙂 end\nsecond line"
        );
        saved.prepare(&lines, 20);
        let doc = Document {
            saved: &lines,
            live: &[],
            saved_rows: &saved,
            live_rows: &live,
            width: 20,
        };
        assert_eq!(
            selection.copy(&doc).unwrap(),
            "hello 世界 e\u{301} 🙂 end\nsecond line"
        );
        selection.clear();
        selection.begin(&doc, 0, 6);
        selection.drag(&doc, 0, 7);
        assert_eq!(selection.copy(&doc).unwrap(), "世");
    }
    #[test]
    fn history_selection_is_bounded_without_global_u16_rows() {
        let lines = (0..100_000)
            .map(|i| Line::raw(format!("row {i}")))
            .collect::<Vec<_>>();
        let mut saved = Viewport::default();
        let live = Viewport::default();
        saved.prepare(&lines, 40);
        let doc = Document {
            saved: &lines,
            live: &[],
            saved_rows: &saved,
            live_rows: &live,
            width: 40,
        };
        let mut selection = Selection::default();
        selection.begin(&doc, 99_998, 0);
        selection.drag(&doc, 99_999, 8);
        assert_eq!(selection.copy(&doc).unwrap(), "row 99998\nrow 99999");
        assert!(selection.cache.len() <= 2);
    }
}
