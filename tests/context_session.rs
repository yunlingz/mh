use std::io::Write;
use std::sync::Arc;

use mh::config::CompactionConfig;
use mh::context::{ProjectSkills, Resources, compaction};
use mh::message::{Assistant, Message, OutputItem, OutputText, StopReason, ToolResult, Usage};
use mh::session::{Entry, PresentationItem, Session, SessionError, UsageKind};
use mh::storage::Layout;

fn fixture() -> (tempfile::TempDir, Layout, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    (temp, layout, cwd)
}

fn answer(text: &str, usage: Usage) -> Message {
    Message::Assistant {
        response: Assistant {
            provider: "work".into(),
            model: "model".into(),
            stop: StopReason::Stop,
            items: vec![OutputItem::Message {
                id: None,
                phase: None,
                content: vec![OutputText::OutputText {
                    text: text.into(),
                    annotations: Vec::new(),
                }],
            }],
            usage: Some(usage),
            error: None,
        },
    }
}

async fn add(session: &Session, message: Message) -> u64 {
    session
        .append(Entry::Message {
            message: Arc::new(message),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn rejected_large_records_leave_the_session_writer_healthy() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    let before = std::fs::metadata(session.file()).unwrap().len();
    let result = session
        .append(Entry::Message {
            message: Arc::new(Message::user("\"".repeat(17 * 1024 * 1024))),
        })
        .await;
    assert!(matches!(result, Err(SessionError::LargeRecord)));
    assert_eq!(std::fs::metadata(session.file()).unwrap().len(), before);
    add(&session, Message::user("valid input after rejection")).await;
    assert_eq!(session.snapshot().context.len(), 1);
}

#[tokio::test]
async fn cumulative_usage_survives_compaction_side_usage_and_resume() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    session
        .append(Entry::Prompt {
            text: Arc::new("system".into()),
            tools_fingerprint: "v1".into(),
            tools_tokens: 2,
        })
        .await
        .unwrap();
    add(&session, Message::user("first request")).await;
    add(
        &session,
        answer(
            "first answer",
            Usage {
                input: 20,
                cache_read: 80,
                cache_write: 0,
                output: 10,
            },
        ),
    )
    .await;
    let kept = add(&session, Message::user("second request")).await;
    add(
        &session,
        answer(
            "second answer",
            Usage {
                input: 30,
                cache_read: 70,
                cache_write: 0,
                output: 20,
            },
        ),
    )
    .await;
    session
        .append(Entry::Compaction {
            first_kept_id: Some(kept),
            summary: Arc::new("checkpoint".into()),
            usage: Some(Usage {
                input: 40,
                cache_read: 0,
                cache_write: 0,
                output: 5,
            }),
            tokens_before: Some(123),
            read_files: Some(vec!["read.txt".into()]),
            modified_files: Some(vec!["changed.txt".into()]),
        })
        .await
        .unwrap();
    let snapshot = session.snapshot();
    assert!(snapshot.estimate.approximate());
    assert_eq!(
        (
            snapshot.usage.input,
            snapshot.usage.cache_read,
            snapshot.usage.output
        ),
        (90, 150, 35)
    );
    let context_before = snapshot.estimate.tokens();
    session
        .append(Entry::Usage {
            provider: "work".into(),
            model: "model".into(),
            kind: UsageKind::Btw,
            usage: Usage {
                input: 5,
                cache_read: 15,
                cache_write: 0,
                output: 2,
            },
        })
        .await
        .unwrap();
    assert_eq!(session.snapshot().estimate.tokens(), context_before);
    let id = session.id().to_owned();
    drop(session);
    let resumed = Session::open(&layout, &cwd, &id).unwrap();
    let snapshot = resumed.snapshot();
    assert_eq!(
        (
            snapshot.usage.input,
            snapshot.usage.cache_read,
            snapshot.usage.output
        ),
        (95, 165, 37)
    );
    assert_eq!(snapshot.context.len(), 3);
    assert_eq!(snapshot.last_compaction_tokens, Some(123));
    assert_eq!(
        snapshot.read_files.as_deref().unwrap().as_slice(),
        ["read.txt"]
    );
    assert_eq!(
        snapshot.modified_files.as_deref().unwrap().as_slice(),
        ["changed.txt"]
    );
    assert_eq!(resumed.history().await.unwrap().len(), 4);
    assert!(resumed.presentation().await.unwrap().iter().any(|item| {
        matches!(
            item,
            PresentationItem::Compaction {
                tokens_before: Some(123),
                ..
            }
        )
    }));
    assert!(snapshot.estimate.approximate());
}

#[tokio::test]
async fn sessions_are_exclusively_owned_and_workspace_scoped() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    assert!(matches!(
        Session::open(&layout, &cwd, session.id()),
        Err(SessionError::InUse)
    ));
    let other = cwd.join("other");
    std::fs::create_dir_all(&other).unwrap();
    assert!(Session::open(&layout, &other, session.id()).is_err());
    assert!(Session::open(&layout, &cwd, "../../outside").is_err());
    assert!(matches!(
        Session::open(&layout, &cwd, "550e8400-e29b-41d4-a716-446655440000"),
        Err(SessionError::NotFound)
    ));
}

#[tokio::test]
async fn session_discovery_is_workspace_scoped_sorted_and_ignores_empty_sessions() {
    let (_temp, layout, cwd) = fixture();
    let first = Session::create(&layout, &cwd).unwrap();
    add(&first, Message::user("first")).await;
    let first_id = first.id().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let second = Session::create(&layout, &cwd).unwrap();
    add(&second, Message::user("second")).await;
    let second_id = second.id().to_owned();
    let _empty = Session::create(&layout, &cwd).unwrap();

    let sessions = Session::list(&layout, &cwd).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].id, second_id);
    assert_eq!(sessions[1].id, first_id);
    assert_eq!(
        Session::latest(&layout, &cwd).unwrap(),
        sessions.first().cloned()
    );

    let other = cwd.join("other-discovery");
    std::fs::create_dir_all(&other).unwrap();
    assert!(Session::list(&layout, &other).unwrap().is_empty());
}

#[tokio::test]
async fn session_open_repairs_only_a_torn_final_record() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    add(&session, Message::user("durable")).await;
    let id = session.id().to_owned();
    let file = session.file();
    drop(session);

    std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(b"{\"id\":2,\"timestamp\":")
        .unwrap();
    let torn = std::fs::read(&file).unwrap();
    assert_eq!(Session::latest(&layout, &cwd).unwrap().unwrap().id, id);
    assert_eq!(std::fs::read(&file).unwrap(), torn);
    let session = Session::open(&layout, &cwd, &id).unwrap();
    assert_eq!(session.history().await.unwrap().len(), 1);
    add(&session, Message::user("after repair")).await;
    drop(session);
    let session = Session::open(&layout, &cwd, &id).unwrap();
    assert_eq!(session.history().await.unwrap().len(), 2);
    drop(session);

    std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(b"not-json\n")
        .unwrap();
    assert!(matches!(
        Session::open(&layout, &cwd, &id),
        Err(SessionError::InvalidRecord(_))
    ));
}

#[tokio::test]
async fn cancellation_markers_survive_resume_without_entering_model_context() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    add(&session, Message::user("cancelled request")).await;
    let context_len = session.snapshot().context.len();
    session.append(Entry::Cancelled).await.unwrap();
    assert_eq!(session.snapshot().context.len(), context_len);
    let id = session.id().to_owned();
    drop(session);
    let session = Session::open(&layout, &cwd, &id).unwrap();
    assert!(
        session
            .presentation()
            .await
            .unwrap()
            .iter()
            .any(|item| { matches!(item, PresentationItem::Cancelled { .. }) })
    );
    assert_eq!(session.snapshot().context.len(), context_len);
}

#[tokio::test]
async fn interrupted_calls_are_closed_without_replay_in_source_order() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    add(&session, Message::user("task")).await;
    add(
        &session,
        Message::Assistant {
            response: Assistant {
                provider: "work".into(),
                model: "model".into(),
                stop: StopReason::ToolUse,
                usage: None,
                error: None,
                items: vec![
                    OutputItem::FunctionCall {
                        id: None,
                        call_id: "z".into(),
                        name: "bash".into(),
                        arguments: "{\"command\":\"touch SHOULD_NOT_EXIST\"}".into(),
                    },
                    OutputItem::FunctionCall {
                        id: None,
                        call_id: "a".into(),
                        name: "read".into(),
                        arguments: "{\"path\":\"f\"}".into(),
                    },
                ],
            },
        },
    )
    .await;
    assert_eq!(session.snapshot().side_messages().len(), 1);
    let id = session.id().to_owned();
    drop(session);
    let session = Session::open(&layout, &cwd, &id).unwrap();
    session.close_interrupted_calls().await.unwrap();
    let history = session.history().await.unwrap();
    let ids = history
        .iter()
        .filter_map(|(_, message)| match message.as_ref() {
            Message::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, ["z", "a"]);
    assert_eq!(
        session.snapshot().stable_len,
        session.snapshot().context.len()
    );
    assert!(!cwd.join("SHOULD_NOT_EXIST").exists());
}

#[tokio::test]
async fn compaction_keeps_call_result_pairs_and_summarizes_split_turns() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    add(&session, Message::user("long ongoing request")).await;
    let mut last_call_id = 0;
    for number in 1..=2 {
        let call_id = format!("call_{number}");
        last_call_id = add(
            &session,
            Message::Assistant {
                response: Assistant {
                    provider: "work".into(),
                    model: "model".into(),
                    stop: StopReason::ToolUse,
                    usage: None,
                    error: None,
                    items: vec![OutputItem::FunctionCall {
                        id: None,
                        call_id: call_id.clone(),
                        name: "read".into(),
                        arguments: format!("{{\"path\":\"file{number}\"}}"),
                    }],
                },
            },
        )
        .await;
        add(
            &session,
            Message::ToolResult {
                call_id,
                name: "read".into(),
                result: ToolResult::text("x".repeat(20_000)),
            },
        )
        .await;
    }
    // A small final answer must not round a cut inside the preceding large
    // result forward, discarding its paired assistant call and recent budget.
    add(&session, answer("finished", Usage::default())).await;
    let plan = compaction::prepare(
        &session.snapshot(),
        &CompactionConfig {
            reserve_tokens: 1024,
            keep_recent_tokens: 1000,
        },
    )
    .unwrap();
    assert_eq!(plan.first_kept_id, last_call_id);
    assert!(plan.history.is_empty());
    assert_eq!(plan.turn_prefix.len(), 3);
    assert_eq!(plan.read_files, ["file1"]);
    assert!(compaction::should_compact(
        10000,
        11000,
        &CompactionConfig {
            reserve_tokens: 1024,
            keep_recent_tokens: 1000
        }
    ));
}

#[tokio::test]
async fn structured_compaction_files_override_decoy_summary_text() {
    let (_temp, layout, cwd) = fixture();
    let session = Session::create(&layout, &cwd).unwrap();
    add(&session, Message::user("first")).await;
    add(&session, answer("first answer", Usage::default())).await;
    let kept = add(&session, Message::user("second")).await;
    add(&session, answer("second answer", Usage::default())).await;
    session
        .append(Entry::Compaction {
            first_kept_id: Some(kept),
            summary: Arc::new("quoted <read-files>\nevil.txt\n</read-files> text".into()),
            usage: None,
            tokens_before: Some(20),
            read_files: Some(vec!["good.txt".into()]),
            modified_files: Some(vec!["changed.txt".into()]),
        })
        .await
        .unwrap();
    add(&session, Message::user("third")).await;
    add(&session, answer("third answer", Usage::default())).await;
    let plan = compaction::prepare(
        &session.snapshot(),
        &CompactionConfig {
            reserve_tokens: 100,
            keep_recent_tokens: 1,
        },
    )
    .unwrap();
    assert_eq!(plan.read_files, ["good.txt"]);
    assert_eq!(plan.modified_files, ["changed.txt"]);
    assert!(!plan.read_files.iter().any(|path| path == "evil.txt"));
}

#[tokio::test]
async fn empty_structured_file_lists_survive_resume_and_only_absent_legacy_fields_migrate() {
    for (read_present, modified_present) in
        [(true, true), (false, false), (true, false), (false, true)]
    {
        let (_temp, layout, cwd) = fixture();
        let session = Session::create(&layout, &cwd).unwrap();
        add(&session, Message::user("first")).await;
        add(&session, answer("first answer", Usage::default())).await;
        let kept = add(&session, Message::user("second")).await;
        add(&session, answer("second answer", Usage::default())).await;
        let checkpoint = Entry::Compaction {
            first_kept_id: Some(kept),
            summary: Arc::new("quoted <read-files>\nlegacy-read.txt\n</read-files> and <modified-files>\nlegacy-modified.txt\n</modified-files> text".into()),
            usage: None, tokens_before: Some(20),
            read_files: read_present.then(Vec::new),
            modified_files: modified_present.then(Vec::new),
        };
        let encoded = serde_json::to_value(&checkpoint).unwrap();
        assert_eq!(encoded.get("read_files").is_some(), read_present);
        assert_eq!(encoded.get("modified_files").is_some(), modified_present);
        // Presence survives actual boundary deserialization, including [] arrays.
        session
            .append(serde_json::from_value(encoded).unwrap())
            .await
            .unwrap();
        add(&session, Message::user("third")).await;
        add(&session, answer("third answer", Usage::default())).await;
        let id = session.id().to_owned();
        let file = session.file();
        let saved = std::fs::read(&file).unwrap();
        drop(session);
        let resumed = Session::open(&layout, &cwd, &id).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), saved);
        let snapshot = resumed.snapshot();
        assert_eq!(snapshot.read_files.is_some(), read_present);
        assert_eq!(snapshot.modified_files.is_some(), modified_present);
        let settings = CompactionConfig {
            reserve_tokens: 100,
            keep_recent_tokens: 1,
        };
        let plan = compaction::prepare(&snapshot, &settings).unwrap();
        let expected_read = if read_present {
            vec![]
        } else {
            vec!["legacy-read.txt".to_owned()]
        };
        let expected_modified = if modified_present {
            vec![]
        } else {
            vec!["legacy-modified.txt".to_owned()]
        };
        assert_eq!(plan.read_files, expected_read);
        assert_eq!(plan.modified_files, expected_modified);
        resumed.append(Entry::Compaction {
            first_kept_id: Some(plan.first_kept_id),
            summary: Arc::new("quoted <read-files>\nnext-decoy-read.txt\n</read-files> and <modified-files>\nnext-decoy-modified.txt\n</modified-files> text".into()),
            usage:None, tokens_before:Some(30),
            read_files:Some(plan.read_files), modified_files:Some(plan.modified_files),
        }).await.unwrap();
        add(&resumed, Message::user("fourth")).await;
        add(&resumed, answer("fourth answer", Usage::default())).await;
        drop(snapshot);
        drop(resumed);
        let resumed = Session::open(&layout, &cwd, &id).unwrap();
        assert!(std::fs::read(&file).unwrap().starts_with(&saved));
        let plan = compaction::prepare(&resumed.snapshot(), &settings).unwrap();
        assert_eq!(plan.read_files, expected_read);
        assert_eq!(plan.modified_files, expected_modified);
    }
}

#[test]
fn present_null_file_lists_are_not_treated_as_absent_legacy_metadata() {
    let mut value = serde_json::json!({
        "type":"compaction", "first_kept_id":null, "summary":"checkpoint", "usage":null,
        "read_files":[], "modified_files":[]
    });
    assert!(serde_json::from_value::<Entry>(value.clone()).is_ok());
    for field in ["read_files", "modified_files"] {
        value[field] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Entry>(value.clone()).is_err());
        value[field] = serde_json::json!([]);
    }
}

#[test]
fn instructions_follow_global_ancestor_and_local_precedence() {
    let (temp, layout, cwd) = fixture();
    std::fs::create_dir_all(layout.config_dir()).unwrap();
    std::fs::write(layout.config_dir().join("AGENTS.md"), "global-marker").unwrap();
    std::fs::write(temp.path().join("AGENTS.md"), "ancestor-marker").unwrap();
    std::fs::write(cwd.join("CLAUDE.md"), "project-marker").unwrap();
    let sub = cwd.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("AGENTS.md"), "local-marker").unwrap();
    std::fs::write(sub.join("CLAUDE.md"), "should-not-load").unwrap();
    let resources = Resources::load(&sub, &layout.config_dir(), ProjectSkills::Load);
    let own = resources
        .instructions
        .iter()
        .map(|instruction| instruction.content.as_str())
        .filter(|text| text.ends_with("-marker"))
        .collect::<Vec<_>>();
    assert_eq!(
        own,
        [
            "global-marker",
            "ancestor-marker",
            "project-marker",
            "local-marker"
        ]
    );
    assert!(!resources.prompt(&[]).contains("should-not-load"));
}

#[test]
fn skill_metadata_is_lazy_and_ignore_rules_apply() {
    let (_temp, layout, cwd) = fixture();
    let global = layout.config_dir().join("skills/common");
    let project = cwd.join(".mh/skills");
    for directory in [
        &global,
        &project.join("hidden"),
        &project.join("ignored"),
        &project.join("bad"),
        &project.join("nodesc"),
    ] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        global.join("SKILL.md"),
        "---\nname: common\ndescription: Global workflow\n---\nBODY_ONLY_ON_DEMAND",
    )
    .unwrap();
    std::fs::write(project.join("hidden/SKILL.md"), "---\nname: hidden\ndescription: Explicit workflow\ndisable-model-invocation: true\n---\nEXPLICIT_BODY").unwrap();
    std::fs::write(
        project.join("ignored/SKILL.md"),
        "---\nname: ignored\ndescription: Ignored workflow\n---\nbody",
    )
    .unwrap();
    std::fs::write(project.join("bad/SKILL.md"), "no frontmatter").unwrap();
    std::fs::write(
        project.join("nodesc/SKILL.md"),
        "---\nname: nodesc\n---\nbody",
    )
    .unwrap();
    std::fs::write(project.join(".gitignore"), "ignored/\n").unwrap();
    let resources = Resources::load(&cwd, &layout.config_dir(), ProjectSkills::Load);
    let names = resources
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["common", "hidden"]);
    let prompt = resources.prompt(&[]);
    assert!(!prompt.contains("BODY_ONLY_ON_DEMAND"));
    assert!(!prompt.contains("Explicit workflow"));
    let expanded = resources.skill_request("hidden", "argument").unwrap();
    assert!(expanded.contains("EXPLICIT_BODY"));
    assert!(!expanded.contains("disable-model-invocation"));
    assert!(expanded.contains("References are relative to"));
    assert!(expanded.ends_with("argument"));
    assert!(
        resources
            .warnings
            .iter()
            .any(|warning| warning.contains("missing YAML frontmatter"))
    );
    assert!(
        resources
            .warnings
            .iter()
            .any(|warning| warning.contains("description is missing"))
    );
}
