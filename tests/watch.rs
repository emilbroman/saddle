//! The file watcher against the real filesystem (inotify on Linux, FSEvents on macOS).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use saddle::events::FsEvent;
use saddle::watch::{ChangeKind, ChangeSet, Detail, Observed, WatchConfig, Watcher};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

const QUIET: Duration = Duration::from_millis(200);

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    observed: Observed,
    events: mpsc::UnboundedReceiver<FsEvent>,
    watcher: Watcher,
}

async fn fixture(max_window: Duration) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let observed = Observed::default();
    let (tx, events) = mpsc::unbounded_channel();
    let watcher = Watcher::spawn(
        root.clone(),
        WatchConfig {
            quiet: QUIET,
            max_window,
            excluded: vec![root.join("target")],
        },
        observed.clone(),
        tx,
    );
    // Let the watches and the initial record settle.
    tokio::time::sleep(Duration::from_millis(300)).await;
    Fixture {
        _dir: dir,
        root,
        observed,
        events,
        watcher,
    }
}

impl Fixture {
    fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    async fn next(&mut self) -> Arc<ChangeSet> {
        match timeout(Duration::from_secs(5), self.events.recv()).await {
            Ok(Some(FsEvent::Changed(set))) => set,
            other => panic!("expected a change set, got {other:?}"),
        }
    }

    async fn assert_quiet(&mut self, period: Duration) {
        if let Ok(event) = timeout(period, self.events.recv()).await {
            panic!("expected no event, got {event:?}");
        }
    }
}

fn summary(set: &ChangeSet) -> Vec<(ChangeKind, &str)> {
    set.files
        .iter()
        .map(|f| (f.kind, f.display.as_str()))
        .collect()
}

fn record_read(observed: &Observed, path: &Path) {
    let bytes = fs::read(path).unwrap();
    observed.record_read(path, &fs::metadata(path).unwrap(), &bytes);
}

#[tokio::test]
async fn a_burst_of_saves_is_one_batch_without_ignored_paths() {
    let mut f = fixture(Duration::from_secs(30)).await;
    record_read(&f.observed, &f.root.join("src/main.rs"));
    let started = Instant::now();
    f.write("src/main.rs", "fn main() {\n    run();\n}\n");
    f.write("src/lib.rs", "pub fn run() {}\n");
    f.write("target/debug/saddle", "build output");
    f.write("debug.log", "ignored");
    f.write(".git/index", "git internals");
    tokio::time::sleep(Duration::from_millis(100)).await;
    f.write("src/lib.rs", "pub fn run() {}\npub fn stop() {}\n");

    let set = f.next().await;
    assert!(started.elapsed() >= QUIET);
    assert_eq!(
        summary(&set),
        [
            (ChangeKind::Created, "src/lib.rs"),
            (ChangeKind::Modified, "src/main.rs")
        ]
    );
    assert!(matches!(
        &set.files[1].detail,
        Detail::Diff { added: 3, removed: 1, diff } if diff.contains("+    run();")
    ));
    f.assert_quiet(QUIET * 3).await;

    // Reading a file (as the agent does) is not a change.
    let _ = fs::read(f.root.join("src/lib.rs")).unwrap();
    f.assert_quiet(QUIET * 3).await;
}

#[tokio::test]
async fn files_in_new_directories_are_seen() {
    let mut f = fixture(Duration::from_secs(30)).await;
    fs::create_dir_all(f.root.join("src/net")).unwrap();
    f.write("src/net/mod.rs", "mod tcp;\n");
    let set = f.next().await;
    assert_eq!(summary(&set), [(ChangeKind::Created, "src/net/mod.rs")]);

    // The new directory is watched from now on.
    f.write("src/net/tcp.rs", "\n");
    let set = f.next().await;
    assert_eq!(summary(&set), [(ChangeKind::Created, "src/net/tcp.rs")]);

    fs::remove_dir_all(f.root.join("src/net")).unwrap();
    let set = f.next().await;
    assert_eq!(
        summary(&set),
        [
            (ChangeKind::Deleted, "src/net/mod.rs"),
            (ChangeKind::Deleted, "src/net/tcp.rs")
        ]
    );
}

#[tokio::test]
async fn continuous_saves_flush_at_the_window_cap() {
    let max_window = Duration::from_millis(1200);
    let mut f = fixture(max_window).await;
    let mut rx = f.watcher.subscribe();
    let started = Instant::now();
    let root = f.root.clone();
    let writer = tokio::spawn(async move {
        for i in 0..30 {
            fs::write(root.join("src/main.rs"), format!("// {i}\n")).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let set = f.next().await;
    let elapsed = started.elapsed();
    assert!(elapsed >= max_window, "{elapsed:?}");
    assert!(
        elapsed < max_window + Duration::from_millis(800),
        "{elapsed:?}"
    );
    assert_eq!(summary(&set), [(ChangeKind::Modified, "src/main.rs")]);
    // Subscribers see the same change set.
    assert_eq!(rx.recv().await.unwrap(), set);
    writer.abort();
}

#[tokio::test]
async fn saving_identical_content_is_not_a_change() {
    let mut f = fixture(Duration::from_secs(30)).await;
    record_read(&f.observed, &f.root.join("src/main.rs"));
    f.write("src/main.rs", "fn main() {}\n");
    f.assert_quiet(QUIET * 4).await;
}
