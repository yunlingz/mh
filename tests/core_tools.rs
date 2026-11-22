use mh::message::{Content, ToolCall};
use mh::tools::{CoreTools, Execution};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn fixture() -> (tempfile::TempDir, CoreTools) {
    let temp = tempfile::tempdir().unwrap();
    let tools = CoreTools::new(temp.path().to_owned(), temp.path().join("outputs")).unwrap();
    (temp, tools)
}

fn shell_command(unix: &str, powershell: &str) -> String {
    if cfg!(windows) { powershell } else { unix }.to_owned()
}

fn native_fixture_command() -> String {
    let executable = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    if cfg!(windows) {
        format!(
            "& '{}' --exact shell_native_subprocess_fixture --nocapture",
            executable.replace('\'', "''")
        )
    } else {
        format!(
            "'{}' --exact shell_native_subprocess_fixture --nocapture",
            executable.replace('\'', "'\\''")
        )
    }
}

fn fixture_execution(
    id: &str,
    cancel: CancellationToken,
    temp: &tempfile::TempDir,
    mode: &str,
) -> Execution {
    let mut execution = Execution::new(id, cancel);
    execution
        .metadata
        .insert("MH_TOOL_FIXTURE_MODE".into(), mode.into());
    execution.metadata.insert(
        "MH_TOOL_FIXTURE_ROOT".into(),
        temp.path().to_string_lossy().into_owned(),
    );
    execution.metadata.insert(
        "MH_TOOL_FIXTURE_EXE".into(),
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    execution
}

// Executed only by this test suite inside its own temporary workspace. A native
// Rust child avoids depending on Bash/Unix utilities in Windows fixtures.
#[test]
fn shell_native_subprocess_fixture() {
    let Ok(mode) = std::env::var("MH_TOOL_FIXTURE_MODE") else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("MH_TOOL_FIXTURE_ROOT").unwrap());
    assert!(root.is_dir());
    if mode == "descendant" || mode == "quiet-descendant" {
        std::fs::write(root.join("started.txt"), "ready").unwrap();
        println!("fixture-ready");
        for _ in 0..if cfg!(windows) { 70 } else { 14 } {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if mode == "descendant" {
                // Keep inherited pipes active past the executor's idle grace.
                println!("fixture-tick");
            }
        }
        std::fs::write(root.join("escaped.txt"), "leaked").unwrap();
    } else {
        assert_eq!(mode, "exit");
        println!("native stdout héllo € 🙂");
        eprintln!("native stderr héllo € 🙂");
        std::process::exit(7);
    }
}

async fn call(tools: &CoreTools, name: &str, id: &str, args: Value) -> mh::message::ToolResult {
    let call = ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    };
    tools
        .execute(&call, &Execution::new(id, CancellationToken::new()))
        .await
}

#[tokio::test]
async fn write_read_and_ranges_use_real_files() {
    let (temp, tools) = fixture();
    let result = call(
        &tools,
        "write",
        "write",
        json!({"path":"nested/f.txt","content":"one\ntwo\nthree\n"}),
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("nested/f.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
    let result = call(
        &tools,
        "read",
        "read",
        json!({"path":"nested/f.txt","offset":2,"limit":1}),
    )
    .await;
    assert!(!result.is_error);
    assert!(result.plain_text().starts_with("two\n\n"));
    assert!(result.plain_text().contains("offset=3"));
    assert!(
        call(
            &tools,
            "read",
            "bad",
            json!({"path":"nested/f.txt","offset":100})
        )
        .await
        .is_error
    );
}

#[tokio::test]
async fn read_head_stops_at_first_byte_overflow_without_skipping_lines() {
    let (temp, tools) = fixture();
    let first = "a".repeat(30_000);
    let second = "b".repeat(30_000);
    std::fs::write(
        temp.path().join("large.txt"),
        format!("{first}\n{second}\nMUST_NOT_APPEAR\n"),
    )
    .unwrap();
    let result = call(&tools, "read", "large", json!({"path":"large.txt"})).await;
    assert!(!result.is_error);
    assert!(result.plain_text().starts_with(&first));
    assert!(!result.plain_text().contains("MUST_NOT_APPEAR"));
    assert!(result.plain_text().contains("offset=2"));
}

#[tokio::test]
async fn read_caps_lines_and_detects_images_from_magic() {
    let (temp, tools) = fixture();
    let lines = (1..=2100)
        .map(|i| format!("line {i}\n"))
        .collect::<String>();
    std::fs::write(temp.path().join("many.txt"), lines).unwrap();
    let result = call(&tools, "read", "many", json!({"path":"many.txt"})).await;
    assert!(result.plain_text().contains("line 2000\n\n["));
    assert!(result.plain_text().contains("offset=2001"));
    let image = image::RgbImage::from_pixel(2, 2, image::Rgb([10u8, 20, 30]));
    image
        .save_with_format(temp.path().join("image.data"), image::ImageFormat::Bmp)
        .unwrap();
    let result = call(&tools, "read", "image", json!({"path":"image.data"})).await;
    assert!(!result.is_error);
    assert!(result.content.iter().any(
        |block| matches!(block, Content::Image { mime_type, .. } if mime_type == "image/png")
    ));

    std::fs::write(
        temp.path().join("malformed-bmp"),
        [b"BM".as_slice(), &[0; 64]].concat(),
    )
    .unwrap();
    let result = call(
        &tools,
        "read",
        "malformed-bmp",
        json!({"path":"malformed-bmp"}),
    )
    .await;
    assert!(
        result
            .content
            .iter()
            .all(|block| matches!(block, Content::Text { .. }))
    );

    let mut apng = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    apng.extend_from_slice(&13_u32.to_be_bytes());
    apng.extend_from_slice(b"IHDR");
    apng.extend_from_slice(&[0; 17]);
    apng.extend_from_slice(&8_u32.to_be_bytes());
    apng.extend_from_slice(b"acTL");
    apng.extend_from_slice(&[0; 12]);
    std::fs::write(temp.path().join("animated.png"), apng).unwrap();
    let result = call(&tools, "read", "animated", json!({"path":"animated.png"})).await;
    assert!(
        result
            .content
            .iter()
            .all(|block| matches!(block, Content::Text { .. }))
    );
}

#[tokio::test]
async fn edits_preserve_bom_and_mixed_line_endings() {
    let (temp, tools) = fixture();
    let path = temp.path().join("mixed.txt");
    std::fs::write(&path, "\u{feff}alpha\r\nbeta\ngamma\r\n").unwrap();
    let result = call(
        &tools,
        "edit",
        "edit",
        json!({"path":"mixed.txt","edits":[
            {"oldText":"alpha\n","newText":"ALPHA\nextra\n"},
            {"oldText":"gamma","newText":"GAMMA"}
        ]}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "\u{feff}ALPHA\r\nextra\r\nbeta\nGAMMA\r\n"
    );
    assert!(result.details["patch"].as_str().unwrap().contains("@@"));
    assert_eq!(result.details["firstChangedLine"], 1);
}

#[tokio::test]
async fn invalid_and_overlapping_edits_leave_the_file_unchanged() {
    let (temp, tools) = fixture();
    let path = temp.path().join("f.txt");
    std::fs::write(&path, "alpha beta gamma\n").unwrap();
    for edits in [
        json!([{ "oldText":"alpha", "newText":"A" },{ "oldText":"missing", "newText":"M" }]),
        json!([{ "oldText":"alpha beta", "newText":"A" },{ "oldText":"beta gamma", "newText":"B" }]),
    ] {
        assert!(
            call(
                &tools,
                "edit",
                "invalid",
                json!({"path":"f.txt","edits":edits})
            )
            .await
            .is_error
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha beta gamma\n"
        );
    }
}

#[tokio::test]
async fn edits_reject_invalid_utf8_without_changing_unrelated_bytes() {
    let (temp, tools) = fixture();
    let path = temp.path().join("binary.txt");
    let original = b"target\n\xffunrelated\n";
    std::fs::write(&path, original).unwrap();
    let result = call(
        &tools,
        "edit",
        "invalid-utf8",
        json!({"path":"binary.txt","edits":[{"oldText":"target","newText":"changed"}]}),
    )
    .await;
    assert!(result.is_error);
    assert!(result.plain_text().contains("not valid UTF-8"));
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[tokio::test]
async fn edits_reject_overlapping_occurrences_of_the_same_needle() {
    let (temp, tools) = fixture();
    let path = temp.path().join("ambiguous.txt");
    for (original, needle) in [("aaa", "aa"), ("ababa", "aba"), ("🙂🙂🙂", "🙂🙂")] {
        std::fs::write(&path, original).unwrap();
        let result = call(
            &tools,
            "edit",
            "ambiguous",
            json!({"path":"ambiguous.txt","edits":[{"oldText":needle,"newText":"changed"}]}),
        )
        .await;
        assert!(result.is_error, "{}", result.plain_text());
        assert!(result.plain_text().contains("2 occurrences"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

#[tokio::test]
async fn fuzzy_substrings_preserve_unrelated_literals_and_unterminated_patch_lines() {
    let (temp, tools) = fixture();
    let file = temp.path().join("f.txt");
    std::fs::write(&file, "const kept = \"ＦＵＬＬ\"; target = “old”;  ").unwrap();
    let result = call(
        &tools,
        "edit",
        "substring",
        json!({"path":"f.txt","oldText":"\"old\"","newText":"\"new\""}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "const kept = \"ＦＵＬＬ\"; target = \"new\";  "
    );
    assert!(
        result.details["patch"]
            .as_str()
            .unwrap()
            .contains("\\ No newline at end of file")
    );
    let read = call(
        &tools,
        "read",
        "negative-footer",
        json!({"path":"f.txt","limit":-1}),
    )
    .await;
    assert!(
        read.plain_text()
            .contains("1 more lines in file. Use offset=1")
    );
}

#[tokio::test]
async fn fuzzy_edits_preserve_unrelated_lines_and_support_legacy_input() {
    let (temp, tools) = fixture();
    std::fs::write(
        temp.path().join("f.txt"),
        "smart \u{201c}quote\u{201d}  \nuntouched \u{2014} line  \n",
    )
    .unwrap();
    let result = call(
        &tools,
        "edit",
        "fuzzy",
        json!({"path":"f.txt","oldText":"smart \"quote\"","newText":"changed"}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("f.txt")).unwrap(),
        "changed  \nuntouched \u{2014} line  \n"
    );
}

#[tokio::test]
async fn edit_diff_is_bounded_around_a_small_change_in_a_large_file() {
    let (temp, tools) = fixture();
    let mut content = "unchanged line\n".repeat(175_000);
    content.push_str("unique target line\n");
    content.push_str(&"unchanged line\n".repeat(175_000));
    std::fs::write(temp.path().join("large.txt"), content).unwrap();
    let result = call(
        &tools,
        "edit",
        "large-edit",
        json!({"path":"large.txt","edits":[{
            "oldText":"unique target line",
            "newText":"changed target line"
        }]}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert!(result.details["diff"].as_str().unwrap().len() < 2_000);
    assert!(result.details["patch"].as_str().unwrap().len() < 2_000);
    assert!(
        std::fs::read_to_string(temp.path().join("large.txt"))
            .unwrap()
            .contains("changed target line")
    );
}

#[tokio::test]
async fn concurrent_disjoint_edits_are_serialized() {
    let (temp, tools) = fixture();
    std::fs::write(temp.path().join("f.txt"), "one two\n").unwrap();
    let (a, b) = tokio::join!(
        call(
            &tools,
            "edit",
            "a",
            json!({"path":"f.txt","edits":[{"oldText":"one","newText":"ONE"}]})
        ),
        call(
            &tools,
            "edit",
            "b",
            json!({"path":"f.txt","edits":[{"oldText":"two","newText":"TWO"}]})
        )
    );
    assert!(!a.is_error && !b.is_error);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("f.txt")).unwrap(),
        "ONE TWO\n"
    );
}

#[tokio::test]
async fn bash_returns_both_streams_and_retains_line_truncated_output() {
    let (temp, tools) = fixture();
    let result = call(
        &tools,
        "bash",
        "both",
        json!({"command":shell_command("printf stdout; printf stderr >&2; exit 7", "[Console]::Out.Write('stdout'); [Console]::Error.Write('stderr'); exit 7")}),
    )
    .await;
    assert!(result.is_error);
    assert!(result.plain_text().contains("stdout"));
    assert!(result.plain_text().contains("stderr"));
    assert_eq!(result.structured_content.as_ref().unwrap()["exit_code"], 7);
    let result = call(
        &tools,
        "bash",
        "large",
        json!({"command":shell_command("i=1; while [ \"$i\" -le 2100 ]; do printf 'line %s\\n' \"$i\"; i=$((i+1)); done", "1..2100 | ForEach-Object { [Console]::Out.Write(\"line $_`n\") }")}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert!(result.plain_text().contains("line 2100"));
    assert!(
        result
            .plain_text()
            .contains("Showing lines 101-2100 of 2100. Full output")
    );
    assert!(!result.plain_text().contains("50.0KB limit"));
    let path = result.details["fullOutputPath"].as_str().unwrap();
    assert!(std::path::Path::new(path).starts_with(temp.path().join("outputs")));
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .starts_with("line 1\n")
    );

    let result = call(
        &tools,
        "bash",
        "long-line",
        json!({"command":shell_command("printf '%060000d' 0", "[Console]::Out.Write(('0' * 60000))")}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert!(
        result
            .plain_text()
            .contains("Showing last 50.0KB of line 1")
    );
    assert!(result.plain_text().contains("line is 58.6KB"));
}

fn descendant_command(wait: bool) -> String {
    if cfg!(windows) {
        let spawn = "$child = Start-Process -FilePath $env:MH_TOOL_FIXTURE_EXE -ArgumentList '--exact shell_native_subprocess_fixture --nocapture' -NoNewWindow -PassThru";
        let suffix = if wait {
            "$child.WaitForExit()"
        } else {
            "while (-not (Test-Path started.txt)) { Start-Sleep -Milliseconds 10 }; exit 0"
        };
        format!("{spawn}; {suffix}")
    } else {
        format!(
            "({}) & {}",
            native_fixture_command(),
            if wait {
                "wait"
            } else {
                "while [ ! -f started.txt ]; do sleep 0.01; done"
            }
        )
    }
}

#[tokio::test]
async fn bash_timeout_kills_the_process_group_or_job() {
    for (id, wait) in [("timeout", true), ("pipe-timeout", false)] {
        let (temp, tools) = fixture();
        let timeout = if cfg!(windows) { 2.0 } else { 0.2 };
        let call = ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command":descendant_command(wait), "timeout":timeout}).to_string(),
        };
        let result = tools
            .execute(
                &call,
                &fixture_execution(id, CancellationToken::new(), &temp, "descendant"),
            )
            .await;
        assert!(result.is_error, "{}", result.plain_text());
        // Prove a descendant actually started; timing out during shell startup
        // alone would not exercise process-tree termination.
        assert!(
            temp.path().join("started.txt").exists(),
            "{}",
            result.plain_text()
        );
        if wait {
            assert!(
                result.plain_text().contains("timed out"),
                "{}",
                result.plain_text()
            );
        } else {
            assert!(
                result.plain_text().contains("exited with code 0"),
                "{}",
                result.plain_text()
            );
            assert_eq!(result.structured_content.as_ref().unwrap()["exit_code"], 0);
        }
        tokio::time::sleep(std::time::Duration::from_millis(if cfg!(windows) {
            1800
        } else {
            800
        }))
        .await;
        assert!(!temp.path().join("escaped.txt").exists());
    }
}

#[tokio::test]
async fn bash_cancellation_and_dropped_execution_kill_descendants() {
    for drop_future in [false, true] {
        let (temp, tools) = fixture();
        let cancel = CancellationToken::new();
        let execution = fixture_execution("cancel-shell", cancel.clone(), &temp, "descendant");
        let call = ToolCall {
            id: "cancel-shell".into(),
            name: "bash".into(),
            arguments: json!({"command":descendant_command(true)}).to_string(),
        };
        let task = tokio::spawn(async move { tools.execute(&call, &execution).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !temp.path().join("started.txt").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant must start in the fixture");
        if drop_future {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            cancel.cancel();
            let result = task.await.unwrap();
            assert!(result.is_error);
            assert!(result.plain_text().contains("Command aborted"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(if cfg!(windows) {
            3600
        } else {
            800
        }))
        .await;
        assert!(!temp.path().join("escaped.txt").exists());
    }
}

#[tokio::test]
async fn bash_preserves_multiline_quotes_unicode_cwd_and_environment() {
    let (temp, tools) = fixture();
    let command = shell_command(
        "value='héllo € 🙂 '\"'\"'quoted'\"'\"''\nprintf '%s\\n' \"$value\"\nprintf '%s\\n' \"$MH_TEST_METADATA\"\nprintf ok > 'space file.txt'\nprintf 'stderr héllo € 🙂\\n' >&2",
        "$value = 'héllo € 🙂 ''quoted'''\n[Console]::Out.WriteLine($value)\n[Console]::Out.WriteLine($env:MH_TEST_METADATA)\n[IO.File]::WriteAllText((Join-Path (Get-Location) 'space file.txt'), 'ok')\n[Console]::Error.WriteLine('stderr héllo € 🙂')",
    );
    let mut execution = Execution::new("unicode", CancellationToken::new());
    execution
        .metadata
        .insert("MH_TEST_METADATA".into(), "metadata héllo € 🙂".into());
    let call = ToolCall {
        id: "unicode".into(),
        name: "bash".into(),
        arguments: json!({"command":command}).to_string(),
    };
    let result = tools.execute(&call, &execution).await;
    assert!(!result.is_error, "{}", result.plain_text());
    for expected in [
        "héllo € 🙂 'quoted'",
        "metadata héllo € 🙂",
        "stderr héllo € 🙂",
    ] {
        assert!(
            result.plain_text().contains(expected),
            "{}",
            result.plain_text()
        );
    }
    assert_eq!(
        std::fs::read_to_string(temp.path().join("space file.txt")).unwrap(),
        "ok"
    );
}

#[tokio::test]
async fn bash_reports_native_subprocess_status_and_unicode_errors() {
    let (temp, tools) = fixture();
    let native_command = native_fixture_command();
    let execution = fixture_execution("native", CancellationToken::new(), &temp, "exit");
    for (id, command, expected) in [
        (
            "native",
            native_command.clone(),
            if cfg!(windows) { 1 } else { 7 },
        ),
        (
            "native-explicit",
            shell_command(
                &native_command,
                &format!("{native_command}; exit $LASTEXITCODE"),
            ),
            7,
        ),
    ] {
        let call = ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command":command}).to_string(),
        };
        let result = tools
            .execute(
                &call,
                &Execution {
                    call_id: id.into(),
                    ..execution.clone()
                },
            )
            .await;
        assert!(result.is_error);
        assert_eq!(
            result.structured_content.as_ref().unwrap()["exit_code"],
            expected,
            "{}",
            result.plain_text()
        );
        assert!(
            result.plain_text().contains("native stdout héllo € 🙂"),
            "{}",
            result.plain_text()
        );
        assert!(
            result.plain_text().contains("native stderr héllo € 🙂"),
            "{}",
            result.plain_text()
        );
    }
    for command in [
        shell_command(
            "printf 'error héllo € 🙂\\n' >&2; exit 1",
            "Write-Error 'error héllo € 🙂'",
        ),
        shell_command("missing_fixture_command", "missing_fixture_command"),
    ] {
        let result = call(&tools, "bash", "shell-error", json!({"command":command})).await;
        assert!(result.is_error);
        assert!(!result.plain_text().contains("#< CLIXML"));
    }
}

#[tokio::test]
async fn bash_bounded_streaming_does_not_wait_for_full_update_channels() {
    let (temp, tools) = fixture();
    let command = shell_command(
        "i=0; while [ \"$i\" -lt 25000 ]; do printf '%064d\\n' \"$i\"; i=$((i+1)); done",
        "0..24999 | ForEach-Object { [Console]::Out.Write(($_.ToString().PadLeft(64, '0') + \"`n\")) }",
    );
    let (sender, _undrained) = tokio::sync::mpsc::channel(1);
    let mut execution = Execution::new("bounded", CancellationToken::new());
    execution.updates = Some(sender);
    let call = ToolCall {
        id: "bounded".into(),
        name: "bash".into(),
        arguments: json!({"command":command, "timeout":20}).to_string(),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(25),
        tools.execute(&call, &execution),
    )
    .await
    .unwrap();
    assert!(!result.is_error, "{}", result.plain_text());
    assert!(result.plain_text().len() < 53_000);
    let structured = result.structured_content.as_ref().unwrap();
    assert!(structured["truncated"].as_bool().unwrap());
    // The bounded head/tail payload also carries a short omission marker.
    assert!(structured["output"].as_str().unwrap().len() <= 1024 * 1024 + 100);
    let retained = std::path::Path::new(result.details["fullOutputPath"].as_str().unwrap());
    assert!(retained.starts_with(temp.path().join("outputs")));
    assert_eq!(std::fs::metadata(retained).unwrap().len(), 25_000 * 65);
}

#[cfg(windows)]
#[tokio::test]
async fn bash_successful_background_job_is_owned_until_session_drop() {
    let (temp, tools) = fixture();
    let call = ToolCall {
        id: "background".into(),
        name: "bash".into(),
        arguments: json!({"command":descendant_command(false)}).to_string(),
    };
    let execution = fixture_execution(
        "background",
        CancellationToken::new(),
        &temp,
        "quiet-descendant",
    );
    let result = tools.execute(&call, &execution).await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert!(temp.path().join("started.txt").exists());
    drop(tools);
    tokio::time::sleep(std::time::Duration::from_millis(3600)).await;
    assert!(!temp.path().join("escaped.txt").exists());
}

#[tokio::test]
async fn bash_invalid_arguments_and_malformed_scripts_do_not_execute_prefixes() {
    let (temp, tools) = fixture();
    let write = shell_command(
        "printf bad > must-not-exist",
        "Set-Content must-not-exist 'bad'",
    );
    for args in [
        json!({"command":&write,"timeout":0}),
        json!({"command":&write,"timeout":-1}),
        json!({"command":&write,"timeout":2147483.648}),
        json!({"command":&write,"extra":true}),
        json!({"command":7}),
    ] {
        assert!(call(&tools, "bash", "bad-args", args).await.is_error);
        assert!(!temp.path().join("must-not-exist").exists());
    }
    let truncated = ToolCall {
        id: "incomplete-json".into(),
        name: "bash".into(),
        arguments: format!("{{\"command\":{}", json!(write)),
    };
    assert!(
        tools
            .execute(
                &truncated,
                &Execution::new("incomplete-json", CancellationToken::new())
            )
            .await
            .is_error
    );
    assert!(!temp.path().join("must-not-exist").exists());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let cancelled_call = ToolCall {
        id: "cancel-before-shell".into(),
        name: "bash".into(),
        arguments: json!({"command":write}).to_string(),
    };
    assert!(
        tools
            .execute(
                &cancelled_call,
                &Execution::new("cancel-before-shell", cancelled)
            )
            .await
            .is_error
    );
    assert!(!temp.path().join("must-not-exist").exists());
    // PowerShell parses the complete transported script. Unix shells deliberately
    // retain their existing incremental parser semantics, so put the side effect
    // inside a malformed compound statement rather than promise atomic parsing.
    for command in if cfg!(windows) {
        [
            "Write-Output done `",
            "Get-ChildItem |",
            "$true -and",
            "Write-Output a,",
            "$x =",
            "Write-Output (1 +",
            "Write-Output \"unclosed",
            "if ($true) {",
        ]
        .iter()
        .map(|tail| format!("{write}; {tail}"))
        .collect::<Vec<_>>()
    } else {
        vec![format!("if true; then {write};"), format!("({write};")]
    } {
        let result = call(&tools, "bash", "malformed", json!({"command":command})).await;
        assert!(result.is_error, "{}", result.plain_text());
        assert!(!temp.path().join("must-not-exist").exists());
    }
    if cfg!(windows) {
        let result = call(
            &tools,
            "bash",
            "oversized",
            json!({"command":format!("{write}; #{}", "x".repeat(20_000))}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.plain_text().contains("command-line limit"));
        assert!(!temp.path().join("must-not-exist").exists());
    }
}

#[tokio::test]
async fn cancellation_before_execution_does_not_write() {
    let (temp, tools) = fixture();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let call = ToolCall {
        id: "cancel".into(),
        name: "write".into(),
        arguments: json!({"path":"f.txt","content":"bad"}).to_string(),
    };
    let result = tools
        .execute(&call, &Execution::new("cancel", cancel))
        .await;
    assert!(result.is_error);
    assert!(!temp.path().join("f.txt").exists());
}

#[tokio::test]
async fn read_accepts_pi_numeric_offsets_and_legacy_slicing() {
    let (temp, tools) = fixture();
    std::fs::write(temp.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
    for offset in [1.0, -2.0] {
        let result = call(
            &tools,
            "read",
            "numeric",
            json!({"path":"f.txt","offset":offset,"limit":1.0}),
        )
        .await;
        assert!(!result.is_error);
        assert!(result.plain_text().starts_with("one\n\n"));
    }
    let fractional = call(
        &tools,
        "read",
        "fractional",
        json!({"path":"f.txt","offset":1.5,"limit":1.0}),
    )
    .await;
    assert!(!fractional.is_error);
    assert!(fractional.plain_text().starts_with("one\n\n"));
    assert!(fractional.plain_text().contains("offset=2.5"));
    let negative = call(
        &tools,
        "read",
        "negative",
        json!({"path":"f.txt","limit":-1.0}),
    )
    .await;
    assert!(!negative.is_error);
    assert!(negative.plain_text().starts_with("one\ntwo\nthree\n\n"));
}

#[tokio::test]
async fn numeric_reads_match_pi_slicing_without_materializing_the_file() {
    fn index(value: f64, length: usize) -> usize {
        let value = value.trunc();
        if value < 0.0 {
            (length as f64 + value).max(0.0) as usize
        } else {
            (value as usize).min(length)
        }
    }
    let (temp, tools) = fixture();
    for content in ["", "\n", "one\ntwo\nthree\n", "one\r\ntwo\nlast"] {
        std::fs::write(temp.path().join("ranges.txt"), content).unwrap();
        let lines = content.split('\n').collect::<Vec<_>>();
        for offset in [-9.0_f64, -0.5, 0.0, 0.5, 1.0, 1.5, 2.5, 5.0] {
            let start_number = if offset == 0.0 {
                0.0
            } else {
                (offset - 1.0).max(0.0)
            };
            for limit in [
                None,
                Some(-10.0),
                Some(-2.5),
                Some(-1.0),
                Some(-0.5),
                Some(0.0),
                Some(0.5),
                Some(1.0),
                Some(1.5),
                Some(3.9),
            ] {
                let result = call(
                    &tools,
                    "read",
                    "ranges",
                    json!({"path":"ranges.txt","offset":offset,"limit":limit}),
                )
                .await;
                if start_number >= lines.len() as f64 {
                    assert!(result.is_error);
                    continue;
                }
                assert!(!result.is_error, "{}", result.plain_text());
                let start = index(start_number, lines.len());
                let end = limit.map_or(lines.len(), |limit| {
                    index((start_number + limit).min(lines.len() as f64), lines.len())
                });
                let expected = if end >= start {
                    lines[start..end].join("\n")
                } else {
                    String::new()
                };
                let output = result.plain_text();
                let selected = output.split("\n\n[").next().unwrap();
                assert_eq!(
                    selected, expected,
                    "offset={offset}, limit={limit:?}, content={content:?}"
                );
            }
        }
    }
    let content = format!("{}\nmiddle\nlast\n", "x".repeat(16 * 1024 * 1024));
    std::fs::write(temp.path().join("large-range.txt"), content).unwrap();
    let head = call(
        &tools,
        "read",
        "fractional-large",
        json!({"path":"large-range.txt","offset":1.5,"limit":0.5}),
    )
    .await;
    assert!(!head.is_error);
    assert!(head.plain_text().contains("exceeds 50.0KB limit"));
    assert!(head.plain_text().len() < 300);
    let tail = call(
        &tools,
        "read",
        "negative-large",
        json!({"path":"large-range.txt","offset":2.5,"limit":-2.5}),
    )
    .await;
    assert!(!tail.is_error);
    assert!(tail.plain_text().starts_with("middle\nlast\n\n["));
}

#[tokio::test]
async fn huge_line_edits_keep_useful_byte_bounded_metadata_before_writing() {
    use mh::tools::output::{MAX_BYTES, MAX_LINES};
    let (temp, tools) = fixture();
    let original = format!(
        "{}left target right{}",
        "🙂".repeat(2 * 1024 * 1024),
        "z".repeat(1024 * 1024)
    );
    let path = temp.path().join("long-line.txt");
    std::fs::write(&path, &original).unwrap();
    let result = call(
        &tools,
        "edit",
        "long-line-edit",
        json!({"path":"long-line.txt","edits":[{"oldText":"target","newText":"changed"}]}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original.replacen("target", "changed", 1)
    );
    for key in ["diff", "patch"] {
        let text = result.details[key].as_str().unwrap();
        assert!(text.len() <= MAX_BYTES, "{key} is {} bytes", text.len());
        assert!(text.lines().count() <= MAX_LINES);
        assert!(text.contains("left target right"));
        assert!(text.contains("left changed right"));
        assert!(text.contains("..."));
    }
    assert!(
        result.details["patch"]
            .as_str()
            .unwrap()
            .contains("not an applicable unified patch")
    );
    assert_eq!(result.details["firstChangedLine"], 1);
    assert!(serde_json::to_vec(&result).unwrap().len() < 3 * MAX_BYTES);
    // Verify the original post-mutation failure boundary, not just display size.
    use mh::message::{Assistant, Message, OutputItem, StopReason};
    use mh::session::{Entry, Session};
    use std::sync::Arc;
    let session = Session::create(
        &mh::storage::Layout {
            root: temp.path().join("mh"),
        },
        temp.path(),
    )
    .unwrap();
    session.append(Entry::Message { message: Arc::new(Message::Assistant { response: Assistant {
        provider: "fixture".into(), model: "fixture".into(), stop: StopReason::ToolUse,
        usage: None, error: None, items: vec![OutputItem::FunctionCall {
            id: None, call_id: "long-line-edit".into(), name: "edit".into(),
            arguments: json!({"path":"long-line.txt","edits":[{"oldText":"target","newText":"changed"}]}).to_string(),
        }],
    } }) }).await.unwrap();
    session
        .append(Entry::Message {
            message: Arc::new(Message::ToolResult {
                call_id: "long-line-edit".into(),
                name: "edit".into(),
                result,
            }),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn edit_metadata_is_bounded_for_many_disjoint_changes_and_large_replacements() {
    use mh::tools::output::{MAX_BYTES, MAX_LINES};
    let (temp, tools) = fixture();
    let content = (0..2400)
        .map(|number| format!("unique-{number}\n"))
        .collect::<String>();
    std::fs::write(temp.path().join("many-edits.txt"), &content).unwrap();
    let edits = (0..2400).map(|number| json!({"oldText":format!("unique-{number}\n"),"newText":format!("changed-{number}\n")})).collect::<Vec<_>>();
    let result = call(
        &tools,
        "edit",
        "many-edits",
        json!({"path":"many-edits.txt","edits":edits}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    for key in ["diff", "patch"] {
        let text = result.details[key].as_str().unwrap();
        assert!(text.len() <= MAX_BYTES);
        assert!(text.lines().count() <= MAX_LINES);
        assert!(text.contains("truncated"));
    }
    std::fs::write(
        temp.path().join("replacement.txt"),
        "one\nkeep\nold-body\nlast\n",
    )
    .unwrap();
    let replacement = format!("new-start\n{}new-end", "long line\n".repeat(5000));
    let result = call(&tools, "edit", "large-replacement", json!({"path":"replacement.txt","edits":[{"oldText":"one","newText":"one"},{"oldText":"old-body","newText":replacement}]})).await;
    assert!(!result.is_error);
    assert_eq!(result.details["firstChangedLine"], 3);
    assert!(
        result.details["diff"]
            .as_str()
            .unwrap()
            .contains("new-start")
    );
    assert!(result.details["diff"].as_str().unwrap().contains("new-end"));
}

#[tokio::test]
async fn ordinary_disjoint_hunks_stop_metadata_at_byte_budget_without_losing_edits() {
    use mh::tools::output::{MAX_BYTES, MAX_LINES};
    let (temp, tools) = fixture();
    let mut content = String::new();
    let mut edits = Vec::new();
    for index in 0..100 {
        content.push_str(&format!("unique target {index}: {}\n", "x".repeat(200)));
        content.push_str(&format!("unchanged {}\n", "y".repeat(200)).repeat(15));
        edits.push(json!({"oldText":format!("unique target {index}:"),"newText":format!("changed target {index}:")}));
    }
    std::fs::write(temp.path().join("hunks.txt"), content).unwrap();
    let result = call(
        &tools,
        "edit",
        "bounded-hunks",
        json!({"path":"hunks.txt","edits":edits}),
    )
    .await;
    assert!(!result.is_error, "{}", result.plain_text());
    for key in ["diff", "patch"] {
        let text = result.details[key].as_str().unwrap();
        assert!(text.len() <= MAX_BYTES);
        assert!(text.lines().count() <= MAX_LINES);
        assert!(text.contains("truncated"));
        assert!(text.contains("changed target 0"));
    }
    assert!(result.details["patch"].as_str().unwrap().contains("@@"));
    assert!(
        std::fs::read_to_string(temp.path().join("hunks.txt"))
            .unwrap()
            .contains("changed target 99")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn file_tools_reject_fifos_and_directories_without_waiting_for_cancellation() {
    let (temp, tools) = fixture();
    let pipe = temp.path().join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .unwrap()
            .success()
    );
    for (name, args) in [
        ("read", json!({"path":"pipe"})),
        ("write", json!({"path":"pipe","content":"must not write"})),
        (
            "edit",
            json!({"path":"pipe","edits":[{"oldText":"x","newText":"y"}]}),
        ),
    ] {
        let owned_tools = tools.clone();
        let call = ToolCall {
            id: "fifo".into(),
            name: name.into(),
            arguments: args.to_string(),
        };
        let cancel = CancellationToken::new();
        let execution = Execution::new("fifo", cancel.clone());
        let mut task = tokio::spawn(async move { owned_tools.execute(&call, &execution).await });
        let settled = tokio::time::timeout(std::time::Duration::from_secs(1), &mut task).await;
        match settled {
            Ok(result) => assert!(result.unwrap().is_error),
            Err(_) => {
                // Unblock a faulty implementation inside the fixture before
                // failing, so a blocked spawn_blocking worker cannot hang tests.
                cancel.cancel();
                let peer = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&pipe)
                    .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                drop(peer);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
                panic!("{name} blocked on a FIFO");
            }
        }
    }
    for (name, args) in [
        ("read", json!({"path":"outputs"})),
        (
            "write",
            json!({"path":"outputs","content":"must not write"}),
        ),
        (
            "edit",
            json!({"path":"outputs","edits":[{"oldText":"x","newText":"y"}]}),
        ),
    ] {
        assert!(call(&tools, name, "directory", args).await.is_error);
    }
}
