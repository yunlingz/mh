//! OpenAI Responses over HTTP SSE only. No alternate transport or remote history.

pub mod proxy;
pub mod sse;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{Credentials, InputModality, ModelConfig};
use crate::message::{Assistant, Message, OutputItem, OutputText, StopReason, SummaryPart, Usage};
use crate::tools::ToolDefinition;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);

enum RequestEnd {
    Cancelled,
    Deadline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Network,
    Deadline,
    Protocol,
    Http(u16),
    ContextOverflow,
    TransientServer,
}

pub struct Outcome {
    pub response: Assistant,
    pub failure: Option<Failure>,
    pub retry_after: Option<Duration>,
}

impl Outcome {
    pub fn retryable(&self) -> bool {
        matches!(
            self.failure,
            Some(Failure::Network | Failure::TransientServer)
        )
    }
}

pub enum Delta {
    Text(String),
    Thinking(String),
    ToolCallStarted {
        output_index: usize,
        call_id: String,
        name: String,
    },
    ToolCallDelta {
        output_index: usize,
        delta: String,
    },
    ToolCallFinished {
        output_index: usize,
    },
}

#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub provider: &'a str,
    pub model: &'a ModelConfig,
    pub instructions: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub estimated_input_tokens: u64,
    pub cache_key: Option<&'a str>,
}

#[derive(Clone)]
pub struct Responses {
    client: reqwest::Client,
    proxy_status: proxy::Status,
}

impl Responses {
    pub fn new() -> Result<Self, String> {
        let (builder, proxy_status) = proxy::configure(reqwest::Client::builder())?;
        let client = builder
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(60))
            // The end-to-end deadline in run() also covers notification sends.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Cannot configure the HTTP client")?;
        Ok(Self {
            client,
            proxy_status,
        })
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            proxy_status: proxy::Status::CustomClient,
        }
    }

    pub fn proxy_status(&self) -> &proxy::Status {
        &self.proxy_status
    }

    pub async fn run(
        &self,
        request: Request<'_>,
        credentials: Credentials,
        updates: Option<mpsc::Sender<Delta>>,
        cancel: &CancellationToken,
    ) -> Outcome {
        let mut state = StreamState::default();
        let deadline = tokio::time::sleep(REQUEST_TIMEOUT);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(RequestEnd::Cancelled),
            _ = deadline => Err(RequestEnd::Deadline),
            result = self.run_inner(request, credentials, updates, cancel, &mut state) => Ok(result),
        };
        match result {
            Ok(outcome) => outcome,
            Err(RequestEnd::Cancelled) => state.aborted(&request),
            Err(RequestEnd::Deadline) => state.finish(
                &request,
                StopReason::Error,
                Some(Failure::Deadline),
                Some("Provider request exceeded the 15 minute limit; partial output was retained. Retry the request or choose another configured model."),
            ),
        }
    }

    async fn run_inner(
        &self,
        request: Request<'_>,
        credentials: Credentials,
        updates: Option<mpsc::Sender<Delta>>,
        cancel: &CancellationToken,
        state: &mut StreamState,
    ) -> Outcome {
        let body = match request_body(&request) {
            Ok(body) => body,
            Err(_) => {
                return state.finish(
                    &request,
                    StopReason::Error,
                    Some(Failure::ContextOverflow),
                    Some("Context window exceeded"),
                );
            }
        };
        let mut builder = self
            .client
            .post(credentials.endpoint)
            .bearer_auth(credentials.api_key)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(key) = request.cache_key {
            builder = builder
                .header("session-id", key)
                .header("x-client-request-id", key);
        }
        let send = builder.json(&body).send();
        let response = tokio::select! {
            _ = cancel.cancelled() => return state.aborted(&request),
            response = send => match response {
                Ok(response) => response,
                Err(_) => return state.finish(&request, StopReason::Error, Some(Failure::Network), Some("Provider connection failed or timed out")),
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let should_retry = response
                .headers()
                .get("x-should-retry")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| match value {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                });
            state.retry_after = retry_after(response.headers());
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => return state.aborted(&request),
                    next = stream.next() => next,
                };
                let Some(Ok(chunk)) = next else {
                    break;
                };
                if bytes.len() + chunk.len() > 64 * 1024 {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            let raw: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            state.usage = raw
                .get("usage")
                .and_then(|usage| serde_json::from_value::<RawUsage>(usage.clone()).ok())
                .map(RawUsage::normalized);
            let code = raw.pointer("/error/code").and_then(Value::as_str);
            let error_type = raw.pointer("/error/type").and_then(Value::as_str);
            let failure = classify_http(status, code, error_type, should_retry);
            if failure == Failure::ContextOverflow {
                return state.finish(
                    &request,
                    StopReason::Error,
                    Some(failure),
                    Some("Context window exceeded"),
                );
            }
            let message = format!(
                "Provider returned HTTP {status}; check provider access, model metadata, and token limits"
            );
            return state.finish(&request, StopReason::Error, Some(failure), Some(&message));
        }
        state.retry_after = retry_after(response.headers());
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return state.finish(
                &request,
                StopReason::Error,
                Some(Failure::Protocol),
                Some("Provider did not return an SSE stream"),
            );
        }
        let mut stream = response.bytes_stream();
        let mut decoder = sse::Decoder::default();
        let mut received = 0usize;
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return state.aborted(&request),
                next = stream.next() => next,
            };
            let chunk = match next {
                Some(Ok(chunk)) => chunk,
                Some(Err(_)) | None => {
                    return state.finish(
                        &request,
                        StopReason::Error,
                        Some(Failure::Network),
                        Some("SSE stream ended without a terminal response event"),
                    );
                }
            };
            received = received.saturating_add(chunk.len());
            if received > 64 * 1024 * 1024 {
                return state.finish(
                    &request,
                    StopReason::Error,
                    Some(Failure::Protocol),
                    Some("Response stream exceeds the 64 MiB limit"),
                );
            }
            let events = match decoder.push(&chunk) {
                Ok(events) => events,
                Err(_) => {
                    return state.finish(
                        &request,
                        StopReason::Error,
                        Some(Failure::Protocol),
                        Some("Invalid or oversized SSE frame"),
                    );
                }
            };
            for data in events {
                if data == "[DONE]" {
                    return state.finish(
                        &request,
                        StopReason::Error,
                        Some(Failure::Protocol),
                        Some("SSE stream lacked a terminal Responses event"),
                    );
                }
                let event: Event = match serde_json::from_str(&data) {
                    Ok(event) => event,
                    Err(_) => {
                        return state.finish(
                            &request,
                            StopReason::Error,
                            Some(Failure::Protocol),
                            Some("Invalid Responses SSE event"),
                        );
                    }
                };
                if event.output_index.is_some_and(|index| index >= 4096) {
                    return state.finish(
                        &request,
                        StopReason::Error,
                        Some(Failure::Protocol),
                        Some("Response contains too many output items"),
                    );
                }
                if let Some(usage) = event
                    .response
                    .as_ref()
                    .and_then(|response| response.usage.as_ref())
                {
                    state.usage = Some(usage.clone().normalized());
                }
                match event.kind.as_str() {
                    "response.output_text.delta" => {
                        if let Some(delta) = event.delta {
                            if state.text_bytes + delta.len() > 16 * 1024 * 1024 {
                                return state.finish(
                                    &request,
                                    StopReason::Error,
                                    Some(Failure::Protocol),
                                    Some("Response text exceeds the 16 MiB limit"),
                                );
                            }
                            state.text_bytes += delta.len();
                            state
                                .text
                                .entry(event.output_index)
                                .or_default()
                                .push_str(&delta);
                            if let Some(sender) = &updates {
                                tokio::select! {
                                    _ = cancel.cancelled() => return state.aborted(&request),
                                    _ = sender.send(Delta::Text(delta)) => {}
                                }
                            }
                        }
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        if let Some(delta) = event.delta {
                            if let Some(index) = event.output_index {
                                let thinking = state.thinking.entry(index).or_default();
                                if thinking.len() + delta.len() > 16 * 1024 * 1024 {
                                    return state.finish(
                                        &request,
                                        StopReason::Error,
                                        Some(Failure::Protocol),
                                        Some("Response reasoning exceeds the 16 MiB limit"),
                                    );
                                }
                                thinking.push_str(&delta);
                            }
                            if let Some(sender) = &updates {
                                tokio::select! {
                                    _ = cancel.cancelled() => return state.aborted(&request),
                                    _ = sender.send(Delta::Thinking(delta)) => {}
                                }
                            }
                        }
                    }
                    "response.reasoning_summary_part.done" => {
                        if let Some(index) = event.output_index {
                            state.thinking.entry(index).or_default().push_str("\n\n");
                        }
                        if let Some(sender) = &updates {
                            tokio::select! {
                                _ = cancel.cancelled() => return state.aborted(&request),
                                _ = sender.send(Delta::Thinking("\n\n".into())) => {}
                            }
                        }
                    }
                    "response.output_item.added" => {
                        if let (Some(index), Some(item)) = (event.output_index, event.item) {
                            if index >= 4096 || state.started_items.len() >= 4096 {
                                return state.finish(
                                    &request,
                                    StopReason::Error,
                                    Some(Failure::Protocol),
                                    Some("Response contains too many output items"),
                                );
                            }
                            if let OutputItem::FunctionCall {
                                call_id,
                                name,
                                arguments,
                                ..
                            } = &item
                            {
                                state.call_arguments.insert(index, arguments.clone());
                                if let Some(sender) = &updates {
                                    tokio::select! {
                                        _ = cancel.cancelled() => return state.aborted(&request),
                                        _ = sender.send(Delta::ToolCallStarted { output_index: index, call_id: call_id.clone(), name: name.clone() }) => {}
                                    }
                                }
                            }
                            // Added items are metadata sources, not completed tool calls.
                            state.started_items.insert(index, item);
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if let (Some(index), Some(delta)) = (event.output_index, event.delta) {
                            let arguments = state.call_arguments.entry(index).or_default();
                            if arguments.len() + delta.len() > 16 * 1024 * 1024 {
                                return state.finish(
                                    &request,
                                    StopReason::Error,
                                    Some(Failure::Protocol),
                                    Some("Tool arguments exceed the 16 MiB limit"),
                                );
                            }
                            arguments.push_str(&delta);
                            if let Some(sender) = &updates {
                                tokio::select! {
                                    _ = cancel.cancelled() => return state.aborted(&request),
                                    _ = sender.send(Delta::ToolCallDelta { output_index: index, delta }) => {}
                                }
                            }
                        }
                    }
                    "response.function_call_arguments.done" => {
                        if let (Some(index), Some(arguments)) =
                            (event.output_index, event.arguments)
                        {
                            if arguments.len() > 16 * 1024 * 1024 {
                                return state.finish(
                                    &request,
                                    StopReason::Error,
                                    Some(Failure::Protocol),
                                    Some("Tool arguments exceed the 16 MiB limit"),
                                );
                            }
                            state.call_arguments.insert(index, arguments);
                        }
                    }
                    "response.output_item.done" => {
                        if let (Some(index), Some(mut item)) = (event.output_index, event.item) {
                            if index >= 4096 {
                                return state.finish(
                                    &request,
                                    StopReason::Error,
                                    Some(Failure::Protocol),
                                    Some("Response contains too many output items"),
                                );
                            }
                            if let Some(started) = state.started_items.get(&index) {
                                retain_output_metadata(&mut item, started);
                            }
                            match &mut item {
                                OutputItem::FunctionCall {
                                    call_id, arguments, ..
                                } => {
                                    if !arguments.is_empty() {
                                        state.call_arguments.insert(index, arguments.clone());
                                    } else if let Some(streamed) =
                                        state.call_arguments.remove(&index)
                                    {
                                        *arguments = streamed;
                                    }
                                    state.completed_calls.insert(call_id.clone());
                                    if let Some(sender) = &updates {
                                        tokio::select! {
                                            _ = cancel.cancelled() => return state.aborted(&request),
                                            _ = sender.send(Delta::ToolCallFinished { output_index: index }) => {}
                                        }
                                    }
                                }
                                OutputItem::Reasoning {
                                    summary, content, ..
                                } if summary.is_empty() && content.is_empty() => {
                                    if let Some(text) = state.thinking.remove(&index)
                                        && !text.is_empty()
                                    {
                                        content.push(SummaryPart {
                                            kind: "reasoning_text".into(),
                                            text: text.trim_end_matches("\n\n").to_owned(),
                                        });
                                    }
                                }
                                _ => {}
                            }
                            state.items.insert(index, item);
                            if state.items.len() > 4096 {
                                return state.finish(
                                    &request,
                                    StopReason::Error,
                                    Some(Failure::Protocol),
                                    Some("Response contains too many output items"),
                                );
                            }
                        }
                    }
                    "response.completed" | "response.incomplete" | "response.failed" => {
                        let Some(response) = event.response else {
                            return state.finish(
                                &request,
                                StopReason::Error,
                                Some(Failure::Protocol),
                                Some("Terminal SSE event has no response"),
                            );
                        };
                        if response.output.len() > 4096 {
                            return state.finish(
                                &request,
                                StopReason::Error,
                                Some(Failure::Protocol),
                                Some("Response contains too many output items"),
                            );
                        }
                        for (index, mut item) in response.output.into_iter().enumerate() {
                            if let Some(streamed) = state.items.get(&index) {
                                retain_output_metadata(&mut item, streamed);
                                if let (
                                    OutputItem::FunctionCall {
                                        call_id, arguments, ..
                                    },
                                    OutputItem::FunctionCall {
                                        call_id: old_call,
                                        arguments: old_arguments,
                                        ..
                                    },
                                ) = (&mut item, streamed)
                                    && call_id == old_call
                                    && arguments.is_empty()
                                {
                                    *arguments = old_arguments.clone();
                                }
                            }
                            if let Some(started) = state.started_items.get(&index) {
                                retain_output_metadata(&mut item, started);
                            }
                            state.items.insert(index, item);
                        }
                        if event.kind == "response.completed"
                            && state.items.values().any(|item| {
                                matches!(item, OutputItem::FunctionCall { call_id, .. }
                                    if !state.completed_calls.contains(call_id))
                            })
                        {
                            return state.finish(
                                &request,
                                StopReason::Error,
                                Some(Failure::Protocol),
                                Some("Provider completed with an unfinished tool call"),
                            );
                        }
                        if event.kind == "response.failed" {
                            let code = response
                                .error
                                .as_ref()
                                .and_then(|error| error.code.as_deref());
                            let error_type = response
                                .error
                                .as_ref()
                                .and_then(|error| error.kind.as_deref());
                            let failure = classify_error(code, error_type);
                            return state.finish(
                                &request,
                                StopReason::Error,
                                Some(failure),
                                Some("Provider reported a failed response"),
                            );
                        }
                        if event.kind == "response.incomplete" {
                            let reason = response
                                .incomplete_details
                                .as_ref()
                                .and_then(|details| details.reason.as_deref());
                            if reason != Some("max_output_tokens") {
                                return state.finish(
                                    &request,
                                    StopReason::Error,
                                    Some(classify_error(reason, None)),
                                    Some("Provider returned an incomplete response"),
                                );
                            }
                        }
                        let stop = if event.kind == "response.incomplete" {
                            StopReason::Length
                        } else if state
                            .items
                            .values()
                            .any(|item| matches!(item, OutputItem::FunctionCall { .. }))
                        {
                            StopReason::ToolUse
                        } else {
                            StopReason::Stop
                        };
                        if request.tools.is_empty()
                            && state
                                .items
                                .values()
                                .any(|item| matches!(item, OutputItem::FunctionCall { .. }))
                        {
                            return state.finish(
                                &request,
                                StopReason::Error,
                                Some(Failure::Protocol),
                                Some("Tool-free request attempted a tool call"),
                            );
                        }
                        return state.finish(&request, stop, None, None);
                    }
                    "error" => {
                        let code = event.code.as_deref().or_else(|| {
                            event.error.as_ref().and_then(|error| error.code.as_deref())
                        });
                        let error_type =
                            event.error.as_ref().and_then(|error| error.kind.as_deref());
                        return state.finish(
                            &request,
                            StopReason::Error,
                            Some(classify_error(code, error_type)),
                            Some("Provider reported an SSE error"),
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

pub fn request_body(request: &Request<'_>) -> Result<Value, &'static str> {
    let available = request
        .model
        .context_window
        .saturating_sub(request.estimated_input_tokens);
    let output_tokens = request.model.max_tokens.min(available);
    if output_tokens < 16 {
        return Err("No output space in context window");
    }
    let images = request.model.input.contains(&InputModality::Image);
    let input: Vec<_> = request
        .messages
        .iter()
        .enumerate()
        .flat_map(|(index, message)| {
            message.to_wire(index, request.provider, &request.model.id, images)
        })
        .collect();
    let mut body = json!({
        "model":request.model.id, "instructions":request.instructions,
        "input":input, "stream":true, "store":false, "max_output_tokens":output_tokens,
    });
    if let Some(key) = request.cache_key {
        body["prompt_cache_key"] = json!(key);
    }
    if request.model.reasoning {
        let effort = request.model.reasoning_effort.as_deref();
        let mut reasoning = json!({});
        if let Some(effort) = effort {
            reasoning["effort"] = json!(effort);
        }
        // Stateless reasoning is opaque continuation data even for a fixed
        // none/default effort. Explicit include is legacy-compatible.
        body["include"] = json!(["reasoning.encrypted_content"]);
        if effort != Some("none") {
            reasoning["summary"] = json!("auto");
        }
        if reasoning
            .as_object()
            .is_some_and(|fields| !fields.is_empty())
        {
            body["reasoning"] = reasoning;
        }
    }
    if request.tools.is_empty() {
        body["tool_choice"] = json!("none");
    } else {
        body["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| json!({
                    "type":"function", "name":tool.name, "description":tool.description,
                    "parameters":tool.parameters, "strict":false
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(body)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    retry_after_at(headers, std::time::SystemTime::now())
}

fn retry_after_at(
    headers: &reqwest::header::HeaderMap,
    now: std::time::SystemTime,
) -> Option<Duration> {
    let bounded = |value: f64, scale: f64| {
        (value.is_finite() && value >= 0.0)
            .then(|| Duration::from_secs_f64((value / scale).min(60.0)))
    };
    if let Some(milliseconds) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .and_then(|value| bounded(value, 1000.0))
    {
        return Some(milliseconds);
    }
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<f64>() {
        return bounded(seconds, 1.0);
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(
        date.duration_since(now)
            .unwrap_or_default()
            .min(Duration::from_secs(60)),
    )
}

fn context_error(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("context_length_exceeded" | "context_window_exceeded" | "prompt_too_long")
    )
}

fn classify_http(
    status: u16,
    code: Option<&str>,
    error_type: Option<&str>,
    should_retry: Option<bool>,
) -> Failure {
    if status == 413 || context_error(code) {
        Failure::ContextOverflow
    } else if quota_error(code, error_type) || should_retry == Some(false) {
        Failure::Http(status)
    } else if should_retry == Some(true) || matches!(status, 408 | 409 | 429) || status >= 500 {
        Failure::TransientServer
    } else {
        Failure::Http(status)
    }
}

fn classify_error(code: Option<&str>, error_type: Option<&str>) -> Failure {
    if context_error(code) {
        Failure::ContextOverflow
    } else if quota_error(code, error_type) {
        Failure::Protocol
    } else if [code, error_type].into_iter().flatten().any(|value| {
        matches!(
            value,
            "server_error"
                | "rate_limit_exceeded"
                | "temporarily_unavailable"
                | "overloaded"
                | "api_error"
                | "upstream_request_failed"
                | "upstream_stream_read_error"
                | "subscription_sharing_usage_unavailable"
                | "subscription_sharing_user_unavailable"
        )
    }) {
        Failure::TransientServer
    } else {
        Failure::Protocol
    }
}

fn quota_error(code: Option<&str>, error_type: Option<&str>) -> bool {
    [code, error_type].into_iter().flatten().any(|value| {
        matches!(
            value,
            "insufficient_quota"
                | "billing_hard_limit_reached"
                | "usage_limit_reached"
                | "subscription_sharing_usage_limit_exceeded"
                | "subscription_sharing_workplace_individual_usage_limit_exceeded"
                | "GoUsageLimitError"
                | "FreeUsageLimitError"
        )
    })
}

#[derive(Default)]
struct StreamState {
    text: BTreeMap<Option<usize>, String>,
    text_bytes: usize,
    thinking: BTreeMap<usize, String>,
    call_arguments: BTreeMap<usize, String>,
    items: BTreeMap<usize, OutputItem>,
    started_items: BTreeMap<usize, OutputItem>,
    completed_calls: BTreeSet<String>,
    usage: Option<Usage>,
    retry_after: Option<Duration>,
}

impl StreamState {
    fn aborted(&mut self, request: &Request<'_>) -> Outcome {
        self.finish(request, StopReason::Aborted, None, Some("Request aborted"))
    }

    fn finish(
        &mut self,
        request: &Request<'_>,
        mut stop: StopReason,
        mut failure: Option<Failure>,
        mut error: Option<&str>,
    ) -> Outcome {
        for (index, mut text) in std::mem::take(&mut self.text) {
            if index.is_some_and(|index| {
                matches!(self.items.get(&index), Some(OutputItem::Message { .. }))
            }) {
                continue; // A completed message is authoritative for this item.
            }
            if index.is_none() {
                // Compatibility for streams omitting output_index: remove only
                // a known completed prefix, never suppress text behind a tool
                // or reasoning item. Indexed streams need no such inference.
                let completed = self
                    .items
                    .values()
                    .filter_map(|item| match item {
                        OutputItem::Message { content, .. } => Some(content),
                        _ => None,
                    })
                    .flatten()
                    .map(|part| match part {
                        OutputText::OutputText { text, .. } => text.as_str(),
                        OutputText::Refusal { refusal } => refusal.as_str(),
                    })
                    .collect::<String>();
                if let Some(remaining) = text.strip_prefix(&completed) {
                    text = remaining.to_owned();
                }
            }
            if text.is_empty() {
                continue;
            }
            let next = self
                .items
                .last_key_value()
                .map_or(0, |(index, _)| index + 1);
            let slot = index
                .filter(|index| !self.items.contains_key(index))
                .unwrap_or(next);
            let (id, phase) = match index.and_then(|index| self.started_items.get(&index)) {
                Some(OutputItem::Message { id, phase, .. }) => (id.clone(), phase.clone()),
                _ => (None, None),
            };
            self.items.insert(
                slot,
                OutputItem::Message {
                    id,
                    phase,
                    content: vec![OutputText::OutputText {
                        text,
                        annotations: Vec::new(),
                    }],
                },
            );
        }
        if failure.is_none()
            && let Some(message) = output_error(self.items.values())
        {
            stop = StopReason::Error;
            failure = Some(Failure::Protocol);
            error = Some(message);
        }
        Outcome {
            response: Assistant {
                provider: request.provider.into(),
                model: request.model.id.clone(),
                items: std::mem::take(&mut self.items).into_values().collect(),
                stop,
                usage: self.usage.take(),
                error: error.map(str::to_owned),
            },
            failure,
            retry_after: self.retry_after,
        }
    }
}

/// Completion events may omit metadata already sent in item events. Only merge
/// matching item identities: reasoning blobs must never move to another item.
fn retain_output_metadata(item: &mut OutputItem, previous: &OutputItem) {
    match (item, previous) {
        (
            OutputItem::Message { id, phase, content },
            OutputItem::Message {
                id: old_id,
                phase: old_phase,
                content: old_content,
            },
        ) if id.is_none() || old_id.is_none() || id == old_id => {
            if id.is_none() {
                *id = old_id.clone();
            }
            if phase.is_none() {
                *phase = old_phase.clone();
            }
            if content.is_empty() {
                *content = old_content.clone();
            }
        }
        (
            OutputItem::Reasoning {
                id,
                summary,
                content,
                encrypted_content,
            },
            OutputItem::Reasoning {
                id: old_id,
                summary: old_summary,
                content: old_content,
                encrypted_content: old_encrypted,
            },
        ) if id == old_id => {
            if summary.is_empty() && content.is_empty() {
                *summary = old_summary.clone();
                *content = old_content.clone();
            }
            if encrypted_content.is_none() {
                *encrypted_content = old_encrypted.clone();
            }
        }
        (
            OutputItem::FunctionCall { id, call_id, .. },
            OutputItem::FunctionCall {
                id: old_id,
                call_id: old_call,
                ..
            },
        ) if call_id == old_call && id.is_none() => {
            *id = old_id.clone();
        }
        _ => {}
    }
}

fn output_error<'a>(items: impl Iterator<Item = &'a OutputItem>) -> Option<&'static str> {
    let mut call_ids = BTreeSet::new();
    for item in items {
        if let OutputItem::FunctionCall { call_id, name, .. } = item
            && (call_id.is_empty() || name.is_empty() || !call_ids.insert(call_id))
        {
            return Some("Provider returned invalid or duplicate tool call metadata");
        }
    }
    None
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    item: Option<OutputItem>,
    #[serde(default)]
    response: Option<RawResponse>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    output: Vec<OutputItem>,
    #[serde(default)]
    usage: Option<RawUsage>,
    #[serde(default)]
    error: Option<RawError>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
}

#[derive(Clone, Default, Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

impl RawUsage {
    fn normalized(self) -> Usage {
        let details = self.input_tokens_details.unwrap_or_default();
        Usage {
            input: self
                .input_tokens
                .saturating_sub(details.cached_tokens)
                .saturating_sub(details.cache_write_tokens),
            cache_read: details.cached_tokens,
            cache_write: details.cache_write_tokens,
            output: self.output_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    use super::{Failure, classify_error, classify_http, retry_after, retry_after_at};

    #[test]
    fn retry_delays_are_bounded_before_duration_conversion() {
        let mut headers = HeaderMap::new();
        for value in ["1e100", "60.5", "18446744073709551616"] {
            headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
            assert_eq!(retry_after(&headers), Some(Duration::from_secs(60)));
        }
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12.5"));
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(12_500)));
        headers.insert(
            "retry-after-ms",
            HeaderValue::from_static("18446744073709551615"),
        );
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(60)));
    }

    #[test]
    fn retry_delay_headers_support_http_dates_and_fractional_milliseconds() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(now + Duration::from_secs(12))).unwrap(),
        );
        assert_eq!(retry_after_at(&headers, now), Some(Duration::from_secs(12)));
        headers.insert("retry-after-ms", HeaderValue::from_static("12.5"));
        assert_eq!(
            retry_after_at(&headers, now),
            Some(Duration::from_micros(12_500))
        );
        headers.remove("retry-after-ms");
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(now - Duration::from_secs(12))).unwrap(),
        );
        assert_eq!(retry_after_at(&headers, now), Some(Duration::ZERO));
    }

    #[test]
    fn retry_classification_honors_headers_and_account_limits() {
        assert_eq!(
            classify_http(429, Some("other"), Some("insufficient_quota"), Some(true)),
            Failure::Http(429)
        );
        assert_eq!(
            classify_http(503, None, None, Some(false)),
            Failure::Http(503)
        );
        assert_eq!(
            classify_http(409, None, None, None),
            Failure::TransientServer
        );
        assert_eq!(
            classify_error(Some("other"), Some("api_error")),
            Failure::TransientServer
        );
        assert_eq!(
            classify_error(Some("other"), Some("insufficient_quota")),
            Failure::Protocol
        );
    }

    #[test]
    fn invalid_retry_delays_are_ignored() {
        let mut headers = HeaderMap::new();
        for value in ["-1", "NaN", "inf", "not-a-number"] {
            headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
            assert_eq!(retry_after(&headers), None);
        }
    }
}
