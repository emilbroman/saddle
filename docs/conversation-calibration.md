# Conversation calibration

Linux live calibration, 2026-10-25, against `https://litellm.bb3.internal/v1/`:

- Messages: `anthropic/claude-sonnet-4-6`, plus a final-prompt A retest with
  `anthropic/claude-sonnet-5-5`.
- Responses: `chatgpt/gpt-5.6-luna`.
- Real Saddle TUI in tmux, real inotify saves from a second shell, real
  rust-analyzer, bwrap-enforced shell. Each run used a throwaway Saddle clone
  containing a small standalone `calibration` Cargo package with a snapshot
  comparison function and a change-kind enum. The fixture was edited by the
  operator, not by the model.
- Credentials were temporary, using the Rig LiteLLM consumer key as in earlier
  integration checks; no service configuration was changed.

## Observed behavior

Both API families used `no_comment` on the intermediate save. The transcript
showed only `✎ 1 file changed` and `⚙ no_comment`, with no assistant text or
visible tool result. Subsequent turns succeeded with the saved tool call/result
in history. Completed edits led to the next step without needing another user
message. The exact step order differed: Responses chose the representation
first, then content pairing; Messages initially combined those steps.

Both challenged basename-based pairing, identified unrelated `mod.rs` files as
false positives, and asked about the user's intent. When told the goal was
rename-and-edit detection, they adapted to exact matches followed by a content
similarity pass. Neither supplied function bodies or patches. When asked for
helper shape, both supplied a signature, although neither consistently cited
an exact `path:line`.

For the wrong-advice scenario, the operator explicitly seeded the premise that
the agent had suggested removing entries while iterating a map. This was not
an organically observed bad suggestion. Saving that reproduction produced a
real E0502 in the combined file/diagnostic event. Messages replied:

> Ah, that's the bad advice — you can't remove from a map while holding the
> iterator over its keys. Collect the keys into a `Vec<String>` first, then
> iterate that and remove.

Responses replied:

> Ah, this is the expected borrow error, so my earlier mutation-during-iteration
> advice was wrong. Since `consume` removes every entry, replace the loop with
> `known.clear()`; for selective consumption, collect owned keys before
> removing them.

After fixing the borrow error and adding a rename regression test, both ran
`cargo test` without being asked. Both obtained exit 0 and two passing tests.
The shell output and status were visible by default. Responses proposed a
renamed-and-edited regression case next; Messages ended with a handoff rather
than proposing an immediate next step.

## Calibration changes and limits

The first Messages run volunteered an enum variant and algorithm together,
narrated investigation, and responded to a partial edit with more instructions
and a confirmation question. The prompt was tightened to explicitly distinguish
code shapes from intent, forbid combining representation and algorithm steps,
and tell the model to assess the *whole* step before responding. A final
short reminder reinforces intent-level proposals, silent investigation and
quiet intermediate saves.

This is behavioral guidance, not a deterministic output grammar. Sonnet still
occasionally narrated tool use, gave more than one instruction, or ended with
a permission/handoff phrase. It also speculated incorrectly about Rust equality
before running a successful check. Responses was generally more concise but
sometimes repeated the test count. Do not interpret passing calibration as a
guarantee that all models will always follow the requested style. The no-write
sandbox and the automatic-only quiet outlet are enforced separately in code.

Final-prompt retests confirmed that wording changes did **not** fully fix
Sonnet 4.6's tendency to volunteer code shapes. Sonnet 5.5 improved the initial
proposal to representation first, without a tool preamble. Adding the variant
but not its formatter produced `no_comment`; finishing the formatter and
obtaining clean LSP diagnostics produced the next content-pairing step without
a permission question. It still gave more implementation detail than ideal.
Responses again stayed quiet on the partial save, then proactively ran tests
and proposed a rename regression case after the representation was complete.
These are observations, not a claim that prompt wording alone eliminated all
deviations.

A separate live display check ran `seq 1 35; exit 7`: by default the TUI showed
status 7, an omission notice for 15 earlier lines, lines 16–35 and the log path.
Ctrl+O showed lines 1–35; a second Ctrl+O restored the preview. The model sees
the full bounded tool result, not the UI tail (one response described all 35
lines as displayed, although the default UI showed only 20).

The first fixture run used a root marker that selected the outer Cargo project,
so rust-analyzer reported an unlinked-file hint. Later runs used a dedicated
fixture root marker. Concurrent indexing of both complete clones plus the
implementation build caused one rust-analyzer to be SIGKILLed and automatically
restarted; the final isolated runs ignored the outer source/test directories.
Pending diagnostics were observed and queried before the checks. These fixture
issues did not change production LSP behavior.

Git-event behavior is prompt guidance only here; actual git movement and commit
review calibration belongs to the git-events implementation. macOS shell-output
rendering remains pending user verification.
