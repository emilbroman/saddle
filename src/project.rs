use std::io;
use std::path::{Path, PathBuf};

/// The nearest ancestor of `start` (inclusive) containing a `.git` entry, or
/// `start` itself outside a repository. `.git` may be a file in linked worktrees
/// and submodules; the directory holding it is still the worktree root.
pub fn resolve_root(start: &Path) -> io::Result<PathBuf> {
    let start = std::fs::canonicalize(start)?;
    let root = start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(&start)
        .to_owned();
    Ok(root)
}

/// Replaces a leading home directory with `~` for display.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    #[test]
    fn outside_a_repository_uses_launch_dir() {
        let (_guard, base) = tmp();
        let sub = base.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_root(&sub).unwrap(), sub);
    }

    #[test]
    fn inside_a_repository_uses_worktree_root() {
        let (_guard, base) = tmp();
        fs::create_dir_all(base.join("repo/.git")).unwrap();
        let sub = base.join("repo/src/deep");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_root(&sub).unwrap(), base.join("repo"));
        assert_eq!(resolve_root(&base.join("repo")).unwrap(), base.join("repo"));
    }

    #[test]
    fn linked_worktree_with_git_file() {
        let (_guard, base) = tmp();
        fs::create_dir_all(base.join("main/.git")).unwrap();
        let worktree = base.join("main/worktrees/feature");
        fs::create_dir_all(worktree.join("src")).unwrap();
        fs::write(
            worktree.join(".git"),
            "gitdir: ../../.git/worktrees/feature\n",
        )
        .unwrap();
        assert_eq!(resolve_root(&worktree.join("src")).unwrap(), worktree);
    }

    #[test]
    fn relative_and_dotted_paths_are_canonicalized() {
        let (_guard, base) = tmp();
        fs::create_dir_all(base.join("repo/.git")).unwrap();
        fs::create_dir_all(base.join("repo/src")).unwrap();
        let dotted = base.join("repo/src/../src/.");
        assert_eq!(resolve_root(&dotted).unwrap(), base.join("repo"));
    }

    #[test]
    fn missing_dir_is_an_error() {
        let (_guard, base) = tmp();
        assert!(resolve_root(&base.join("nope")).is_err());
    }

    #[test]
    fn display_abbreviates_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            display_path(Path::new("/home/u/src/x"), Some(home)),
            "~/src/x"
        );
        assert_eq!(display_path(Path::new("/home/u"), Some(home)), "~");
        assert_eq!(display_path(Path::new("/home/uu"), Some(home)), "/home/uu");
        assert_eq!(display_path(Path::new("/opt/x"), None), "/opt/x");
    }
}
