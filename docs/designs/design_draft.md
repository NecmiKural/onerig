# onerig — design draft

Status: draft 1, 2026-11-23. Follows `concepts_brief.md`. Not yet implemented.

Statements about a harness are marked **[observed]** when they were checked on
the owner's machine, and **[unobserved]** when they come from documentation or
from another project's source code. Every unobserved statement must be
confirmed by a captured fixture before the adapter that relies on it ships.

## 1. Summary

onerig is a command-line tool with a terminal UI. Given a goal and a git
repository, it asks Claude Code for a plan, hands each task to a cheaper coding
harness, asks Claude Code to review the result, and loops until the review
passes or a budget ends. When a worker's quota is exhausted, the next worker in
a configured order continues the same task.

onerig never calls a model API. It starts the harness CLIs the user already
has (`claude`, `agy`, `dsh`) as subprocesses and reads their output.

## 2. Goals and non-goals

Goals for the first milestone:

- Run plan, execute, review and fix as one command, without a server.
- Keep Claude Code usage to planning and review.
- Fail over between workers on quota exhaustion without losing the task.
- Leave the user's checkout untouched: all work happens on a separate branch
  in a separate worktree.
- Make every run replayable from a log file.

Non-goals for the first milestone are listed in the brief. The ones that shape
this design: no parallel workers, no ACP, no long-lived harness sessions, no
Windows support.

## 3. Terms

| Term | Meaning |
|------|---------|
| Run | One invocation of `onerig run` for one goal. Has an id, a branch and a worktree. |
| Task | One unit of the plan: a title, instructions and acceptance criteria. |
| Attempt | One worker execution for a task. A task has one or more attempts. |
| Role | `planner`, `worker` or `reviewer`. |
| Lane | A named, configured way to run one harness: harness kind, binary, model, effort, mode. |
| Chain | The ordered list of lanes for a role. For `worker`, the fail-over order. |
| Job | One subprocess call of a harness: a prompt, a working directory and options. |
| Hand-over | The text given to the next lane when a task moves between lanes. |
| Event | One line of the run log. |

## 4. Architecture

A Cargo workspace with two crates:

```text
crates/
  onerig-core/        library, no terminal or argument-parsing dependencies
    src/
      config.rs       load and validate onerig.toml
      plan.rs         Plan, Task, Verdict types and their JSON Schemas
      run.rs          the run state machine
      harness/
        mod.rs        Harness trait, Job, Event, Outcome, FailureKind
        process.rs    shared subprocess driver (spawn, stream, watchdog, kill)
        claude.rs     adapter
        agy.rs        adapter
        dsh.rs        adapter
        fake.rs       scripted adapter for tests
      policy.rs       lane selection, quota checks, fail-over
      handover.rs     hand-over text
      worktree.rs     git worktree and commit operations
      store.rs        run directory, event log, state file
  onerig/             binary
    src/
      main.rs         argument parsing, wiring
      plain.rs        line-per-event output
      tui/            terminal UI
```

A later GUI, or an MCP server front-end, depends on `onerig-core` only.

Dependencies: `serde`, `serde_json`, `toml` and `thiserror` in the library;
`anyhow`, `clap`, `ratatui` and `crossterm` in the binary. There is no async
runtime: one child process runs at a time, so the driver uses
`std::process` with one reader thread per pipe and a channel. Git is driven
through the `git` executable, not a library.

## 5. Run lifecycle

```text
            +-----------+
 run  --->  | Planning  |
            +-----+-----+
                  | plan accepted
                  v
            +-----------+   quota / no lane    +---------+
       +--> | Executing | -------------------> | Paused  |
       |    +-----+-----+                      +----+----+
       |          | worker done                     | onerig resume
       |          v                                 |
       |    +-----------+                           |
       |    | Checking  |  optional check command   |
       |    +-----+-----+                           |
       |          v                                 |
       |    +-----------+                           |
       |    | Reviewing | <-------------------------+ (after a manual hand-over)
       |    +-----+-----+
       | revise   |  approve
       +----------+----------> commit task, next task ... -> Done
                  |
                  | review rounds exhausted
                  v
               Failed
```

Rules:

1. **Planning.** One planner job. The result must match the plan schema
   (section 7). An invalid or empty plan fails the run before any worker runs.
2. **Executing.** Tasks run one at a time, in plan order, in the run's
   worktree. Each task sees the commits of the tasks before it.
3. **Checking.** If `run.check` is set, onerig runs that shell command in the
   worktree after the worker finishes. Its exit code and the last 100 lines of
   output go to the reviewer. A failing check does not skip the review.
4. **Reviewing.** One reviewer job per attempt. The reviewer receives the task,
   its acceptance criteria, the diff of the worktree against the task's base
   commit, and the check result. It returns a verdict (section 7).
5. **Approve.** onerig commits the worktree changes as one commit with the task
   title, then moves to the next task.
6. **Revise.** The reviewer's issues become the prompt of the next attempt, in
   the same worktree, on the same lane if it is still available. After
   `run.max_review_rounds` rounds (default 3) the task and the run fail.
7. **Paused.** The run stops without losing state when no worker lane is
   available, when a manual hand-over is waiting for the human, or when the
   user pauses it. `onerig resume` continues from the stored state.
8. **Done.** onerig prints the branch name. It does not merge into the user's
   branch and does not delete the worktree.

A worker job that makes no change to the worktree counts as a failed attempt of
kind `Unknown`, whatever the harness reported.

## 6. Harness adapter contract

Each adapter is a set of pure functions. One shared driver does the process
work.

```rust
pub struct Job {
    pub role: Role,
    pub prompt: String,
    pub cwd: PathBuf,
    pub schema: Option<serde_json::Value>, // structured result wanted
    pub resume: Option<String>,            // harness session id
    pub writable: bool,                    // false for planner and reviewer
}

pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub stdin: Option<String>,
}

pub enum Event {
    Started { session_id: Option<String>, model: Option<String> },
    Text(String),
    Tool { name: String, detail: String },
    Usage { input_tokens: u64, output_tokens: u64 },
    Notice(String),
}

pub enum Outcome {
    Done {
        text: String,
        structured: Option<serde_json::Value>,
        session_id: Option<String>,
    },
    Failed {
        kind: FailureKind,
        message: String,
        reset_at: Option<SystemTime>,
    },
}

pub enum FailureKind { Quota, RateLimit, Auth, Session, Timeout, Unsupported, Unknown }

pub trait Harness {
    /// Version and capability check. Must not call a model.
    fn probe(&self) -> Result<Probe, ProbeError>;
    /// Remaining quota, when the harness can report it without a model call.
    fn quota(&self) -> Option<Result<Quota, ProbeError>>;
    /// Build the subprocess call. Pure.
    fn spawn(&self, job: &Job) -> Spawn;
    /// Turn one stdout line into zero or more events. Pure; unknown lines yield nothing.
    fn parse(&self, line: &str) -> Vec<Event>;
    /// Decide the outcome from everything the process produced. Pure.
    fn outcome(&self, exit_code: Option<i32>, stdout: &[String], stderr_tail: &str) -> Outcome;
}
```

What each failure kind makes the policy do:

| Kind | Action |
|------|--------|
| `Quota` | Mark the lane exhausted until `reset_at` (or for the rest of the run). Move the task to the next lane with a hand-over. |
| `RateLimit` | Wait (30 s, then 2 min), retry the same lane twice, then treat as `Quota`. |
| `Auth` | Stop the run. Tell the user which login command to run. Never retried. |
| `Session` | Retry once on the same lane without `resume`, with a hand-over. |
| `Timeout` | Count as a failed attempt. Move to the next lane. |
| `Unsupported` | Disable the lane for the run with the version message. Move to the next lane. |
| `Unknown` | Count as a failed attempt on the same lane; a second one in a row moves to the next lane. |

The shared driver (`process.rs`):

- Starts the child in its own process group, with stdin, stdout and stderr
  piped. Stdin is written once and closed.
- Feeds each stdout line to `parse`, appends the events to the run log, and
  keeps the raw lines for `outcome`. Keeps the last 16 KB of stderr.
- Kills the process group when no stdout line arrives for
  `run.idle_timeout` (default 5 min): SIGTERM, then SIGKILL after 2 s. The
  outcome is then `Failed { kind: Timeout }`.
- Prompts go through stdin, never through argv, where the harness accepts it.
- Before a line is logged or shown, values of the secrets onerig injected are
  replaced with `***`.

## 7. Plan and verdict formats

The planner must return:

```json
{
  "tasks": [
    {
      "id": "t1",
      "title": "Add retry to the HTTP client",
      "instructions": "What to change and where, in full sentences.",
      "acceptance": ["Requests are retried three times on 5xx", "Unit test covers the retry"]
    }
  ]
}
```

Constraints: 1 to 20 tasks, unique ids, non-empty strings, at least one
acceptance item per task.

The reviewer must return:

```json
{
  "verdict": "approve",
  "summary": "One paragraph.",
  "issues": [{ "file": "src/http.rs", "note": "Retry also fires on 4xx." }]
}
```

`verdict` is `approve` or `revise`. `revise` requires at least one issue.
A reviewer result that does not match the schema counts as a failed reviewer
job, not as an approval.

## 8. Adapters

### 8.1 Claude Code (`claude`) — planner and reviewer

Call **[flags observed in `--help` of 2.1.330; behaviour unobserved]**:

```text
claude -p --safe-mode
  --model <lane.model> --effort <lane.effort>
  --output-format stream-json --verbose
  --json-schema <schema>
  --tools Read Glob Grep
  --permission-mode dontAsk
  --disallowedTools AskUserQuestion
  --no-session-persistence
  --append-system-prompt <role prompt>
```

Claude Code's documentation says `--bare` will become the default for print
mode, and bare mode cannot use a subscription login. onerig always passes its
mode flags explicitly and `doctor` checks that a subscription-backed call
still works after a Claude Code update.

- The job prompt is written to stdin. The working directory is the run's
  worktree. For the reviewer, the diff and check output are part of the prompt.
- `--safe-mode` keeps the user's plugins, hooks and MCP servers out of every
  call. If it cannot be combined with subscription login in print mode, the
  fallback is `--setting-sources` with an empty list plus `--strict-mcp-config`.
- Environment: remove `CLAUDECODE`, `AI_AGENT`, `CLAUDE_CODE_ENTRYPOINT` and
  every `ANTHROPIC_*` variable, so that a run started from inside a Claude Code
  session, or from a shell with a custom endpoint, still uses the user's own
  login.
- `parse` **[unobserved]**: lines are JSON objects with a `type`.
  `system`/`init` gives `session_id` and `model`. `assistant` messages give
  text and tool-use blocks. `result` is the last line.
- `outcome` **[unobserved]**: `Done` only when the last line has
  `type == "result"`, `subtype == "success"`, `is_error == false` and, when a
  schema was requested, a `structured_output` value that validates. Quota: a
  `rate_limit_event` with status `rejected`, or a result with API status 429,
  gives `Quota` with the reported reset time. "Not logged in" text gives `Auth`.
- `probe`: `claude --version` and `claude auth status`. No model call.
- `quota`: none. Claude Code has no free quota command; exhaustion is seen
  only after a call.

When the planner or the reviewer hits `Quota`, there is no second lane by
default, so the run pauses with the reset time.

### 8.2 Antigravity (`agy`) — worker, optional

The lane has a `mode`:

- `off`: the lane is ignored.
- `handover` (default): onerig never starts `agy`. When the policy reaches
  this lane it writes the hand-over file, prints the command for the human to
  run (`cd <worktree> && agy`), and pauses the run. After the human finishes,
  `onerig resume` goes to Checking and Reviewing.
- `direct`: onerig starts `agy` itself. Turning this on prints a one-time
  warning that quotes the vendor's position (see section 13) and requires
  `accept_vendor_terms_risk = true` on the lane.

Call in `direct` mode **[flags observed in `--help`; behaviour unobserved]**:

```text
<lane.bin> -p --output-format stream-json --input-format stream-json
  --model <lane.model> --mode accept-edits --sandbox
  --print-timeout 30m
  [--conversation <resume id>]
```

- The prompt is one stdin line: `{"event":"user","message":{"content":"..."}}`
  **[unobserved]**.
- `parse` **[unobserved]**: events are `init` (with `conversation_id`),
  `step_update` (text deltas and tool steps) and `result`.
- `outcome` **[unobserved]**: `Done` when the exit code is 0, the last event
  is `result` with status `SUCCESS`, there is no `finished_reason` and
  `denied_actions` is empty. `Quota` when stderr has a line starting with
  `AGY_ERROR:` whose JSON has status `RESOURCE_EXHAUSTED` or code 429, or when
  the error text matches "quota reached" or "exhausted your quota"; the reset
  time is parsed from "Resets in 1h2m3s".
- `probe`: `<bin> --version` must be at least 1.5.10; older versions give
  `Unsupported`.
- `agy` updates itself in the background on ordinary invocations
  **[observed: a 1.4.7 binary replaced itself with 1.5.14 after one
  `agy models` call]**. Every `agy` child gets
  `AGY_CLI_DISABLE_AUTO_UPDATE=true` **[unobserved]** so the version cannot
  change during a run, and the probe runs at the start of every run.
- `quota` **[observed on 1.4.7]**: `<bin> -p /usage --output-format json`
  returns `num_turns: 0`, zero tokens, and
  `command.data.groups[].buckets[]` with `remaining_fraction` and
  `reset_time`. The lane's `quota_group` names the group to read (substring
  match on the group name). The lane is unavailable when any bucket of that
  group is below `policy.min_remaining` (default 0.05); it becomes available
  again at the latest `reset_time` of the low buckets.

### 8.3 DeepSeek Harness (`dsh`) — worker

Call **[flags observed in `--help` of 0.2.2-rc.5; behaviour unobserved]**:

```text
dsh --profile headless --json
  --patch <run dir>/dsh-worker.patch.yml
  --permission-mode workspace-write --approval-policy never
  [--session-id <resume id>]
  -
```

- The task is written to stdin (`-`). The working directory is the worktree.
- The patch file is generated by onerig and applies to this call only. It
  turns off `session-log-deepseek` (uploads of session logs) and sets
  `agent-default-model` to the lane's model. Both ids exist in the installed
  version **[observed with `--dump-config`]**; that the patch is accepted is
  **[unobserved]**.
- Environment: `DSH_TELEMETRY_MODE=DISABLED`, and `DEEPSEEK_API_KEY` copied
  from the variable named by `lane.api_key_env` when that is set. If the
  variable is missing, onerig does not set a key and lets `dsh` use whatever
  login its own profile has.
- `parse` and `outcome` **[unobserved]**: NDJSON run events; exact field names
  are to be taken from the first captured run. Unknown event types are ignored.
- `probe`: `dsh --version`, and `dsh --profile headless --help` must mention
  `--json` and `--approval-policy`; otherwise `Unsupported`.
- `quota`: none. An empty balance is seen only after a call.

### 8.4 A second DeepSeek route, later

Claude Code can talk to DeepSeek's Anthropic-compatible endpoint. That lane
would reuse the Claude adapter with `base_url`, `api_key_env` and a separate
`CLAUDE_CONFIG_DIR`, so it can never fall back to the subscription login. It is
not in the first milestone; the native `dsh` lane comes first because it is the
vendor's own harness.

## 9. Lane selection and fail-over

Before every worker attempt the policy walks the worker chain in order and
takes the first lane that is:

1. not `off` and not disabled for this run,
2. not marked exhausted (or its `reset_at` has passed),
3. above `policy.min_remaining` when the harness reports quota.

`handover`-mode lanes are taken only when every automatic lane before them is
unavailable, because they stop the run and wait for a human.

When the chain yields no lane, the run pauses and reports the earliest
`reset_at` it knows.

When a task moves to another lane (after `Quota`, `Timeout`, `Unsupported` or
repeated `Unknown`), the next attempt starts in the same worktree with a
hand-over as its prompt. Work already on disk is kept.

## 10. Hand-over

Built by onerig, with no external tool:

```text
# Task
<title, instructions, acceptance criteria>

# State
Another agent started this task and stopped (<reason>).
Its changes are already in this directory. Continue from them; do not start over.

# Changes so far
<git diff --stat, then the diff, truncated to 60 KB>

# Last output of the previous agent
<last 4 KB of its text>

# Review issues to fix
<only on a revise round>
```

When `handover.provider = "copass"` and `copass` is on PATH, onerig instead
runs `copass relay --stdout --no-copy --from <agent> --dir <worktree>
--goal <task title>` **[unobserved]** and uses its output, falling back to the
built-in text if copass exits non-zero. The default is the built-in text.

## 11. Git and worktrees

- `onerig run` requires a git repository with at least one commit. It refuses
  to start when the repository has no commits.
- Run id: `YYYYMMDD-HHMMSS-<4 hex>`.
- Branch `onerig/<run id>` is created from the current `HEAD`. Uncommitted
  changes in the user's checkout are not included; onerig says so when the
  checkout is dirty.
- Worktree outside the project tree, at
  `$XDG_DATA_HOME/onerig/worktrees/<repo name>/<run id>` (default
  `~/.local/share/onerig/...`), so the user's editor, test runner and search
  tools never see a second copy of the code. The path is stored in
  `state.json`.
- Run files live in `<repo>/.onerig/`. On first use onerig appends `.onerig/`
  to `.git/info/exclude`, so the user's `.gitignore` is untouched.
- Workers are told not to run `git commit`. After an approval onerig runs
  `git add -A` and `git commit` in the worktree with the task title as the
  message.
- onerig never pushes, merges, rebases or deletes a branch or a worktree.

## 12. Configuration

`onerig.toml` in the repository root; if absent,
`~/.config/onerig/config.toml`. One file is used, never a merge of both.

```toml
[run]
max_review_rounds = 3
idle_timeout = "5m"
check = "cargo test"            # optional

[policy]
min_remaining = 0.05

[handover]
provider = "builtin"            # builtin | copass

[roles]
planner  = ["plan"]
worker   = ["deepseek", "agy"]
reviewer = ["review"]

[lanes.plan]
harness = "claude"
model = "sonnet"
effort = "medium"

[lanes.review]
harness = "claude"
model = "sonnet"
effort = "low"

[lanes.deepseek]
harness = "dsh"
model = "deepseek-flash"
api_key_env = "DEEPSEEK_API_KEY"

[lanes.agy]
harness = "agy"
mode = "handover"               # off | handover | direct
model = "gemini-4-hydrogen-medium"
quota_group = "Gemini Models"
# accept_vendor_terms_risk = true   # required for mode = "direct"
```

Validation at load: every lane named in `roles` exists; `planner` and
`reviewer` lanes use a harness that supports structured output (only `claude`
in the first milestone); `mode = "direct"` without
`accept_vendor_terms_risk = true` is an error; unknown keys are an error.
Secrets are never written in the file, only the name of the environment
variable that holds them.

Every lane also accepts `bin` (binary name or path; default is the harness's
usual name on PATH) and `extra_args = [...]`, appended to the harness call, so
a user can adapt to a harness release without waiting for an onerig release.

## 13. Safety

- **Vendor terms.** `direct` mode for `agy` is off by default. The warning
  shown when it is enabled states that Google's Antigravity terms and FAQ
  treat driving `agy` from another agent on a consumer sign-in as a breach
  that can lead to account suspension, and that the user enables it at their
  own risk.
- **Credentials.** onerig never reads a harness's credential store or
  keychain entry, never performs a login, and never changes a harness's global
  configuration files. It passes API keys only from environment variables the
  user named.
- **Blast radius.** Workers run unattended with edits and shell commands
  auto-approved, inside the harness's own sandbox limited to the worktree.
  Text in the repository can steer a worker (prompt injection); the sandbox
  and the worktree are what limit the damage. onerig never grants a worker
  access to the git directory, so a worker cannot rewrite hooks or history.
  Planner and reviewer jobs get read-only tools.
- **Untrusted text.** Worker output and diffs are untrusted input to the
  reviewer. The reviewer cannot write, and its answer is accepted only when it
  matches the verdict schema.
- **Data leaving the machine.** The first use of a lane whose harness sends
  code to a third party other than the planner's vendor is shown in the run
  header (for example "worker: DeepSeek").
- **Logs.** Injected secrets are masked before anything is logged or shown.

## 14. State on disk

```text
<repo>/.onerig/
  runs/<run id>/
    state.json        current state, task index, attempt counts, lane marks
    plan.json         the accepted plan
    events.jsonl      append-only: one JSON object per line
    handover-<task id>-<n>.md
    dsh-worker.patch.yml
```

The worktree is elsewhere (section 11).

Each `events.jsonl` line has `ts`, `run`, `task` (optional), `attempt`
(optional), `lane` (optional) and one of: a state change, a harness `Event`,
an `Outcome`, or a policy decision with its reason. `state.json` is rewritten
atomically (write to a temporary file, then rename) after every state change.
`onerig resume` reads `state.json`; the event log is for display and audit.

## 15. Command line

```text
onerig run "<goal>" [--plain]     start a run in the current repository
onerig resume [<run id>]          continue the latest (or given) paused run
onerig doctor                     probe every configured lane: binary, version,
                                  login, quota. Makes no model call.
```

`run` and `resume` show the TUI when stdout is a terminal and `--plain` is not
given; otherwise they print one line per event.

Exit codes: 0 done, 1 failed, 2 usage or configuration error, 3 paused.

## 16. Terminal UI

One screen, read from the same event stream as `--plain`:

```text
+ onerig  run 20261123-201500-a1f3   goal: add retry to the HTTP client -----+
| Tasks                      | t2  attempt 1  lane: deepseek (dsh)            |
|  ok  t1 Add retry helper   | > reading src/http.rs                          |
|  >>  t2 Wire into client   | > edit src/http.rs                             |
|  ..  t3 Update docs        | Applied the retry helper to send() ...         |
|----------------------------|                                                |
| Lanes                      |                                                |
|  plan     claude  ok       |                                                |
|  deepseek dsh     ok       |                                                |
|  agy      handover         |                                                |
+ q quit (run keeps state)  p pause after this attempt ----------------------+
```

Keys in the first milestone: `q` and `p` only. Quota bars appear for lanes
whose harness reports quota.

## 17. Testing

- **Adapters.** `spawn`, `parse` and `outcome` are pure, so each is tested
  with captured output kept under `tests/fixtures/<harness>/`. A fixture is
  captured from a real run before its adapter is written; secrets and personal
  paths are removed from it by hand.
- **State machine and policy.** Tested with the `fake` adapter, which replays
  a scripted list of outcomes: approve on first round, revise then approve,
  rounds exhausted, quota on the first lane then success on the second, no
  lane available, resume after pause.
- **Git.** `worktree.rs` is tested against temporary repositories.
- **Driver.** Tested with small shell scripts as the child process: normal
  exit, silence until the watchdog fires, output after SIGTERM.
- The project's quality gates (complexity, duplication, mutation testing) run
  on every change.

## 18. Milestones

| Step | Content | Needs a live harness |
|------|---------|----------------------|
| M0 | Workspace, config, store, driver, fake adapter, state machine, policy, worktree, `--plain` output | no |
| M1 | Claude adapter, `onerig doctor`, fixtures from one real plan and one real review | yes, small |
| M2 | `dsh` adapter, fixtures from one real task | yes, small |
| M3 | `agy` lane: quota probe, `handover` mode, then `direct` mode | probe: no; direct: yes |
| M4 | Terminal UI | no |

After M2 the tool does useful work end to end.

## 19. Open questions

1. Does `--safe-mode` work with subscription login in print mode, and how many
   tokens does a minimal planner call cost with and without it?
2. Does `dsh` accept the generated patch file, and where does it read its API
   key when `DEEPSEEK_API_KEY` is not set?
3. What does each harness print on quota exhaustion? No fixture exists yet for
   any of the three.
4. Should a failing `run.check` send the task straight back to the worker
   without spending a reviewer call?
5. Licence: MIT is assumed, matching copass.
