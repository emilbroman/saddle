# macOS sandbox checklist

The Seatbelt backend cannot be exercised by the Linux test suite. Run this on
a Mac after changing `src/sandbox/seatbelt.rs` or the job runner, and record
the result (macOS version, pass/fail per step) wherever the change is tracked.

## Setup

```sh
cargo build --release
cd ~/src/saddle                     # or any Cargo project under git
~/src/saddle/target/release/saddle
```

The status line must show `sandbox: seatbelt`, and the transcript must not
show "The shell tool is disabled".

The profile Saddle generates is spelled out in the `profile_denies_writes_outside_carve_outs`
test in `src/sandbox/seatbelt.rs`.

## Steps

Ask the agent to run each command with `shell_start` (for example "run
`touch x` in the shell and tell me the exact output"). Expected results:

| # | Command | Expected |
| - | ------- | -------- |
| 1 | `touch new-file` | fails, "Operation not permitted"; no `new-file` in the project |
| 2 | `echo x >> README.md` | fails; `git status` in your own terminal is clean |
| 3 | `rm README.md`, `mv README.md x`, `mkdir d` | all fail |
| 4 | `git commit --allow-empty -m x` | fails (cannot write `.git`) |
| 5 | `git stash` after you change a file yourself | fails; your change is still there |
| 6 | `git status && git log --oneline -3` | succeeds |
| 7 | `ln README.md "$TMPDIR/h" && echo x >> "$TMPDIR/h"` | the `ln` fails |
| 8 | `ln -s "$PWD/src" "$TMPDIR/s" && touch "$TMPDIR/s/x"` | `touch` fails |
| 9 | `echo hi > "$TMPDIR/f" && cat "$TMPDIR/f"; echo "$TMPDIR"` | succeeds; TMPDIR is a `saddle-` directory under `/private/var/folders/...` |
| 10 | `cargo build` (with `target/` removed first, and a dependency not yet in `~/.cargo`) | succeeds: fetches from the network, writes `~/.cargo` and `target/` |
| 11 | a `build.rs` that writes into `src/` (add one temporarily yourself) | `cargo build` fails with "Operation not permitted" |
| 12 | `cat ~/.config/saddle/credentials.toml` | fails; the key is not shown |
| 13 | `env` | no API keys or tokens; `SADDLE_SANDBOX=seatbelt`, `TERM=dumb` |
| 14 | `mv target "$TMPDIR/t"` | fails |
| 15 | `sleep 120` as a job, then save a file in Helix while the agent waits on it | the wait returns "Wait interrupted; the job keeps running."; `shell_wait` on the same job later still works |
| 16 | `sleep 300` as a job, then `shell_kill` | job reported killed; `pgrep -f "sleep 300"` in your terminal finds nothing |
| 17 | quit Saddle (Ctrl+C) with a `sleep 300` job running | `pgrep -f "sleep 300"` finds nothing; the `saddle-*` scratch directory is gone |
| 18 | `config.toml`: `[sandbox] backend = "bwrap"` | Saddle starts with the shell disabled and says the backend is not available on this OS |

If step 10 fails because a tool writes to a cache outside `~/.cargo` (for
example Xcode's `xcrun` cache), add that directory to `sandbox.writable` and
note it here.
