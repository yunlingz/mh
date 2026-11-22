//! Todo-derived status decoration for mh-owned task/tool/queue/activity UI.
//! Never use these mappings to rewrite user or model content.

use ratatui::style::{Color, Style};
use ratatui::text::Span;

use crate::features::todo::state::Status as TodoStatus;
use crate::theme::Palette;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Deleted,
    Error,
    Cancelled,
}

impl From<TodoStatus> for Status {
    fn from(status: TodoStatus) -> Self {
        match status {
            TodoStatus::Pending => Self::Pending,
            TodoStatus::InProgress => Self::InProgress,
            TodoStatus::Completed => Self::Completed,
            TodoStatus::Deleted => Self::Deleted,
        }
    }
}

impl Status {
    /// One conventional Unicode cell, without a font-specific icon dependency.
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::InProgress => "◐",
            Self::Completed => "✓",
            Self::Deleted | Self::Cancelled => "×",
            Self::Error => "!",
        }
    }

    pub fn color(self, palette: &Palette) -> Color {
        match self {
            Self::Pending | Self::Deleted | Self::Cancelled => palette.dim,
            Self::InProgress => palette.warning,
            Self::Completed => palette.success,
            Self::Error => palette.error,
        }
    }

    pub fn style(self, palette: &Palette) -> Style {
        Style::default().fg(self.color(palette))
    }

    /// A marker and its trailing space, for existing compact status rows.
    pub fn span(self, palette: &Palette) -> Span<'static> {
        Span::styled(format!("{} ", self.marker()), self.style(palette))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn semantic_statuses_keep_todo_markers_colors_and_portable_cell_widths() {
        let palette = Palette::default();
        for (status, expected, color) in [
            (TodoStatus::Pending, Status::Pending, palette.dim),
            (TodoStatus::InProgress, Status::InProgress, palette.warning),
            (TodoStatus::Completed, Status::Completed, palette.success),
            (TodoStatus::Deleted, Status::Deleted, palette.dim),
        ] {
            let mapped = Status::from(status);
            assert_eq!(mapped, expected);
            assert_eq!(mapped.color(&palette), color);
        }
        for status in [
            Status::Pending,
            Status::InProgress,
            Status::Completed,
            Status::Deleted,
            Status::Error,
            Status::Cancelled,
        ] {
            assert_eq!(UnicodeWidthStr::width(status.marker()), 1);
            assert_eq!(status.span(&palette).width(), 2);
        }
        assert_eq!(Status::Error.color(&palette), palette.error);
        assert_eq!(Status::Cancelled.color(&palette), palette.dim);
    }
}
