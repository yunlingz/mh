//! Shared native dialog chrome. Inputs use the separate rule-only Editor.
//!
//! Pi's retained heading/body/actions hierarchy and modal-first controls stay
//! intact; opaque Plain Ratatui frames are the requested native adaptation.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::text::terminal_text;
use crate::theme::Palette;

/// A rectangular, opaque section frame, never an input frame.
pub fn section(palette: &Palette, title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Plain)
        .title(format!(" {} ", terminal_text(title, false)))
        .border_style(Style::default().fg(palette.border))
        .style(Style::default().fg(palette.text).bg(palette.background))
}

/// Clear everything underneath a modal, including blank cells and old styles.
/// Returns the content rectangle clipped to the actual terminal viewport.
pub fn modal(frame: &mut Frame<'_>, area: Rect, palette: &Palette, title: &str) -> Rect {
    let area = area.intersection(frame.area());
    frame.render_widget(Clear, area);
    let block = section(palette, title).border_style(Style::default().fg(palette.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Place an editable field's label outside its top/bottom rules.
pub fn label(frame: &mut Frame<'_>, area: Rect, palette: &Palette, text: &str) {
    frame.render_widget(
        Paragraph::new(terminal_text(text, false))
            .style(Style::default().fg(palette.accent).bg(palette.background)),
        area,
    );
}

/// Label, optional blank separator, then the shared rule-only input.
/// Collapse the separator first when a terminal cannot fit all five rows.
pub fn label_input(area: Rect) -> [Rect; 2] {
    let label_height = area.height.min(1);
    let separator = u16::from(area.height >= 5);
    let label = Rect {
        height: label_height,
        ..area
    };
    let input = Rect {
        y: area.y + label_height + separator,
        height: area.height - label_height - separator,
        ..area
    };
    [label, input]
}

/// Reserve sticky actions and one separating blank row where space permits.
/// Neither region can extend outside even a tiny content rectangle.
pub fn body_actions(area: Rect, action_height: u16) -> [Rect; 2] {
    let height = action_height.min(area.height);
    let separator = u16::from(height > 0 && area.height > height.saturating_add(1));
    let body = Rect {
        height: area.height - height - separator,
        ..area
    };
    let actions = Rect {
        y: area.bottom() - height,
        height,
        ..area
    };
    [body, actions]
}

/// Bound action wrapping so narrow dialogs keep both hints and content usable.
pub fn actions_height(text: &str, width: u16, maximum: u16) -> u16 {
    if width == 0 || maximum == 0 {
        return 0;
    }
    u16::try_from(
        Paragraph::new(terminal_text(text, false))
            .wrap(Wrap { trim: false })
            .line_count(width),
    )
    .unwrap_or(u16::MAX)
    .clamp(1, maximum)
}

pub fn actions(frame: &mut Frame<'_>, area: Rect, palette: &Palette, text: &str) {
    frame.render_widget(
        Paragraph::new(terminal_text(text, false))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(palette.dim).bg(palette.background)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn modal_is_rectangular_opaque_and_clipped_on_tiny_viewports() {
        let palette = Palette::default();
        for (width, height) in [(1, 1), (2, 2), (6, 3), (40, 12)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("Z".repeat(1000)).wrap(Wrap { trim: false }),
                        frame.area(),
                    );
                    let inner = modal(frame, Rect::new(0, 0, 80, 30), &palette, "Dialog");
                    let [body, footer] = body_actions(inner, 2);
                    assert!(body.bottom() <= inner.bottom());
                    assert!(footer.bottom() <= inner.bottom());
                    assert!(inner.right() <= width);
                    assert!(inner.bottom() <= height);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for cell in &buffer.content {
                assert_ne!(cell.symbol(), "Z");
                assert_eq!(cell.bg, palette.background);
                assert!(!["╭", "╮", "╰", "╯"].contains(&cell.symbol()));
            }
            assert_eq!(buffer[(0, 0)].symbol(), "┌");
        }
    }
}
