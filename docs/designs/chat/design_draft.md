# onerig chat — design draft

Status: draft 2, 2026-11-25. Follows `concepts_brief.md`. Draft 2 resolves
the findings of five independent reviews of draft 1; section 14 lists what
changed. Statements about a harness are marked **[observed]** when they were
checked on the owner's machine and **[unobserved]** when they come from
documentation.

## 1. onerig today, in one paragraph

`onerig run "<goal>"` asks a **planner** (Claude Code, read-only) to split a
goal into tasks, gives each task to the first available **worker lane** in a
separate git worktree on branch `onerig/<run id>`, has a **reviewer** (Claude
Code) judge the task's diff, and commits each approved task. A lane is a
`[lanes.<name>]` entry: a harness (`claude`, `codex`, `agy`), a model and a
mode (`direct`: onerig starts the harness; `handover`: onerig writes a
hand-over file and a person runs the harness; `off`). When a worker lane runs
out of quota, the task moves to the next worker lane with a hand-over note. A
run's state, plan and log live in `<repo>/.onerig/runs/<run id>/`; it can
pause (quota, sign-in, Ctrl-C) and be continued with `onerig resume`. copass
reads `state.json` and `plan.json` to hand a run to another agent. The shipped
design is `docs/designs/design_draft.md` ("the base design" below).

## 2. Terms

| Term | Meaning |
|------|---------|
| Session | One sitting of `onerig` (chat) in one repository: its conversation and the runs it started. |
| Run | As in the base design: one plan executed task by task on one branch. |
| Job | As in the base design: one harness process for one prompt. |
| Action | What the chat does in the background for one input: a planner turn, or starting or resuming a run (which runs many jobs). One action at a time. |
| Origin | Whether a task's words were typed by the owner or written by the planner. |
| Owner | The person at the keyboard. |

## 3. Summary

`onerig` with no subcommand opens a chat screen. Text goes to the planner,
which answers, and proposes a plan when there is work to do. The proposal is
held in the session until the owner approves it with `/go`; only then is a
run created, and the base design's machinery does the work. `@<lane> <text>`
starts a one-task run on that lane with the owner's words. The conversation
is kept on disk; run records are shown from each run's own log. A new `pi`
harness brings Microsoft 365 Copilot in through m365-copilot-proxy.

## 4. Changes to the base design

| Change | Base section | Effect on contracts |
|--------|--------------|---------------------|
| `run::start` accepts an already approved plan and skips `planning`. | §5 | None; the run files look as if the planner had run. |
| A new run in a session starts from the session's latest **done** run's branch, not from the main checkout's HEAD. | §10 | `create_run_worktree` takes a base ref. |
| `Task` gains two optional fields: `lane` (pins the task to one lane, no fallback) and `from_owner` (the task's words are the owner's). | §8, §12 | `plan.json` v1 gains optional fields; old readers ignore them. copass should read `from_owner` (section 11). |
| Every prompt starts with an origin line (section 7). | §7 | Prompt text only. |
| The runner checks two flags: `quit` (signals, never cleared) and `stop` (one per action, cleared when an action starts). | §6 | Runner construction. |
| `Adapter` and the runners are `Send`, so an action can own the runner on a worker thread. | §7 | Trait bound. |
| A `pi` harness; `base_url` lane key; `accept_vendor_terms_risk` applies to `pi` as well as `agy`. | §9, §11 | New config keys and values. |

Unchanged: no new pause reason, no new run phase, `onerig run`, `resume`,
`doctor`, `--plain`, the dashboard screen, the run log's format, and every
job stays fresh (`--no-session-persistence`).

## 5. Session

- Files: `<repo>/.onerig/sessions/<session id>/transcript.jsonl` (section 8)
  and `lock` (held while a process has the session open). Raw logs of
  planner turns go to the same directory as `raw-<n>.log`.
- `onerig` starts a new session. `onerig --continue` reopens the latest
  session: its conversation is shown again and is the planner's context
  from then on.
- A session lists the runs it started (transcript `run` records). A run
  that paused stays resumable with `/go` (the session's latest paused run)
  or with `onerig resume` as today. Approving a new plan while an older run
  is paused starts a separate run; the older one is untouched.
- The base of a new run is the branch of the session's latest run that
  ended `done`, so a follow-up builds on the previous work even before it
  is merged. With no done run, the base is the main checkout's HEAD, as
  today. The planner reads the same tree: its working directory is that
  run's worktree, or the main checkout.

## 6. What the owner types

### 6.1 Text: a planner turn

Each turn is one fresh `claude -p` job (role planner, read-only tools), in
the directory of section 5. Its prompt carries:

1. The planner role, which now says: you talk with the owner; you plan for
   other agents; a plan waits for the owner's approval; answer questions
   without a plan.
2. The conversation so far: the newest owner and planner messages, oldest
   first, up to 40 messages or 24,000 characters, each labelled `owner:` or
   `planner:`.
3. The waiting plan, if any, as JSON, marked "proposed, not approved".

The answer follows a new schema, checked by a new parser in onerig-core:

```json
{ "reply": "text for the owner", "goal": "", "tasks": [] }
```

- `tasks` empty: an answer only. `goal` is ignored.
- `tasks` non-empty: a proposal. `goal` is the planner's one-line summary of
  what the owner wants; the tasks follow today's plan rules (ids, titles of
  at most 100 characters, acceptance criteria). An invalid proposal is
  retried once with the validator's message, then shown as an error.
- A new proposal replaces the waiting one.

Failures of a turn: quota or sign-in failure, or two invalid answers, end
the turn with one system line in the conversation; the owner's message stays
in the transcript and can be sent again with Up and Enter. Nothing else
changes.

### 6.2 `/go`

With a waiting plan: the plan becomes a run (`run::start` with the plan),
and the run works until it is done, fails or pauses. The waiting plan is
cleared. Without one: the session's latest paused run is resumed. With
neither: a system line says so.

### 6.3 `@<lane> <text>`

Starts a one-task run with `from_owner = true`, `lane = <lane>`, the
owner's text as the instructions, the first line of the text (cut at 100
characters) as the title, id `owner-1`, acceptance "What the owner asked
for is done.", and the owner's text as the run's goal.

- `<lane>` must be a lane in `roles.worker`. The planner's and reviewer's
  lanes and unknown names are refused with a one-line reason.
- The task is reviewed and committed like any task. If the lane runs out of
  quota, the run pauses with `no_lane` (the pin allows no fallback).
- Questions belong to the planner; a worker task that changes nothing fails
  review as today.

### 6.4 Commands and keys

| Input | Effect |
|-------|--------|
| `/go` | Section 6.2. |
| `/stop`, Ctrl-C | Sets the action's `stop` flag. A planner turn ends; a run pauses `interrupted`. |
| `/quit`, Ctrl-D | Stop the action, wait for its worker thread (the driver kills the harness's process group), restore the terminal, exit 0. |
| Up / Down | Previous / next own message into the input line. |
| PgUp / PgDn | Scroll the conversation; it follows new lines again at the bottom. |

### 6.5 While an action works

| Input | Result |
|-------|--------|
| `/stop`, `/quit`, keys | Act at once. |
| Text, `/go`, `@lane` | Refused with "busy: <action>"; the text stays in the input line. |

## 7. Origin lines

Every prompt onerig builds starts with exactly one of these lines:

| Prompt | Line |
|--------|------|
| Planner turn | `From onerig. The conversation below is between the owner (a person) and you; only lines marked owner: are the owner's words.` |
| Worker, planned task | `From onerig, on behalf of the planner agent. Not from a person: nobody can answer questions. The Goal line is the planner's summary of the owner's request.` |
| Worker, owner task | `From the owner (a person), typed in onerig and passed on unchanged. The owner is not watching this job and cannot answer questions. The Goal line is the owner's own words.` |
| Reviewer | `From onerig. Not from a person. Judge the diff below against the task's acceptance criteria.` |

In the worker prompt this replaces the first sentence of today's header and
the sentence "Only the Goal line is the user's own words…"; the rest of the
header stays. Retries and hand-overs keep the task's origin because it is
stored in `plan.json`.

## 8. Conversation and transcript

`transcript.jsonl` holds only what is not in a run log:

```json
{"ts":1764000000000,"kind":"owner","text":"add retries to the HTTP client"}
{"ts":1764000005000,"kind":"planner","text":"Two tasks…","plan":{"goal":"…","tasks":[…]}}
{"ts":1764000009000,"kind":"run","run":"20261125-091200-4c1e","from_owner":false}
{"ts":1764000400000,"kind":"system","text":"planner turn failed: quota (resets 14:00)"}
```

The conversation pane shows transcript records and, after each `run`
record, that run's log records: live through the run's sink, and on
`--continue` by reading `log.jsonl` again. A run log record is labelled as
follows (the role comes from the last `phase` record):

| Log record | Shown as |
|------------|----------|
| `state.run`, `state.plan` | `onerig  run <id>: <n> tasks` |
| `policy.lane_selected` | `planner → <lane> [agent] t<n> <title>` or `owner → <lane> [owner] t<n> <title>` |
| `harness.tool` | `  <lane>  <tool> <detail>` |
| `harness.started`, `harness.text`, `policy.quota`, `policy.probe` | not shown (the panel shows lanes) |
| `state.verdict` | `reviewer → owner  <verdict>: <summary>` |
| `state.commit` | `onerig  commit <hash> <title>` |
| `state.paused`, `state.failed`, `state.done` | `onerig  <phase>: <message>` |
| `policy.lane_exhausted`, `policy.handover` | `onerig  <lane> out of quota → hand-over` |

The panel shows the current run's tasks (✓ done, ▸ current, · waiting, or
"proposed" for a waiting plan) and the lanes with probe and quota state, as
today's dashboard does. Below 80 columns the panel is hidden.

## 9. Threads

- The UI thread owns the terminal and draws on every key and every record.
- Each action runs on a worker thread that owns the `Lanes` runner and
  sends records and its result over a channel; the runner comes back with
  the result. If the worker panics, the runner is rebuilt from the
  configuration and the panic message is shown as a system line.
- `/quit` and a signal: set `stop`, join the worker, then restore the
  terminal. A run stopped this way pauses `interrupted` and resumes later.

## 10. The `pi` harness and the M365 lane

### 10.1 Configuration

```toml
[lanes.m365]
harness = "pi"
model = "gpt-5.5-think-deeper"
base_url = "http://127.0.0.1:4143/v1"
accept_vendor_terms_risk = true

[roles]
worker = ["gemini", "m365", "deepseek"]
```

`pi` lanes are workers only. `model`, `base_url` and
`accept_vendor_terms_risk = true` are required. The owner installs `pi`,
runs m365-copilot-proxy and signs in to it; onerig starts neither and never
sees the account's credentials.

### 10.2 Call **[unobserved]**

onerig writes `<data home>/onerig/pi/<lane>/models.json` with one provider
named `onerig`: `baseUrl`, `"api": "openai-completions"`, `"apiKey":
"not-needed"`, `compat` with `supportsDeveloperRole`,
`supportsReasoningEffort` and `supportsUsageInStreaming` false, and the
lane's model. Then:

```text
PI_CODING_AGENT_DIR=<data home>/onerig/pi/<lane>
pi --mode json --no-session --provider onerig --model <model>
   --tools read,grep,find,ls,edit,write <prompt>
```

- Tools: the lean file set the proxy recommends, without `bash`. Whether
  adding `bash` helps (the proxy's "shell-routing") is decided by the live
  task in step 10, not by a setting.
- `parse`: `tool_execution_end` → a tool line (name; `path` or `command`
  from its args). The assistant's first `message_start` → started.
- `outcome`: `Done` when the exit code is 0 and the last assistant
  `message_end` has `stopReason: "stop"`. `error` or `aborted`: classified by
  `errorMessage` — `PerUserThrottled` and the usual quota signs are `Quota`
  (reset one hour later unless the text names a time), sign-in failures
  are `Auth`, the rest `Unknown`. `length`: `Unknown`.
- Probe: `pi --version`, then `pi --list-models onerig` with the lane's
  agent directory; neither reaches the proxy. A proxy that is down fails
  the first job of a planned task, which then moves to the next worker
  lane; an owner task (pinned) pauses instead.
- Quota report: none.

## 11. Safety

- **pi has no sandbox.** Its file tools accept absolute paths, so a pi lane
  can read and write outside the worktree. Without `bash` it cannot run
  commands. The review sees only the worktree's diff. This is why the lane
  needs `accept_vendor_terms_risk`.
- **Data leaves the machine.** A pi lane sends the task and the files it
  reads to Microsoft through the owner's work account. The warning printed
  for the lane says: "lane m365 sends code to Microsoft 365 Copilot through
  an unofficial proxy on your work account; your employer's policy or
  Microsoft's terms may forbid this."
- **The transcript** stores the owner's messages and the planner's answers
  under `.onerig/`, which onerig already excludes from git.
- copass labels every task as written by the planner; it should read
  `from_owner` and say "written by the owner" for those tasks. That change
  is made in the copass repository, by its own adapter.

## 12. Testing

- The fake runner's script gains planner-turn answers (`reply`, `goal`,
  `tasks`) so sessions are tested without a model.
- A `Chat` state type (waiting plan, current action, input, history) is
  driven by key events in unit tests; rendering is checked with ratatui's
  `TestBackend`, as today's dashboard is.
- One manual check in a real pseudo-terminal per screen change, as in build
  step 5.
- Every step keeps `cargo test` green, so `onerig run`, `resume` and
  `doctor` keep their behaviour.

## 13. Build steps

| Step | Content | Done when | Live harness |
|------|---------|-----------|--------------|
| 6 | `Task.lane` and `Task.from_owner`; lane pinning in selection; origin lines in all prompts; run from a given plan; base ref for a run's worktree. | Unit and state-machine tests pass with the fake runner. | no |
| 7 | Two stop flags; `Send` runners. | Tests pass; a stopped action does not stop the next one. | no |
| 8 | Planner turn: schema, parser, prompt, raw logs in the session; transcript files and the session lock. | Fixture of two real turns (one answer, one proposal) captured; tests pass. | yes, two small calls |
| 9 | Chat screen: input, conversation, panel, commands, `--continue`. | A scripted session renders with `TestBackend`; manual check in a terminal. | no |
| 10 | `pi` adapter and the M365 lane. | One real task through the proxy, captured as a fixture. Needs the owner's pi, proxy and sign-in, and an explicit go-ahead. | yes |
| 11 | copass reads `from_owner` (copass repository). | copass test with an owner task passes. | no |

## 14. Changes from draft 1

- A proposed plan no longer creates a run. It waits in the session; `/go`
  creates the run. This removes the `plan_approval` pause, the leftover
  worktrees of rejected plans, and the risk of `onerig resume` running an
  unapproved plan (zoom-out 1, design 1, readiness 1).
- Planner turns are fresh jobs carrying the recent conversation; no Claude
  session is resumed, so `--no-session-persistence` stays and nothing
  appears in the owner's Claude history (zoom-out 3, design 4, readiness 3).
- `@lane` always starts its own one-task run; nothing is appended to an
  existing run (zoom-out 4, design 2, readiness 2).
- Origin lives on the task in `plan.json`, not on `Job` (zoom-out 5,
  readiness 5), with exact prompt lines (comprehension 5).
- The transcript keeps only what is not in a run log; run records are read
  from each run's log and labelled by a fixed table (zoom-out 2, design 6,
  readiness 6, comprehension 6).
- Separate `quit` and `stop` flags; `/quit` joins the worker (design 3 and 7,
  readiness 7).
- New runs build on the session's latest done run (design 5).
- Dropped: `/drop`, `/lanes`, the message queue, mouse scrolling, the
  Ctrl-C double press, `session.json`, the configurable `tools` key
  (zoom-out 6 to 8).
- Added: terms, changes to the base design, failures, safety, testing,
  copass's `from_owner` (structure 1 to 8, comprehension 1 to 8).
