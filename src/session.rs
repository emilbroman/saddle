//! Durable, per-worktree conversation history. The lock lives independently of
//! log generations so `/new` cannot admit a second writer.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail, ensure};
use genai::chat::{ChatMessage, ChatRole, MessageContent, ToolResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::transcript::{Entry, StreamState};
use crate::watch::state::Snapshot;

const VERSION: u32 = 1;
const INTERRUPTED_TOOL: &str = "Interrupted: Saddle exited before this tool returned a result.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UiEvent {
    Push(Entry),
    Delta(String),
    Finish(StreamState),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogEvent {
    Model(ChatMessage),
    AssistantDelta(String),
    Ui(UiEvent),
    Usage { prompt_tokens: usize },
    JobStarted { id: u64, command: String },
    JobFinished { id: u64 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub observed: Snapshot,
    pub per_branch: BTreeMap<String, String>,
    pub branch: Option<String>,
    pub head: Option<String>,
}

#[derive(Debug, Default)]
pub struct Restored {
    pub messages: Vec<ChatMessage>,
    pub ui: Vec<UiEvent>,
    pub state: PersistedState,
    pub pending_jobs: BTreeMap<u64, String>,
    pub prompt_tokens: Option<usize>,
    pub existing: bool,
    pub last_job_id: u64,
    pending_model_text: String,
}

impl Restored {
    fn apply(&mut self, event: LogEvent) {
        match event {
            LogEvent::Model(message) => {
                if matches!(message.role, ChatRole::Assistant) {
                    self.pending_model_text.clear();
                }
                self.messages.push(message);
            }
            LogEvent::AssistantDelta(delta) => self.pending_model_text.push_str(&delta),
            LogEvent::Ui(event) => self.ui.push(event),
            LogEvent::Usage { prompt_tokens } => self.prompt_tokens = Some(prompt_tokens),
            LogEvent::JobStarted { id, command } => {
                self.last_job_id = self.last_job_id.max(id);
                self.pending_jobs.insert(id, command);
            }
            LogEvent::JobFinished { id } => {
                self.last_job_id = self.last_job_id.max(id);
                self.pending_jobs.remove(&id);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    version: u32,
    payload: T,
}

#[derive(Serialize, Deserialize)]
enum Record {
    Start { generation: String },
    Event(LogEvent),
}

#[derive(Serialize, Deserialize)]
struct StateFile {
    generation: String,
    state: PersistedState,
}

struct Inner {
    _lock: File,
    log: File,
    generation: String,
    state: PersistedState,
    failed: bool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // A concurrent fork can briefly inherit this descriptor before exec.
        // Release our lock now rather than waiting for that child to close it.
        unsafe {
            libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Clone)]
pub struct Session {
    dir: Arc<PathBuf>,
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Session {
    #[cfg(test)]
    pub(crate) fn fail_writes(&self) {
        self.inner.lock().unwrap().log = File::open(self.dir.join("events.jsonl")).unwrap();
    }

    pub fn open(root: &Path) -> Result<(Self, Restored)> {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .context("Neither XDG_DATA_HOME nor HOME provides a session storage directory")?;
        Self::open_in(root, &data_home)
    }

    pub fn open_in(root: &Path, data_home: &Path) -> Result<(Self, Restored)> {
        let root = root.canonicalize().context("Canonicalizing session root")?;
        let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        ensure!(
            data_home.is_absolute(),
            "Session data directory must be absolute"
        );
        let dir = canonical_destination(&data_home.join("saddle/sessions").join(key))?;
        ensure!(
            !dir.starts_with(&root),
            "Session storage must be outside the project: {}",
            dir.display()
        );
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .context("Creating session storage")?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let lock = lock(&dir.join("lock"))?;
        let log_path = dir.join("events.jsonl");
        let resumed = log_path.try_exists()?;
        if !resumed {
            replace_log(&dir, &generation())?;
        }
        let mut log = OpenOptions::new().read(true).append(true).open(&log_path)?;
        let (generation, mut restored) = replay(&mut log).context("Reading session event log")?;
        restored.existing = resumed;
        let state_path = dir.join("state.json");
        if state_path.try_exists()? {
            let state: StateFile =
                decode(&fs::read(&state_path)?).context("Reading session snapshots")?;
            if state.generation == generation {
                restored.state = state.state;
            }
        }
        let session = Self {
            dir: Arc::new(dir),
            inner: Arc::new(Mutex::new(Inner {
                _lock: lock,
                log,
                generation,
                state: restored.state.clone(),
                failed: false,
            })),
        };
        session.repair_interrupted(&mut restored)?;
        Ok((session, restored))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn append(&self, event: LogEvent) -> Result<()> {
        let bytes = encode(&Record::Event(event))?;
        let mut inner = self.inner.lock().unwrap();
        ensure!(
            !inner.failed,
            "Session log failed to write; restart Saddle before continuing"
        );
        let result = inner
            .log
            .write_all(&bytes)
            .and_then(|()| inner.log.sync_data());
        if result.is_err() {
            inner.failed = true;
        }
        result.context("Persisting session event")
    }

    pub fn save_state(&self, state: &PersistedState) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        self.write_state(&inner.generation, state)?;
        inner.state = state.clone();
        Ok(())
    }

    pub fn save_snapshots(&self, observed: Snapshot) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let mut state = inner.state.clone();
        state.observed = observed;
        self.write_state(&inner.generation, &state)?;
        inner.state = state;
        Ok(())
    }

    pub fn git_state(&self) -> crate::git::GitState {
        let inner = self.inner.lock().unwrap();
        crate::git::GitState {
            per_branch: inner.state.per_branch.clone(),
            branch: inner.state.branch.clone(),
            head: inner.state.head.clone(),
        }
    }

    pub fn generation(&self) -> String {
        self.inner.lock().unwrap().generation.clone()
    }

    pub fn save_git(&self, git: crate::git::GitState, generation: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            return Ok(());
        }
        let mut state = inner.state.clone();
        state.per_branch = git.per_branch;
        state.branch = git.branch;
        state.head = git.head;
        self.write_state(&inner.generation, &state)?;
        inner.state = state;
        Ok(())
    }

    fn write_state(&self, generation: &str, state: &PersistedState) -> Result<()> {
        let bytes = encode(&StateFile {
            generation: generation.to_owned(),
            state: state.clone(),
        })?;
        atomic_write(&self.dir.join("state.json"), &bytes).context("Persisting session snapshots")
    }

    /// Archive the old generation before atomically publishing an empty one.
    /// A crash at any point leaves either the old or the new log resumable.
    pub fn reset(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let archives = self.dir.join("archives");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&archives)?;
        let archive = archives.join(format!("{}.jsonl", inner.generation));
        if !archive.try_exists()? {
            fs::hard_link(self.dir.join("events.jsonl"), &archive)?;
        }
        File::open(&archives)?.sync_all()?;
        let generation = generation();
        let log = replace_log(&self.dir, &generation)?;
        inner.log = log;
        inner.generation = generation;
        inner.state = PersistedState::default();
        inner.failed = false;
        // The generation mismatch invalidates old state without a second commit.
        Ok(())
    }

    fn repair_interrupted(&self, restored: &mut Restored) -> Result<()> {
        if !restored.pending_model_text.trim().is_empty() {
            let event =
                LogEvent::Model(ChatMessage::assistant(restored.pending_model_text.clone()));
            self.append(event.clone())?;
            restored.apply(event);
        }
        let mut pending = BTreeMap::new();
        for message in &restored.messages {
            for call in message.content.tool_calls() {
                pending.insert(call.call_id.clone(), call.clone());
            }
            for response in message.content.tool_responses() {
                pending.remove(&response.call_id);
            }
        }
        for call in pending.into_values() {
            let event = LogEvent::Model(ChatMessage::tool(MessageContent::from_tool_responses(
                vec![ToolResponse::from_tool_call(&call, INTERRUPTED_TOOL)],
            )));
            self.append(event.clone())?;
            restored.apply(event);
            let event = LogEvent::Ui(UiEvent::Push(Entry::ToolResult {
                call_id: call.call_id,
                output: INTERRUPTED_TOOL.into(),
                is_error: true,
            }));
            self.append(event.clone())?;
            restored.apply(event);
        }
        let mut streaming = false;
        for event in &restored.ui {
            match event {
                UiEvent::Delta(_) => streaming = true,
                UiEvent::Push(Entry::Assistant { state, .. }) => {
                    streaming = *state == StreamState::Streaming
                }
                UiEvent::Push(_) | UiEvent::Finish(_) => streaming = false,
            }
        }
        if streaming {
            let event = LogEvent::Ui(UiEvent::Finish(StreamState::Interrupted));
            self.append(event.clone())?;
            restored.apply(event);
        }
        Ok(())
    }
}

fn canonical_destination(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("Session data directory has no existing ancestor")?;
            let mut parent = canonical_destination(parent)?;
            match path.components().next_back() {
                Some(std::path::Component::ParentDir) => {
                    parent.pop();
                }
                Some(std::path::Component::Normal(name)) => parent.push(name),
                _ => {}
            }
            Ok(parent)
        }
        Err(error) => Err(error.into()),
    }
}

fn generation() -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{timestamp}-{}", std::process::id())
}

fn encode<T: Serialize>(payload: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&Envelope {
        version: VERSION,
        payload,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let envelope: Envelope<serde_json::Value> = serde_json::from_slice(bytes)?;
    ensure!(
        envelope.version == VERSION,
        "Unsupported session format version {} (supported: {VERSION})",
        envelope.version
    );
    Ok(serde_json::from_value(envelope.payload)?)
}

fn replay(log: &mut File) -> Result<(String, Restored)> {
    log.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&mut *log);
    let mut restored = Restored::default();
    let mut generation = None;
    let mut valid_bytes = 0;
    let mut line = Vec::new();
    let mut number = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            break;
        }
        number += 1;
        let record: Record =
            decode(&line).with_context(|| format!("Invalid session record {number}"))?;
        match record {
            Record::Start { generation: value } if generation.is_none() && number == 1 => {
                generation = Some(value);
            }
            Record::Event(event) if generation.is_some() => restored.apply(event),
            _ => bail!("Invalid session header at record {number}"),
        }
        valid_bytes += line.len() as u64;
    }
    let generation = generation.context("Session log has no complete header")?;
    drop(reader);
    if log.metadata()?.len() != valid_bytes {
        log.set_len(valid_bytes)?;
        log.sync_data()?;
    }
    Ok((generation, restored))
}

fn replace_log(dir: &Path, generation: &str) -> Result<File> {
    let path = dir.join("events.jsonl");
    atomic_write(
        &path,
        &encode(&Record::Start {
            generation: generation.to_owned(),
        })?,
    )?;
    Ok(OpenOptions::new().read(true).append(true).open(path)?)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .context("Session file has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn lock(path: &Path) -> Result<File> {
    use std::os::fd::AsRawFd;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // The inode must remain in place: unlinking a held lock admits a second owner.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        let mut pid = String::new();
        let _ = file.read_to_string(&mut pid);
        bail!(
            "Another Saddle instance holds this session (PID {}): {error}",
            pid.trim()
        );
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::Observed;
    use genai::chat::ToolCall;

    struct Fixture {
        root: tempfile::TempDir,
        data: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                data: tempfile::tempdir().unwrap(),
            }
        }

        fn open(&self) -> (Session, Restored) {
            Session::open_in(self.root.path(), self.data.path()).unwrap()
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            fn_name: "read_file".into(),
            fn_arguments: serde_json::json!({"path":"src/a.rs"}),
            thought_signatures: Some(vec!["opaque-call-signature".into()]),
        }
    }

    #[test]
    fn inherited_descriptor_does_not_prolong_a_closed_session_lock() {
        let fixture = Fixture::new();
        let (session, _) = fixture.open();
        let inherited = session.inner.lock().unwrap()._lock.try_clone().unwrap();
        drop(session);
        let (reopened, _) = fixture.open();
        assert!(Session::open_in(fixture.root.path(), fixture.data.path()).is_err());
        drop(inherited);
        drop(reopened);
    }

    #[test]
    fn exact_model_and_ui_roundtrip_including_reasoning_and_signatures() {
        let f = Fixture::new();
        let (session, initial) = f.open();
        assert!(!initial.existing);
        let messages = vec![
            ChatMessage::user("Explain this"),
            ChatMessage::assistant_tool_calls_with_thoughts(
                vec![call("one")],
                vec!["opaque-turn-signature".into()],
            )
            .with_reasoning_content(Some("reasoning text".into())),
            ChatMessage::tool(MessageContent::from_tool_responses(vec![
                ToolResponse::new("one", "contents"),
            ])),
            ChatMessage::assistant("The answer"),
        ];
        for message in &messages {
            session.append(LogEvent::Model(message.clone())).unwrap();
        }
        let ui = vec![
            UiEvent::Push(Entry::User {
                text: "Explain this".into(),
            }),
            UiEvent::Delta("The ".into()),
            UiEvent::Delta("answer".into()),
            UiEvent::Finish(StreamState::Complete),
        ];
        for event in &ui {
            session.append(LogEvent::Ui(event.clone())).unwrap();
        }
        session
            .append(LogEvent::Usage { prompt_tokens: 123 })
            .unwrap();
        drop(session);
        let (_session, restored) = f.open();
        assert!(restored.existing);
        assert_eq!(
            serde_json::to_value(restored.messages).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
        assert_eq!(
            serde_json::to_value(restored.ui).unwrap(),
            serde_json::to_value(ui).unwrap()
        );
        assert_eq!(restored.prompt_tokens, Some(123));
    }

    #[test]
    fn interruption_preserves_completed_results_and_repairs_only_missing_results() {
        let f = Fixture::new();
        let (session, _) = f.open();
        session
            .append(LogEvent::Model(ChatMessage::assistant(
                MessageContent::from_tool_calls(vec![call("one"), call("two")]),
            )))
            .unwrap();
        session
            .append(LogEvent::Model(ChatMessage::tool(
                MessageContent::from_tool_responses(vec![ToolResponse::new("one", "saved result")]),
            )))
            .unwrap();
        drop(session);
        let (session, restored) = f.open();
        let responses: Vec<_> = restored
            .messages
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].content, "saved result");
        assert_eq!(responses[1].call_id, "two");
        assert_eq!(responses[1].content, INTERRUPTED_TOOL);
        drop(session);
        let (_, again) = f.open();
        assert_eq!(again.messages.len(), restored.messages.len());
    }

    #[test]
    fn crash_recovers_partial_model_text_without_duplicating_completed_messages() {
        let f = Fixture::new();
        let (session, _) = f.open();
        session
            .append(LogEvent::AssistantDelta("complete".into()))
            .unwrap();
        session
            .append(LogEvent::Model(ChatMessage::assistant("complete")))
            .unwrap();
        session
            .append(LogEvent::AssistantDelta("part ".into()))
            .unwrap();
        session
            .append(LogEvent::AssistantDelta("two".into()))
            .unwrap();
        drop(session);
        let (session, restored) = f.open();
        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[1].content.texts().join(""), "part two");
        drop(session);
        assert_eq!(f.open().1.messages.len(), 2);
    }

    #[test]
    fn trailing_stream_is_interrupted_once_on_resume() {
        let f = Fixture::new();
        let (session, _) = f.open();
        session
            .append(LogEvent::Ui(UiEvent::Delta("partial answer".into())))
            .unwrap();
        drop(session);
        let (session, restored) = f.open();
        assert!(matches!(
            restored.ui.last(),
            Some(UiEvent::Finish(StreamState::Interrupted))
        ));
        drop(session);
        assert_eq!(f.open().1.ui.len(), 2);
    }

    #[test]
    fn lock_refuses_second_owner_and_stale_pid_does_not_block_recovery() {
        let f = Fixture::new();
        let (session, _) = f.open();
        let clone = session.clone();
        let path = session.dir().join("lock");
        let error = Session::open_in(f.root.path(), f.data.path())
            .err()
            .unwrap();
        assert!(error.to_string().contains("Another Saddle instance"));
        assert!(error.to_string().contains(&std::process::id().to_string()));
        drop(session);
        assert!(Session::open_in(f.root.path(), f.data.path()).is_err());
        drop(clone);
        fs::write(path, "999999999\n").unwrap();
        let (_session, _) = f.open();
    }

    #[test]
    fn state_versions_are_refused_and_session_files_are_private() {
        let f = Fixture::new();
        let (session, _) = f.open();
        session.save_state(&PersistedState::default()).unwrap();
        assert_eq!(
            fs::metadata(session.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["lock", "events.jsonl", "state.json"] {
            assert_eq!(
                fs::metadata(session.dir().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let path = session.dir().join("state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state["version"] = 999.into();
        fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
        drop(session);
        let error = Session::open_in(f.root.path(), f.data.path())
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("Unsupported session format version 999"));
    }

    #[test]
    fn cloned_handles_serialize_appends_without_losing_records() {
        let f = Fixture::new();
        let (session, _) = f.open();
        std::thread::scope(|scope| {
            for thread in 0..4 {
                let session = session.clone();
                scope.spawn(move || {
                    for number in 0..10 {
                        session
                            .append(LogEvent::Model(ChatMessage::user(format!(
                                "{thread}:{number}"
                            ))))
                            .unwrap();
                    }
                });
            }
        });
        drop(session);
        assert_eq!(f.open().1.messages.len(), 40);
    }

    #[test]
    fn unsupported_versions_are_refused_without_modification() {
        let f = Fixture::new();
        let (session, _) = f.open();
        let path = session.dir().join("events.jsonl");
        drop(session);
        let unsupported = b"{\"version\":999,\"payload\":{}}\n";
        fs::write(&path, unsupported).unwrap();
        let error = Session::open_in(f.root.path(), f.data.path())
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("Unsupported session format version 999"));
        assert_eq!(fs::read(path).unwrap(), unsupported);
    }

    #[test]
    fn only_unterminated_final_record_is_recovered() {
        let f = Fixture::new();
        let (session, _) = f.open();
        session
            .append(LogEvent::Model(ChatMessage::user("kept")))
            .unwrap();
        let path = session.dir().join("events.jsonl");
        let good_len = fs::metadata(&path).unwrap().len();
        drop(session);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"version\":1,\"pay")
            .unwrap();
        let (session, restored) = f.open();
        assert_eq!(restored.messages.len(), 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), good_len);
        session
            .append(LogEvent::Model(ChatMessage::user("also kept")))
            .unwrap();
        drop(session);
        assert_eq!(f.open().1.messages.len(), 2);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"corrupt complete record\n")
            .unwrap();
        let error = Session::open_in(f.root.path(), f.data.path())
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("Invalid session record 4"));
    }

    #[test]
    fn state_and_job_handles_roundtrip_and_reset_archives_log() {
        let f = Fixture::new();
        let path = f.root.path().join("file");
        fs::write(&path, "snapshot").unwrap();
        let observed = Observed::default();
        observed.record_read(&path, &fs::metadata(&path).unwrap(), b"snapshot");
        let state = PersistedState {
            observed: observed.snapshot(),
            per_branch: [("main".into(), "abc123".into())].into(),
            branch: Some("main".into()),
            head: Some("abc123".into()),
        };
        let (session, _) = f.open();
        session.save_state(&state).unwrap();
        session
            .append(LogEvent::Model(ChatMessage::user("old")))
            .unwrap();
        session
            .append(LogEvent::JobStarted {
                id: 41,
                command: "cargo test".into(),
            })
            .unwrap();
        session
            .append(LogEvent::JobStarted {
                id: 42,
                command: "ls".into(),
            })
            .unwrap();
        session.append(LogEvent::JobFinished { id: 42 }).unwrap();
        drop(session);
        let (session, restored) = f.open();
        assert_eq!(
            serde_json::to_value(restored.state).unwrap(),
            serde_json::to_value(state).unwrap()
        );
        assert_eq!(restored.pending_jobs, [(41, "cargo test".into())].into());
        assert_eq!(restored.last_job_id, 42);
        session.reset().unwrap();
        let archives: Vec<_> = fs::read_dir(session.dir().join("archives"))
            .unwrap()
            .collect();
        assert_eq!(archives.len(), 1);
        assert!(
            fs::read_to_string(archives[0].as_ref().unwrap().path())
                .unwrap()
                .contains("cargo test")
        );
        drop(session);
        let (_, restored) = f.open();
        assert!(restored.messages.is_empty());
        assert!(restored.pending_jobs.is_empty());
        assert!(restored.state.per_branch.is_empty());
        assert_eq!(
            serde_json::to_value(restored.state.observed).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn reset_generation_rejects_stale_state_after_a_crash() {
        let f = Fixture::new();
        let (session, _) = f.open();
        session
            .save_state(&PersistedState {
                head: Some("old".into()),
                ..Default::default()
            })
            .unwrap();
        let path = session.dir().join("state.json");
        let old_state = fs::read(&path).unwrap();
        session.reset().unwrap();
        fs::write(&path, old_state).unwrap();
        drop(session);
        assert_eq!(f.open().1.state.head, None);
    }

    #[test]
    fn canonical_roots_share_identity_and_storage_never_enters_project() {
        let f = Fixture::new();
        let (session, _) = f.open();
        let dir = session.dir().to_owned();
        drop(session);
        let (other, _) = Session::open_in(&f.root.path().join("."), f.data.path()).unwrap();
        assert_eq!(other.dir(), dir);
        assert!(fs::read_dir(f.root.path()).unwrap().next().is_none());
        assert!(Session::open_in(f.root.path(), &f.root.path().join("data")).is_err());
        assert!(!f.root.path().join("data").exists());
    }

    #[cfg(unix)]
    #[test]
    fn storage_symlink_into_project_is_refused() {
        let f = Fixture::new();
        let data = f.data.path().join("linked");
        std::os::unix::fs::symlink(f.root.path(), &data).unwrap();
        assert!(Session::open_in(f.root.path(), &data).is_err());
        assert!(fs::read_dir(f.root.path()).unwrap().next().is_none());
    }
}
