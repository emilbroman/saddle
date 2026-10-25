//! A generic Language Server Protocol client. Servers are configured per
//! language in `config.toml` and started lazily, one per language and
//! workspace root. Documents are synced from disk only; Saddle never sees
//! unsaved editor buffers.
//!
//! Positions in this API are zero-based, with columns counted in Unicode
//! scalar values; conversion to the server's negotiated encoding happens here.

pub mod changes;
mod diagnostics;
mod encoding;
mod language;
mod server;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use lsp_types::request::{
    DocumentSymbolRequest, GotoDefinition, HoverRequest, References, WorkspaceSymbolRequest,
};
use lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, FileChangeType, GotoDefinitionParams,
    GotoDefinitionResponse, HoverContents, HoverParams, MarkedString, ReferenceContext,
    ReferenceParams, SymbolKind, TextDocumentIdentifier, TextDocumentPositionParams, Url,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub use encoding::PositionEncoding;
pub use language::{Languages, workspace_root};

use crate::config::LspServer;
use crate::events::LspEvent;
use encoding::Lines;
use server::{Server, ServerKey, Shared};

/// Files larger than this are not sent to servers.
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;
/// Upper bound on files examined when detecting which languages a project uses.
const DETECT_FILE_LIMIT: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPosition {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based column in Unicode scalar values.
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextRange {
    pub start: TextPosition,
    pub end: TextPosition,
}

impl TextRange {
    fn from_lsp(lines: &Lines, encoding: PositionEncoding, range: lsp_types::Range) -> Self {
        Self {
            start: lines.decode(encoding, range.start),
            end: lines.decode(encoding, range.end),
        }
    }

    /// Used when the file's content is unavailable.
    fn raw(range: lsp_types::Range) -> Self {
        let pos = |p: lsp_types::Position| TextPosition {
            line: p.line,
            column: p.character,
        };
        Self {
            start: pos(range.start),
            end: pos(range.end),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub range: TextRange,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    pub detail: Option<String>,
    pub container: Option<String>,
    pub location: Location,
    /// Nesting level within a document outline; zero for workspace results.
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Starting,
    Ready,
    /// Reporting work in progress, e.g. indexing.
    Busy(String),
    Restarting {
        attempt: u32,
        reason: String,
    },
    Failed(String),
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub language: String,
    pub root: PathBuf,
    pub state: ServerState,
}

/// Compact status-line text, e.g. `rust ✓` or `rust indexing…`.
pub fn status_summary(statuses: &[ServerStatus]) -> Option<String> {
    let parts: Vec<String> = statuses
        .iter()
        .filter(|s| s.state != ServerState::Stopped)
        .map(|s| {
            let state = match &s.state {
                ServerState::Starting => "starting…".to_string(),
                ServerState::Ready => "✓".to_string(),
                ServerState::Busy(title) => {
                    let title: String = title.chars().take(24).collect();
                    format!("{}…", title.to_lowercase())
                }
                ServerState::Restarting { attempt, .. } => format!("restarting ({attempt})"),
                ServerState::Failed(_) => "✗ failed".to_string(),
                ServerState::Stopped => unreachable!(),
            };
            format!("{} {state}", s.language)
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("no language server is configured for {}", .0.display())]
    NoServer(PathBuf),
    #[error("{} is outside the project", .0.display())]
    OutsideProject(PathBuf),
    #[error("cannot read {}: {message}", path.display())]
    Io { path: PathBuf, message: String },
    #[error("the {language} language server failed: {reason}")]
    Failed { language: String, reason: String },
    #[error("the language server has stopped")]
    Stopped,
    #[error("the {language} language server did not answer {what} in time")]
    Timeout { language: String, what: String },
    #[error("{method} failed on the {language} language server: {message}")]
    Request {
        language: String,
        method: String,
        message: String,
    },
}

/// Handle to all language servers of one project. Cheap to clone.
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Inner>,
}

struct Inner {
    project_root: PathBuf,
    languages: Languages,
    shared: Arc<Shared>,
    servers: Mutex<BTreeMap<ServerKey, Arc<Server>>>,
    supervisors: Mutex<Vec<JoinHandle<()>>>,
    changes: mpsc::UnboundedSender<Vec<PathBuf>>,
}

impl LspManager {
    /// Must be called within a Tokio runtime. Status changes are sent to
    /// `events` when given.
    pub fn new(
        project_root: PathBuf,
        servers: &BTreeMap<String, LspServer>,
        events: Option<mpsc::UnboundedSender<LspEvent>>,
    ) -> Self {
        let (changes, mut changes_rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
        let inner = Arc::new(Inner {
            project_root,
            languages: Languages::new(servers),
            shared: Arc::new(Shared::new(events)),
            servers: Mutex::default(),
            supervisors: Mutex::default(),
            changes,
        });

        // Applies change batches in order, without blocking callers.
        let weak: Weak<Inner> = Arc::downgrade(&inner);
        tokio::spawn(async move {
            while let Some(paths) = changes_rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                let manager = LspManager { inner };
                manager.apply_changes(&paths);
                manager.inner.shared.pending.fetch_sub(1, Ordering::SeqCst);
                manager.inner.shared.bump();
            }
        });

        Self { inner }
    }

    pub fn project_root(&self) -> &Path {
        &self.inner.project_root
    }

    pub fn has_servers_configured(&self) -> bool {
        !self.inner.languages.is_empty()
    }

    /// Starts a server for each configured language that has files in the
    /// project (gitignore-aware), in the background.
    pub fn start_detected(&self) {
        if self.inner.languages.is_empty() {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            let inner = manager.inner.clone();
            let found =
                tokio::task::spawn_blocking(move || detect(&inner.project_root, &inner.languages))
                    .await
                    .unwrap_or_default();
            for (language, file) in found {
                manager.server(&language, &file);
            }
        });
    }

    /// Tells servers that these files changed on disk (created, modified or
    /// deleted). Paths are absolute; content is re-read from disk. Returns
    /// immediately; changes are applied in call order.
    pub fn did_change(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.inner.languages.is_empty() {
            return;
        }
        self.inner.shared.pending.fetch_add(1, Ordering::SeqCst);
        self.inner.shared.bump();
        if self.inner.changes.send(paths).is_err() {
            self.inner.shared.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn apply_changes(&self, paths: &[PathBuf]) {
        let root = &self.inner.project_root;
        let mut events: Vec<(PathBuf, FileChangeType)> = Vec::new();
        for path in paths {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            if !path.starts_with(root) {
                continue;
            }
            let exists = path.exists();
            events.push((
                path.clone(),
                if exists {
                    FileChangeType::CHANGED
                } else {
                    FileChangeType::DELETED
                },
            ));
            let Some(language) = self.inner.languages.for_path(&path) else {
                continue;
            };
            if !exists {
                // Only servers that already know the file care about its removal.
                for server in self.servers() {
                    server.sync(&path, None, false);
                }
                continue;
            }
            if let Ok(text) = read_text(&path)
                && let Some(server) = self.server(language, &path)
            {
                server.sync(&path, Some(text), true);
            }
        }
        for server in self.servers() {
            let relevant: Vec<_> = events
                .iter()
                .filter(|(path, _)| path.starts_with(&server.key.root))
                .cloned()
                .collect();
            server.watched(&relevant);
        }
    }

    fn servers(&self) -> Vec<Arc<Server>> {
        self.inner
            .servers
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    /// The server responsible for `file`, started if needed.
    fn server(&self, language: &str, file: &Path) -> Option<Arc<Server>> {
        let config = self.inner.languages.server(language)?;
        let key = ServerKey {
            language: language.to_owned(),
            root: workspace_root(&self.inner.project_root, file, &config.root_markers),
        };
        let mut servers = self.inner.servers.lock().unwrap();
        if let Some(server) = servers.get(&key) {
            return Some(server.clone());
        }
        let server = Server::new(key.clone(), config.clone(), self.inner.shared.clone());
        let supervisor = tokio::spawn(server.clone().supervise());
        self.inner.supervisors.lock().unwrap().push(supervisor);
        servers.insert(key, server.clone());
        Some(server)
    }

    fn server_for(&self, path: &Path) -> Result<Arc<Server>, LspError> {
        let language = self
            .inner
            .languages
            .for_path(path)
            .ok_or_else(|| LspError::NoServer(path.to_owned()))?;
        self.server(language, path)
            .ok_or_else(|| LspError::NoServer(path.to_owned()))
    }

    /// Resolves a query path (absolute or relative to the project root) to a
    /// canonical path inside the project.
    fn resolve(&self, path: &Path) -> Result<PathBuf, LspError> {
        let root = &self.inner.project_root;
        let joined = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        let canonical = joined.canonicalize().map_err(|e| LspError::Io {
            path: joined.clone(),
            message: e.to_string(),
        })?;
        if !canonical.starts_with(root) {
            return Err(LspError::OutsideProject(path.to_owned()));
        }
        Ok(canonical)
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        self.inner.shared.statuses()
    }

    /// Recent stderr, log and status lines of a language's servers.
    pub fn log(&self, language: &str) -> Vec<String> {
        self.servers()
            .into_iter()
            .filter(|s| s.key.language == language)
            .flat_map(|s| s.log())
            .collect()
    }

    /// Increments on anything that may affect diagnostics: publications,
    /// progress, server state changes and document sync.
    pub fn activity(&self) -> watch::Receiver<u64> {
        self.inner.shared.activity()
    }

    /// Waits until no server is starting or reporting progress, no changes
    /// are pending, and nothing happened for `quiet`. Gives up after `cap`;
    /// returns whether diagnostics settled.
    pub async fn settle(&self, quiet: Duration, cap: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + cap;
        let mut activity = self.activity();
        loop {
            activity.borrow_and_update();
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let busy = self.inner.shared.busy();
            let settled_at = now + quiet;
            let until = if busy {
                deadline
            } else {
                settled_at.min(deadline)
            };
            match tokio::time::timeout_at(until, activity.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return false,
                Err(_) => return !busy && settled_at <= deadline,
            }
        }
    }

    /// Current diagnostics of every file that has any.
    pub fn diagnostics(&self) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        self.inner
            .shared
            .diagnostics
            .snapshot(|_| true, |p| self.text_of(p))
    }

    /// Current diagnostics of the given absolute paths.
    pub fn diagnostics_for(&self, paths: &[PathBuf]) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        self.inner
            .shared
            .diagnostics
            .snapshot(|p| paths.iter().any(|q| q == p), |p| self.text_of(p))
    }

    fn text_of(&self, path: &Path) -> Option<String> {
        self.servers()
            .iter()
            .find_map(|s| s.document_text(path))
            .or_else(|| read_text(path).ok())
    }

    /// Opens or refreshes a document from disk before reading diagnostics.
    pub async fn sync_document(&self, path: &Path) -> Result<(), LspError> {
        let path = self.resolve(path)?;
        let server = self.server_for(&path)?;
        server.ensure_open(&path)?;
        server.ready().await?;
        Ok(())
    }

    /// Opens the document and converts `pos` to the server's encoding.
    async fn position_params(
        &self,
        path: &Path,
        pos: TextPosition,
    ) -> Result<(Arc<Server>, TextDocumentPositionParams, PositionEncoding), LspError> {
        let path = self.resolve(path)?;
        let server = self.server_for(&path)?;
        let text = server.ensure_open(&path)?;
        let encoding = server.ready().await?.encoding;
        let params = TextDocumentPositionParams {
            text_document: identifier(&path),
            position: Lines::new(&text).encode(encoding, pos),
        };
        Ok((server, params, encoding))
    }

    pub async fn definition(
        &self,
        path: &Path,
        pos: TextPosition,
    ) -> Result<Vec<Location>, LspError> {
        let (server, params, encoding) = self.position_params(path, pos).await?;
        let response = server
            .request::<GotoDefinition>(GotoDefinitionParams {
                text_document_position_params: params,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?;
        let raw: Vec<(Url, lsp_types::Range)> = match response {
            None => vec![],
            Some(GotoDefinitionResponse::Scalar(l)) => vec![(l.uri, l.range)],
            Some(GotoDefinitionResponse::Array(ls)) => {
                ls.into_iter().map(|l| (l.uri, l.range)).collect()
            }
            Some(GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|l| (l.target_uri, l.target_selection_range))
                .collect(),
        };
        Ok(self.locations(encoding, raw))
    }

    pub async fn references(
        &self,
        path: &Path,
        pos: TextPosition,
        include_declaration: bool,
    ) -> Result<Vec<Location>, LspError> {
        let (server, params, encoding) = self.position_params(path, pos).await?;
        let response = server
            .request::<References>(ReferenceParams {
                text_document_position: params,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: ReferenceContext {
                    include_declaration,
                },
            })
            .await?;
        let raw = response
            .unwrap_or_default()
            .into_iter()
            .map(|l| (l.uri, l.range))
            .collect();
        Ok(self.locations(encoding, raw))
    }

    /// Hover text (usually Markdown), if the server has any.
    pub async fn hover(&self, path: &Path, pos: TextPosition) -> Result<Option<String>, LspError> {
        let (server, params, _) = self.position_params(path, pos).await?;
        let response = server
            .request::<HoverRequest>(HoverParams {
                text_document_position_params: params,
                work_done_progress_params: Default::default(),
            })
            .await?;
        Ok(response
            .map(|hover| hover_text(hover.contents))
            .filter(|text| !text.trim().is_empty()))
    }

    /// The document's outline, flattened in order with nesting depth.
    pub async fn document_symbols(&self, path: &Path) -> Result<Vec<Symbol>, LspError> {
        let path = self.resolve(path)?;
        let server = self.server_for(&path)?;
        server.ensure_open(&path)?;
        let encoding = server.ready().await?.encoding;
        let response = server
            .request::<DocumentSymbolRequest>(DocumentSymbolParams {
                text_document: identifier(&path),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?;
        let text = self.text_of(&path).unwrap_or_default();
        let lines = Lines::new(&text);
        let mut symbols = Vec::new();
        match response {
            None => {}
            Some(DocumentSymbolResponse::Flat(flat)) => {
                for info in flat {
                    symbols.push(Symbol {
                        name: info.name,
                        kind: symbol_kind(info.kind),
                        detail: None,
                        container: info.container_name,
                        location: Location {
                            path: path.clone(),
                            range: TextRange::from_lsp(&lines, encoding, info.location.range),
                        },
                        depth: 0,
                    });
                }
            }
            Some(DocumentSymbolResponse::Nested(nested)) => {
                flatten(&path, &lines, encoding, nested, None, 0, &mut symbols)
            }
        }
        Ok(symbols)
    }

    /// Workspace-wide symbol search across every running server; servers for
    /// the project's languages are started first if none is.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<Symbol>, LspError> {
        if self.servers().is_empty() {
            let inner = self.inner.clone();
            let found =
                tokio::task::spawn_blocking(move || detect(&inner.project_root, &inner.languages))
                    .await
                    .unwrap_or_default();
            for (language, file) in found {
                self.server(&language, &file);
            }
        }
        let servers = self.servers();
        let mut symbols = Vec::new();
        let mut first_error = None;
        let mut answered = false;
        for server in servers {
            let encoding = match server.ready().await {
                Ok(running) => running.encoding,
                Err(e) => {
                    first_error.get_or_insert(e);
                    continue;
                }
            };
            let response = server
                .request::<WorkspaceSymbolRequest>(WorkspaceSymbolParams {
                    query: query.to_owned(),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                })
                .await;
            let raw: Vec<RawSymbol> = match response {
                Ok(None) => vec![],
                Ok(Some(WorkspaceSymbolResponse::Flat(flat))) => flat
                    .into_iter()
                    .map(|s| {
                        (
                            s.name,
                            s.kind,
                            s.container_name,
                            s.location.uri,
                            Some(s.location.range),
                        )
                    })
                    .collect(),
                Ok(Some(WorkspaceSymbolResponse::Nested(nested))) => nested
                    .into_iter()
                    .map(|s| match s.location {
                        lsp_types::OneOf::Left(l) => {
                            (s.name, s.kind, s.container_name, l.uri, Some(l.range))
                        }
                        lsp_types::OneOf::Right(l) => {
                            (s.name, s.kind, s.container_name, l.uri, None)
                        }
                    })
                    .collect(),
                Err(e) => {
                    first_error.get_or_insert(e);
                    continue;
                }
            };
            answered = true;
            let mut texts = TextCache::new(self);
            for (name, kind, container, uri, range) in raw {
                let Ok(path) = uri.to_file_path() else {
                    continue;
                };
                let range = range.unwrap_or_default();
                symbols.push(Symbol {
                    name,
                    kind: symbol_kind(kind),
                    detail: None,
                    container,
                    location: Location {
                        range: texts.convert(&path, encoding, range),
                        path,
                    },
                    depth: 0,
                });
            }
        }
        match first_error {
            Some(e) if !answered => Err(e),
            _ => Ok(symbols),
        }
    }

    fn locations(
        &self,
        encoding: PositionEncoding,
        raw: Vec<(Url, lsp_types::Range)>,
    ) -> Vec<Location> {
        let mut texts = TextCache::new(self);
        raw.into_iter()
            .filter_map(|(uri, range)| {
                let path = uri.to_file_path().ok()?;
                Some(Location {
                    range: texts.convert(&path, encoding, range),
                    path,
                })
            })
            .collect()
    }

    /// Shuts every server down gracefully, killing those that do not exit in time.
    pub async fn shutdown(&self) {
        for server in self.servers() {
            server.shutdown();
        }
        let supervisors: Vec<_> = self.inner.supervisors.lock().unwrap().drain(..).collect();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            futures::future::join_all(supervisors),
        )
        .await;
    }
}

/// Name, kind, container, file and (unless the server defers it) range.
type RawSymbol = (
    String,
    SymbolKind,
    Option<String>,
    Url,
    Option<lsp_types::Range>,
);

/// File contents for converting positions, read at most once per path.
struct TextCache<'a> {
    manager: &'a LspManager,
    texts: HashMap<PathBuf, Option<String>>,
}

impl<'a> TextCache<'a> {
    fn new(manager: &'a LspManager) -> Self {
        Self {
            manager,
            texts: HashMap::new(),
        }
    }

    fn convert(
        &mut self,
        path: &Path,
        encoding: PositionEncoding,
        range: lsp_types::Range,
    ) -> TextRange {
        let text = self
            .texts
            .entry(path.to_owned())
            .or_insert_with(|| self.manager.text_of(path));
        match text {
            Some(text) => TextRange::from_lsp(&Lines::new(text), encoding, range),
            None => TextRange::raw(range),
        }
    }
}

fn identifier(path: &Path) -> TextDocumentIdentifier {
    TextDocumentIdentifier {
        uri: Url::from_file_path(path).expect("absolute path"),
    }
}

pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(format!("larger than {MAX_DOCUMENT_BYTES} bytes"));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|_| "not UTF-8 text".into())
}

/// The first file found for each configured language.
fn detect(root: &Path, languages: &Languages) -> Vec<(String, PathBuf)> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    let total = languages.names().count();
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten().take(DETECT_FILE_LIMIT) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Some(language) = languages.for_path(entry.path())
            && !found.contains_key(language)
        {
            found.insert(language.to_owned(), entry.into_path());
            if found.len() == total {
                break;
            }
        }
    }
    found.into_iter().collect()
}

fn flatten(
    path: &Path,
    lines: &Lines,
    encoding: PositionEncoding,
    nested: Vec<lsp_types::DocumentSymbol>,
    container: Option<&str>,
    depth: usize,
    out: &mut Vec<Symbol>,
) {
    for symbol in nested {
        out.push(Symbol {
            name: symbol.name.clone(),
            kind: symbol_kind(symbol.kind),
            detail: symbol.detail,
            container: container.map(str::to_owned),
            location: Location {
                path: path.to_owned(),
                range: TextRange::from_lsp(lines, encoding, symbol.selection_range),
            },
            depth,
        });
        if let Some(children) = symbol.children {
            flatten(
                path,
                lines,
                encoding,
                children,
                Some(&symbol.name),
                depth + 1,
                out,
            );
        }
    }
}

fn hover_text(contents: HoverContents) -> String {
    fn marked(s: MarkedString) -> String {
        match s {
            MarkedString::String(s) => s,
            MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
        }
    }
    match contents {
        HoverContents::Scalar(s) => marked(s),
        HoverContents::Array(items) => items
            .into_iter()
            .map(marked)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

fn symbol_kind(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type parameter",
        _ => "symbol",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(language: &str, state: ServerState) -> ServerStatus {
        ServerStatus {
            language: language.into(),
            root: "/p".into(),
            state,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn settle_resets_quiet_on_publication_and_sync() {
        let manager = LspManager::new(PathBuf::from("/p"), &BTreeMap::new(), None);
        let task = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .settle(Duration::from_secs(1), Duration::from_secs(5))
                    .await
            }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(900)).await;
        manager.inner.shared.bump();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(900)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(task.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn settle_caps_busy_and_continuous_activity() {
        let manager = LspManager::new(PathBuf::from("/p"), &BTreeMap::new(), None);
        for busy in [false, true] {
            manager
                .inner
                .shared
                .pending
                .store(usize::from(busy), Ordering::SeqCst);
            let task = tokio::spawn({
                let manager = manager.clone();
                async move {
                    manager
                        .settle(Duration::from_secs(1), Duration::from_secs(5))
                        .await
                }
            });
            tokio::task::yield_now().await;
            for _ in 0..10 {
                tokio::time::advance(Duration::from_millis(500)).await;
                manager.inner.shared.bump();
                tokio::task::yield_now().await;
            }
            assert!(!task.await.unwrap());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn settle_waits_for_pending_sync_to_finish() {
        let manager = LspManager::new(PathBuf::from("/p"), &BTreeMap::new(), None);
        manager.inner.shared.pending.store(1, Ordering::SeqCst);
        let task = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .settle(Duration::from_secs(1), Duration::from_secs(5))
                    .await
            }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!task.is_finished());
        manager.inner.shared.pending.store(0, Ordering::SeqCst);
        manager.inner.shared.bump();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(task.await.unwrap());
    }

    #[test]
    fn summary() {
        assert_eq!(status_summary(&[]), None);
        assert_eq!(
            status_summary(&[status("rust", ServerState::Stopped)]),
            None
        );
        assert_eq!(
            status_summary(&[
                status("rust", ServerState::Ready),
                status("toml", ServerState::Busy("Indexing".into())),
                status(
                    "go",
                    ServerState::Restarting {
                        attempt: 2,
                        reason: "x".into()
                    }
                ),
                status("py", ServerState::Failed("x".into())),
                status("ts", ServerState::Starting),
            ])
            .as_deref(),
            Some("rust ✓, toml indexing…, go restarting (2), py ✗ failed, ts starting…")
        );
    }

    #[test]
    fn hover_markup_variants() {
        use lsp_types::{LanguageString, MarkupContent, MarkupKind};
        assert_eq!(
            hover_text(HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "**x**".into()
            })),
            "**x**"
        );
        assert_eq!(
            hover_text(HoverContents::Array(vec![
                MarkedString::LanguageString(LanguageString {
                    language: "rust".into(),
                    value: "fn f()".into()
                }),
                MarkedString::String("doc".into()),
            ])),
            "```rust\nfn f()\n```\n\ndoc"
        );
    }

    #[test]
    fn detect_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::write(root.join("gen/x.py"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();

        let mut map = BTreeMap::new();
        for (name, ext) in [("rust", "rs"), ("python", "py")] {
            map.insert(
                name.to_string(),
                LspServer {
                    command: "x".into(),
                    args: vec![],
                    file_types: vec![ext.into()],
                    root_markers: vec![],
                },
            );
        }
        let found = detect(root, &Languages::new(&map));
        assert_eq!(found, vec![("rust".to_string(), root.join("src/main.rs"))]);
    }

    #[test]
    fn read_text_rejects_binary_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(dir.path().join("ok.rs"), "fn main() {}").unwrap();
        assert!(read_text(&dir.path().join("bin")).is_err());
        assert!(read_text(dir.path()).is_err());
        assert_eq!(
            read_text(&dir.path().join("ok.rs")).unwrap(),
            "fn main() {}"
        );
    }
}
