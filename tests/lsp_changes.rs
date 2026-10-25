use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use saddle::config::LspServer;
use saddle::events::FsEvent;
use saddle::lsp::{LspManager, changes};
use saddle::watch::{ChangeKind, ChangeSet, Detail, FileChange};
use tokio::sync::mpsc;

fn changed(root: &Path, path: &str) -> FsEvent {
    FsEvent::Changed(Arc::new(ChangeSet {
        files: vec![FileChange {
            path: root.join(path),
            display: path.into(),
            kind: ChangeKind::Modified,
            detail: Detail::Unseen { size: 10 },
        }],
        diagnostics: None,
    }))
}

async fn receive(rx: &mut mpsc::UnboundedReceiver<FsEvent>) -> FsEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn no_configured_servers_forward_without_waiting() {
    let manager = LspManager::new(PathBuf::from("/p"), &BTreeMap::new(), None);
    let (tx, rx) = mpsc::unbounded_channel();
    let (out, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(changes::forward(
        rx,
        out,
        manager,
        Duration::from_secs(1),
        Duration::from_secs(5),
    ));
    let start = tokio::time::Instant::now();
    tx.send(changed(Path::new("/p"), "a.txt")).unwrap();
    assert!(matches!(events.recv().await, Some(FsEvent::Changed(_))));
    assert_eq!(tokio::time::Instant::now(), start);
    drop(tx);
    task.await.unwrap();
}

#[tokio::test]
async fn coalesced_changes_carry_new_and_resolved_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let servers = BTreeMap::from([(
        "fake".into(),
        LspServer {
            command: "python3".into(),
            args: vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_lsp.py")
                    .display()
                    .to_string(),
            ],
            file_types: vec!["txt".into()],
            root_markers: vec![],
        },
    )]);
    let manager = LspManager::new(root.clone(), &servers, None);
    std::fs::write(root.join("a.txt"), "fine\n").unwrap();
    manager.sync_document(&root.join("a.txt")).await.unwrap();
    assert!(
        manager
            .settle(Duration::from_millis(50), Duration::from_secs(5))
            .await
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let (out, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(changes::forward(
        rx,
        out,
        manager.clone(),
        Duration::from_millis(100),
        Duration::from_secs(5),
    ));
    std::fs::write(root.join("a.txt"), "€😀 ERROR\n").unwrap();
    tx.send(changed(&root, "a.txt")).unwrap();
    assert_eq!(receive(&mut events).await, FsEvent::Settling);
    std::fs::write(root.join("b.txt"), "ERROR\n").unwrap();
    tx.send(changed(&root, "b.txt")).unwrap();
    let FsEvent::Changed(set) = receive(&mut events).await else {
        panic!("expected combined event")
    };
    assert_eq!(set.files.len(), 2);
    let diagnostics = set.diagnostics.as_deref().unwrap();
    assert!(
        diagnostics.contains("new: a.txt:1:4 Error: found ERROR"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("new: b.txt:1:1 Error: found ERROR"),
        "{diagnostics}"
    );
    assert!(!diagnostics.contains("pending"));
    std::fs::write(root.join("a.txt"), "fine\n").unwrap();
    tx.send(changed(&root, "a.txt")).unwrap();
    assert_eq!(receive(&mut events).await, FsEvent::Settling);
    let FsEvent::Changed(set) = receive(&mut events).await else {
        panic!("expected combined event")
    };
    let diagnostics = set.diagnostics.as_deref().unwrap();
    assert!(diagnostics.contains("resolved: a.txt:1:4"), "{diagnostics}");
    assert!(!diagnostics.contains("b.txt"));
    drop(tx);
    task.await.unwrap();
    manager.shutdown().await;
}

#[tokio::test]
async fn deadline_delivers_pending_instead_of_waiting_for_server() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let servers = BTreeMap::from([(
        "fake".into(),
        LspServer {
            command: "python3".into(),
            args: vec!["-c".into(), "import time; time.sleep(30)".into()],
            file_types: vec!["txt".into()],
            root_markers: vec![],
        },
    )]);
    std::fs::write(root.join("a.txt"), "fine\n").unwrap();
    let manager = LspManager::new(root.clone(), &servers, None);
    let (tx, rx) = mpsc::unbounded_channel();
    let (out, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(changes::forward(
        rx,
        out,
        manager.clone(),
        Duration::from_secs(1),
        Duration::from_millis(20),
    ));
    tx.send(changed(&root, "a.txt")).unwrap();
    assert_eq!(receive(&mut events).await, FsEvent::Settling);
    let FsEvent::Changed(set) = receive(&mut events).await else {
        panic!("expected combined event")
    };
    assert!(
        set.diagnostics
            .as_deref()
            .unwrap()
            .contains("still pending")
    );
    drop(tx);
    task.await.unwrap();
    manager.shutdown().await;
}
