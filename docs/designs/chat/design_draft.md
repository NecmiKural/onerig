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
| `/btw <text>` | A side question to the planner, answered while an action works (section 6.6). When nothing works, the same as plain text. |
| `/stop`, Ctrl-C | Sets the action's `stop` flag. A planner turn ends; a run pauses `interrupted`. |
| `/quit`, Ctrl-D | Stop the action, wait for its worker thread (the driver kills the harness's process group), restore the terminal, exit 0. |
| Up / Down | Previous / next own message into the input line. |
| PgUp / PgDn | Scroll the conversation; it follows new lines again at the bottom. |

### 6.5 While an action works

| Input | Result |
|-------|--------|
| `/stop`, `/quit`, keys | Act at once. |
| `/btw <text>` | Answered by a side planner turn, section 6.6. |
| Text, `/go`, `@lane` | Refused with "busy: <action> — use /btw to ask meanwhile"; the text stays in the input line. |

### 6.6 `/btw`: talking while the work goes on

Added at the owner's request on 2026-11-25: the owner may want to say or ask
something while agents work. `/btw <text>` starts a side planner turn next
to the running action, so the run is not interrupted:

- The side turn is an ordinary planner turn (section 6.1) whose prompt also
  carries the current run's goal and task list with each task's state. Its
  answer is a reply only: any `tasks` in it are ignored, and the reply says
  that new work can be planned once the run stops.
- It runs on its own thread with its own runner, built from the
  configuration like the main one. At most one side turn at a time; a
  second `/btw` while one is answering is refused.
- Owner and planner lines are recorded as transcript kinds `owner` and
  `planner` with `"btw": true`, shown as `owner → planner (btw)` and
  `planner → owner (btw)`, and are part of the planner's context afterwards.
- `/stop` stops the main action only; a side turn ends by itself.
  `/quit` stops and joins both.
- A side turn cannot reach the worker that is running: harness processes
  take their prompt once. Telling a running worker something new
  ("steering") stays out of scope.

## 7. Origin lines

Every prompt onerig builds starts with exactly one of these lines:

| Prompt | Lines (as built in step 6) |
|--------|------|
| Planner, `onerig run` | `From onerig, not from a person. The Goal line is the owner's own words.` |
| Planner turn in the chat | `From onerig, not from a person. The conversation below is between the owner (a person) and you; only lines marked owner: are the owner's words.` (step 8) |
| Worker, planned task | Header line 2 starts `From onerig, on behalf of the planner agent; not from a person, and nobody can answer questions.`; line 4 is `The Goal line states what the owner asked for; the task below was written from it by the planner agent.` |
| Worker, owner task | Header line 2 starts `From the owner (a person), typed in onerig and passed on unchanged; the owner is not watching this job and cannot answer questions.`; line 4 is `The Goal line and the task below are the owner's own words.` |
| Reviewer | `From onerig, not from a person.` |

In the worker prompt these replace the first sentence of the old header and
the old sentence "Only the Goal line is the user's own words…"; the rest of
the header stays. Retries and hand-overs keep the task's origin because it
is stored in `plan.json`. A planner's answer that sets `lane` or
`from_owner` is rejected, so only the owner's `@lane` text can carry the
owner's origin.

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
- A `/btw` side turn runs on a second worker thread with a second runner
  (section 6.6); its records go over the same channel, tagged as side.
- `/quit` and a signal: set `stop`, join the workers, then restore the
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
   --tools read,grep,find,ls,edit,write @<prompt file>
```

The prompt is written to a file in the run directory and passed as
`@<file>`, which keeps the command line short on Windows (section 10a).

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

## 10a. Windows

The owner tests the M365 lane from a Windows computer, natively (decided
2026-11-25, over WSL). Three parts of onerig are Unix-only today; each gets a
Windows counterpart behind `cfg`, with the Unix code unchanged:

| Today (Unix) | Where | Windows |
|--------------|-------|---------|
| Each job leads its own process group (`process_group(0)`); stop is SIGTERM to the group, then SIGKILL after the grace period. | `process.rs` | Each job is assigned to a Job Object (`windows-sys`), started with `CREATE_NEW_PROCESS_GROUP`; stop sends `CTRL_BREAK_EVENT`, then `TerminateJobObject` after the grace period. Closing the job handle kills what is left. |
| The run lock is `flock`; an orphan check sends signal 0 to the recorded group. | `store.rs` | The lock is `File::try_lock` from the standard library on both platforms (replaces `libc::flock`). The lock file records the job's process id; the orphan check opens that process and asks whether it has exited. |
| SIGINT, SIGTERM and SIGHUP set the stop flag. | `main.rs` | Ctrl-C and Ctrl-Break set it (console control handler). |

Also needed on Windows:

- **Finding a harness.** npm-installed CLIs are `.cmd` files (`claude.cmd`,
  `pi.cmd`). onerig resolves a lane's `bin` through `PATH` and `PATHEXT`
  before spawning.
- **Long prompts.** A command line through `cmd.exe` is limited to 8191
  characters, and a prompt with a diff is longer. Prompts that today go as
  an argument move to stdin (Claude, Codex already) or to a file: `pi` reads
  `@<file>`; `agy`'s prompt stays an argument and is checked against the
  limit **[unobserved on Windows]**.
- **Paths.** Without `XDG_DATA_HOME` and `HOME`, the data home is
  `%LOCALAPPDATA%` and the configuration is
  `%APPDATA%\onerig\config.toml`.
- **CI.** A `windows-latest` job runs fmt, clippy and the tests; tests that
  need a Unix shell script are marked Unix-only, each with a Windows twin
  where the behaviour matters (process stop, lock, orphan check).

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
| 6 | `Task.lane` and `Task.from_owner`; lane pinning in selection; origin lines in all prompts; run from a given plan; base ref for a run's worktree. | Done 2026-11-25: unit and state-machine tests pass with the fake runner. | no |
| 7 | Two stop flags; `Send` runners. | Done 2026-11-25: `Halt` holds a quit flag (signals, never cleared) and a stop flag (cleared per action); tests show a cleared stop lets the next action run and a quit keeps pausing; runners are `Send`. | no |
| 8 | Planner turn: schema, parser, prompt, raw logs in the session; transcript files and the session lock. | Fixture of two real turns (one answer, one proposal) captured; tests pass. | yes, two small calls |
| 9 | Chat screen: input, conversation, panel, commands, `/btw` side turns, `--continue`. | A scripted session, with a `/btw` answered while a run works, renders with `TestBackend`; manual check in a terminal. | no |
| 10 | Windows (section 10a). | CI is green on Windows; on the owner's Windows computer `onerig doctor` probes the lanes and one fake-lane run ends done. | no (owner's machine) |
| 11 | `pi` adapter and the M365 lane. | One real task through the proxy, run by the owner on Windows, captured as a fixture. Needs the owner's pi, proxy and sign-in, and an explicit go-ahead. | yes |
| 12 | copass reads `from_owner` (copass repository). | copass test with an owner task passes. | no |

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
