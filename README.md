# Saddle

A *reverse agent harness*: a terminal UI for a coding agent, in the spirit of
Claude Code, Codex, OpenCode and Pi, except that the agent has no write access
to your codebase. Every change is made by you. The agent explains what should
change and why, watches your edits as you save them, and uses language servers
to keep an accurate picture of the code.

Status: early skeleton. The TUI, configuration and event loop exist; inference,
tools, file watching, LSP and the sandboxed shell are still being built.

## Building and running

Requires a current stable Rust toolchain (1.88 or newer). Linux and macOS are
supported.

```sh
cargo run --release          # run from inside the project you want to work on
cargo build --release        # or build and copy target/release/saddle onto your PATH
```

The project root is the directory Saddle is launched in, or the root of the git
worktree containing it. It is shown in the status line.

### Keys

| Key                                   | Action                          |
| ------------------------------------- | ------------------------------- |
| Enter                                 | Send message                    |
| Shift+Enter, Alt+Enter, Ctrl+J        | Insert newline                  |
| PageUp / PageDown                     | Scroll transcript by a page     |
| Shift+Up / Shift+Down                 | Scroll transcript by a line     |
| Ctrl+Home / Ctrl+End                  | Jump to top / back to bottom    |
| Ctrl+C                                | Quit                            |

Shift+Enter needs a terminal that supports the kitty keyboard protocol (kitty,
WezTerm, Ghostty, recent iTerm2); elsewhere use Alt+Enter or Ctrl+J. Other keys
edit the input with Emacs-style bindings.

## Configuration

Saddle reads two files from `$XDG_CONFIG_HOME/saddle/`, or `~/.config/saddle/`
when `XDG_CONFIG_HOME` is unset (also on macOS). Unknown keys are rejected, so
typos fail loudly.

### `config.toml`

```toml
# Required
model = "claude-sonnet-4-5"              # model name as the inference endpoint knows it
api = "messages"                         # "messages" (Anthropic Messages) or "responses" (OpenAI Responses)
base_url = "https://litellm.example.internal"

# Language servers, one table per language.
[lsp.rust]
command = "rust-analyzer"
args = []                                # optional
file_types = ["rs"]                      # extensions without the dot
root_markers = ["Cargo.toml"]            # optional

# Writable carve-outs for the agent's sandboxed shell commands. The project
# itself is always read-only.
[sandbox]
writable = ["~/.cargo"]                  # global paths (default: ["~/.cargo"])

[sandbox.projects."~/src/saddle"]        # keyed by project root
writable = ["target"]                    # relative to that root
# Projects without an entry get `target` if they contain a Cargo.toml.

[batching]                               # grouping of file-change notifications
quiet_ms = 1000                          # deliver after this long without changes (default 1000)
max_window_ms = 30000                    # never hold a batch longer than this (default 30000)

[search]                                 # web search via Exa
enabled = true                           # default true; needs an Exa key
num_results = 5                          # 1–100, default 5
```

### `credentials.toml`

Secrets only. Keep it private (`chmod 600`). Saddle never prints its values,
including in parse errors.

```toml
[inference]
api_key = "sk-..."                       # required

[exa]
api_key = "..."                          # optional; web search is unavailable without it
```

## Architecture

A single crate with a library (`src/lib.rs`) and a thin binary (`src/main.rs`):

- `config` — typed `config.toml` / `credentials.toml` loading and validation.
- `project` — project-root resolution.
- `transcript` — the conversation model (user, assistant, tool call/result and
  notice entries), its wrapped layout and the transcript scroll state.
- `events` — channels for agent, filesystem and job events.
- `app` — application state and the central `tokio::select!` event loop over
  terminal input and those channels.
- `ui` — rendering with `ratatui`; `tui` — terminal setup and restore,
  including on panic.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt
```

`tests/smoke.rs` drives the event loop headlessly against ratatui's test
backend.
