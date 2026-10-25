use std::path::Path;

use futures::future::BoxFuture;
use globset::GlobBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use super::fs::is_regular_file;
use super::{ProjectFs, Tool, ToolOutput, clip_line, looks_binary, str_arg};

pub const MAX_MATCHES: usize = 200;
const MAX_LINE_CHARS: usize = 300;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub struct Grep {
    fs: ProjectFs,
}

impl Grep {
    pub fn new(fs: ProjectFs) -> Self {
        Self { fs }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: String,
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
}

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents in the project with a regular expression (Rust regex syntax, matched per line). \
         Returns `path:line:text` for each matching line, at most 200 matches. Files ignored by .gitignore \
         and binary files are skipped."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "File or directory to search, relative to the project root (default: the root)." },
                "glob": { "type": "string", "description": "Only search files whose name matches this glob, e.g. `*.rs`." },
                "case_insensitive": { "type": "boolean", "description": "Match case-insensitively (default false)." }
            },
            "required": ["pattern"]
        })
    }

    fn summary(&self, args: &Value) -> String {
        let mut summary = format!("/{}/", str_arg(args, "pattern").unwrap_or("?"));
        if let Some(path) = str_arg(args, "path").filter(|p| !p.is_empty()) {
            summary.push_str(&format!(" in {path}"));
        }
        if let Some(glob) = str_arg(args, "glob") {
            summary.push_str(&format!(" ({glob})"));
        }
        summary
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        let fs = self.fs.clone();
        Box::pin(async move {
            let args: Args = match super::parse_args(args) {
                Ok(args) => args,
                Err(e) => return e,
            };
            tokio::task::spawn_blocking(move || grep(&fs, &args))
                .await
                .unwrap_or_else(|e| ToolOutput::error(format!("grep failed: {e}")))
        })
    }
}

fn grep(fs: &ProjectFs, args: &Args) -> ToolOutput {
    let regex = match RegexBuilder::new(&args.pattern)
        .case_insensitive(args.case_insensitive)
        .size_limit(10 * 1024 * 1024)
        .build()
    {
        Ok(regex) => regex,
        Err(e) => return ToolOutput::error(format!("invalid regex: {e}")),
    };
    let name_filter = match &args.glob {
        Some(glob) => match GlobBuilder::new(glob).literal_separator(true).build() {
            Ok(glob) => Some(glob.compile_matcher()),
            Err(e) => return ToolOutput::error(format!("invalid glob: {e}")),
        },
        None => None,
    };
    let start = match fs.resolve(&args.path) {
        Ok(start) => start,
        Err(e) => return ToolOutput::error(e.to_string()),
    };

    let mut out = Vec::new();
    let mut total = 0usize;
    let mut search = |path: &Path| {
        if let Some(filter) = &name_filter
            && !path.file_name().is_some_and(|n| filter.is_match(n))
        {
            return;
        }
        if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            return;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        if looks_binary(&bytes) {
            return;
        }
        let text = String::from_utf8_lossy(&bytes);
        let shown = fs.display(path);
        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                total += 1;
                if out.len() < MAX_MATCHES {
                    out.push(format!(
                        "{shown}:{}:{}",
                        index + 1,
                        clip_line(line, MAX_LINE_CHARS)
                    ));
                }
            }
        }
    };

    if start.is_file() {
        search(&start);
    } else {
        for entry in fs.walk(&start, None).flatten() {
            if is_regular_file(&entry) {
                search(entry.path());
            }
        }
    }

    if total == 0 {
        return ToolOutput::ok("no matches");
    }
    let mut result = out.join("\n");
    if total > MAX_MATCHES {
        result.push_str(&format!(
            "\n[{} more matches not shown; narrow the pattern, path or glob]",
            total - MAX_MATCHES
        ));
    }
    ToolOutput::ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project() -> (tempfile::TempDir, Grep) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::write(root.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        fs::write(root.join("src/b.txt"), "Alpha in prose\n").unwrap();
        fs::write(root.join("target/gen.rs"), "fn alpha_generated() {}\n").unwrap();
        fs::write(root.join("bin.dat"), b"alpha\0").unwrap();
        fs::write(dir.path().join("outside.rs"), "fn alpha_outside() {}\n").unwrap();
        let tool = Grep::new(ProjectFs::new(&root).unwrap());
        (dir, tool)
    }

    #[tokio::test]
    async fn finds_matches_skipping_ignored_and_binary() {
        let (_d, tool) = project();
        let out = tool.call(json!({"pattern": "alpha"})).await;
        assert_eq!(out, ToolOutput::ok("src/a.rs:1:fn alpha() {}"));
    }

    #[tokio::test]
    async fn options_narrow_or_widen() {
        let (_d, tool) = project();
        let out = tool
            .call(json!({"pattern": "alpha", "case_insensitive": true}))
            .await;
        assert_eq!(
            out.content,
            "src/a.rs:1:fn alpha() {}\nsrc/b.txt:1:Alpha in prose"
        );
        let out = tool
            .call(json!({"pattern": "alpha", "case_insensitive": true, "glob": "*.txt"}))
            .await;
        assert_eq!(out.content, "src/b.txt:1:Alpha in prose");
        let out = tool
            .call(json!({"pattern": "fn", "path": "src/a.rs"}))
            .await;
        assert_eq!(
            out.content,
            "src/a.rs:1:fn alpha() {}\nsrc/a.rs:2:fn beta() {}"
        );
        assert_eq!(
            tool.call(json!({"pattern": "zeta"})).await.content,
            "no matches"
        );
    }

    #[tokio::test]
    async fn rejects_bad_input_and_escapes() {
        let (_d, tool) = project();
        assert!(tool.call(json!({"pattern": "("})).await.is_error);
        let out = tool
            .call(json!({"pattern": "alpha", "path": "../outside.rs"}))
            .await;
        assert!(out.is_error && out.content.contains("outside the project root"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_symlinks_out_of_the_root() {
        let (d, tool) = project();
        std::os::unix::fs::symlink(d.path().join("outside.rs"), d.path().join("p/src/link.rs"))
            .unwrap();
        std::os::unix::fs::symlink(d.path(), d.path().join("p/up")).unwrap();
        let out = tool.call(json!({"pattern": "outside"})).await;
        assert_eq!(out.content, "no matches");
    }

    #[tokio::test]
    async fn caps_matches() {
        let (d, tool) = project();
        let many: String = (0..MAX_MATCHES + 5).map(|i| format!("hit {i}\n")).collect();
        fs::write(d.path().join("p/many.txt"), many).unwrap();
        let out = tool.call(json!({"pattern": "^hit"})).await;
        assert_eq!(out.content.lines().count(), MAX_MATCHES + 1);
        assert!(
            out.content
                .ends_with("[5 more matches not shown; narrow the pattern, path or glob]")
        );
    }
}
