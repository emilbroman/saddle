use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::watch::Observed;

use super::{MAX_OUTPUT_BYTES, ProjectFs, Tool, ToolOutput, clip_line, looks_binary, str_arg};

pub const MAX_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub struct ReadFile {
    fs: ProjectFs,
    observed: Observed,
}

impl ReadFile {
    pub fn new(fs: ProjectFs, observed: Observed) -> Self {
        Self { fs, observed }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a text file in the project. Lines are prefixed with their 1-based line number and a tab. \
         Reads at most 2000 lines per call; use start_line/end_line to page through longer files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the project root." },
                "start_line": { "type": "integer", "minimum": 1, "description": "First line to read (1-based, default 1)." },
                "end_line": { "type": "integer", "minimum": 1, "description": "Last line to read, inclusive." }
            },
            "required": ["path"]
        })
    }

    fn summary(&self, args: &Value) -> String {
        let path = str_arg(args, "path").unwrap_or("?");
        let start = args.get("start_line").and_then(Value::as_u64);
        let end = args.get("end_line").and_then(Value::as_u64);
        match (start, end) {
            (None, None) => path.to_owned(),
            (start, end) => format!(
                "{path}:{}-{}",
                start.unwrap_or(1),
                end.map(|e| e.to_string()).unwrap_or_default()
            ),
        }
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        let fs = self.fs.clone();
        let observed = self.observed.clone();
        Box::pin(async move {
            let args: Args = match super::parse_args(args) {
                Ok(args) => args,
                Err(e) => return e,
            };
            tokio::task::spawn_blocking(move || read(&fs, &observed, args))
                .await
                .unwrap_or_else(|e| ToolOutput::error(format!("read_file failed: {e}")))
        })
    }
}

fn read(fs: &ProjectFs, observed: &Observed, args: Args) -> ToolOutput {
    let path = match fs.resolve(&args.path) {
        Ok(path) => path,
        Err(e) => return ToolOutput::error(e.to_string()),
    };
    let shown = fs.display(&path);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) => return ToolOutput::error(format!("{shown}: {e}")),
    };
    if meta.is_dir() {
        return ToolOutput::error(format!(
            "{shown} is a directory; use the list tool to see its contents"
        ));
    }
    if meta.len() > MAX_FILE_BYTES {
        return ToolOutput::error(format!(
            "{shown} is {} bytes, too large to read; use grep to find the relevant part",
            meta.len()
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return ToolOutput::error(format!("{shown}: {e}")),
    };
    observed.record_read(&path, &meta, &bytes);
    if looks_binary(&bytes) {
        return ToolOutput::ok(format!(
            "{shown} is a binary file ({} bytes); not shown",
            bytes.len()
        ));
    }
    let text = String::from_utf8_lossy(&bytes);
    if text.is_empty() {
        return ToolOutput::ok(format!("{shown} is empty"));
    }

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = args.start_line.unwrap_or(1).max(1);
    if start > total {
        return ToolOutput::error(format!(
            "start_line {start} is past the end of {shown} ({total} lines)"
        ));
    }
    let requested_end = args.end_line.unwrap_or(total).clamp(start, total);
    let end = requested_end.min(start + MAX_LINES - 1);

    // Leave room for the notice so the registry's hard cap never cuts it off.
    let budget = MAX_OUTPUT_BYTES - 256;
    let mut out = String::new();
    let mut last = start - 1;
    for (index, line) in lines[start - 1..end].iter().enumerate() {
        let number = start + index;
        let row = format!("{number}\t{}\n", clip_line(line, MAX_LINE_CHARS));
        if out.len() + row.len() > budget {
            break;
        }
        out.push_str(&row);
        last = number;
    }

    if last < requested_end || (args.end_line.is_none() && last < total) {
        out.push_str(&format!(
            "[showing lines {start}-{last} of {total}; call again with start_line={} to continue]",
            last + 1
        ));
    } else {
        out.pop();
    }
    ToolOutput::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(content: &str) -> (tempfile::TempDir, ReadFile) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), content).unwrap();
        let tool = ReadFile::new(ProjectFs::new(dir.path()).unwrap(), Observed::default());
        (dir, tool)
    }

    async fn run(tool: &ReadFile, args: Value) -> ToolOutput {
        tool.call(args).await
    }

    #[tokio::test]
    async fn reads_numbered_lines() {
        let (_d, tool) = setup("one\ntwo\nthree\n");
        let out = run(&tool, json!({"path": "f.txt"})).await;
        assert_eq!(out, ToolOutput::ok("1\tone\n2\ttwo\n3\tthree"));
    }

    #[tokio::test]
    async fn reads_a_range() {
        let (_d, tool) = setup("one\ntwo\nthree\nfour\n");
        let out = run(
            &tool,
            json!({"path": "f.txt", "start_line": 2, "end_line": 3}),
        )
        .await;
        assert_eq!(out, ToolOutput::ok("2\ttwo\n3\tthree"));
        let out = run(&tool, json!({"path": "f.txt", "start_line": 9})).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn caps_line_count_with_notice() {
        let content: String = (1..=MAX_LINES + 10).map(|i| format!("{i}\n")).collect();
        let (_d, tool) = setup(&content);
        let out = run(&tool, json!({"path": "f.txt"})).await;
        assert!(!out.is_error);
        let last = out.content.lines().last().unwrap();
        assert_eq!(
            last,
            format!(
                "[showing lines 1-{MAX_LINES} of {}; call again with start_line={} to continue]",
                MAX_LINES + 10,
                MAX_LINES + 1
            )
        );
    }

    #[tokio::test]
    async fn caps_bytes_with_notice() {
        let line = "x".repeat(1000);
        let content: String = (0..100).map(|_| format!("{line}\n")).collect();
        let (_d, tool) = setup(&content);
        let out = run(&tool, json!({"path": "f.txt"})).await;
        assert!(out.content.len() <= MAX_OUTPUT_BYTES);
        assert!(
            out.content.ends_with("to continue]"),
            "{}",
            &out.content[out.content.len() - 100..]
        );
        assert!(out.content.contains("of 100;"));
    }

    #[tokio::test]
    async fn clips_long_lines() {
        let (_d, tool) = setup(&"y".repeat(MAX_LINE_CHARS + 5));
        let out = run(&tool, json!({"path": "f.txt"})).await;
        assert!(out.content.ends_with("… [line truncated]"));
    }

    #[tokio::test]
    async fn refuses_directories_binaries_and_escapes() {
        let (dir, tool) = setup("");
        std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2]).unwrap();
        assert!(run(&tool, json!({"path": "."})).await.is_error);
        assert!(
            run(&tool, json!({"path": "b.bin"}))
                .await
                .content
                .contains("binary")
        );
        assert_eq!(
            run(&tool, json!({"path": "f.txt"})).await.content,
            "f.txt is empty"
        );
        let out = run(&tool, json!({"path": "../etc/passwd"})).await;
        assert!(out.is_error && out.content.contains("outside the project root"));
        assert!(run(&tool, json!({})).await.is_error);
    }
}
