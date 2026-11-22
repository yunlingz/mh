use mh::config::{Config, Credentials, ModelConfig};
use mh::message::{Assistant, Content, Message, OutputItem, OutputText, StopReason, ToolResult};
use mh::providers::{Failure, Request, Responses, request_body};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn model() -> ModelConfig {
    let config = Config::parse(&json!({
        "defaultModel":"test/model", "providers":{"test":{
            "api":"openai-responses-sse", "base_url_env":"TEST_BASE", "api_key_env":"TEST_KEY",
            "models":[{"id":"model", "reasoning":true, "reasoningEffort":"low", "input":["text","image"],
                "contextWindow":64000,"maxTokens":1024,
                "compaction":{"reserveTokens":2048,"keepRecentTokens":4000}}]
        }}
    }).to_string()).unwrap();
    config.providers["test"].models[0].clone()
}

async fn server(
    status: u16,
    content_type: &str,
    body: String,
    hold: bool,
) -> (Credentials, oneshot::Receiver<Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = oneshot::channel();
    let content_type = content_type.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let header_end;
        loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = end + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() - header_end < length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        let mut request: Value =
            serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
        request["_test_session_id"] = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("session-id:")
                    .or_else(|| line.strip_prefix("Session-Id:"))
                    .map(str::trim)
            })
            .map_or(Value::Null, |value| json!(value));
        request["_test_client_request_id"] = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("x-client-request-id:")
                    .or_else(|| line.strip_prefix("X-Client-Request-Id:"))
                    .map(str::trim)
            })
            .map_or(Value::Null, |value| json!(value));
        let _ = sender.send(request);
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status} Result\r\nContent-Type: {content_type}\r\nRetry-After: 0.01\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        for chunk in body.as_bytes().chunks(3) {
            if socket.write_all(chunk).await.is_err() {
                return;
            }
        }
        if hold {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
    (
        Credentials {
            endpoint: url::Url::parse(&format!("http://{address}/responses")).unwrap(),
            api_key: "test-key".into(),
        },
        receiver,
    )
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn client() -> Responses {
    Responses::with_client(reqwest::Client::builder().no_proxy().build().unwrap())
}

fn request<'a>(model: &'a ModelConfig, messages: &'a [Message]) -> Request<'a> {
    Request {
        provider: "test",
        model,
        instructions: "fixed instructions",
        messages,
        tools: &[],
        estimated_input_tokens: 100,
        cache_key: Some("session"),
    }
}

#[tokio::test]
async fn request_deadline_includes_backpressured_stream_notifications() {
    let body = event(json!({"type":"response.output_text.delta","delta":"first"}))
        + &event(json!({"type":"response.output_text.delta","delta":"second"}))
        + &event(json!({"type":"response.output_text.delta","delta":"third"}));
    let (credentials, _) = server(200, "text/event-stream", body, true).await;
    let (updates, mut receiver) = tokio::sync::mpsc::channel(1);
    let task = tokio::spawn(async move {
        let model = model();
        let client = Responses::with_client(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(900))
                .build()
                .unwrap(),
        );
        client
            .run(
                request(&model, &[]),
                credentials,
                Some(updates),
                &CancellationToken::new(),
            )
            .await
    });
    assert!(receiver.recv().await.is_some());
    while receiver.is_empty() {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(900)).await;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect(
            "the entire provider request must stop at its deadline even when UI updates are full",
        )
        .unwrap();
    assert_eq!(outcome.response.stop, StopReason::Error);
    assert_eq!(outcome.failure, Some(Failure::Deadline));
    assert!(
        !outcome.retryable(),
        "a long generation deadline must not restart the entire wait automatically"
    );
    assert!(outcome.response.text().starts_with("first"));
}

#[tokio::test]
async fn request_deadline_also_bounds_waiting_for_response_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started, received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        assert!(socket.read(&mut buffer).await.unwrap() > 0);
        started.send(()).unwrap();
        // Keep the connection open without returning headers.
        std::future::pending::<()>().await;
        drop(socket);
    });
    let task = tokio::spawn(async move {
        let model = model();
        client()
            .run(
                request(&model, &[]),
                Credentials {
                    endpoint: url::Url::parse(&format!("http://{address}/responses")).unwrap(),
                    api_key: "fixture".into(),
                },
                None,
                &CancellationToken::new(),
            )
            .await
    });
    received.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(900)).await;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("waiting for headers must be bounded")
        .unwrap();
    assert_eq!(outcome.failure, Some(Failure::Deadline));
    assert_eq!(outcome.response.stop, StopReason::Error);
    assert!(!outcome.retryable());
    server.abort();
}

#[tokio::test]
async fn cancellation_wins_over_deadline_with_backpressured_updates() {
    let body = event(json!({"type":"response.output_text.delta","delta":"first"}))
        + &event(json!({"type":"response.output_text.delta","delta":"second"}))
        + &event(json!({"type":"response.output_text.delta","delta":"third"}));
    let (credentials, _) = server(200, "text/event-stream", body, true).await;
    let (updates, mut receiver) = tokio::sync::mpsc::channel(1);
    let cancel = CancellationToken::new();
    let request_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let model = model();
        client()
            .run(
                request(&model, &[]),
                credentials,
                Some(updates),
                &request_cancel,
            )
            .await
    });
    assert!(receiver.recv().await.is_some());
    while receiver.is_empty() {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    cancel.cancel();
    tokio::time::advance(std::time::Duration::from_secs(900)).await;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("cancellation must unblock stream notifications")
        .unwrap();
    assert_eq!(outcome.response.stop, StopReason::Aborted);
    assert_eq!(outcome.failure, None);
    assert!(outcome.response.text().starts_with("first"));
    assert!(!outcome.retryable());
}

#[tokio::test]
async fn sse_streaming_preserves_text_and_normalizes_usage() {
    let body = event(json!({"type":"response.output_text.delta","delta":"Hello 🙂"}))
        + &event(json!({"type":"response.completed","response":{
            "output":[{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"Hello 🙂","annotations":[]}]}],
            "usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":60,"cache_write_tokens":10}}
        }}));
    let (credentials, received) = server(200, "text/event-stream", body, false).await;
    let model = model();
    let messages = [Message::user("hello")];
    let outcome = client()
        .run(
            request(&model, &messages),
            credentials,
            None,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome.response.stop, StopReason::Stop);
    assert_eq!(outcome.response.text(), "Hello 🙂");
    let usage = outcome.response.usage.unwrap();
    assert_eq!(
        (
            usage.input,
            usage.cache_read,
            usage.cache_write,
            usage.output
        ),
        (30, 60, 10, 20)
    );
    assert_eq!(usage.cache_percent(), Some(60.0));
    let sent = received.await.unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["store"], false);
    assert_eq!(sent["tool_choice"], "none");
    assert_eq!(sent["reasoning"]["effort"], "low");
    assert_eq!(sent["include"][0], "reasoning.encrypted_content");
    assert_eq!(sent["_test_session_id"], "session");
    assert_eq!(sent["_test_client_request_id"], "session");
    assert!(sent.get("api").is_none());
}

#[tokio::test]
async fn fixed_none_effort_is_explicit_and_distinct_from_provider_default() {
    let body = event(json!({"type":"response.completed","response":{"output":[]}}));
    let (credentials, received) = server(200, "text/event-stream", body, false).await;
    let mut model = model();
    model.reasoning_effort = Some("none".into());
    let request = request(&model, &[]);
    let outcome = client()
        .run(request, credentials, None, &CancellationToken::new())
        .await;
    assert_eq!(outcome.response.stop, StopReason::Stop);
    let sent = received.await.unwrap();
    assert_eq!(sent["reasoning"]["effort"], "none");
    assert!(sent["reasoning"].get("summary").is_none());
    assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn non_reasoning_model_has_no_effort_or_reasoning_metadata() {
    let mut model = model();
    model.reasoning = false;
    model.reasoning_effort = None;
    let body = request_body(&request(&model, &[])).unwrap();
    assert!(body.get("reasoning").is_none());
    assert!(body.get("include").is_none());
    assert_eq!(model.effort_label(), "off");
}

#[test]
fn supported_reasoning_without_explicit_effort_keeps_continuation_fields() {
    let mut model = model();
    model.reasoning_effort = None;
    let body = request_body(&request(&model, &[])).unwrap();
    assert!(body["reasoning"].get("effort").is_none());
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
}

#[tokio::test]
async fn unfinished_answers_survive_completed_reasoning_tools_and_other_messages() {
    for previous in [
        json!({"type":"reasoning","id":"r","summary":[],"encrypted_content":"opaque-fixture"}),
        json!({"type":"function_call","id":"fc","call_id":"call","name":"read","arguments":"{}"}),
        json!({"type":"message","id":"first","content":[{"type":"output_text","text":"complete ","annotations":[]}]}),
    ] {
        let body = [
            event(json!({"type":"response.output_item.done","output_index":0,"item":previous})),
            event(json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"partial-id","phase":"final_answer","content":[]}})),
            event(json!({"type":"response.output_text.delta","output_index":1,"delta":"partial answer"})),
        ].concat();
        let (credentials, _) = server(200, "text/event-stream", body, false).await;
        let model = model();
        let outcome = client()
            .run(
                request(&model, &[]),
                credentials,
                None,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.response.stop, StopReason::Error);
        assert!(outcome.response.text().ends_with("partial answer"));
        assert!(
            matches!(outcome.response.items.last(), Some(OutputItem::Message { id: Some(id), phase: Some(phase), .. }) if id == "partial-id" && phase == "final_answer")
        );
    }
}

#[tokio::test]
async fn non_sse_and_missing_terminal_events_are_not_successes() {
    for (kind, body) in [
        ("application/json", "{}".into()),
        (
            "text/event-stream",
            event(json!({"type":"response.output_text.delta","delta":"partial"})),
        ),
    ] {
        let (credentials, _) = server(200, kind, body, false).await;
        let model = model();
        let outcome = client()
            .run(
                request(&model, &[]),
                credentials,
                None,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.response.stop, StopReason::Error);
        assert!(outcome.failure.is_some());
    }
}

#[tokio::test]
async fn provider_error_bodies_do_not_leak_into_diagnostics() {
    let (credentials, _) = server(
        401,
        "application/json",
        json!({"error":{"message":"DO_NOT_ECHO_PRIVATE_VALUE"}}).to_string(),
        false,
    )
    .await;
    let model = model();
    let outcome = client()
        .run(
            request(&model, &[]),
            credentials,
            None,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome.failure, Some(Failure::Http(401)));
    assert!(
        !outcome
            .response
            .error
            .unwrap()
            .contains("DO_NOT_ECHO_PRIVATE_VALUE")
    );
}

#[tokio::test]
async fn context_overflow_is_classified_without_raw_error_text() {
    let (credentials, _) = server(
        400,
        "application/json",
        json!({"error":{"code":"context_length_exceeded","message":"private body"}}).to_string(),
        false,
    )
    .await;
    let model = model();
    let outcome = client()
        .run(
            request(&model, &[]),
            credentials,
            None,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome.failure, Some(Failure::ContextOverflow));
}

#[tokio::test]
async fn http_retry_classification_distinguishes_overflow_quota_and_throttling() {
    for (status, body, failure, retryable) in [
        (
            413,
            json!({"error":{"message":"private"}}),
            Failure::ContextOverflow,
            false,
        ),
        (
            429,
            json!({"error":{"code":"insufficient_quota","message":"private"}}),
            Failure::Http(429),
            false,
        ),
        (
            429,
            json!({"error":{"code":"rate_limit_exceeded","message":"private"}}),
            Failure::TransientServer,
            true,
        ),
        (
            503,
            json!({"error":{"type":"api_error","message":"private"}}),
            Failure::TransientServer,
            true,
        ),
    ] {
        let (credentials, _) = server(status, "application/json", body.to_string(), false).await;
        let model = model();
        let outcome = client()
            .run(
                request(&model, &[]),
                credentials,
                None,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.failure, Some(failure));
        assert_eq!(outcome.retryable(), retryable);
        assert_eq!(
            outcome.retry_after,
            Some(std::time::Duration::from_millis(10))
        );
        assert!(
            !outcome
                .response
                .error
                .unwrap_or_default()
                .contains("private")
        );
    }
}

#[tokio::test]
async fn cancellation_only_uses_the_request_token() {
    let (credentials, _) = server(
        200,
        "text/event-stream",
        event(json!({"type":"response.output_text.delta","delta":"partial"})),
        true,
    )
    .await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        trigger.cancel();
    });
    let model = model();
    let outcome = client()
        .run(request(&model, &[]), credentials, None, &cancel)
        .await;
    assert_eq!(outcome.response.stop, StopReason::Aborted);
    assert!(!outcome.retryable());
}

#[tokio::test]
async fn output_truncation_keeps_calls_for_safe_failure_handling() {
    let body = event(json!({"type":"response.incomplete","response":{
        "incomplete_details":{"reason":"max_output_tokens"},
        "output":[{
            "type":"function_call","id":"fc_1","call_id":"call_1","name":"write","arguments":"{\"path\":\"f\""
        }],"usage":{"input_tokens":100,"output_tokens":1024}
    }}));
    let (credentials, _) = server(200, "text/event-stream", body, false).await;
    let model = model();
    let tools = mh::tools::definitions();
    let mut request = request(&model, &[]);
    request.tools = &tools;
    let outcome = client()
        .run(request, credentials, None, &CancellationToken::new())
        .await;
    assert_eq!(outcome.response.stop, StopReason::Length);
    assert_eq!(outcome.response.tool_calls().len(), 1);
}

#[tokio::test]
async fn completed_stream_rejects_tool_calls_without_output_item_done() {
    let call = json!({
        "type":"function_call","id":"fc_1","call_id":"call_1",
        "name":"bash","arguments":"{\"command\":\"echo incomplete"
    });
    let body = event(json!({"type":"response.output_item.added","output_index":0,"item":call}))
        + &event(json!({
            "type":"response.function_call_arguments.delta","output_index":0,
            "delta":"{\"command\":\"echo incomplete"
        }))
        + &event(json!({"type":"response.completed","response":{"output":[call]}}));
    let (credentials, _) = server(200, "text/event-stream", body, false).await;
    let model = model();
    let tools = mh::tools::definitions();
    let mut request = request(&model, &[]);
    request.tools = &tools;
    let outcome = client()
        .run(request, credentials, None, &CancellationToken::new())
        .await;
    assert_eq!(outcome.response.stop, StopReason::Error);
    assert_eq!(outcome.failure, Some(Failure::Protocol));
    assert_eq!(
        outcome.response.error.as_deref(),
        Some("Provider completed with an unfinished tool call")
    );
}

#[tokio::test]
async fn completed_stream_accepts_only_finished_tool_calls() {
    let call = json!({
        "type":"function_call","id":"fc_1","call_id":"call_1",
        "name":"bash","arguments":"{\"command\":\"echo safe\"}"
    });
    let body = event(json!({
        "type":"response.output_item.done","output_index":0,"item":call
    })) + &event(json!({"type":"response.completed","response":{"output":[call]}}));
    let (credentials, _) = server(200, "text/event-stream", body, false).await;
    let model = model();
    let tools = mh::tools::definitions();
    let mut request = request(&model, &[]);
    request.tools = &tools;
    let outcome = client()
        .run(request, credentials, None, &CancellationToken::new())
        .await;
    assert_eq!(outcome.response.stop, StopReason::ToolUse);
    assert_eq!(outcome.failure, None);
}

#[tokio::test]
async fn streamed_tool_arguments_are_finalized_from_argument_events() {
    let added = json!({
        "type":"function_call","id":"fc_1","call_id":"call_1",
        "name":"bash","arguments":"{"
    });
    let mut done = added.clone();
    done["arguments"] = json!("");
    let body = event(json!({
        "type":"response.output_item.added","output_index":0,"item":added
    })) + &event(json!({
        "type":"response.function_call_arguments.delta","output_index":0,
        "delta":"\"command\":\"echo "
    })) + &event(json!({
        "type":"response.function_call_arguments.done","output_index":0,
        "arguments":"{\"command\":\"echo safe\"}"
    })) + &event(json!({
        "type":"response.output_item.done","output_index":0,"item":done
    })) + &event(json!({"type":"response.completed","response":{"output":[]}}));
    let (credentials, _) = server(200, "text/event-stream", body, false).await;
    let model = model();
    let tools = mh::tools::definitions();
    let mut request = request(&model, &[]);
    request.tools = &tools;
    let outcome = client()
        .run(request, credentials, None, &CancellationToken::new())
        .await;
    assert_eq!(outcome.response.stop, StopReason::ToolUse);
    assert_eq!(
        outcome.response.tool_calls()[0].arguments,
        "{\"command\":\"echo safe\"}"
    );
}

#[tokio::test]
async fn streamed_reasoning_is_retained_when_done_item_has_no_text() {
    let body = event(json!({
        "type":"response.output_item.added","output_index":0,
        "item":{"type":"reasoning","id":"rs_1","summary":[]}
    })) + &event(json!({
        "type":"response.reasoning_text.delta","output_index":0,"delta":"kept reasoning"
    })) + &event(json!({
        "type":"response.output_item.done","output_index":0,
        "item":{"type":"reasoning","id":"rs_1","summary":[]}
    })) + &event(json!({"type":"response.completed","response":{"output":[]}}));
    let (credentials, _) = server(200, "text/event-stream", body, false).await;
    let model = model();
    let outcome = client()
        .run(
            request(&model, &[]),
            credentials,
            None,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome.response.stop, StopReason::Stop);
    assert_eq!(outcome.response.thinking(), "kept reasoning");
}

#[tokio::test]
async fn stateless_sse_replays_opaque_reasoning_and_phase_in_output_order() {
    let reasoning =
        json!({"type":"reasoning","id":"rs_first","summary":[],"encrypted_content":"opaque-first"});
    let commentary = json!({"type":"message","id":"msg_progress","phase":"commentary","content":[{"type":"output_text","text":"progress","annotations":[]}]});
    let call = json!({"type":"function_call","id":"fc_done","call_id":"call_done","name":"read","arguments":"{\"path\":\"fixture.txt\"}"});
    let mut final_reasoning = reasoning.clone();
    final_reasoning
        .as_object_mut()
        .unwrap()
        .remove("encrypted_content");
    let mut final_commentary = commentary.clone();
    final_commentary.as_object_mut().unwrap().remove("phase");
    let body = event(
        json!({"type":"response.output_item.added","output_index":0,"item":reasoning}),
    ) + &event(
        json!({"type":"response.output_item.done","output_index":0,"item":final_reasoning}),
    ) + &event(
        json!({"type":"response.output_item.added","output_index":1,"item":commentary}),
    ) + &event(
        json!({"type":"response.output_item.done","output_index":1,"item":final_commentary}),
    ) + &event(json!({"type":"response.output_item.done","output_index":2,"item":call}))
        + &event(
            json!({"type":"response.completed","response":{"output":[final_reasoning,final_commentary,call]}}),
        );
    let (credentials, _) = server(200, "text/event-stream", body, false).await;
    let mut model = model();
    model.alias = Some("local-fast".into());
    let tools = mh::tools::definitions();
    let messages = [Message::user("fixture")];
    let mut initial = request(&model, &messages);
    initial.tools = &tools;
    let outcome = client()
        .run(initial, credentials, None, &CancellationToken::new())
        .await;
    assert_eq!(outcome.response.stop, StopReason::ToolUse);
    assert_eq!(outcome.response.text(), "progress");
    assert!(
        outcome.response.thinking().is_empty(),
        "opaque data must not become visible reasoning"
    );
    assert!(outcome.response.error.is_none());
    assert!(!format!("{:?}", outcome.response).contains("opaque-first"));
    let messages = [
        messages[0].clone(),
        Message::Assistant {
            response: outcome.response,
        },
        Message::ToolResult {
            call_id: "call_done".into(),
            name: "read".into(),
            result: ToolResult::text("controlled result"),
        },
    ];
    // Changing only the local entry/effort keeps the wire model compatible.
    model.alias = Some("local-deep".into());
    model.reasoning_effort = Some("high".into());
    let replay = request_body(&request(&model, &messages)).unwrap();
    let input = replay["input"].as_array().unwrap();
    assert_eq!(replay["model"], "model");
    assert_eq!(replay["reasoning"]["effort"], "high");
    assert_eq!(replay["store"], false);
    assert!(replay.get("previous_response_id").is_none());
    assert!(
        replay["reasoning"].get("context").is_none(),
        "do not send model-specific reuse controls universally"
    );
    assert_eq!(input.len(), 5);
    assert_eq!(input[1]["encrypted_content"], "opaque-first");
    assert_eq!(input[2]["phase"], "commentary");
    assert_eq!(input[3]["type"], "function_call");
    assert_eq!(input[4]["type"], "function_call_output");
    let mut foreign = model.clone();
    foreign.id = "different-family".into();
    let replay = request_body(&request(&foreign, &messages)).unwrap();
    assert!(
        !replay["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning")
    );
    let mut other_provider = request(&model, &messages);
    other_provider.provider = "other";
    let replay = request_body(&other_provider).unwrap();
    assert!(
        !replay["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning")
    );
}

#[tokio::test]
async fn invalid_tool_call_metadata_is_rejected_at_the_provider_boundary() {
    for output in [
        json!([{"type":"function_call","call_id":"","name":"read","arguments":"{}"}]),
        json!([
            {"type":"function_call","call_id":"duplicate","name":"read","arguments":"{}"},
            {"type":"function_call","call_id":"duplicate","name":"write","arguments":"{}"}
        ]),
        json!([{"type":"function_call","call_id":"call","name":"","arguments":"{}"}]),
    ] {
        let body = event(json!({"type":"response.incomplete","response":{
            "incomplete_details":{"reason":"max_output_tokens"},
            "output":output
        }}));
        let (credentials, _) = server(200, "text/event-stream", body, false).await;
        let model = model();
        let tools = mh::tools::definitions();
        let mut request = request(&model, &[]);
        request.tools = &tools;
        let outcome = client()
            .run(request, credentials, None, &CancellationToken::new())
            .await;
        assert_eq!(outcome.response.stop, StopReason::Error);
        assert_eq!(outcome.failure, Some(Failure::Protocol));
        assert_eq!(
            outcome.response.error.as_deref(),
            Some("Provider returned invalid or duplicate tool call metadata")
        );
    }
}

#[test]
fn wire_conversion_normalizes_foreign_ids_and_empty_tool_output() {
    let model = model();
    let messages = vec![
        Message::Assistant {
            response: Assistant {
                provider: "test".into(),
                model: "different-model".into(),
                items: vec![
                    OutputItem::Message {
                        id: Some("msg_foreign".into()),
                        phase: Some("final_answer".into()),
                        content: vec![OutputText::OutputText {
                            text: "answer".into(),
                            annotations: Vec::new(),
                        }],
                    },
                    OutputItem::FunctionCall {
                        id: Some("fc_foreign".into()),
                        call_id: "bad|call".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                ],
                stop: StopReason::ToolUse,
                usage: None,
                error: None,
            },
        },
        Message::ToolResult {
            call_id: "bad|call".into(),
            name: "read".into(),
            result: ToolResult {
                content: Vec::new(),
                details: Value::Null,
                is_error: false,
                structured_content: None,
            },
        },
        Message::ToolResult {
            call_id: "image-call".into(),
            name: "read".into(),
            result: ToolResult {
                content: vec![Content::Image {
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                }],
                details: Value::Null,
                is_error: false,
                structured_content: None,
            },
        },
    ];
    let body = request_body(&request(&model, &messages)).unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0]["id"], "msg_mh_0_0");
    assert!(input[0].get("phase").is_none());
    assert!(input[1].get("id").is_none());
    let normalized = input[1]["call_id"].as_str().unwrap();
    assert!(normalized.starts_with("call_"));
    assert!(normalized.len() <= 64);
    assert_eq!(input[2]["call_id"], normalized);
    assert_eq!(input[2]["output"], "(no tool output)");
    assert_eq!(input[3]["output"][0]["type"], "input_image");
}

#[tokio::test]
async fn filtered_and_permanent_sse_errors_are_not_retried() {
    for body in [
        event(
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"output":[]}}),
        ),
        event(json!({"type":"response.incomplete","response":{"output":[]}})),
        event(json!({"type":"error","code":"invalid_request_error","message":"must not echo"})),
    ] {
        let (credentials, _) = server(200, "text/event-stream", body, false).await;
        let model = model();
        let outcome = client()
            .run(
                request(&model, &[]),
                credentials,
                None,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.failure, Some(Failure::Protocol));
        assert!(!outcome.retryable());
    }
}
