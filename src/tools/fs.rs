//! Read-only, root-confined view of the project for the agent's tools.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("{0}: no such file or directory")]
    NotFound(String),
    #[error("{0}: outside the project root")]
    OutsideRoot(String),
    #[error("{0}: {1}")]
    Io(String, String),
}

/// The project tree as tools may see it. Every path a tool touches is
/// resolved through [`ProjectFs::resolve`], which follows symlinks and
/// refuses anything that ends up outside the root.
#[derive(Debug, Clone)]
pub struct ProjectFs {
    root: PathBuf,
}

impl ProjectFs {
    pub fn new(root: &Path) -> io::Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root)?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a model-supplied path (relative to the root, or absolute
    /// inside it) to an existing canonical path inside the root.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, PathError> {
        let trimmed = path.trim();
        let requested = Path::new(if trimmed.is_empty() { "." } else { trimmed });
        let joined = self.root.join(requested);

        // Reject lexical escapes up front so that a missing target outside the
        // root reports "outside" rather than leaking whether it exists.
        if !lexically_normal(&joined).starts_with(&self.root) {
            return Err(PathError::OutsideRoot(path.to_owned()));
        }
        let canonical = fs::canonicalize(&joined).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => PathError::NotFound(path.to_owned()),
            _ => PathError::Io(path.to_owned(), e.to_string()),
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(PathError::OutsideRoot(path.to_owned()));
        }
        Ok(canonical)
    }

    /// `path` relative to the root, with `/` separators, for display to the model.
    pub fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
            Ok(rel) => rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => path.display().to_string(),
        }
    }

    /// A gitignore-aware walker over `dir` (already resolved). Hidden files are
    /// included, `.git` is not, and symlinks are never followed.
    pub fn walk(&self, dir: &Path, max_depth: Option<usize>) -> ignore::Walk {
        WalkBuilder::new(dir)
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .max_depth(max_depth)
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(|entry| entry.file_name() != ".git")
            .build()
    }
}

/// Whether a walked entry is a regular file that may be read. Symlinks are
/// excluded: their targets may lie outside the root.
pub fn is_regular_file(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|t| t.is_file())
}

fn lexically_normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, ProjectFs) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        let pfs = ProjectFs::new(&root).unwrap();
        (dir, pfs)
    }

    #[test]
    fn resolves_relative_absolute_and_root_paths() {
        let (_dir, pfs) = project();
        let main = pfs.root().join("src/main.rs");
        assert_eq!(pfs.resolve("src/main.rs").unwrap(), main);
        assert_eq!(pfs.resolve("./src/../src/main.rs").unwrap(), main);
        assert_eq!(pfs.resolve(main.to_str().unwrap()).unwrap(), main);
        assert_eq!(pfs.resolve("").unwrap(), pfs.root());
        assert_eq!(pfs.resolve(".").unwrap(), pfs.root());
    }

    #[test]
    fn rejects_traversal_and_outside_absolute_paths() {
        let (_dir, pfs) = project();
        for path in [
            "../secret.txt",
            "src/../../secret.txt",
            "../missing",
            "/etc/passwd",
            "/",
        ] {
            assert_eq!(
                pfs.resolve(path),
                Err(PathError::OutsideRoot(path.into())),
                "{path}"
            );
        }
    }

    #[test]
    fn reports_missing_files() {
        let (_dir, pfs) = project();
        assert_eq!(
            pfs.resolve("nope.rs"),
            Err(PathError::NotFound("nope.rs".into()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escapes_but_allows_internal_links() {
        use std::os::unix::fs::symlink;
        let (dir, pfs) = project();
        symlink(dir.path().join("secret.txt"), pfs.root().join("leak")).unwrap();
        symlink(dir.path(), pfs.root().join("up")).unwrap();
        symlink(pfs.root().join("src"), pfs.root().join("code")).unwrap();

        assert_eq!(
            pfs.resolve("leak"),
            Err(PathError::OutsideRoot("leak".into()))
        );
        assert_eq!(
            pfs.resolve("up/secret.txt"),
            Err(PathError::OutsideRoot("up/secret.txt".into()))
        );
        assert_eq!(
            pfs.resolve("code/main.rs").unwrap(),
            pfs.root().join("src/main.rs")
        );
    }

    #[test]
    fn display_is_root_relative() {
        let (_dir, pfs) = project();
        assert_eq!(pfs.display(&pfs.root().join("src/main.rs")), "src/main.rs");
        assert_eq!(pfs.display(pfs.root()), ".");
    }
}
