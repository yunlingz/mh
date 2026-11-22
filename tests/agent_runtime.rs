use std::io::{Read, Write};
use std::sync::Arc;

use mh::agent::tools::ToolEvent;
use mh::agent::{Agent, Event, Phase, RunStatus};
use mh::config::Config;
use mh::context::{ProjectSkills, Resources};
use mh::features::btw::Thread;
use mh::message::{Assistant, Message, OutputItem, OutputText, StopReason};
use mh::providers::Responses;
use mh::session::{Entry, ModelChoice, Session};
use mh::storage::Layout;
use serde_json::{Value, json};

fn fixture(reasoning: bool) -> (tempfile::TempDir, Layout, std::path::PathBuf, Arc<Config>) {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    let config = Config::parse(
        &json!({
            "defaultModel":"test/model",
            "providers":{"test":{
                "api":"openai-responses-sse",
                "base_url_env":"MH_AGENT_TEST_BASE",
                "api_key_env":"MH_AGENT_TEST_KEY",
                "models":[{
                    "id":"model",
                    "reasoning":reasoning,
                    "reasoningEffort":if reasoning { json!("low") } else { Value::Null },
                    "contextWindow":64000,
                    "maxTokens":1024,
                    "compaction":{"reserveTokens":2048,"keepRecentTokens":4000}
                }]
            }}
        })
        .to_string(),
    )
    .unwrap();
    (temp, layout, cwd, Arc::new(config))
}

fn compaction_fixture(
    keep_recent_tokens: u64,
) -> (tempfile::TempDir, Layout, std::path::PathBuf, Arc<Config>) {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    let config = Config::parse(
        &json!({
            "defaultModel":"test/model",
            "providers":{"test":{
                "api":"openai-responses-sse",
                "base_url_env":"MH_AGENT_TEST_BASE",
                "api_key_env":"MH_AGENT_TEST_KEY",
                "models":[{
                    "id":"model","reasoning":true,"reasoningEffort":"low",
                    "contextWindow":64000,"maxTokens":1024,
                    "compaction":{"reserveTokens":2048,"keepRecentTokens":keep_recent_tokens}
                }]
            }}
        })
        .to_string(),
    )
    .unwrap();
    (temp, layout, cwd, Arc::new(config))
}

fn resources(cwd: &std::path::Path, layout: &Layout) -> Resources {
    Resources::load(cwd, &layout.config_dir(), ProjectSkills::Skip)
}

fn client() -> Responses {
    Responses::with_client(reqwest::Client::builder().no_proxy().build().unwrap())
}

#[tokio::test]
async fn interactive_agent_requires_a_questionnaire_event_receiver() {
    let (_temp, layout, cwd, config) = fixture(true);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let error = match Agent::new(
        config,
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        true,
    )
    .await
    {
        Ok(_) => panic!("interactive construction unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(error.contains("questionnaire event receiver"));
    assert!(session.snapshot().prompt.is_empty());
}

#[tokio::test]
async fn question_tool_is_advertised_only_to_interactive_agents() {
    for interactive in [false, true] {
        let (_temp, layout, cwd, config) = fixture(true);
        let session = Arc::new(Session::create(&layout, &cwd).unwrap());
        let (_sender_guard, tool_events) = if interactive {
            let (sender, receiver) = tokio::sync::mpsc::channel(8);
            (Some(receiver), Some(sender))
        } else {
            (None, None)
        };
        let _agent = Agent::new(
            config,
            session.clone(),
            resources(&cwd, &layout),
            client(),
            None,
            tool_events,
            interactive,
        )
        .await
        .unwrap();
        let prompt = session.snapshot().prompt.clone();
        assert!(prompt.contains("- todo:"));
        assert_eq!(prompt.contains("- ask_user_question:"), interactive);
    }
}

#[tokio::test]
async fn legacy_effort_is_normalized_to_the_configured_fixed_entry_with_a_notice() {
    let (_temp, layout, cwd, config) = fixture(false);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let old: ModelChoice = serde_json::from_value(json!({
        "qualified":"test/model", "thinking":"high"
    }))
    .unwrap();
    session.append(Entry::Model { choice: old }).await.unwrap();
    let agent = Agent::new(
        config,
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(agent.model_choice(), ModelChoice::new("test/model"));
    assert!(
        agent
            .resources()
            .warnings
            .iter()
            .any(|warning| warning.contains("normalized"))
    );
    assert!(session.history().await.unwrap().is_empty());
}

#[tokio::test]
async fn resource_refresh_publishes_an_arc_snapshot_and_updates_the_agent_prompt() {
    let (_temp, layout, cwd, config) = fixture(false);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let agent = Agent::new(
        config,
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        false,
    )
    .await
    .unwrap();
    let before = agent.resources();
    std::fs::write(cwd.join("AGENTS.md"), "Refreshed fixture instructions.").unwrap();
    agent
        .refresh_resources(resources(&cwd, &layout))
        .await
        .unwrap();
    let after = agent.resources();
    assert!(!Arc::ptr_eq(&before, &after));
    assert!(
        session
            .snapshot()
            .prompt
            .contains("Refreshed fixture instructions.")
    );
    assert!(session.history().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_unblocks_a_main_task_when_notifications_are_full() {
    let (_temp, layout, cwd, config) = fixture(false);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let (events, _receiver) = tokio::sync::mpsc::channel(1);
    events.send(Event::Notice("queued".into())).await.unwrap();
    let agent = Arc::new(
        Agent::new(
            config,
            session.clone(),
            resources(&cwd, &layout),
            client(),
            Some(events),
            None,
            false,
        )
        .await
        .unwrap(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let request_cancel = cancel.clone();
    let task =
        tokio::spawn(async move { agent.run("controlled prompt".into(), &request_cancel).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while session.snapshot().context.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("cancellation must unblock queued notifications")
        .unwrap();
}

fn read_request(stream: &mut std::net::TcpStream) -> Value {
    // Winsock can inherit the listener's nonblocking mode on accept.
    // This fixture intentionally uses blocking reads with a bounded timeout.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap();
    while bytes.len() - header_end < length {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn agent_fixture_server(listener: std::net::TcpListener) {
    listener.set_nonblocking(true).unwrap();
    for turn in 0..3 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "agent fixture timed out"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("agent fixture accept failed: {error}"),
            }
        };
        let request = read_request(&mut stream);
        if turn == 0 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let body = json!({
                "error":{"code":"server_error"},
                "usage":{"input_tokens":5,"output_tokens":1}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 503 Unavailable\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            continue;
        }
        let response = if turn == 1 {
            json!({
                "type":"response.incomplete",
                "response":{
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "output":[{
                        "type":"function_call","id":"fc_1","call_id":"call_1",
                        "name":"bash","arguments":"{\"command\":\"touch SHOULD_NOT_EXIST\"}"
                    }]
                }
            })
        } else {
            assert!(request["input"].as_array().unwrap().iter().any(|item| {
                item["type"] == "function_call_output"
                    && item["output"]
                        .as_str()
                        .is_some_and(|text| text.contains("truncated"))
            }));
            json!({
                "type":"response.completed",
                "response":{
                    "output":[{
                        "type":"message","id":"msg_final",
                        "content":[{"type":"output_text","text":"finished","annotations":[]}]
                    }],
                    "usage":{"input_tokens":63000,"output_tokens":100}
                }
            })
        };
        let body = format!("data: {response}\n\n");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }
}

#[test]
fn full_agent_loop_rejects_overlap_and_continues_after_truncated_calls() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || agent_fixture_server(listener));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "agent_loop_child", "--nocapture"])
        .env("MH_AGENT_LOOP_CHILD", "1")
        .env("MH_AGENT_TEST_BASE", format!("http://{address}"))
        .env("MH_AGENT_TEST_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[tokio::test]
async fn agent_loop_child() {
    if std::env::var_os("MH_AGENT_LOOP_CHILD").is_none() {
        return;
    }
    let (_temp, layout, cwd, config) = fixture(true);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let (event_sender, mut events) = tokio::sync::mpsc::channel(32);
    let (tool_sender, mut tool_events) = tokio::sync::mpsc::channel(8);
    let agent = Arc::new(
        Agent::new(
            config,
            session.clone(),
            resources(&cwd, &layout),
            client(),
            Some(event_sender),
            Some(tool_sender),
            false,
        )
        .await
        .unwrap(),
    );
    let first_agent = agent.clone();
    let first_cancel = tokio_util::sync::CancellationToken::new();
    let first =
        tokio::spawn(async move { first_agent.run("first prompt".into(), &first_cancel).await });
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, Event::Phase(Phase::Streaming)) {
            break;
        }
    }
    let second = agent
        .run(
            "must not be appended".into(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert!(second.unwrap_err().contains("Another main operation"));
    assert!(
        agent
            .refresh_resources(resources(&cwd, &layout))
            .await
            .unwrap_err()
            .contains("Another main operation")
    );

    let outcome = first.await.unwrap().unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.response.unwrap().text(), "finished");
    let history = session.history().await.unwrap();
    assert_eq!(history.len(), 4);
    assert_eq!(session.snapshot().usage.input, 63_005);
    assert_eq!(session.snapshot().usage.output, 101);
    assert_eq!(
        history
            .iter()
            .filter(|(_, message)| matches!(message.as_ref(), Message::User { .. }))
            .count(),
        1
    );
    assert!(history.iter().any(|(_, message)| {
        matches!(message.as_ref(), Message::ToolResult { result, .. }
            if result.is_error && result.plain_text().contains("truncated"))
    }));
    assert!(!cwd.join("SHOULD_NOT_EXIST").exists());

    let started = tokio::time::timeout(std::time::Duration::from_secs(1), tool_events.recv())
        .await
        .unwrap()
        .unwrap();
    let finished = tokio::time::timeout(std::time::Duration::from_secs(1), tool_events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(started, ToolEvent::Started(ref call) if call.id == "call_1"));
    assert!(matches!(finished, ToolEvent::Finished { ref call_id, .. } if call_id == "call_1"));
}

#[test]
fn retries_stop_at_the_budget_and_respect_quota_and_backoff_cancellation() {
    for case in ["exhausted", "quota", "cancel"] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let expected = if case == "exhausted" { 4 } else { 1 };
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            for _ in 0..expected {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline);
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(error) => panic!("retry fixture accept failed: {error}"),
                    }
                };
                let request = read_request(&mut stream);
                let input = request["input"].as_array().unwrap();
                assert_eq!(input.len(), 1, "retry must not append another user prompt");
                assert_eq!(input[0]["role"], "user");
                let code = if case == "quota" {
                    "insufficient_quota"
                } else {
                    "server_error"
                };
                let status = if case == "quota" { 429 } else { 503 };
                let delay = if case == "cancel" { 60 } else { 0 };
                let body = json!({
                    "error":{"code":code},
                    "usage":{"input_tokens":2,"output_tokens":1}
                })
                .to_string();
                write!(stream,
                    "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nRetry-After: {delay}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).unwrap();
            }
        });
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "retry_budget_child", "--nocapture"])
            .env("MH_RETRY_BUDGET_CHILD", case)
            .env("MH_AGENT_TEST_BASE", format!("http://{address}"))
            .env("MH_AGENT_TEST_KEY", "fixture")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case} child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        server.join().unwrap();
    }
}

#[tokio::test]
async fn retry_budget_child() {
    let Ok(case) = std::env::var("MH_RETRY_BUDGET_CHILD") else {
        return;
    };
    let (_temp, layout, cwd, config) = fixture(false);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let (sender, mut events) = tokio::sync::mpsc::channel(32);
    let agent = Agent::new(
        config,
        session.clone(),
        resources(&cwd, &layout),
        client(),
        Some(sender),
        None,
        false,
    )
    .await
    .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let request_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        agent
            .run("controlled retry prompt".into(), &request_cancel)
            .await
    });
    if case == "cancel" {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let Event::RetryScheduled {
                scope,
                attempt,
                delay,
                ..
            } = event
            {
                assert_eq!(scope, mh::agent::RetryScope::Main);
                assert_eq!(attempt, 1);
                assert_eq!(delay, std::time::Duration::from_secs(60));
                cancel.cancel();
                break;
            }
        }
    }
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("retry budget/cancellation must finish promptly")
        .unwrap()
        .unwrap();
    let mut attempts = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::RetryScheduled {
            scope,
            attempt,
            max_retries,
            ..
        } = event
        {
            assert_eq!(scope, mh::agent::RetryScope::Main);
            assert_eq!(max_retries, 3);
            attempts.push(attempt);
        }
    }
    if case == "cancel" {
        assert_eq!(outcome.status, RunStatus::Cancelled);
        assert!(attempts.is_empty());
    } else {
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(
            attempts,
            if case == "exhausted" {
                vec![1, 2, 3]
            } else {
                vec![]
            }
        );
        assert_eq!(session.history().await.unwrap().len(), 2);
    }
    let expected = if case == "exhausted" { 4 } else { 1 };
    assert_eq!(session.snapshot().usage.input, 2 * expected);
    assert_eq!(session.snapshot().usage.output, expected);
    let history = session.history().await.unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|(_, message)| matches!(message.as_ref(), Message::User { .. }))
            .count(),
        1
    );
    assert!(
        history
            .iter()
            .all(|(_, message)| !matches!(message.as_ref(), Message::ToolResult { .. }))
    );
}

fn compaction_server(listener: std::net::TcpListener) {
    for turn in 0..2 {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        assert_eq!(request["tool_choice"], "none");
        assert_eq!(request["reasoning"]["effort"], "low");
        if turn == 0 {
            let body = json!({
                "error":{"code":"server_error"},
                "usage":{"input_tokens":5,"output_tokens":1}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 503 Unavailable\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        } else {
            let body = format!(
                "data: {}\n\n",
                json!({
                    "type":"response.completed",
                    "response":{
                        "output":[{
                            "type":"message","id":"msg_summary",
                            "content":[{"type":"output_text","text":"checkpoint summary","annotations":[]}]
                        }],
                        "usage":{"input_tokens":10,"output_tokens":2}
                    }
                })
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    }
}

#[test]
fn compaction_retries_transient_summary_failures_and_counts_each_attempt() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || compaction_server(listener));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "compaction_retry_child", "--nocapture"])
        .env("MH_COMPACTION_RETRY_CHILD", "1")
        .env("MH_AGENT_TEST_BASE", format!("http://{address}"))
        .env("MH_AGENT_TEST_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[tokio::test]
async fn compaction_retry_child() {
    if std::env::var_os("MH_COMPACTION_RETRY_CHILD").is_none() {
        return;
    }
    let (_temp, layout, cwd, config) = compaction_fixture(5);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let agent = Agent::new(
        config,
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        false,
    )
    .await
    .unwrap();
    for (user, answer) in [("first", "first answer"), ("second", "second answer")] {
        session
            .append(Entry::Message {
                message: Arc::new(Message::user(user)),
            })
            .await
            .unwrap();
        session
            .append(Entry::Message {
                message: Arc::new(Message::Assistant {
                    response: Assistant {
                        provider: "test".into(),
                        model: "model".into(),
                        items: vec![OutputItem::Message {
                            id: None,
                            phase: None,
                            content: vec![OutputText::OutputText {
                                text: answer.into(),
                                annotations: Vec::new(),
                            }],
                        }],
                        stop: StopReason::Stop,
                        usage: None,
                        error: None,
                    },
                }),
            })
            .await
            .unwrap();
    }
    agent
        .compact(
            "retain decisions",
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let snapshot = session.snapshot();
    assert_eq!(
        snapshot.previous_summary.as_deref().map(String::as_str),
        Some("checkpoint summary")
    );
    assert_eq!(snapshot.usage.input, 15);
    assert_eq!(snapshot.usage.output, 3);
}

fn side_retry_server(listener: std::net::TcpListener) {
    for turn in 0..2 {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        assert_eq!(request["tool_choice"], "none");
        if turn == 0 {
            let body = json!({
                "error":{"code":"server_error"},
                "usage":{"input_tokens":4,"output_tokens":1}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 503 Unavailable\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        } else {
            let body = format!(
                "data: {}\n\n",
                json!({
                    "type":"response.completed",
                    "response":{
                        "output":[{
                            "type":"message","id":"msg_side",
                            "content":[{"type":"output_text","text":"side answer","annotations":[]}]
                        }],
                        "usage":{"input_tokens":8,"output_tokens":2}
                    }
                })
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    }
}

#[test]
fn side_requests_retry_without_mutating_main_history() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || side_retry_server(listener));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "side_retry_child", "--nocapture"])
        .env("MH_SIDE_RETRY_CHILD", "1")
        .env("MH_AGENT_TEST_BASE", format!("http://{address}"))
        .env("MH_AGENT_TEST_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[tokio::test]
async fn side_retry_child() {
    if std::env::var_os("MH_SIDE_RETRY_CHILD").is_none() {
        return;
    }
    let (_temp, layout, cwd, config) = fixture(true);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let agent = Agent::new(
        config,
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        false,
    )
    .await
    .unwrap();
    let answer = agent
        .side(
            "quick question",
            &Thread::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(answer.text(), "side answer");
    assert!(session.history().await.unwrap().is_empty());
    assert_eq!(session.snapshot().usage.input, 12);
    assert_eq!(session.snapshot().usage.output, 3);
}

fn overflow_recovery_server(listener: std::net::TcpListener) {
    for turn in 0..3 {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        if turn == 0 {
            assert!(request["tools"].is_array());
            let body = json!({
                "error":{"code":"context_length_exceeded"},
                "usage":{"input_tokens":3,"output_tokens":1}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 400 Context\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        } else if turn == 1 {
            assert_eq!(request["tool_choice"], "none");
            let body = format!(
                "data: {}\n\n",
                json!({
                    "type":"response.completed",
                    "response":{
                        "output":[{
                            "type":"message","id":"msg_summary",
                            "content":[{"type":"output_text","text":"overflow checkpoint","annotations":[]}]
                        }],
                        "usage":{"input_tokens":10,"output_tokens":2}
                    }
                })
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        } else {
            assert!(request["input"].as_array().unwrap().iter().any(|item| {
                item["role"] == "user"
                    && item["content"].as_array().is_some_and(|content| {
                        content.iter().any(|part| {
                            part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("Previous context checkpoint"))
                        })
                    })
            }));
            let body = format!(
                "data: {}\n\n",
                json!({
                    "type":"response.completed",
                    "response":{
                        "output":[{
                            "type":"message","id":"msg_recovered",
                            "content":[{"type":"output_text","text":"recovered answer","annotations":[]}]
                        }],
                        "usage":{"input_tokens":20,"output_tokens":4}
                    }
                })
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    }
}

#[test]
fn context_overflow_compacts_and_retries_the_main_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || overflow_recovery_server(listener));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "overflow_recovery_child", "--nocapture"])
        .env("MH_OVERFLOW_RECOVERY_CHILD", "1")
        .env("MH_AGENT_TEST_BASE", format!("http://{address}"))
        .env("MH_AGENT_TEST_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[tokio::test]
async fn overflow_recovery_child() {
    if std::env::var_os("MH_OVERFLOW_RECOVERY_CHILD").is_none() {
        return;
    }
    let (_temp, layout, cwd, config) = compaction_fixture(8);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let agent = Agent::new(
        config,
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        false,
    )
    .await
    .unwrap();
    for (user, answer) in [("first", "first answer"), ("second", "second answer")] {
        session
            .append(Entry::Message {
                message: Arc::new(Message::user(user)),
            })
            .await
            .unwrap();
        session
            .append(Entry::Message {
                message: Arc::new(Message::Assistant {
                    response: Assistant {
                        provider: "test".into(),
                        model: "model".into(),
                        items: vec![OutputItem::Message {
                            id: None,
                            phase: None,
                            content: vec![OutputText::OutputText {
                                text: answer.into(),
                                annotations: Vec::new(),
                            }],
                        }],
                        stop: StopReason::Stop,
                        usage: None,
                        error: None,
                    },
                }),
            })
            .await
            .unwrap();
    }
    let outcome = agent
        .run(
            "third prompt".into(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.response.unwrap().text(), "recovered answer");
    assert_eq!(
        session
            .snapshot()
            .previous_summary
            .as_deref()
            .map(String::as_str),
        Some("overflow checkpoint")
    );
    assert_eq!(session.history().await.unwrap().len(), 6);
    assert_eq!(session.snapshot().usage.input, 33);
    assert_eq!(session.snapshot().usage.output, 7);
}

fn reasoning_fixture_response(stream: &mut std::net::TcpStream, events: &[Value]) {
    let body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    write!(stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
    ).unwrap();
}

#[test]
fn reasoning_continuation_survives_tool_retry_resume_and_retained_compaction_context() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut before_retry = None;
        for turn in 0..6 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert_eq!(request["store"], false);
            assert!(request.get("previous_response_id").is_none());
            if turn == 1 || turn == 2 {
                let input = request["input"].as_array().unwrap();
                assert_eq!(input[1]["encrypted_content"], "opaque-tool");
                assert_eq!(input[2]["phase"], "commentary");
                assert_eq!(input[3]["type"], "function_call");
                assert_eq!(input[4]["type"], "function_call_output");
                if turn == 1 {
                    before_retry = Some(request.clone());
                    let body = json!({"error":{"code":"server_error"}}).to_string();
                    write!(stream,
                        "HTTP/1.1 503 Result\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                    ).unwrap();
                    continue;
                }
                assert_eq!(before_retry.as_ref(), Some(&request));
            }
            if turn == 3 {
                let input = request["input"].as_array().unwrap();
                assert_eq!(input[1]["encrypted_content"], "opaque-tool");
                assert_eq!(input[5]["encrypted_content"], "opaque-answer");
                assert_eq!(input[6]["phase"], "final_answer");
                assert_eq!(input[7]["role"], "user");
            }
            if turn >= 4 {
                assert_eq!(request["tool_choice"], "none");
                assert!(
                    !request["input"].to_string().contains("opaque-"),
                    "encrypted data must not be summarized as text"
                );
            }
            let output = if turn == 0 {
                let call = json!({"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"read","arguments":"{\"path\":\"fixture.txt\"}"});
                let reasoning = json!({"type":"reasoning","id":"rs_tool","summary":[],"encrypted_content":"opaque-tool"});
                let commentary = json!({"type":"message","id":"msg_progress","phase":"commentary","content":[{"type":"output_text","text":"checking fixture","annotations":[]}]});
                reasoning_fixture_response(
                    &mut stream,
                    &[
                        json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
                        json!({"type":"response.output_item.done","output_index":1,"item":commentary}),
                        json!({"type":"response.output_item.done","output_index":2,"item":call}),
                        json!({"type":"response.completed","response":{"output":[reasoning,commentary,call]}}),
                    ],
                );
                continue;
            } else {
                let text = if turn == 2 {
                    "first answer"
                } else if turn == 3 {
                    "recent answer"
                } else {
                    "checkpoint summary"
                };
                let mut output = Vec::new();
                if turn == 2 || turn == 3 {
                    output.push(json!({"type":"reasoning","id":format!("rs_{turn}"),"summary":[],"encrypted_content":if turn == 2 {"opaque-answer"} else {"opaque-recent"}}));
                }
                output.push(json!({"type":"message","id":format!("msg_{turn}"),"phase":"final_answer","content":[{"type":"output_text","text":text,"annotations":[]}]}));
                output
            };
            reasoning_fixture_response(
                &mut stream,
                &[json!({"type":"response.completed","response":{"output":output}})],
            );
        }
    });
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "reasoning_continuation_child", "--nocapture"])
        .env("MH_REASONING_CONTINUATION_CHILD", "1")
        .env("MH_AGENT_TEST_BASE", format!("http://{address}"))
        .env("MH_AGENT_TEST_KEY", "fixture")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "reasoning fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[tokio::test]
async fn reasoning_continuation_child() {
    if std::env::var_os("MH_REASONING_CONTINUATION_CHILD").is_none() {
        return;
    }
    let (_temp, layout, cwd, config) = compaction_fixture(4);
    std::fs::write(cwd.join("fixture.txt"), "controlled continuation fixture").unwrap();
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let agent = Agent::new(
        config.clone(),
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        false,
    )
    .await
    .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    assert_eq!(
        agent.run("first".into(), &cancel).await.unwrap().status,
        RunStatus::Completed
    );
    let file = session.file();
    let id = session.id().to_owned();
    let saved = std::fs::read(&file).unwrap();
    drop(agent);
    drop(session);
    let session = Arc::new(Session::open(&layout, &cwd, &id).unwrap());
    let agent = Agent::new(
        config.clone(),
        session.clone(),
        resources(&cwd, &layout),
        client(),
        None,
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        agent.run("second".into(), &cancel).await.unwrap().status,
        RunStatus::Completed
    );
    agent.compact("retain context", &cancel).await.unwrap();
    assert!(std::fs::read(&file).unwrap().starts_with(&saved));
    let snapshot = session.snapshot();
    assert_eq!(session.history().await.unwrap().len(), 6);
    let messages = snapshot.messages();
    let (provider, _, model) = config.model(&config.default_model).unwrap();
    let replay = mh::providers::request_body(&mh::providers::Request {
        provider,
        model,
        instructions: &snapshot.prompt,
        messages: &messages,
        tools: &[],
        estimated_input_tokens: 100,
        cache_key: None,
    })
    .unwrap();
    assert_eq!(replay["input"][1]["encrypted_content"], "opaque-recent");
    assert_eq!(replay["input"][2]["phase"], "final_answer");
    assert!(!replay["input"].to_string().contains("opaque-tool"));
    assert!(!replay["input"].to_string().contains("opaque-answer"));
    drop(snapshot);
    drop(agent);
    drop(session);
    let reopened = Session::open(&layout, &cwd, &id).unwrap();
    let messages = reopened.snapshot().messages();
    assert_eq!(
        messages[1].to_wire(1, provider, &model.id, false)[0]["encrypted_content"],
        "opaque-recent"
    );
    assert!(
        messages
            .iter()
            .all(|message| !message.text().contains("opaque-"))
    );
}

fn side_cancel_server(listener: std::net::TcpListener) {
    let (mut stream, _) = listener.accept().unwrap();
    let _ = read_request(&mut stream);
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"partial side"})
    )
    .unwrap();
    stream.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(1));
}

#[test]
fn side_requests_cancel_independently_after_streaming_text() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || side_cancel_server(listener));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "side_cancel_child", "--nocapture"])
        .env("MH_SIDE_CANCEL_CHILD", "1")
        .env("MH_AGENT_TEST_BASE", format!("http://{address}"))
        .env("MH_AGENT_TEST_KEY", "test-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[tokio::test]
async fn side_cancel_child() {
    if std::env::var_os("MH_SIDE_CANCEL_CHILD").is_none() {
        return;
    }
    let (_temp, layout, cwd, config) = fixture(true);
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let (event_sender, mut events) = tokio::sync::mpsc::channel(8);
    let agent = Arc::new(
        Agent::new(
            config,
            session.clone(),
            resources(&cwd, &layout),
            client(),
            Some(event_sender),
            None,
            false,
        )
        .await
        .unwrap(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let request_cancel = cancel.clone();
    let request_agent = agent.clone();
    let request = tokio::spawn(async move {
        let thread = Thread::default();
        request_agent
            .side("quick question", &thread, &request_cancel)
            .await
    });
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, Event::SideText(ref text) if text == "partial side") {
            cancel.cancel();
            break;
        }
    }
    let answer = request.await.unwrap().unwrap();
    assert_eq!(answer.stop, StopReason::Aborted);
    assert!(session.history().await.unwrap().is_empty());
}
