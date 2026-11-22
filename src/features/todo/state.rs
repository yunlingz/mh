//! Pure task state and mutation rules ported from rpiv-todo.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Deleted => "deleted",
        }
    }

    fn allows(self, target: Self) -> bool {
        self == target
            || match self {
                Self::Pending | Self::InProgress => true,
                Self::Completed => target == Self::Deleted,
                Self::Deleted => false,
            }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Update,
    List,
    Get,
    Delete,
    Clear,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: u64,
    pub subject: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub tasks: Vec<Task>,
    pub next_id: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewTask {
    pub subject: String,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub blocked_by: Option<Vec<u64>>,
    pub owner: Option<String>,
    pub metadata: Option<BTreeMap<String, Value>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Params {
    pub action: Action,
    pub id: Option<u64>,
    pub subject: Option<String>,
    pub tasks: Option<Vec<NewTask>>,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub status: Option<Status>,
    pub blocked_by: Option<Vec<u64>>,
    pub add_blocked_by: Option<Vec<u64>>,
    pub remove_blocked_by: Option<Vec<u64>>,
    pub owner: Option<String>,
    pub metadata: Option<BTreeMap<String, Value>>,
    pub include_deleted: Option<bool>,
}

pub enum Operation {
    Created(Vec<u64>),
    Updated {
        id: u64,
        from: Status,
        to: Status,
        changed: bool,
    },
    Deleted(Task),
    Cleared(usize),
    Listed(Vec<Task>),
    Found(Task),
}

impl Operation {
    pub fn changes_state(&self) -> bool {
        match self {
            Self::Created(_) | Self::Deleted(_) | Self::Cleared(_) => true,
            Self::Updated { changed, .. } => *changed,
            Self::Listed(_) | Self::Found(_) => false,
        }
    }
}

impl State {
    pub fn from_saved(value: &Value) -> Result<Self, String> {
        let state: Self =
            serde_json::from_value(value.clone()).map_err(|_| "Invalid saved todo state")?;
        let ids = state
            .tasks
            .iter()
            .map(|task| task.id)
            .collect::<BTreeSet<_>>();
        if ids.len() != state.tasks.len()
            || ids.contains(&0)
            || state.next_id == 0
            || ids.last().is_some_and(|id| *id >= state.next_id)
        {
            return Err("Invalid saved todo IDs".into());
        }
        if state
            .tasks
            .iter()
            .any(|task| task.blocked_by.iter().any(|id| !ids.contains(id)))
            || has_cycle(&state.tasks)
        {
            return Err("Invalid saved todo dependencies".into());
        }
        Ok(state)
    }

    pub fn apply(&self, params: &Params) -> Result<(Self, Operation), String> {
        let mut next = self.clone();
        let op = match params.action {
            Action::Create => {
                let specs = if let Some(tasks) = &params.tasks {
                    if params.subject.is_some() {
                        return Err("create requires subject or tasks[], not both".into());
                    }
                    if tasks.is_empty() {
                        return Err("tasks[] must be non-empty".into());
                    }
                    if params.description.is_some()
                        || params.active_form.is_some()
                        || params.blocked_by.is_some()
                        || params.owner.is_some()
                        || params.metadata.is_some()
                    {
                        return Err("create with tasks[] takes task fields on each item, not at the top level".into());
                    }
                    tasks.clone()
                } else {
                    vec![NewTask {
                        subject: params
                            .subject
                            .clone()
                            .ok_or("subject required for create")?,
                        description: params.description.clone(),
                        active_form: params.active_form.clone(),
                        blocked_by: params.blocked_by.clone(),
                        owner: params.owner.clone(),
                        metadata: params.metadata.clone(),
                    }]
                };
                let end = next
                    .next_id
                    .checked_add(specs.len() as u64)
                    .ok_or("Task ID space exhausted")?;
                let sibling_ids = (next.next_id..end).collect::<BTreeSet<_>>();
                for (index, spec) in specs.iter().enumerate() {
                    if spec.subject.trim().is_empty() {
                        return Err(format!("tasks[{index}]: subject required for create"));
                    }
                    for id in spec.blocked_by.as_deref().unwrap_or_default() {
                        if sibling_ids.contains(id) {
                            return Err(
                                "Batch tasks cannot depend on another item in the same batch"
                                    .into(),
                            );
                        }
                        dependency(&self.tasks, *id, "blockedBy")?;
                    }
                }
                let ids = sibling_ids.iter().copied().collect::<Vec<_>>();
                for (id, spec) in ids.iter().zip(specs) {
                    next.tasks.push(Task {
                        id: *id,
                        subject: spec.subject,
                        status: Status::Pending,
                        description: spec.description.filter(|value| !value.is_empty()),
                        active_form: spec.active_form.filter(|value| !value.is_empty()),
                        blocked_by: spec.blocked_by.unwrap_or_default(),
                        owner: spec.owner.filter(|value| !value.is_empty()),
                        metadata: spec.metadata,
                    });
                }
                next.next_id = end;
                Operation::Created(ids)
            }
            Action::Update => {
                if params.blocked_by.is_some() {
                    return Err(
                        "blockedBy is create-only; use addBlockedBy/removeBlockedBy for updates"
                            .into(),
                    );
                }
                let id = params.id.ok_or("id required for update")?;
                let index = next
                    .tasks
                    .iter()
                    .position(|task| task.id == id)
                    .ok_or_else(|| format!("#{id} not found"))?;
                if params.subject.is_none()
                    && params.description.is_none()
                    && params.active_form.is_none()
                    && params.status.is_none()
                    && params.owner.is_none()
                    && params.metadata.is_none()
                    && params.add_blocked_by.as_ref().is_none_or(Vec::is_empty)
                    && params.remove_blocked_by.as_ref().is_none_or(Vec::is_empty)
                {
                    return Err("update requires at least one mutable field".into());
                }
                let before = next.tasks[index].clone();
                let target = params.status.unwrap_or(before.status);
                if !before.status.allows(target) {
                    return Err(format!(
                        "illegal transition {} -> {}",
                        before.status.label(),
                        target.label()
                    ));
                }
                let mut updated = before.clone();
                updated.status = target;
                if let Some(value) = &params.subject {
                    updated.subject = value.clone();
                }
                if let Some(value) = &params.description {
                    updated.description = Some(value.clone());
                }
                if let Some(value) = &params.active_form {
                    updated.active_form = Some(value.clone());
                }
                if let Some(value) = &params.owner {
                    updated.owner = Some(value.clone());
                }
                if let Some(ids) = &params.remove_blocked_by {
                    updated.blocked_by.retain(|id| !ids.contains(id));
                }
                if let Some(ids) = &params.add_blocked_by {
                    for dep in ids {
                        if *dep == id {
                            return Err(format!("cannot block #{id} on itself"));
                        }
                        dependency(&self.tasks, *dep, "addBlockedBy")?;
                        if !updated.blocked_by.contains(dep) {
                            updated.blocked_by.push(*dep);
                        }
                    }
                }
                if let Some(metadata) = &params.metadata {
                    let mut merged = updated.metadata.unwrap_or_default();
                    for (key, value) in metadata {
                        if value.is_null() {
                            merged.remove(key);
                        } else {
                            merged.insert(key.clone(), value.clone());
                        }
                    }
                    updated.metadata = (!merged.is_empty()).then_some(merged);
                }
                let changed = updated != before;
                next.tasks[index] = updated;
                if has_cycle(&next.tasks) {
                    return Err("addBlockedBy would create a cycle in the blockedBy graph".into());
                }
                Operation::Updated {
                    id,
                    from: before.status,
                    to: target,
                    changed,
                }
            }
            Action::Delete => {
                let id = params.id.ok_or("id required for delete")?;
                let task = next
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == id)
                    .ok_or_else(|| format!("#{id} not found"))?;
                if task.status == Status::Deleted {
                    return Err(format!("#{id} is already deleted"));
                }
                task.status = Status::Deleted;
                Operation::Deleted(task.clone())
            }
            Action::Clear => {
                let count = next.tasks.len();
                next = Self::default();
                Operation::Cleared(count)
            }
            Action::List => Operation::Listed(
                next.tasks
                    .iter()
                    .filter(|task| {
                        (params.include_deleted.unwrap_or(false) || task.status != Status::Deleted)
                            && params.status.is_none_or(|status| task.status == status)
                    })
                    .cloned()
                    .collect(),
            ),
            Action::Get => {
                let id = params.id.ok_or("id required for get")?;
                Operation::Found(
                    next.tasks
                        .iter()
                        .find(|task| task.id == id)
                        .cloned()
                        .ok_or_else(|| format!("#{id} not found"))?,
                )
            }
        };
        Ok((next, op))
    }
}

fn dependency(tasks: &[Task], id: u64, field: &str) -> Result<(), String> {
    match tasks.iter().find(|task| task.id == id) {
        None => Err(format!("{field}: #{id} not found")),
        Some(task) if task.status == Status::Deleted => Err(format!("{field}: #{id} is deleted")),
        Some(_) => Ok(()),
    }
}

fn has_cycle(tasks: &[Task]) -> bool {
    let edges = tasks
        .iter()
        .map(|task| (task.id, &task.blocked_by))
        .collect::<BTreeMap<_, _>>();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for &root in edges.keys() {
        let mut stack = vec![(root, false)];
        while let Some((id, leaving)) = stack.pop() {
            if leaving {
                visiting.remove(&id);
                visited.insert(id);
            } else if !visited.contains(&id) {
                if !visiting.insert(id) {
                    return true;
                }
                stack.push((id, true));
                if let Some(dependencies) = edges.get(&id) {
                    stack.extend(dependencies.iter().rev().map(|&id| (id, false)));
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_dependency_graphs_use_bounded_heap_traversal_not_the_call_stack() {
        let mut tasks = (1..=20_000)
            .map(|id| Task {
                id,
                subject: "fixture".into(),
                status: Status::Pending,
                description: None,
                active_form: None,
                owner: None,
                metadata: None,
                blocked_by: if id == 20_000 {
                    Vec::new()
                } else {
                    vec![id + 1]
                },
            })
            .collect::<Vec<_>>();
        assert!(!has_cycle(&tasks));
        tasks[19_999].blocked_by.push(1);
        assert!(has_cycle(&tasks));
    }
}
