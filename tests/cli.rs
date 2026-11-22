use std::io::{Read, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn binary() -> std::path::PathBuf {
    let configured = std::path::PathBuf::from(env!("CARGO_BIN_EXE_mh"));
    if configured.is_file() {
        configured
    } else {
        std::env::current_exe()
            .expect("current test executable")
            .parent()
            .expect("test executable directory")
            .join(if cfg!(windows) { "mh.exe" } else { "mh" })
    }
}

fn request(stream: &mut std::net::TcpStream) -> Value {
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

fn write_config(home: &std::path::Path) {
    let directory = home.join(".mh/config");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("config.json"),
        serde_json::to_vec_pretty(&json!({
            "defaultModel":"test/model",
            "providers":{"test":{
                "api":"openai-responses-sse",
                "base_url_env":"MH_CLI_TEST_BASE",
                "api_key_env":"MH_CLI_TEST_KEY",
                "models":[{
                    "id":"model","contextWindow":64000,"maxTokens":1024,
                    "compaction":{"reserveTokens":2048,"keepRecentTokens":4000}
                }]
            }}
        }))
        .unwrap(),
    )
    .unwrap();
}

fn home(command: &mut Command, home: &std::path::Path) {
    command.env("HOME", home).env("USERPROFILE", home);
}

#[test]
fn help_and_argument_errors_do_not_create_user_state() {
    let temp = tempfile::tempdir().unwrap();
    let mut help = Command::new(binary());
    home(&mut help, temp.path());
    let output = help.arg("--help").stdin(Stdio::null()).output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Minimal native coding harness"));
    assert!(!temp.path().join(".mh").exists());

    let mut invalid = Command::new(binary());
    home(&mut invalid, temp.path());
    let output = invalid
        .args(["--api-key", "must-not-be-accepted"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!temp.path().join(".mh").exists());
    let mut removed = Command::new(binary());
    home(&mut removed, temp.path());
    let output = removed
        .args(["--thinking", "high"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!temp.path().join(".mh").exists());
}

#[test]
fn bare_resume_requires_a_terminal_without_creating_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = Command::new(binary());
    home(&mut command, temp.path());
    let output = command
        .arg("--resume")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Session picker requires a terminal"));
    assert!(!temp.path().join(".mh").exists());
}

#[test]
fn init_creates_config_templates_and_refreshes_only_schemas() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = Command::new(binary());
    home(&mut command, temp.path());
    let output = command.arg("--init").stdin(Stdio::null()).output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = temp.path().join(".mh/config");
    for name in [
        "config.json",
        "config.schema.json",
        "theme.json",
        "theme.schema.json",
    ] {
        assert!(config.join(name).is_file(), "missing {name}");
    }
    assert!(!temp.path().join(".mh/sessions").exists());
    let preserved = "{\"user\":\"preserved\"}\n";
    std::fs::write(config.join("config.json"), preserved).unwrap();
    std::fs::write(config.join("config.schema.json"), "modified").unwrap();

    let mut command = Command::new(binary());
    home(&mut command, temp.path());
    let output = command.arg("--init").stdin(Stdio::null()).output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        std::fs::read_to_string(config.join("config.json")).unwrap(),
        preserved
    );
    assert_eq!(
        std::fs::read_to_string(config.join("config.schema.json")).unwrap(),
        mh::setup::CONFIG_SCHEMA
    );
}

#[test]
fn startup_repairs_schemas_then_rejects_invalid_settings_without_creating_sessions() {
    let temp = tempfile::tempdir().unwrap();
    write_config(temp.path());
    let config = temp.path().join(".mh/config");
    std::fs::write(config.join("config.schema.json"), "{\"type\":\"null\"}").unwrap();
    std::fs::write(config.join("theme.schema.json"), "{\"type\":\"null\"}").unwrap();
    std::fs::write(
        config.join("theme.json"),
        "{\"themes\":\"private-diagnostic-marker\"}",
    )
    .unwrap();
    let mut command = Command::new(binary());
    home(&mut command, temp.path());
    let output = command
        .args(["--print", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("theme.json does not fulfill its schema"));
    assert!(!stderr.contains("private-diagnostic-marker"));
    assert!(output.stdout.is_empty());
    assert!(!temp.path().join(".mh/sessions").exists());
    assert_eq!(
        std::fs::read_to_string(config.join("config.schema.json")).unwrap(),
        mh::setup::CONFIG_SCHEMA
    );
    assert_eq!(
        std::fs::read_to_string(config.join("theme.schema.json")).unwrap(),
        mh::setup::THEME_SCHEMA
    );
    std::fs::write(
        config.join("config.json"),
        "{\"$schema\":\"./config.schema.json\"}",
    )
    .unwrap();
    let mut command = Command::new(binary());
    home(&mut command, temp.path());
    let output = command
        .args(["--print", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("config.json does not fulfill its schema")
    );
    assert!(!temp.path().join(".mh/sessions").exists());
}

#[test]
fn print_mode_selects_a_fixed_alias_keeps_stdout_clean_and_omits_the_question_tool() {
    let temp = tempfile::tempdir().unwrap();
    let home_dir = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_config(&home_dir);
    let config_path = home_dir.join(".mh/config/config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    let mut fast = config["providers"]["test"]["models"][0].clone();
    fast["alias"] = json!("fast");
    fast["reasoning"] = json!(true);
    fast["reasoningEffort"] = json!("low");
    let mut deep = fast.clone();
    deep["alias"] = json!("deep");
    deep["reasoningEffort"] = json!("high");
    config["defaultModel"] = json!("test/fast");
    config["providers"]["test"]["models"] = json!([fast, deep]);
    let configured = config.to_string();
    std::fs::write(&config_path, &configured).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = request(&mut stream);
            assert_eq!(request["model"], "model");
            assert_eq!(request["reasoning"]["effort"], "high");
            assert_eq!(request["store"], false);
            assert!(request.get("previous_response_id").is_none());
            let tools = request["tools"].as_array().unwrap();
            assert!(tools.iter().any(|tool| tool["name"] == "todo"));
            assert!(!tools.iter().any(|tool| tool["name"] == "ask_user_question"));
            let body = format!(
                "data: {}\n\n",
                json!({
                    "type":"response.completed",
                    "response":{"output":[{
                        "type":"message","id":"msg_1",
                        "content":[{"type":"output_text","text":"final answer","annotations":[]}]
                    }]}
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
    });

    let mut command = Command::new(binary());
    home(&mut command, &home_dir);
    let output = command
        .current_dir(&project)
        .args(["--print", "--model", "test/deep", "hello"])
        .env("MH_CLI_TEST_BASE", format!("http://{address}"))
        .env("MH_CLI_TEST_KEY", "test-key")
        .env("HTTP_PROXY", "")
        .env("HTTPS_PROXY", "")
        .env("NO_PROXY", "*")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), configured);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "final answer\n");
    assert!(output.stderr.is_empty());
    assert_eq!(
        std::fs::read_to_string(home_dir.join(".mh/config/config.schema.json")).unwrap(),
        mh::setup::CONFIG_SCHEMA
    );
    assert_eq!(
        std::fs::read_to_string(home_dir.join(".mh/config/theme.schema.json")).unwrap(),
        mh::setup::THEME_SCHEMA
    );
    for model in config["providers"]["test"]["models"]
        .as_array_mut()
        .unwrap()
    {
        let effort = model
            .as_object_mut()
            .unwrap()
            .remove("reasoningEffort")
            .unwrap();
        model["thinkingLevel"] = effort.clone();
        model["thinkingLevelMap"] = json!({"off":"none", "low":"low", "high":"high"});
    }
    let legacy = config.to_string();
    std::fs::write(&config_path, &legacy).unwrap();
    let mut command = Command::new(binary());
    home(&mut command, &home_dir);
    let output = command
        .current_dir(&project)
        .args(["--print", "--model", "test/deep", "legacy fixture"])
        .env("MH_CLI_TEST_BASE", format!("http://{address}"))
        .env("MH_CLI_TEST_KEY", "test-key")
        .env("HTTP_PROXY", "")
        .env("HTTPS_PROXY", "")
        .env("NO_PROXY", "*")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"final answer\n");
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), legacy);
    assert!(
        !std::fs::read_to_string(home_dir.join(".mh/config/config.schema.json"))
            .unwrap()
            .contains("thinkingLevel")
    );
    server.join().unwrap();
}

#[test]
fn print_mode_reports_redacted_provider_failures_on_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let home_dir = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_config(&home_dir);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = request(&mut stream);
        let body = json!({"error":{"code":"invalid_api_key","message":"RAW_PRIVATE_DIAGNOSTIC"}})
            .to_string();
        write!(stream,
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        ).unwrap();
    });
    let mut command = Command::new(binary());
    home(&mut command, &home_dir);
    let output = command
        .current_dir(&project)
        .args(["--print", "controlled failure"])
        .env("MH_CLI_TEST_BASE", format!("http://{address}"))
        .env("MH_CLI_TEST_KEY", "test-key")
        .env("HTTP_PROXY", "")
        .env("HTTPS_PROXY", "")
        .env("NO_PROXY", "*")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Provider returned HTTP 401"));
    assert!(!error.contains("RAW_PRIVATE_DIAGNOSTIC"));
    assert!(!error.contains("test-key"));
    assert!(!error.contains(&address.to_string()));
    server.join().unwrap();
}

#[test]
fn print_mode_returns_two_and_preserves_raw_redirected_partial_answers() {
    let temp = tempfile::tempdir().unwrap();
    let home_dir = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_config(&home_dir);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let answer = "partial answer\x1b]52;c;ZmFrZQ==\x07\x1b[2J";
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = request(&mut stream);
        let body = format!(
            "data: {}\n\n",
            json!({
                "type":"response.incomplete",
                "response":{
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "output":[{
                        "type":"message","id":"msg_partial",
                        "content":[{"type":"output_text","text":answer,"annotations":[]}]
                    }]
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
    });
    let mut command = Command::new(binary());
    home(&mut command, &home_dir);
    let output = command
        .current_dir(&project)
        .args(["--print", "hello"])
        .env("MH_CLI_TEST_BASE", format!("http://{address}"))
        .env("MH_CLI_TEST_KEY", "test-key")
        .env("HTTP_PROXY", "")
        .env("HTTPS_PROXY", "")
        .env("NO_PROXY", "*")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, format!("{answer}\n").as_bytes());
    assert!(output.stderr.is_empty());
    server.join().unwrap();
}
