//! Dispatch the four core tools and two named capabilities without a plugin layer.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::features::{ask_user_question as ask, todo};
use crate::message::{ToolCall, ToolResult};
use crate::session::{Entry, Session};
use crate::tools::{CoreTools, Execution, ToolDefinition, ToolUpdate};

pub struct QuestionRequest {
    pub call_id: String,
    pub params: Arc<ask::Params>,
    pub reply: oneshot::Sender<ask::Answers>,
}

pub enum ToolEvent {
    Started(ToolCall),
    Update(ToolUpdate),
    Question(QuestionRequest),
    Finished {
        call_id: String,
        result: Arc<ToolResult>,
    },
}

#[derive(Clone)]
pub struct Tools {
    core: CoreTools,
    session: Arc<Session>,
    todo: Arc<Mutex<todo::state::State>>,
    events: Option<mpsc::Sender<ToolEvent>>,
    interactive: bool,
}

pub fn definitions(interactive: bool) -> Vec<ToolDefinition> {
    let mut definitions = crate::tools::definitions();
    definitions.push(todo::tool());
    if interactive {
        definitions.push(ask::tool());
    }
    definitions
}

impl Tools {
    pub fn new(
        session: Arc<Session>,
        events: Option<mpsc::Sender<ToolEvent>>,
        interactive: bool,
    ) -> Result<Self, String> {
        if interactive && events.is_none() {
            return Err("Interactive tools require a questionnaire event receiver".into());
        }
        let state = session
            .snapshot()
            .todo
            .as_ref()
            .map(|value| todo::state::State::from_saved(value))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            core: CoreTools::new(session.cwd().to_owned(), session.outputs())?,
            session,
            todo: Arc::new(Mutex::new(state)),
            events,
            interactive,
        })
    }

    async fn emit(&self, event: ToolEvent, cancel: &CancellationToken) -> bool {
        let Some(events) = &self.events else {
            return true;
        };
        tokio::select! {
            _ = cancel.cancelled() => false,
            sent = events.send(event) => sent.is_ok(),
        }
    }

    pub async fn execute(&self, call: &ToolCall, cancel: &CancellationToken) -> Arc<ToolResult> {
        self.emit(ToolEvent::Started(call.clone()), cancel).await;
        let result = match self.run(call, cancel).await {
            Ok(result) => result,
            Err(error) => ToolResult::error(error),
        };
        let result = Arc::new(result);
        self.emit(
            ToolEvent::Finished {
                call_id: call.id.clone(),
                result: result.clone(),
            },
            cancel,
        )
        .await;
        result
    }

    pub async fn reject(
        &self,
        call: &ToolCall,
        message: &'static str,
        cancel: &CancellationToken,
    ) -> Arc<ToolResult> {
        self.emit(ToolEvent::Started(call.clone()), cancel).await;
        let result = Arc::new(ToolResult::error(message));
        self.emit(
            ToolEvent::Finished {
                call_id: call.id.clone(),
                result: result.clone(),
            },
            cancel,
        )
        .await;
        result
    }

    async fn run(&self, call: &ToolCall, cancel: &CancellationToken) -> Result<ToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        match call.name.as_str() {
            "todo" => {
                let raw: Value = serde_json::from_str(&call.arguments)
                    .map_err(|_| "Invalid tool arguments: expected complete JSON")?;
                let mut state = self.todo.lock().await;
                let outcome = todo::execute(&state, raw);
                if outcome.changed {
                    self.session
                        .append(Entry::Todo {
                            state: Arc::new(
                                serde_json::to_value(&outcome.state)
                                    .expect("serializable task state"),
                            ),
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                    *state = outcome.state;
                }
                Ok(outcome.result)
            }
            "ask_user_question" => {
                if !self.interactive || self.events.is_none() {
                    return Ok(ask::failure(
                        "ui_unavailable",
                        "Interactive UI is unavailable",
                    ));
                }
                let raw: Value = serde_json::from_str(&call.arguments)
                    .map_err(|_| "Invalid tool arguments: expected complete JSON")?;
                let params = match ask::prepare(raw) {
                    Ok(params) => Arc::new(params),
                    Err(result) => return Ok(result),
                };
                let (reply, answer) = oneshot::channel();
                let delivered = self
                    .emit(
                        ToolEvent::Question(QuestionRequest {
                            call_id: call.id.clone(),
                            params,
                            reply,
                        }),
                        cancel,
                    )
                    .await;
                if !delivered {
                    return if cancel.is_cancelled() {
                        Err("Operation aborted".into())
                    } else {
                        Ok(ask::failure(
                            "ui_unavailable",
                            "Questionnaire UI is unavailable",
                        ))
                    };
                }
                tokio::select! {
                    _ = cancel.cancelled() => Err("Operation aborted".into()),
                    answer = answer => Ok(match answer {
                        Ok(answer) => answer.tool_result(),
                        Err(_) => ask::failure("ui_unavailable", "Questionnaire UI closed without an answer"),
                    }),
                }
            }
            _ => {
                let mut execution = Execution::new(&call.id, cancel.clone());
                let bridge = self.events.as_ref().map(|events| {
                    let events = events.clone();
                    let cancel = cancel.clone();
                    let (sender, mut receiver) = mpsc::channel(128);
                    execution.updates = Some(sender);
                    tokio::spawn(async move {
                        while let Some(update) = receiver.recv().await {
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                sent = events.send(ToolEvent::Update(update)) => {
                                    if sent.is_err() { break; }
                                }
                            }
                        }
                    })
                });
                let result = self.core.execute(call, &execution).await;
                drop(execution);
                if let Some(bridge) = bridge {
                    let _ = bridge.await;
                }
                Ok(result)
            }
        }
    }
}
