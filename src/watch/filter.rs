//! Which paths in the project are worth reporting: everything except `.git`,
//! ignored files (`.gitignore`, `.ignore`, `.git/info/exclude` and the global
//! excludes file) and the shell sandbox's writable carve-outs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder, gitconfig_excludes_path};

const IGNORE_FILES: [&str; 2] = [".gitignore", ".ignore"];

#[derive(Debug)]
pub struct Filter {
    root: PathBuf,
    excluded: Vec<PathBuf>,
    repo: Gitignore,
    /// Per-directory ignore files, loaded on demand.
    dirs: HashMap<PathBuf, Gitignore>,
}

impl Filter {
    /// `root` must be canonical; `excluded` are directories (inside the root or
    /// not) whose contents are never reported.
    pub fn new(root: &Path, excluded: Vec<PathBuf>) -> Self {
        Self {
            root: root.to_owned(),
            excluded,
            repo: repo_rules(root),
            dirs: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `path` (absolute) changing could matter. Paths outside the root
    /// never do.
    pub fn is_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return true;
        };
        if rel.components().any(|c| c.as_os_str() == ".git") {
            return true;
        }
        if self.excluded.iter().any(|dir| path.starts_with(dir)) {
            return true;
        }
        // The deepest ignore file with an opinion wins, as in git.
        for dir in path.ancestors().skip(1) {
            if !dir.starts_with(&self.root) {
                break;
            }
            let rules = self
                .dirs
                .entry(dir.to_owned())
                .or_insert_with(|| dir_rules(dir));
            let matched = rules.matched_path_or_any_parents(path, is_dir);
            if matched.is_ignore() {
                return true;
            }
            if matched.is_whitelist() {
                return false;
            }
        }
        self.repo
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
    }

    /// Forgets cached rules that `path` changing may have altered. Returns
    /// whether it was an ignore file.
    pub fn invalidate(&mut self, path: &Path) -> bool {
        let Some(name) = path.file_name() else {
            return false;
        };
        if IGNORE_FILES.iter().any(|f| name == *f) {
            if let Some(dir) = path.parent() {
                self.dirs.remove(dir);
            }
            return true;
        }
        if path == self.root.join(".git/info/exclude") {
            self.repo = repo_rules(&self.root);
            return true;
        }
        false
    }

    /// A walker over `dir` that agrees with [`Self::is_ignored`] closely
    /// enough for discovering files and directories: hidden files are
    /// included, symlinks are not followed, `.git` and excluded directories
    /// are skipped.
    pub fn walk(&self, dir: &Path) -> ignore::Walk {
        let excluded = self.excluded.clone();
        WalkBuilder::new(dir)
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .filter_entry(move |entry| {
                entry.file_name() != ".git" && !excluded.iter().any(|d| entry.path().starts_with(d))
            })
            .build()
    }
}

fn dir_rules(dir: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(dir);
    for name in IGNORE_FILES {
        let file = dir.join(name);
        if file.is_file() {
            builder.add(file);
        }
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn repo_rules(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let files = gitconfig_excludes_path()
        .into_iter()
        .chain([root.join(".git/info/exclude")]);
    for file in files {
        if file.is_file() {
            builder.add(file);
        }
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::create_dir_all(root.join("src/gen")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n/build/\n").unwrap();
        fs::write(root.join("src/.gitignore"), "gen/\n!keep.log\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "scratch.txt\n").unwrap();
        (dir, root)
    }

    #[test]
    fn filters_git_ignored_and_excluded_paths() {
        let (_d, root) = project();
        let mut f = Filter::new(&root, vec![root.join("target")]);
        let ignored = |f: &mut Filter, p: &str, dir: bool| f.is_ignored(&root.join(p), dir);

        assert!(!ignored(&mut f, "src/main.rs", false));
        assert!(!ignored(&mut f, ".gitignore", false));
        assert!(!ignored(&mut f, "README.md", false));
        assert!(ignored(&mut f, ".git/index", false));
        assert!(ignored(&mut f, ".git", true));
        assert!(ignored(&mut f, "target/debug/saddle", false));
        assert!(ignored(&mut f, "target", true));
        assert!(ignored(&mut f, "logs/today.log", false));
        assert!(ignored(&mut f, "build/out.o", false));
        assert!(!ignored(&mut f, "src/build/out.o", false));
        assert!(ignored(&mut f, "src/gen/code.rs", false));
        assert!(!ignored(&mut f, "src/keep.log", false));
        assert!(ignored(&mut f, "scratch.txt", false));
        assert!(f.is_ignored(Path::new("/elsewhere/file.rs"), false));
    }

    #[test]
    fn ignore_file_changes_take_effect_after_invalidation() {
        let (_d, root) = project();
        let mut f = Filter::new(&root, vec![]);
        let notes = root.join("notes.md");
        assert!(!f.is_ignored(&notes, false));

        fs::write(root.join(".gitignore"), "*.md\n").unwrap();
        assert!(f.invalidate(&root.join(".gitignore")));
        assert!(f.is_ignored(&notes, false));

        fs::write(root.join(".git/info/exclude"), "").unwrap();
        assert!(f.invalidate(&root.join(".git/info/exclude")));
        assert!(!f.is_ignored(&root.join("scratch.txt"), false));
        assert!(!f.invalidate(&root.join("src/main.rs")));
    }

    #[test]
    fn walk_skips_git_ignored_and_excluded() {
        let (_d, root) = project();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("src/gen/code.rs"), "").unwrap();
        fs::write(root.join("target/debug/bin"), "").unwrap();
        fs::write(root.join("logs/a.log"), "").unwrap();
        let f = Filter::new(&root, vec![root.join("target")]);
        let mut seen: Vec<_> = f
            .walk(&root)
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.path().strip_prefix(&root).unwrap().to_owned())
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [
                PathBuf::from(".gitignore"),
                "src/.gitignore".into(),
                "src/main.rs".into()
            ]
        );
    }
}
