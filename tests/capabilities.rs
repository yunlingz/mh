use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mh::features::todo::state::{State, Status};
use mh::features::{ask_user_question as ask, btw, todo};
use mh::message::{Assistant, Message, OutputItem, OutputText, StopReason, Usage};
use mh::session::Session;
use mh::storage::Layout;
use mh::theme::Palette;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

fn mutate(state: &State, args: Value) -> State {
    let out = todo::execute(state, args);
    assert!(!out.result.is_error, "{}", out.result.plain_text());
    out.state
}

fn params(count: usize, multi: bool) -> Arc<ask::Params> {
    let questions = (0..count).map(|index| json!({
        "question":format!("Question {index}?"), "header":format!("Q{index}"), "multiSelect":multi,
        "options":[{"label":"First","description":"First choice","preview":"# Preview\n```rust\nfn main() {}\n```"},
            {"label":"Second","description":"Second choice"}]
    })).collect::<Vec<_>>();
    Arc::new(
        ask::prepare(json!({"questions":questions}))
            .unwrap_or_else(|error| panic!("{}", error.plain_text())),
    )
}

fn press(dialog: &mut ask::view::Dialog, code: KeyCode) -> Option<ask::Answers> {
    dialog.key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn todo_batches_are_atomic_and_dependencies_are_validated() {
    let state = mutate(
        &State::default(),
        json!({"action":"create","subject":"Prepare"}),
    );
    let batch = mutate(
        &state,
        json!({"action":"create","tasks":[{"subject":"Build","blockedBy":[1]},{"subject":"Test","blockedBy":[1]}]}),
    );
    assert_eq!(
        batch.tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let invalid = todo::execute(
        &state,
        json!({"action":"create","tasks":[{"subject":"A"},{"subject":"B","blockedBy":[99]}]}),
    );
    assert!(invalid.result.is_error);
    assert_eq!(invalid.state.tasks, state.tasks);
    assert_eq!(invalid.state.next_id, 2);
    assert!(
        todo::execute(
            &state,
            json!({"action":"create","tasks":[{"subject":"A"},{"subject":"B","blockedBy":[2]}]})
        )
        .result
        .is_error
    );
    assert!(
        todo::execute(&batch, json!({"action":"update","id":1,"addBlockedBy":[3]}))
            .result
            .is_error
    );
    let updated = mutate(
        &batch,
        json!({"action":"update","id":3,"removeBlockedBy":[1],"addBlockedBy":[2]}),
    );
    assert_eq!(updated.tasks[2].blocked_by, [2]);
}

#[test]
fn todo_transitions_tombstones_metadata_and_idempotence_match_the_contract() {
    let state = mutate(
        &State::default(),
        json!({"action":"create","subject":"Build","owner":"local","metadata":{"keep":1,"remove":2}}),
    );
    let state = mutate(
        &state,
        json!({"action":"update","id":1,"status":"in_progress","activeForm":"Building"}),
    );
    let repeated = todo::execute(
        &state,
        json!({"action":"update","id":1,"status":"in_progress","activeForm":"Building"}),
    );
    assert!(!repeated.changed);
    assert!(repeated.result.plain_text().starts_with("No change:"));
    let state = mutate(
        &state,
        json!({"action":"update","id":1,"metadata":{"remove":null,"added":3}}),
    );
    assert_eq!(state.tasks[0].metadata.as_ref().unwrap().len(), 2);
    let details = todo::execute(&state, json!({"action":"get","id":1}))
        .result
        .plain_text();
    assert!(details.contains("activeForm: Building"));
    assert!(details.contains("owner: local"));
    let state = mutate(
        &state,
        json!({"action":"update","id":1,"status":"completed"}),
    );
    assert!(
        todo::execute(&state, json!({"action":"update","id":1,"status":"pending"}))
            .result
            .is_error
    );
    let state = mutate(&state, json!({"action":"delete","id":1}));
    assert_eq!(state.tasks[0].status, Status::Deleted);
    assert_eq!(
        todo::execute(&state, json!({"action":"list"}))
            .result
            .plain_text(),
        "No tasks"
    );
    assert!(
        todo::execute(&state, json!({"action":"list","includeDeleted":true}))
            .result
            .plain_text()
            .contains("[deleted]")
    );
    assert!(
        todo::execute(&state, json!({"action":"get","id":1}))
            .result
            .plain_text()
            .contains("[deleted]")
    );
    assert_eq!(mutate(&state, json!({"action":"clear"})).next_id, 1);
}

#[test]
fn todo_normalizes_string_encoded_batches_and_integral_json_numbers() {
    let state = mutate(
        &State::default(),
        json!({"action":"create","tasks":"[\"{\\\"subject\\\":\\\"Build\\\"}\"]"}),
    );
    let updated = mutate(
        &state,
        json!({"action":"update","id":1.0,"status":"in_progress"}),
    );
    assert_eq!(updated.tasks[0].status, Status::InProgress);
    let state = mutate(
        &updated,
        json!({"action":"create","tasks":[{"subject":"Test","blockedBy":[1.0]}]}),
    );
    assert_eq!(state.tasks[1].blocked_by, [1]);
}

#[tokio::test]
async fn todo_snapshots_resume_without_a_model_summary() {
    let temp = tempfile::tempdir().unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    let session = Session::create(&layout, temp.path()).unwrap();
    let state = mutate(
        &State::default(),
        json!({"action":"create","subject":"Build"}),
    );
    let state = mutate(
        &state,
        json!({"action":"create","subject":"Test","blockedBy":[1]}),
    );
    session
        .append(mh::session::Entry::Todo {
            state: Arc::new(serde_json::to_value(&state).unwrap()),
        })
        .await
        .unwrap();
    let id = session.id().to_owned();
    drop(session);
    let session = Session::open(&layout, temp.path(), &id).unwrap();
    let restored = State::from_saved(session.snapshot().todo.as_ref().unwrap()).unwrap();
    assert_eq!(restored.tasks, state.tasks);
    assert_eq!(restored.next_id, 3);
}

#[test]
fn todo_panel_bounds_height_preserves_pending_and_hides_completed_next_run() {
    let state = mutate(
        &State::default(),
        json!({"action":"create","tasks":(0..20).map(|index| json!({"subject":format!("Task {index}")})).collect::<Vec<_>>()}),
    );
    let state = mutate(
        &state,
        json!({"action":"update","id":1,"status":"completed"}),
    );
    let state = mutate(
        &state,
        json!({"action":"update","id":2,"status":"completed"}),
    );
    let mut panel = todo::view::Panel::default();
    let lines = panel.render(&state, &Palette::default(), 5);
    assert_eq!(lines.len(), 5);
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Todos (2/20)"));
    assert!(text.contains("Task 2"));
    assert!(text.contains("+17 more (2 completed, 15 pending)"));
    assert!(text.contains("├─ ○ "));
    assert!(text.contains("└─ +"));
    panel.next_run();
    assert!(
        panel.render(&state, &Palette::default(), 5)[0]
            .to_string()
            .contains("(0/18)")
    );
    panel.collapsed = true;
    assert_eq!(panel.render(&state, &Palette::default(), 5).len(), 2);
}

#[test]
fn questions_reject_reserved_duplicate_and_out_of_range_data() {
    for label in ["Other", "Type something.", "Next"] {
        let raw = json!({"questions":[{"question":"Choose?","header":"Choice","options":[{"label":label,"description":"One"},{"label":"Allowed","description":"Two"}]}]});
        assert!(ask::prepare(raw).is_err());
    }
    assert!(ask::prepare(json!({"questions":[]})).is_err());
    let raw = json!({"questions":[{"question":"Choose?","header":"Choice","options":[{"label":"Same","description":"One"},{"label":"Same","description":"Two"}]}]});
    assert!(ask::prepare(raw).is_err());
    let raw = json!({"questions":[{"question":"Choose?","header":"12345678901234567","options":[{"label":"A","description":"One"},{"label":"B","description":"Two"}]}]});
    assert!(ask::prepare(raw).is_err());
}

#[test]
fn question_non_editor_newline_keys_never_confirm_and_custom_intent_survives_deselection() {
    let mut dialog = ask::view::Dialog::new(params(2, true));
    assert!(
        dialog
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .is_none()
    );
    assert!(dialog.questionnaire.drafts[0].intent.is_none());
    dialog.questionnaire.toggle(0, 0);
    dialog.questionnaire.drafts[0].custom = "kept custom answer".into();
    assert!(dialog.questionnaire.commit_custom(0));
    dialog.questionnaire.toggle(0, 0);
    let answers = dialog.questionnaire.finish(false);
    assert_eq!(
        answers.answers[0].answer.as_deref(),
        Some("kept custom answer")
    );
}

#[test]
fn question_custom_input_uses_line_deletion_and_never_submits_newlines() {
    let mut dialog = ask::view::Dialog::new(params(1, false));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    dialog.paste("kept");
    assert!(
        dialog
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .is_none()
    );
    dialog.paste("delete");
    dialog.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(dialog.questionnaire.drafts[0].custom, "kept\n");
    assert!(
        dialog
            .key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .is_none()
    );
    dialog.paste("tail");
    press(&mut dialog, KeyCode::Home);
    dialog.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(dialog.questionnaire.drafts[0].custom, "kept\n\n");
    dialog.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    for key in [
        KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT),
    ] {
        dialog.key(key);
    }
    assert_eq!(dialog.questionnaire.drafts[0].custom, "kept\n\n");
}

#[test]
fn question_keyboard_preserves_custom_drafts_notes_and_partial_submission() {
    let mut dialog = ask::view::Dialog::new(params(2, false));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    for char in "custom".chars() {
        press(&mut dialog, KeyCode::Char(char));
    }
    press(&mut dialog, KeyCode::Tab);
    press(&mut dialog, KeyCode::Enter);
    assert_eq!(dialog.questionnaire.drafts[0].custom, "custom");
    assert!(dialog.questionnaire.drafts[0].intent.is_none());
    press(&mut dialog, KeyCode::Char('n'));
    for char in "global note".chars() {
        press(&mut dialog, KeyCode::Char(char));
    }
    press(&mut dialog, KeyCode::Enter);
    let answer = press(&mut dialog, KeyCode::Enter).unwrap();
    assert_eq!(answer.answers.len(), 1);
    assert_eq!(answer.unanswered, [0]);
    assert_eq!(answer.global_note.as_deref(), Some("global note"));
    assert!(!answer.cancelled);
    assert_eq!(answer.answers[0].answer.as_deref(), Some("First"));
}

#[test]
fn question_multiselect_combines_options_custom_text_and_notes() {
    let mut dialog = ask::view::Dialog::new(params(1, true));
    press(&mut dialog, KeyCode::Char(' '));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Char(' '));
    press(&mut dialog, KeyCode::Char('n'));
    for char in "a note".chars() {
        press(&mut dialog, KeyCode::Char(char));
    }
    press(&mut dialog, KeyCode::Enter);
    press(&mut dialog, KeyCode::Down);
    for char in "third".chars() {
        press(&mut dialog, KeyCode::Char(char));
    }
    let result = press(&mut dialog, KeyCode::Enter).unwrap();
    assert_eq!(
        result.answers[0].selected.as_ref().unwrap(),
        &["First", "Second"]
    );
    assert_eq!(result.answers[0].answer.as_deref(), Some("third"));
    assert_eq!(result.answers[0].notes.as_deref(), Some("a note"));
    assert!(result.answers[0].preview.is_none());
}

#[test]
fn multiselect_toggles_persist_without_next_and_deselecting_clears_the_answer() {
    let params = params(2, true);
    let mut questionnaire = ask::Questionnaire::new(params);
    questionnaire.toggle(0, 0);
    let result = questionnaire.finish(false);
    assert_eq!(result.answers.len(), 1);
    assert_eq!(
        result.answers[0].selected.as_deref(),
        Some(["First".to_owned()].as_slice())
    );
    assert_eq!(result.unanswered_questions, ["Question 1?"]);
    assert!(
        result
            .tool_result()
            .plain_text()
            .contains("\"Question 1?\"")
    );

    questionnaire.toggle(0, 0);
    let result = questionnaire.finish(false);
    assert!(result.answers.is_empty());
    assert_eq!(result.unanswered, [0, 1]);

    assert!(questionnaire.commit_custom(0));
    assert!(
        questionnaire
            .finish(false)
            .tool_result()
            .plain_text()
            .contains("(no input)")
    );
}

#[test]
fn questionnaire_normalization_removes_lone_carriage_returns() {
    let params = ask::prepare(json!({"questions":[{
        "question":"GEMBA\r_LOG?","header":"Header",
        "options":[
            {"label":"A\rB","description":"One\rTwo"},
            {"label":"C","description":"Three"}
        ]
    }]}))
    .unwrap();
    assert_eq!(params.questions[0].question, "GEMBA_LOG?");
    assert_eq!(params.questions[0].options[0].label, "AB");
    assert_eq!(params.questions[0].options[0].description, "OneTwo");
}

#[test]
fn question_cancellation_and_missing_ui_are_distinct() {
    let mut dialog = ask::view::Dialog::new(params(1, false));
    let result = press(&mut dialog, KeyCode::Esc).unwrap().tool_result();
    assert!(!result.is_error);
    assert_eq!(result.plain_text(), "User declined to answer questions");
    let unavailable = ask::failure("ui_unavailable", "Interactive UI is unavailable");
    assert!(unavailable.is_error);
    assert!(
        unavailable
            .plain_text()
            .contains("do not treat this as a decline")
    );
}

#[test]
fn questionnaire_paste_is_literal_in_the_focused_editor() {
    let mut dialog = ask::view::Dialog::new(params(1, false));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    dialog.paste("first line\n/compact is text");
    assert_eq!(
        dialog.questionnaire.drafts[0].custom,
        "first line\n/compact is text"
    );
    let answer = press(&mut dialog, KeyCode::Enter).unwrap();
    assert_eq!(
        answer.answers[0].answer.as_deref(),
        Some("first line\n/compact is text")
    );
}

#[test]
fn questionnaire_tabs_restore_rows_and_show_retained_custom_drafts() {
    let mut dialog = ask::view::Dialog::new(params(2, false));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    for character in "retained draft".chars() {
        press(&mut dialog, KeyCode::Char(character));
    }
    press(&mut dialog, KeyCode::Tab);
    press(&mut dialog, KeyCode::BackTab);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Type something.: retained draft"));
}

#[test]
fn question_editor_hardware_cursor_follows_unicode_cells() {
    let mut dialog = ask::view::Dialog::new(params(1, false));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    dialog.paste("你好");
    let mut terminal = Terminal::new(TestBackend::new(80, 28)).unwrap();
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
        .unwrap();
    let position = terminal.get_cursor_position().unwrap();
    // Outer border, the two-cell prompt, then two double-width CJK characters.
    assert_eq!(position.x, 7);
}

#[test]
fn long_question_previews_can_be_paged_without_changing_the_selection() {
    let preview = format!(
        "```text\n{}\n```",
        (0..100)
            .map(|row| format!("preview line {row:03}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let params = Arc::new(
        ask::prepare(json!({"questions":[{
            "question":"Choose?", "header":"Choice", "options":[
                {"label":"Alpha","description":"First","preview":preview},
                {"label":"Beta","description":"Second"}
            ]
        }]}))
        .unwrap(),
    );
    let mut dialog = ask::view::Dialog::new(params);
    let mut terminal = Terminal::new(TestBackend::new(120, 28)).unwrap();
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("preview line 000"));
    press(&mut dialog, KeyCode::PageDown);
    press(&mut dialog, KeyCode::PageDown);
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!text.contains("preview line 000"));
    assert!(text.contains("preview line 016"));
    for _ in 0..100 {
        press(&mut dialog, KeyCode::PageDown);
    }
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
        .unwrap();
    let bottom = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    press(&mut dialog, KeyCode::PageUp);
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
        .unwrap();
    let back = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_ne!(
        bottom, back,
        "scroll overshoot must not trap the preview at the bottom"
    );
    let answer = press(&mut dialog, KeyCode::Enter).unwrap();
    assert_eq!(answer.answers[0].answer.as_deref(), Some("Alpha"));
}

#[test]
fn question_review_keeps_submit_and_cancel_visible_with_long_notes() {
    let mut dialog = ask::view::Dialog::new(params(2, false));
    press(&mut dialog, KeyCode::Char('n'));
    dialog.paste(&"long note ".repeat(100));
    press(&mut dialog, KeyCode::Enter);
    press(&mut dialog, KeyCode::Tab);
    press(&mut dialog, KeyCode::Tab);
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    for _ in 0..2 {
        terminal
            .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Submit answers"));
        assert!(text.contains("Cancel"));
        press(&mut dialog, KeyCode::PageDown);
    }
}

#[test]
fn questionnaire_is_opaque_accepts_real_ctrl_m_and_does_not_trap_notes_tabs() {
    use ratatui::widgets::{Paragraph, Wrap};
    let palette = Palette::default();
    let mut dialog = ask::view::Dialog::new(params(2, false));
    let mut terminal = Terminal::new(TestBackend::new(80, 28)).unwrap();
    let area = ratatui::layout::Rect::new(3, 3, 70, 22);
    terminal
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new("Z".repeat(5000)).wrap(Wrap { trim: false }),
                frame.area(),
            );
            dialog.render(frame, area, &palette);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            assert_ne!(buffer[(x, y)].symbol(), "Z");
        }
    }
    press(&mut dialog, KeyCode::Char('n'));
    dialog.paste("saved notes");
    press(&mut dialog, KeyCode::Tab);
    assert_eq!(dialog.questionnaire.drafts[0].notes, "saved notes");
    press(&mut dialog, KeyCode::Enter);
    let answer = press(&mut dialog, KeyCode::Enter).unwrap();
    assert_eq!(answer.answers.len(), 1);
    let mut dialog = ask::view::Dialog::new(params(1, false));
    let answer = dialog
        .key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL))
        .unwrap();
    assert!(!answer.cancelled);
    assert_eq!(answer.answers[0].answer.as_deref(), Some("First"));
}

#[test]
fn long_custom_answers_wrap_without_changing_submitted_text() {
    let mut dialog = ask::view::Dialog::new(params(1, false));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    let text = format!("BEGIN {}END", "word ".repeat(40));
    dialog.paste(&text);
    let mut terminal = Terminal::new(TestBackend::new(48, 28)).unwrap();
    terminal
        .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
        .unwrap();
    let view = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(view.contains("BEGIN") && view.contains("END"));
    let answer = dialog
        .key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(answer.answers[0].answer.as_deref(), Some(text.as_str()));
}

#[test]
fn questionnaire_notes_and_custom_input_ignore_removed_undo_keys_and_keep_literal_pastes() {
    let mut dialog = ask::view::Dialog::new(params(2, false));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    let pasted = "kept 中文\n/compact is literal\n[Pasted text #1]\n".repeat(4);
    dialog.paste(&pasted);
    assert_eq!(dialog.questionnaire.drafts[0].custom, pasted);
    dialog.key(KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL));
    assert_eq!(dialog.questionnaire.drafts[0].custom, pasted);
    dialog.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(dialog.questionnaire.drafts[0].custom, pasted);
    press(&mut dialog, KeyCode::Home);
    dialog.key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT));
    dialog.paste("replacement 你好");
    assert_eq!(dialog.questionnaire.drafts[0].custom, "replacement 你好");
    dialog.key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::CONTROL));
    assert_eq!(dialog.questionnaire.drafts[0].custom, "replacement 你好");
    dialog.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    press(&mut dialog, KeyCode::Tab);
    press(&mut dialog, KeyCode::Char('n'));
    dialog.paste("saved note");
    dialog.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT));
    assert_eq!(dialog.questionnaire.drafts[1].notes, "saved note");
    dialog.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(dialog.questionnaire.drafts[1].notes, "saved note");
    press(&mut dialog, KeyCode::Enter);
    press(&mut dialog, KeyCode::BackTab);
    press(&mut dialog, KeyCode::Down);
    let answer = press(&mut dialog, KeyCode::Enter);
    assert!(answer.is_none());
    assert_eq!(dialog.questionnaire.drafts[0].custom, "replacement 你好");
    let answer = press(&mut dialog, KeyCode::Esc).unwrap();
    assert!(
        answer.cancelled,
        "the modal still consumes cancellation before main work"
    );
}

#[test]
fn questionnaire_field_labels_rules_opacity_and_tiny_layouts_are_consistent() {
    use ratatui::widgets::{Paragraph, Wrap};
    let palette = Palette::default();
    for (width, height) in [(1, 1), (2, 2), (8, 6), (40, 12), (80, 28)] {
        for focus in ["options", "custom", "notes", "collapsed"] {
            let mut dialog = ask::view::Dialog::new(params(1, false));
            match focus {
                "custom" => {
                    press(&mut dialog, KeyCode::Down);
                    press(&mut dialog, KeyCode::Down);
                    dialog.paste("你好");
                }
                "notes" => {
                    press(&mut dialog, KeyCode::Char('n'));
                    dialog.paste("你好");
                }
                "collapsed" => {
                    dialog.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
                }
                _ => {}
            }
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("▒".repeat(5000)).wrap(Wrap { trim: false }),
                        frame.area(),
                    );
                    dialog.render(frame, frame.area(), &palette);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for y in 0..height {
                for x in 0..width {
                    let cell = &buffer[(x, y)];
                    assert_ne!(
                        cell.symbol(),
                        "▒",
                        "underlying transcript leaked at {width}x{height}/{focus}"
                    );
                    // Ratatui resets a wide glyph's continuation cell; the
                    // preceding opaque glyph owns both terminal cells.
                    let continuation = x > 0
                        && unicode_width::UnicodeWidthStr::width(buffer[(x - 1, y)].symbol()) == 2;
                    assert!(
                        cell.bg == palette.background
                            || cell.bg == palette.selected_bg
                            || continuation,
                        "unexpected background {:?} for {:?} at {width}x{height}/{focus}",
                        cell.bg,
                        cell.symbol()
                    );
                    assert!(!["╭", "╮", "╰", "╯"].contains(&cell.symbol()));
                }
            }
            if width == 80 && ["notes", "custom"].contains(&focus) {
                let label = if focus == "notes" {
                    "Notes"
                } else {
                    "Type something."
                };
                let row = |y| {
                    (1..width - 1)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                };
                let label_y = (1..height - 1).find(|y| row(*y).contains(label)).unwrap();
                for x in 1..width - 1 {
                    assert_eq!(buffer[(x, label_y + 1)].symbol(), " ");
                    assert_eq!(buffer[(x, label_y + 2)].symbol(), "─");
                }
                assert_eq!(buffer[(1, label_y + 3)].symbol(), ">");
                assert_eq!(terminal.get_cursor_position().unwrap().x, 7);
            }
        }
    }
}

#[test]
fn question_rendering_uses_native_borders_and_changes_preview_layout() {
    for width in [1, 40, 80, 120] {
        let mut terminal = Terminal::new(TestBackend::new(width, 28)).unwrap();
        let mut dialog = ask::view::Dialog::new(params(1, false));
        terminal
            .draw(|frame| dialog.render(frame, frame.area(), &Palette::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        if width >= 40 {
            assert!(text.contains("Preview"));
            assert!(text.contains("Type something."));
        }
        assert_eq!(buffer[(0, 0)].symbol(), "┌");
        assert!(!text.chars().any(|c| ['╭', '╮', '╰', '╯'].contains(&c)));
    }
}

fn side_answer(text: &str) -> Assistant {
    Assistant {
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
        usage: Some(Usage {
            input: 1,
            cache_read: 2,
            cache_write: 0,
            output: 3,
        }),
        error: None,
    }
}

#[test]
fn btw_thread_is_bounded_and_never_produces_tool_results() {
    let mut thread = btw::Thread::default();
    for index in 0..25 {
        thread.push(
            format!("Question {index}"),
            side_answer(&format!("Answer {index}")),
        );
    }
    assert_eq!(thread.len(), 20);
    assert_eq!(thread.latest().unwrap().question, "Question 24");
    thread.clear();
    assert!(thread.is_empty());
    assert!(thread.latest().is_none());
}

#[tokio::test]
async fn btw_uses_stable_context_without_mutating_main_history() {
    let temp = tempfile::tempdir().unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    let session = Session::create(&layout, temp.path()).unwrap();
    session
        .append(mh::session::Entry::Message {
            message: Arc::new(Message::user("Main question")),
        })
        .await
        .unwrap();
    let mut thread = btw::Thread::default();
    thread.push("Side one".into(), side_answer("Side answer"));
    let messages = thread.messages(&session.snapshot(), "Side two");
    assert_eq!(messages.len(), 4);
    assert_eq!(session.snapshot().context.len(), 1);
    assert_eq!(session.history().await.unwrap().len(), 1);
    assert_eq!(session.snapshot().usage.input, 0);
}
