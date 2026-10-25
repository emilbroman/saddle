//! Real metadata watches, including the two administrative dirs of linked worktrees.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use saddle::events::FsEvent;
use saddle::git::{GitState, Tracker};
use saddle::watch::{ChangeSet, GitTracking, Observed, WatchConfig, Watcher, filter::Filter};
use tokio::sync::mpsc;
use tokio::time::timeout;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Watcher Test"]);
    git(
        dir.path(),
        &["config", "user.email", "watcher@example.invalid"],
    );
    fs::write(dir.path().join("a.txt"), "main\n").unwrap();
    fs::write(dir.path().join("dirty.txt"), "base\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "initial"]);
    dir
}

struct Watching {
    root: PathBuf,
    rx: mpsc::UnboundedReceiver<FsEvent>,
    _watcher: Watcher,
}

impl Watching {
    async fn start(root: &Path) -> Self {
        let root = root.canonicalize().unwrap();
        let observed = Observed::default();
        observed.seed(&Filter::new(&root, vec![]));
        for file in ["a.txt", "dirty.txt"] {
            let path = root.join(file);
            let bytes = fs::read(&path).unwrap();
            observed.record_read(&path, &fs::metadata(&path).unwrap(), &bytes);
        }
        let tracker = Tracker::new(root.clone(), None, GitState::default());
        tracker.classify(&mut vec![], None, true).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let watcher = Watcher::spawn_with_git(
            root.clone(),
            WatchConfig {
                quiet: Duration::from_millis(200),
                max_window: Duration::from_secs(2),
                excluded: vec![],
            },
            observed.clone(),
            tx,
            GitTracking {
                tracker,
                session: None,
            },
        );
        let mut result = Self {
            root,
            rx,
            _watcher: watcher,
        };
        result.quiet().await;
        result
    }

    async fn next(&mut self) -> Arc<ChangeSet> {
        match timeout(Duration::from_secs(8), self.rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            FsEvent::Changed(set) => set,
            event => panic!("unexpected event: {event:?}"),
        }
    }

    async fn quiet(&mut self) {
        assert!(
            timeout(Duration::from_millis(650), self.rx.recv())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn checkout_folds_head_blobs_but_preserves_dirty_changes_and_sync_paths() {
    let dir = repo();
    git(dir.path(), &["checkout", "-qb", "feature"]);
    fs::write(dir.path().join("a.txt"), "feature\n").unwrap();
    git(dir.path(), &["commit", "-qam", "feature work"]);
    git(dir.path(), &["checkout", "-q", "main"]);
    let mut w = Watching::start(dir.path()).await;
    fs::write(w.root.join("dirty.txt"), "my pending edit\n").unwrap();
    git(&w.root, &["checkout", "-q", "feature"]);
    let set = w.next().await;
    assert_eq!(set.git.len(), 1, "{set:?}");
    assert!(
        set.git[0].text.to_lowercase().contains("branch switch"),
        "{set:?}"
    );
    assert!(set.git[0].text.contains("feature work"));
    assert_eq!(set.files.len(), 1);
    assert_eq!(set.files[0].display, "dirty.txt");
    assert!(set.sync_paths.contains(&w.root.join("a.txt")));
    w.quiet().await;
    fs::write(w.root.join("a.txt"), "my next edit\n").unwrap();
    let set = w.next().await;
    assert!(set.git.is_empty());
    assert!(
        matches!(&set.files[0].detail, saddle::watch::Detail::Diff { diff, .. } if diff.contains("-feature") && !diff.contains("-main"))
    );
}

#[tokio::test]
async fn local_commit_is_a_metadata_only_event_and_index_refresh_is_quiet() {
    let dir = repo();
    let mut w = Watching::start(dir.path()).await;
    fs::write(w.root.join("a.txt"), "committed content\n").unwrap();
    let set = w.next().await;
    assert!(set.git.is_empty());
    git(&w.root, &["commit", "-qam", "own change"]);
    let set = w.next().await;
    assert!(set.files.is_empty(), "{set:?}");
    assert_eq!(set.git.len(), 1);
    assert!(set.git[0].text.contains("committed "), "{set:?}");
    assert!(set.git[0].text.contains("own change"));
    git(&w.root, &["status", "--porcelain"]);
    w.quiet().await;
}

#[tokio::test]
async fn stash_restore_and_hard_reset_are_reverts_not_patches() {
    let dir = repo();
    let mut w = Watching::start(dir.path()).await;
    for command in [
        vec!["stash", "push", "-qm", "stash"],
        vec!["checkout", "--", "a.txt"],
        vec!["reset", "--hard", "HEAD"],
    ] {
        fs::write(w.root.join("a.txt"), "dirty\n").unwrap();
        w.next().await;
        git(&w.root, &command);
        let set = w.next().await;
        assert!(set.files.is_empty(), "{command:?}: {set:?}");
        assert!(
            set.git[0]
                .text
                .to_lowercase()
                .contains("working tree reverted to head"),
            "{set:?}"
        );
        assert!(set.git[0].text.contains("a.txt"));
        w.quiet().await;
    }
}

#[tokio::test]
async fn linked_worktree_observes_private_head_and_shared_refs() {
    let dir = repo();
    let outside = tempfile::tempdir().unwrap();
    let linked = outside.path().join("linked");
    git(
        dir.path(),
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    let mut w = Watching::start(&linked).await;
    git(&w.root, &["checkout", "-qb", "another"]);
    let set = w.next().await;
    assert!(
        set.git[0].text.to_lowercase().contains("branch switch"),
        "{set:?}"
    );
    fs::write(w.root.join("a.txt"), "linked edit\n").unwrap();
    git(&w.root, &["commit", "-qam", "linked commit"]);
    let set = w.next().await;
    assert!(set.git[0].text.contains("committed "), "{set:?}");
    assert!(set.files.is_empty());
    w.quiet().await;
    // Packed refs are an independently watched metadata source.
    git(&w.root, &["pack-refs", "--all", "--prune"]);
    w.quiet().await;
}
