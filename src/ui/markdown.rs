//! Small Markdown renderer shared by chat, side answers, and question previews.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::text::terminal_text;
use crate::theme::Palette;

pub fn render(text: &str, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut styles = vec![Style::default().fg(palette.text)];
    let mut code = false;
    let mut list = Vec::new();
    fn flush(current: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
        if !current.is_empty() {
            lines.push(Line::from(std::mem::take(current)));
        }
    }
    for event in Parser::new_ext(
        text,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS,
    ) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                flush(&mut current, &mut lines);
                styles.push(
                    Style::default()
                        .fg(palette.md_heading)
                        .add_modifier(Modifier::BOLD),
                );
            }
            Event::Start(Tag::Strong) => styles.push(
                styles
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .add_modifier(Modifier::BOLD),
            ),
            Event::Start(Tag::Emphasis) => styles.push(
                styles
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .add_modifier(Modifier::ITALIC),
            ),
            Event::Start(Tag::Strikethrough) => styles.push(
                styles
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
            Event::Start(Tag::Link { .. }) => styles.push(
                Style::default()
                    .fg(palette.md_heading)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            Event::Start(Tag::CodeBlock(_)) => {
                flush(&mut current, &mut lines);
                code = true;
                styles.push(Style::default().fg(palette.md_code).bg(palette.selected_bg));
            }
            Event::Start(Tag::List(start)) => list.push(start),
            Event::Start(Tag::Item) => {
                flush(&mut current, &mut lines);
                let prefix = if let Some(Some(number)) = list.last_mut() {
                    let prefix = format!("{number}. ");
                    *number += 1;
                    prefix
                } else {
                    "- ".into()
                };
                current.push(Span::styled(prefix, Style::default().fg(palette.accent)));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut current, &mut lines);
                current.push(Span::styled("> ", Style::default().fg(palette.dim)));
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut current, &mut lines);
                styles.pop();
            }
            Event::End(
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link,
            ) => {
                styles.pop();
            }
            Event::End(TagEnd::CodeBlock) => {
                flush(&mut current, &mut lines);
                code = false;
                styles.pop();
            }
            Event::End(TagEnd::List(_)) => {
                list.pop();
                flush(&mut current, &mut lines);
            }
            Event::End(TagEnd::Paragraph) => {
                flush(&mut current, &mut lines);
                if list.is_empty() && lines.last().is_some_and(|line| !line.spans.is_empty()) {
                    lines.push(Line::default());
                }
            }
            Event::End(TagEnd::Item | TagEnd::TableRow) => flush(&mut current, &mut lines),
            Event::Start(Tag::TableCell) if !current.is_empty() => {
                current.push(Span::styled(" | ", Style::default().fg(palette.dim)))
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                let safe = terminal_text(&text, true);
                let mut parts = safe.split('\n').peekable();
                while let Some(part) = parts.next() {
                    current.push(Span::styled(
                        part.to_owned(),
                        *styles.last().expect("base style"),
                    ));
                    if parts.peek().is_some() {
                        lines.push(Line::from(std::mem::take(&mut current)));
                    }
                }
            }
            Event::Code(text) => current.push(Span::styled(
                terminal_text(&text, false),
                Style::default().fg(palette.md_code).bg(palette.selected_bg),
            )),
            Event::SoftBreak if !code => current.push(Span::raw(" ")),
            Event::SoftBreak | Event::HardBreak => {
                lines.push(Line::from(std::mem::take(&mut current)));
            }
            Event::Rule => {
                flush(&mut current, &mut lines);
                lines.push(Line::styled("---", Style::default().fg(palette.dim)));
            }
            Event::TaskListMarker(checked) => {
                current.push(Span::raw(if checked { "[x] " } else { "[ ] " }))
            }
            _ => {}
        }
    }
    flush(&mut current, &mut lines);
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}
