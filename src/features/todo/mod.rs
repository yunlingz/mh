//! Native todo capability, separate from filesystem and process tools.

pub mod state;
pub mod view;

use serde_json::{Value, json};

use crate::message::ToolResult;
use crate::text::terminal_text;
use crate::tools::{ToolDefinition, definition, parse};
use state::{Operation, Params, State, Task};

pub struct Outcome {
    pub state: State,
    pub result: ToolResult,
    pub changed: bool,
}

pub fn tool() -> ToolDefinition {
    definition::<Params>(
        "todo",
        "Manage multi-step tasks. Actions: create, update, list, get, delete, clear. Create uses subject or independent tasks[]. Update requires id and a mutable field. Use blockedBy dependencies; updates use addBlockedBy/removeBlockedBy. Track progress with pending, in_progress, completed, or deleted.",
    )
}

pub fn execute(state: &State, raw: Value) -> Outcome {
    let raw = normalize(raw);
    let operation = parse::<Params>(raw.clone(), "todo").and_then(|params| state.apply(&params));
    match operation {
        Ok((next, op)) => {
            let changed = op.changes_state();
            let content = match &op {
                Operation::Created(ids) => ids
                    .iter()
                    .map(|id| {
                        let task = next
                            .tasks
                            .iter()
                            .find(|task| task.id == *id)
                            .expect("created task");
                        format!(
                            "Created #{id}: {} (pending)",
                            terminal_text(&task.subject, false)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Operation::Updated {
                    id,
                    to,
                    changed: false,
                    ..
                } => format!(
                    "No change: #{id} already matches the requested values (status: {})",
                    to.label()
                ),
                Operation::Updated { id, from, to, .. } if from != to => {
                    format!("Updated #{id} ({} -> {})", from.label(), to.label())
                }
                Operation::Updated { id, .. } => format!("Updated #{id}"),
                Operation::Deleted(task) => format!(
                    "Deleted #{}: {}",
                    task.id,
                    terminal_text(&task.subject, false)
                ),
                Operation::Cleared(count) => format!("Cleared {count} tasks"),
                Operation::Listed(tasks) if tasks.is_empty() => "No tasks".into(),
                Operation::Listed(tasks) => tasks.iter().map(row).collect::<Vec<_>>().join("\n"),
                Operation::Found(task) => {
                    let mut text = row(task);
                    if let Some(description) = &task.description {
                        text.push_str(&format!(
                            "\n  description: {}",
                            terminal_text(description, false)
                        ));
                    }
                    if let Some(active_form) = &task.active_form {
                        text.push_str(&format!(
                            "\n  activeForm: {}",
                            terminal_text(active_form, false)
                        ));
                    }
                    if !task.blocked_by.is_empty() {
                        text.push_str(&format!(
                            "\n  blockedBy: {}",
                            task.blocked_by
                                .iter()
                                .map(|id| format!("#{id}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    if let Some(owner) = &task.owner {
                        text.push_str(&format!("\n  owner: {}", terminal_text(owner, false)));
                    }
                    let blocks = next
                        .tasks
                        .iter()
                        .filter(|other| other.blocked_by.contains(&task.id))
                        .map(|other| format!("#{}", other.id))
                        .collect::<Vec<_>>();
                    if !blocks.is_empty() {
                        text.push_str(&format!("\n  blocks: {}", blocks.join(", ")));
                    }
                    text
                }
            };
            let details = json!({"action":raw.get("action"),"params":raw,"tasks":next.tasks,"nextId":next.next_id});
            Outcome {
                state: next,
                changed,
                result: ToolResult {
                    details,
                    ..ToolResult::text(content)
                },
            }
        }
        Err(error) => Outcome {
            state: state.clone(),
            changed: false,
            result: ToolResult {
                details: json!({"params":raw,"tasks":state.tasks,"nextId":state.next_id,"error":error}),
                ..ToolResult::error(format!("Error: {error}"))
            },
        },
    }
}

pub fn row(task: &Task) -> String {
    let mut text = format!(
        "[{}] #{} {}",
        task.status.label(),
        task.id,
        terminal_text(&task.subject, false)
    );
    if task.status == state::Status::InProgress
        && let Some(form) = &task.active_form
    {
        text.push_str(&format!(" ({})", terminal_text(form, false)));
    }
    if !task.blocked_by.is_empty() {
        text.push_str(&format!(
            " blockedBy: {}",
            task.blocked_by
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    text
}

fn normalize(mut raw: Value) -> Value {
    for key in ["id", "blockedBy", "addBlockedBy", "removeBlockedBy"] {
        if let Some(value) = raw.get_mut(key) {
            integral_ids(value);
        }
    }
    if let Some(tasks) = raw.get_mut("tasks") {
        if let Some(text) = tasks.as_str()
            && let Ok(value @ Value::Array(_)) = serde_json::from_str::<Value>(text)
        {
            *tasks = value;
        }
        if let Some(tasks) = tasks.as_array_mut() {
            for task in tasks {
                if let Some(text) = task.as_str()
                    && let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(text)
                {
                    *task = value;
                }
                if let Some(value) = task.get_mut("blockedBy") {
                    integral_ids(value);
                }
            }
        }
    }
    raw
}

fn integral_ids(value: &mut Value) {
    if let Some(ids) = value.as_array_mut() {
        for id in ids {
            integral_ids(id);
        }
    } else if let Some(number) = value.as_f64()
        && number.fract() == 0.0
        && (0.0..=9_007_199_254_740_991.0).contains(&number)
    {
        // JSON Schema treats 1.0 as an integer; serde's u64 decoder does not.
        *value = Value::from(number as u64);
    }
}
