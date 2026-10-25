//! How a batch of changes is put to the agent and shown in the transcript.

use super::{ChangeKind, Detail, FileChange};

/// Upper bound on one file's diff in a change event, in bytes.
pub const MAX_DIFF_BYTES: usize = 16 * 1024;
/// Upper bound on all diffs in one change event, in bytes.
pub const MAX_EVENT_DIFF_BYTES: usize = 40 * 1024;

/// The message that starts the agent's turn after the user saved changes.
pub fn describe(changes: &[FileChange]) -> String {
    let count = changes.len();
    let mut out = format!(
        "<file-changes>\nThe user saved changes to {count} {}. Diffs are against the version you last saw.\n",
        plural(count, "file", "files")
    );
    let mut budget = MAX_EVENT_DIFF_BYTES;
    for change in changes {
        out.push('\n');
        out.push_str(&change.kind.to_string());
        out.push(' ');
        out.push_str(&change.display);
        match &change.detail {
            Detail::Diff {
                diff,
                added,
                removed,
            } => {
                out.push_str(&format!(" (+{added} -{removed})\n"));
                if diff.len() > MAX_DIFF_BYTES.min(budget) {
                    out.push_str(
                        "[diff too large to include; read the file to see its current content]\n",
                    );
                } else {
                    budget -= diff.len();
                    let fence = fence(diff);
                    out.push_str(&format!("{fence}diff\n{diff}"));
                    if !diff.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&fence);
                    out.push('\n');
                }
            }
            Detail::Unseen { size } => {
                out.push_str(&format!(" ({size} bytes; content not seen before)\n"))
            }
            Detail::Binary { size } => out.push_str(&format!(" (binary, {size} bytes)\n")),
            Detail::TooLarge { size } => {
                out.push_str(&format!(" ({size} bytes; too large to diff)\n"))
            }
            Detail::Symlink => out.push_str(" (symlink)\n"),
            Detail::None => out.push('\n'),
        }
    }
    out.push_str("</file-changes>");
    out
}

/// One-line headline for the transcript, e.g. "3 files changed".
pub fn headline(changes: &[FileChange]) -> String {
    let count = changes.len();
    format!("{count} {} changed", plural(count, "file", "files"))
}

/// Short per-file line for the transcript, e.g. "modified src/app.rs (+2 -1)".
pub fn file_line(change: &FileChange) -> String {
    let stat = match &change.detail {
        Detail::Diff { added, removed, .. } => format!(" (+{added} -{removed})"),
        Detail::Binary { .. } => " (binary)".into(),
        Detail::Symlink => " (symlink)".into(),
        Detail::Unseen { .. } | Detail::TooLarge { .. } | Detail::None => String::new(),
    };
    format!("{} {}{stat}", change.kind, change.display)
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChangeKind::Created => "created",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        })
    }
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// A backtick fence longer than any backtick run inside `text`.
fn fence(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn change(kind: ChangeKind, display: &str, detail: Detail) -> FileChange {
        FileChange {
            path: PathBuf::from("/p").join(display),
            display: display.into(),
            kind,
            detail,
        }
    }

    #[test]
    fn describes_each_kind_of_change() {
        let changes = [
            change(
                ChangeKind::Modified,
                "src/app.rs",
                Detail::Diff {
                    diff: "@@ -1 +1 @@\n-a\n+b\n".into(),
                    added: 1,
                    removed: 1,
                },
            ),
            change(
                ChangeKind::Created,
                "src/new.rs",
                Detail::Unseen { size: 12 },
            ),
            change(ChangeKind::Modified, "logo.png", Detail::Binary { size: 9 }),
            change(ChangeKind::Deleted, "src/old.rs", Detail::None),
        ];
        assert_eq!(
            describe(&changes),
            "<file-changes>\n\
             The user saved changes to 4 files. Diffs are against the version you last saw.\n\
             \n\
             modified src/app.rs (+1 -1)\n\
             ```diff\n\
             @@ -1 +1 @@\n-a\n+b\n\
             ```\n\
             \n\
             created src/new.rs (12 bytes; content not seen before)\n\
             \n\
             modified logo.png (binary, 9 bytes)\n\
             \n\
             deleted src/old.rs\n\
             </file-changes>"
        );
        assert_eq!(headline(&changes), "4 files changed");
        assert_eq!(headline(&changes[..1]), "1 file changed");
        assert_eq!(file_line(&changes[0]), "modified src/app.rs (+1 -1)");
        assert_eq!(file_line(&changes[1]), "created src/new.rs");
    }

    #[test]
    fn oversized_diffs_are_left_out() {
        let big = format!("@@ -1 +1 @@\n{}", "+x\n".repeat(MAX_DIFF_BYTES));
        let small = "@@ -1 +1 @@\n-a\n+b\n".to_owned();
        let diff = |d: &str| Detail::Diff {
            diff: d.into(),
            added: 1,
            removed: 1,
        };
        let changes = [
            change(ChangeKind::Modified, "big.rs", diff(&big)),
            change(ChangeKind::Modified, "small.rs", diff(&small)),
        ];
        let text = describe(&changes);
        assert!(text.contains("big.rs (+1 -1)\n[diff too large"));
        assert!(text.contains(&small));
        assert!(text.len() < MAX_DIFF_BYTES);
    }

    #[test]
    fn total_diff_budget_is_bounded() {
        let diff = format!("@@ -1 +1 @@\n{}", "+x\n".repeat(MAX_DIFF_BYTES / 4));
        let changes: Vec<_> = (0..20)
            .map(|i| {
                change(
                    ChangeKind::Modified,
                    &format!("f{i}.rs"),
                    Detail::Diff {
                        diff: diff.clone(),
                        added: 1,
                        removed: 0,
                    },
                )
            })
            .collect();
        let text = describe(&changes);
        assert!(text.len() < MAX_EVENT_DIFF_BYTES + 4096);
        assert!(text.contains("f19.rs (+1 -0)\n[diff too large"));
    }

    #[test]
    fn fences_outgrow_backticks_in_the_diff() {
        assert_eq!(fence("plain"), "```");
        assert_eq!(fence("+```rust\n"), "````");
    }
}
