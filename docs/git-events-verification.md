# Git movement verification

Linux verification on 2026-10-25, with real inotify, Git, and a full-screen TUI in
tmux. macOS/FSEvents verification remains user-side.

## Automated coverage

- Temporary repositories: first launch (including no repository and unborn HEAD),
  fast-forward/unseen commits, same-email commits inside and outside the batch
  window, amend, reset, rebase, branch merge-base/default-branch detection and
  override, persisted per-branch tips, detached HEAD, capped lists/stat, and
  linked-worktree private/common administrative directories.
- Real watches: checkout with clean files and a dirty edit, metadata-only commit,
  stash, `checkout --`, hard reset, new refs and packed refs, linked worktrees,
  and quiet index refreshes. HEAD-folded snapshots become the next diff baseline.
- Application lifecycle: first-launch orientation, offline commit + dirty edit in
  one automatic request, unchanged silent resume, `/new` orientation, and active
  turn interruption by both checkout and commit.
- LSP: HEAD-folded files still synchronize and contribute diagnostics; git events
  survive coalescing while servers settle.
- Versioned transcript restoration, compact/expanded rendering and control-character
  sanitization; git checkpoint updates preserve file snapshots and reject stale
  journal generations.

The full suite has 247 passing tests, plus one optional ignored rust-analyzer
integration test. Strict all-target clippy and formatting checks pass. Both Linux
sandbox backends are exercised by the sandbox integration suite.

A full-suite run exposed an interaction with the existing notify debouncer:
coalesced events may retain an older occurrence timestamp. Using that timestamp
for the quiet-period timer prematurely flushed continuous saves. The timer now
continues to use receipt time; a separate occurrence timestamp establishes the
commit-classification window. A regression test covers the distinction.

## Live inference

Model: `anthropic/claude-sonnet-4-6`, Messages through BB3 LiteLLM. A throwaway clone,
configuration, data directory and Cargo home were used; the real checkout and user
credentials were not exposed to shell commands.

1. First launch delivered only the branch, tip and subject. The agent replied:
   > On `main` at `86dd78f`. What are we working on?
   It did not explore the repository.
2. While the agent was working on a requested module walkthrough, a checkout of
   `smoke-feature` interrupted it. The transcript showed `Interrupted: repository
   changed.`, one branch-switch event with the unseen fixture commit and stat,
   and `no_comment`. There were no per-file checkout diffs. Quiet output for an
   irrelevant fixture-only change is allowed; in this run the model did not
   resume its interrupted walkthrough automatically.
3. During a streaming sandbox explanation, a local commit deliberately introduced
   a false claim in the fixture README. The turn was interrupted and replaced by
   a lightweight `committed` event, not an unseen-commit list or inline patch.
   Without being asked to review, the agent used sandboxed `git show --stat` and
   `git show`, then replied:
   > That last line is wrong — "Saddle can edit source files directly when asked"
   > is the opposite of true. Saddle has no write access and must not claim
   > otherwise. Worth correcting before that text propagates anywhere.

These are observed model reactions, not deterministic guarantees. The existing
conversation prompt was retained without a provisional git paragraph.

## macOS checklist (pending)

Run against a throwaway clone, and repeat the metadata checks in a linked worktree:

1. First launch: one compact git entry and a brief orientation. Ctrl+O exposes the
   branch and HEAD tip; `/new` repeats orientation without an unseen backlog.
2. Quit and reopen unchanged: transcript restored, no new turn. Commit/pull while
   closed and reopen: unseen commits and stat, without per-file committed patches.
3. While a turn is active, check out another branch. It must interrupt and deliver
   a branch-switch event. Make an unrelated dirty edit in the same burst: only
   that edit should also appear as a file-change event.
4. Commit while running: lightweight committed event and a default commit review.
   Amend/rebase/reset: history-rewritten counts, with new commits classified by
   the email/time heuristic.
5. Stash, `git checkout -- <file>`, and `git reset --hard`: reverted paths rather
   than patches when HEAD stays fixed. A subsequent edit must diff against the
   restored content.
6. Check new nested branch refs, `git pack-refs --all`, and both administrative
   directories of a linked worktree. Verify metadata changes are observed by
   FSEvents, and ordinary Git reads/index refreshes do not create spurious turns.
