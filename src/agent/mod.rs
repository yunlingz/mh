//! Streaming agent orchestration. Terminal drawing is not part of this module.

pub mod queue;
pub mod tools;

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::context::{Resources, compaction};
use crate::features::btw;
use crate::message::{Assistant, Content, Message, StopReason};
use crate::providers::{Delta, Failure, Request, Responses};
use crate::session::{Entry, ModelChoice, Session, UsageKind};

const MAX_RETRIES: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Streaming,
    Tools,
    Compacting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Truncated,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub status: RunStatus,
    pub response: Option<Arc<Assistant>>,
}

impl RunOutcome {
    fn from_response(response: Arc<Assistant>) -> Self {
        let status = match response.stop {
            StopReason::Stop | StopReason::ToolUse => RunStatus::Completed,
            StopReason::Length => RunStatus::Truncated,
            StopReason::Error => RunStatus::Failed,
            StopReason::Aborted => RunStatus::Cancelled,
        };
        Self {
            status,
            response: Some(response),
        }
    }

    fn cancelled(response: Option<Arc<Assistant>>) -> Self {
        Self {
            status: RunStatus::Cancelled,
            response,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryScope {
    Main,
    Compaction,
    Side,
}

pub enum Event {
    RetryScheduled {
        scope: RetryScope,
        attempt: u8,
        max_retries: u8,
        delay: Duration,
    },
    RetryStarted {
        scope: RetryScope,
    },
    Phase(Phase),
    Text(String),
    Thinking(String),
    Assistant(Arc<Assistant>),
    StreamToolCallStarted {
        output_index: usize,
        call_id: String,
        name: String,
    },
    StreamToolCallDelta {
        output_index: usize,
        delta: String,
    },
    StreamToolCallFinished {
        output_index: usize,
    },
    QueuedInputConsumed {
        id: u64,
    },
    Notice(String),
    SideText(String),
    SideRetry {
        attempt: u8,
    },
}

pub struct Agent {
    pub session: Arc<Session>,
    resources: std::sync::RwLock<Arc<Resources>>,
    config: Arc<Config>,
    http: Responses,
    tools: tools::Tools,
    definitions: Vec<crate::tools::ToolDefinition>,
    events: Option<mpsc::Sender<Event>>,
    operation: Mutex<()>,
    next_model: std::sync::Mutex<Option<ModelChoice>>,
}

impl Agent {
    pub async fn new(
        config: Arc<Config>,
        session: Arc<Session>,
        resources: Resources,
        http: Responses,
        events: Option<mpsc::Sender<Event>>,
        tool_events: Option<mpsc::Sender<tools::ToolEvent>>,
        interactive: bool,
    ) -> Result<Self, String> {
        if interactive && tool_events.is_none() {
            return Err("Interactive mode requires a questionnaire event receiver".into());
        }
        session
            .close_interrupted_calls()
            .await
            .map_err(|error| error.to_string())?;
        let definitions = tools::definitions(interactive);
        let (tools_fingerprint, tools_tokens) = crate::context::tool_metadata(&definitions);
        let prompt = resources.prompt(&definitions);
        let snapshot = session.snapshot();
        if snapshot.prompt.as_str() != prompt || snapshot.tools_fingerprint != tools_fingerprint {
            session
                .append(Entry::Prompt {
                    text: Arc::new(prompt),
                    tools_fingerprint,
                    tools_tokens,
                })
                .await
                .map_err(|error| error.to_string())?;
        }
        let saved = snapshot
            .choice
            .clone()
            .unwrap_or_else(|| ModelChoice::new(&config.default_model));
        let (choice, migrated) = saved.normalize(&config)?;
        if snapshot.choice.is_none() || migrated {
            session
                .append(Entry::Model { choice })
                .await
                .map_err(|error| error.to_string())?;
        }
        let mut resources = resources;
        if migrated {
            resources
                .warnings
                .push(crate::session::MODEL_CHOICE_MIGRATION_NOTICE.into());
        }
        let tools = tools::Tools::new(session.clone(), tool_events, interactive)?;
        Ok(Self {
            session,
            resources: std::sync::RwLock::new(Arc::new(resources)),
            config,
            http,
            tools,
            definitions,
            events,
            operation: Mutex::new(()),
            next_model: std::sync::Mutex::new(None),
        })
    }

    /// Catalog snapshots can be retained by the UI without holding a draw-time lock.
    pub fn resources(&self) -> Arc<Resources> {
        self.resources.read().expect("resources lock").clone()
    }

    /// Replace the prompt/catalog only while no main operation is active. Persistence
    /// precedes publication, so a failed journal write cannot expose a stale prompt.
    pub async fn refresh_resources(&self, resources: Resources) -> Result<(), String> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| "Another main operation is active")?;
        let (tools_fingerprint, tools_tokens) = crate::context::tool_metadata(&self.definitions);
        let prompt = resources.prompt(&self.definitions);
        let snapshot = self.session.snapshot();
        if snapshot.prompt.as_str() != prompt || snapshot.tools_fingerprint != tools_fingerprint {
            self.session
                .append(Entry::Prompt {
                    text: Arc::new(prompt),
                    tools_fingerprint,
                    tools_tokens,
                })
                .await
                .map_err(|error| error.to_string())?;
        }
        *self.resources.write().expect("resources lock") = Arc::new(resources);
        Ok(())
    }

    pub fn proxy_status(&self) -> &crate::providers::proxy::Status {
        self.http.proxy_status()
    }

    async fn emit(&self, event: Event, cancel: &CancellationToken) {
        if let Some(events) = &self.events {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                _ = events.send(event) => {},
            }
        }
    }

    pub async fn select_model(&self, qualified: &str) -> Result<(), String> {
        self.config.model(qualified)?;
        let choice = ModelChoice::new(qualified);
        let Ok(_operation) = self.operation.try_lock() else {
            *self.next_model.lock().expect("pending model mutex") = Some(choice);
            return Ok(());
        };
        self.next_model.lock().expect("pending model mutex").take();
        self.session
            .append(Entry::Model { choice })
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn model_choice(&self) -> ModelChoice {
        self.next_model
            .lock()
            .expect("pending model mutex")
            .clone()
            .unwrap_or_else(|| {
                self.session
                    .snapshot()
                    .choice
                    .clone()
                    .expect("initialized model")
            })
    }

    async fn apply_next_model(&self) -> Result<(), String> {
        let choice = self.next_model.lock().expect("pending model mutex").take();
        if let Some(choice) = choice {
            self.session
                .append(Entry::Model { choice })
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn validate_input(&self, content: &[Content]) -> Result<(), String> {
        crate::session::validate_user_input(content).map_err(|error| error.to_string())?;
        let snapshot = self.session.snapshot();
        let choice = snapshot.choice.as_ref().expect("initialized model");
        let pending = self.next_model.lock().expect("pending model mutex");
        let choice = pending.as_ref().unwrap_or(choice);
        let (_, _, model) = self.config.model(&choice.qualified)?;
        if content
            .iter()
            .any(|part| matches!(part, Content::Image { .. }))
            && !model.input.contains(&crate::config::InputModality::Image)
        {
            return Err(
                "Selected model does not support image input; choose an image-capable model".into(),
            );
        }
        Ok(())
    }

    pub async fn run(
        &self,
        prompt: String,
        cancel: &CancellationToken,
    ) -> Result<RunOutcome, String> {
        self.run_input(vec![Content::text(prompt)], cancel).await
    }

    pub async fn run_input(
        &self,
        content: Vec<Content>,
        cancel: &CancellationToken,
    ) -> Result<RunOutcome, String> {
        self.run_input_with_queue(Some(content), &queue::Queue::default(), cancel)
            .await
    }

    pub async fn run_input_with_queue(
        &self,
        content: Option<Vec<Content>>,
        queue: &queue::Queue,
        cancel: &CancellationToken,
    ) -> Result<RunOutcome, String> {
        if let Some(content) = &content {
            self.validate_input(content)?;
        }
        if cancel.is_cancelled() {
            return Ok(RunOutcome::cancelled(None));
        }
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| "Another main operation is active")?;
        let result = async {
            if let Some(content) = content {
                self.session
                    .append(Entry::Message {
                        message: Arc::new(Message::User { content }),
                    })
                    .await
                    .map_err(|error| error.to_string())?;
            } else if !self.deliver_queued(queue, true, cancel).await? {
                return Ok(RunOutcome {
                    status: RunStatus::Completed,
                    response: None,
                });
            }
            self.run_loop(cancel, queue).await
        }
        .await;
        self.apply_next_model().await?;
        self.emit(Event::Phase(Phase::Idle), cancel).await;
        result
    }

    async fn deliver_queued(
        &self,
        queue: &queue::Queue,
        follow_up: bool,
        cancel: &CancellationToken,
    ) -> Result<bool, String> {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let Some(claim) = queue.claim(follow_up) else {
            return Ok(false);
        };
        self.apply_next_model().await?;
        self.validate_input(&claim.input().content)?;
        let id = claim.input().id;
        self.session
            .append(Entry::Message {
                message: Arc::new(Message::User {
                    content: claim.input().content.clone(),
                }),
            })
            .await
            .map_err(|error| error.to_string())?;
        claim.commit();
        self.emit(Event::QueuedInputConsumed { id }, cancel).await;
        Ok(true)
    }

    async fn run_loop(
        &self,
        cancel: &CancellationToken,
        queue: &queue::Queue,
    ) -> Result<RunOutcome, String> {
        let mut retries = 0;
        let mut overflow_recovered = false;
        let mut last_response = None;
        loop {
            if cancel.is_cancelled() {
                self.record_cancelled().await?;
                return Ok(RunOutcome::cancelled(last_response));
            }
            self.apply_next_model().await?;
            let snapshot = self.session.snapshot();
            let choice = snapshot.choice.as_ref().expect("initialized model");
            let (_, _, model) = self.config.model(&choice.qualified)?;
            if compaction::should_compact(
                snapshot.estimate.tokens(),
                model.context_window,
                &model.compaction,
            ) && let Err(error) = self.compact_inner("", cancel).await
            {
                self.emit(
                    Event::Notice(format!("Automatic compaction deferred: {error}")),
                    cancel,
                )
                .await;
            }
            let snapshot = self.session.snapshot();
            let choice = snapshot.choice.as_ref().expect("initialized model");
            let (provider, settings, model) = self.config.model(&choice.qualified)?;
            let messages = snapshot.messages();
            let (updates, bridge) = self.delta_bridge(false, cancel);
            self.emit(Event::Phase(Phase::Streaming), cancel).await;
            let outcome = self
                .http
                .run(
                    Request {
                        provider,
                        model,
                        instructions: &snapshot.prompt,
                        messages: &messages,
                        tools: &self.definitions,
                        estimated_input_tokens: snapshot.estimate.tokens(),
                        cache_key: Some(self.session.id()),
                    },
                    settings.resolve()?,
                    updates,
                    cancel,
                )
                .await;
            if let Some(bridge) = bridge {
                if outcome.failure == Some(Failure::Deadline) {
                    bridge.abort();
                }
                let _ = bridge.await;
            }
            let retryable = outcome.retryable();
            let retry_after = outcome.retry_after;
            let failure = outcome.failure.clone();
            let response = Arc::new(outcome.response);
            last_response = Some(response.clone());
            if failure == Some(Failure::ContextOverflow) && !overflow_recovered {
                overflow_recovered = true;
                match self.compact_inner("", cancel).await {
                    Ok(()) => {
                        self.record_usage(
                            provider,
                            &model.id,
                            UsageKind::ProviderAttempt,
                            &response,
                        )
                        .await?;
                        continue;
                    }
                    Err(error) => {
                        self.emit(
                            Event::Notice(format!(
                                "Context overflow could not be compacted: {error}"
                            )),
                            cancel,
                        )
                        .await;
                    }
                }
            } else if response.stop == StopReason::Error && retryable && retries < MAX_RETRIES {
                self.record_usage(provider, &model.id, UsageKind::ProviderAttempt, &response)
                    .await?;
                retries += 1;
                if !self
                    .retry_wait(RetryScope::Main, retries, retry_after, cancel)
                    .await
                {
                    self.record_cancelled().await?;
                    return Ok(RunOutcome::cancelled(Some(response.clone())));
                }
                continue;
            }
            self.session
                .append(Entry::Message {
                    message: Arc::new(Message::Assistant {
                        response: (*response).clone(),
                    }),
                })
                .await
                .map_err(|error| error.to_string())?;
            self.emit(Event::Assistant(response.clone()), cancel).await;
            if matches!(response.stop, StopReason::Error | StopReason::Aborted) {
                return Ok(RunOutcome::from_response(response));
            }
            retries = 0;
            overflow_recovered = false;
            let calls = response.tool_calls();
            if calls.is_empty() {
                let snapshot = self.session.snapshot();
                if !cancel.is_cancelled()
                    && compaction::should_compact(
                        snapshot.estimate.tokens(),
                        model.context_window,
                        &model.compaction,
                    )
                    && let Err(error) = self.compact_inner("", cancel).await
                {
                    self.emit(
                        Event::Notice(format!("Automatic compaction deferred: {error}")),
                        cancel,
                    )
                    .await;
                }
                if self.deliver_queued(queue, true, cancel).await? {
                    continue;
                }
                return Ok(RunOutcome::from_response(response));
            }
            self.emit(Event::Phase(Phase::Tools), cancel).await;
            let results = if response.stop == StopReason::Length {
                join_all(calls.iter().map(|call| {
                    self.tools
                        .reject(call, "Tool call was truncated; it was not executed", cancel)
                }))
                .await
            } else {
                join_all(calls.iter().map(|call| self.tools.execute(call, cancel))).await
            };
            for (call, result) in calls.into_iter().zip(results) {
                self.session
                    .append(Entry::Message {
                        message: Arc::new(Message::ToolResult {
                            call_id: call.id,
                            name: call.name,
                            result: (*result).clone(),
                        }),
                    })
                    .await
                    .map_err(|error| error.to_string())?;
            }
            if cancel.is_cancelled() {
                self.record_cancelled().await?;
                return Ok(RunOutcome::cancelled(Some(response)));
            }
            self.deliver_queued(queue, false, cancel).await?;
        }
    }

    fn delta_bridge(
        &self,
        side: bool,
        cancel: &CancellationToken,
    ) -> (
        Option<mpsc::Sender<Delta>>,
        Option<tokio::task::JoinHandle<()>>,
    ) {
        let Some(events) = &self.events else {
            return (None, None);
        };
        let events = events.clone();
        let cancel = cancel.clone();
        let (sender, mut receiver) = mpsc::channel(128);
        let bridge = tokio::spawn(async move {
            while let Some(delta) = receiver.recv().await {
                let event = match delta {
                    Delta::Text(text) if side => Event::SideText(text),
                    Delta::Text(text) => Event::Text(text),
                    Delta::Thinking(text) if !side => Event::Thinking(text),
                    Delta::ToolCallStarted { .. }
                    | Delta::ToolCallDelta { .. }
                    | Delta::ToolCallFinished { .. }
                        if side =>
                    {
                        continue;
                    }
                    Delta::ToolCallStarted {
                        output_index,
                        call_id,
                        name,
                    } => Event::StreamToolCallStarted {
                        output_index,
                        call_id,
                        name,
                    },
                    Delta::ToolCallDelta {
                        output_index,
                        delta,
                    } => Event::StreamToolCallDelta {
                        output_index,
                        delta,
                    },
                    Delta::ToolCallFinished { output_index } => {
                        Event::StreamToolCallFinished { output_index }
                    }
                    Delta::Thinking(_) => continue,
                };
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    sent = events.send(event) => if sent.is_err() { break },
                }
            }
        });
        (Some(sender), Some(bridge))
    }

    pub async fn compact(&self, focus: &str, cancel: &CancellationToken) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| "Another main operation is active")?;
        let result = self.compact_inner(focus, cancel).await;
        self.apply_next_model().await?;
        self.emit(Event::Phase(Phase::Idle), cancel).await;
        result
    }

    async fn compact_inner(&self, focus: &str, cancel: &CancellationToken) -> Result<(), String> {
        let snapshot = self.session.snapshot();
        let choice = snapshot.choice.as_ref().expect("initialized model");
        let (_, _, model) = self.config.model(&choice.qualified)?;
        let plan = compaction::prepare(&snapshot, &model.compaction).map_err(str::to_owned)?;
        self.emit(Event::Phase(Phase::Compacting), cancel).await;
        let history = if plan.history.is_empty() {
            plan.previous_summary.clone().unwrap_or_default()
        } else {
            self.summarize(plan.history_prompt(focus), choice, cancel)
                .await?
        };
        let prefix = if plan.turn_prefix.is_empty() {
            String::new()
        } else {
            self.summarize(plan.turn_prompt(focus), choice, cancel)
                .await?
        };
        self.session
            .append(Entry::Compaction {
                first_kept_id: Some(plan.first_kept_id),
                summary: Arc::new(plan.merge(&history, &prefix)),
                usage: None,
                tokens_before: Some(plan.tokens_before),
                read_files: Some(plan.read_files.clone()),
                modified_files: Some(plan.modified_files.clone()),
            })
            .await
            .map_err(|error| error.to_string())?;
        self.emit(Event::Notice("Context compacted".into()), cancel)
            .await;
        Ok(())
    }

    async fn summarize(
        &self,
        prompt: String,
        choice: &ModelChoice,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let (provider, settings, model) = self.config.model(&choice.qualified)?;
        let messages = [Message::user(prompt)];
        let mut retries = 0_u8;
        loop {
            let outcome = self
                .http
                .run(
                    Request {
                        provider,
                        model,
                        instructions: compaction::SUMMARY_SYSTEM,
                        messages: &messages,
                        tools: &[],
                        estimated_input_tokens: messages[0].estimate_tokens() + 100,
                        cache_key: None,
                    },
                    settings.resolve()?,
                    None,
                    cancel,
                )
                .await;
            self.record_usage(
                provider,
                &model.id,
                UsageKind::CompactionAttempt,
                &outcome.response,
            )
            .await?;
            if compaction::summary_is_complete(&outcome.response) {
                return Ok(outcome.response.text());
            }
            if outcome.retryable() && retries < MAX_RETRIES && !cancel.is_cancelled() {
                retries += 1;
                if !self
                    .retry_wait(RetryScope::Compaction, retries, outcome.retry_after, cancel)
                    .await
                {
                    break;
                }
                continue;
            }
            break;
        }
        Err("Compaction did not produce a complete summary; context was retained".into())
    }

    pub async fn side(
        &self,
        question: &str,
        thread: &btw::Thread,
        cancel: &CancellationToken,
    ) -> Result<Assistant, String> {
        let snapshot = self.session.snapshot();
        let choice = snapshot.choice.as_ref().expect("initialized model");
        let (provider, settings, model) = self.config.model(&choice.qualified)?;
        let messages = thread.messages(&snapshot, question);
        let instructions = format!("{}\n\n{}", snapshot.prompt, btw::SIDE_RULES);
        let estimate = messages.iter().map(Message::estimate_tokens).sum::<u64>()
            + (instructions.encode_utf16().count() as u64).div_ceil(4);
        let mut retries = 0_u8;
        loop {
            let (updates, bridge) = self.delta_bridge(true, cancel);
            let outcome = self
                .http
                .run(
                    Request {
                        provider,
                        model,
                        instructions: &instructions,
                        messages: &messages,
                        tools: &[],
                        estimated_input_tokens: estimate,
                        cache_key: None,
                    },
                    settings.resolve()?,
                    updates,
                    cancel,
                )
                .await;
            if let Some(bridge) = bridge {
                if outcome.failure == Some(Failure::Deadline) {
                    bridge.abort();
                }
                let _ = bridge.await;
            }
            self.record_usage(provider, &model.id, UsageKind::Btw, &outcome.response)
                .await?;
            if outcome.retryable() && retries < MAX_RETRIES && !cancel.is_cancelled() {
                retries += 1;
                self.emit(Event::SideRetry { attempt: retries }, cancel)
                    .await;
                if !self
                    .retry_wait(RetryScope::Side, retries, outcome.retry_after, cancel)
                    .await
                {
                    let mut response = outcome.response;
                    response.stop = StopReason::Aborted;
                    response.error = Some("Request aborted".into());
                    return Ok(response);
                }
                continue;
            }
            return Ok(outcome.response);
        }
    }

    async fn retry_wait(
        &self,
        scope: RetryScope,
        attempt: u8,
        provider: Option<Duration>,
        cancel: &CancellationToken,
    ) -> bool {
        let delay = retry_delay(provider, attempt);
        self.emit(
            Event::RetryScheduled {
                scope,
                attempt,
                max_retries: MAX_RETRIES,
                delay,
            },
            cancel,
        )
        .await;
        let ready = tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(delay) => true,
        };
        if ready {
            self.emit(Event::RetryStarted { scope }, cancel).await;
        }
        ready
    }

    async fn record_cancelled(&self) -> Result<(), String> {
        self.session
            .append(Entry::Cancelled)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn record_usage(
        &self,
        provider: &str,
        model: &str,
        kind: UsageKind,
        response: &Assistant,
    ) -> Result<(), String> {
        if let Some(usage) = &response.usage {
            self.session
                .append(Entry::Usage {
                    provider: provider.into(),
                    model: model.into(),
                    kind,
                    usage: usage.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn retry_delay(provider: Option<Duration>, attempt: u8) -> Duration {
    provider
        .unwrap_or_else(|| Duration::from_secs(2 << attempt.saturating_sub(1)))
        .min(Duration::from_secs(60))
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn retries_follow_pis_bounded_exponential_delays() {
        assert_eq!(MAX_RETRIES, 3);
        for (attempt, seconds) in [(1, 2), (2, 4), (3, 8)] {
            assert_eq!(retry_delay(None, attempt), Duration::from_secs(seconds));
        }
        assert_eq!(
            retry_delay(Some(Duration::from_secs(120)), 1),
            Duration::from_secs(60)
        );
    }
}
