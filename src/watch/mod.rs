//! Watches the project for saved changes and turns bursts of them into
//! batched change sets: the agent's view of the user's edits.
//!
//! OS events (via `notify` and its full debouncer) only mark paths as
//! invalid. Saddle's own [`batch::Batcher`] decides when a burst is over, and
//! [`state::Observed`] re-reads the disk to establish what actually changed.

pub mod batch;
pub mod filter;
pub mod message;
pub mod state;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, NoCache, new_debouncer_opt};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::events::FsEvent;
use batch::{Batch, Batcher};
use filter::Filter;
pub use state::Observed;

/// The debouncer only merges duplicate OS events; batching policy is ours.
const DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detail {
    /// Unified diff against the content Saddle last saw.
    Diff {
        diff: String,
        added: usize,
        removed: usize,
    },
    /// A text file whose previous content Saddle never saw.
    Unseen {
        size: u64,
    },
    Binary {
        size: u64,
    },
    TooLarge {
        size: u64,
    },
    Symlink,
    /// Deleted.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Absolute and canonical-rooted.
    pub path: PathBuf,
    /// Relative to the project root, `/`-separated.
    pub display: String,
    pub kind: ChangeKind,
    pub detail: Detail,
}

/// One flushed batch; never empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub files: Vec<FileChange>,
    pub git: Vec<crate::git::GitEvent>,
    /// Includes HEAD-folded changes, which still need document synchronization.
    pub sync_paths: Vec<PathBuf>,
    /// Bounded diagnostic changes, filled after language servers settle.
    pub diagnostics: Option<String>,
}

impl ChangeSet {
    pub fn changed_paths(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .map(|f| f.path.clone())
            .chain(self.sync_paths.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub quiet: Duration,
    pub max_window: Duration,
    /// Directories whose contents are never reported (the sandbox's writable
    /// carve-outs, such as `target/`).
    pub excluded: Vec<PathBuf>,
}

#[derive(Clone)]
pub struct GitTracking {
    pub tracker: crate::git::Tracker,
    pub session: Option<crate::session::Session>,
}

impl GitTracking {
    pub fn classify(
        &self,
        files: &mut Vec<FileChange>,
        window: Option<(std::time::SystemTime, std::time::SystemTime)>,
        first: bool,
    ) -> anyhow::Result<Option<crate::git::GitEvent>> {
        let generation = self.session.as_ref().map(|s| s.generation());
        let event = self.tracker.classify(files, window, first)?;
        if let (Some(session), Some(generation)) = (&self.session, generation) {
            session.save_git(self.tracker.state(), &generation)?;
        }
        Ok(event)
    }
}

/// The running watcher. Change sets go to the app as [`FsEvent::Changed`];
/// other consumers (the LSP client) can [`subscribe`](Self::subscribe).
pub struct Watcher {
    changes: broadcast::Sender<Arc<ChangeSet>>,
    task: JoinHandle<()>,
}

impl Watcher {
    /// `root` must be canonical.
    pub fn spawn(
        root: PathBuf,
        config: WatchConfig,
        observed: Observed,
        events: mpsc::UnboundedSender<FsEvent>,
    ) -> Self {
        Self::spawn_inner(root, config, observed, events, None)
    }

    pub fn spawn_with_git(
        root: PathBuf,
        config: WatchConfig,
        observed: Observed,
        events: mpsc::UnboundedSender<FsEvent>,
        git: GitTracking,
    ) -> Self {
        Self::spawn_inner(root, config, observed, events, Some(git))
    }

    fn spawn_inner(
        root: PathBuf,
        config: WatchConfig,
        observed: Observed,
        events: mpsc::UnboundedSender<FsEvent>,
        git: Option<GitTracking>,
    ) -> Self {
        let (changes, _) = broadcast::channel(64);
        let task = tokio::spawn(run(root, config, observed, events, changes.clone(), git));
        Self { changes, task }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ChangeSet>> {
        self.changes.subscribe()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Task {
    debouncer: Debouncer<RecommendedWatcher, NoCache>,
    /// Watch each reportable directory on its own rather than the whole tree,
    /// so ignored trees such as `target/` and `node_modules/` cost no inotify
    /// watches and send no events. FSEvents (macOS) is recursive by nature.
    per_directory: bool,
    filter: Option<Filter>,
    batcher: Batcher,
    events: mpsc::UnboundedSender<FsEvent>,
    warned: HashSet<String>,
    git: Option<GitTracking>,
    git_dirs: Vec<PathBuf>,
    git_watches: HashSet<PathBuf>,
}

async fn run(
    root: PathBuf,
    config: WatchConfig,
    observed: Observed,
    events: mpsc::UnboundedSender<FsEvent>,
    changes: broadcast::Sender<Arc<ChangeSet>>,
    git: Option<GitTracking>,
) {
    let (raw_tx, mut raw) = mpsc::unbounded_channel::<DebounceEventResult>();
    let debouncer = new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
        DEBOUNCE.min(config.quiet.max(Duration::from_millis(1))),
        None,
        move |result| {
            let _ = raw_tx.send(result);
        },
        NoCache,
        notify::Config::default(),
    );
    let debouncer = match debouncer {
        Ok(debouncer) => debouncer,
        Err(e) => {
            let _ = events.send(FsEvent::Warning(format!(
                "File watching is unavailable: {e}"
            )));
            return;
        }
    };
    let mut task = Task {
        debouncer,
        per_directory: cfg!(target_os = "linux"),
        filter: Some(Filter::new(&root, config.excluded)),
        batcher: Batcher::new(config.quiet, config.max_window),
        events,
        warned: HashSet::new(),
        git_dirs: if git.is_some() {
            crate::git::metadata_dirs(&root)
        } else {
            Vec::new()
        },
        git_watches: HashSet::new(),
        git,
    };

    if task.per_directory {
        task.watch_tree(&root);
    } else if let Err(e) = task.debouncer.watch(&root, RecursiveMode::Recursive) {
        task.warn(format!("File watching is unavailable: {e}"));
        return;
    }

    task.watch_git();

    // Seeding walks the whole project; events meanwhile wait in the channel.
    let filter = task.filter.take().unwrap();
    let seeding = observed.clone();
    task.filter = Some(
        tokio::task::spawn_blocking(move || {
            seeding.seed(&filter);
            filter
        })
        .await
        .expect("seeding the file record panicked"),
    );

    if task.git.is_some() {
        task.batcher.add_rescan(Instant::now());
    }

    loop {
        let deadline = task.batcher.deadline();
        tokio::select! {
            result = raw.recv() => match result {
                Some(Ok(batch)) => {
                    let now = Instant::now();
                    for event in batch {
                        task.ingest(&event, now, event.time);
                    }
                }
                Some(Err(errors)) => {
                    for error in errors {
                        task.warn(format!("File watcher error: {error}"));
                    }
                }
                None => return,
            },
            _ = sleep_until(deadline), if deadline.is_some() => {
                if let Some(batch) = task.batcher.take_due(Instant::now()) {
                    let set = task.compare(&observed, batch).await;
                    if !set.files.is_empty() || !set.git.is_empty() {
                        let set = Arc::new(set);
                        let _ = changes.send(set.clone());
                        let _ = task.events.send(FsEvent::Changed(set));
                    }
                }
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

impl Task {
    fn filter(&mut self) -> &mut Filter {
        self.filter
            .as_mut()
            .expect("filter is only lent out while comparing")
    }

    fn ingest(&mut self, event: &notify::Event, now: Instant, occurred: Instant) {
        if event.need_rescan() {
            self.batcher.add_rescan(now);
            self.batcher.note_occurred(occurred);
        }
        // Reads (including the agent's own) open files; only writes matter.
        if let EventKind::Access(kind) = event.kind
            && kind != AccessKind::Close(AccessMode::Write)
        {
            return;
        }
        for path in &event.paths {
            if self.is_git_metadata(path) {
                self.batcher.add_git(path.clone(), now);
                self.batcher.note_occurred(occurred);
                self.watch_git();
                continue;
            }
            let rules_changed = self.filter().invalidate(path);
            let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
            if self.filter().is_ignored(path, is_dir) {
                continue;
            }
            if self.per_directory && (is_dir || rules_changed) {
                // New directories need watches of their own; changed ignore
                // rules may have un-ignored some.
                let dir = if is_dir {
                    path.clone()
                } else {
                    self.filter().root().to_owned()
                };
                self.watch_tree(&dir);
            }
            self.batcher.add(path.clone(), now);
            self.batcher.note_occurred(occurred);
        }
    }

    fn watch_tree(&mut self, dir: &Path) {
        let dirs: Vec<_> = self
            .filter()
            .walk(dir)
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
            .map(|e| e.into_path())
            .collect();
        for dir in dirs {
            if let Err(e) = self.debouncer.watch(&dir, RecursiveMode::NonRecursive)
                && dir.exists()
            {
                let reason = match e.kind {
                    notify::ErrorKind::MaxFilesWatch => {
                        "the OS watch limit is reached (fs.inotify.max_user_watches)".into()
                    }
                    notify::ErrorKind::Io(io) => io.to_string(),
                    other => format!("{other:?}"),
                };
                self.warn(format!(
                    "Some directories cannot be watched, so changes there go unnoticed: {reason}"
                ));
            }
        }
    }

    fn is_git_metadata(&self, path: &Path) -> bool {
        self.git_dirs.iter().any(|dir| {
            let Ok(relative) = path.strip_prefix(dir) else {
                return false;
            };
            if path.extension().is_some_and(|e| e == "lock") {
                return false;
            }
            relative.as_os_str().is_empty()
                || matches!(
                    relative.to_str(),
                    Some("HEAD" | "index" | "ORIG_HEAD" | "packed-refs")
                )
                || relative.starts_with("refs")
        })
    }

    fn watch_git(&mut self) {
        for dir in self.git_dirs.clone() {
            for (path, mode) in [
                (dir.clone(), RecursiveMode::NonRecursive),
                (dir.join("refs"), RecursiveMode::Recursive),
            ] {
                if path.is_dir() && !self.git_watches.contains(&path) {
                    match self.debouncer.watch(&path, mode) {
                        Ok(()) => {
                            self.git_watches.insert(path);
                        }
                        Err(e) => self.warn(format!(
                            "Git metadata watching is unavailable for {}: {e}",
                            path.display()
                        )),
                    }
                }
            }
        }
    }

    async fn compare(&mut self, observed: &Observed, mut batch: Batch) -> ChangeSet {
        let mut filter = self.filter.take().unwrap();
        let observed = observed.clone();
        let git = self.git.clone();
        let movement = !batch.git_paths.is_empty() || batch.rescan;
        // A metadata event may arrive without individual checkout paths (or before
        // the debouncer releases them). The snapshot is the authority either way.
        if movement {
            batch.rescan = true;
        }
        let (filter, set, error) = tokio::task::spawn_blocking(move || {
            let window = batch.window;
            let mut files = observed.compare(&mut filter, batch);
            let sync_paths = files.iter().map(|f| f.path.clone()).collect();
            let mut events = Vec::new();
            let mut error = None;
            if movement && let Some(git) = git {
                match git.classify(&mut files, window, false) {
                    Ok(Some(event)) => events.push(event),
                    Ok(None) => {}
                    Err(e) => error = Some(format!("Cannot classify git movement: {e:#}")),
                }
            }
            (
                filter,
                ChangeSet {
                    files,
                    git: events,
                    sync_paths,
                    diagnostics: None,
                },
                error,
            )
        })
        .await
        .expect("comparing changes panicked");
        self.filter = Some(filter);
        if let Some(error) = error {
            self.warn(error);
        }
        set
    }

    /// Reports a problem once; watcher errors tend to repeat.
    fn warn(&mut self, message: String) {
        if self.warned.insert(message.clone()) {
            let _ = self.events.send(FsEvent::Warning(message));
        }
    }
}
