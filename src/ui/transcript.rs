//! Retained transcript presentation, independent of persistence and streaming.

use std::collections::BTreeMap;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::message::{Message, ToolCall, ToolResult};
use crate::session::PresentationItem;
use crate::text::terminal_text;
use crate::theme::Palette;
use crate::ui::{markdown, status::Status};

pub fn render(items: &[PresentationItem], palette: &Palette, expanded: bool) -> Vec<Line<'static>> {
    render_with_anchors(items, palette, expanded).0
}

pub fn render_with_anchors(
    items: &[PresentationItem],
    palette: &Palette,
    expanded: bool,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let calls = items
        .iter()
        .filter_map(|item| match item {
            PresentationItem::Message { message, .. } => match message.as_ref() {
                Message::Assistant { response } => Some(response.tool_calls()),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .map(|call| (call.id.clone(), call))
        .collect::<BTreeMap<_, _>>();
    let mut lines = Vec::new();
    let mut anchors = Vec::new();
    for item in items {
        let start = lines.len();
        match item {
            PresentationItem::Message { message, .. } => match message.as_ref() {
                Message::User { content } => {
                    separate(&mut lines);
                    let mut text = String::new();
                    let mut image = 0;
                    for part in content {
                        match part {
                            crate::message::Content::Text { text: value } => text.push_str(value),
                            crate::message::Content::Image { .. } => {
                                image += 1;
                                text.push_str(&format!("[Image #{image}]"));
                            }
                        }
                    }
                    let text = terminal_text(&text, true);
                    for (index, line) in text.lines().enumerate() {
                        lines.push(
                            Line::from(vec![
                                Span::styled(
                                    if index == 0 { "❯ " } else { "  " },
                                    Style::default().fg(palette.accent),
                                ),
                                Span::styled(
                                    line.to_owned(),
                                    Style::default().fg(palette.user_message_text),
                                ),
                            ])
                            .style(Style::default().bg(palette.user_message_bg)),
                        );
                    }
                    separate(&mut lines);
                }
                Message::Assistant { response } => {
                    let thought = response.thinking();
                    if !thought.is_empty() {
                        thinking(&thought, palette, expanded, &mut lines);
                    }
                    let text = response.text();
                    if !text.is_empty() {
                        separate(&mut lines);
                        lines.extend(markdown::render(&text, palette));
                        separate(&mut lines);
                    }
                    if let Some(error) = &response.error {
                        lines.push(Line::from(vec![
                            Status::Error.span(palette),
                            Span::styled(
                                terminal_text(error, false),
                                Style::default().fg(palette.error),
                            ),
                        ]));
                        separate(&mut lines);
                    }
                }
                Message::ToolResult {
                    call_id,
                    name,
                    result,
                } => {
                    let fallback = ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: "{}".into(),
                    };
                    let call = calls.get(call_id).unwrap_or(&fallback);
                    tool(call, Some(result), None, palette, expanded, &mut lines);
                }
            },
            PresentationItem::Compaction { tokens_before, .. } => {
                separate(&mut lines);
                lines.push(Line::styled(
                    tokens_before.map_or_else(
                        || "◈ Context compacted".into(),
                        |tokens| format!("◈ Context compacted · {tokens} tokens before"),
                    ),
                    Style::default().fg(palette.dim),
                ));
                separate(&mut lines);
            }
            PresentationItem::Cancelled { .. } => {
                lines.push(Line::styled(
                    format!("{} Main work cancelled", Status::Cancelled.marker()),
                    Style::default().fg(palette.warning),
                ));
                separate(&mut lines);
            }
            PresentationItem::Todo { .. } => {}
        }
        if matches!(item, PresentationItem::Message { .. }) && lines.len() > start {
            anchors.push(start);
        }
    }
    (lines, anchors)
}

pub fn thinking(text: &str, palette: &Palette, expanded: bool, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::styled(
        if expanded {
            "▾ Thinking"
        } else {
            "▸ Thinking · Ctrl+O to expand"
        },
        Style::default().fg(palette.thinking_text),
    ));
    if expanded {
        lines.extend(terminal_text(text, true).lines().map(|line| {
            Line::styled(
                format!("  {line}"),
                Style::default().fg(palette.thinking_text),
            )
        }));
    }
}

pub fn tool(
    call: &ToolCall,
    result: Option<&ToolResult>,
    output: Option<&str>,
    palette: &Palette,
    expanded: bool,
    lines: &mut Vec<Line<'static>>,
) {
    separate(lines);
    let status = result.map_or(Status::InProgress, |result| {
        if result.is_error {
            Status::Error
        } else {
            Status::Completed
        }
    });
    let color = status.color(palette);
    let marker = status.marker();
    let arguments: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let title = match call.name.as_str() {
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "bash" if cfg!(windows) => "PowerShell",
        "bash" => "Shell",
        "todo" => "Todo",
        "ask_user_question" => "Ask user",
        other => other,
    };
    let description = arguments
        .get("path")
        .or_else(|| arguments.get("command"))
        .or_else(|| arguments.get("subject"))
        .or_else(|| arguments.get("action"))
        .and_then(Value::as_str)
        .unwrap_or("");
    lines.push(Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(color)),
        Span::styled(
            title.to_owned(),
            Style::default()
                .fg(palette.tool_title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if description.is_empty() {
                String::new()
            } else {
                format!("  {}", terminal_text(description, false))
            },
            Style::default().fg(palette.muted),
        ),
    ]));
    let content = result
        .and_then(|result| result.details.get("diff").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| result.map(ToolResult::plain_text))
        .unwrap_or_else(|| output.unwrap_or("").to_owned());
    let safe = terminal_text(&content, true);
    let output_lines = safe.lines().collect::<Vec<_>>();
    let budget = if expanded { output_lines.len() } else { 6 };
    let start = if call.name == "bash" {
        output_lines.len().saturating_sub(budget)
    } else {
        0
    };
    for line in output_lines.iter().skip(start).take(budget) {
        let color = if result.is_some_and(|result| result.details.get("diff").is_some()) {
            if line.starts_with('+') {
                palette.tool_diff_added
            } else if line.starts_with('-') {
                palette.tool_diff_removed
            } else {
                palette.tool_diff_context
            }
        } else {
            palette.tool_output
        };
        lines.push(Line::from(vec![
            Span::styled("  │ ", Style::default().fg(palette.border)),
            Span::styled((*line).to_owned(), Style::default().fg(color)),
        ]));
    }
    if output_lines.len() > budget {
        lines.push(Line::from(vec![
            Span::styled("  └ ", Style::default().fg(palette.border)),
            Span::styled(
                format!(
                    "… {} more lines · Ctrl+O to expand",
                    output_lines.len() - budget
                ),
                Style::default().fg(palette.dim),
            ),
        ]));
    }
    separate(lines);
}

// Separate message/tool groups, not every content line. This is presentation
// spacing; original user/model/tool strings and their internal layout stay intact.
fn separate(lines: &mut Vec<Line<'static>>) {
    if lines.is_empty() {
        return;
    }
    let blank = lines
        .iter()
        .rev()
        .take_while(|line| line.spans.iter().all(|span| span.content.is_empty()))
        .count();
    for _ in blank..2 {
        lines.push(Line::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Assistant, OutputItem, OutputText, StopReason};
    use std::sync::Arc;

    #[test]
    fn tool_connectors_share_one_color_in_rendered_cells() {
        use ratatui::{
            Terminal,
            backend::TestBackend,
            widgets::{Block, Paragraph, Wrap},
        };
        let palette = Palette::default();
        let call = ToolCall {
            id: "fixture".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        let result = ToolResult::text("// guard comment\n".repeat(294));
        let mut lines = Vec::new();
        tool(&call, Some(&result), None, &palette, false, &mut lines);
        for width in [24, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Block::default().style(Style::default().bg(palette.background)),
                        frame.area(),
                    );
                    frame.render_widget(
                        Paragraph::new(lines.clone()).wrap(Wrap { trim: false }),
                        frame.area(),
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let connectors = buffer
                .content
                .iter()
                .filter(|cell| matches!(cell.symbol(), "│" | "└"))
                .collect::<Vec<_>>();
            assert_eq!(connectors.len(), 7);
            assert!(
                connectors
                    .iter()
                    .all(|cell| cell.fg == palette.border && cell.bg == palette.background)
            );
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| cell.symbol() == "…" && cell.fg == palette.dim)
            );
        }
    }

    #[test]
    fn readable_tools_keep_original_call_context_and_show_omissions() {
        let palette = Palette::default();
        let response = Assistant {
            provider: "test".into(),
            model: "model".into(),
            items: vec![OutputItem::FunctionCall {
                id: None,
                call_id: "call".into(),
                name: "read".into(),
                arguments: "{\"path\":\"src/main.rs\"}".into(),
            }],
            stop: StopReason::ToolUse,
            usage: None,
            error: None,
        };
        let items = vec![
            PresentationItem::Message {
                id: 1,
                message: Arc::new(Message::Assistant { response }),
            },
            PresentationItem::Message {
                id: 2,
                message: Arc::new(Message::ToolResult {
                    call_id: "call".into(),
                    name: "read".into(),
                    result: ToolResult::text("line\n".repeat(20)),
                }),
            },
        ];
        let collapsed = render(&items, &palette, false)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(collapsed.contains("✓ Read  src/main.rs"));
        assert!(collapsed.contains("14 more lines"));
        assert!(!collapsed.contains("[tool result:"));
        assert!(render(&items, &palette, true).len() > render(&items, &palette, false).len());
    }

    #[test]
    fn user_and_model_unicode_is_preserved_but_controls_are_not() {
        let items = vec![
            PresentationItem::Message {
                id: 1,
                message: Arc::new(Message::user("你好🙂\u{1b}]0;hidden\u{7}")),
            },
            PresentationItem::Message {
                id: 2,
                message: Arc::new(Message::Assistant {
                    response: Assistant {
                        provider: "test".into(),
                        model: "model".into(),
                        stop: StopReason::Stop,
                        usage: None,
                        error: None,
                        items: vec![OutputItem::Message {
                            id: None,
                            phase: None,
                            content: vec![OutputText::OutputText {
                                text: "A useful answer.".into(),
                                annotations: Vec::new(),
                            }],
                        }],
                    },
                }),
            },
        ];
        let text = render(&items, &Palette::default(), false)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("❯ 你好🙂"));
        assert!(!text.contains("hidden"));
        assert!(text.contains("A useful answer."));
    }
}
