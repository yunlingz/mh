//! Append-only session ownership, stable context snapshots, and lifetime usage.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::config::{Config, ModelConfig, ThinkingLevel};
use crate::message::{Assistant, Content, Message, StopReason, ToolCall, ToolResult, Usage};
use crate::storage::{Layout, new_private_file, private_dir, timestamp};

const VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;

/// Preflight the actual escaped journal representation without allocating a
/// second large payload. Maximum envelope integers cover every future record ID.
pub fn validate_user_input(content: &[Content]) -> Result<(), SessionError> {
    #[derive(Serialize)]
    struct User<'a> {
        role: &'static str,
        content: &'a [Content],
    }
    #[derive(Serialize)]
    struct MessageEntry<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        message: User<'a>,
    }
    #[derive(Serialize)]
    struct InputRecord<'a> {
        id: u64,
        timestamp: u64,
        entry: MessageEntry<'a>,
    }
    struct Limit {
        remaining: u64,
    }
    impl Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.remaining = self
                .remaining
                .checked_sub(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("journal size limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let record = InputRecord {
        id: u64::MAX,
        timestamp: u64::MAX,
        entry: MessageEntry {
            kind: "message",
            message: User {
                role: "user",
                content,
            },
        },
    };
    serde_json::to_writer(
        Limit {
            remaining: MAX_RECORD_BYTES - 1,
        },
        &record,
    )
    .map_err(|_| SessionError::LargeRecord)
}

pub const MODEL_CHOICE_MIGRATION_NOTICE: &str = "Saved model choice normalized to a configured entry with fixed reasoning effort; historical messages were not changed";

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session is already open in another process")]
    InUse,
    #[error("Session was not found in this workspace")]
    NotFound,
    #[error("Cannot read or write the session file")]
    Io(#[from] std::io::Error),
    #[error("Invalid session record at line {0}")]
    InvalidRecord(usize),
    #[error("Unsupported session format or workspace")]
    Format,
    #[error("Invalid session sequence or compaction boundary")]
    Sequence,
    #[error("Session record exceeds the 32 MiB limit")]
    LargeRecord,
    #[error("Session writer failed; start a new session before continuing")]
    FailedWriter,
    #[error("Session worker failed")]
    Worker,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelChoice {
    pub qualified: String,
    // Accepted only at the journal boundary. New records contain entry identity,
    // never a mutable effort override. Historical records are not rewritten.
    #[serde(default, rename = "thinking", skip_serializing)]
    legacy_thinking: Option<ThinkingLevel>,
}

impl ModelChoice {
    pub fn new(qualified: impl Into<String>) -> Self {
        Self {
            qualified: qualified.into(),
            legacy_thinking: None,
        }
    }

    /// Resolve old wire-ID/effort choices to one configured fixed entry. Ambiguous
    /// renames require an explicit selection rather than guessing an entry.
    pub fn normalize(&self, config: &Config) -> Result<(Self, bool), String> {
        let exact = config.model(&self.qualified).ok();
        if let Some((_, _, model)) = exact
            && self
                .legacy_thinking
                .is_none_or(|level| legacy_effort_matches(model, level))
        {
            return Ok((Self::new(&self.qualified), self.legacy_thinking.is_some()));
        }
        let (provider, wire_id) = self
            .qualified
            .split_once('/')
            .ok_or("Unknown saved model; select a configured model with --model")?;
        let definition = config
            .providers
            .get(provider)
            .ok_or("Unknown saved model; select a configured model with --model")?;
        let candidates: Vec<_> = definition
            .models
            .iter()
            .filter(|model| model.id == wire_id)
            .collect();
        let matching: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|model| {
                self.legacy_thinking
                    .is_some_and(|level| legacy_effort_matches(model, level))
            })
            .collect();
        let candidates = if matching.is_empty() {
            candidates
        } else {
            matching
        };
        let selected = if candidates.len() == 1 {
            candidates[0]
        } else if let Some(model) = candidates
            .iter()
            .find(|model| config.default_model == format!("{provider}/{}", model.local_id()))
        {
            model
        } else if let Some((_, _, model)) = exact
            && (candidates.is_empty()
                || candidates
                    .iter()
                    .any(|candidate| candidate.local_id() == model.local_id()))
        {
            model
        } else {
            return Err(
                "Saved model is missing or ambiguous; select a configured entry with --model"
                    .into(),
            );
        };
        Ok((
            Self::new(format!("{provider}/{}", selected.local_id())),
            true,
        ))
    }
}

fn legacy_effort_matches(model: &ModelConfig, level: ThinkingLevel) -> bool {
    if level == ThinkingLevel::Off {
        !model.reasoning || model.reasoning_effort.as_deref() == Some("none")
    } else {
        model.reasoning && model.reasoning_effort.as_deref() == Some(level.label())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    ProviderAttempt,
    Btw,
    CompactionAttempt,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    Session {
        version: u32,
        session_id: String,
        cwd: PathBuf,
    },
    Prompt {
        text: Arc<String>,
        tools_fingerprint: String,
        tools_tokens: u64,
    },
    Model {
        choice: ModelChoice,
    },
    Message {
        message: Arc<Message>,
    },
    Omit {
        message_id: u64,
    },
    Compaction {
        first_kept_id: Option<u64>,
        summary: Arc<String>,
        usage: Option<Usage>,
        #[serde(default)]
        tokens_before: Option<u64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "file_list"
        )]
        read_files: Option<Vec<String>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "file_list"
        )]
        modified_files: Option<Vec<String>>,
    },
    Todo {
        state: Arc<Value>,
    },
    Cancelled,
    Usage {
        provider: String,
        model: String,
        kind: UsageKind,
        usage: Usage,
    },
}

// Missing fields identify legacy checkpoints; present empty arrays are
// authoritative. Reject null rather than treating a present field as absent.
fn file_list<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Vec::deserialize(deserializer).map(Some)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: u64,
    pub timestamp: u64,
    pub entry: Entry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextSource {
    Message,
    Compaction,
}

#[derive(Clone)]
pub struct ContextItem {
    pub id: u64,
    pub message: Arc<Message>,
    pub source: ContextSource,
}

#[derive(Clone)]
pub enum PresentationItem {
    Message {
        id: u64,
        message: Arc<Message>,
    },
    Compaction {
        id: u64,
        summary: Arc<String>,
        tokens_before: Option<u64>,
    },
    Todo {
        id: u64,
        state: Arc<Value>,
    },
    Cancelled {
        id: u64,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum Estimate {
    Measured(u64),
    Approximate(u64),
}

impl Estimate {
    pub fn tokens(self) -> u64 {
        match self {
            Self::Measured(tokens) | Self::Approximate(tokens) => tokens,
        }
    }

    pub fn approximate(self) -> bool {
        matches!(self, Self::Approximate(_))
    }

    fn add(self, tokens: u64) -> Self {
        Self::Approximate(self.tokens().saturating_add(tokens))
    }
}

pub struct Snapshot {
    pub prompt: Arc<String>,
    pub tools_fingerprint: String,
    pub choice: Option<ModelChoice>,
    pub context: Arc<Vec<ContextItem>>,
    pub stable_len: usize,
    pub estimate: Estimate,
    pub usage: Usage,
    pub todo: Option<Arc<Value>>,
    pub previous_summary: Option<Arc<String>>,
    pub read_files: Option<Arc<Vec<String>>>,
    pub modified_files: Option<Arc<Vec<String>>>,
    pub last_message_id: Option<u64>,
    pub last_compaction_id: Option<u64>,
    pub last_compaction_tokens: Option<u64>,
    pub presentation_revision: u64,
}

impl Snapshot {
    pub fn messages(&self) -> Vec<Message> {
        self.context
            .iter()
            .map(|item| (*item.message).clone())
            .collect()
    }

    pub fn side_messages(&self) -> Vec<Message> {
        self.context[..self.stable_len]
            .iter()
            .map(|item| (*item.message).clone())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub updated_at: u64,
    pub first_message: String,
    pub message_count: usize,
    pub model: Option<ModelChoice>,
}

#[derive(Clone)]
pub struct Session {
    id: String,
    cwd: PathBuf,
    dir: PathBuf,
    journal: Arc<Mutex<Journal>>,
    snapshots: watch::Receiver<Arc<Snapshot>>,
}

impl Session {
    pub fn create(layout: &Layout, cwd: &Path) -> Result<Self, SessionError> {
        let cwd = std::fs::canonicalize(cwd)?;
        let id = uuid::Uuid::new_v4().to_string();
        let dir = layout.workspace_dir(&cwd)?.join(&id);
        private_dir(&dir.join("outputs"))?;
        let file = new_private_file(&dir.join("session.jsonl"))?;
        let lock_file = session_lock(&dir.join("session.lock"))?;
        let mut live = Live::default();
        let header = Record {
            id: 0,
            timestamp: timestamp(),
            entry: Entry::Session {
                version: VERSION,
                session_id: id.clone(),
                cwd: cwd.clone(),
            },
        };
        live.apply(&header);
        let (sender, snapshots) = watch::channel(Arc::new(live.snapshot()));
        let mut journal = Journal {
            file,
            _lock_file: lock_file,
            next_id: 1,
            failed: false,
            needs_newline: false,
            live,
            sender,
        };
        journal.write(&header)?;
        Ok(Self {
            id,
            cwd,
            dir,
            journal: Arc::new(Mutex::new(journal)),
            snapshots,
        })
    }

    pub fn list(layout: &Layout, cwd: &Path) -> Result<Vec<SessionInfo>, SessionError> {
        let cwd = std::fs::canonicalize(cwd)?;
        let workspace = layout.workspace_dir(&cwd)?;
        let entries = match std::fs::read_dir(workspace) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut sessions = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(id) = uuid::Uuid::parse_str(&name).map(|id| id.to_string()) else {
                continue;
            };
            if id != name {
                continue;
            }
            if let Ok(Some(info)) = inspect_session(&entry.path().join("session.jsonl"), &cwd, &id)
            {
                sessions.push(info);
            }
        }
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sessions)
    }

    pub fn latest(layout: &Layout, cwd: &Path) -> Result<Option<SessionInfo>, SessionError> {
        Ok(Self::list(layout, cwd)?.into_iter().next())
    }

    pub fn open(layout: &Layout, cwd: &Path, id: &str) -> Result<Self, SessionError> {
        let id = uuid::Uuid::parse_str(id)
            .map_err(|_| SessionError::Format)?
            .to_string();
        let cwd = std::fs::canonicalize(cwd)?;
        let dir = layout.workspace_dir(&cwd)?.join(&id);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("session.jsonl"))
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    SessionError::NotFound
                } else {
                    SessionError::Io(error)
                }
            })?;
        let lock_file = session_lock(&dir.join("session.lock"))?;
        let mut reader = BufReader::new(&mut file);
        let mut live = Live::default();
        let mut next_id = 0;
        let mut line = 0;
        let mut needs_newline = false;
        let mut valid_end = 0_u64;
        let mut repair_tail = None;
        loop {
            let mut bytes = Vec::new();
            let count = reader
                .by_ref()
                .take(MAX_RECORD_BYTES + 1)
                .read_until(b'\n', &mut bytes)?;
            if count == 0 {
                break;
            }
            line += 1;
            if count as u64 > MAX_RECORD_BYTES {
                return Err(SessionError::LargeRecord);
            }
            needs_newline = !bytes.ends_with(b"\n");
            let record: Record = match serde_json::from_slice(&bytes) {
                Ok(record) => record,
                Err(_) if needs_newline && valid_end > 0 => {
                    repair_tail = Some(valid_end);
                    break;
                }
                Err(_) => return Err(SessionError::InvalidRecord(line)),
            };
            if record.id != next_id {
                return Err(SessionError::Sequence);
            }
            if line == 1 {
                match &record.entry {
                    Entry::Session {
                        version,
                        session_id,
                        cwd: saved,
                    } if *version == VERSION && session_id == &id && saved == &cwd => {}
                    _ => return Err(SessionError::Format),
                }
            } else {
                live.validate(&record.entry)?;
            }
            live.apply(&record);
            next_id = next_id.checked_add(1).ok_or(SessionError::Sequence)?;
            valid_end = valid_end
                .checked_add(count as u64)
                .ok_or(SessionError::LargeRecord)?;
        }
        if line == 0 {
            return Err(SessionError::Format);
        }
        drop(reader);
        if let Some(length) = repair_tail {
            file.set_len(length)?;
            file.sync_data()?;
            needs_newline = false;
        }
        file.seek(SeekFrom::End(0))?;
        let (sender, snapshots) = watch::channel(Arc::new(live.snapshot()));
        let journal = Journal {
            file,
            _lock_file: lock_file,
            next_id,
            failed: false,
            needs_newline,
            live,
            sender,
        };
        Ok(Self {
            id,
            cwd,
            dir,
            journal: Arc::new(Mutex::new(journal)),
            snapshots,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn file(&self) -> PathBuf {
        self.dir.join("session.jsonl")
    }
    pub fn outputs(&self) -> PathBuf {
        self.dir.join("outputs")
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshots.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshots.clone()
    }

    pub async fn append(&self, entry: Entry) -> Result<u64, SessionError> {
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || {
            let mut journal = journal.lock().map_err(|_| SessionError::Worker)?;
            if journal.failed {
                return Err(SessionError::FailedWriter);
            }
            journal.live.validate(&entry)?;
            let record = Record {
                id: journal.next_id,
                timestamp: timestamp(),
                entry,
            };
            if let Err(error) = journal.write(&record) {
                // Size/serialization rejection occurs before any filesystem write.
                // Only an I/O failure can leave a torn journal and poison its writer.
                journal.failed = matches!(error, SessionError::Io(_));
                return Err(error);
            }
            journal.next_id = journal
                .next_id
                .checked_add(1)
                .ok_or(SessionError::Sequence)?;
            journal.live.apply(&record);
            let snapshot = Arc::new(journal.live.snapshot());
            journal.sender.send_replace(snapshot);
            Ok(record.id)
        })
        .await
        .map_err(|_| SessionError::Worker)?
    }

    pub async fn history(&self) -> Result<Vec<(u64, Arc<Message>)>, SessionError> {
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || {
            Ok(journal
                .lock()
                .map_err(|_| SessionError::Worker)?
                .live
                .history
                .clone())
        })
        .await
        .map_err(|_| SessionError::Worker)?
    }

    pub async fn presentation(&self) -> Result<Vec<PresentationItem>, SessionError> {
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || {
            Ok(journal
                .lock()
                .map_err(|_| SessionError::Worker)?
                .live
                .presentation
                .clone())
        })
        .await
        .map_err(|_| SessionError::Worker)?
    }

    pub async fn close_interrupted_calls(&self) -> Result<(), SessionError> {
        let calls = {
            let journal = self.journal.lock().map_err(|_| SessionError::Worker)?;
            journal
                .live
                .pending_order
                .iter()
                .filter_map(|id| journal.live.pending.get(id).cloned())
                .collect::<Vec<_>>()
        };
        for call in calls {
            self.append(Entry::Message {
                message: Arc::new(Message::ToolResult {
                    call_id: call.id,
                    name: call.name,
                    result: ToolResult::error(
                        "Tool outcome unknown after session exit; the call was not replayed.",
                    ),
                }),
            })
            .await?;
        }
        Ok(())
    }
}

fn inspect_session(
    path: &Path,
    cwd: &Path,
    expected_id: &str,
) -> Result<Option<SessionInfo>, SessionError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut next_id = 0_u64;
    let mut line = 0_usize;
    let mut updated_at = 0_u64;
    let mut has_user = false;
    let mut first_message = String::new();
    let mut message_count = 0;
    let mut model = None;
    loop {
        let mut bytes = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_RECORD_BYTES + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        line += 1;
        if count as u64 > MAX_RECORD_BYTES {
            return Err(SessionError::LargeRecord);
        }
        let record: Record = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(_) if !bytes.ends_with(b"\n") && line > 1 => break,
            Err(_) => return Err(SessionError::InvalidRecord(line)),
        };
        if record.id != next_id {
            return Err(SessionError::Sequence);
        }
        if line == 1 {
            match &record.entry {
                Entry::Session {
                    version,
                    session_id,
                    cwd: saved,
                } if *version == VERSION && session_id == expected_id && saved == cwd => {}
                _ => return Err(SessionError::Format),
            }
        }
        if let Entry::Model { choice } = &record.entry {
            model = Some(choice.clone());
        }
        if let Entry::Message { message } = &record.entry {
            message_count += 1;
            if first_message.is_empty()
                && let Message::User { content } = message.as_ref()
            {
                first_message = content
                    .iter()
                    .find_map(|part| match part {
                        crate::message::Content::Text { text } if !text.trim().is_empty() => {
                            Some(crate::text::terminal_text(
                                &text.chars().take(180).collect::<String>(),
                                false,
                            ))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| "[Image input]".into());
            }
        }
        has_user |= matches!(
            &record.entry,
            Entry::Message { message } if matches!(message.as_ref(), Message::User { .. })
        );
        updated_at = record.timestamp;
        next_id = next_id.checked_add(1).ok_or(SessionError::Sequence)?;
    }
    if line == 0 {
        return Err(SessionError::Format);
    }
    Ok(has_user.then(|| SessionInfo {
        id: expected_id.to_owned(),
        updated_at,
        first_message,
        message_count,
        model,
    }))
}

fn session_lock(path: &Path) -> Result<File, SessionError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    lock(&file)?;
    Ok(file)
}

fn lock(file: &File) -> Result<(), SessionError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(SessionError::InUse),
        Err(TryLockError::Error(error)) => Err(SessionError::Io(error)),
    }
}

struct Journal {
    file: File,
    _lock_file: File,
    next_id: u64,
    failed: bool,
    needs_newline: bool,
    live: Live,
    sender: watch::Sender<Arc<Snapshot>>,
}

impl Journal {
    fn write(&mut self, record: &Record) -> Result<(), SessionError> {
        let mut bytes = serde_json::to_vec(record).map_err(|_| SessionError::LargeRecord)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(SessionError::LargeRecord);
        }
        if self.needs_newline {
            self.file.write_all(b"\n")?;
            self.needs_newline = false;
        }
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        Ok(())
    }
}

struct Live {
    prompt: Arc<String>,
    tools_tokens: u64,
    tools_fingerprint: String,
    choice: Option<ModelChoice>,
    history: Vec<(u64, Arc<Message>)>,
    presentation: Vec<PresentationItem>,
    presentation_revision: u64,
    active: Vec<ContextItem>,
    pending: BTreeMap<String, ToolCall>,
    pending_order: Vec<String>,
    pending_start: Option<usize>,
    estimate: Estimate,
    usage: Usage,
    todo: Option<Arc<Value>>,
    summary: Option<Arc<String>>,
    read_files: Option<Arc<Vec<String>>>,
    modified_files: Option<Arc<Vec<String>>>,
    last_message: Option<u64>,
    last_compaction: Option<u64>,
    last_compaction_tokens: Option<u64>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            prompt: Arc::new(String::new()),
            tools_tokens: 0,
            tools_fingerprint: String::new(),
            choice: None,
            history: Vec::new(),
            presentation: Vec::new(),
            presentation_revision: 0,
            active: Vec::new(),
            pending: BTreeMap::new(),
            pending_order: Vec::new(),
            pending_start: None,
            estimate: Estimate::Approximate(0),
            usage: Usage::default(),
            todo: None,
            summary: None,
            read_files: None,
            modified_files: None,
            last_message: None,
            last_compaction: None,
            last_compaction_tokens: None,
        }
    }
}

impl Live {
    fn validate(&self, entry: &Entry) -> Result<(), SessionError> {
        match entry {
            Entry::Session { .. } => Err(SessionError::Sequence),
            Entry::Message { message } => match message.as_ref() {
                Message::User { .. } | Message::Assistant { .. } if !self.pending.is_empty() => {
                    Err(SessionError::Sequence)
                }
                Message::ToolResult { call_id, name, .. }
                    if self
                        .pending
                        .get(call_id)
                        .is_none_or(|call| &call.name != name) =>
                {
                    Err(SessionError::Sequence)
                }
                Message::Assistant { response }
                    if !matches!(response.stop, StopReason::Error | StopReason::Aborted) =>
                {
                    let calls = response.tool_calls();
                    let ids: BTreeSet<_> = calls.iter().map(|call| &call.id).collect();
                    if ids.len() != calls.len()
                        || calls
                            .iter()
                            .any(|call| call.id.is_empty() || call.name.is_empty())
                    {
                        Err(SessionError::Sequence)
                    } else {
                        Ok(())
                    }
                }
                _ => Ok(()),
            },
            Entry::Omit { message_id }
                if !self.pending.is_empty()
                    || !self.history.iter().any(|(id, message)| {
                        id == message_id && matches!(message.as_ref(), Message::Assistant { .. })
                    }) =>
            {
                Err(SessionError::Sequence)
            }
            Entry::Compaction { first_kept_id, .. } => {
                if !self.pending.is_empty()
                    || first_kept_id.is_some_and(|id| {
                        !self.active.iter().any(|item| {
                            item.id == id
                                && item.source == ContextSource::Message
                                && !matches!(item.message.as_ref(), Message::ToolResult { .. })
                        })
                    })
                {
                    Err(SessionError::Sequence)
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    fn heuristic(&mut self) {
        let tokens = (self.prompt.encode_utf16().count() as u64)
            .div_ceil(4)
            .saturating_add(self.tools_tokens)
            .saturating_add(
                self.active
                    .iter()
                    .map(|item| item.message.estimate_tokens())
                    .sum(),
            );
        self.estimate = Estimate::Approximate(tokens);
    }

    fn apply(&mut self, record: &Record) {
        if !matches!(record.entry, Entry::Usage { .. }) {
            self.presentation_revision = record.id;
        }
        match &record.entry {
            Entry::Session { .. } => {}
            Entry::Prompt {
                text,
                tools_tokens,
                tools_fingerprint,
            } => {
                self.prompt = text.clone();
                self.tools_tokens = *tools_tokens;
                self.tools_fingerprint = tools_fingerprint.clone();
                self.heuristic();
            }
            Entry::Model { choice } => {
                self.choice = Some(choice.clone());
                self.heuristic();
            }
            Entry::Todo { state } => {
                self.todo = Some(state.clone());
                self.presentation.push(PresentationItem::Todo {
                    id: record.id,
                    state: state.clone(),
                });
            }
            Entry::Cancelled => self
                .presentation
                .push(PresentationItem::Cancelled { id: record.id }),
            Entry::Usage { usage, .. } => self.usage.add(usage),
            Entry::Message { message } => {
                self.last_message = Some(record.id);
                self.history.push((record.id, message.clone()));
                self.presentation.push(PresentationItem::Message {
                    id: record.id,
                    message: message.clone(),
                });
                if let Message::Assistant { response } = message.as_ref() {
                    if let Some(usage) = &response.usage {
                        self.usage.add(usage);
                    }
                    if matches!(response.stop, StopReason::Error | StopReason::Aborted) {
                        return;
                    }
                    let calls = response.tool_calls();
                    if !calls.is_empty() {
                        self.pending_start = Some(self.active.len());
                        self.pending_order = calls.iter().map(|call| call.id.clone()).collect();
                        self.pending = calls
                            .into_iter()
                            .map(|call| (call.id.clone(), call))
                            .collect();
                    }
                }
                self.active.push(ContextItem {
                    id: record.id,
                    message: message.clone(),
                    source: ContextSource::Message,
                });
                self.estimate = self.estimate.add(message.estimate_tokens());
                if let Message::Assistant {
                    response:
                        Assistant {
                            usage: Some(usage), ..
                        },
                } = message.as_ref()
                    && usage.context_total() > 0
                {
                    self.estimate = Estimate::Measured(usage.context_total());
                }
                if let Message::ToolResult { call_id, .. } = message.as_ref() {
                    self.pending.remove(call_id);
                    if self.pending.is_empty() {
                        self.pending_start = None;
                    }
                }
            }
            Entry::Omit { message_id } => {
                let calls = self
                    .history
                    .iter()
                    .find(|(id, _)| id == message_id)
                    .and_then(|(_, message)| match message.as_ref() {
                        Message::Assistant { response } => Some(
                            response
                                .tool_calls()
                                .into_iter()
                                .map(|call| call.id)
                                .collect::<BTreeSet<_>>(),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default();
                self.active.retain(|item| item.id != *message_id && !matches!(item.message.as_ref(), Message::ToolResult { call_id, .. } if calls.contains(call_id)));
                self.presentation.retain(|item| match item {
                    PresentationItem::Message { id, .. } if id == message_id => false,
                    PresentationItem::Message { message, .. } => {
                        !matches!(message.as_ref(), Message::ToolResult { call_id, .. } if calls.contains(call_id))
                    }
                    _ => true,
                });
                self.pending.retain(|id, _| !calls.contains(id));
                if self.pending.is_empty() {
                    self.pending_start = None;
                }
                self.heuristic();
            }
            Entry::Compaction {
                first_kept_id,
                summary,
                usage,
                tokens_before,
                read_files,
                modified_files,
            } => {
                if let Some(usage) = usage {
                    self.usage.add(usage);
                }
                self.presentation.push(PresentationItem::Compaction {
                    id: record.id,
                    summary: summary.clone(),
                    tokens_before: *tokens_before,
                });
                let start = first_kept_id
                    .and_then(|id| self.active.iter().position(|item| item.id == id))
                    .unwrap_or(self.active.len());
                let mut tail = self.active[start..]
                    .iter()
                    .filter(|item| item.source == ContextSource::Message)
                    .cloned()
                    .collect::<Vec<_>>();
                self.active = vec![ContextItem {
                    id: record.id,
                    source: ContextSource::Compaction,
                    message: Arc::new(Message::user(format!(
                        "Previous context checkpoint:\n{summary}"
                    ))),
                }];
                self.active.append(&mut tail);
                self.summary = Some(summary.clone());
                self.read_files = read_files.as_ref().map(|files| Arc::new(files.clone()));
                self.modified_files = modified_files.as_ref().map(|files| Arc::new(files.clone()));
                self.last_compaction = Some(record.id);
                self.last_compaction_tokens = *tokens_before;
                self.heuristic();
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            prompt: self.prompt.clone(),
            tools_fingerprint: self.tools_fingerprint.clone(),
            choice: self.choice.clone(),
            context: Arc::new(self.active.clone()),
            stable_len: self.pending_start.unwrap_or(self.active.len()),
            estimate: self.estimate,
            usage: self.usage.clone(),
            todo: self.todo.clone(),
            previous_summary: self.summary.clone(),
            read_files: self.read_files.clone(),
            modified_files: self.modified_files.clone(),
            last_message_id: self.last_message,
            last_compaction_id: self.last_compaction,
            last_compaction_tokens: self.last_compaction_tokens,
            presentation_revision: self.presentation_revision,
        }
    }
}
