use futures::future::BoxFuture;
use globset::GlobBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use super::fs::is_regular_file;
use super::{ProjectFs, Tool, ToolOutput, str_arg};

const MAX_ENTRIES: usize = 1000;

/// Lists one directory level.
pub struct List {
    fs: ProjectFs,
}

impl List {
    pub fn new(fs: ProjectFs) -> Self {
        Self { fs }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    path: String,
}

impl Tool for List {
    fn name(&self) -> &'static str {
        "list"
    }

    fn description(&self) -> &'static str {
        "List the entries of a directory in the project (one level, not recursive). \
         Directories end in `/`, symlinks in `@`. Files ignored by .gitignore are omitted."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory relative to the project root (default: the root)." }
            }
        })
    }

    fn summary(&self, args: &Value) -> String {
        str_arg(args, "path")
            .filter(|p| !p.is_empty())
            .unwrap_or(".")
            .to_owned()
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        let fs = self.fs.clone();
        Box::pin(async move {
            let args: ListArgs = match super::parse_args(args) {
                Ok(args) => args,
                Err(e) => return e,
            };
            tokio::task::spawn_blocking(move || list(&fs, &args.path))
                .await
                .unwrap_or_else(|e| ToolOutput::error(format!("list failed: {e}")))
        })
    }
}

fn list(fs: &ProjectFs, path: &str) -> ToolOutput {
    let dir = match fs.resolve(path) {
        Ok(dir) => dir,
        Err(e) => return ToolOutput::error(e.to_string()),
    };
    let shown = fs.display(&dir);
    if !dir.is_dir() {
        return ToolOutput::error(format!(
            "{shown} is not a directory; use read_file to read it"
        ));
    }

    let mut entries = Vec::new();
    for entry in fs.walk(&dir, Some(1)).flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let suffix = match entry.file_type() {
            Some(t) if t.is_dir() => "/",
            Some(t) if t.is_symlink() => "@",
            _ => "",
        };
        entries.push(format!("{name}{suffix}"));
    }
    if entries.is_empty() {
        return ToolOutput::ok(format!("{shown} is empty (or everything in it is ignored)"));
    }
    ToolOutput::ok(capped(entries, "entries"))
}

/// Finds files by glob pattern, recursively.
pub struct Glob {
    fs: ProjectFs,
}

impl Glob {
    pub fn new(fs: ProjectFs) -> Self {
        Self { fs }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: String,
}

impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files in the project by glob pattern, recursively. A pattern without `/` matches file names \
         anywhere (e.g. `*.rs`, `Cargo.toml`); a pattern with `/` matches paths relative to `path` \
         (e.g. `src/**/*.rs`; `*` does not cross `/`, `**` does). Returns matching file paths relative \
         to the project root. Files ignored by .gitignore are omitted."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. `*.rs` or `src/**/mod.rs`." },
                "path": { "type": "string", "description": "Directory to search, relative to the project root (default: the root)." }
            },
            "required": ["pattern"]
        })
    }

    fn summary(&self, args: &Value) -> String {
        let pattern = str_arg(args, "pattern").unwrap_or("?");
        match str_arg(args, "path").filter(|p| !p.is_empty()) {
            Some(path) => format!("{pattern} in {path}"),
            None => pattern.to_owned(),
        }
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        let fs = self.fs.clone();
        Box::pin(async move {
            let args: GlobArgs = match super::parse_args(args) {
                Ok(args) => args,
                Err(e) => return e,
            };
            tokio::task::spawn_blocking(move || glob(&fs, &args))
                .await
                .unwrap_or_else(|e| ToolOutput::error(format!("glob failed: {e}")))
        })
    }
}

fn glob(fs: &ProjectFs, args: &GlobArgs) -> ToolOutput {
    let pattern = args.pattern.trim().trim_start_matches("./");
    let by_name = !pattern.contains('/');
    let matcher = match GlobBuilder::new(pattern).literal_separator(true).build() {
        Ok(glob) => glob.compile_matcher(),
        Err(e) => return ToolOutput::error(format!("invalid glob pattern: {e}")),
    };
    let dir = match fs.resolve(&args.path) {
        Ok(dir) => dir,
        Err(e) => return ToolOutput::error(e.to_string()),
    };
    if !dir.is_dir() {
        return ToolOutput::error(format!("{} is not a directory", fs.display(&dir)));
    }

    let mut matches = Vec::new();
    for entry in fs.walk(&dir, None).flatten() {
        if !is_regular_file(&entry) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&dir) else {
            continue;
        };
        let hit = if by_name {
            matcher.is_match(entry.file_name())
        } else {
            matcher.is_match(relative)
        };
        if hit {
            matches.push(fs.display(entry.path()));
        }
    }
    if matches.is_empty() {
        return ToolOutput::ok(format!("no files match {pattern:?}"));
    }
    ToolOutput::ok(capped(matches, "matches"))
}

fn capped(mut lines: Vec<String>, what: &str) -> String {
    let total = lines.len();
    lines.truncate(MAX_ENTRIES);
    let mut out = lines.join("\n");
    if total > MAX_ENTRIES {
        out.push_str(&format!(
            "\n[{} more {what} not shown; narrow the search]",
            total - MAX_ENTRIES
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project() -> (tempfile::TempDir, ProjectFs) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        for d in ["src/tools", "target/debug", ".git", ".github"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in [
            "Cargo.toml",
            "src/main.rs",
            "src/tools/mod.rs",
            "target/debug/out.rs",
            ".git/HEAD",
            ".github/ci.yml",
            "notes.md",
        ] {
            fs::write(root.join(f), "x").unwrap();
        }
        fs::write(root.join(".gitignore"), "/target\n").unwrap();
        fs::write(dir.path().join("outside.rs"), "x").unwrap();
        let pfs = ProjectFs::new(&root).unwrap();
        (dir, pfs)
    }

    #[tokio::test]
    async fn list_shows_one_level_without_ignored_or_git() {
        let (_d, pfs) = project();
        let out = List::new(pfs).call(json!({})).await;
        assert_eq!(
            out.content,
            ".github/\n.gitignore\nCargo.toml\nnotes.md\nsrc/"
        );
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn list_subdirectory_and_errors() {
        let (_d, pfs) = project();
        let tool = List::new(pfs);
        assert_eq!(
            tool.call(json!({"path": "src"})).await.content,
            "main.rs\ntools/"
        );
        assert!(tool.call(json!({"path": "Cargo.toml"})).await.is_error);
        assert!(tool.call(json!({"path": ".."})).await.is_error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_marks_symlinks_and_glob_skips_them() {
        let (d, pfs) = project();
        std::os::unix::fs::symlink(d.path().join("outside.rs"), pfs.root().join("link.rs"))
            .unwrap();
        let out = List::new(pfs.clone()).call(json!({})).await;
        assert!(out.content.contains("link.rs@"));
        let out = Glob::new(pfs).call(json!({"pattern": "*.rs"})).await;
        assert!(!out.content.contains("link.rs"));
    }

    #[tokio::test]
    async fn glob_by_name_and_by_path() {
        let (_d, pfs) = project();
        let tool = Glob::new(pfs);
        assert_eq!(
            tool.call(json!({"pattern": "*.rs"})).await.content,
            "src/main.rs\nsrc/tools/mod.rs"
        );
        assert_eq!(
            tool.call(json!({"pattern": "src/*.rs"})).await.content,
            "src/main.rs"
        );
        assert_eq!(
            tool.call(json!({"pattern": "**/mod.rs"})).await.content,
            "src/tools/mod.rs"
        );
        assert_eq!(
            tool.call(json!({"pattern": "*.rs", "path": "src/tools"}))
                .await
                .content,
            "src/tools/mod.rs"
        );
        assert_eq!(
            tool.call(json!({"pattern": "*.yml"})).await.content,
            ".github/ci.yml"
        );
        assert!(
            tool.call(json!({"pattern": "*.py"}))
                .await
                .content
                .starts_with("no files")
        );
        assert!(tool.call(json!({"pattern": "a{"})).await.is_error);
        assert!(
            tool.call(json!({"pattern": "*", "path": "../"}))
                .await
                .is_error
        );
    }

    #[test]
    fn capped_reports_remainder() {
        let lines: Vec<String> = (0..MAX_ENTRIES + 3).map(|i| i.to_string()).collect();
        let out = capped(lines, "entries");
        assert!(out.ends_with("[3 more entries not shown; narrow the search]"));
    }
}
