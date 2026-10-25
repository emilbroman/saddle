//! One language-server process per (language, workspace root): spawning,
//! initialisation, server-initiated messages, document sync from disk, and a
//! supervisor that restarts a crashed server a bounded number of times.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_lsp::router::Router;
use async_lsp::{ErrorCode, MainLoop, ResponseError, ServerSocket};
use globset::{GlobBuilder, GlobMatcher};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument,
    DidSaveTextDocument, Exit, Initialized, LogMessage, Notification, Progress, PublishDiagnostics,
    ShowMessage,
};
use lsp_types::request::{
    ApplyWorkspaceEdit, Initialize, RegisterCapability, Request, ShowDocument, ShowMessageRequest,
    Shutdown, UnregisterCapability, WorkDoneProgressCreate, WorkspaceConfiguration,
    WorkspaceFoldersRequest,
};
use lsp_types::{
    ApplyWorkspaceEditResponse, ClientCapabilities, ClientInfo, DidChangeTextDocumentParams,
    DidChangeWatchedFilesClientCapabilities, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolClientCapabilities,
    FileChangeType, FileEvent, GeneralClientCapabilities, GlobPattern, GotoCapability,
    HoverClientCapabilities, InitializeParams, InitializedParams, MarkupKind, NumberOrString,
    OneOf, ProgressParamsValue, PublishDiagnosticsClientCapabilities, ShowDocumentResult,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentSyncCapability, TextDocumentSyncClientCapabilities,
    TextDocumentSyncKind, TextDocumentSyncSaveOptions, Url, VersionedTextDocumentIdentifier,
    WatchKind, WindowClientCapabilities, WorkDoneProgress, WorkspaceClientCapabilities,
    WorkspaceFolder,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use super::diagnostics::DiagnosticsStore;
use super::encoding::PositionEncoding;
use super::{LspError, ServerState, ServerStatus};
use crate::config::LspServer;
use crate::events::LspEvent;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// A server may crash this many times within `CRASH_WINDOW` and still be restarted.
const MAX_RESTARTS: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(300);
/// Documents beyond this are closed, least recently used first.
const MAX_OPEN_DOCUMENTS: usize = 64;
const LOG_LINES: usize = 200;
const STDERR_RELEVANCE: Duration = Duration::from_secs(3);
const SERVER_CANCELLED: ErrorCode = ErrorCode(-32802);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerKey {
    pub language: String,
    pub root: PathBuf,
}

/// State shared by the manager and every server.
pub(crate) struct Shared {
    pub diagnostics: DiagnosticsStore,
    activity: watch::Sender<u64>,
    states: Mutex<BTreeMap<ServerKey, ServerState>>,
    events: Option<mpsc::UnboundedSender<LspEvent>>,
    next_instance: AtomicU64,
    /// Change batches accepted but not yet applied.
    pub pending: AtomicUsize,
}

impl Shared {
    pub fn new(events: Option<mpsc::UnboundedSender<LspEvent>>) -> Self {
        Self {
            diagnostics: DiagnosticsStore::default(),
            activity: watch::Sender::new(0),
            states: Mutex::default(),
            events,
            next_instance: AtomicU64::new(1),
            pending: AtomicUsize::new(0),
        }
    }

    /// Something happened that may change diagnostics.
    pub fn bump(&self) {
        self.activity.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn activity(&self) -> watch::Receiver<u64> {
        self.activity.subscribe()
    }

    /// Whether any server is starting or reporting work in progress, or
    /// changes are still being applied.
    pub fn busy(&self) -> bool {
        self.pending.load(Ordering::SeqCst) > 0
            || self.states.lock().unwrap().values().any(|s| {
                matches!(
                    s,
                    ServerState::Starting | ServerState::Restarting { .. } | ServerState::Busy(_)
                )
            })
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        self.states
            .lock()
            .unwrap()
            .iter()
            .map(|(key, state)| ServerStatus {
                language: key.language.clone(),
                root: key.root.clone(),
                state: state.clone(),
            })
            .collect()
    }

    fn set_state(&self, key: &ServerKey, state: ServerState) {
        let changed = {
            let mut states = self.states.lock().unwrap();
            states.insert(key.clone(), state.clone()) != Some(state)
        };
        if changed {
            if let Some(events) = &self.events {
                let _ = events.send(LspEvent::Status(self.statuses()));
            }
            self.bump();
        }
    }
}

#[derive(Clone)]
pub(crate) enum Phase {
    Starting,
    Running(Arc<Running>),
    Restarting { attempt: u32, reason: String },
    Failed(String),
    Stopped,
}

pub(crate) struct Running {
    socket: ServerSocket,
    pub encoding: PositionEncoding,
    sync: SyncCaps,
    instance: Arc<Instance>,
}

#[derive(Debug, Clone, Copy)]
struct SyncCaps {
    open_close: bool,
    change: bool,
    /// `Some(include_text)` when the server wants `didSave`.
    save: Option<bool>,
}

impl SyncCaps {
    fn from(capability: Option<&TextDocumentSyncCapability>) -> Self {
        match capability {
            None => Self {
                open_close: false,
                change: false,
                save: None,
            },
            Some(TextDocumentSyncCapability::Kind(kind)) => Self {
                open_close: *kind != TextDocumentSyncKind::NONE,
                change: *kind != TextDocumentSyncKind::NONE,
                save: None,
            },
            Some(TextDocumentSyncCapability::Options(options)) => Self {
                open_close: options.open_close.unwrap_or(false),
                change: options
                    .change
                    .is_some_and(|k| k != TextDocumentSyncKind::NONE),
                save: match &options.save {
                    Some(TextDocumentSyncSaveOptions::Supported(true)) => Some(false),
                    Some(TextDocumentSyncSaveOptions::SaveOptions(o)) => {
                        Some(o.include_text.unwrap_or(false))
                    }
                    _ => None,
                },
            },
        }
    }
}

/// One process lifetime of a server.
struct Instance {
    id: u64,
    encoding: OnceLock<PositionEncoding>,
    progress: Mutex<BTreeMap<String, String>>,
    watchers: Mutex<BTreeMap<String, Vec<FileWatcher>>>,
}

struct FileWatcher {
    base: Option<PathBuf>,
    glob: GlobMatcher,
    kind: WatchKind,
}

impl FileWatcher {
    fn new(watcher: lsp_types::FileSystemWatcher) -> Option<Self> {
        let (base, pattern) = match watcher.glob_pattern {
            GlobPattern::String(pattern) => (None, pattern),
            GlobPattern::Relative(relative) => {
                let uri = match relative.base_uri {
                    OneOf::Left(folder) => folder.uri,
                    OneOf::Right(uri) => uri,
                };
                (Some(uri.to_file_path().ok()?), relative.pattern)
            }
        };
        let glob = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .ok()?
            .compile_matcher();
        Some(Self {
            base,
            glob,
            kind: watcher
                .kind
                .unwrap_or(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
        })
    }

    fn matches(&self, path: &Path, change: FileChangeType) -> bool {
        let kind = match change {
            FileChangeType::CREATED => WatchKind::Create,
            FileChangeType::DELETED => WatchKind::Delete,
            _ => WatchKind::Change,
        };
        if !self.kind.contains(kind) {
            return false;
        }
        match &self.base {
            Some(base) => path
                .strip_prefix(base)
                .is_ok_and(|relative| self.glob.is_match(relative)),
            None => self.glob.is_match(path),
        }
    }
}

struct Document {
    version: i32,
    text: String,
    last_used: u64,
}

#[derive(Default)]
struct Documents {
    open: HashMap<PathBuf, Document>,
    clock: u64,
}

enum Outcome {
    Shutdown,
    Crashed(String),
}

pub(crate) struct Server {
    pub key: ServerKey,
    config: LspServer,
    shared: Arc<Shared>,
    documents: Mutex<Documents>,
    phase: watch::Sender<Phase>,
    shutdown: CancellationToken,
    log: Mutex<VecDeque<String>>,
    last_stderr: Mutex<Option<(Instant, String)>>,
}

impl Server {
    pub fn new(key: ServerKey, config: LspServer, shared: Arc<Shared>) -> Arc<Self> {
        let server = Arc::new(Self {
            key,
            config,
            shared,
            documents: Mutex::default(),
            phase: watch::Sender::new(Phase::Starting),
            shutdown: CancellationToken::new(),
            log: Mutex::default(),
            last_stderr: Mutex::default(),
        });
        server.publish_state();
        server
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    fn push_log(&self, line: String) {
        let mut log = self.log.lock().unwrap();
        if log.len() == LOG_LINES {
            log.pop_front();
        }
        log.push_back(line);
    }

    fn running(&self) -> Option<Arc<Running>> {
        match &*self.phase.borrow() {
            Phase::Running(running) => Some(running.clone()),
            _ => None,
        }
    }

    fn set_phase(&self, phase: Phase) {
        self.phase.send_replace(phase);
        self.publish_state();
    }

    fn publish_state(&self) {
        let state = match &*self.phase.borrow() {
            Phase::Starting => ServerState::Starting,
            Phase::Running(running) => {
                let progress = running.instance.progress.lock().unwrap();
                match progress.values().next() {
                    Some(title) => ServerState::Busy(title.clone()),
                    None => ServerState::Ready,
                }
            }
            Phase::Restarting { attempt, reason } => ServerState::Restarting {
                attempt: *attempt,
                reason: reason.clone(),
            },
            Phase::Failed(reason) => ServerState::Failed(reason.clone()),
            Phase::Stopped => ServerState::Stopped,
        };
        self.shared.set_state(&self.key, state);
    }

    /// Waits until the server is running, or fails if it cannot get there.
    pub async fn ready(&self) -> Result<Arc<Running>, LspError> {
        let mut phase = self.phase.subscribe();
        let wait = async {
            loop {
                match &*phase.borrow_and_update() {
                    Phase::Running(running) => return Ok(running.clone()),
                    Phase::Failed(reason) => return Err(self.failed(reason)),
                    Phase::Stopped => return Err(LspError::Stopped),
                    Phase::Starting | Phase::Restarting { .. } => {}
                }
                if phase.changed().await.is_err() {
                    return Err(LspError::Stopped);
                }
            }
        };
        tokio::time::timeout(INITIALIZE_TIMEOUT, wait)
            .await
            .map_err(|_| LspError::Timeout {
                language: self.key.language.clone(),
                what: "start".into(),
            })?
    }

    fn failed(&self, reason: &str) -> LspError {
        LspError::Failed {
            language: self.key.language.clone(),
            reason: reason.to_owned(),
        }
    }

    /// Sends a request once the server is running, retrying while the server
    /// reports that its state changed underneath the request.
    pub async fn request<R>(&self, params: R::Params) -> Result<R::Result, LspError>
    where
        R: Request,
        R::Params: Clone,
    {
        let running = self.ready().await?;
        let mut attempt = 0;
        loop {
            let response =
                tokio::time::timeout(REQUEST_TIMEOUT, running.socket.request::<R>(params.clone()))
                    .await
                    .map_err(|_| LspError::Timeout {
                        language: self.key.language.clone(),
                        what: R::METHOD.into(),
                    })?;
            match response {
                Ok(result) => return Ok(result),
                Err(async_lsp::Error::Response(e))
                    if (e.code == ErrorCode::CONTENT_MODIFIED || e.code == SERVER_CANCELLED)
                        && attempt < 5 =>
                {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200 * attempt)).await;
                }
                Err(e) => {
                    return Err(LspError::Request {
                        language: self.key.language.clone(),
                        method: R::METHOD.into(),
                        message: e.to_string(),
                    });
                }
            }
        }
    }

    /// Text of an open document.
    pub fn document_text(&self, path: &Path) -> Option<String> {
        let docs = self.documents.lock().unwrap();
        docs.open.get(path).map(|d| d.text.clone())
    }

    /// Opens `path` from disk if it is not open yet; returns its content.
    pub fn ensure_open(&self, path: &Path) -> Result<String, LspError> {
        if let Some(text) = self.touch(path) {
            return Ok(text);
        }
        let text = super::read_text(path).map_err(|e| LspError::Io {
            path: path.to_owned(),
            message: e,
        })?;
        self.sync(path, Some(text.clone()), false);
        Ok(text)
    }

    fn touch(&self, path: &Path) -> Option<String> {
        let mut docs = self.documents.lock().unwrap();
        docs.clock += 1;
        let clock = docs.clock;
        docs.open.get_mut(path).map(|doc| {
            doc.last_used = clock;
            doc.text.clone()
        })
    }

    /// Applies on-disk state to the document: `None` means the file is gone.
    /// `saved` adds a `didSave`, which some servers (rust-analyzer's
    /// check-on-save) need to refresh project-wide diagnostics.
    pub fn sync(&self, path: &Path, content: Option<String>, saved: bool) {
        let Ok(uri) = Url::from_file_path(path) else {
            return;
        };
        let mut docs = self.documents.lock().unwrap();
        let running = self.running();
        let send = |f: &dyn Fn(&Running)| {
            if let Some(running) = &running {
                f(running);
            }
        };

        let Some(text) = content else {
            if docs.open.remove(path).is_some() {
                send(&|r| close(r, &uri));
            }
            return;
        };

        docs.clock += 1;
        let clock = docs.clock;
        match docs.open.get_mut(path) {
            Some(doc) => {
                doc.last_used = clock;
                if doc.text != text {
                    doc.version += 1;
                    doc.text = text;
                    send(&|r| change(r, &uri, doc.version, &doc.text));
                }
            }
            None => {
                send(&|r| open(r, &uri, &self.key.language, 0, &text));
                docs.open.insert(
                    path.to_owned(),
                    Document {
                        version: 0,
                        text,
                        last_used: clock,
                    },
                );
                while docs.open.len() > MAX_OPEN_DOCUMENTS {
                    let oldest = docs
                        .open
                        .iter()
                        .min_by_key(|(_, d)| d.last_used)
                        .map(|(p, _)| p.clone())
                        .expect("non-empty");
                    docs.open.remove(&oldest);
                    if let Ok(uri) = Url::from_file_path(&oldest) {
                        send(&|r| close(r, &uri));
                    }
                }
            }
        }
        if saved {
            let text = &docs.open[path].text;
            send(&|r| save(r, &uri, text));
        }
        self.shared.bump();
    }

    /// Forwards file events matching the server's registered watchers.
    pub fn watched(&self, changes: &[(PathBuf, FileChangeType)]) {
        let Some(running) = self.running() else {
            return;
        };
        let events: Vec<FileEvent> = {
            let watchers = running.instance.watchers.lock().unwrap();
            changes
                .iter()
                .filter(|(path, typ)| watchers.values().flatten().any(|w| w.matches(path, *typ)))
                .filter_map(|(path, typ)| {
                    Some(FileEvent {
                        uri: Url::from_file_path(path).ok()?,
                        typ: *typ,
                    })
                })
                .collect()
        };
        if !events.is_empty() {
            let _ = running
                .socket
                .notify::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams { changes: events });
            self.shared.bump();
        }
    }

    /// Runs the server until shutdown, restarting it after crashes.
    pub async fn supervise(self: Arc<Self>) {
        let mut crashes: VecDeque<Instant> = VecDeque::new();
        loop {
            let reason = match self.clone().run_instance().await {
                Ok(Outcome::Shutdown) => break,
                Ok(Outcome::Crashed(reason)) => reason,
                Err(reason) => {
                    // The command cannot be started; retrying will not help.
                    self.set_phase(Phase::Failed(reason));
                    return;
                }
            };
            self.push_log(format!("[saddle] server stopped: {reason}"));
            let now = Instant::now();
            crashes.push_back(now);
            while crashes
                .front()
                .is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW)
            {
                crashes.pop_front();
            }
            if crashes.len() > MAX_RESTARTS {
                self.set_phase(Phase::Failed(format!(
                    "{reason} (gave up after {MAX_RESTARTS} restarts)"
                )));
                return;
            }
            let attempt = crashes.len() as u32;
            self.set_phase(Phase::Restarting { attempt, reason });
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(u64::from(attempt))) => {}
                _ = self.shutdown.cancelled() => break,
            }
        }
        self.set_phase(Phase::Stopped);
    }

    /// One process lifetime. `Err` means the process could not be spawned.
    async fn run_instance(self: Arc<Self>) -> Result<Outcome, String> {
        let mut child = tokio::process::Command::new(&self.config.command)
            .args(&self.config.args)
            .current_dir(&self.key.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start `{}`: {e}", self.config.command))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let log_server = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    *log_server.last_stderr.lock().unwrap() = Some((Instant::now(), line.clone()));
                }
                log_server.push_log(line);
            }
        });

        let instance = Arc::new(Instance {
            id: self.shared.next_instance.fetch_add(1, Ordering::Relaxed),
            encoding: OnceLock::new(),
            progress: Mutex::default(),
            watchers: Mutex::default(),
        });
        let (mainloop, socket) = MainLoop::new_client(|_| router(self.clone(), instance.clone()));
        let mut main = tokio::spawn(mainloop.run_buffered(stdout.compat(), stdin.compat_write()));

        let exit = self
            .clone()
            .drive(&mut child, &mut main, socket, instance.clone())
            .await;

        self.shared.diagnostics.clear_instance(instance.id);
        main.abort();
        let _ = child.start_kill();
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await;
        Ok(exit)
    }

    async fn drive(
        self: Arc<Self>,
        child: &mut tokio::process::Child,
        main: &mut tokio::task::JoinHandle<async_lsp::Result<()>>,
        socket: ServerSocket,
        instance: Arc<Instance>,
    ) -> Outcome {
        let initialize = socket.request::<Initialize>(self.initialize_params());
        let result = tokio::select! {
            result = tokio::time::timeout(INITIALIZE_TIMEOUT, initialize) => match result {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => return Outcome::Crashed(format!("initialize failed: {e}")),
                Err(_) => return Outcome::Crashed("initialize timed out".into()),
            },
            status = child.wait() => return Outcome::Crashed(self.exit_reason(status)),
            _ = self.shutdown.cancelled() => return Outcome::Shutdown,
        };
        if socket.notify::<Initialized>(InitializedParams {}).is_err() {
            return Outcome::Crashed("connection closed after initialize".into());
        }

        let encoding = PositionEncoding::negotiated(result.capabilities.position_encoding.as_ref());
        let _ = instance.encoding.set(encoding);
        let running = Arc::new(Running {
            socket: socket.clone(),
            encoding,
            sync: SyncCaps::from(result.capabilities.text_document_sync.as_ref()),
            instance,
        });
        if let Some(info) = &result.server_info {
            self.push_log(format!(
                "[saddle] initialized {} {} ({encoding:?})",
                info.name,
                info.version.as_deref().unwrap_or("")
            ));
        }

        {
            // Re-open documents under the same lock that `sync` takes, so none
            // is lost or opened twice.
            let docs = self.documents.lock().unwrap();
            for (path, doc) in &docs.open {
                if let Ok(uri) = Url::from_file_path(path) {
                    open(&running, &uri, &self.key.language, doc.version, &doc.text);
                }
            }
            self.phase.send_replace(Phase::Running(running));
        }
        self.publish_state();

        tokio::select! {
            result = &mut *main => Outcome::Crashed(match result {
                Ok(Ok(())) => "connection closed".into(),
                Ok(Err(e)) => match child.try_wait() {
                    Ok(Some(status)) => self.exit_reason(Ok(status)),
                    _ => format!("connection failed: {e}"),
                },
                Err(e) => format!("client task failed: {e}"),
            }),
            status = child.wait() => Outcome::Crashed(self.exit_reason(status)),
            _ = self.shutdown.cancelled() => {
                let graceful = async {
                    socket.request::<Shutdown>(()).await.ok()?;
                    socket.notify::<Exit>(()).ok()?;
                    child.wait().await.ok()
                };
                let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, graceful).await;
                Outcome::Shutdown
            }
        }
    }

    fn exit_reason(&self, status: std::io::Result<std::process::ExitStatus>) -> String {
        let status = match status {
            Ok(status) => format!("exited with {status}"),
            Err(e) => format!("wait failed: {e}"),
        };
        // Only stderr written just before the exit is likely to explain it.
        let last = self
            .last_stderr
            .lock()
            .unwrap()
            .clone()
            .filter(|(at, _)| at.elapsed() < STDERR_RELEVANCE)
            .map(|(_, line)| line);
        match last {
            Some(line) => format!("{status}: {}", truncate(&line, 200)),
            None => status,
        }
    }

    #[allow(deprecated)] // root_uri: still read by servers that predate workspace folders
    fn initialize_params(&self) -> InitializeParams {
        let root_uri = Url::from_directory_path(&self.key.root).ok();
        InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root_uri.clone(),
            workspace_folders: root_uri.map(|uri| vec![self.workspace_folder(uri)]),
            client_info: Some(ClientInfo {
                name: "saddle".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(PositionEncoding::offered()),
                    ..Default::default()
                }),
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                workspace: Some(WorkspaceClientCapabilities {
                    apply_edit: Some(false),
                    configuration: Some(true),
                    workspace_folders: Some(true),
                    did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration: Some(true),
                        relative_pattern_support: Some(true),
                    }),
                    ..Default::default()
                }),
                text_document: Some(TextDocumentClientCapabilities {
                    synchronization: Some(TextDocumentSyncClientCapabilities {
                        did_save: Some(true),
                        ..Default::default()
                    }),
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                        related_information: Some(true),
                        version_support: Some(true),
                        ..Default::default()
                    }),
                    hover: Some(HoverClientCapabilities {
                        content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                        ..Default::default()
                    }),
                    definition: Some(GotoCapability {
                        link_support: Some(true),
                        ..Default::default()
                    }),
                    document_symbol: Some(DocumentSymbolClientCapabilities {
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn workspace_folder(&self, uri: Url) -> WorkspaceFolder {
        WorkspaceFolder {
            uri,
            name: self
                .key
                .root
                .file_name()
                .map_or_else(|| "root".into(), |n| n.to_string_lossy().into_owned()),
        }
    }
}

fn open(running: &Running, uri: &Url, language: &str, version: i32, text: &str) {
    if running.sync.open_close {
        let _ = running
            .socket
            .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: language.into(),
                    version,
                    text: text.into(),
                },
            });
    }
}

fn change(running: &Running, uri: &Url, version: i32, text: &str) {
    if running.sync.open_close && running.sync.change {
        // A change without a range replaces the whole document, which every
        // server accepts whatever sync kind it asked for.
        let _ = running
            .socket
            .notify::<DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.into(),
                }],
            });
    }
}

fn save(running: &Running, uri: &Url, text: &str) {
    if let Some(include_text) = running.sync.save {
        let _ = running
            .socket
            .notify::<DidSaveTextDocument>(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                text: include_text.then(|| text.into()),
            });
    }
}

fn close(running: &Running, uri: &Url) {
    if running.sync.open_close {
        let _ = running
            .socket
            .notify::<DidCloseTextDocument>(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            });
    }
}

struct RouterState {
    server: Arc<Server>,
    instance: Arc<Instance>,
}

fn router(server: Arc<Server>, instance: Arc<Instance>) -> Router<RouterState> {
    let mut router = Router::new(RouterState { server, instance });
    router
        .notification::<PublishDiagnostics>(|st, params| {
            let encoding = st
                .instance
                .encoding
                .get()
                .copied()
                .unwrap_or(PositionEncoding::Utf16);
            st.server
                .shared
                .diagnostics
                .publish(st.instance.id, encoding, params);
            st.server.shared.bump();
            ControlFlow::Continue(())
        })
        .notification::<Progress>(|st, params| {
            let token = match params.token {
                NumberOrString::Number(n) => n.to_string(),
                NumberOrString::String(s) => s,
            };
            let ProgressParamsValue::WorkDone(progress) = params.value;
            {
                let mut active = st.instance.progress.lock().unwrap();
                match progress {
                    WorkDoneProgress::Begin(begin) => {
                        active.insert(token, begin.title);
                    }
                    WorkDoneProgress::Report(_) => {}
                    WorkDoneProgress::End(_) => {
                        active.remove(&token);
                    }
                }
            }
            if st.server.running().is_some() {
                st.server.publish_state();
            }
            ControlFlow::Continue(())
        })
        .notification::<LogMessage>(|st, params| {
            st.server.push_log(params.message);
            ControlFlow::Continue(())
        })
        .notification::<ShowMessage>(|st, params| {
            st.server
                .push_log(format!("[{:?}] {}", params.typ, params.message));
            ControlFlow::Continue(())
        })
        .unhandled_notification(|_, _| ControlFlow::Continue(()))
        .request::<WorkspaceConfiguration, _>(|_, params| {
            // No settings: servers fall back to their defaults.
            let answer = vec![serde_json::Value::Null; params.items.len()];
            async move { Ok(answer) }
        })
        .request::<WorkDoneProgressCreate, _>(|_, _| async { Ok(()) })
        .request::<RegisterCapability, _>(|st, params| {
            let mut watchers = st.instance.watchers.lock().unwrap();
            for registration in params.registrations {
                if registration.method != DidChangeWatchedFiles::METHOD {
                    continue;
                }
                let Some(options) = registration.register_options.and_then(|o| {
                    serde_json::from_value::<DidChangeWatchedFilesRegistrationOptions>(o).ok()
                }) else {
                    continue;
                };
                let compiled = options
                    .watchers
                    .into_iter()
                    .filter_map(FileWatcher::new)
                    .collect();
                watchers.insert(registration.id, compiled);
            }
            async { Ok(()) }
        })
        .request::<UnregisterCapability, _>(|st, params| {
            let mut watchers = st.instance.watchers.lock().unwrap();
            for unregistration in params.unregisterations {
                watchers.remove(&unregistration.id);
            }
            async { Ok(()) }
        })
        .request::<WorkspaceFoldersRequest, _>(|st, _| {
            let folders = Url::from_directory_path(&st.server.key.root)
                .ok()
                .map(|uri| vec![st.server.workspace_folder(uri)]);
            async move { Ok(folders) }
        })
        .request::<ApplyWorkspaceEdit, _>(|_, _| async {
            Ok(ApplyWorkspaceEditResponse {
                applied: false,
                failure_reason: Some("Saddle never edits files".into()),
                failed_change: None,
            })
        })
        .request::<ShowMessageRequest, _>(|_, _| async { Ok(None) })
        .request::<ShowDocument, _>(|_, _| async { Ok(ShowDocumentResult { success: false }) })
        .unhandled_request(|_, request| {
            // `workspace/*/refresh` requests only ask the client to re-query.
            let refresh = request.method.ends_with("/refresh");
            let method = request.method;
            async move {
                if refresh {
                    Ok(serde_json::Value::Null)
                } else {
                    Err(ResponseError::new(
                        ErrorCode::METHOD_NOT_FOUND,
                        format!("unsupported request {method}"),
                    ))
                }
            }
        });
    router
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::{FileSystemWatcher, RelativePattern, SaveOptions, TextDocumentSyncOptions};

    use super::*;

    fn watcher(pattern: GlobPattern, kind: Option<WatchKind>) -> FileWatcher {
        FileWatcher::new(FileSystemWatcher {
            glob_pattern: pattern,
            kind,
        })
        .unwrap()
    }

    #[test]
    fn file_watchers_match_absolute_and_relative_patterns() {
        let absolute = watcher(GlobPattern::String("/p/**/*.rs".into()), None);
        assert!(absolute.matches(Path::new("/p/src/a.rs"), FileChangeType::CHANGED));
        assert!(absolute.matches(Path::new("/p/a.rs"), FileChangeType::DELETED));
        assert!(!absolute.matches(Path::new("/q/a.rs"), FileChangeType::CHANGED));

        let any_depth = watcher(GlobPattern::String("**/Cargo.{toml,lock}".into()), None);
        assert!(any_depth.matches(Path::new("/p/crates/x/Cargo.toml"), FileChangeType::CREATED));
        assert!(!any_depth.matches(Path::new("/p/Cargo.tomlx"), FileChangeType::CREATED));

        let relative = watcher(
            GlobPattern::Relative(RelativePattern {
                base_uri: OneOf::Right(Url::from_directory_path("/p").unwrap()),
                pattern: "src/*.rs".into(),
            }),
            Some(WatchKind::Create | WatchKind::Delete),
        );
        assert!(relative.matches(Path::new("/p/src/a.rs"), FileChangeType::CREATED));
        assert!(!relative.matches(Path::new("/p/src/a.rs"), FileChangeType::CHANGED));
        assert!(!relative.matches(Path::new("/p/src/deep/a.rs"), FileChangeType::CREATED));
        assert!(!relative.matches(Path::new("/q/src/a.rs"), FileChangeType::CREATED));
    }

    #[test]
    fn sync_capabilities() {
        let none = SyncCaps::from(None);
        assert!(!none.open_close && !none.change && none.save.is_none());

        let full = SyncCaps::from(Some(&TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::FULL,
        )));
        assert!(full.open_close && full.change && full.save.is_none());

        let options = SyncCaps::from(Some(&TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(true),
                })),
                ..Default::default()
            },
        )));
        assert!(options.open_close && options.change);
        assert_eq!(options.save, Some(true));
    }
}
