# Status: where the work stands

Last updated 2026-11-25, at commit `f43234b`. Update this file in the same
commit as the work it describes, so any agent can pick up from here.

## Shipped

- onerig 0.1.1 is on crates.io (`onerig`, `onerig-core`). 0.1.0 is yanked:
  it pointed to a commit that was rewritten.
- Build steps 0 to 5 of `docs/designs/design_draft.md`: plan with Claude
  Code, work with DeepSeek through Codex or with `agy` (manual hand-over by
  default, `direct` opt-in), review with Claude Code, commit; pause and
  resume; the dashboard screen; `onerig doctor`.
- copass has an `onerig` adapter (copass PR #6, merged to copass `main`).

## In progress: the chat screen

Design: `docs/designs/chat/design_draft.md` (draft 2, reviewed by five
independent reviewers). The owner's decisions: `docs/designs/chat/concepts_brief.md`.

| Step | What | State |
|------|------|-------|
| 6 | `Task.lane`, `Task.from_owner`, lane pinning, origin lines in every prompt, start a run from an approved plan on a base ref | done, `f0c3269` |
| 7 | `Halt` (quit and per-action stop), `Send` runners | done, `f43234b` |
| 8 | Planner turn: answer schema and parser, prompt with the conversation, session files, transcript, session lock | **next** |
| 9 | Chat screen: input, conversation, panel, commands, `/btw`, `--continue` | to do |
| 10 | Native Windows support (design section 10a) | to do |
| 11 | `pi` adapter and the M365 lane; the owner runs the live task on Windows | to do |
| 12 | copass reads `from_owner` (in the copass repository) | to do |

## Next: step 8, concretely

Read design sections 5, 6.1, 6.6, 8 and 13 first. Then, test first:

1. **Answer schema and parser** in onerig-core: `{ "reply", "goal", "tasks" }`.
   Empty `tasks` is an answer only. Non-empty `tasks` is a proposal checked
   by the same rules as `parse_plan` (`plan.rs`), which already rejects
   `lane` and `from_owner` from a planner.
2. **Prompt** for a planner turn: the origin line of design section 7, the
   newest owner and planner messages (at most 40 messages or 24,000
   characters, oldest first, labelled `owner:` and `planner:`), and the
   waiting plan if any, marked "proposed, not approved". A `/btw` side turn
   also carries the current run's goal and task states and may not propose.
3. **Session files**: `<repo>/.onerig/sessions/<session id>/transcript.jsonl`
   (records of kinds `owner`, `planner`, `run`, `system`; design section 8),
   `lock` (reuse `store::Lock`), and `raw-<n>.log` for planner turns.
4. **A planner-turn function** that runs one fresh `claude -p` job with role
   planner (read-only tools, `--no-session-persistence` stays), retries an
   invalid proposal once with the validator's message, and maps quota, auth
   and stop to results the chat can show.
5. **Live capture**: two real planner turns on a toy repository (one plain
   answer, one proposal) saved as fixtures under
   `crates/onerig-core/tests/fixtures/claude/`, like the existing ones.
   These are small paid calls; tell the owner before making them.

## Rules for whoever continues

- Test first; keep `cargo test --workspace` green at every commit.
- Gates before each commit (PATH needs rustup and cargo):
  ```text
  export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
  cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
  crapper --changed   # CRAP below 10 for non-test functions
  dryer --changed     # no duplicate candidates
  ```
  Mutation testing mutates files in place, so run it in a copy:
  ```text
  rsync -a --delete --exclude target --exclude .metrics ./ target/mut/
  cd target/mut && export PYTHONPATH="$HOME/.local/share/uv/tools/crapper/lib/python3.13/site-packages"
  crapper >/dev/null; mutator crates/onerig-core/src/<file>.rs
  ```
  Known equivalent mutant: `run/execute.rs` streak reset (`0 -> 1`) after a
  non-retry failure; the next attempt resets the streak anyway.
- Commit messages: conventional (`feat:`, `fix:`, `docs:` ...), with a body
  that says why. **No attribution lines**: no `Co-Authored-By`, no
  "Generated with". Push to `main` as work lands.
- Code, comments and docs in English. The owner reads replies in Turkish.
- Never run a live `agy` direct task or an M365 task without the owner's
  explicit go-ahead for that run. Never print, store or ask for secrets;
  `DEEPSEEK_API_KEY` comes from the owner's shell.
- Throwaway work (toy repositories, mutation copies) goes under `target/`,
  which git ignores.
- Publishing: bump `version` in the root `Cargo.toml` and the `onerig-core`
  dependency version in `crates/onerig/Cargo.toml`, commit, then
  `cargo publish --workspace` (the owner's crates.io login is on the
  machine). If a dry run compiles against a stale `onerig-core`, run
  `cargo clean -p onerig-core` and retry.
