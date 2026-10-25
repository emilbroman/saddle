//! What Saddle last knew about each project file, and how a batch of
//! invalidated paths compares against it. OS events only say *where* to look;
//! the change itself is established by re-reading the disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use similar::{ChangeTag, TextDiff};

use super::batch::Batch;
use super::filter::Filter;
use super::{ChangeKind, Detail, FileChange};
use crate::tools::looks_binary;

/// Files larger than this are reported by path and size only.
pub const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
enum Content {
    Text(Arc<str>),
    Binary,
    TooLarge,
    /// Exists, but its content was never looked at.
    Unread,
    Symlink,
}

#[derive(Debug, Clone)]
struct Known {
    len: u64,
    modified: Option<SystemTime>,
    content: Content,
}

impl Known {
    fn same_metadata(&self, meta: &Metadata) -> bool {
        self.len == meta.len() && self.modified == meta.modified().ok()
    }
}

/// Shared between the watcher, which compares batches against it, and
/// `read_file`, which records what the agent has seen.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    files: Arc<Mutex<BTreeMap<PathBuf, Known>>>,
}

impl Observed {
    /// Records a file the agent read in full, so later changes to it are diffed.
    pub fn record_read(&self, path: &Path, meta: &Metadata, bytes: &[u8]) {
        let content = classify(bytes);
        let known = Known {
            len: bytes.len() as u64,
            modified: meta.modified().ok(),
            content,
        };
        self.files.lock().unwrap().insert(path.to_owned(), known);
    }

    /// Records the existence (not the content) of every reportable file, so
    /// that later events can tell created files from modified ones.
    pub fn seed(&self, filter: &Filter) {
        let found: Vec<_> = filter
            .walk(filter.root())
            .flatten()
            .filter(|e| e.depth() > 0 && !e.file_type().is_some_and(|t| t.is_dir()))
            .filter_map(|e| Some((e.path().to_owned(), e.metadata().ok()?)))
            .collect();
        let mut files = self.files.lock().unwrap();
        for (path, meta) in found {
            files.entry(path).or_insert_with(|| unread(&meta));
        }
    }

    /// Re-reads every path the batch invalidated and reports what actually
    /// changed since Saddle last looked, updating the record as it goes.
    pub fn compare(&self, filter: &mut Filter, batch: Batch) -> Vec<FileChange> {
        let mut files = self.files.lock().unwrap();
        let candidates = expand(filter, &files, batch);
        let root = filter.root().to_owned();
        candidates
            .into_iter()
            .filter_map(|path| {
                let kind_detail = compare_one(&mut files, &path)?;
                Some(FileChange {
                    display: display(&root, &path),
                    path,
                    kind: kind_detail.0,
                    detail: kind_detail.1,
                })
            })
            .collect()
    }
}

/// Turns invalidated paths into the files to compare: directories stand for
/// everything below them, both on disk and in the record.
fn expand(
    filter: &mut Filter,
    files: &BTreeMap<PathBuf, Known>,
    batch: Batch,
) -> BTreeSet<PathBuf> {
    let mut paths = batch.paths;
    if batch.rescan {
        paths.insert(filter.root().to_owned());
    }
    let mut out = BTreeSet::new();
    for path in paths {
        let is_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        if is_dir {
            out.extend(
                filter
                    .walk(&path)
                    .flatten()
                    .filter(|e| !e.file_type().is_some_and(|t| t.is_dir()))
                    .map(|e| e.into_path()),
            );
        }
        out.extend(
            files
                .range(path.clone()..)
                .map(|(known, _)| known)
                .take_while(|known| known.starts_with(&path))
                .cloned(),
        );
        if !is_dir {
            out.insert(path);
        }
    }
    out.retain(|path| !filter.is_ignored(path, false));
    out
}

fn compare_one(files: &mut BTreeMap<PathBuf, Known>, path: &Path) -> Option<(ChangeKind, Detail)> {
    let prior = files.get(path);
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_dir() => meta,
        _ => {
            files.remove(path)?;
            return Some((ChangeKind::Deleted, Detail::None));
        }
    };
    let kind = if prior.is_some() {
        ChangeKind::Modified
    } else {
        ChangeKind::Created
    };

    if meta.file_type().is_symlink() {
        if prior.is_some_and(|k| k.same_metadata(&meta)) {
            return None;
        }
        let mut known = unread(&meta);
        known.content = Content::Symlink;
        files.insert(path.to_owned(), known);
        return Some((kind, Detail::Symlink));
    }

    // Without a snapshot there is nothing finer to compare than metadata.
    if let Some(known) = prior
        && !matches!(known.content, Content::Text(_))
        && known.same_metadata(&meta)
    {
        return None;
    }

    let size = meta.len();
    let content = if size > MAX_SNAPSHOT_BYTES {
        Content::TooLarge
    } else {
        match fs::read(path) {
            Ok(bytes) => classify(&bytes),
            // Vanished or unreadable between the stat and the read; a later
            // event will tell.
            Err(_) => return None,
        }
    };
    let detail = match (prior.map(|k| &k.content), &content) {
        (Some(Content::Text(old)), Content::Text(new)) if old == new => {
            files.insert(
                path.to_owned(),
                Known {
                    len: size,
                    modified: meta.modified().ok(),
                    content,
                },
            );
            return None;
        }
        (Some(Content::Text(old)), Content::Text(new)) => diff(old, new),
        (_, Content::Binary) => Detail::Binary { size },
        (_, Content::TooLarge) => Detail::TooLarge { size },
        _ => Detail::Unseen { size },
    };
    files.insert(
        path.to_owned(),
        Known {
            len: size,
            modified: meta.modified().ok(),
            content,
        },
    );
    Some((kind, detail))
}

fn classify(bytes: &[u8]) -> Content {
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        Content::TooLarge
    } else if looks_binary(bytes) {
        Content::Binary
    } else {
        Content::Text(String::from_utf8_lossy(bytes).into())
    }
}

fn unread(meta: &Metadata) -> Known {
    Known {
        len: meta.len(),
        modified: meta.modified().ok(),
        content: Content::Unread,
    }
}

fn diff(old: &str, new: &str) -> Detail {
    let diff = TextDiff::from_lines(old, new);
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    Detail::Diff {
        diff: diff.unified_diff().context_radius(3).to_string(),
        added,
        removed,
    }
}

fn display(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Project {
        _dir: tempfile::TempDir,
        root: PathBuf,
        filter: Filter,
        observed: Observed,
    }

    impl Project {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("target")).unwrap();
            fs::write(root.join(".gitignore"), "*.log\n").unwrap();
            fs::write(
                root.join("src/main.rs"),
                "fn main() {\n    println!(\"hi\");\n}\n",
            )
            .unwrap();
            fs::write(root.join("src/lib.rs"), "pub mod a;\n").unwrap();
            let filter = Filter::new(&root, vec![root.join("target")]);
            let observed = Observed::default();
            observed.seed(&filter);
            Self {
                _dir: dir,
                root,
                filter,
                observed,
            }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
            let path = self.path(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn read(&self, rel: &str) {
            let path = self.path(rel);
            let bytes = fs::read(&path).unwrap();
            self.observed
                .record_read(&path, &fs::metadata(&path).unwrap(), &bytes);
        }

        fn compare(&mut self, rels: &[&str]) -> Vec<(ChangeKind, String, Detail)> {
            let batch = Batch {
                paths: rels.iter().map(|r| self.path(r)).collect(),
                rescan: false,
            };
            self.compare_batch(batch)
        }

        fn compare_batch(&mut self, batch: Batch) -> Vec<(ChangeKind, String, Detail)> {
            self.observed
                .compare(&mut self.filter, batch)
                .into_iter()
                .map(|c| (c.kind, c.display, c.detail))
                .collect()
        }
    }

    #[test]
    fn diffs_files_the_agent_has_read() {
        let mut p = Project::new();
        p.read("src/main.rs");
        p.write("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
        let changes = p.compare(&["src/main.rs"]);
        assert_eq!(changes.len(), 1);
        let (kind, path, detail) = &changes[0];
        assert_eq!(
            (*kind, path.as_str()),
            (ChangeKind::Modified, "src/main.rs")
        );
        assert_eq!(
            detail,
            &Detail::Diff {
                diff: "@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"hi\");\n+    println!(\"hello\");\n }\n"
                    .into(),
                added: 1,
                removed: 1,
            }
        );
    }

    #[test]
    fn unseen_files_report_size_then_diff_on_the_next_change() {
        let mut p = Project::new();
        p.write("src/lib.rs", "pub mod a;\npub mod b;\n");
        assert_eq!(
            p.compare(&["src/lib.rs"]),
            [(
                ChangeKind::Modified,
                "src/lib.rs".into(),
                Detail::Unseen { size: 22 }
            )]
        );
        p.write("src/lib.rs", "pub mod b;\n");
        let changes = p.compare(&["src/lib.rs"]);
        assert!(matches!(
            &changes[0].2,
            Detail::Diff { added: 0, removed: 1, diff } if diff.contains("-pub mod a;")
        ));
    }

    #[test]
    fn created_and_deleted_files() {
        let mut p = Project::new();
        p.write("src/new.rs", "// new\n");
        fs::remove_file(p.path("src/lib.rs")).unwrap();
        assert_eq!(
            p.compare(&["src/new.rs", "src/lib.rs"]),
            [
                (ChangeKind::Deleted, "src/lib.rs".into(), Detail::None),
                (
                    ChangeKind::Created,
                    "src/new.rs".into(),
                    Detail::Unseen { size: 7 }
                ),
            ]
        );
        // Both are now known: deleting the new file and recreating the old one.
        fs::remove_file(p.path("src/new.rs")).unwrap();
        p.write("src/lib.rs", "x\n");
        assert_eq!(
            p.compare(&["src/new.rs", "src/lib.rs"]),
            [
                (
                    ChangeKind::Created,
                    "src/lib.rs".into(),
                    Detail::Unseen { size: 2 }
                ),
                (ChangeKind::Deleted, "src/new.rs".into(), Detail::None),
            ]
        );
    }

    #[test]
    fn unchanged_content_and_transient_files_are_dropped() {
        let mut p = Project::new();
        p.read("src/main.rs");
        let content = fs::read(p.path("src/main.rs")).unwrap();
        p.write("src/main.rs", &content);
        // An editor's temporary file that is already gone again.
        assert_eq!(p.compare(&["src/main.rs", "src/.main.rs.swp"]), []);
        // A file never read whose metadata did not change.
        assert_eq!(p.compare(&["src/lib.rs"]), []);
    }

    #[test]
    fn binary_and_oversized_files_are_path_only() {
        let mut p = Project::new();
        p.write("logo.png", [0x89, b'P', b'N', b'G', 0, 1, 2]);
        p.write("big.txt", "x".repeat(MAX_SNAPSHOT_BYTES as usize + 1));
        assert_eq!(
            p.compare(&["logo.png", "big.txt"]),
            [
                (
                    ChangeKind::Created,
                    "big.txt".into(),
                    Detail::TooLarge {
                        size: MAX_SNAPSHOT_BYTES + 1
                    }
                ),
                (
                    ChangeKind::Created,
                    "logo.png".into(),
                    Detail::Binary { size: 7 }
                ),
            ]
        );
    }

    #[test]
    fn directories_expand_to_their_files() {
        let mut p = Project::new();
        p.write("src/net/mod.rs", "mod tcp;\n");
        p.write("src/net/tcp.rs", "\n");
        p.write("src/net/debug.log", "ignored\n");
        let changes = p.compare(&["src/net"]);
        let paths: Vec<_> = changes.iter().map(|c| (c.0, c.1.as_str())).collect();
        assert_eq!(
            paths,
            [
                (ChangeKind::Created, "src/net/mod.rs"),
                (ChangeKind::Created, "src/net/tcp.rs")
            ]
        );

        fs::remove_dir_all(p.path("src/net")).unwrap();
        let changes = p.compare(&["src/net"]);
        let paths: Vec<_> = changes.iter().map(|c| (c.0, c.1.as_str())).collect();
        assert_eq!(
            paths,
            [
                (ChangeKind::Deleted, "src/net/mod.rs"),
                (ChangeKind::Deleted, "src/net/tcp.rs")
            ]
        );
    }

    #[test]
    fn rescan_compares_the_whole_project() {
        let mut p = Project::new();
        p.read("src/main.rs");
        p.write("src/main.rs", "fn main() {}\n");
        p.write("README.md", "# hi\n");
        p.write("target/debug/out", "ignored\n");
        let changes = p.compare_batch(Batch {
            paths: BTreeSet::new(),
            rescan: true,
        });
        let paths: Vec<_> = changes.iter().map(|c| (c.0, c.1.as_str())).collect();
        assert_eq!(
            paths,
            [
                (ChangeKind::Created, "README.md"),
                (ChangeKind::Modified, "src/main.rs")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        let mut p = Project::new();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), p.path("link")).unwrap();
        assert_eq!(
            p.compare(&["link"]),
            [(ChangeKind::Created, "link".into(), Detail::Symlink)]
        );
    }
}
