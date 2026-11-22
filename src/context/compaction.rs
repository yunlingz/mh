//! Pure compaction planning. Tool results never become independent cut points.

use std::collections::BTreeSet;

use crate::config::CompactionConfig;
use crate::message::{Content, Message, StopReason};
use crate::session::{ContextSource, Snapshot};

pub const SUMMARY_SYSTEM: &str = "You are a context summarization assistant. Read the supplied conversation and produce a structured checkpoint. Do not continue the conversation or answer its questions. Do not call tools. Preserve exact paths, identifiers, decisions, constraints, and unresolved work.";

pub struct Plan {
    pub first_kept_id: u64,
    pub history: Vec<Message>,
    pub turn_prefix: Vec<Message>,
    pub previous_summary: Option<String>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub tokens_before: u64,
}

impl Plan {
    pub fn history_prompt(&self, focus: &str) -> String {
        let previous = self
            .previous_summary
            .as_ref()
            .map_or(String::new(), |summary| {
                format!("<previous-summary>\n{summary}\n</previous-summary>\n\n")
            });
        format!(
            "<conversation>\n{}\n</conversation>\n\n{previous}Create or update a concise checkpoint with these sections:\n\n## Goal\n## Constraints & Preferences\n## Progress\n### Done\n### In Progress\n### Blocked\n## Key Decisions\n## Next Steps\n## Critical Context\n\nPreserve relevant previous information. Update progress without inventing completed work. Preserve exact paths and errors. Additional focus: {focus}",
            serialize(&self.history)
        )
    }

    pub fn turn_prompt(&self, focus: &str) -> String {
        format!(
            "<conversation>\n{}\n</conversation>\n\nThese are earlier messages from one ongoing user request. Later messages are retained separately. Summarize only what is present. Use these sections:\n\n## Original Request\n## Progress So Far\n## Context Needed to Continue\n\nAdditional focus: {focus}",
            serialize(&self.turn_prefix)
        )
    }

    pub fn merge(&self, history: &str, turn: &str) -> String {
        let mut summary = history.to_owned();
        if !turn.is_empty() {
            summary.push_str(&format!("\n\n---\n\nTurn Context (split turn):\n\n{turn}"));
        }
        if !self.read_files.is_empty() {
            summary.push_str(&format!(
                "\n\n<read-files>\n{}\n</read-files>",
                self.read_files.join("\n")
            ));
        }
        if !self.modified_files.is_empty() {
            summary.push_str(&format!(
                "\n\n<modified-files>\n{}\n</modified-files>",
                self.modified_files.join("\n")
            ));
        }
        summary
    }
}

pub fn should_compact(tokens: u64, window: u64, settings: &CompactionConfig) -> bool {
    tokens > window.saturating_sub(settings.reserve_tokens)
}

pub fn prepare(snapshot: &Snapshot, settings: &CompactionConfig) -> Result<Plan, &'static str> {
    if snapshot.stable_len != snapshot.context.len() {
        return Err("Wait for the current tool batch before compacting");
    }
    let context = snapshot
        .context
        .iter()
        .filter(|item| item.source == ContextSource::Message)
        .collect::<Vec<_>>();
    let cuts = context
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            matches!(
                item.message.as_ref(),
                Message::User { .. } | Message::Assistant { .. }
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let Some(&first) = cuts.first() else {
        return Err("No earlier context to summarize");
    };
    let mut cut = first;
    let mut accumulated = 0;
    for index in (0..context.len()).rev() {
        accumulated += context[index].message.estimate_tokens();
        if accumulated >= settings.keep_recent_tokens {
            cut = cuts
                .iter()
                .rev()
                .copied()
                .find(|candidate| *candidate <= index)
                .unwrap_or(*cuts.last().expect("valid cut point"));
            break;
        }
    }
    let split = !matches!(context[cut].message.as_ref(), Message::User { .. });
    let turn_start = if split {
        (0..cut)
            .rev()
            .find(|&index| matches!(context[index].message.as_ref(), Message::User { .. }))
    } else {
        None
    };
    let history_end = turn_start.unwrap_or(cut);
    let history = context[..history_end]
        .iter()
        .map(|item| (*item.message).clone())
        .collect::<Vec<_>>();
    let turn_prefix = turn_start.map_or(Vec::new(), |start| {
        context[start..cut]
            .iter()
            .map(|item| (*item.message).clone())
            .collect()
    });
    if history.is_empty() && turn_prefix.is_empty() {
        return Err("No earlier context to summarize");
    }
    let mut read = snapshot
        .read_files
        .iter()
        .flat_map(|files| files.iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut modified = snapshot
        .modified_files
        .iter()
        .flat_map(|files| files.iter().cloned())
        .collect::<BTreeSet<_>>();
    // Migrate each absent legacy field once. Present empty arrays must not turn
    // quoted summary text into authoritative file metadata.
    if let Some(previous) = &snapshot.previous_summary {
        if snapshot.read_files.is_none() {
            read.extend(file_section(previous, "read-files"));
        }
        if snapshot.modified_files.is_none() {
            modified.extend(file_section(previous, "modified-files"));
        }
    }
    for message in history.iter().chain(&turn_prefix) {
        if let Message::Assistant { response } = message {
            for call in response.tool_calls() {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or_default();
                if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                    match call.name.as_str() {
                        "read" => {
                            read.insert(path.to_owned());
                        }
                        "write" | "edit" => {
                            modified.insert(path.to_owned());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    read.retain(|path| !modified.contains(path));
    Ok(Plan {
        first_kept_id: context[cut].id,
        history,
        turn_prefix,
        previous_summary: snapshot
            .previous_summary
            .as_ref()
            .map(|summary| summary.to_string()),
        read_files: read.into_iter().collect(),
        modified_files: modified.into_iter().collect(),
        tokens_before: snapshot.estimate.tokens(),
    })
}

fn file_section(summary: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>\n");
    let close = format!("\n</{tag}>");
    summary
        .split_once(&open)
        .and_then(|(_, tail)| tail.split_once(&close))
        .map_or(Vec::new(), |(body, _)| {
            body.lines().map(str::to_owned).collect()
        })
}

pub fn summary_is_complete(response: &crate::message::Assistant) -> bool {
    matches!(response.stop, StopReason::Stop)
        && response.tool_calls().is_empty()
        && !response.text().trim().is_empty()
}

fn serialize(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => Some(format!(
                "[User]: {}",
                content
                    .iter()
                    .filter_map(|block| match block {
                        Content::Text { text } => Some(text.as_str()),
                        Content::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            Message::Assistant { response } => {
                let mut parts = Vec::new();
                let text = response.text();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls = response.tool_calls();
                if !calls.is_empty() {
                    parts.push(format!(
                        "[Assistant tool calls]: {}",
                        calls
                            .iter()
                            .map(|call| format!("{}({})", call.name, call.arguments))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
                (!parts.is_empty()).then(|| parts.join("\n"))
            }
            Message::ToolResult { result, .. } => {
                let text = result.plain_text();
                let clipped = text.chars().take(2000).collect::<String>();
                let omitted = text.chars().count().saturating_sub(2000);
                Some(format!(
                    "[Tool result]: {clipped}{}",
                    if omitted > 0 {
                        format!("\n[... {omitted} more characters truncated]")
                    } else {
                        String::new()
                    }
                ))
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
