//! Tools the agent can call. None of them can write to the project: the file
//! tools only read, and the shell runs under the OS sandbox
//! ([`crate::sandbox`]). LSP queries arrive through the same [`Tool`] trait.

pub mod fs;
mod grep;
mod list;
mod read_file;
pub mod shell;
pub mod web_search;

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use fs::ProjectFs;
pub use grep::Grep;
pub use list::{Glob, List};
pub use read_file::ReadFile;
pub use web_search::WebSearch;

use crate::watch::Observed;

/// Upper bound on any tool result sent to the model, in bytes.
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema of the arguments object.
    fn schema(&self) -> Value;
    /// One-line rendering of a call for the transcript.
    fn summary(&self, args: &Value) -> String;
    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput>;

    /// For tools with something useful to report when the turn is
    /// interrupted: a call that watches `interrupt` itself and then returns
    /// promptly with partial results, instead of being dropped.
    fn call_interruptible(
        &self,
        _args: Value,
        _interrupt: CancellationToken,
    ) -> Option<BoxFuture<'static, ToolOutput>> {
        None
    }
}

/// A running tool call; see [`Registry::start`].
pub struct Call {
    pub future: BoxFuture<'static, ToolOutput>,
    /// The future returns by itself soon after the interrupt fires.
    pub interruptible: bool,
}

#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    /// The read-only file tools over `fs`. Files read in full are recorded in
    /// `observed`, so that later changes to them can be shown as diffs.
    pub fn read_only(fs: ProjectFs, observed: Observed) -> Self {
        let mut registry = Self::default();
        registry.register(ReadFile::new(fs.clone(), observed));
        registry.register(List::new(fs.clone()));
        registry.register(Glob::new(fs.clone()));
        registry.register(Grep::new(fs));
        registry
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        // genai sends a tool named `web_search` as the provider's built-in
        // server-side search, whose stream it then fails to parse.
        assert_ne!(tool.name(), "web_search", "reserved tool name");
        assert!(
            self.get(tool.name()).is_none(),
            "duplicate tool {}",
            tool.name()
        );
        self.tools.push(Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.iter()
    }

    pub fn definitions(&self) -> Vec<genai::chat::Tool> {
        self.tools
            .iter()
            .map(|tool| {
                genai::chat::Tool::new(tool.name())
                    .with_description(tool.description())
                    .with_schema(tool.schema())
            })
            .collect()
    }

    pub fn summary(&self, name: &str, args: &Value) -> String {
        match self.get(name) {
            Some(tool) => tool.summary(args),
            None => args.to_string(),
        }
    }

    /// Runs a call. Unknown tools and failures become error outputs for the
    /// model; the result is always capped at [`MAX_OUTPUT_BYTES`].
    pub fn call(&self, name: &str, args: Value) -> BoxFuture<'static, ToolOutput> {
        self.start(name, args, None).future
    }

    /// Like [`Self::call`], letting tools that support it observe `interrupt`.
    pub fn start(&self, name: &str, args: Value, interrupt: Option<CancellationToken>) -> Call {
        let Some(tool) = self.get(name) else {
            let known: Vec<_> = self.tools.iter().map(|t| t.name()).collect();
            let output = ToolOutput::error(format!(
                "unknown tool {name:?}; available tools: {}",
                known.join(", ")
            ));
            return Call {
                future: Box::pin(async move { output }),
                interruptible: true,
            };
        };
        let (call, interruptible) = match interrupt
            .and_then(|interrupt| tool.call_interruptible(args.clone(), interrupt))
        {
            Some(call) => (call, true),
            None => (tool.call(args), false),
        };
        Call {
            future: Box::pin(async move {
                let mut output = call.await;
                output.content = truncate(&output.content, MAX_OUTPUT_BYTES);
                output
            }),
            interruptible,
        }
    }
}

/// Parses tool arguments, mapping failures to a message for the model.
pub(crate) fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolOutput> {
    // Some providers send `null` or an empty string for an argument-less call.
    let args = match args {
        Value::Null => Value::Object(Default::default()),
        Value::String(s) if s.trim().is_empty() => Value::Object(Default::default()),
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    serde_json::from_value(args).map_err(|e| ToolOutput::error(format!("invalid arguments: {e}")))
}

/// Cuts `text` to at most `max_bytes` (on a char boundary, preferring a line
/// boundary) and appends a notice saying how much was dropped.
pub fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = text[..end].rfind('\n')
        && newline > 0
    {
        end = newline + 1;
    }
    let dropped = text[end..].lines().count();
    format!(
        "{}\n[output truncated: {} more bytes in {dropped} more lines not shown]",
        text[..end].trim_end_matches('\n'),
        text.len() - end
    )
}

/// Cuts a single line to `max_chars` characters, marking the cut.
pub(crate) fn clip_line(line: &str, max_chars: usize) -> String {
    match line.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}… [line truncated]", &line[..at]),
        None => line.to_owned(),
    }
}

/// Whether the leading bytes look binary (contain NUL), as git and ripgrep judge it.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn truncate_cuts_at_line_boundary_with_notice() {
        let text = "aaaa\nbbbb\ncccc\n";
        assert_eq!(
            truncate(text, 12),
            "aaaa\nbbbb\n[output truncated: 5 more bytes in 1 more lines not shown]"
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let out = truncate("ééééé", 5);
        assert!(out.starts_with("éé\n[output truncated: 6 more bytes"));
    }

    #[test]
    fn clip_line_marks_cut() {
        assert_eq!(clip_line("abcdef", 3), "abc… [line truncated]");
        assert_eq!(clip_line("abc", 3), "abc");
    }

    #[test]
    fn parse_args_accepts_null_and_stringified_json() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct A {
            #[serde(default)]
            x: Option<u32>,
        }
        assert_eq!(parse_args::<A>(Value::Null).unwrap(), A { x: None });
        assert_eq!(
            parse_args::<A>(json!("{\"x\":1}")).unwrap(),
            A { x: Some(1) }
        );
        assert!(parse_args::<A>(json!({"x": "no"})).unwrap_err().is_error);
    }

    #[tokio::test]
    async fn registry_reports_unknown_tools() {
        let dir = tempfile::tempdir().unwrap();
        let registry =
            Registry::read_only(ProjectFs::new(dir.path()).unwrap(), Observed::default());
        let output = registry.call("write_file", json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("read_file, list, glob, grep"));
    }

    #[test]
    #[should_panic(expected = "reserved tool name")]
    fn registry_refuses_provider_builtin_names() {
        struct Builtin;
        impl Tool for Builtin {
            fn name(&self) -> &'static str {
                "web_search"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn summary(&self, _: &Value) -> String {
                String::new()
            }
            fn call(&self, _: Value) -> BoxFuture<'static, ToolOutput> {
                Box::pin(async { ToolOutput::ok("") })
            }
        }
        Registry::default().register(Builtin);
    }

    #[tokio::test]
    async fn registry_caps_output() {
        struct Loud;
        impl Tool for Loud {
            fn name(&self) -> &'static str {
                "loud"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn summary(&self, _: &Value) -> String {
                String::new()
            }
            fn call(&self, _: Value) -> BoxFuture<'static, ToolOutput> {
                Box::pin(async { ToolOutput::ok("x\n".repeat(MAX_OUTPUT_BYTES)) })
            }
        }
        let mut registry = Registry::default();
        registry.register(Loud);
        let output = registry.call("loud", Value::Null).await;
        assert!(output.content.len() < MAX_OUTPUT_BYTES + 100);
        assert!(output.content.ends_with("lines not shown]"));
    }
}
