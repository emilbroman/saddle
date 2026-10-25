# Saddle

A *reverse agent harness*: a terminal UI for a coding agent, in the spirit of
Claude Code, Codex, OpenCode and Pi, except that the agent has no write access
to your codebase. Every change is made by you. The agent explains what should
change and why, watches your edits as you save them, and uses language servers
to keep an accurate picture of the code.

Status: early. The agent converses, streams its answers, reads the project
through read-only file and LSP tools, runs shell commands in an OS-enforced
read-only sandbox and receives diffs and diagnostics when you save changes.

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

### Saved changes

Saddle watches the project and tells the agent when you save. Changes are
batched: a batch is delivered once nothing has changed for `quiet_ms` (1 s by
default), or after `max_window_ms` (30 s) even if you keep saving. A batch that
arrives while the agent is working interrupts it immediately, like Esc.
Saddle then syncs the changed files to the language servers and waits for
diagnostics to settle before starting a turn with one combined event. The
transcript shows a one-line `✎ N files changed` entry (Ctrl+O expands it).

The agent receives the changed, created and deleted paths, and a unified diff
for each text file whose previous content it has seen: files it read with
`read_file`, and files that changed earlier in the session. Other files are
reported with their size; binary files and files over 1 MiB by path only.
Saving a file without changing it is not reported.

The same event includes new, changed and resolved diagnostics, compared with
the snapshot before syncing the batch. The extra wait defaults to 1 s of
quiet, capped at 5 s (`diagnostics_quiet_ms` / `diagnostics_max_wait_ms`).
Settling considers document sync and server progress across the workspace,
not just diagnostic notifications; a cold rust-analyzer may still be working
at the cap. In that case the event marks diagnostics as pending, and the
agent can query late results with `lsp_diagnostics`. With no language servers
configured, there is no extra wait. These timings do not change the watcher
batching defaults above.

Ignored files are never reported: `.git`, anything matched by `.gitignore`,
`.ignore`, `.git/info/exclude` or the global git excludes file, and the
sandbox's writable carve-outs (such as `target/`). On Linux each reportable
directory gets its own inotify watch, so ignored trees cost none; on macOS one
FSEvents stream covers the project.

Shift+Enter needs a terminal that supports the kitty keyboard protocol (kitty,
WezTerm, Ghostty, recent iTerm2); elsewhere use Alt+Enter or Ctrl+J. Other keys
edit the input with Emacs-style bindings.

### Sessions and context

Saddle keeps one living conversation per git worktree, or per directory outside
Git. Sessions are stored under `$XDG_DATA_HOME/saddle/sessions/<key>/`, falling
back to `~/.local/share/saddle/sessions/<key>/` (also on macOS). The key is a
stable hash of the canonical project root. No session data goes into the
repository or its git directory. Moving or renaming a worktree starts a new
session; different worktrees have independent conversations. A lock refuses a
second Saddle instance on the same worktree.

Reopening restores the full transcript, scrolled to the end, with tool results
collapsed. If nothing changed, resume is silent: no agent turn starts. Edits made
while Saddle was closed are reported as diffs against the last content Saddle
observed. Files whose current content matches `HEAD` are folded silently into
the snapshots; git-movement notifications are a follow-up feature, not yet
available. Shell jobs do not survive exit or restart; pending restored job
handles are reported as lost when resuming.

Send `/new` to archive the current log and start an empty conversation, resetting
the file snapshots and saved git state. This command is idle-only: press Esc
first if a turn is running. Archives stay on disk, but cannot be browsed or
resumed in Saddle yet.

The status line shows the last request's provider-reported prompt tokens over
`context_window` (default **128000** tokens). Set it to suit your model. Before
each request, Saddle estimates its size from the last reported usage plus new
content, or a serialized-content estimate before usage is available. When the
budget fills, it drops the oldest turns from the model request, keeping the
system prompt and whole tool round-trips intact. A one-line notice reports this
truncation; the on-disk log and visible transcript retain everything. On a
provider context-length error, Saddle truncates further and retries once. There
is no summarising compaction. The latest input and newest tool round-trip are
retained; if these and the system prompt alone exceed the model limit, the
provider error is shown rather than splitting a tool exchange.

Session and resume behavior on macOS remains pending user verification.

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
context_window = 128000                  # positive token budget for model requests (default 128000)
reasoning_effort = "medium"              # none, minimal, low, medium, high, xhigh, max, or a token budget
ca_file = "~/.config/saddle/bb3_root_ca.crt"    # extra trusted CA (PEM), on top of the system store

# Language servers, one table per language.
[lsp.rust]
command = "rust-analyzer"
args = []                                # optional
file_types = ["rs"]                      # extensions without the dot
root_markers = ["Cargo.toml"]            # optional

# The agent's sandboxed shell. The project itself is always read-only.
[sandbox]
writable = ["~/.cargo"]                  # writable paths outside projects, absolute or ~/ (default: ["~/.cargo"])
backend = "auto"                         # auto, bwrap, landlock (Linux) or seatbelt (macOS)
env = []                                 # extra environment variable names passed to commands

[sandbox.projects."~/src/saddle"]        # writable build-output dirs, keyed by project root
writable = ["target"]                    # relative to that root, no `..`
# Projects without an entry get `target` if they contain a Cargo.toml.

[batching]                               # grouping of file-change notifications
quiet_ms = 1000                          # deliver after this long without changes (default 1000)
max_window_ms = 30000                    # never hold a file batch longer than this (default 30000)
diagnostics_quiet_ms = 1000              # LSP quiet period after syncing a batch (default 1000)
diagnostics_max_wait_ms = 5000           # cap on the extra diagnostics wait (default 5000)

[search]                                 # web search via Exa
enabled = true                           # default true; needs an Exa key
num_results = 5                          # results per search unless the agent asks; 1–10, default 5
```

The adapter is chosen by `api`, never inferred from the model name. Against
BB3's LiteLLM, `anthropic/*` models must use `messages` (Sonnet and Opus are
refused on `responses`), `chatgpt/*` models belong on `responses` (the only
route that surfaces their reasoning), and `melious/*` models work on either.

TLS uses the operating system's trust store (the Keychain on macOS). If the
endpoint's CA is not trusted there, point `ca_file` at it.

### Language servers

Each `[lsp.<language>]` table configures one server; the table name is also
the `languageId` sent to it, so use the usual identifiers (`rust`, `python`,
`typescript`, …). A file belongs to the first language (in name order) whose
`file_types` contains its extension or its whole file name.

Servers start in the background at launch for each language that has files in
the project (respecting `.gitignore`), or later when a file of that language
changes or is queried. There is one server per language and workspace root;
the root is the topmost directory between the file and the project root that
contains one of `root_markers`, or the project root. Servers get no settings
(`workspace/configuration` is answered with nulls), so they run with their
defaults. Saddle syncs documents from disk only and never applies workspace
edits.

The status line shows each server's state (`starting…`, `✓`, the title of its
work in progress such as `indexing…`, `restarting`, `✗ failed`). A server that
crashes is restarted up to three times within five minutes; a command that
cannot be started is not retried. For rust-analyzer, make sure the standard
library sources are available (`rustup component add rust-src`, or
`RUST_SRC_PATH` as the Nix dev shell sets it).

### `credentials.toml`

Secrets only. Keep it private (`chmod 600`). Saddle never prints its values,
including in parse errors.

```toml
[inference]
api_key = "sk-..."                       # required

[exa]
api_key = "..."                          # optional; Exa key for web search
```

## Agent tools

The agent cannot write anything. Its tools are read-only. The file tools are
confined to the project root: paths that escape it, through `..` or symlinks,
are refused.

| Tool        | What it does                                                                 |
| ----------- | ---------------------------------------------------------------------------- |
| `read_file` | Reads a text file with line numbers; at most 2000 lines per call, paged with `start_line` / `end_line`. |
| `list`      | Lists one directory level.                                                   |
| `glob`      | Finds files by glob pattern, recursively.                                    |
| `grep`      | Searches file contents with a regular expression; at most 200 matches.       |
| `search_web` | Searches the web through Exa; title, URL, date and an excerpt for up to 10 results, optionally limited to the past day, week, month or year. |

`list`, `glob` and `grep` respect `.gitignore` and skip `.git`; none of them
follows symlinks. Every tool result is capped at 50 KiB with a notice saying
what was cut.

`search_web` calls `https://api.exa.ai/search` directly with the key in
`[exa] api_key` of `credentials.toml` (create one at
<https://dashboard.exa.ai/api-keys>). Without a key the tool is left out of
the agent's tool list and the status line says so; `[search] enabled = false`
turns it off silently. Each search costs Exa credits. Results are framed to the
model as untrusted source material, not instructions. Requests time out after
30 s; key, credit, rate-limit and network errors appear as the tool's result in
the transcript. The agent cannot fetch whole pages.

### LSP

| Tool | What it does |
| ---- | ------------ |
| `lsp_workspace_symbols(query)` | Searches symbols by name across workspace servers; the entry point from a name to a location. |
| `lsp_document_symbols(path)` | Lists a file's symbol outline. |
| `lsp_definition(path, line, col)` | Finds definitions at a location. |
| `lsp_references(path, line, col, include_declaration?)` | Finds references; includes the declaration by default. |
| `lsp_hover(path, line, col)` | Gets type information or documentation. |
| `lsp_diagnostics(path?)` | Shows current diagnostics with severity, for one file or the workspace. |

Paths are project-relative. Lines and columns are 1-based; columns count
Unicode scalar values, not bytes or UTF-16 units. Saddle converts positions
to and from the server's negotiated encoding. File queries open and sync the
document from disk first; missing server configuration or a server that is
not ready produces a tool error.

Results are compact and bounded, with `path:line:col` locations and symbol
names, signatures or diagnostic text where available. Locations outside the
project are labeled as external; Saddle does not read those files. Workspace
diagnostics show what active servers have published, not a full-project check.

### Shell

The shell tools run commands asynchronously, as jobs:

| Tool           | What it does                                                              |
| -------------- | ------------------------------------------------------------------------- |
| `shell_start`  | Starts `/bin/sh -c <command>` (optionally in a project subdirectory) and returns a job id, plus the result if it finishes within 1.5 s. |
| `shell_wait`   | Waits up to `timeout_secs` (default 60, at most 600) for a job to finish and returns its status and new output. |
| `shell_output` | Returns a job's status and its output since the last read, without waiting. |
| `shell_kill`   | Terminates a job's process group (SIGTERM, then SIGKILL after 2 s).        |
| `shell_list`   | Lists the session's jobs.                                                 |

A wait is interrupted, like any tool call, by Esc or by a batch of saved
changes; the job keeps running and the agent can wait on it again. Output is
captured with stderr merged into stdout. Each read returns at most the first
8 KiB and the last 24 KiB of what arrived since the previous read; the full
output (up to 64 MiB) is kept in `$TMPDIR/jobs/<id>.log`, where the agent can
`grep` it. Running jobs are counted in the status line, and the transcript
notes when each one ends. All jobs are killed when Saddle exits.

#### The sandbox

Every command runs under an OS-enforced sandbox, never unconfined:

- **The project is read-only** through every path: its files, `.git` (so
  `git commit`, `stash`, `checkout` and the like fail, while `status`, `log`,
  `diff` work), build scripts and tests run by the agent, symlinks and hard
  links.
- **Writable:** a private per-session scratch directory, which is `TMPDIR`
  (on Linux it is also mounted at `/tmp`) and is deleted when Saddle exits; the
  project's build-output carve-outs (`target/` by default for Cargo projects,
  created if missing); and the global carve-outs (`~/.cargo` by default, plus
  `$CARGO_HOME` when it is set). Add other toolchain caches, such as
  `~/.rustup` or `~/Library/Caches/...`, to `sandbox.writable` if your
  commands need them. Paths that contain the project, or lie inside `.git`, are
  refused with a notice. Everything else on the filesystem is readable and
  read-only.
- **Network:** unrestricted.
- **Hidden:** `credentials.toml` (bwrap and Seatbelt; see the gaps below).
- **Environment:** a curated one: `PATH`, `HOME`, `USER`, locale, `TZ`, TLS
  certificate paths, compiler and SDK variables, and anything starting with
  `LC_`, `NIX_`, `CARGO_`, `RUSTUP_`, `RUST` or `PKG_CONFIG`, plus the names
  in `sandbox.env`. Names with a part such as `KEY`, `TOKEN`, `SECRET`,
  `PASSWORD` or `AUTH` are always dropped. Commands get `TERM=dumb`, `cat` as
  pager, `GIT_OPTIONAL_LOCKS=0` and `SADDLE_SANDBOX=<backend>`. There is no
  terminal and stdin is `/dev/null`.
- Commands start in their own session and process group, and inherit no file
  descriptors besides stdin, stdout and stderr.

The backend, shown in the status line:

- **Linux: Bubblewrap** (`bwrap` on `PATH`), which needs unprivileged user
  namespaces. The root filesystem is bind-mounted read-only, with the scratch
  directory, the carve-outs and an empty `/dev/null` over the credentials file
  mounted on top, in fresh user, PID, IPC and UTS namespaces sharing the
  host's network. Where a new `/proc` cannot be mounted (inside unprivileged
  containers) the host's is bound read-only instead.
- **Linux fallback: Landlock** (kernel 6.2 or newer, Landlock ABI 3), used when
  bwrap is missing or cannot create namespaces. Saddle re-executes itself as a
  small launcher that restricts every filesystem write right to the writable
  paths and then runs the shell.
- **macOS: Seatbelt**, through `/usr/bin/sandbox-exec` with a generated
  profile: everything is allowed except writes outside the writable paths,
  hard links, moving the carve-outs themselves, and reading the credentials
  file. Apple has deprecated `sandbox-exec`, but it ships with every macOS.

Saddle probes the sandbox at startup, including an attempt to create a
directory in the project that must fail, and re-probes before each command. If
no backend works, or one stops working, the shell tools are disabled, and the
status line and transcript say why; set `sandbox.backend` to force one.

Known gaps:

- Landlock does not restrict metadata changes (`chmod`, `touch` on existing
  files, extended attributes) and cannot hide single files from reads, so the
  agent could read `credentials.toml` there. File contents are protected.
- Seatbelt is verified by hand, not by the test suite: see
  [docs/macos-sandbox-checklist.md](docs/macos-sandbox-checklist.md).
- On macOS, a process a command detaches into its own session survives
  `shell_kill`; on Linux the PID namespace ends with the job.
- Read access is not restricted: commands can read anything your user can,
  apart from the hidden credentials file.

The system prompt is `src/prompts/system.md`.

## Architecture

A single crate with a library (`src/lib.rs`) and a thin binary (`src/main.rs`):

- `config` — typed `config.toml` / `credentials.toml` loading and validation.
- `project` — project-root resolution.
- `session` — durable per-worktree event log, instance lock and resume state.
- `context` — request-only leading truncation against the configured token budget.
- `transcript` — the conversation model (user, assistant, tool call/result and
  notice entries), its wrapped layout and the transcript scroll state.
- `events` — channels for agent, filesystem, job and language-server events.
- `agent` — the conversation and its turn loop (stream, run tool calls, feed
  results back, repeat), on its own task behind `AgentHandle`, which also
  interrupts the running turn.
- `provider` — streaming inference behind a `Provider` trait: `genai` against
  the configured endpoint, and a scripted provider for tests.
- `tools` — the `Tool` trait and registry, root confinement, the read-only
  file tools, LSP queries, the shell tools and Exa web search.
- `sandbox` — the sandbox policy, curated environment and backends (Bubblewrap
  arguments, the Landlock launcher, the Seatbelt profile), with probing.
- `jobs` — shell jobs: spawning under the sandbox, output capture, waiting and
  killing.
- `watch` — the file watcher (`notify` with its full debouncer), ignore
  filtering, the batching policy, the record of what Saddle last saw of each
  file, and the change event sent to the agent. Change sets also go to
  `Watcher::subscribe` receivers.
- `lsp` — the generic language-server client: one supervised process per
  language and workspace root (`async-lsp` over stdio), document sync from
  disk, diagnostics settling and diffs for change events, and definition,
  references, hover and symbol queries in Unicode-scalar positions, converted
  to the server's negotiated encoding.
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
backend, including agent turns with a scripted provider and a file change
interrupting a turn. `tests/watch.rs` runs the watcher against the real
filesystem. `tests/sandbox.rs` runs commands under every Linux backend the
host provides, skipping the others; set `SADDLE_REQUIRE_SANDBOX=bwrap,landlock`
to make a missing backend fail instead. `tests/lsp.rs` runs the LSP client
against a scripted server (`tests/fixtures/fake_lsp.py`; skipped without
`python3`). A slower smoke test against a real rust-analyzer on a copy of this
crate is ignored by default:

```sh
cargo test --test lsp_rust_analyzer -- --ignored --nocapture
```

The rust-analyzer test also checks the registered tools and a combined diagnostic
change event using the default settling timings. Set `SADDLE_LIVE_LSP=1` to opt
into a live inference turn (using normal config and credentials): it asks the
model to run symbols → definition → references and then explain an introduced
type error. This makes inference requests to the configured endpoint.
