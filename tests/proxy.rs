use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use mh::config::{Config, Credentials};
use mh::message::StopReason;
use mh::providers::proxy::{Status, configure_with};
use mh::providers::{Request, Responses};
use mh::theme::Palette;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::widgets::Paragraph;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const PROXY_VARIABLES: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "all_proxy",
];

#[test]
fn proxy_status_is_redacted_and_excluded_variables_are_not_read() {
    let (_, status) = configure_with(reqwest::Client::builder(), |name| match name {
        "HTTP_PROXY" => Some(OsString::from(
            "http://private-user:private-password@private-proxy:3128",
        )),
        "NO_PROXY" => Some(OsString::from("private-host,10.0.0.0/8")),
        "ALL_PROXY" | "all_proxy" => panic!("Excluded proxy variable was read"),
        _ => None,
    })
    .unwrap_or_else(|error| panic!("{error}"));
    let text = format!("{status:?}\n{}", status.lines().join("\n"));
    assert!(text.contains("HTTP_PROXY"));
    assert!(text.contains("NO_PROXY"));
    assert!(!text.contains("private-"));
    assert!(!text.contains("10.0.0.0"));
    assert!(!text.contains("ALL_PROXY"));
}

#[test]
fn invalid_proxy_errors_do_not_expose_values() {
    let result = configure_with(reqwest::Client::builder(), |name| {
        (name == "HTTP_PROXY").then(|| OsString::from("://PRIVATE_INVALID_VALUE"))
    });
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("Invalid proxy setting was accepted"),
    };
    assert_eq!(error, "Invalid HTTP proxy setting in HTTP_PROXY");
    assert!(!error.contains("PRIVATE_INVALID_VALUE"));
}

#[test]
fn startup_proxy_section_renders_without_credentials_or_proxy_addresses() {
    let (_, status) = configure_with(reqwest::Client::builder(), |name| {
        (name == "https_proxy")
            .then(|| OsString::from("http://private-user:private-password@private-proxy:3128"))
    })
    .unwrap_or_else(|error| panic!("{error}"));
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new(mh::ui::startup::proxy_lines(&status, &Palette::default())),
                frame.area(),
            );
        })
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("[Proxy]"));
    assert!(text.contains("HTTPS: proxy configured from https_proxy"));
    assert!(!text.contains("private-"));
}

#[derive(Default)]
struct Seen {
    requests: AtomicUsize,
    connects: AtomicUsize,
    valid: AtomicBool,
    proxy_auth: AtomicBool,
}

struct Fixture {
    address: std::net::SocketAddr,
    seen: Arc<Seen>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(proxy: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Seen::default());
        let observed = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let observed = observed.clone();
                tokio::spawn(async move {
                    let Ok(Some((header, body))) =
                        tokio::time::timeout(Duration::from_secs(2), read_request(&mut socket))
                            .await
                    else {
                        return;
                    };
                    if header.starts_with("CONNECT ") {
                        observed.connects.fetch_add(1, Ordering::SeqCst);
                        let _ = socket
                            .write_all(b"HTTP/1.1 502 Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    observed.requests.fetch_add(1, Ordering::SeqCst);
                    let absolute = header.starts_with("POST http://");
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    observed.valid.store(
                        absolute == proxy && body["stream"] == true && body["store"] == false,
                        Ordering::SeqCst,
                    );
                    observed.proxy_auth.store(
                        header.to_ascii_lowercase().contains("proxy-authorization:"),
                        Ordering::SeqCst,
                    );
                    let text = if proxy { "Proxy" } else { "Direct" };
                    let event = json!({"type":"response.completed","response":{
                        "output":[{"type":"message","content":[{"type":"output_text","text":text,"annotations":[]}]}],
                        "usage":{"input_tokens":3,"output_tokens":1}
                    }});
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {event}\n\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            address,
            seen,
            task,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        let count = socket.read(&mut buffer).await.ok()?;
        if count == 0 || bytes.len() + count > 64 * 1024 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let header = String::from_utf8(bytes[..end].to_vec()).ok()?;
    let length = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while bytes.len() - end < length {
        let count = socket.read(&mut buffer).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Some((header, bytes[end..end + length].to_vec()))
}

#[derive(Clone, Copy, PartialEq)]
enum Route {
    Proxy,
    Direct,
    Connect,
}

async fn case(variable: &str, bypass: Option<(&str, &str)>, route: Route, extra: &[(&str, &str)]) {
    let proxy = Fixture::start(true).await;
    let direct = Fixture::start(false).await;
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "proxy_child", "--nocapture"]);
    for name in PROXY_VARIABLES {
        command.env_remove(name);
    }
    command.env_remove("REQUEST_METHOD");
    let proxy_url = format!("http://fixture-user:fixture-password@{}", proxy.address);
    command.env(variable, proxy_url);
    if let Some((name, value)) = bypass {
        command.env(name, value);
    }
    for (name, value) in extra {
        command.env(name, value);
    }
    command.env("MH_PROXY_CHILD", "1");
    command.env(
        "MH_PROXY_EXPECT",
        if route == Route::Connect {
            "Connect"
        } else if route == Route::Proxy {
            "Proxy"
        } else {
            "Direct"
        },
    );
    command.env(
        "MH_PROXY_TARGET",
        format!(
            "{}://{}/responses",
            if route == Route::Connect {
                "https"
            } else {
                "http"
            },
            direct.address
        ),
    );
    let result = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "Controlled child fixture failed for {variable}: {}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    match route {
        Route::Proxy => {
            assert_eq!(proxy.seen.requests.load(Ordering::SeqCst), 1);
            assert!(proxy.seen.valid.load(Ordering::SeqCst));
            assert!(proxy.seen.proxy_auth.load(Ordering::SeqCst));
            assert_eq!(direct.seen.requests.load(Ordering::SeqCst), 0);
        }
        Route::Direct => {
            assert_eq!(direct.seen.requests.load(Ordering::SeqCst), 1);
            assert!(direct.seen.valid.load(Ordering::SeqCst));
            assert!(!direct.seen.proxy_auth.load(Ordering::SeqCst));
            assert_eq!(proxy.seen.requests.load(Ordering::SeqCst), 0);
        }
        Route::Connect => assert_eq!(proxy.seen.connects.load(Ordering::SeqCst), 1),
    }
}

#[tokio::test]
async fn provider_honors_six_variables_and_ignores_all_proxy() {
    for variable in ["HTTP_PROXY", "http_proxy"] {
        case(variable, None, Route::Proxy, &[]).await;
    }
    for variable in ["HTTPS_PROXY", "https_proxy"] {
        case(variable, None, Route::Connect, &[]).await;
    }
    for variable in ["NO_PROXY", "no_proxy"] {
        case(
            "HTTP_PROXY",
            Some((variable, "127.0.0.1")),
            Route::Direct,
            &[],
        )
        .await;
        case(
            "HTTP_PROXY",
            Some((variable, "127.0.0.0/8")),
            Route::Direct,
            &[],
        )
        .await;
        case("HTTP_PROXY", Some((variable, "*")), Route::Direct, &[]).await;
    }
    for variable in ["ALL_PROXY", "all_proxy"] {
        case(variable, None, Route::Direct, &[]).await;
    }
    case(
        "HTTP_PROXY",
        None,
        Route::Direct,
        &[("REQUEST_METHOD", "GET")],
    )
    .await;
    #[cfg(not(windows))]
    {
        case(
            "HTTP_PROXY",
            None,
            Route::Proxy,
            &[("http_proxy", "http://127.0.0.1:1")],
        )
        .await;
        case(
            "HTTP_PROXY",
            None,
            Route::Proxy,
            &[("NO_PROXY", ""), ("no_proxy", "127.0.0.1")],
        )
        .await;
        case("http_proxy", None, Route::Direct, &[("HTTP_PROXY", "")]).await;
    }
}

#[tokio::test]
async fn proxy_child() {
    if std::env::var_os("MH_PROXY_CHILD").is_none() {
        return;
    }
    let config = Config::parse(
        &json!({
            "defaultModel":"test/model", "providers":{"test":{
                "api":"openai-responses-sse", "base_url_env":"TEST_BASE", "api_key_env":"TEST_KEY",
                "models":[{"id":"model","contextWindow":64000,"maxTokens":1024,
                    "compaction":{"reserveTokens":2048,"keepRecentTokens":4000}}]
            }}
        })
        .to_string(),
    )
    .unwrap();
    let model = &config.providers["test"].models[0];
    let client = Responses::new().unwrap();
    assert!(matches!(client.proxy_status(), Status::Environment { .. }));
    let request = Request {
        provider: "test",
        model,
        instructions: "Fixture",
        messages: &[],
        tools: &[],
        estimated_input_tokens: 20,
        cache_key: None,
    };
    let credentials = Credentials {
        endpoint: url::Url::parse(&std::env::var("MH_PROXY_TARGET").unwrap()).unwrap(),
        api_key: "fixture-key".into(),
    };
    let cancel = CancellationToken::new();
    let timer = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        timer.cancel();
    });
    let outcome = client.run(request, credentials, None, &cancel).await;
    let expected = std::env::var("MH_PROXY_EXPECT").unwrap();
    if expected == "Connect" {
        assert_eq!(outcome.response.stop, StopReason::Error);
    } else {
        assert_eq!(outcome.response.stop, StopReason::Stop);
        assert_eq!(outcome.response.text(), expected);
    }
}
