//! The four core tools. Named task/question capabilities live elsewhere.

pub mod bash;
pub mod edit;
pub mod images;
pub mod mutations;
pub mod output;
pub mod path;
pub mod read;
pub mod shell;
pub mod write;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::de::{DeserializeOwned, IntoDeserializer};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::message::{ToolCall, ToolResult};
use crate::storage::private_dir;

#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[derive(Clone)]
pub struct ToolUpdate {
    pub call_id: String,
    pub text: String,
}

#[derive(Clone)]
pub struct Execution {
    pub call_id: String,
    pub cancel: CancellationToken,
    pub metadata: BTreeMap<String, String>,
    pub updates: Option<mpsc::Sender<ToolUpdate>>,
}

impl Execution {
    pub fn new(call_id: impl Into<String>, cancel: CancellationToken) -> Self {
        Self {
            call_id: call_id.into(),
            cancel,
            metadata: BTreeMap::new(),
            updates: None,
        }
    }
}

#[derive(Clone)]
pub struct CoreTools(Arc<CoreState>);

struct CoreState {
    cwd: PathBuf,
    outputs: PathBuf,
    mutations: mutations::MutationQueue,
    lifetime: CancellationToken,
}

impl Drop for CoreState {
    fn drop(&mut self) {
        self.lifetime.cancel();
    }
}

impl CoreTools {
    pub fn new(cwd: PathBuf, outputs: PathBuf) -> Result<Self, String> {
        private_dir(&outputs).map_err(|_| "Cannot create the session output directory")?;
        Ok(Self(Arc::new(CoreState {
            cwd,
            outputs,
            mutations: mutations::MutationQueue::default(),
            lifetime: CancellationToken::new(),
        })))
    }

    pub async fn execute(&self, call: &ToolCall, execution: &Execution) -> ToolResult {
        match self.run(call, execution).await {
            Ok(result) => result,
            Err(error) => ToolResult::error(error),
        }
    }

    async fn run(&self, call: &ToolCall, execution: &Execution) -> Result<ToolResult, String> {
        if execution.cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let raw: Value = serde_json::from_str(&call.arguments)
            .map_err(|_| "Invalid tool arguments: expected complete JSON")?;
        let cwd = self.0.cwd.clone();
        let cancel = execution.cancel.clone();
        match call.name.as_str() {
            "bash" => {
                let args = parse::<bash::BashArgs>(raw, "bash")?;
                bash::execute(args, &cwd, &self.0.outputs, execution, &self.0.lifetime).await
            }
            "read" => {
                let args = parse::<read::ReadArgs>(raw, "read")?;
                tokio::task::spawn_blocking(move || read::execute(args, &cwd, &cancel))
                    .await
                    .map_err(|_| "Read worker failed")?
            }
            "write" => {
                let args = parse::<write::WriteArgs>(raw, "write")?;
                let target = path::resolve(&args.path, &cwd);
                let lease = self
                    .0
                    .mutations
                    .lock(&target)
                    .await
                    .map_err(|error| path::io_code(&error))?;
                tokio::task::spawn_blocking(move || {
                    // The worker keeps this lock until filesystem operations settle, even on task cancellation.
                    let _lease = lease;
                    write::execute(args, &cwd, &cancel)
                })
                .await
                .map_err(|_| "Write worker failed")?
            }
            "edit" => {
                let args = parse::<edit::EditArgs>(edit::prepare(raw), "edit")?;
                let target = path::resolve(&args.path, &cwd);
                let lease = self
                    .0
                    .mutations
                    .lock(&target)
                    .await
                    .map_err(|error| path::io_code(&error))?;
                tokio::task::spawn_blocking(move || {
                    let _lease = lease;
                    edit::execute(args, &cwd, &cancel)
                })
                .await
                .map_err(|_| "Edit worker failed")?
            }
            _ => Err("Tool not found".into()),
        }
    }
}

pub fn parse<T: DeserializeOwned>(value: Value, tool: &str) -> Result<T, String> {
    serde_path_to_error::deserialize(value.into_deserializer())
        .map_err(|error| format!("Invalid {tool} arguments at {}", error.path()))
}

pub fn definition<T: JsonSchema>(name: &'static str, description: &'static str) -> ToolDefinition {
    let mut parameters =
        serde_json::to_value(schemars::schema_for!(T)).expect("serializable schema");
    parameters
        .as_object_mut()
        .expect("schema object")
        .remove("$schema");
    ToolDefinition {
        name,
        description,
        parameters,
    }
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition::<read::ReadArgs>(
            "read",
            "Read text files and image attachments. Text is limited to 2000 lines or 50 KiB. Use offset and limit to continue large reads.",
        ),
        definition::<write::WriteArgs>(
            "write",
            "Write a complete file. Creates parent directories and overwrites existing content. Use for new files or complete rewrites.",
        ),
        definition::<edit::EditArgs>(
            "edit",
            "Edit a single file using unique, disjoint oldText/newText replacements in edits[]. Match the original file, not incremental results. Batch targeted changes.",
        ),
        definition::<bash::BashArgs>("bash", shell::tool_description()),
    ]
}
