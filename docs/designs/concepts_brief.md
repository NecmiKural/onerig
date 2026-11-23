# onerig — concepts brief

Status: agreed direction, 2026-11-23. Input for `design_draft.md`.

## Problem

The owner pays for three AI coding subscriptions and uses one of them for
everything. Claude Code (Team plan) plans, executes and reviews, so the
strongest model is spent on trivial work while Antigravity (Google AI Pro) and
DeepSeek (API) sit idle.

## Goal

One small tool that splits a coding job across the harnesses the user already
has:

- **Claude Code plans and reviews.**
- **A cheaper harness executes.**
- **When the executor's quota runs out, the next one continues the same task.**

Each model is driven through its own harness CLI (`claude`, `agy`, `dsh`), not
through a model API, so every model runs in the harness it works best in.

## Owner decisions

| Topic | Decision |
|-------|----------|
| Name | `onerig` ("one rig"; rig = harness). Free on crates.io, npm and GitHub on 2026-11-23. The phrase "one ring to rule them all" stays out of the name and tagline (trademark). |
| Language | Rust. One lightweight binary. |
| Interface | TUI first. A GUI may come later, so the core must be a library. |
| Distribution | Open source. |
| Antigravity | Driving `agy` directly is **optional and off by default**; the owner turns it on for their own account and accepts the risk (see Risks). The default is hand-over mode: onerig writes a hand-over note and the human runs `agy`. API-key auth for `agy` comes later. |
| DeepSeek | API key only. |
| copass | The two projects stay independent. Each works without the other. |
| Git | Work on `main`, commit as we go. No push and no public repository until the owner says so. |

## Why Rust

Raw speed is not the reason: onerig spends its time waiting for other
programs. The reasons are distribution and correctness.

- **One file, no runtime.** Only Rust and Go give that. TypeScript, Python and
  Java need a runtime on the user's machine or a large bundle, which misses
  the "lightweight, runs everywhere" goal.
- **Rust over Go.** The run state machine and the failure policy are enums the
  compiler checks for missing cases, and harness output is parsed into typed
  values. A later GUI can link the core library directly. Go is the close
  second (easier cross-compilation, simpler concurrency) and would be the
  choice if the maintainer prefers it.
- **TypeScript** would be the fastest to write and shares an ecosystem with
  copass, but requires Node.

Switching language is free until the first milestone is built and a rewrite
after it.

## Prior art: the gap is real

Read at source on 2026-11-23, nothing executed:

- **openagents** and **alook** both drive `claude` and `agy` through their own
  harnesses, but as chat-room bridges. Neither has planner/worker/reviewer
  roles, and neither switches harness when a quota runs out (alook rules it out
  by design). Both need a server and a Node runtime.
- **zeroshot** (Rust) routes roles per graph node but has no Antigravity or
  DeepSeek lane and no automatic resume. **agent-of-empires** (Rust) parks a
  rate-limited agent and offers a manual switch, with no roles.
- **SpurCode** (Rust TUI, GPL-3.0, very new) is the closest: a planning brain,
  workers in worktrees, a review lane, and a vendor switch on rate limits. Its
  README shows no Antigravity or DeepSeek lane and no automatic worker
  fail-over on quota. It was read, not run.
- **rune** is a GPL-3.0 Go IDE and terminal multiplexer, not an orchestrator.
  Borrow ideas only, never code. The same holds for SpurCode's code.

What was not found in any of them: one binary, no server, an explicit
plan/execute/review policy, a quota check before dispatch, and automatic
fail-over between harnesses that include `agy` and `dsh`. The gap is narrow
and this space moves weekly.

## What is verified on the owner's machine

Ran locally:

- `claude` 2.1.330, `agy` 1.5.14, `dsh` 0.2.2-rc.5 (shipped by the DeepSeek
  Harness desktop app), `copass`, `cline`, `codex`. Not installed: `opencode`,
  `pi`.
- `agy` updates itself: the PATH binary was 1.4.7 and replaced itself with
  1.5.14 after one `agy models` call during this research.
- `agy -p /usage --output-format json` returns the quota with `num_turns: 0`
  and zero tokens. Shape: `command.data.groups[].buckets[]` with `window`
  (`weekly` or `5h`), `remaining_fraction` and `reset_time`. Groups today:
  "Gemini Models", "Claude and GPT models", "Gemini 4 Argon".
- `agy models` lists model slugs and proves the sign-in works.
- `agy` has print mode with `--output-format stream-json`,
  `--input-format stream-json`, `--conversation`, `--model`, `--mode`,
  `--sandbox`, `--print-timeout`. 1.5.x adds an `acp` subcommand.
- `dsh --profile headless` takes one task (argument or stdin) and exits. Flags:
  `--json` (NDJSON run events), `--session-id`, `--permission-mode`,
  `--approval-policy ask|never`, `--add-dir`.

From documentation and other projects' source, not yet observed here:

- Event shapes of `claude -p --output-format stream-json`, `agy -p` stream-json
  and `dsh --json`.
- What each harness prints when a quota is exhausted.
- `copass relay --stdout --no-copy --from <agent> --session <id> --dir <dir>`.

The first real run of each harness must be captured and kept as a test fixture.

## Direction

1. **onerig owns the loop.** The Rust binary runs plan, execute, review and
   fix as an explicit state machine with a loop budget. It spawns harnesses as
   subprocesses. Exposing onerig to an interactive Claude Code session as an
   MCP server is a later, thin front-end over the same core.
2. **One adapter per harness, headless.** Each adapter is a pure module (build
   argv, parse a line, reduce events, classify a failure) plus a thin process
   driver. No PTY scraping. ACP is a later second transport behind the same
   trait.
3. **Roles map to ordered chains.** `planner`, `worker` and `reviewer` each
   name an ordered list of harness entries with model and effort. The worker
   chain is the fail-over order.
4. **Quota is a first-class signal.** Check before dispatch where the harness
   allows it for free (`agy -p /usage`). Classify failures after a run into
   quota, rate limit, auth, session or unknown. Quota advances the chain; a
   rate limit waits; nothing else retries blindly.
5. **Fail-over carries the task.** The next worker gets a hand-over: the task
   spec, acceptance criteria, the git diff so far and the tail of the previous
   output. copass is an optional richer provider, detected at run time.
6. **One git worktree per run.** Tasks run one after another in it, and each
   approved task becomes one commit. Workers write only there. The reviewer
   reads the diff and has no write tools.
7. **Three `agy` modes.** `off`, `handover` (default: write the note, the human
   runs `agy`) and `direct` (opt-in: onerig spawns `agy -p`, with a one-time
   warning that names the vendor terms).
8. **State is plain files.** One append-only JSONL event log per run under a
   project-local `.onerig/` directory. The TUI and any later GUI replay the
   log.

## First milestone

`onerig run "<goal>"` in a git repository:

1. Claude Code produces a plan as structured tasks.
2. Each task runs in the run's worktree on the first available worker
   (`dsh headless` by default; `agy` when set to `direct`).
3. Claude Code reviews the diff and approves or returns issues.
4. Issues go back to the worker until approved or the loop budget ends.
5. The TUI shows tasks, the live event stream and quota per harness.

## Not in the first milestone

- A GUI, a daemon, a server or a chat workspace.
- Adapters for Codex, Cline, pi, Hermes or OpenCode.
- ACP, mid-turn steering, parallel workers.
- API-key auth for `agy`.
- Installing or updating harnesses for the user.
- Windows process handling.

## Risks

- **Antigravity terms.** Section 6 of the Antigravity Additional Terms, the
  official FAQ and a maintainer statement of 2026-11-12 say that driving
  `agy -p` or `agy acp` from another agent or orchestrator on a consumer
  sign-in is not permitted, even for one personal account, and can end in
  suspension with a single appeal. `direct` mode does exactly that. It ships
  off, and the README must say so plainly.
- **Claude plan terms.** A user running the unmodified `claude` binary with
  their own login is allowed; onerig must never read credentials, proxy a
  login or modify the binary. Plan and review calls share the owner's seat
  limits with interactive use.
- **Preview harnesses.** `agy` and `dsh` release every few days and their
  output is not a stable contract. Adapters probe the version, ignore unknown
  events and say "unsupported version" instead of failing mid-run.
- **Data leaving the machine.** A hand-over to DeepSeek carries code and
  conversation text to a third party. `dsh` uploads session logs by default
  and must be configured not to.
- **Fail-over can make things worse.** A weaker worker with a lossy hand-over
  may redo or damage work. Worktree isolation and the review gate are needed
  from the first version.

## Open questions for the draft

- The minimum supported version of each harness.
- Planner output schema, and how the reviewer verdict is shaped.
- Whether plan and review use `--safe-mode` so the user's plugins and hooks
  do not load into every call.
- How `dsh` is isolated (`DSH_HOME`, session-log upload off) without touching
  the user's desktop app profile.
- Licence: MIT, matching copass, unless the owner prefers otherwise.
