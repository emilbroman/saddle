//! Agent tools exercised through a UTF-16 fake server.

use std::collections::BTreeMap;

use saddle::config::LspServer;
use saddle::lsp::LspManager;
use saddle::tools::{ProjectFs, Registry, lsp};
use serde_json::json;

const SERVER: &str = r#"
import json, sys

def send(msg):
    body = json.dumps(dict(jsonrpc='2.0', **msg)).encode()
    sys.stdout.buffer.write(b'Content-Length: %d\r\n\r\n' % len(body) + body)
    sys.stdout.buffer.flush()

docs = {}
while True:
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line == b'\r\n': break
        if line.lower().startswith(b'content-length:'): length = int(line.split(b':')[1])
    msg = json.loads(sys.stdin.buffer.read(length))
    method, params = msg.get('method'), msg.get('params') or {}
    result = None
    if method == 'initialize':
        result = {'capabilities': {'positionEncoding': 'utf-16', 'textDocumentSync': 1}}
    elif method in ('textDocument/didOpen', 'textDocument/didChange'):
        uri = params['textDocument']['uri']
        text = params['textDocument'].get('text') if method.endswith('didOpen') else params['contentChanges'][-1]['text']
        docs[uri] = text
        diagnostics = []
        if 'ERROR' in text:
            diagnostics = [{'range': {'start': {'line': 1, 'character': 0}, 'end': {'line': 1, 'character': 5}}, 'severity': 1, 'message': 'bad code', 'source': 'fake'}]
        send({'method': 'textDocument/publishDiagnostics', 'params': {'uri': uri, 'diagnostics': diagnostics}})
    elif method in ('textDocument/definition', 'textDocument/references', 'textDocument/hover'):
        uri = params['textDocument']['uri']
        assert params['position'] == {'line': 0, 'character': 4}, params
        target = {'uri': uri, 'range': {'start': {'line': 0, 'character': 4}, 'end': {'line': 0, 'character': 10}}}
        if method.endswith('definition'): result = target
        elif method.endswith('references'):
            result = [target] if params['context']['includeDeclaration'] else []
        else: result = {'contents': {'kind': 'markdown', 'value': '```rust\nfn target()\n```'}}
    elif method == 'textDocument/documentSymbol':
        result = [{'name': 'target', 'detail': 'fn target()', 'kind': 12, 'range': {'start': {'line': 0, 'character': 0}, 'end': {'line': 0, 'character': 10}}, 'selectionRange': {'start': {'line': 0, 'character': 4}, 'end': {'line': 0, 'character': 10}}}]
    elif method == 'workspace/symbol':
        result = [{'name': 'target', 'kind': 12, 'location': {'uri': next(iter(docs)), 'range': {'start': {'line': 0, 'character': 4}, 'end': {'line': 0, 'character': 10}}}}]
    elif method == 'exit': sys.exit(0)
    if 'id' in msg: send({'id': msg['id'], 'result': result})
"#;

#[tokio::test]
async fn six_tools_sync_query_and_render_unicode_positions() {
    if !std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fs = ProjectFs::new(dir.path()).unwrap();
    let file = fs.root().join("a.txt");
    std::fs::write(&file, "€😀 target\nERROR\n").unwrap();
    let servers = BTreeMap::from([(
        "fake".into(),
        LspServer {
            command: "python3".into(),
            args: vec!["-c".into(), SERVER.into()],
            file_types: vec!["txt".into()],
            root_markers: vec![],
        },
    )]);
    let manager = LspManager::new(fs.root().into(), &servers, None);
    let mut registry = Registry::default();
    lsp::install(&mut registry, manager.clone(), fs);

    // A diagnostics query must open the file without an earlier navigation query.
    let diagnostics = registry
        .call("lsp_diagnostics", json!({"path": "a.txt"}))
        .await;
    assert!(!diagnostics.is_error, "{}", diagnostics.content);
    assert!(
        diagnostics
            .content
            .contains("Errors (1)\na.txt:2:1  bad code [fake]"),
        "{}",
        diagnostics.content
    );

    let position = json!({"path": "a.txt", "line": 1, "col": 4});
    for tool in ["lsp_definition", "lsp_references"] {
        let output = registry.call(tool, position.clone()).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "a.txt:1:4  €😀 target");
    }
    let output = registry
        .call(
            "lsp_references",
            json!({"path": "a.txt", "line": 1, "col": 4, "include_declaration": false}),
        )
        .await;
    assert_eq!(output.content, "No locations found.");
    let hover = registry.call("lsp_hover", position).await;
    assert!(
        !hover.is_error && hover.content.contains("a.txt:1:4\n```rust\nfn target()"),
        "{}",
        hover.content
    );
    for (tool, args) in [
        ("lsp_document_symbols", json!({"path": "a.txt"})),
        ("lsp_workspace_symbols", json!({"query": "target"})),
    ] {
        let output = registry.call(tool, args).await;
        assert!(
            !output.is_error && output.content.contains("a.txt:1:4"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("target (function)"),
            "{}",
            output.content
        );
    }
    std::fs::write(&file, "€😀 target\nfine\n").unwrap();
    let diagnostics = registry
        .call("lsp_diagnostics", json!({"path": "a.txt"}))
        .await;
    assert!(
        !diagnostics.is_error && diagnostics.content.starts_with("No published diagnostics."),
        "{}",
        diagnostics.content
    );
    manager.shutdown().await;
}
