//! Source offsets for native Paragraph wrapping, used only for selection.
//!
//! Adapted from Ratatui 0.29.0 widgets/reflow.rs WordWrapper (trim=false).
//! Copyright (c) 2016-2022 Florian Dehau
//! Copyright (c) 2023-2024 The Ratatui Developers
//! MIT; the retained copyright and permission notice is in LICENSE.
//! Drawing itself remains Ratatui's native Paragraph, not this offset mapper.

use std::collections::VecDeque;
use std::ops::Range;

use ratatui::layout::Alignment;
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct Glyph {
    pub bytes: Range<usize>,
    pub columns: Range<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub glyphs: Vec<Glyph>,
    pub end: usize,
}

#[derive(Clone)]
struct Unit {
    bytes: Range<usize>,
    width: usize,
    whitespace: bool,
}

pub fn rows(line: &Line<'_>, width: u16) -> Vec<Row> {
    if width == 0 {
        return Vec::new();
    }
    let width = usize::from(width);
    let mut units = Vec::new();
    let mut offset = 0;
    for span in &line.spans {
        for (at, symbol) in span.content.grapheme_indices(true) {
            if symbol == "\n" {
                continue;
            }
            units.push(Unit {
                bytes: offset + at..offset + at + symbol.len(),
                width: symbol.width(),
                whitespace: symbol == "\u{200b}"
                    || symbol != "\u{a0}" && symbol.chars().all(char::is_whitespace),
            });
        }
        offset += span.content.len();
    }
    let mut wrapped: Vec<Vec<Unit>> = Vec::new();
    let mut pending = Vec::new();
    let mut word = Vec::new();
    let mut whitespace: VecDeque<Unit> = VecDeque::new();
    let (mut line_width, mut word_width, mut whitespace_width) = (0, 0, 0);
    let mut non_whitespace_previous = false;
    for unit in units {
        if unit.width > width {
            continue;
        }
        let word_found = non_whitespace_previous && unit.whitespace;
        let overflow = pending.is_empty() && word_width + whitespace_width + unit.width > width;
        if word_found || overflow {
            pending.extend(whitespace.drain(..));
            line_width += whitespace_width;
            pending.append(&mut word);
            line_width += word_width;
            whitespace_width = 0;
            word_width = 0;
        }
        if line_width >= width
            || unit.width > 0 && line_width + whitespace_width + word_width >= width
        {
            let mut remaining = width.saturating_sub(line_width);
            wrapped.push(std::mem::take(&mut pending));
            line_width = 0;
            while let Some(front) = whitespace.front() {
                if front.width > remaining {
                    break;
                }
                remaining -= front.width;
                whitespace_width -= front.width;
                whitespace.pop_front();
            }
            if unit.whitespace && whitespace.is_empty() {
                continue;
            }
        }
        non_whitespace_previous = !unit.whitespace;
        if unit.whitespace {
            whitespace_width += unit.width;
            whitespace.push_back(unit);
        } else {
            word_width += unit.width;
            word.push(unit);
        }
    }
    if pending.is_empty() && word.is_empty() && !whitespace.is_empty() {
        wrapped.push(Vec::new());
    }
    pending.extend(whitespace);
    pending.extend(word);
    if !pending.is_empty() {
        wrapped.push(pending);
    }
    if wrapped.is_empty() {
        wrapped.push(Vec::new());
    }
    let mut previous_end = 0;
    wrapped
        .into_iter()
        .map(|units| {
            let used = units.iter().map(|unit| unit.width).sum::<usize>();
            let mut column = match line.alignment.unwrap_or(Alignment::Left) {
                Alignment::Left => 0,
                Alignment::Center => (width / 2).saturating_sub(used / 2),
                Alignment::Right => width.saturating_sub(used),
            };
            let mut row = Row::default();
            for unit in units {
                row.glyphs.push(Glyph {
                    bytes: unit.bytes.clone(),
                    columns: column..column + unit.width,
                });
                column += unit.width;
                previous_end = unit.bytes.end;
            }
            row.end = previous_end;
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Span;
    use ratatui::widgets::{Paragraph, Widget, Wrap};

    #[test]
    fn mapped_cells_match_native_word_wrapping_and_graphemes() {
        for text in [
            "",
            " ",
            "   ",
            "hello world",
            "aaaa aaaa aaaa",
            "a\u{a0}b c",
            "a\u{200b}b",
            "你好吗🙂 hello e\u{301}",
            "longwordwithoutspaces",
            "  indentation and spaces   ",
        ] {
            for width in 1..=18 {
                for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
                    let line = Line::from(text).alignment(alignment);
                    let mapped = rows(&line, width);
                    let height = Paragraph::new(line.clone())
                        .wrap(Wrap { trim: false })
                        .line_count(width)
                        .max(1);
                    assert_eq!(mapped.len(), height, "{text:?}, width {width}");
                    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height as u16));
                    Paragraph::new(line)
                        .wrap(Wrap { trim: false })
                        .render(buffer.area, &mut buffer);
                    for (y, row) in mapped.iter().enumerate() {
                        for glyph in &row.glyphs {
                            if glyph.columns.is_empty() {
                                continue;
                            }
                            assert_eq!(
                                buffer[(glyph.columns.start as u16, y as u16)].symbol(),
                                &text[glyph.bytes.clone()],
                                "{text:?}, width {width}"
                            );
                        }
                    }
                }
            }
        }
        let split = Line::from(vec![Span::raw("a"), Span::raw("\u{301} b")]);
        assert!(!rows(&split, 5).is_empty());
    }
}
