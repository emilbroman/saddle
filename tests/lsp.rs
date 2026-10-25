//! LSP client lifecycle tests against a scripted server (`fixtures/fake_lsp.py`).
//! Skipped when `python3` is not available.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use saddle::config::LspServer;
use saddle::events::LspEvent;
use saddle::lsp::{LspManager, ServerState, Severity, TextPosition};
use tokio::sync::mpsc;

fn python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn fake_server() -> LspServer {
    LspServer {
        command: "python3".into(),
        args: vec![
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake_lsp.py")
                .to_string_lossy()
                .into_owned(),
        ],
        file_types: vec!["txt".into()],
        root_markers: vec![],
    }
}

fn setup(
    server: LspServer,
) -> (
    tempfile::TempDir,
    PathBuf,
    LspManager,
    mpsc::UnboundedReceiver<LspEvent>,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert("fake".to_string(), server);
    let (tx, rx) = mpsc::unbounded_channel();
    let manager = LspManager::new(root.clone(), &servers, Some(tx));
    (dir, root, manager, rx)
}

async fn settle(manager: &LspManager) {
    assert!(
        manager
            .settle(Duration::from_millis(300), Duration::from_secs(20))
            .await,
        "diagnostics did not settle: {:?}",
        manager.statuses()
    );
}

/// Waits for a status event satisfying `predicate`.
async fn wait_for(
    rx: &mut mpsc::UnboundedReceiver<LspEvent>,
    predicate: impl Fn(&ServerState) -> bool,
) {
    let wait = async {
        while let Some(LspEvent::Status(statuses)) = rx.recv().await {
            if statuses.iter().any(|s| predicate(&s.state)) {
                return;
            }
        }
        panic!("event channel closed");
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("expected server state never arrived");
}

#[tokio::test]
async fn sync_diagnostics_definition_and_shutdown() {
    if !python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (_dir, root, manager, _rx) = setup(fake_server());
    let file = root.join("a.txt");
    std::fs::write(&file, "hello\n€😀 ERROR here\nthe target\n").unwrap();

    manager.did_change(vec![file.clone()]);
    settle(&manager).await;
    assert_eq!(manager.statuses()[0].state, ServerState::Ready);

    let diagnostics = manager.diagnostics_for(std::slice::from_ref(&file));
    let ds = &diagnostics[&file];
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].severity, Severity::Error);
    assert_eq!(ds[0].message, "found ERROR");
    // UTF-16 offset 4 is character 3, after "€😀 ".
    assert_eq!(ds[0].range.start, TextPosition { line: 1, column: 3 });
    assert_eq!(ds[0].range.end, TextPosition { line: 1, column: 8 });

    let definition = manager
        .definition(Path::new("a.txt"), TextPosition { line: 0, column: 0 })
        .await
        .unwrap();
    assert_eq!(definition.len(), 1);
    assert_eq!(definition[0].path, file);
    assert_eq!(
        definition[0].range.start,
        TextPosition { line: 2, column: 4 }
    );

    // Fixing the file on disk clears the diagnostic.
    std::fs::write(&file, "hello\nfine\nthe target\n").unwrap();
    manager.did_change(vec![file.clone()]);
    settle(&manager).await;
    assert!(manager.diagnostics().is_empty());

    // A non-language file reaches the server through its registered watcher.
    let toml = root.join("Cargo.toml");
    std::fs::write(&toml, "").unwrap();
    manager.did_change(vec![toml]);
    settle(&manager).await;

    // Deleting an open document closes it.
    std::fs::write(&file, "ERROR again\n").unwrap();
    manager.did_change(vec![file.clone()]);
    settle(&manager).await;
    assert_eq!(manager.diagnostics().len(), 1);
    std::fs::remove_file(&file).unwrap();
    manager.did_change(vec![file.clone()]);
    settle(&manager).await;
    assert!(manager.diagnostics().is_empty());

    let log = manager.log("fake").join("\n");
    assert!(log.contains("offered utf-8,utf-32,utf-16"), "{log}");
    assert!(log.contains("configuration=[null, null]"), "{log}");
    assert!(log.contains("registered"), "{log}");
    assert!(log.contains("saved file://"), "{log}");
    assert!(log.contains("Cargo.toml"), "{log}");

    assert!(
        manager
            .definition(
                Path::new("../outside.txt"),
                TextPosition { line: 0, column: 0 }
            )
            .await
            .is_err()
    );
    assert!(
        manager
            .hover(Path::new("Cargo.toml"), TextPosition { line: 0, column: 0 })
            .await
            .is_err()
    );

    manager.shutdown().await;
    assert_eq!(manager.statuses()[0].state, ServerState::Stopped);
}

#[tokio::test]
async fn crashed_server_restarts_and_recovers() {
    if !python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (_dir, root, manager, mut rx) = setup(fake_server());
    let file = root.join("b.txt");
    std::fs::write(&file, "CRASH\n").unwrap();
    manager.did_change(vec![file.clone()]);
    // Stderr is read independently and may arrive after the exit status.
    wait_for(
        &mut rx,
        |s| matches!(s, ServerState::Restarting { attempt: 1, reason } if !reason.is_empty()),
    )
    .await;

    // Restarts re-open the remembered content, so fixing the file lets it recover.
    std::fs::write(&file, "ERROR\n").unwrap();
    manager.did_change(vec![file.clone()]);
    wait_for(&mut rx, |s| *s == ServerState::Ready).await;
    settle(&manager).await;
    assert_eq!(
        manager.diagnostics_for(std::slice::from_ref(&file))[&file].len(),
        1
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn repeated_crashes_give_up() {
    if !python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (_dir, root, manager, mut rx) = setup(fake_server());
    let file = root.join("c.txt");
    std::fs::write(&file, "CRASH\n").unwrap();
    manager.did_change(vec![file.clone()]);
    wait_for(
        &mut rx,
        |s| matches!(s, ServerState::Failed(reason) if reason.contains("gave up")),
    )
    .await;
    let error = manager
        .definition(&file, TextPosition { line: 0, column: 0 })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("failed"), "{error}");
    manager.shutdown().await;
}

#[tokio::test]
async fn missing_command_fails_without_retrying() {
    let mut server = fake_server();
    server.command = "saddle-no-such-language-server".into();
    let (_dir, root, manager, mut rx) = setup(server);
    let file = root.join("d.txt");
    std::fs::write(&file, "x\n").unwrap();
    manager.did_change(vec![file]);
    wait_for(
        &mut rx,
        |s| matches!(s, ServerState::Failed(reason) if reason.contains("cannot start")),
    )
    .await;
    manager.shutdown().await;
}
