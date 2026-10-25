//! `textDocument/publishDiagnostics` results, kept per file in the server's
//! position encoding and converted when read.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lsp_types::{NumberOrString, PublishDiagnosticsParams};

use super::encoding::{Lines, PositionEncoding};
use super::{Diagnostic, Severity, TextRange};

struct Published {
    instance: u64,
    encoding: PositionEncoding,
    diagnostics: Vec<lsp_types::Diagnostic>,
}

#[derive(Default)]
pub struct DiagnosticsStore {
    files: Mutex<HashMap<PathBuf, Published>>,
}

impl DiagnosticsStore {
    /// Replaces the file's diagnostics (an empty list clears them).
    pub fn publish(
        &self,
        instance: u64,
        encoding: PositionEncoding,
        params: PublishDiagnosticsParams,
    ) {
        let Ok(path) = params.uri.to_file_path() else {
            return;
        };
        let mut files = self.files.lock().unwrap();
        if params.diagnostics.is_empty() {
            files.remove(&path);
        } else {
            files.insert(
                path,
                Published {
                    instance,
                    encoding,
                    diagnostics: params.diagnostics,
                },
            );
        }
    }

    /// Drops everything a server instance published, e.g. after it crashed.
    pub fn clear_instance(&self, instance: u64) {
        self.files
            .lock()
            .unwrap()
            .retain(|_, published| published.instance != instance);
    }

    /// Diagnostics for each file that has any, ordered by path then position.
    /// `text` supplies the current content used to convert columns.
    pub fn snapshot(
        &self,
        filter: impl Fn(&Path) -> bool,
        text: impl Fn(&Path) -> Option<String>,
    ) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        let raw: Vec<(PathBuf, PositionEncoding, Vec<lsp_types::Diagnostic>)> = {
            let files = self.files.lock().unwrap();
            files
                .iter()
                .filter(|(path, _)| filter(path))
                .map(|(path, p)| (path.clone(), p.encoding, p.diagnostics.clone()))
                .collect()
        };
        raw.into_iter()
            .map(|(path, encoding, diagnostics)| {
                let content = text(&path);
                let lines = content.as_deref().map(Lines::new);
                let mut converted: Vec<Diagnostic> = diagnostics
                    .into_iter()
                    .map(|d| convert(&path, encoding, lines.as_ref(), d))
                    .collect();
                converted.sort_by_key(|d| (d.range.start, d.severity));
                (path, converted)
            })
            .collect()
    }
}

fn convert(
    path: &Path,
    encoding: PositionEncoding,
    lines: Option<&Lines>,
    d: lsp_types::Diagnostic,
) -> Diagnostic {
    Diagnostic {
        path: path.to_owned(),
        range: match lines {
            Some(lines) => TextRange::from_lsp(lines, encoding, d.range),
            None => TextRange::raw(d.range),
        },
        severity: match d.severity {
            Some(lsp_types::DiagnosticSeverity::WARNING) => Severity::Warning,
            Some(lsp_types::DiagnosticSeverity::INFORMATION) => Severity::Information,
            Some(lsp_types::DiagnosticSeverity::HINT) => Severity::Hint,
            // Unspecified severity is up to the client; treat it as an error.
            _ => Severity::Error,
        },
        message: d.message,
        source: d.source,
        code: d.code.map(|code| match code {
            NumberOrString::Number(n) => n.to_string(),
            NumberOrString::String(s) => s,
        }),
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::{DiagnosticSeverity, Position, Range, Url};

    use super::*;
    use crate::lsp::TextPosition;

    fn diag(
        line: u32,
        start: u32,
        end: u32,
        severity: Option<DiagnosticSeverity>,
    ) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: Range::new(Position::new(line, start), Position::new(line, end)),
            severity,
            code: Some(NumberOrString::String("E0308".into())),
            source: Some("rustc".into()),
            message: "mismatched types".into(),
            ..Default::default()
        }
    }

    fn params(path: &Path, diagnostics: Vec<lsp_types::Diagnostic>) -> PublishDiagnosticsParams {
        PublishDiagnosticsParams {
            uri: Url::from_file_path(path).unwrap(),
            diagnostics,
            version: None,
        }
    }

    #[test]
    fn publish_convert_and_clear() {
        let store = DiagnosticsStore::default();
        let a = PathBuf::from("/p/src/a.rs");
        let b = PathBuf::from("/p/src/b.rs");
        store.publish(
            1,
            PositionEncoding::Utf8,
            params(
                &a,
                vec![
                    diag(1, 9, 10, Some(DiagnosticSeverity::WARNING)),
                    diag(0, 0, 2, None),
                ],
            ),
        );
        store.publish(
            2,
            PositionEncoding::Utf16,
            params(&b, vec![diag(0, 0, 1, None)]),
        );

        let text = |p: &Path| (p == a).then(|| "ok\nlet é = x;\n".to_string());
        let all = store.snapshot(|_| true, text);
        assert_eq!(all.len(), 2);
        let ds = &all[&a];
        assert_eq!(ds[0].severity, Severity::Error);
        assert_eq!(ds[0].code.as_deref(), Some("E0308"));
        assert_eq!(ds[1].severity, Severity::Warning);
        // Byte 9 on "let é = x;" is the `x` at character 8.
        assert_eq!(ds[1].range.start, TextPosition { line: 1, column: 8 });
        // Without content, offsets pass through unchanged.
        assert_eq!(all[&b][0].range.end, TextPosition { line: 0, column: 1 });

        assert_eq!(store.snapshot(|p| p == b, text).len(), 1);

        store.clear_instance(2);
        assert!(!store.snapshot(|_| true, text).contains_key(&b));
        store.publish(1, PositionEncoding::Utf8, params(&a, vec![]));
        assert!(store.snapshot(|_| true, text).is_empty());
    }
}
