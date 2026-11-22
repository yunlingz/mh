//! Theme-colored transient side-answer overlay with independent scrolling.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::text::terminal_text;
use crate::theme::Palette;
use crate::ui::{chrome, markdown};

#[derive(Default)]
pub struct Overlay {
    pub visible: bool,
    pub question: String,
    pub answer: String,
    pub waiting: bool,
    pub scroll: u16,
}

impl Overlay {
    pub fn begin(&mut self, question: String) {
        self.visible = true;
        self.question = question;
        self.answer.clear();
        self.waiting = true;
        self.scroll = 0;
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        palette: &Palette,
        activity: Option<&str>,
    ) {
        let inner = chrome::modal(frame, area, palette, "btw");
        if inner.is_empty() {
            return;
        }
        let mut lines = vec![
            Line::styled(
                terminal_text(&self.question, false),
                Style::default().fg(palette.accent),
            ),
            Line::default(),
        ];
        if self.answer.is_empty() && self.waiting {
            lines.push(Line::styled(
                activity.unwrap_or("Answering...").to_owned(),
                Style::default().fg(palette.muted),
            ));
        } else {
            lines.extend(markdown::render(&self.answer, palette));
        }
        let actions = "Esc close · Ctrl+C cancel side request · x clear side history";
        let maximum_actions = inner.height.saturating_sub(4).clamp(1, 3);
        let [body, footer] = chrome::body_actions(
            inner,
            chrome::actions_height(actions, inner.width, maximum_actions),
        );
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        self.scroll = self.scroll.min(
            u16::try_from(paragraph.line_count(body.width))
                .unwrap_or(u16::MAX)
                .saturating_sub(body.height),
        );
        frame.render_widget(paragraph.scroll((self.scroll, 0)), body);
        chrome::actions(frame, footer, palette, actions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn side_overlay_is_opaque_plain_and_keeps_content_through_tiny_resizes() {
        let palette = Palette::default();
        let mut overlay = Overlay::default();
        overlay.begin("Side question 中文".into());
        overlay.answer = "# Kept answer\n\n**Unicode** 👨‍👩‍👧‍👦".into();
        overlay.waiting = false;
        let answer = overlay.answer.clone();
        for (width, height) in [(1, 1), (2, 3), (8, 6), (40, 12), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("▒".repeat(2000)).wrap(Wrap { trim: false }),
                        frame.area(),
                    );
                    overlay.render(frame, frame.area(), &palette, None);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(0, 0)].symbol(), "┌");
            assert!(
                !buffer
                    .content
                    .iter()
                    .any(|cell| ["▒", "╭", "╮", "╰", "╯"].contains(&cell.symbol()))
            );
            assert_eq!(overlay.answer, answer);
        }
    }

    #[test]
    fn narrow_side_overlay_keeps_retry_countdown_and_sticky_actions_separated() {
        let palette = Palette::default();
        let mut overlay = Overlay::default();
        overlay.begin("Side question".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                overlay.render(
                    frame,
                    frame.area(),
                    &palette,
                    Some("Retrying (1/3) in 2s... · 1s"),
                )
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Retrying (1/3) in 2s"));
        assert!(text.contains("Ctrl+C cancel side request"));
        let inner = Rect::new(1, 1, 38, 10);
        let [body, footer] = chrome::body_actions(
            inner,
            chrome::actions_height(
                "Esc close · Ctrl+C cancel side request · x clear side history",
                inner.width,
                3,
            ),
        );
        assert_eq!(footer.y - body.bottom(), 1);
        for x in inner.x..inner.right() {
            assert_eq!(
                terminal.backend().buffer()[(x, body.bottom())].symbol(),
                " "
            );
        }
        assert!(overlay.answer.is_empty());
    }

    #[test]
    fn waiting_overlay_keeps_retry_status_visible_inside_the_modal() {
        let mut overlay = Overlay::default();
        overlay.begin("Controlled side question".into());
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal
            .draw(|frame| {
                overlay.render(
                    frame,
                    frame.area(),
                    &Palette::default(),
                    Some("Retrying (1/3) in 2s... · 1s · Ctrl+C cancel"),
                );
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Retrying (1/3) in 2s"));
        assert!(text.contains("Ctrl+C cancel"));
        assert!(!text.contains("Answering..."));
        assert!(overlay.answer.is_empty());
    }
}
