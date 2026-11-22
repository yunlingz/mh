//! Scripted local SSE fixtures: no generated tools outside the temporary workspace.
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use mh::agent::{
    Agent, Event,
    queue::{Kind, Queue},
};
use mh::config::Config;
use mh::context::{ProjectSkills, Resources};
use mh::message::{Content, Message};
use mh::providers::Responses;
use mh::session::Session;
use mh::storage::Layout;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn request(socket: &mut std::net::TcpStream) -> Value {
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let end = loop {
        let n = socket.read(&mut chunk).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(i) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..end]);
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() - end < length {
        let n = socket.read(&mut chunk).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
}
fn event(socket: &mut std::net::TcpStream, value: Value) {
    write!(socket, "data: {value}\n\n").unwrap();
    socket.flush().unwrap();
}

#[test]
fn streaming_queue_edits_are_delivered_once_after_complete_tool_batches() {
    if std::env::var_os("MH_QUEUE_CHILD").is_some() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..4 {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_nonblocking(false).unwrap();
            let body = request(&mut socket);
            requests.push(body);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            if index == 0 {
                event(
                    &mut socket,
                    json!({"type":"response.output_text.delta","delta":"active"}),
                );
                std::thread::sleep(Duration::from_millis(300));
            }
            let output = if index < 2 {
                let call = json!({"type":"function_call","id":format!("fc_{index}"),"call_id":format!("read_{index}"),"name":"read","arguments":"{\"path\":\"wait.txt\"}"});
                event(
                    &mut socket,
                    json!({"type":"response.output_item.added","output_index":2,"item":call}),
                );
                event(
                    &mut socket,
                    json!({"type":"response.output_item.done","output_index":2,"item":call}),
                );
                vec![
                    json!({"type":"reasoning","id":format!("rs_{index}"),"summary":[],"encrypted_content":"opaque-queue-reasoning"}),
                    json!({"type":"message","id":format!("msg_progress_{index}"),"phase":"commentary","content":[{"type":"output_text","text":"active","annotations":[]}]}),
                    call,
                ]
            } else {
                vec![
                    json!({"type":"message","id":format!("msg_{index}"),"content":[{"type":"output_text","text":"done","annotations":[]}]}),
                ]
            };
            event(
                &mut socket,
                json!({"type":"response.completed","response":{"output":output,"usage":{"input_tokens":20,"output_tokens":10}}}),
            );
        }
        requests
    });
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "input_queue_child", "--nocapture"])
        .env("MH_QUEUE_CHILD", "1")
        .env("MH_QUEUE_BASE", format!("http://{address}"))
        .env("MH_QUEUE_KEY", "fixture-key")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let requests = server.join().unwrap();
    assert!(requests.iter().all(|request| request["model"] == "model"));
    assert_eq!(requests[0]["reasoning"]["effort"], "low");
    assert!(
        requests[1..]
            .iter()
            .all(|request| request["reasoning"]["effort"] == "high")
    );
    assert!(requests.iter().all(|request| request["store"] == false));
    let continuation = requests[1]["input"].as_array().unwrap();
    assert_eq!(
        continuation[1]["encrypted_content"],
        "opaque-queue-reasoning"
    );
    assert_eq!(continuation[2]["phase"], "commentary");
    assert_eq!(continuation[3]["type"], "function_call");
    assert_eq!(continuation[4]["type"], "function_call_output");
    let text = |i: usize| requests[i]["input"].to_string();
    assert!(!text(0).contains("edited-final"));
    assert!(text(1).contains("edited-final"));
    assert!(text(1).contains("function_call_output"));
    assert!(!text(1).contains("follow-later"));
    assert!(!text(2).contains("follow-later"));
    assert!(text(3).contains("follow-later"));
    assert!(
        requests
            .iter()
            .all(|r| !r["input"].to_string().contains("discarded-draft"))
    );
}

#[tokio::test]
async fn input_queue_child() {
    if std::env::var_os("MH_QUEUE_CHILD").is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("workspace");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("wait.txt"), "controlled tool fixture").unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    let config=Arc::new(Config::parse(&json!({"defaultModel":"test/fast","providers":{"test":{
        "api":"openai-responses-sse","base_url_env":"MH_QUEUE_BASE","api_key_env":"MH_QUEUE_KEY",
        "models":[
            {"id":"model","alias":"fast","reasoning":true,"reasoningEffort":"low","contextWindow":64000,"maxTokens":1024,"compaction":{"reserveTokens":2048,"keepRecentTokens":4000}},
            {"id":"model","alias":"deep","reasoning":true,"reasoningEffort":"high","contextWindow":64000,"maxTokens":1024,"compaction":{"reserveTokens":2048,"keepRecentTokens":4000}}
        ]
    }}}).to_string()).unwrap());
    let session = Arc::new(Session::create(&layout, &cwd).unwrap());
    let (sender, mut events) = tokio::sync::mpsc::channel(256);
    let agent = Arc::new(
        Agent::new(
            config,
            session.clone(),
            Resources::load(&cwd, &layout.config_dir(), ProjectSkills::Skip),
            Responses::with_client(reqwest::Client::builder().no_proxy().build().unwrap()),
            Some(sender),
            None,
            false,
        )
        .await
        .unwrap(),
    );
    let queue = Queue::default();
    let producer = queue.clone();
    let task_agent = agent.clone();
    let task = tokio::spawn(async move {
        task_agent
            .run_input_with_queue(
                Some(vec![Content::text("root")]),
                &producer,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
    });
    while !matches!(events.recv().await, Some(Event::Text(_))) {}
    queue
        .push(
            Kind::Steer,
            vec![Content::text("discarded-draft-a")],
            "a".into(),
        )
        .unwrap();
    queue
        .push(
            Kind::Steer,
            vec![Content::text("discarded-draft-b")],
            "b".into(),
        )
        .unwrap();
    queue
        .edit(|inputs| {
            assert_eq!(inputs.len(), 2);
            Ok(())
        })
        .unwrap();
    queue
        .push(
            Kind::Steer,
            vec![Content::text("edited-final")],
            "final".into(),
        )
        .unwrap();
    queue
        .push(
            Kind::FollowUp,
            vec![Content::text("follow-later")],
            "later".into(),
        )
        .unwrap();
    agent.select_model("test/deep").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert!(queue.snapshot().is_empty());
    let history = session.history().await.unwrap();
    let user = history
        .iter()
        .filter_map(|(_, m)| match m.as_ref() {
            Message::User { .. } => Some(m.text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(user, ["root", "edited-final", "follow-later"]);
}
