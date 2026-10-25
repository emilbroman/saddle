//! Enrich watcher batches before delivering them, without delaying interruption.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{Diagnostic, LspManager, ServerState};
use crate::events::FsEvent;
use crate::watch::ChangeSet;

const MAX_DIAGNOSTICS_BYTES: usize = 12 * 1024;
type Snapshot = BTreeMap<PathBuf, Vec<Diagnostic>>;

pub async fn forward(
    mut incoming: mpsc::UnboundedReceiver<FsEvent>,
    outgoing: mpsc::UnboundedSender<FsEvent>,
    lsp: LspManager,
    quiet: Duration,
    cap: Duration,
) {
    while let Some(event) = incoming.recv().await {
        let FsEvent::Changed(first) = event else {
            if outgoing.send(event).is_err() {
                return;
            }
            continue;
        };
        if !lsp.has_servers_configured() {
            if outgoing.send(FsEvent::Changed(first)).is_err() {
                return;
            }
            continue;
        }
        if outgoing.send(FsEvent::Settling).is_err() {
            return;
        }
        let before = lsp.diagnostics();
        let mut files = first.files.clone();
        let mut git = first.git.clone();
        let mut sync_paths = first.sync_paths.clone();
        lsp.did_change(first.changed_paths());
        let settled = lsp.settle(quiet, cap);
        tokio::pin!(settled);
        let settled = loop {
            tokio::select! {
                biased;
                result = &mut settled => break result,
                next = incoming.recv() => match next {
                    Some(FsEvent::Changed(set)) => {
                        lsp.did_change(set.changed_paths());
                        files.extend(set.files.iter().cloned());
                        git.extend(set.git.iter().cloned());
                        sync_paths.extend(set.sync_paths.iter().cloned());
                    }
                    Some(event) => { let _ = outgoing.send(event); }
                    None => break settled.await,
                }
            }
        };
        let mut diagnostics = describe(lsp.project_root(), &before, &lsp.diagnostics(), settled);
        for status in lsp.statuses() {
            if let ServerState::Failed(reason) = status.state {
                diagnostics.push_str(&format!(
                    "\nLSP {} unavailable: {}",
                    status.language,
                    short(&reason)
                ));
            }
        }
        let set = Arc::new(ChangeSet {
            files,
            git,
            sync_paths,
            diagnostics: Some(diagnostics),
        });
        if outgoing.send(FsEvent::Changed(set)).is_err() {
            return;
        }
    }
}

/// Match exact diagnostics first, then pair edits at the same source location.
pub fn describe(root: &Path, before: &Snapshot, after: &Snapshot, settled: bool) -> String {
    let mut removed: Vec<_> = before.values().flatten().collect();
    let mut added = Vec::new();
    for diagnostic in after.values().flatten() {
        if let Some(index) = removed.iter().position(|old| *old == diagnostic) {
            removed.remove(index);
        } else {
            added.push(diagnostic);
        }
    }
    let mut out = String::from("<diagnostic-changes>\n");
    let mut truncated = false;
    let mut emit = |line: String| {
        if out.len() + line.len() <= MAX_DIAGNOSTICS_BYTES - 256 {
            out.push_str(&line);
            out.push('\n');
        } else {
            truncated = true;
        }
    };
    for diagnostic in added {
        let old = removed
            .iter()
            .position(|old| {
                old.path == diagnostic.path
                    && old.range.start == diagnostic.range.start
                    && old.source == diagnostic.source
                    && old.code == diagnostic.code
            })
            .map(|index| removed.remove(index));
        match old {
            Some(old) => emit(format!(
                "changed: {} (was: {})",
                line(root, diagnostic),
                short(&old.message)
            )),
            None => emit(format!("new: {}", line(root, diagnostic))),
        }
    }
    for diagnostic in removed {
        emit(format!("resolved: {}", line(root, diagnostic)));
    }
    if out == "<diagnostic-changes>\n" {
        out.push_str("No diagnostic changes reported.\n");
    }
    if truncated {
        out.push_str("[diagnostic changes truncated; use lsp_diagnostics for current results]\n");
    }
    if !settled {
        out.push_str("Diagnostics still pending (settle deadline reached); late results are available via lsp_diagnostics.\n");
    }
    out.push_str("</diagnostic-changes>");
    out
}

fn line(root: &Path, d: &Diagnostic) -> String {
    let path = d.path.strip_prefix(root).unwrap_or(&d.path);
    format!(
        "{}:{}:{} {:?}{}: {}",
        path.display(),
        u64::from(d.range.start.line) + 1,
        u64::from(d.range.start.column) + 1,
        d.severity,
        d.code
            .as_ref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default(),
        short(&d.message)
    )
}

fn short(text: &str) -> String {
    let mut chars = text.chars();
    let clipped: String = chars
        .by_ref()
        .take(600)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{Severity, TextPosition, TextRange};

    fn diagnostic(message: &str, line: u32) -> Diagnostic {
        let pos = TextPosition { line, column: 2 };
        Diagnostic {
            path: "/p/a.rs".into(),
            range: TextRange {
                start: pos,
                end: pos,
            },
            severity: Severity::Error,
            message: message.into(),
            source: Some("rustc".into()),
            code: Some("E0308".into()),
        }
    }
    fn snapshot(ds: Vec<Diagnostic>) -> Snapshot {
        BTreeMap::from([(PathBuf::from("/p/a.rs"), ds)])
    }

    #[test]
    fn diff_omits_unchanged_and_reports_new_changed_resolved() {
        let before = snapshot(vec![
            diagnostic("same", 0),
            diagnostic("old", 1),
            diagnostic("gone", 2),
        ]);
        let after = snapshot(vec![
            diagnostic("same", 0),
            diagnostic("fixed wording", 1),
            diagnostic("new", 3),
        ]);
        let text = describe(Path::new("/p"), &before, &after, false);
        assert!(!text.contains("same"));
        assert!(text.contains("changed: a.rs:2:3 Error [E0308]: fixed wording (was: old)"));
        assert!(text.contains("new: a.rs:4:3"));
        assert!(text.contains("resolved: a.rs:3:3"));
        assert!(text.contains("still pending"));
    }

    #[test]
    fn bounded_unicode_output() {
        let after = snapshot(
            (0..1000)
                .map(|n| diagnostic(&"é".repeat(1000), n))
                .collect(),
        );
        let text = describe(Path::new("/p"), &Snapshot::new(), &after, true);
        assert!(text.len() <= MAX_DIAGNOSTICS_BYTES);
        assert!(text.contains("truncated"));
    }
}
