//! Read-only language intelligence with 1-based Unicode-scalar positions.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::lsp::{Diagnostic, Location, LspManager, ServerState, Severity, Symbol, TextPosition};

use super::{
    MAX_OUTPUT_BYTES, ProjectFs, Registry, Tool, ToolOutput, clip_line, parse_args, str_arg,
};

const MAX_RESULTS: usize = 200;
const MAX_TEXT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DETAIL_CHARS: usize = 240;

pub fn install(registry: &mut Registry, manager: LspManager, fs: ProjectFs) {
    for operation in [
        Operation::WorkspaceSymbols,
        Operation::DocumentSymbols,
        Operation::Definition,
        Operation::References,
        Operation::Hover,
        Operation::Diagnostics,
    ] {
        registry.register(LspTool {
            operation,
            manager: manager.clone(),
            fs: fs.clone(),
        });
    }
}

#[derive(Clone, Copy)]
enum Operation {
    WorkspaceSymbols,
    DocumentSymbols,
    Definition,
    References,
    Hover,
    Diagnostics,
}

#[derive(Clone)]
struct LspTool {
    operation: Operation,
    manager: LspManager,
    fs: ProjectFs,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryArgs {
    query: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionArgs {
    path: String,
    line: u32,
    col: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceArgs {
    path: String,
    line: u32,
    col: u32,
    #[serde(default = "include_declaration")]
    include_declaration: bool,
}

fn include_declaration() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticsArgs {
    path: Option<String>,
}

impl Tool for LspTool {
    fn name(&self) -> &'static str {
        match self.operation {
            Operation::WorkspaceSymbols => "lsp_workspace_symbols",
            Operation::DocumentSymbols => "lsp_document_symbols",
            Operation::Definition => "lsp_definition",
            Operation::References => "lsp_references",
            Operation::Hover => "lsp_hover",
            Operation::Diagnostics => "lsp_diagnostics",
        }
    }

    fn description(&self) -> &'static str {
        match self.operation {
            Operation::WorkspaceSymbols => {
                "Search symbols across the project's language servers. Returns bounded path:line:col results with symbol names and signatures."
            }
            Operation::DocumentSymbols => {
                "List a file's symbol outline, with nesting and signatures. Locations use 1-based lines and Unicode-scalar columns."
            }
            Operation::Definition => {
                "Find definitions at path:line:col. Lines and columns are 1-based; columns count Unicode characters, not bytes or UTF-16 units. External targets are labeled."
            }
            Operation::References => {
                "Find references at path:line:col. Lines and Unicode-scalar columns are 1-based. Includes the declaration by default."
            }
            Operation::Hover => {
                "Get type information or documentation at path:line:col. Lines and Unicode-scalar columns are 1-based. Uses the file saved on disk."
            }
            Operation::Diagnostics => {
                "Show current language-server diagnostics grouped by severity, optionally for one path. Opens and syncs a requested file from disk; workspace results cover only diagnostics published by active servers, not a full-project check."
            }
        }
    }

    fn schema(&self) -> Value {
        let path =
            json!({"type": "string", "description": "File path relative to the project root."});
        let position = json!({"type": "integer", "minimum": 1, "maximum": u32::MAX});
        let mut schema = match self.operation {
            Operation::WorkspaceSymbols => json!({
                "properties": {"query": {"type": "string"}}, "required": ["query"]
            }),
            Operation::DocumentSymbols => {
                json!({"properties": {"path": path}, "required": ["path"]})
            }
            Operation::Diagnostics => json!({"properties": {"path": path}}),
            Operation::Definition | Operation::Hover | Operation::References => json!({
                "properties": {"path": path, "line": position, "col": position},
                "required": ["path", "line", "col"]
            }),
        };
        if matches!(self.operation, Operation::References) {
            schema["properties"]["include_declaration"] =
                json!({"type": "boolean", "default": true});
        }
        schema["type"] = json!("object");
        schema["additionalProperties"] = json!(false);
        schema
    }

    fn summary(&self, args: &Value) -> String {
        if matches!(self.operation, Operation::WorkspaceSymbols) {
            return str_arg(args, "query").unwrap_or("?").to_owned();
        }
        let path = str_arg(args, "path").unwrap_or("workspace");
        match (args.get("line"), args.get("col")) {
            (Some(line), Some(col)) => format!("{path}:{line}:{col}"),
            _ => path.to_owned(),
        }
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        let tool = self.clone();
        Box::pin(async move {
            match tool.execute(args).await {
                Ok(content) => ToolOutput::ok(bounded_text(&content)),
                Err(error) => ToolOutput::error(bounded_text(&error)),
            }
        })
    }
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    parse_args(value).map_err(|e| e.content)
}

impl LspTool {
    async fn execute(&self, value: Value) -> Result<String, String> {
        match self.operation {
            Operation::WorkspaceSymbols => {
                let args: QueryArgs = args(value)?;
                if !self.manager.has_servers_configured() {
                    return Err("no language servers are configured for this project".into());
                }
                let symbols = self
                    .manager
                    .workspace_symbols(&args.query)
                    .await
                    .map_err(|e| e.to_string())?;
                if self.manager.statuses().is_empty() {
                    return Err("no language server is active for the project's files".into());
                }
                let fs = self.fs.clone();
                blocking(move || render_symbols(&fs, &symbols)).await
            }
            Operation::DocumentSymbols => {
                let args: PathArgs = args(value)?;
                let path = self.fs.resolve(&args.path).map_err(|e| e.to_string())?;
                let symbols = self
                    .manager
                    .document_symbols(&path)
                    .await
                    .map_err(|e| e.to_string())?;
                let fs = self.fs.clone();
                blocking(move || render_symbols(&fs, &symbols)).await
            }
            Operation::Definition | Operation::References | Operation::Hover => {
                let (args, include_declaration) = if matches!(self.operation, Operation::References)
                {
                    let args: ReferenceArgs = args(value)?;
                    (
                        PositionArgs {
                            path: args.path,
                            line: args.line,
                            col: args.col,
                        },
                        args.include_declaration,
                    )
                } else {
                    (args::<PositionArgs>(value)?, true)
                };
                let fs = self.fs.clone();
                let (path, position) =
                    tokio::task::spawn_blocking(move || resolve_position(&fs, args))
                        .await
                        .map_err(|e| e.to_string())??;
                if matches!(self.operation, Operation::Hover) {
                    let hover = self
                        .manager
                        .hover(&path, position)
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(format!(
                        "{}\n{}",
                        location_label(&self.fs, &path, position),
                        hover.as_deref().unwrap_or("No hover information.")
                    ));
                }
                let locations = if matches!(self.operation, Operation::Definition) {
                    self.manager.definition(&path, position).await
                } else {
                    self.manager
                        .references(&path, position, include_declaration)
                        .await
                }
                .map_err(|e| e.to_string())?;
                let fs = self.fs.clone();
                blocking(move || render_locations(&fs, &locations)).await
            }
            Operation::Diagnostics => {
                let args: DiagnosticsArgs = args(value)?;
                let path = args
                    .path
                    .as_deref()
                    .map(|p| self.fs.resolve(p))
                    .transpose()
                    .map_err(|e| e.to_string())?;
                if let Some(path) = &path {
                    self.manager
                        .sync_document(path)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                let statuses = self.manager.statuses();
                if statuses.is_empty() {
                    return Err("no language server is active; request diagnostics for a file to start its configured server".into());
                }
                if !statuses
                    .iter()
                    .any(|s| matches!(s.state, ServerState::Ready | ServerState::Busy(_)))
                {
                    return Err(format!(
                        "language servers are not ready: {}",
                        statuses
                            .iter()
                            .map(|s| format!("{} {:?}", s.language, s.state))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let settled = self
                    .manager
                    .settle(Duration::from_millis(150), Duration::from_secs(2))
                    .await;
                let diagnostics = match path {
                    Some(path) => self.manager.diagnostics_for(&[path]),
                    None => self.manager.diagnostics(),
                };
                let fs = self.fs.clone();
                blocking(move || {
                    let mut text = render_diagnostics(&fs, &diagnostics);
                    text.push_str(if settled {
                        "\n[Current published diagnostics; not a full-project check.]"
                    } else {
                        "\n[Diagnostics are still updating; this snapshot may be incomplete.]"
                    });
                    text
                })
                .await
            }
        }
    }
}

async fn blocking(f: impl FnOnce() -> String + Send + 'static) -> Result<String, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())
}

fn resolve_position(fs: &ProjectFs, args: PositionArgs) -> Result<(PathBuf, TextPosition), String> {
    let position = TextPosition {
        line: args.line.checked_sub(1).ok_or("line must be at least 1")?,
        column: args.col.checked_sub(1).ok_or("col must be at least 1")?,
    };
    let path = fs.resolve(&args.path).map_err(|e| e.to_string())?;
    let text = read_text(&path)?;
    let line = text
        .split('\n')
        .nth(position.line as usize)
        .ok_or_else(|| {
            format!(
                "line {} is past the end of {}",
                args.line,
                fs.display(&path)
            )
        })?;
    let line = line.strip_suffix('\r').unwrap_or(line);
    if position.column as usize > line.chars().count() {
        return Err(format!(
            "col {} is past the end of line {} in {} (columns count Unicode characters)",
            args.col,
            args.line,
            fs.display(&path)
        ));
    }
    Ok((path, position))
}

fn read_text(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut text = String::new();
    file.take(MAX_TEXT_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_TEXT_BYTES {
        return Err(format!(
            "{} is too large for language-server tools",
            path.display()
        ));
    }
    Ok(text)
}

fn location_label(fs: &ProjectFs, path: &Path, position: TextPosition) -> String {
    let external = !path.starts_with(fs.root())
        || matches!(
            fs.resolve(&path.to_string_lossy()),
            Err(super::fs::PathError::OutsideRoot(_))
        );
    let shown = if external {
        path.display().to_string()
    } else {
        fs.display(path)
    };
    format!(
        "{shown}:{}:{}{}",
        u64::from(position.line) + 1,
        u64::from(position.column) + 1,
        if external { " [external]" } else { "" }
    )
}

struct Render<'a> {
    fs: &'a ProjectFs,
    texts: HashMap<PathBuf, Option<String>>,
    output: String,
    count: usize,
}

impl<'a> Render<'a> {
    fn new(fs: &'a ProjectFs) -> Self {
        Self {
            fs,
            texts: HashMap::new(),
            output: String::new(),
            count: 0,
        }
    }

    fn snippet(&mut self, location: &Location) -> String {
        let Ok(path) = self.fs.resolve(&location.path.to_string_lossy()) else {
            return String::new();
        };
        let text = self
            .texts
            .entry(path.clone())
            .or_insert_with(|| read_text(&path).ok());
        text.as_deref()
            .and_then(|t| t.split('\n').nth(location.range.start.line as usize))
            .map(|line| compact(line.trim()))
            .unwrap_or_default()
    }

    fn push(&mut self, row: &str) -> bool {
        if self.count >= MAX_RESULTS || self.output.len() + row.len() > MAX_OUTPUT_BYTES - 512 {
            return false;
        }
        self.output.push_str(row);
        self.output.push('\n');
        self.count += 1;
        true
    }

    fn finish(mut self, total: usize, empty: &str) -> String {
        if self.count < total {
            self.output.push_str(&format!(
                "[{} more results not shown; narrow the query or path]\n",
                total - self.count
            ));
        }
        if self.output.is_empty() {
            empty.into()
        } else {
            self.output.trim_end().into()
        }
    }
}

fn compact(text: &str) -> String {
    clip_line(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_DETAIL_CHARS,
    )
}

fn render_locations(fs: &ProjectFs, locations: &[Location]) -> String {
    let mut render = Render::new(fs);
    for location in locations {
        let snippet = render.snippet(location);
        let row = format!(
            "{}  {snippet}",
            location_label(fs, &location.path, location.range.start)
        );
        if !render.push(row.trim_end()) {
            break;
        }
    }
    render.finish(locations.len(), "No locations found.")
}

fn render_symbols(fs: &ProjectFs, symbols: &[Symbol]) -> String {
    let mut render = Render::new(fs);
    for symbol in symbols {
        let detail = symbol
            .detail
            .as_deref()
            .map(compact)
            .unwrap_or_else(|| render.snippet(&symbol.location));
        let container = symbol
            .container
            .as_deref()
            .map(|c| format!(" in {}", compact(c)))
            .unwrap_or_default();
        let row = format!(
            "{}  {}{} ({}){container}  {detail}",
            location_label(fs, &symbol.location.path, symbol.location.range.start),
            "  ".repeat(symbol.depth.min(12)),
            compact(&symbol.name),
            symbol.kind
        );
        if !render.push(row.trim_end()) {
            break;
        }
    }
    render.finish(symbols.len(), "No symbols found.")
}

fn render_diagnostics(fs: &ProjectFs, diagnostics: &BTreeMap<PathBuf, Vec<Diagnostic>>) -> String {
    let mut render = Render::new(fs);
    let total = diagnostics.values().map(Vec::len).sum();
    'groups: for (path, file_diagnostics) in diagnostics {
        if file_diagnostics.is_empty() {
            continue;
        }
        if render.output.len() + path.as_os_str().len() > MAX_OUTPUT_BYTES - 512 {
            break;
        }
        render.output.push_str(&format!("{}\n", fs.display(path)));
        for (severity, label) in [
            (Severity::Error, "Errors"),
            (Severity::Warning, "Warnings"),
            (Severity::Information, "Information"),
            (Severity::Hint, "Hints"),
        ] {
            let entries: Vec<_> = file_diagnostics
                .iter()
                .filter(|d| d.severity == severity)
                .collect();
            if entries.is_empty() {
                continue;
            }
            render
                .output
                .push_str(&format!("{label} ({})\n", entries.len()));
            for diagnostic in entries {
                let source = diagnostic.source.as_deref().unwrap_or("");
                let code = diagnostic.code.as_deref().unwrap_or("");
                let origin = compact(&format!("{source} {code}"));
                let origin = if origin.is_empty() {
                    origin
                } else {
                    format!(" [{origin}]")
                };
                let row = format!(
                    "{}  {}{origin}",
                    location_label(fs, &diagnostic.path, diagnostic.range.start),
                    compact(&diagnostic.message)
                );
                if !render.push(&row) {
                    break 'groups;
                }
            }
        }
    }
    render.finish(total, "No published diagnostics.")
}

fn bounded_text(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.into();
    }
    let mut end = MAX_OUTPUT_BYTES - 64;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::TextRange;

    fn location(path: PathBuf, line: u32, column: u32) -> Location {
        let position = TextPosition { line, column };
        Location {
            path,
            range: TextRange {
                start: position,
                end: position,
            },
        }
    }

    #[test]
    fn validates_one_based_unicode_positions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "€😀 x\r\n").unwrap();
        let fs = ProjectFs::new(dir.path()).unwrap();
        let parse = |line, col| {
            resolve_position(
                &fs,
                PositionArgs {
                    path: "a.txt".into(),
                    line,
                    col,
                },
            )
        };
        assert_eq!(parse(1, 4).unwrap().1, TextPosition { line: 0, column: 3 });
        assert!(parse(1, 5).is_ok());
        for (line, col) in [(0, 1), (1, 0), (1, 6), (3, 1), (u32::MAX, 1)] {
            assert!(parse(line, col).is_err());
        }
        assert!(
            args::<PositionArgs>(json!({"path": "a.txt", "line": 4294967296u64, "col": 1}))
                .is_err()
        );
        assert!(args::<PositionArgs>(json!({"path": "a.txt", "line": -1, "col": 1})).is_err());
        assert!(args::<PositionArgs>(json!({"path": "a.txt", "line": 1.5, "col": 1})).is_err());
    }

    #[test]
    fn renders_locations_and_never_reads_external_snippets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "€😀 target\n").unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, "PRIVATE CONTENT").unwrap();
        let fs = ProjectFs::new(&root).unwrap();
        let locations = [location(root.join("a.txt"), 0, 3), location(secret, 0, 0)];
        let result = render_locations(&fs, &locations);
        assert!(result.contains("a.txt:1:4  €😀 target"), "{result}");
        assert!(result.contains("secret.txt:1:1 [external]"), "{result}");
        assert!(!result.contains("PRIVATE"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&locations[1].path, root.join("escape.txt")).unwrap();
            let result = render_locations(&fs, &[location(root.join("escape.txt"), 0, 0)]);
            assert!(result.contains("[external]"));
            assert!(!result.contains("PRIVATE"));
        }
    }

    #[test]
    fn renders_symbols_diagnostics_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = ProjectFs::new(dir.path()).unwrap();
        let location = location(fs.root().join("a.txt"), 1, 3);
        let symbol = Symbol {
            name: "run".into(),
            kind: "function",
            detail: Some("fn run() -> bool".into()),
            container: Some("Thing".into()),
            location: location.clone(),
            depth: 1,
        };
        assert!(
            render_symbols(&fs, std::slice::from_ref(&symbol))
                .contains("a.txt:2:4    run (function) in Thing  fn run() -> bool")
        );
        let long = render_symbols(&fs, &vec![symbol; 250]);
        assert!(long.contains("50 more results not shown"));
        assert!(long.len() <= MAX_OUTPUT_BYTES);
        let diagnostic = |severity, message: &str| Diagnostic {
            path: location.path.clone(),
            range: location.range,
            severity,
            message: message.into(),
            source: Some("fake".into()),
            code: Some("E1".into()),
        };
        let diagnostics = BTreeMap::from([(
            location.path.clone(),
            vec![
                diagnostic(Severity::Warning, "warning\n detail"),
                diagnostic(Severity::Error, "bad"),
            ],
        )]);
        let output = render_diagnostics(&fs, &diagnostics);
        assert!(
            output.starts_with("a.txt\nErrors (1)\na.txt:2:4  bad [fake E1]\nWarnings (1)"),
            "{output}"
        );
        assert!(output.contains("warning detail"));
        assert!(bounded_text(&"😀".repeat(MAX_OUTPUT_BYTES)).len() <= MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn registers_six_tools_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let fs = ProjectFs::new(dir.path()).unwrap();
        std::fs::write(fs.root().join("a.txt"), "x\n").unwrap();
        let manager = LspManager::new(fs.root().into(), &BTreeMap::new(), None);
        let mut registry = Registry::default();
        install(&mut registry, manager.clone(), fs);
        assert_eq!(registry.iter().count(), 6);
        let no_server = registry
            .call("lsp_hover", json!({"path": "a.txt", "line": 1, "col": 1}))
            .await;
        assert!(no_server.is_error && no_server.content.contains("no language server"));
        let escape = registry
            .call("lsp_document_symbols", json!({"path": "../secret"}))
            .await;
        assert!(escape.is_error && escape.content.contains("outside the project root"));
        assert!(
            registry
                .call("lsp_workspace_symbols", json!({"query": "x"}))
                .await
                .is_error
        );
        assert!(registry.call("lsp_diagnostics", json!({})).await.is_error);
        assert!(
            registry
                .call(
                    "lsp_definition",
                    json!({"path": "a.txt", "line": 0, "col": 1})
                )
                .await
                .is_error
        );
        assert!(
            registry
                .call(
                    "lsp_hover",
                    json!({"path": "a.txt", "line": 1, "col": 1, "unexpected": true})
                )
                .await
                .is_error
        );
        manager.shutdown().await;
    }
}
