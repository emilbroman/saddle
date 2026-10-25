//! Smoke test against a real rust-analyzer on a copy of this repository.
//!
//! Ignored by default because it needs `rust-analyzer` on PATH and indexes the
//! crate with its dependencies. Run with:
//!
//!     cargo test --test lsp_rust_analyzer -- --ignored --nocapture

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use saddle::agent::Agent;
use saddle::events::{AgentEvent, FsEvent, TurnOutcome};
use saddle::tools::{ProjectFs, Registry, lsp as lsp_tools};
use saddle::watch::{ChangeKind, ChangeSet, Detail, FileChange};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use saddle::config::LspServer;
use saddle::lsp::{LspManager, ServerState, Severity, TextPosition};

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Position of the first occurrence of `needle` in `file`, plus `offset` characters.
fn find(file: &Path, needle: &str, offset: u32) -> TextPosition {
    let text = std::fs::read_to_string(file).unwrap();
    for (line, content) in text.lines().enumerate() {
        if let Some(byte) = content.find(needle) {
            return TextPosition {
                line: line as u32,
                column: content[..byte].chars().count() as u32 + offset,
            };
        }
    }
    panic!("{needle:?} not found in {}", file.display());
}

#[tokio::test]
#[ignore = "needs rust-analyzer and indexes the whole crate"]
async fn rust_analyzer_on_saddle() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = tempfile::tempdir().unwrap();
    let root: PathBuf = dir.path().canonicalize().unwrap();
    for file in ["Cargo.toml", "Cargo.lock"] {
        std::fs::copy(manifest.join(file), root.join(file)).unwrap();
    }
    copy_tree(&manifest.join("src"), &root.join("src"));
    copy_tree(&manifest.join("tests"), &root.join("tests"));
    // Reuse build output across runs for rust-analyzer's `cargo check`.
    let target = manifest.join("target/rust-analyzer-smoke");
    // SAFETY: set before any server process is spawned; tests here are single-threaded.
    unsafe { std::env::set_var("CARGO_TARGET_DIR", &target) };

    let mut servers = BTreeMap::new();
    servers.insert(
        "rust".to_string(),
        LspServer {
            command: "rust-analyzer".into(),
            args: vec![],
            file_types: vec!["rs".into()],
            root_markers: vec!["Cargo.toml".into()],
        },
    );
    let manager = LspManager::new(root.clone(), &servers, None);
    let started = Instant::now();
    manager.start_detected();
    let settled = manager
        .settle(Duration::from_secs(2), Duration::from_secs(600))
        .await;
    eprintln!(
        "initial settle: {settled} after {:?}; {:?}",
        started.elapsed(),
        manager.statuses()
    );
    assert!(matches!(manager.statuses()[0].state, ServerState::Ready));

    // Definition across files: `project::resolve_root` in main.rs.
    let main = root.join("src/main.rs");
    let pos = find(&main, "project::resolve_root", "project::".len() as u32 + 2);
    let started = Instant::now();
    let definition = manager.definition(&main, pos).await.unwrap();
    eprintln!("definition after {:?}: {definition:?}", started.elapsed());
    let project = root.join("src/project.rs");
    assert_eq!(definition.len(), 1);
    assert_eq!(definition[0].path, project);
    assert_eq!(
        definition[0].range.start,
        find(&project, "pub fn resolve_root", "pub fn ".len() as u32)
    );

    let hover = manager.hover(&main, pos).await.unwrap().unwrap();
    eprintln!("hover: {hover}");
    assert!(hover.contains("resolve_root"));

    let references = manager
        .references(&project, definition[0].range.start, true)
        .await
        .unwrap();
    eprintln!("references: {}", references.len());
    assert!(references.iter().any(|r| r.path == main));

    let symbols = manager.document_symbols(&project).await.unwrap();
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "resolve_root" && s.kind == "function")
    );
    let symbols = manager.workspace_symbols("resolve_root").await.unwrap();
    assert!(symbols.iter().any(|s| s.location.path == project));

    let mut tools = Registry::default();
    lsp_tools::install(&mut tools, manager.clone(), ProjectFs::new(&root).unwrap());
    for (name, args) in [
        (
            "lsp_workspace_symbols",
            serde_json::json!({"query": "resolve_root"}),
        ),
        (
            "lsp_definition",
            serde_json::json!({"path": "src/main.rs", "line": pos.line + 1, "col": pos.column + 1}),
        ),
        (
            "lsp_references",
            serde_json::json!({"path": "src/project.rs", "line": definition[0].range.start.line + 1, "col": definition[0].range.start.column + 1}),
        ),
    ] {
        let result = tools.call(name, args).await;
        assert!(!result.is_error, "{}", result.content);
        eprintln!("{name}: {}", result.content);
        assert!(result.content.contains("src/"));
    }

    // Opt-in live provider verification; credentials come from normal config.
    let mut live_agent = if std::env::var_os("SADDLE_LIVE_LSP").is_some() {
        let loaded = saddle::config::load().unwrap();
        let provider =
            saddle::provider::GenaiProvider::new(&loaded.config, &loaded.credentials, None)
                .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::new(Arc::new(provider), tools, saddle::prompt::system(&root), tx);
        let outcome = agent.submit("Use lsp_workspace_symbols to find resolve_root, then lsp_definition on its returned location, then lsp_references on the definition. Call those three tools in that order, and briefly tell me which files reference it. Do not edit anything.".into(), &CancellationToken::new()).await;
        assert_eq!(outcome, TurnOutcome::Completed);
        let mut names = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolCall { name, .. } => {
                    eprintln!("live tool: {name}");
                    names.push(name);
                }
                AgentEvent::ToolResult {
                    is_error, output, ..
                } => {
                    assert!(!is_error, "{output}");
                    eprintln!("live result: {output}");
                }
                AgentEvent::AssistantDelta(text) => eprint!("{text}"),
                _ => {}
            }
        }
        for name in ["lsp_workspace_symbols", "lsp_definition", "lsp_references"] {
            assert!(names.iter().any(|n| n == name), "missing {name}: {names:?}");
        }
        Some((agent, rx))
    } else {
        None
    };

    let (changes_tx, changes_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let forward = tokio::spawn(saddle::lsp::changes::forward(
        changes_rx,
        events_tx,
        manager.clone(),
        Duration::from_secs(1),
        Duration::from_secs(5),
    ));

    // Introduce a type error on disk and report the change.
    let broken = root.join("src/prompt.rs");
    let mut text = std::fs::read_to_string(&broken).unwrap();
    text.push_str("\npub fn broken() -> u32 {\n    \"not a number\"\n}\n");
    std::fs::write(&broken, &text).unwrap();
    let started = Instant::now();
    changes_tx
        .send(FsEvent::Changed(Arc::new(ChangeSet {
            files: vec![FileChange {
                path: broken.clone(),
                display: "src/prompt.rs".into(),
                kind: ChangeKind::Modified,
                detail: Detail::Unseen {
                    size: text.len() as u64,
                },
            }],
            diagnostics: None,
        })))
        .unwrap();
    assert_eq!(events_rx.recv().await.unwrap(), FsEvent::Settling);
    let FsEvent::Changed(set) = tokio::time::timeout(Duration::from_secs(10), events_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected change event")
    };
    let diagnostic_changes = set.diagnostics.as_deref().unwrap();
    eprintln!(
        "combined event after {:?}: {diagnostic_changes}",
        started.elapsed()
    );
    assert!(
        diagnostic_changes.contains("new: src/prompt.rs:"),
        "{diagnostic_changes}"
    );
    assert!(
        diagnostic_changes.contains("mismatched types"),
        "{diagnostic_changes}"
    );
    if let Some((agent, rx)) = &mut live_agent {
        let message = format!(
            "{}\n{}\nBriefly explain the new diagnostic; do not edit anything.",
            saddle::watch::message::describe(&set.files),
            diagnostic_changes
        );
        assert_eq!(
            agent.submit(message, &CancellationToken::new()).await,
            TurnOutcome::Completed
        );
        let mut response = String::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::AssistantDelta(text) = event {
                response.push_str(&text);
            }
        }
        eprintln!("live reaction: {response}");
        assert!(
            response.contains("u32") && (response.contains("str") || response.contains("string")),
            "{response}"
        );
    }
    let errors = loop {
        manager
            .settle(Duration::from_secs(1), Duration::from_secs(300))
            .await;
        let errors: Vec<_> = manager
            .diagnostics_for(std::slice::from_ref(&broken))
            .remove(&broken)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        if !errors.is_empty() || started.elapsed() > Duration::from_secs(300) {
            break errors;
        }
    };
    eprintln!("diagnostics after {:?}: {errors:#?}", started.elapsed());
    let expected = find(&broken, "\"not a number\"", 0);
    assert!(
        errors.iter().any(|d| d.range.start == expected),
        "no error at {expected:?}"
    );

    // Reverting clears it.
    text.truncate(text.find("\npub fn broken").unwrap());
    text.push('\n');
    std::fs::write(&broken, &text).unwrap();
    let started = Instant::now();
    manager.did_change(vec![broken.clone()]);
    loop {
        manager
            .settle(Duration::from_secs(1), Duration::from_secs(300))
            .await;
        let errors = manager
            .diagnostics_for(std::slice::from_ref(&broken))
            .remove(&broken)
            .unwrap_or_default();
        if errors.iter().all(|d| d.severity != Severity::Error) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "error did not clear"
        );
    }
    eprintln!("cleared after {:?}", started.elapsed());

    eprintln!("server log tail:");
    for line in manager.log("rust").iter().rev().take(10).rev() {
        eprintln!("  {line}");
    }
    let started = Instant::now();
    drop(changes_tx);
    forward.await.unwrap();
    manager.shutdown().await;
    eprintln!("shutdown after {:?}", started.elapsed());
    assert_eq!(manager.statuses()[0].state, ServerState::Stopped);
}
