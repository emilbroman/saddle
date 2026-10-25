# Saddle

A *reverse agent harness*: a terminal UI for a coding agent, in the spirit of
Claude Code, Codex, OpenCode and Pi, except that the agent has no write access
to your codebase. Every change is made by you. The agent explains what should
change and why, watches your edits as you save them, and uses language servers
to keep an accurate picture of the code.

Status: early. The agent converses, streams its answers and reads the project
through read-only file tools. File watching, LSP and the sandboxed shell are
still being built.

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
| Esc                                   | Interrupt the running turn      |
| Ctrl+O                                | Expand / collapse tool output   |
| Shift+Enter, Alt+Enter, Ctrl+J        | Insert newline                  |
| PageUp / PageDown                     | Scroll transcript by a page     |
| Shift+Up / Shift+Down                 | Scroll transcript by a line     |
| Ctrl+Home / Ctrl+End                  | Jump to top / back to bottom    |
| Ctrl+C                                | Quit                            |

While the agent is working, Enter keeps your draft in the input instead of
sending it; press Esc to interrupt. An interrupted answer stays in the
transcript, marked `[interrupted]`, and the agent sees what it had said so far.

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
model = "anthropic/claude-sonnet-4-6"    # passed verbatim, LiteLLM route prefix included
api = "messages"                         # "messages" (Anthropic Messages) or "responses" (OpenAI Responses)
base_url = "https://litellm.bb3.internal/v1/"   # the API family's path is appended

# Optional
max_tokens = 16384                       # output limit per model response (default 16384)
reasoning_effort = "medium"              # none, minimal, low, medium, high, xhigh, max, or a token budget
ca_file = "~/.config/saddle/bb3_root_ca.crt"    # extra trusted CA (PEM), on top of the system store

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

The adapter is chosen by `api`, never inferred from the model name. Against
BB3's LiteLLM, `anthropic/*` models must use `messages` (Sonnet and Opus are
refused on `responses`), `chatgpt/*` models belong on `responses` (the only
route that surfaces their reasoning), and `melious/*` models work on either.

TLS uses the operating system's trust store (the Keychain on macOS). If the
endpoint's CA is not trusted there, point `ca_file` at it.

### `credentials.toml`

Secrets only. Keep it private (`chmod 600`). Saddle never prints its values,
including in parse errors.

```toml
[inference]
api_key = "sk-..."                       # required

[exa]
api_key = "..."                          # optional; web search is unavailable without it
```

## Agent tools

The agent cannot write anything. Its tools are read-only and confined to the
project root: paths that escape it, through `..` or symlinks, are refused.

| Tool        | What it does                                                                 |
| ----------- | ---------------------------------------------------------------------------- |
| `read_file` | Reads a text file with line numbers; at most 2000 lines per call, paged with `start_line` / `end_line`. |
| `list`      | Lists one directory level.                                                   |
| `glob`      | Finds files by glob pattern, recursively.                                    |
| `grep`      | Searches file contents with a regular expression; at most 200 matches.       |

`list`, `glob` and `grep` respect `.gitignore` and skip `.git`; none of them
follows symlinks. Every tool result is capped at 50 KiB with a notice saying
what was cut.

The system prompt is `src/prompts/system.md`.

## Architecture

A single crate with a library (`src/lib.rs`) and a thin binary (`src/main.rs`):

- `config` — typed `config.toml` / `credentials.toml` loading and validation.
- `project` — project-root resolution.
- `transcript` — the conversation model (user, assistant, tool call/result and
  notice entries), its wrapped layout and the transcript scroll state.
- `events` — channels for agent, filesystem and job events.
- `agent` — the conversation and its turn loop (stream, run tool calls, feed
  results back, repeat), on its own task behind `AgentHandle`, which also
  interrupts the running turn.
- `provider` — streaming inference behind a `Provider` trait: `genai` against
  the configured endpoint, and a scripted provider for tests.
- `tools` — the `Tool` trait and registry, root confinement, and the read-only
  file tools.
- `prompt` — the system prompt.
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
backend, including agent turns with a scripted provider.
