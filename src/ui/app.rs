//! Fullscreen Ratatui application and input coordinator.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ui::completion::{self, Menu};
use crate::ui::editor::Editor;
use crate::ui::picker::{Action as PickerAction, Picker};
use crossterm::event::{
    Event as TerminalEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::tools::{QuestionRequest, ToolEvent};
use crate::agent::{Event, Phase, RetryScope, RunStatus};
use crate::features::ask_user_question::{Answers, view::Dialog};
use crate::features::btw::{Thread, view::Overlay};
use crate::features::todo::{self, state::State as TodoState, view::Panel as TodoPanel};
use crate::message::{Assistant, Message, ToolCall, ToolResult};
use crate::runtime::{Harness, Options, SessionSelection};
use crate::session::{PresentationItem, Snapshot};
use crate::text::terminal_text;
use crate::ui::{footer, markdown, startup, terminal, transcript};

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = terminal::enter_screen(&mut stdout) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = terminal::leave_screen(&mut stdout);
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::leave_screen(self.terminal.backend_mut());
        let _ = disable_raw_mode();
    }
}

enum MainCompletion {
    Run(RunStatus),
    Compacted,
}

struct Backoff {
    attempt: u8,
    max_retries: u8,
    until: Instant,
}

struct Activity {
    started: Instant,
    cancel: CancellationToken,
    task: JoinHandle<Result<MainCompletion, String>>,
}

struct SideActivity {
    started: Instant,
    question: String,
    cancel: CancellationToken,
    task: JoinHandle<Result<Assistant, String>>,
    retain: bool,
}

struct ToolView {
    call: ToolCall,
    output: String,
    result: Option<Arc<ToolResult>>,
}

struct ClipboardActivity {
    cancel: CancellationToken,
    task: JoinHandle<Result<crate::clipboard::Item, String>>,
    retain: bool,
}

struct CopyActivity {
    cancel: CancellationToken,
    task: JoinHandle<Result<(), String>>,
}

struct SkillCompletion {
    path: std::path::PathBuf,
    resources: crate::context::Resources,
}

#[derive(Clone, Copy)]
enum PickerKind {
    Session,
    Model,
}
struct ActivePicker {
    picker: Picker,
    kind: PickerKind,
}
pub struct ExitInfo {
    pub session_id: String,
    pub has_conversation: bool,
}

struct ActiveQuestion {
    dialog: Dialog,
    reply: Option<oneshot::Sender<Answers>>,
}

struct App {
    harness: Harness,
    event_tx: mpsc::Sender<Event>,
    tool_tx: mpsc::Sender<ToolEvent>,
    events: mpsc::Receiver<Event>,
    tool_events: mpsc::Receiver<ToolEvent>,
    editor: Editor,
    input_queue: crate::agent::queue::Queue,
    completion: Option<Menu>,
    picker: Option<ActivePicker>,
    help: Option<crate::ui::help::Help>,
    message_lines: Vec<usize>,
    review: bool,
    scroll_down_requested: bool,
    transcript_area: Rect,
    editor_area: Rect,
    jump_area: Rect,
    files: Vec<String>,
    file_task: Option<JoinHandle<Vec<String>>>,
    clipboard: Option<ClipboardActivity>,
    copy: Option<CopyActivity>,
    skills: Option<crate::ui::skills::Wizard>,
    skill_task: Option<JoinHandle<Result<SkillCompletion, String>>>,
    selection: crate::ui::selection::Selection,
    live_lines: Vec<Line<'static>>,
    live_viewport: crate::ui::viewport::Viewport,
    presentation: Vec<PresentationItem>,
    presentation_revision: u64,
    history_lines: Vec<Line<'static>>,
    viewport: crate::ui::viewport::Viewport,
    snapshots: watch::Receiver<Arc<Snapshot>>,
    expanded: bool,
    animation_frame: u64,
    phase: Phase,
    live_text: String,
    live_thinking: String,
    live_tool_progress: bool,
    tools: BTreeMap<String, ToolView>,
    tool_order: Vec<String>,
    main: Option<Activity>,
    main_backoff: Option<Backoff>,
    side: Option<SideActivity>,
    side_backoff: Option<Backoff>,
    side_thread: Thread,
    side_overlay: Overlay,
    question: Option<ActiveQuestion>,
    queued_questions: VecDeque<ActiveQuestion>,
    todo_panel: TodoPanel,
    todos_visible: bool,
    todos_scroll: u16,
    status: String,
    scroll: usize,
    transcript_width: u16,
    follow: bool,
    dirty: bool,
    quit: bool,
    last_ctrl_c: Option<Instant>,
}

pub async fn run(
    harness: Harness,
    event_tx: mpsc::Sender<Event>,
    tool_tx: mpsc::Sender<ToolEvent>,
    events: mpsc::Receiver<Event>,
    tool_events: mpsc::Receiver<ToolEvent>,
    initial_prompt: Option<String>,
) -> Result<ExitInfo, String> {
    let mut app = App::new(harness, event_tx, tool_tx, events, tool_events).await?;
    let mut terminal = TerminalGuard::enter().map_err(|_| "Cannot enter fullscreen mode")?;
    terminal
        .terminal
        .clear()
        .map_err(|_| "Cannot clear the terminal")?;
    if let Some(prompt) = initial_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        app.start_main(prompt);
    }
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut result = Ok(());
    let mut title = terminal::Title::default();
    let shutdown_signal = crate::signals::cancellation();
    tokio::pin!(shutdown_signal);
    while !app.quit {
        tokio::select! {
            _ = &mut shutdown_signal => app.quit = true,
            _ = tick.tick() => {
                app.finish_tasks().await;
                app.refresh_presentation().await;
                let animation = crate::storage::timestamp() / 1000;
                if (app.main.is_some() || app.side.is_some()) && animation != app.animation_frame {
                    app.animation_frame = animation;
                    app.dirty = true;
                }
                if app.dirty {
                    let _ = title.update(terminal.terminal.backend_mut(), app.title_activity());
                    if terminal.terminal.draw(|frame| app.render(frame)).is_err() {
                        result = Err("Cannot draw the terminal".into());
                        app.quit = true;
                    }
                    app.dirty = false;
                }
            }
            Some(event) = app.events.recv() => app.agent_event(event),
            Some(event) = app.tool_events.recv() => app.tool_event(event),
            event = input.next() => {
                match event {
                    Some(Ok(event)) => {
                        if let Err(error) = app.terminal_event(event).await {
                            result = Err(error);
                            app.quit = true;
                        }
                    }
                    Some(Err(_)) => {
                        result = Err("Cannot read terminal input".into());
                        app.quit = true;
                    }
                    None => app.quit = true,
                }
            }
        }
    }
    app.shutdown().await;
    let history = app
        .harness
        .agent
        .session
        .history()
        .await
        .map_err(|error| error.to_string())?;
    let exit = ExitInfo {
        session_id: app.harness.agent.session.id().into(),
        has_conversation: history
            .iter()
            .any(|(_, message)| matches!(message.as_ref(), Message::User { .. })),
    };
    drop(terminal);
    result.map(|()| exit)
}

pub async fn choose_session(prepared: &crate::runtime::Prepared) -> Result<Option<String>, String> {
    let layout = prepared.layout.clone();
    let cwd = prepared.cwd.clone();
    let sessions =
        tokio::task::spawn_blocking(move || crate::session::Session::list(&layout, &cwd))
            .await
            .map_err(|_| "Session discovery worker failed")?
            .map_err(|error| error.to_string())?;
    let mut picker = Picker::new("Resume session", crate::ui::picker::sessions(sessions), "");
    let mut terminal = TerminalGuard::enter().map_err(|_| "Cannot enter session picker")?;
    let mut input = EventStream::new();
    loop {
        terminal
            .terminal
            .draw(|frame| picker.render(frame, frame.area(), &prepared.palette))
            .map_err(|_| "Cannot draw session picker")?;
        match input.next().await {
            Some(Ok(TerminalEvent::Key(mut key))) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('m') && key.modifiers == KeyModifiers::CONTROL {
                    key.code = KeyCode::Enter;
                    key.modifiers = KeyModifiers::NONE;
                }
                match picker.key(key) {
                    Some(PickerAction::Selected(id)) => return Ok(Some(id)),
                    Some(PickerAction::Cancelled) => return Ok(None),
                    None => {}
                }
            }
            Some(Ok(TerminalEvent::Paste(text))) => picker.paste(&text),
            Some(Ok(TerminalEvent::Resize(_, _))) => {}
            Some(Ok(_)) => {}
            _ => return Err("Cannot read session picker input".into()),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(activity) = &self.main {
            activity.cancel.cancel();
        }
        if let Some(activity) = &self.side {
            activity.cancel.cancel();
        }
        if let Some(activity) = &self.clipboard {
            activity.cancel.cancel();
        }
        if let Some(activity) = &self.copy {
            activity.cancel.cancel();
        }
    }
}

impl App {
    fn title_activity(&self) -> &'static str {
        if self.skill_task.is_some() {
            return "Creating skill";
        }
        if self.main.is_some() {
            if self.main_backoff.is_some() {
                return "Retrying";
            }
            return match self.phase {
                Phase::Compacting => "Compacting",
                Phase::Tools => "Running commands",
                Phase::Streaming if !self.live_thinking.is_empty() => "Thinking",
                _ => "Working",
            };
        }
        if self.side.is_some() {
            return "Side question";
        }
        "Ready"
    }

    fn selection_document(&self) -> crate::ui::selection::Document<'_> {
        crate::ui::selection::Document {
            saved: &self.history_lines,
            live: &self.live_lines,
            saved_rows: &self.viewport,
            live_rows: &self.live_viewport,
            width: self.transcript_width,
        }
    }

    fn copy_selection(&mut self) {
        if self.copy.is_some() {
            return;
        }
        match self.selection.copy(&self.selection_document()) {
            Ok(text) => {
                let cancel = CancellationToken::new();
                let request_cancel = cancel.clone();
                let task =
                    tokio::spawn(
                        async move { crate::clipboard::write(&text, &request_cancel).await },
                    );
                self.copy = Some(CopyActivity { cancel, task });
                self.status = "Copying selection...".into();
            }
            Err(error) => self.status = error.into(),
        }
        self.dirty = true;
    }

    fn select_mouse(&mut self, row: usize, column: usize, begin: bool) {
        let mut selection = std::mem::take(&mut self.selection);
        let document = self.selection_document();
        if begin {
            selection.begin(&document, row, column);
        } else {
            selection.drag(&document, row, column);
        }
        self.selection = selection;
        self.follow = false;
        self.scroll_down_requested = false;
    }

    fn select_key(&mut self, movement: crate::ui::selection::Movement) {
        let mut selection = std::mem::take(&mut self.selection);
        let target = selection.extend(&self.selection_document(), movement, self.scroll);
        self.selection = selection;
        if let Some(row) = target {
            let height = usize::from(self.transcript_area.height.max(1));
            if row < self.scroll {
                self.scroll = row;
            } else if row >= self.scroll.saturating_add(height) {
                self.scroll = row + 1 - height;
            }
        }
        self.follow = false;
        self.dirty = true;
    }

    async fn new(
        harness: Harness,
        event_tx: mpsc::Sender<Event>,
        tool_tx: mpsc::Sender<ToolEvent>,
        events: mpsc::Receiver<Event>,
        tool_events: mpsc::Receiver<ToolEvent>,
    ) -> Result<Self, String> {
        let presentation = harness
            .agent
            .session
            .presentation()
            .await
            .map_err(|error| error.to_string())?;
        let (history_lines, message_lines) = render_history(&presentation, &harness, false);
        let snapshots = harness.agent.session.subscribe();
        let presentation_revision = snapshots.borrow().presentation_revision;
        let mut editor = Editor::default();
        for item in &presentation {
            if let PresentationItem::Message { message, .. } = item
                && let Message::User { content } = message.as_ref()
                && let Ok(restored) = Editor::from_content(content)
            {
                editor.remember(restored.draft());
            }
        }
        let cwd = harness.cwd.clone();
        let file_task = Some(tokio::task::spawn_blocking(move || {
            completion::file_index(&cwd)
        }));
        Ok(Self {
            harness,
            event_tx,
            tool_tx,
            events,
            tool_events,
            editor,
            input_queue: crate::agent::queue::Queue::default(),
            completion: None,
            picker: None,
            help: None,
            message_lines,
            review: false,
            scroll_down_requested: false,
            transcript_area: Rect::default(),
            editor_area: Rect::default(),
            jump_area: Rect::default(),
            files: Vec::new(),
            file_task,
            clipboard: None,
            copy: None,
            skills: None,
            skill_task: None,
            selection: crate::ui::selection::Selection::default(),
            live_lines: Vec::new(),
            live_viewport: crate::ui::viewport::Viewport::default(),
            presentation,
            presentation_revision,
            history_lines,
            viewport: crate::ui::viewport::Viewport::default(),
            snapshots,
            expanded: false,
            animation_frame: 0,
            phase: Phase::Idle,
            live_text: String::new(),
            live_thinking: String::new(),
            live_tool_progress: false,
            tools: BTreeMap::new(),
            tool_order: Vec::new(),
            main: None,
            main_backoff: None,
            side: None,
            side_backoff: None,
            side_thread: Thread::default(),
            side_overlay: Overlay::default(),
            question: None,
            queued_questions: VecDeque::new(),
            todo_panel: TodoPanel::default(),
            todos_visible: false,
            todos_scroll: 0,
            status: "Ready".into(),
            scroll: 0,
            transcript_width: 0,
            follow: true,
            dirty: true,
            quit: false,
            last_ctrl_c: None,
        })
    }

    fn start_main(&mut self, prompt: String) {
        if self.main.is_some() {
            self.status = "Main work is active; cancel it or wait before submitting".into();
            self.dirty = true;
            return;
        }
        self.start_input(vec![crate::message::Content::text(prompt)]);
    }

    fn start_input(&mut self, content: Vec<crate::message::Content>) {
        self.todo_panel.next_run();
        let agent = self.harness.agent.clone();
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let queue = self.input_queue.clone();
        let task = tokio::spawn(async move {
            agent
                .run_input_with_queue(Some(content), &queue, &request_cancel)
                .await
                .map(|outcome| MainCompletion::Run(outcome.status))
        });
        self.main = Some(Activity {
            started: Instant::now(),
            cancel,
            task,
        });
        self.status = "Working...".into();
        self.follow = true;
        self.dirty = true;
    }

    fn start_queued(&mut self) {
        self.todo_panel.next_run();
        let agent = self.harness.agent.clone();
        let queue = self.input_queue.clone();
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            agent
                .run_input_with_queue(None, &queue, &request_cancel)
                .await
                .map(|result| MainCompletion::Run(result.status))
        });
        self.main = Some(Activity {
            started: Instant::now(),
            cancel,
            task,
        });
        self.status = "Working on queued input...".into();
        self.dirty = true;
    }

    fn restore_queued(&mut self) {
        let current = self.editor.draft();
        let result = self.input_queue.edit(|inputs| {
            if inputs.is_empty() {
                return Err("No queued input to edit".into());
            }
            let mut content = Vec::new();
            for input in inputs {
                if !content.is_empty() {
                    content.push(crate::message::Content::text("\n\n"));
                }
                content.extend(input.content.clone());
            }
            if !current.is_empty() {
                content.push(crate::message::Content::text("\n\n"));
                content.extend(current.content());
            }
            Editor::from_content(&content).map(|editor| (editor.draft(), inputs.len()))
        });
        match result {
            Ok((draft, count)) => {
                self.editor.restore(draft);
                self.status = format!("Restored {count} queued messages to edit");
            }
            Err(error) => self.status = error,
        }
        self.completion = None;
        self.dirty = true;
    }

    fn start_compaction(&mut self, focus: String) {
        if self.main.is_some() {
            self.status = "Main work is active; compact when it is idle".into();
            self.dirty = true;
            return;
        }
        let agent = self.harness.agent.clone();
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            agent
                .compact(&focus, &request_cancel)
                .await
                .map(|()| MainCompletion::Compacted)
        });
        self.main = Some(Activity {
            started: Instant::now(),
            cancel,
            task,
        });
        self.status = "Compacting context...".into();
        self.dirty = true;
    }

    fn start_side(&mut self, question: String) {
        if self.side.is_some() {
            self.status = "A side question is already running".into();
            self.dirty = true;
            return;
        }
        let agent = self.harness.agent.clone();
        let thread = self.side_thread.clone();
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let task_question = question.clone();
        let task =
            tokio::spawn(async move { agent.side(&task_question, &thread, &request_cancel).await });
        self.side_overlay.begin(question.clone());
        self.side = Some(SideActivity {
            started: Instant::now(),
            question,
            cancel,
            task,
            retain: true,
        });
        self.dirty = true;
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.agent_event(event);
        }
        while let Ok(event) = self.tool_events.try_recv() {
            self.tool_event(event);
        }
    }

    async fn finish_tasks(&mut self) {
        if self
            .copy
            .as_ref()
            .is_some_and(|activity| activity.task.is_finished())
        {
            let activity = self.copy.take().expect("finished copy");
            self.status = match activity.task.await {
                Ok(Ok(())) => "Selection copied · Esc clears selection".into(),
                Ok(Err(error)) => error,
                Err(_) => "Clipboard writer failed".into(),
            };
            self.dirty = true;
        }
        if self
            .skill_task
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let task = self.skill_task.take().expect("finished skill creation");
            let result = match task.await {
                Ok(Ok(created)) => self
                    .harness
                    .agent
                    .refresh_resources(created.resources)
                    .await
                    .map(|()| created.path),
                Ok(Err(error)) => Err(error),
                Err(_) => Err("Skill creation worker failed".into()),
            };
            match result {
                Ok(path) => {
                    self.skills = None;
                    self.selection.clear();
                    self.reload_presentation().await;
                    self.status = format!("Skill created: {}", crate::text::display_path(&path));
                    self.update_completion(false);
                }
                Err(error) => {
                    if let Some(wizard) = &mut self.skills {
                        wizard.failed(error.clone());
                    }
                    self.status = error;
                }
            }
            self.dirty = true;
        }
        if self
            .file_task
            .as_ref()
            .is_some_and(|task| task.is_finished())
            && let Some(task) = self.file_task.take()
        {
            self.files = task.await.unwrap_or_default();
            self.update_completion(false);
        }
        if self
            .clipboard
            .as_ref()
            .is_some_and(|activity| activity.task.is_finished())
        {
            let activity = self.clipboard.take().expect("finished clipboard activity");
            let result = match activity.task.await {
                result if !activity.retain => {
                    drop(result);
                    Ok(())
                }
                Ok(Ok(crate::clipboard::Item::Image(content))) => self.editor.image(content),
                Ok(Ok(crate::clipboard::Item::Text(text))) => self.editor.paste(&text),
                Ok(Ok(crate::clipboard::Item::Empty)) => Err("Clipboard is empty".into()),
                Ok(Err(error)) => Err(error),
                Err(_) => Err("Clipboard worker failed".into()),
            };
            if let Err(error) = result {
                self.status = error;
            }
            self.update_completion(false);
            self.dirty = true;
        }
        if self
            .main
            .as_ref()
            .is_some_and(|activity| activity.task.is_finished())
        {
            // A finished producer has sent all its events, but the receivers
            // may still contain them. Apply those before installing final state.
            self.drain_events();
            let activity = self.main.take().expect("checked main activity");
            self.main_backoff = None;
            let completion = activity.task.await;
            let continue_queue = matches!(
                &completion,
                Ok(Ok(
                    MainCompletion::Run(RunStatus::Completed) | MainCompletion::Compacted
                ))
            );
            match completion {
                Ok(Ok(MainCompletion::Run(RunStatus::Completed))) => self.status = "Ready".into(),
                Ok(Ok(MainCompletion::Run(RunStatus::Truncated))) => {
                    self.status = "Response was truncated".into()
                }
                Ok(Ok(MainCompletion::Run(RunStatus::Failed))) => {
                    self.status = "Provider request failed".into()
                }
                Ok(Ok(MainCompletion::Run(RunStatus::Cancelled))) => {
                    self.status = "Main work cancelled".into()
                }
                Ok(Ok(MainCompletion::Compacted)) => self.status = "Context compacted".into(),
                Ok(Err(error)) => self.status = error,
                Err(_) => self.status = "Main task failed".into(),
            }
            self.phase = Phase::Idle;
            self.live_text.clear();
            self.live_thinking.clear();
            self.tools.clear();
            self.tool_order.clear();
            self.question = None;
            self.queued_questions.clear();
            self.reload_presentation().await;
            if continue_queue && !self.quit && !self.input_queue.snapshot().is_empty() {
                self.start_queued();
            }
            self.dirty = true;
        }
        if self
            .side
            .as_ref()
            .is_some_and(|activity| activity.task.is_finished())
        {
            self.drain_events();
            let activity = self.side.take().expect("checked side activity");
            self.side_backoff = None;
            match activity.task.await {
                Ok(Ok(answer)) => {
                    self.side_overlay.answer = answer.text();
                    self.side_overlay.waiting = false;
                    if activity.retain && matches!(answer.stop, crate::message::StopReason::Stop) {
                        self.side_thread.push(activity.question, answer);
                    } else if self.side_overlay.answer.is_empty() {
                        self.side_overlay.answer = answer
                            .error
                            .clone()
                            .unwrap_or_else(|| "Side request did not complete".into());
                    }
                }
                Ok(Err(error)) => {
                    self.side_overlay.answer = error;
                    self.side_overlay.waiting = false;
                }
                Err(_) => {
                    self.side_overlay.answer = "Side request failed".into();
                    self.side_overlay.waiting = false;
                }
            }
            self.dirty = true;
        }
    }

    async fn refresh_presentation(&mut self) {
        if self.snapshots.has_changed().unwrap_or(false) {
            // Borrow the immutable update before reading the presentation so a
            // later journal append is still observed on the next frame.
            let revision = self.snapshots.borrow_and_update().presentation_revision;
            if revision != self.presentation_revision {
                self.reload_presentation().await;
            } else {
                self.dirty = true; // Usage/footer changed, finalized history did not.
            }
        }
    }

    async fn reload_presentation(&mut self) {
        let revision = self.harness.agent.session.snapshot().presentation_revision;
        match self.harness.agent.session.presentation().await {
            Ok(presentation) => {
                let (lines, anchors) = render_history(&presentation, &self.harness, self.expanded);
                if lines != self.history_lines {
                    // Stable finalized selections survive appends. Dynamic text
                    // may be regrouped at finalization; never reuse stale offsets.
                    self.selection.retain_saved(self.history_lines.len());
                }
                (self.history_lines, self.message_lines) = (lines, anchors);
                self.viewport.invalidate();
                self.presentation = presentation;
                self.presentation_revision = revision;
            }
            Err(error) => self.status = error.to_string(),
        }
        self.dirty = true;
    }

    async fn shutdown(&mut self) {
        if let Some(activity) = self.copy.take() {
            activity.cancel.cancel();
            let _ = activity.task.await;
        }
        if let Some(task) = self.skill_task.take() {
            let _ = task.await;
        }
        if let Some(activity) = self.clipboard.take() {
            activity.cancel.cancel();
            let _ = activity.task.await;
        }
        if let Some(task) = self.file_task.take() {
            let _ = task.await;
        }
        if let Some(activity) = &self.main {
            activity.cancel.cancel();
        }
        if let Some(activity) = &self.side {
            activity.cancel.cancel();
        }
        if let Some(activity) = self.main.take() {
            let _ = activity.task.await;
        }
        if let Some(activity) = self.side.take() {
            let _ = activity.task.await;
        }
    }

    fn agent_event(&mut self, event: Event) {
        match event {
            Event::RetryScheduled {
                scope,
                attempt,
                max_retries,
                delay,
            } => {
                let backoff = Some(Backoff {
                    attempt,
                    max_retries,
                    until: Instant::now() + delay,
                });
                if scope == RetryScope::Side {
                    self.side_backoff = backoff;
                } else {
                    self.main_backoff = backoff;
                }
            }
            Event::RetryStarted { scope } => {
                if scope == RetryScope::Side {
                    self.side_backoff = None;
                } else {
                    self.main_backoff = None;
                }
            }
            Event::QueuedInputConsumed { .. } => {}
            Event::Phase(phase) => {
                if phase == Phase::Streaming {
                    self.main_backoff = None;
                    self.live_text.clear();
                    self.live_thinking.clear();
                    self.live_tool_progress = false;
                }
                self.phase = phase;
            }
            Event::Text(text) => self.live_text.push_str(&text),
            Event::Thinking(text) => self.live_thinking.push_str(&text),
            Event::Assistant(_) => {
                // The authoritative response is already in the journal. It is
                // rendered by the presentation snapshot, never a second copy.
                self.live_text.clear();
                self.live_thinking.clear();
            }
            Event::StreamToolCallStarted {
                output_index,
                call_id,
                name,
            } => {
                self.live_tool_progress = true;
                self.status = format!("Preparing tool call {output_index}: {name} ({call_id})");
            }
            Event::StreamToolCallDelta { output_index, .. } => {
                self.status = format!("Streaming tool call {output_index}...");
            }
            Event::StreamToolCallFinished { output_index } => {
                self.status = format!("Tool call {output_index} is ready");
            }
            Event::Notice(notice) => self.status = notice,
            Event::SideText(text) => self.side_overlay.answer.push_str(&text),
            Event::SideRetry { attempt } => {
                self.side_overlay.answer.clear();
                self.side_overlay.waiting = true;
                if self.main.is_none() {
                    self.status = format!("Retrying side question ({attempt}/3)");
                }
            }
        }
        self.dirty = true;
    }

    fn tool_event(&mut self, event: ToolEvent) {
        match event {
            ToolEvent::Started(call) => {
                if !self.tools.contains_key(&call.id) {
                    self.tool_order.push(call.id.clone());
                }
                self.tools.insert(
                    call.id.clone(),
                    ToolView {
                        call,
                        output: String::new(),
                        result: None,
                    },
                );
            }
            ToolEvent::Update(update) => {
                if let Some(tool) = self.tools.get_mut(&update.call_id) {
                    tool.output = update.text;
                }
            }
            ToolEvent::Question(QuestionRequest { params, reply, .. }) => {
                let question = ActiveQuestion {
                    dialog: Dialog::new(params),
                    reply: Some(reply),
                };
                if self.question.is_some() {
                    self.queued_questions.push_back(question);
                } else {
                    self.question = Some(question);
                }
            }
            ToolEvent::Finished { call_id, result } => {
                if let Some(tool) = self.tools.get_mut(&call_id) {
                    tool.output = result.plain_text();
                    tool.result = Some(result);
                }
            }
        }
        self.dirty = true;
    }

    async fn terminal_event(&mut self, event: TerminalEvent) -> Result<(), String> {
        match event {
            TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => self.key(key).await?,
            TerminalEvent::Paste(text) => {
                if let Some(wizard) = &mut self.skills {
                    wizard.paste(&text);
                } else if let Some(question) = &mut self.question {
                    question.dialog.paste(&text);
                } else if let Some(picker) = &mut self.picker {
                    picker.picker.paste(&text);
                } else if self.help.is_some() {
                    // Local help owns input; pasted text must not alter the hidden draft.
                } else if !self.todos_visible && !self.side_overlay.visible {
                    if let Err(error) = self.editor.paste(&text) {
                        self.status = error;
                    }
                    self.completion = None;
                }
                self.dirty = true;
            }
            TerminalEvent::Mouse(mouse) => {
                if self.skills.is_some() {
                    return Ok(());
                }
                use crossterm::event::{MouseButton, MouseEventKind};
                let delta = match mouse.kind {
                    MouseEventKind::ScrollUp => -3,
                    MouseEventKind::ScrollDown => 3,
                    _ => 0,
                };
                if delta != 0 {
                    if let Some(question) = &mut self.question {
                        question.dialog.scroll(delta);
                    } else if let Some(help) = &mut self.help {
                        help.wheel(delta);
                    } else if let Some(picker) = &mut self.picker {
                        picker.picker.navigate(delta);
                    } else if self.todos_visible {
                        self.todos_scroll = if delta < 0 {
                            self.todos_scroll.saturating_sub(3)
                        } else {
                            self.todos_scroll.saturating_add(3)
                        };
                    } else if self.side_overlay.visible {
                        self.side_overlay.scroll = if delta < 0 {
                            self.side_overlay.scroll.saturating_sub(3)
                        } else {
                            self.side_overlay.scroll.saturating_add(3)
                        };
                    } else if delta < 0 {
                        self.scroll_up(3);
                    } else {
                        self.scroll_down(3);
                    }
                } else if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && self.question.is_none()
                    && self.picker.is_none()
                    && self.help.is_none()
                    && !self.todos_visible
                    && !self.side_overlay.visible
                {
                    let position = ratatui::layout::Position::new(mouse.column, mouse.row);
                    if self.jump_area.contains(position) {
                        self.follow = true;
                    } else if self.editor_area.contains(position) {
                        self.selection.clear();
                        self.editor.click(
                            mouse.row.saturating_sub(self.editor_area.y + 1),
                            mouse.column.saturating_sub(self.editor_area.x),
                        );
                    } else if self.transcript_area.contains(position) {
                        self.select_mouse(
                            self.scroll + usize::from(mouse.row - self.transcript_area.y),
                            usize::from(mouse.column - self.transcript_area.x),
                            true,
                        );
                    }
                } else if mouse.kind == MouseEventKind::Drag(MouseButton::Left)
                    && self.selection.dragging()
                    && self.question.is_none()
                    && self.picker.is_none()
                    && self.help.is_none()
                    && !self.todos_visible
                    && !self.side_overlay.visible
                {
                    if mouse.row < self.transcript_area.y {
                        self.scroll_up(1);
                    } else if mouse.row >= self.transcript_area.bottom() {
                        self.scroll_down(1);
                    }
                    let row = mouse
                        .row
                        .saturating_sub(self.transcript_area.y)
                        .min(self.transcript_area.height.saturating_sub(1));
                    self.select_mouse(
                        self.scroll + usize::from(row),
                        usize::from(mouse.column.saturating_sub(self.transcript_area.x)),
                        false,
                    );
                } else if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                    self.selection.finish_drag();
                }
                self.dirty = true;
            }
            TerminalEvent::Resize(_, _) => self.dirty = true,
            _ => {}
        }
        Ok(())
    }

    async fn key(&mut self, mut key: KeyEvent) -> Result<(), String> {
        if key.code == KeyCode::Char('m') && key.modifiers == KeyModifiers::CONTROL {
            key.code = KeyCode::Enter;
            key.modifiers = KeyModifiers::NONE;
        }
        if key.kind == KeyEventKind::Repeat
            && (matches!(key.code, KeyCode::Enter | KeyCode::Esc)
                || self.side_overlay.visible && key.code == KeyCode::Char('x')
                || key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'm' | 'v' | 'o' | 't')))
        {
            return Ok(());
        }
        if let Some(wizard) = &mut self.skills {
            match wizard.key(key) {
                crate::ui::skills::Action::Cancel => self.skills = None,
                crate::ui::skills::Action::Create { scope, draft } => {
                    let cwd = self.harness.cwd.clone();
                    let global = self.harness.layout.config_dir();
                    self.skill_task = Some(tokio::task::spawn_blocking(move || {
                        let store = crate::context::SkillStore::new(&cwd, &global)
                            .map_err(|error| error.to_string())?;
                        let path = store
                            .create(scope, &draft)
                            .map_err(|error| error.to_string())?;
                        let resources = crate::context::Resources::load(
                            &cwd,
                            &global,
                            crate::context::ProjectSkills::Load,
                        );
                        Ok(SkillCompletion { path, resources })
                    }));
                }
                crate::ui::skills::Action::None => {}
            }
            self.dirty = true;
            return Ok(());
        }
        if let Some(question) = &mut self.question {
            if let Some(answer) = question.dialog.key(key) {
                if let Some(reply) = question.reply.take() {
                    let _ = reply.send(answer);
                }
                self.question = self.queued_questions.pop_front();
            }
            self.dirty = true;
            return Ok(());
        }
        if let Some(help) = &mut self.help {
            if help.key(key) {
                self.help = None;
            }
            self.dirty = true;
            return Ok(());
        }
        if let Some(mut active) = self.picker.take() {
            match active.picker.key(key) {
                Some(PickerAction::Cancelled) => {}
                Some(PickerAction::Selected(value)) => {
                    let result = match active.kind {
                        PickerKind::Session => self
                            .switch_session(SessionSelection::Exact(value))
                            .await
                            .map(|()| true),
                        PickerKind::Model => self.command(&format!("model {value}")).await,
                    };
                    if let Err(error) = result {
                        active.picker.error(error.clone());
                        self.status = error;
                        self.picker = Some(active);
                    }
                }
                None => self.picker = Some(active),
            }
            self.dirty = true;
            return Ok(());
        }
        if self.todos_visible {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.todos_visible = false,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.todos_visible = false
                }
                KeyCode::Up => self.todos_scroll = self.todos_scroll.saturating_sub(1),
                KeyCode::Down => self.todos_scroll = self.todos_scroll.saturating_add(1),
                KeyCode::PageUp => self.todos_scroll = self.todos_scroll.saturating_sub(8),
                KeyCode::PageDown => self.todos_scroll = self.todos_scroll.saturating_add(8),
                KeyCode::Home => self.todos_scroll = 0,
                _ => {}
            }
            self.dirty = true;
            return Ok(());
        }
        if self.side_overlay.visible {
            match key.code {
                KeyCode::Esc => self.side_overlay.visible = false,
                KeyCode::Char('x') if key.modifiers.is_empty() => {
                    self.side_thread.clear();
                    if let Some(side) = &mut self.side {
                        side.retain = false;
                        side.cancel.cancel();
                    }
                    self.side_overlay.visible = false;
                    self.status = "Side history cleared".into();
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(side) = &self.side {
                        side.cancel.cancel();
                        self.status = "Cancelling side request...".into();
                    }
                }
                KeyCode::Up => {
                    self.side_overlay.scroll = self.side_overlay.scroll.saturating_sub(1)
                }
                KeyCode::Down => {
                    self.side_overlay.scroll = self.side_overlay.scroll.saturating_add(1)
                }
                KeyCode::PageUp => {
                    self.side_overlay.scroll = self.side_overlay.scroll.saturating_sub(8)
                }
                KeyCode::PageDown => {
                    self.side_overlay.scroll = self.side_overlay.scroll.saturating_add(8)
                }
                _ => {}
            }
            self.dirty = true;
            return Ok(());
        }
        if key.code == KeyCode::Char('c')
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::CONTROL) && self.selection.selected())
        {
            self.copy_selection();
            return Ok(());
        }
        if key.modifiers.contains(KeyModifiers::SHIFT) && (self.review || self.selection.focused())
        {
            use crate::ui::selection::Movement;
            let movement = match key.code {
                KeyCode::Left => Some(Movement::Left),
                KeyCode::Right => Some(Movement::Right),
                KeyCode::Up => Some(Movement::Up),
                KeyCode::Down => Some(Movement::Down),
                KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(Movement::Start)
                }
                KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(Movement::Finish)
                }
                KeyCode::Home => Some(Movement::Home),
                KeyCode::End => Some(Movement::End),
                _ => None,
            };
            if let Some(movement) = movement {
                self.select_key(movement);
                return Ok(());
            }
        }
        if self.completion.is_none()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            self.jump_message(key.code == KeyCode::Up);
            self.dirty = true;
            return Ok(());
        }
        if self.review && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            if key.code == KeyCode::Up {
                self.scroll_up(1);
            } else {
                self.scroll_down(1);
            }
            self.dirty = true;
            return Ok(());
        }
        if self.review
            && (key.code == KeyCode::Esc
                || key.code == KeyCode::Char('q')
                || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.review = false;
            self.expanded = false;
            self.selection.clear();
            (self.history_lines, self.message_lines) =
                render_history(&self.presentation, &self.harness, false);
            self.viewport.invalidate();
            self.dirty = true;
            return Ok(());
        }
        if self.review
            && !matches!(
                key.code,
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
            )
            && !(key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Ok(());
        }
        if key.modifiers.contains(KeyModifiers::ALT)
            && (key.code == KeyCode::Up || key.code == KeyCode::Char('q'))
        {
            self.restore_queued();
            return Ok(());
        }
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT) {
            self.submit_kind(crate::agent::queue::Kind::FollowUp)
                .await?;
            self.dirty = true;
            return Ok(());
        }
        if let Some(menu) = &mut self.completion {
            match key.code {
                KeyCode::Esc => {
                    self.completion = None;
                    self.dirty = true;
                    return Ok(());
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    menu.navigate(match key.code {
                        KeyCode::Up => -1,
                        KeyCode::Down => 1,
                        KeyCode::PageUp => -5,
                        _ => 5,
                    });
                    self.dirty = true;
                    return Ok(());
                }
                KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                    if let Some(item) = menu.selected() {
                        let value = item.value.clone();
                        let count = menu.prefix.chars().count();
                        let exact_command_argument = self.editor.before_cursor().starts_with('/')
                            && menu.prefix.trim_end() == value.trim_end();
                        let submit =
                            key.code == KeyCode::Enter && (menu.command || exact_command_argument);
                        self.editor.complete(count, &value)?;
                        self.completion = None;
                        if submit {
                            self.submit().await?;
                        }
                        self.dirty = true;
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                let clipboard_active = self
                    .clipboard
                    .as_ref()
                    .is_some_and(|activity| activity.retain);
                if let Some(activity) = &mut self.clipboard {
                    activity.cancel.cancel();
                    activity.retain = false;
                }
                if self.editor.lines().iter().any(|line| !line.is_empty()) {
                    self.editor.clear();
                    self.completion = None;
                    self.status = "Input cleared".into();
                    self.last_ctrl_c = Some(Instant::now());
                } else if clipboard_active {
                    self.status = "Clipboard read cancelled".into();
                    self.last_ctrl_c = None;
                } else if self
                    .last_ctrl_c
                    .is_some_and(|previous| previous.elapsed() < Duration::from_secs(1))
                {
                    self.quit = true;
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.status = "Press Ctrl+C again to quit".into();
                }
            }
            (KeyCode::Char('v'), modifiers)
                if modifiers.contains(KeyModifiers::CONTROL)
                    || modifiers.contains(KeyModifiers::ALT) =>
            {
                if self.clipboard.is_none() {
                    let dir = self.harness.agent.session.outputs();
                    let cancel = CancellationToken::new();
                    let request_cancel = cancel.clone();
                    let task =
                        tokio::spawn(
                            async move { crate::clipboard::read(&dir, &request_cancel).await },
                        );
                    self.clipboard = Some(ClipboardActivity {
                        cancel,
                        task,
                        retain: true,
                    });
                    self.status = "Reading clipboard...".into();
                }
            }
            (KeyCode::Char('p'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_model(modifiers.contains(KeyModifiers::SHIFT))
                    .await?
            }
            (KeyCode::Tab, _) => self.update_completion(true),
            _ if crate::ui::editor::is_newline(key) => {
                self.editor.insert_newline();
            }
            (KeyCode::Char('o'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.completion = None;
                self.selection.clear();
                self.expanded = !self.expanded;
                self.review = self.expanded;
                (self.history_lines, self.message_lines) =
                    render_history(&self.presentation, &self.harness, self.expanded);
                self.viewport.invalidate();
            }
            (KeyCode::Char('t'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.todo_panel.collapsed = !self.todo_panel.collapsed;
            }
            (KeyCode::Enter, modifiers)
                if modifiers.is_empty() || modifiers == KeyModifiers::CONTROL =>
            {
                self.submit().await?
            }
            (KeyCode::Char('m'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit().await?
            }
            (KeyCode::Esc, _) => {
                if let Some(main) = &self.main {
                    main.cancel.cancel();
                    if !self.input_queue.snapshot().is_empty() {
                        self.restore_queued();
                    } else {
                        self.status = "Cancelling main work...".into();
                    }
                } else {
                    self.selection.clear();
                }
            }
            (KeyCode::PageUp, _) => self.scroll_up(usize::from(
                self.transcript_area.height.saturating_sub(2).max(1),
            )),
            (KeyCode::PageDown, _) => self.scroll_down(usize::from(
                self.transcript_area.height.saturating_sub(2).max(1),
            )),
            (KeyCode::Home, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.follow = false;
                self.scroll = 0;
            }
            (KeyCode::End, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.follow = true;
            }
            _ => {
                self.selection.clear();
                self.editor.input(key);
                self.update_completion(false);
            }
        }
        self.dirty = true;
        Ok(())
    }

    fn scroll_up(&mut self, rows: usize) {
        self.follow = false;
        self.scroll_down_requested = false;
        self.scroll = self.scroll.saturating_sub(rows);
    }
    fn scroll_down(&mut self, rows: usize) {
        self.scroll_down_requested = true;
        self.scroll = self.scroll.saturating_add(rows);
    }
    fn jump_message(&mut self, previous: bool) {
        self.viewport
            .prepare(&self.history_lines, self.transcript_area.width);
        let rows = self
            .message_lines
            .iter()
            .map(|line| self.viewport.row_of_line(*line))
            .collect::<Vec<_>>();
        let target = if previous {
            rows.iter().rev().find(|row| **row < self.scroll).copied()
        } else {
            rows.iter().find(|row| **row > self.scroll).copied()
        };
        if let Some(row) = target {
            self.follow = false;
            self.scroll = row;
        } else if previous {
            self.follow = false;
            self.scroll = 0;
        } else {
            self.follow = true;
        }
    }
    async fn cycle_model(&mut self, previous: bool) -> Result<(), String> {
        let models = completion::models(&self.harness.config);
        let choice = self.harness.agent.model_choice();
        if let Some(index) = models
            .iter()
            .position(|item| item.value == choice.qualified)
        {
            let next = (index + if previous { models.len() - 1 } else { 1 }) % models.len();
            if let Err(error) = self.command(&format!("model {}", models[next].value)).await {
                self.status = error;
            }
        }
        Ok(())
    }

    fn update_completion(&mut self, force: bool) {
        if force && self.file_task.is_none() {
            let cwd = self.harness.cwd.clone();
            self.file_task = Some(tokio::task::spawn_blocking(move || {
                completion::file_index(&cwd)
            }));
        }
        if self.review || self.picker.is_some() || self.question.is_some() || self.skills.is_some()
        {
            self.completion = None;
            return;
        }
        let qualified = self.harness.agent.model_choice().qualified;
        self.completion = completion::menu(
            &self.editor.before_cursor(),
            &self.harness.config,
            &self.harness.agent.resources(),
            &qualified,
            &self.files,
            force,
        );
    }

    async fn submit(&mut self) -> Result<(), String> {
        self.submit_kind(crate::agent::queue::Kind::Steer).await
    }

    async fn submit_kind(&mut self, kind: crate::agent::queue::Kind) -> Result<(), String> {
        let draft = self.editor.draft();
        if draft.is_empty() {
            return Ok(());
        }
        let text = draft.text();
        if draft.has_images() && text.starts_with('/') {
            self.status = "Send image attachments as a main prompt, not a slash command".into();
            return Ok(());
        }
        let content = draft.content();
        if let Err(error) = self.harness.agent.validate_input(&content) {
            self.status = error;
            return Ok(());
        }
        let accepted = if let Some(command) = text.strip_prefix('/') {
            match self.command_with_kind(command, kind).await {
                Ok(accepted) => accepted,
                Err(error) => {
                    self.status = error;
                    self.dirty = true;
                    false
                }
            }
        } else {
            if self.main.is_some() || !self.input_queue.snapshot().is_empty() {
                match self.input_queue.push(kind, content, draft.summary()) {
                    Ok(_) => {
                        self.status = "Queued · Alt+Q to edit pending messages".into();
                        if self.main.is_none() {
                            self.start_queued();
                        }
                    }
                    Err(error) => {
                        self.status = error;
                        return Ok(());
                    }
                }
            } else {
                self.start_input(content);
            }
            true
        };
        if accepted {
            self.editor.remember(draft);
            self.editor.reset();
            self.completion = None;
        }
        Ok(())
    }

    async fn command(&mut self, command: &str) -> Result<bool, String> {
        self.command_with_kind(command, crate::agent::queue::Kind::Steer)
            .await
    }

    async fn command_with_kind(
        &mut self,
        command: &str,
        kind: crate::agent::queue::Kind,
    ) -> Result<bool, String> {
        let (name, args) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, args)| (name, args.trim()));
        match name {
            "quit" | "exit" => self.quit = true,
            "help" => {
                self.selection.clear();
                self.completion = None;
                self.help = Some(crate::ui::help::Help::default());
            }
            "skills" if args == "create" => {
                if self.main.is_some() || !self.input_queue.snapshot().is_empty() {
                    return Err("Finish or cancel main/queued work before creating a skill".into());
                }
                if let Some(activity) = self.clipboard.take() {
                    activity.cancel.cancel();
                    let _ = activity.task.await;
                }
                let store = crate::context::SkillStore::new(
                    &self.harness.cwd,
                    &self.harness.layout.config_dir(),
                )
                .map_err(|error| error.to_string())?;
                self.selection.clear();
                self.skills = Some(crate::ui::skills::Wizard::new(
                    store.root(crate::context::SkillScope::Project),
                    store.root(crate::context::SkillScope::Global),
                ));
            }
            "skills" => {
                self.status = "Use /skills create to create a project or global skill".into();
                return Ok(false);
            }
            "todos" => {
                self.todos_scroll = 0;
                self.todos_visible = true;
            }
            "compact" => {
                if self.main.is_some() {
                    return Err("Finish or cancel active work before manual compaction".into());
                }
                self.start_compaction(args.to_owned());
            }
            "btw" if args.is_empty() => {
                if self.side.as_ref().is_some_and(|side| side.retain) {
                    self.side_overlay.visible = true;
                } else if let Some(exchange) = self.side_thread.latest() {
                    self.side_overlay.visible = true;
                    self.side_overlay.question = exchange.question.clone();
                    self.side_overlay.answer = exchange.answer.text();
                    self.side_overlay.waiting = false;
                } else {
                    self.status = "No side-question history".into();
                }
            }
            "btw" => self.start_side(args.to_owned()),
            "model" if args.is_empty() || self.harness.config.model(args).is_err() => {
                self.picker = Some(ActivePicker {
                    picker: Picker::new(
                        "Select model",
                        completion::models(&self.harness.config),
                        args,
                    ),
                    kind: PickerKind::Model,
                });
            }
            "model" => {
                if self.side.is_some() {
                    return Err("Wait for the side request before changing models".into());
                }
                let current = self.harness.agent.model_choice();
                self.harness.agent.select_model(args).await?;
                self.status = format!(
                    "{}: {args}",
                    if self.main.is_some() {
                        "Next request model"
                    } else {
                        "Model"
                    }
                );
                if current.qualified != args {
                    self.dirty = true;
                }
            }
            "new" => self.switch_session(SessionSelection::New).await?,
            "resume" if args.is_empty() => {
                let layout = self.harness.layout.clone();
                let cwd = self.harness.cwd.clone();
                let current = self.harness.agent.session.id().to_owned();
                let sessions = tokio::task::spawn_blocking(move || {
                    crate::session::Session::list(&layout, &cwd)
                })
                .await
                .map_err(|_| "Session discovery worker failed")?
                .map_err(|error| error.to_string())?;
                let items = crate::ui::picker::sessions(
                    sessions.into_iter().filter(|s| s.id != current).collect(),
                );
                self.picker = Some(ActivePicker {
                    picker: Picker::new("Resume session", items, ""),
                    kind: PickerKind::Session,
                });
            }
            "resume" => {
                self.switch_session(SessionSelection::Exact(args.into()))
                    .await?
            }
            name if name.starts_with("skill:") => {
                let skill = &name[6..];
                let prompt = self.harness.agent.resources().skill_request(skill, args)?;
                if self.main.is_some() {
                    self.input_queue.push(
                        kind,
                        vec![crate::message::Content::text(prompt)],
                        format!("/skill:{skill} {args}"),
                    )?;
                } else {
                    self.start_main(prompt);
                }
            }
            _ => {
                self.status = "Unknown or incomplete command; use /help".into();
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn switch_session(&mut self, selection: SessionSelection) -> Result<(), String> {
        if self.main.is_some() || self.side.is_some() || !self.input_queue.snapshot().is_empty() {
            return Err("Finish, cancel, or edit queued work before switching sessions".into());
        }
        if let Some(activity) = self.clipboard.take() {
            activity.cancel.cancel();
            let _ = activity.task.await;
        }
        let harness = Harness::load_with_layout(
            &self.harness.cwd,
            self.harness.layout.clone(),
            Options {
                selection,
                model: None,
                interactive: true,
                events: Some(self.event_tx.clone()),
                tool_events: Some(self.tool_tx.clone()),
            },
        )
        .await?;
        self.presentation = harness
            .agent
            .session
            .presentation()
            .await
            .map_err(|error| error.to_string())?;
        self.harness = harness;
        self.snapshots = self.harness.agent.session.subscribe();
        self.presentation_revision = self.snapshots.borrow().presentation_revision;
        (self.history_lines, self.message_lines) =
            render_history(&self.presentation, &self.harness, self.expanded);
        self.viewport.invalidate();
        while self.events.try_recv().is_ok() {}
        while self.tool_events.try_recv().is_ok() {}
        self.editor = Editor::default();
        for item in &self.presentation {
            if let PresentationItem::Message { message, .. } = item
                && let Message::User { content } = message.as_ref()
                && let Ok(editor) = Editor::from_content(content)
            {
                self.editor.remember(editor.draft());
            }
        }
        self.completion = None;
        self.picker = None;
        self.help = None;
        self.review = false;
        self.side_thread.clear();
        self.selection.clear();
        self.live_lines.clear();
        self.live_viewport.invalidate();
        self.skills = None;
        self.side_overlay = Overlay::default();
        self.question = None;
        self.queued_questions.clear();
        self.live_text.clear();
        self.live_thinking.clear();
        self.tools.clear();
        self.tool_order.clear();
        self.todo_panel.reset_completed();
        self.todos_scroll = 0;
        self.scroll = 0;
        self.follow = true;
        self.status = format!("Session {}", self.harness.agent.session.id());
        Ok(())
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(self.harness.palette.background)),
            area,
        );
        let todo_state = self
            .harness
            .agent
            .session
            .snapshot()
            .todo
            .as_ref()
            .and_then(|value| TodoState::from_saved(value).ok());
        let todo_lines = todo_state.as_ref().map_or_else(Vec::new, |state| {
            self.todo_panel
                .render(state, &self.harness.palette, usize::from(area.height / 3))
        });
        let todo_height = u16::try_from(todo_lines.len()).unwrap_or(u16::MAX);
        let pending = self.input_queue.snapshot();
        let mut pending_rows = pending
            .iter()
            .take(3)
            .map(|input| {
                Line::styled(
                    format!(
                        "{} {} {}",
                        crate::ui::status::Status::Pending.marker(),
                        if input.kind == crate::agent::queue::Kind::Steer {
                            "Steering:"
                        } else {
                            "Follow-up:"
                        },
                        terminal_text(&input.summary, false)
                    ),
                    Style::default().fg(self.harness.palette.muted),
                )
            })
            .collect::<Vec<_>>();
        if pending.len() > 3 {
            pending_rows.push(Line::styled(
                format!("  {} more queued", pending.len() - 3),
                Style::default().fg(self.harness.palette.dim),
            ));
        }
        if !pending.is_empty() {
            pending_rows.push(Line::styled(
                "↳ Alt+Up / Alt+Q to edit queued messages",
                Style::default().fg(self.harness.palette.dim),
            ));
        }
        let pending_height = u16::try_from(pending_rows.len()).unwrap_or(5);
        let footer_lines = footer::lines(
            &self.harness.agent.session.snapshot(),
            &self.harness.config,
            &self.harness.cwd,
            &self.harness.palette,
            area.width,
        );
        let footer_height = u16::try_from(footer_lines.len())
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_sub(4));
        let editor_height = if self.review {
            0
        } else {
            self.editor.height(
                area.width,
                (area.height / 3)
                    .max(5)
                    .min(area.height.saturating_sub(footer_height + 5).max(1)),
            )
        };
        let completion_height = self.completion.as_ref().map_or(0, |menu| {
            u16::try_from(menu.items.len().min(5) + 1).unwrap_or(6)
        });
        // No margin or padding: every pane spans the complete terminal width.
        let parts = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(todo_height),
            Constraint::Length(pending_height),
            Constraint::Length(1),
            Constraint::Length(editor_height),
            Constraint::Length(completion_height),
            Constraint::Length(footer_height),
        ])
        .split(area);
        self.transcript_area = parts[0];
        self.editor_area = parts[4];
        self.render_transcript(frame, parts[0]);
        if todo_height > 0 {
            frame.render_widget(Paragraph::new(todo_lines), parts[1]);
        }
        frame.render_widget(Paragraph::new(pending_rows), parts[2]);
        frame.render_widget(Paragraph::new(self.status_line(area.width)), parts[3]);
        let focused = self.skills.is_none()
            && self.question.is_none()
            && self.picker.is_none()
            && self.help.is_none()
            && !self.side_overlay.visible
            && !self.todos_visible
            && !self.review;
        self.editor
            .render(frame, parts[4], &self.harness.palette, focused);
        if let Some(menu) = &self.completion {
            let start = menu.selected.unwrap_or(0).saturating_sub(4);
            let mut rows = menu
                .items
                .iter()
                .enumerate()
                .skip(start)
                .take(5)
                .map(|(index, item)| {
                    Line::styled(
                        format!(
                            "{} {}  {}",
                            if menu.selected == Some(index) {
                                ">"
                            } else {
                                " "
                            },
                            terminal_text(&item.label, false),
                            terminal_text(&item.description, false)
                        ),
                        Style::default().fg(self.harness.palette.text).bg(
                            if menu.selected == Some(index) {
                                self.harness.palette.selected_bg
                            } else {
                                self.harness.palette.background
                            },
                        ),
                    )
                })
                .collect::<Vec<_>>();
            rows.push(Line::styled(
                if menu.items.is_empty() {
                    "No commands match · Esc dismiss".into()
                } else {
                    format!(
                        "↑/↓ choose · Tab complete · Enter select · Esc dismiss ({}/{})",
                        menu.selected.map_or(0, |i| i + 1),
                        menu.items.len()
                    )
                },
                Style::default().fg(self.harness.palette.dim),
            ));
            frame.render_widget(
                Paragraph::new(rows).style(Style::default().bg(self.harness.palette.background)),
                parts[5],
            );
        }
        frame.render_widget(Paragraph::new(footer_lines), parts[6]);
        if let Some(wizard) = &mut self.skills {
            wizard.render(frame, modal_area(area), &self.harness.palette);
        }
        if let Some(picker) = &mut self.picker {
            picker
                .picker
                .render(frame, modal_area(area), &self.harness.palette);
        }
        if self.todos_visible {
            self.render_todos(frame, modal_area(area));
        }
        if self.side_overlay.visible {
            let activity = self.side.as_ref().map(|side| {
                let label = self.side_backoff.as_ref().map_or_else(
                    || "Waiting for provider (no visible output yet)".into(),
                    backoff_label,
                );
                format!(
                    "{label} · {} · Ctrl+C cancel",
                    elapsed_label(side.started.elapsed())
                )
            });
            self.side_overlay.render(
                frame,
                modal_area(area),
                &self.harness.palette,
                activity.as_deref(),
            );
        }
        if let Some(help) = &mut self.help {
            help.render(frame, modal_area(area), &self.harness.palette);
        }
        if let Some(question) = &mut self.question {
            let question_area = if question.dialog.collapsed {
                collapsed_modal_area(area)
            } else {
                modal_area(area)
            };
            question
                .dialog
                .render(frame, question_area, &self.harness.palette);
        }
    }

    fn render_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let palette = &self.harness.palette;
        let mut lines = Vec::new();
        if self.presentation.is_empty() && self.main.is_none() {
            lines.push(Line::styled(
                "What would you like to work on?",
                Style::default().fg(palette.text),
            ));
            lines.push(Line::styled(
                "/model · /todos · /btw · /skills create",
                Style::default().fg(palette.dim),
            ));
        }
        if !self.live_thinking.is_empty() {
            transcript::thinking(&self.live_thinking, palette, self.expanded, &mut lines);
        }
        if !self.live_text.is_empty() {
            lines.extend(markdown::render(&self.live_text, palette));
        }
        for id in &self.tool_order {
            let persisted = self.presentation.iter().any(|item| {
                matches!(item, PresentationItem::Message { message, .. }
                    if matches!(message.as_ref(), Message::ToolResult { call_id, .. } if call_id == id))
            });
            if !persisted && let Some(tool) = self.tools.get(id) {
                transcript::tool(
                    &tool.call,
                    tool.result.as_deref(),
                    Some(&tool.output),
                    palette,
                    self.expanded,
                    &mut lines,
                );
            }
        }
        if !self.follow && self.transcript_width != 0 && self.transcript_width != area.width {
            self.scroll = reflow_scroll(
                &self.history_lines,
                self.scroll,
                self.transcript_width,
                area.width,
            );
        }
        self.transcript_width = area.width;
        self.viewport.prepare(&self.history_lines, area.width);
        let saved_height = self.viewport.height();
        self.live_lines = lines;
        self.live_viewport.invalidate();
        self.live_viewport.prepare(&self.live_lines, area.width);
        let total = saved_height.saturating_add(self.live_viewport.height());
        let max_scroll = total.saturating_sub(usize::from(area.height));
        if self.scroll_down_requested && self.scroll >= max_scroll {
            self.follow = true;
        }
        self.scroll_down_requested = false;
        self.scroll = if self.follow {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        let (mut visible, offset) = if self.scroll < saved_height {
            self.viewport
                .visible(&self.history_lines, self.scroll, area.height)
        } else {
            self.live_viewport.visible(
                &self.live_lines,
                self.scroll.saturating_sub(saved_height),
                area.height,
            )
        };
        if self.scroll < saved_height
            && self.scroll.saturating_add(usize::from(area.height)) > saved_height
        {
            let height = self.scroll.saturating_add(usize::from(area.height)) - saved_height;
            let (live, _) = self.live_viewport.visible(
                &self.live_lines,
                0,
                height.min(usize::from(area.height)) as u16,
            );
            visible.extend(live);
        }
        frame.render_widget(
            Paragraph::new(visible)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            area,
        );
        let mut selection = std::mem::take(&mut self.selection);
        selection.highlight(
            &self.selection_document(),
            frame.buffer_mut(),
            area,
            self.scroll,
            palette,
        );
        self.selection = selection;
        self.jump_area = Rect::default();
        if !self.follow && area.height > 0 && !self.selection.focused() {
            self.jump_area = Rect::new(area.x, area.bottom() - 1, area.width, 1);
            frame.render_widget(Clear, self.jump_area);
            frame.render_widget(
                Paragraph::new("↓ Jump to latest · Ctrl+End")
                    .style(Style::default().fg(palette.accent).bg(palette.selected_bg)),
                self.jump_area,
            );
        }
    }

    fn status_line(&self, width: u16) -> Line<'static> {
        if self.selection.selected() {
            let hint = if self.main.is_some() {
                "Esc cancels work"
            } else {
                "Esc clears selection"
            };
            let label = if self.copy.is_some() {
                "Copying selection"
            } else {
                "Selected text"
            };
            return Line::styled(
                footer::ellipsize(
                    &format!("{label} · Ctrl+C / Alt+C copy · {hint}"),
                    usize::from(width),
                ),
                Style::default().fg(self.harness.palette.accent),
            );
        }
        if self.review {
            return Line::styled(
                footer::ellipsize(
                    "Detailed transcript · ↑/↓ scroll · Ctrl+↑/↓ messages · Ctrl+O/Esc return",
                    usize::from(width),
                ),
                Style::default().fg(self.harness.palette.dim),
            );
        }
        let active = self.main.is_some() || self.side.is_some();
        let text = if active {
            let (elapsed, backoff) = if let Some(main) = &self.main {
                (main.started.elapsed(), self.main_backoff.as_ref())
            } else {
                let side = self.side.as_ref().expect("active side request");
                (side.started.elapsed(), self.side_backoff.as_ref())
            };
            let label = if let Some(backoff) = backoff {
                backoff_label(backoff)
            } else if self.main.is_some()
                && self.phase == Phase::Streaming
                && self.live_text.is_empty()
                && self.live_thinking.is_empty()
                && !self.live_tool_progress
            {
                "Waiting for provider (no visible output yet)".into()
            } else {
                terminal_text(&self.status, false)
            };
            format!(
                "{} {} · {} · Esc cancel",
                crate::ui::status::Status::InProgress.marker(),
                label,
                elapsed_label(elapsed)
            )
        } else if self.status == "Ready" {
            "Enter submit · Ctrl+J newline · Ctrl+O details · /help".into()
        } else {
            terminal_text(&self.status, false)
        };
        let text = if self.follow {
            text
        } else {
            format!("↑ Scrolled · Ctrl+End to follow · {text}")
        };
        Line::styled(
            footer::ellipsize(&text, usize::from(width)),
            Style::default().fg(if active {
                self.harness.palette.accent
            } else {
                self.harness.palette.dim
            }),
        )
    }

    fn render_todos(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner = crate::ui::chrome::modal(frame, area, &self.harness.palette, "Todos");
        let lines = self
            .harness
            .agent
            .session
            .snapshot()
            .todo
            .as_ref()
            .and_then(|value| TodoState::from_saved(value).ok())
            .map_or_else(
                || {
                    vec![Line::styled(
                        "No tasks",
                        Style::default().fg(self.harness.palette.dim),
                    )]
                },
                |state| {
                    state
                        .tasks
                        .iter()
                        .filter(|task| task.status != todo::state::Status::Deleted)
                        .map(|task| {
                            Line::styled(
                                todo::row(task),
                                Style::default().fg(self.harness.palette.text),
                            )
                        })
                        .collect()
                },
            );
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        self.todos_scroll = self.todos_scroll.min(
            u16::try_from(paragraph.line_count(inner.width))
                .unwrap_or(u16::MAX)
                .saturating_sub(inner.height),
        );
        frame.render_widget(paragraph.scroll((self.todos_scroll, 0)), inner);
    }
}

fn render_history(
    items: &[PresentationItem],
    harness: &Harness,
    expanded: bool,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = startup::header(
        &harness.config,
        &harness.agent.resources(),
        harness.agent.proxy_status(),
        &harness.palette,
        expanded,
    );
    let offset = lines.len();
    let (history, anchors) = transcript::render_with_anchors(items, &harness.palette, expanded);
    lines.extend(history);
    (
        lines,
        anchors.into_iter().map(|anchor| anchor + offset).collect(),
    )
}

fn reflow_scroll(lines: &[Line<'static>], scroll: usize, old_width: u16, width: u16) -> usize {
    if old_width == 0 || width == 0 {
        return scroll;
    }
    let mut old_start = 0_usize;
    let mut new_start = 0_usize;
    for line in lines {
        let paragraph = Paragraph::new(line.clone()).wrap(Wrap { trim: false });
        let old_height = paragraph.line_count(old_width).max(1);
        let new_height = paragraph.line_count(width).max(1);
        if old_start + old_height > scroll {
            let within = scroll.saturating_sub(old_start).min(new_height - 1);
            return new_start.saturating_add(within);
        }
        old_start += old_height;
        new_start += new_height;
    }
    new_start
}

fn modal_area(area: Rect) -> Rect {
    if area.width < 4 || area.height < 4 {
        return area;
    }
    let width = area.width.saturating_mul(9) / 10;
    let height = area.height.saturating_mul(4) / 5;
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn collapsed_modal_area(area: Rect) -> Rect {
    let width = area.width.saturating_mul(9) / 10;
    let height = area.height.min(3);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.bottom().saturating_sub(height),
        width,
        height,
    }
}

fn backoff_label(backoff: &Backoff) -> String {
    let seconds = backoff
        .until
        .saturating_duration_since(Instant::now())
        .as_secs_f64()
        .ceil() as u64;
    format!(
        "Retrying ({}/{}) in {seconds}s...",
        backoff.attempt, backoff.max_retries
    )
}

fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::ui::editor::Editor;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{ActiveQuestion, Activity, App, MainCompletion, SideActivity, reflow_scroll};
    use crate::agent::tools::{QuestionRequest, ToolEvent};
    use crate::agent::{Agent, Event, RunStatus};
    use crate::config::Config;
    use crate::context::{ProjectSkills, Resources};
    use crate::features::ask_user_question::{self as ask, view::Dialog};
    use crate::message::{Assistant, Message, OutputItem, OutputText, StopReason};
    use crate::providers::Responses;
    use crate::runtime::{Harness, SessionSelection};
    use crate::session::{Entry, Session};
    use crate::storage::Layout;
    use crate::theme::Palette;

    async fn fixture() -> (tempfile::TempDir, App) {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let layout = Layout {
            root: temp.path().join("mh"),
        };
        let config = Arc::new(
            Config::parse(
                &json!({
                    "defaultModel":"test/model",
                    "providers":{"test":{
                        "api":"openai-responses-sse",
                        "base_url_env":"MH_UI_TEST_BASE",
                        "api_key_env":"MH_UI_TEST_KEY",
                        "models":[{
                            "id":"model","contextWindow":64000,"maxTokens":1024,
                            "compaction":{"reserveTokens":2048,"keepRecentTokens":4000}
                        }]
                    }}
                })
                .to_string(),
            )
            .unwrap(),
        );
        std::fs::create_dir_all(layout.config_dir()).unwrap();
        std::fs::write(
            layout.config_file(),
            serde_json::to_vec_pretty(config.as_ref()).unwrap(),
        )
        .unwrap();
        let session = Arc::new(Session::create(&layout, &cwd).unwrap());
        let resources = Resources::load(&cwd, &layout.config_dir(), ProjectSkills::Skip);
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let (tool_tx, tool_rx) = tokio::sync::mpsc::channel(16);
        let agent = Arc::new(
            Agent::new(
                config.clone(),
                session.clone(),
                resources,
                Responses::with_client(reqwest::Client::builder().no_proxy().build().unwrap()),
                Some(event_tx.clone()),
                Some(tool_tx.clone()),
                true,
            )
            .await
            .unwrap(),
        );
        session
            .append(Entry::Message {
                message: Arc::new(Message::user(
                    "你好 🙂 \u{1b}]8;;https://bad.invalid\u{7}unsafe\u{1b}]8;;\u{7}",
                )),
            })
            .await
            .unwrap();
        let harness = Harness {
            agent,
            config,
            palette: Palette::default(),
            layout,
            cwd,
        };
        let app = App::new(harness, event_tx, tool_tx, event_rx, tool_rx)
            .await
            .unwrap();
        (temp, app)
    }

    fn answer(text: &str) -> Assistant {
        Assistant {
            provider: "test".into(),
            model: "model".into(),
            stop: StopReason::Stop,
            usage: None,
            error: None,
            items: vec![OutputItem::Message {
                id: None,
                phase: None,
                content: vec![OutputText::OutputText {
                    text: text.into(),
                    annotations: Vec::new(),
                }],
            }],
        }
    }

    fn question() -> Arc<ask::Params> {
        Arc::new(ask::prepare(json!({"questions":[{
            "question":"Choose?", "header":"Choice",
            "options":[{"label":"Alpha","description":"First"},{"label":"Beta","description":"Second"}]
        }]})).unwrap())
    }

    #[tokio::test]
    async fn composer_completes_commands_keeps_pastes_literal_and_rejects_unsupported_images() {
        let (_temp, mut app) = fixture().await;
        let before = app.harness.agent.session.history().await.unwrap().len();
        app.editor.insert_str("/res");
        app.update_completion(false);
        assert_eq!(
            app.completion.as_ref().unwrap().selected().unwrap().label,
            "/resume"
        );
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.editor.draft().text(), "/resume ");
        assert!(!app.quit);
        app.editor.reset();
        app.terminal_event(crossterm::event::Event::Paste(
            "/quit\nnot a submission".into(),
        ))
        .await
        .unwrap();
        assert!(!app.quit);
        assert_eq!(app.editor.draft().text(), "/quit\nnot a submission");
        app.editor.reset();
        app.editor
            .image(crate::message::Content::Image {
                data: "fixture".into(),
                mime_type: "image/png".into(),
            })
            .unwrap();
        app.submit().await.unwrap();
        assert!(app.status.contains("does not support image"));
        assert!(app.editor.draft().has_images());
        assert!(app.main.is_none());
        assert_eq!(
            app.harness.agent.session.history().await.unwrap().len(),
            before
        );
        app.editor.reset();
        app.clipboard = Some(super::ClipboardActivity {
            cancel: CancellationToken::new(),
            retain: true,
            task: tokio::spawn(async {
                Ok(crate::clipboard::Item::Image(
                    crate::message::Content::Image {
                        data: "fixture".into(),
                        mime_type: "image/png".into(),
                    },
                ))
            }),
        });
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        app.finish_tasks().await;
        assert!(
            !app.editor.draft().has_images(),
            "cancelled clipboard completion must not repopulate a cleared draft"
        );
    }

    #[tokio::test]
    async fn streaming_inputs_can_be_taken_back_and_reedited_without_stopping_work() {
        let (_temp, mut app) = fixture().await;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        app.main = Some(Activity {
            started: Instant::now(),
            cancel: cancel.clone(),
            task: tokio::spawn(async move {
                stop.cancelled().await;
                Ok(MainCompletion::Run(RunStatus::Cancelled))
            }),
        });
        for text in ["first", "second"] {
            app.editor.insert_str(text);
            app.submit().await.unwrap();
        }
        assert_eq!(app.input_queue.snapshot().len(), 2);
        assert!(app.editor.draft().is_empty());
        app.restore_queued();
        assert_eq!(app.editor.draft().text(), "first\n\nsecond");
        assert!(app.input_queue.snapshot().is_empty());
        assert!(!cancel.is_cancelled());
        app.editor.clear();
        app.editor.insert_str("edited");
        app.submit().await.unwrap();
        app.restore_queued();
        assert_eq!(app.editor.draft().text(), "edited");
        app.submit().await.unwrap();
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(app.editor.draft().text(), "edited");
        assert!(app.input_queue.snapshot().is_empty());
        app.shutdown().await;
    }

    #[tokio::test]
    async fn scrolling_back_to_bottom_resumes_following_and_review_routes_arrows() {
        let (_temp, mut app) = fixture().await;
        let mut terminal = Terminal::new(TestBackend::new(80, 28)).unwrap();
        app.live_text = format!(
            "```text\n{}\n```",
            (0..80)
                .map(|i| format!("row {i:03}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        terminal.draw(|f| app.render(f)).unwrap();
        app.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await
            .unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(!app.follow);
        app.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await
            .unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(app.follow);
        let previous = app.scroll;
        app.live_text = format!(
            "```text\n{}\n```",
            (0..100)
                .map(|i| format!("row {i:03}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        terminal.draw(|f| app.render(f)).unwrap();
        assert!(app.scroll > previous);
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("/64.0k ("));
        app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let previous = app.scroll;
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.scroll < previous);
        assert!(app.editor.draft().is_empty());
    }

    #[tokio::test]
    async fn concurrent_questionnaires_are_answered_in_order() {
        let (_temp, mut app) = fixture().await;
        let (first_reply, first) = tokio::sync::oneshot::channel();
        let (second_reply, second) = tokio::sync::oneshot::channel();
        for (call_id, reply) in [("first", first_reply), ("second", second_reply)] {
            app.tool_event(ToolEvent::Question(QuestionRequest {
                call_id: call_id.into(),
                params: question(),
                reply,
            }));
        }
        assert_eq!(app.queued_questions.len(), 1);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            first.await.unwrap().answers[0].answer.as_deref(),
            Some("Alpha")
        );
        assert!(app.question.is_some());
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(second.await.unwrap().cancelled);
        assert!(app.question.is_none());
    }

    #[tokio::test]
    async fn late_stream_events_do_not_duplicate_completed_answers() {
        let (_temp, mut app) = fixture().await;
        app.harness
            .agent
            .session
            .append(Entry::Message {
                message: Arc::new(Message::Assistant {
                    response: answer("authoritative answer"),
                }),
            })
            .await
            .unwrap();
        app.event_tx
            .send(Event::Text("late partial".into()))
            .await
            .unwrap();
        let task = tokio::spawn(async { Ok(MainCompletion::Run(RunStatus::Completed)) });
        app.main = Some(Activity {
            started: Instant::now(),
            cancel: CancellationToken::new(),
            task,
        });
        while !app.main.as_ref().unwrap().task.is_finished() {
            tokio::task::yield_now().await;
        }
        app.finish_tasks().await;
        assert!(app.live_text.is_empty());
        assert!(app.events.is_empty());
        let text = app
            .history_lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches("authoritative answer").count(), 1);
        assert!(!text.contains("late partial"));

        app.event_tx
            .send(Event::SideText("late delta".into()))
            .await
            .unwrap();
        let task = tokio::spawn(async { Ok(answer("side answer")) });
        app.side = Some(SideActivity {
            started: Instant::now(),
            question: "Side?".into(),
            cancel: CancellationToken::new(),
            task,
            retain: true,
        });
        while !app.side.as_ref().unwrap().task.is_finished() {
            tokio::task::yield_now().await;
        }
        app.finish_tasks().await;
        assert_eq!(app.side_overlay.answer, "side answer");
        assert_eq!(app.side_thread.len(), 1);
    }

    #[tokio::test]
    async fn clear_side_history_invalidates_an_in_flight_answer() {
        let (_temp, mut app) = fixture().await;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async { Ok(answer("must not reappear")) });
        app.side = Some(SideActivity {
            started: Instant::now(),
            question: "Side?".into(),
            cancel: cancel.clone(),
            task,
            retain: true,
        });
        app.side_overlay.begin("Side?".into());
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cancel.is_cancelled());
        while !app.side.as_ref().unwrap().task.is_finished() {
            tokio::task::yield_now().await;
        }
        app.finish_tasks().await;
        assert!(app.side_thread.is_empty());
        assert!(!app.side_overlay.visible);
    }

    #[test]
    fn resize_keeps_the_same_logical_transcript_line() {
        let lines = vec![
            ratatui::text::Line::raw("x".repeat(80)),
            ratatui::text::Line::raw("anchor"),
        ];
        assert_eq!(reflow_scroll(&lines, 4, 20, 40), 2);
        assert_eq!(reflow_scroll(&lines, 2, 40, 20), 4);
    }

    #[tokio::test]
    async fn transcript_mouse_keyboard_selection_and_copy_routing_preserve_editor() {
        use crossterm::event::{Event as Input, MouseButton, MouseEvent, MouseEventKind};
        let (_temp, mut app) = fixture().await;
        app.history_lines = vec![
            ratatui::text::Line::raw("alpha 世界 e\u{301}"),
            ratatui::text::Line::raw("second"),
        ];
        app.presentation = vec![crate::session::PresentationItem::Message {
            id: 1,
            message: Arc::new(Message::user("fixture")),
        }];
        app.viewport.invalidate();
        app.editor.insert_text("keep draft").unwrap();
        let mut terminal = Terminal::new(TestBackend::new(50, 18)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 0),
            (MouseEventKind::Drag(MouseButton::Left), 6),
            (MouseEventKind::Up(MouseButton::Left), 6),
        ] {
            app.terminal_event(Input::Mouse(MouseEvent {
                kind,
                column,
                row: app.transcript_area.y,
                modifiers: KeyModifiers::NONE,
            }))
            .await
            .unwrap();
        }
        assert_eq!(
            app.selection.copy(&app.selection_document()).unwrap(),
            "alpha 世"
        );
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT))
            .await
            .unwrap();
        assert_eq!(
            app.selection.copy(&app.selection_document()).unwrap(),
            "alpha 世界"
        );
        // A controlled pending writer prevents this unit test touching the OS clipboard.
        let task = tokio::spawn(std::future::pending::<Result<(), String>>());
        app.copy = Some(super::CopyActivity {
            cancel: CancellationToken::new(),
            task,
        });
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.editor.draft().text(), "keep draft");
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(0, app.transcript_area.y)].bg,
            app.harness.palette.selected_bg
        );
        app.copy.take().unwrap().task.abort();
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(!app.selection.selected());
    }

    #[tokio::test]
    async fn fully_typed_skill_create_submits_on_enter_not_only_completion() {
        let (_temp, mut app) = fixture().await;
        app.editor.insert_text("/skills create").unwrap();
        app.update_completion(false);
        assert!(app.completion.is_some());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.skills.is_some());
        assert!(app.editor.draft().is_empty());
    }

    #[tokio::test]
    async fn local_skill_wizard_publishes_once_refreshes_catalog_and_keeps_unsent_input() {
        use crossterm::event::Event as Input;
        let (_temp, mut app) = fixture().await;
        app.editor.insert_text("unsent main draft").unwrap();
        let history_before = app.harness.agent.session.history().await.unwrap().len();
        app.command("skills create").await.unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        app.terminal_event(Input::Paste("local-fixture".into()))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        app.terminal_event(Input::Paste("A controlled local skill".into()))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        app.terminal_event(Input::Paste(
            "Preserve these instructions.\nDo not invoke any tools.".into(),
        ))
        .await
        .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.skills.as_ref().unwrap().creating());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !app.skill_task.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        app.finish_tasks().await;
        assert!(app.skills.is_none());
        assert_eq!(app.editor.draft().text(), "unsent main draft");
        let resources = app.harness.agent.resources();
        let skill = resources
            .skills
            .iter()
            .find(|skill| skill.name == "local-fixture")
            .unwrap();
        assert!(skill.disabled_model_invocation);
        assert!(
            skill
                .path
                .starts_with(std::fs::canonicalize(&app.harness.cwd).unwrap())
        );
        assert!(
            resources
                .skill_request("local-fixture", "")
                .unwrap()
                .contains("Preserve these instructions")
        );
        assert!(
            !app.harness
                .agent
                .session
                .snapshot()
                .prompt
                .contains("<name>local-fixture</name>")
        );
        assert_eq!(
            app.harness.agent.session.history().await.unwrap().len(),
            history_before
        );
        assert_eq!(
            std::fs::read_to_string(&skill.path)
                .unwrap()
                .matches("name: local-fixture")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn quiet_requests_and_backoff_show_elapsed_time_and_cancellation() {
        let (_temp, mut app) = fixture().await;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            stop.cancelled().await;
            Ok(MainCompletion::Run(RunStatus::Cancelled))
        });
        app.main = Some(Activity {
            started: Instant::now() - Duration::from_secs(247),
            cancel,
            task,
        });
        app.phase = crate::agent::Phase::Streaming;
        let text = app.status_line(120).to_string();
        assert!(text.contains("Waiting for provider (no visible output yet)"));
        assert!(text.contains("4m 07s"));
        assert!(text.contains("Esc cancel"));
        app.agent_event(Event::RetryScheduled {
            scope: crate::agent::RetryScope::Main,
            attempt: 1,
            max_retries: 3,
            delay: Duration::from_secs(8),
        });
        assert!(
            app.status_line(120)
                .to_string()
                .contains("Retrying (1/3) in 8s")
        );
        app.agent_event(Event::RetryStarted {
            scope: crate::agent::RetryScope::Main,
        });
        assert!(!app.status_line(120).to_string().contains("Retrying"));
        app.agent_event(Event::StreamToolCallStarted {
            output_index: 0,
            call_id: "fixture".into(),
            name: "read".into(),
        });
        let text = app.status_line(120).to_string();
        assert!(text.contains("Preparing tool call"));
        assert!(!text.contains("no visible output yet"));
        app.agent_event(Event::Phase(crate::agent::Phase::Streaming));
        assert!(
            app.status_line(120)
                .to_string()
                .contains("no visible output yet")
        );
        app.main.as_ref().unwrap().cancel.cancel();
    }

    #[tokio::test]
    async fn large_transcripts_scroll_past_the_u16_row_limit() {
        let (_temp, mut app) = fixture().await;
        app.history_lines = (0..100_000)
            .map(|row| ratatui::text::Line::raw(format!("row {row}")))
            .collect();
        app.viewport.invalidate();
        let mut terminal = Terminal::new(TestBackend::new(80, 28)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(app.scroll > usize::from(u16::MAX));
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("row 99999"));
        app.live_text = "streaming update".into();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("streaming update"));
    }

    #[tokio::test]
    async fn presentation_updates_during_a_run_not_only_at_completion() {
        let (_temp, mut app) = fixture().await;
        app.harness
            .agent
            .session
            .append(Entry::Message {
                message: Arc::new(Message::user("new prompt")),
            })
            .await
            .unwrap();
        app.refresh_presentation().await;
        assert!(
            app.history_lines
                .iter()
                .any(|line| line.to_string().contains("new prompt"))
        );
    }

    #[tokio::test]
    async fn side_usage_does_not_rebuild_immutable_saved_history() {
        let (_temp, mut app) = fixture().await;
        app.viewport.prepare(&app.history_lines, 80);
        let height = app.viewport.height();
        let revision = app.presentation_revision;
        let pointer = app.history_lines.as_ptr();
        app.harness
            .agent
            .session
            .append(Entry::Usage {
                provider: "test".into(),
                model: "model".into(),
                kind: crate::session::UsageKind::Btw,
                usage: crate::message::Usage {
                    input: 3,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        app.refresh_presentation().await;
        assert_eq!(app.history_lines.as_ptr(), pointer);
        assert_eq!(app.presentation_revision, revision);
        assert_eq!(app.viewport.height(), height);
        assert_eq!(app.harness.agent.session.snapshot().usage.input, 3);
    }

    #[tokio::test]
    async fn oversized_escaped_input_is_rejected_before_clearing_the_draft() {
        let (_temp, mut app) = fixture().await;
        let count = app.harness.agent.session.snapshot().context.len();
        let text = "\"".repeat(17 * 1024 * 1024);
        app.editor.paste(&text).unwrap();
        app.submit().await.unwrap();
        assert!(app.status.contains("32 MiB"));
        assert!(app.main.is_none());
        assert_eq!(app.editor.draft().text(), text);
        assert_eq!(app.harness.agent.session.snapshot().context.len(), count);
    }

    #[tokio::test]
    async fn local_help_and_editing_do_not_submit_or_mutate_history() {
        let (_temp, mut app) = fixture().await;
        let count = app.presentation.len();
        for key in [
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('-'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT),
        ] {
            app.key(key).await.unwrap();
            assert!(!app.quit && app.main.is_none());
        }
        app.editor.insert_str("first");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .await
            .unwrap();
        app.editor.insert_str("second");
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.editor.draft().text(), "first\nsecond\n");
        assert!(app.main.is_none());
        app.command("help").await.unwrap();
        assert!(app.help.is_some());
        app.terminal_event(crossterm::event::Event::Paste("hidden paste".into()))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.editor.draft().text(), "first\nsecond\n");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.help.is_none() && app.main.is_none());
        assert_eq!(app.presentation.len(), count);
        for command in ["exit", "quit"] {
            app.quit = false;
            assert!(app.command(command).await.unwrap());
            assert!(app.quit);
        }
    }

    #[tokio::test]
    async fn complete_frames_render_at_narrow_and_wide_sizes() {
        let (_temp, mut app) = fixture().await;
        for width in [1, 20, 40, 80, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 28)).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(!text.contains("https://bad.invalid"));
            if width >= 40 {
                assert!(text.contains("model test/model"));
                assert!(text.contains('>'));
                let buffer = terminal.backend().buffer();
                let top = (0..28)
                    .find(|y| buffer[(0, *y)].symbol() == "─")
                    .expect("editor starts at the left edge");
                assert_eq!(
                    buffer[(width - 1, top)].symbol(),
                    "─",
                    "editor must reach the right edge"
                );
                // Ratatui resets continuation cells behind wide Unicode
                // glyphs. The physical viewport edges must still be painted.
                for y in 0..28 {
                    for x in [0, width - 1] {
                        let background = buffer[(x, y)].bg;
                        assert!(
                            background == app.harness.palette.background
                                || background == app.harness.palette.user_message_bg
                        );
                    }
                }
                assert!((0..width).all(|x| buffer[(x, 27)].bg == app.harness.palette.background));
                assert!(text.contains("cache"), "billing row must not be clipped");
            }
        }

        app.editor.insert_str("/model missing/model");
        app.submit().await.unwrap();
        assert!(app.picker.is_some());
        assert!(!app.quit);
        assert!(app.editor.draft().is_empty());
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();

        app.editor = Editor::default();
        app.editor.insert_str("/help");
        app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(app.editor.lines().iter().all(String::is_empty));
        assert!(app.help.is_some());

        let params = Arc::new(
            ask::prepare(json!({"questions":[{
                "question":"Continue?","header":"Confirm",
                "options":[
                    {"label":"Yes","description":"Continue"},
                    {"label":"No","description":"Stop"}
                ]
            }]}))
            .unwrap(),
        );
        let (reply, answer) = tokio::sync::oneshot::channel();
        app.question = Some(ActiveQuestion {
            dialog: Dialog::new(params),
            reply: Some(reply),
        });
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.question.is_none());
        assert!(answer.await.unwrap().cancelled);
        assert!(app.editor.lines().iter().all(String::is_empty));

        let previous = app.harness.agent.session.id().to_owned();
        app.switch_session(SessionSelection::New).await.unwrap();
        assert_ne!(app.harness.agent.session.id(), previous);
        assert!(app.presentation.is_empty());
        assert!(app.side_thread.is_empty());
    }
}
