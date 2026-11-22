//! Cached native wrapping metrics; drawing clones only the visible history.

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Default)]
pub struct Viewport {
    width: u16,
    starts: Vec<usize>,
}

impl Viewport {
    pub fn invalidate(&mut self) {
        self.width = 0;
    }

    /// Callers invalidate after replacing their immutable finalized lines.
    pub fn prepare(&mut self, lines: &[Line<'static>], width: u16) {
        if self.width == width && self.starts.len() == lines.len() + 1 {
            return;
        }
        self.width = width;
        self.starts.clear();
        self.starts.push(0);
        for line in lines {
            let height = Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(width)
                .max(1);
            self.starts.push(self.height().saturating_add(height));
        }
    }

    pub fn row_of_line(&self, line: usize) -> usize {
        self.starts
            .get(line)
            .copied()
            .unwrap_or_else(|| self.height())
    }

    pub fn locate_row(&self, row: usize) -> Option<(usize, usize)> {
        if self.starts.len() < 2 || row >= self.height() {
            return None;
        }
        let line = self
            .starts
            .partition_point(|start| *start <= row)
            .saturating_sub(1);
        Some((line, row - self.starts[line]))
    }

    pub fn height(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    pub fn visible(
        &self,
        lines: &[Line<'static>],
        scroll: usize,
        height: u16,
    ) -> (Vec<Line<'static>>, u16) {
        if lines.is_empty() || height == 0 || scroll >= self.height() {
            return (Vec::new(), 0);
        }
        let first = self
            .starts
            .partition_point(|start| *start <= scroll)
            .saturating_sub(1)
            .min(lines.len() - 1);
        let end = scroll.saturating_add(usize::from(height));
        let last = self
            .starts
            .partition_point(|start| *start < end)
            .min(lines.len());
        let within = u16::try_from(scroll.saturating_sub(self.starts[first])).unwrap_or(u16::MAX);
        (lines[first..last.max(first + 1)].to_vec(), within)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_history_is_bounded_even_after_tens_of_thousands_of_rows() {
        let lines = (0..100_000)
            .map(|index| Line::raw(format!("row {index}")))
            .collect::<Vec<_>>();
        let mut viewport = Viewport::default();
        viewport.prepare(&lines, 80);
        assert_eq!(viewport.height(), 100_000);
        let (visible, offset) = viewport.visible(&lines, 99_990, 8);
        assert_eq!(visible.len(), 8);
        assert_eq!(visible[0].to_string(), "row 99990");
        assert_eq!(offset, 0);
        viewport.prepare(&lines, 80);
        assert_eq!(viewport.height(), 100_000);
    }

    #[test]
    fn native_wrapping_and_intra_line_scroll_match_the_drawn_viewport() {
        let lines = vec![Line::raw("x".repeat(100)), Line::raw("anchor")];
        let mut viewport = Viewport::default();
        viewport.prepare(&lines, 20);
        assert_eq!(viewport.height(), 6);
        let (visible, offset) = viewport.visible(&lines, 3, 3);
        assert_eq!(visible.len(), 2);
        assert_eq!(offset, 3);
        viewport.invalidate();
        viewport.prepare(&lines, 40);
        assert_eq!(viewport.height(), 4);
    }
}
