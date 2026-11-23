# onerig — design draft

Status: draft 2, 2026-11-23. Follows `concepts_brief.md`. Not yet implemented.
Draft 2 resolves the findings of five independent reviews of draft 1; the last
section lists what changed.

Statements about a harness are marked **[observed]** when they were checked on
the owner's machine, and **[unobserved]** when they come from documentation or
from another project's source code. Every unobserved statement must be
confirmed by a captured fixture before the adapter that relies on it ships.

## 1. Summary

onerig is a command-line tool with a terminal UI. Given a goal and a git
repository, it asks Claude Code for a plan, hands each task to a cheaper coding
harness, asks Claude Code to review the result, and repeats until the review
passes or a budget ends. When a worker's quota is exhausted, the next worker in
a configured order continues the same task.

onerig never calls a model API. It starts the harness CLIs the user already
has (`claude`, `agy`, `dsh`) as subprocesses and reads their output.

In the default configuration the only automatic worker is DeepSeek Harness
(`dsh`). Antigravity (`agy`) is a manual lane by default: onerig writes a
hand-over note, pauses, and the human runs `agy`. Letting onerig start `agy`
itself is an opt-in mode.

## 2. Goals and non-goals

Goals for the first release:

- Run plan, execute, review and fix as one command, without a server.
- Keep Claude Code usage to planning and review.
- Fail over between workers on quota exhaustion without losing the task.
- Leave the user's checkout untouched: all work happens on a separate branch
  in a separate worktree.
- Leave a complete log of every run, and let any run that has not finished be
  resumed.

Not in the first release: parallel workers, the Agent Client Protocol (ACP, a
JSON-RPC protocol some harnesses speak), resuming a harness's own conversation,
a check command run by onerig, Windows support, a GUI, a daemon.

## 3. Terms

| Term | Meaning |
|------|---------|
| Harness | An external coding-agent CLI: `claude`, `agy` or `dsh`. |
| Adapter | onerig's module for one harness: it builds the command line and reads the output. |
| Run | One goal, from `onerig run` through any number of `onerig resume` calls. Has an id, a branch and a worktree. |
| Task | One unit of the plan: a title, instructions and acceptance criteria. |
| Attempt | One worker job for a task, whether it succeeded or failed. A task has one or more attempts. |
| Role | `planner`, `worker` or `reviewer`. |
| Lane | A named entry in the configuration: one harness plus its model and options. Two lanes may use the same harness. |
| Chain | The ordered list of lanes for a role. For `worker` it is the fail-over order. |
| Job | One subprocess call of a harness: a prompt and a working directory. |
| Hand-over | The prompt of every attempt after a task's first: the task plus what has happened so far. |
| Log record | One line of the run log. |

## 4. Architecture

A Cargo workspace with two crates:

```text
crates/
  onerig-core/        library, no terminal or argument-parsing dependencies
    src/
      config.rs       load and validate the configuration file
      plan.rs         Plan, Task, Verdict: types, JSON Schema text, checks
      prompt.rs       planner, worker, hand-over and reviewer prompt text
      run.rs          the run state machine
      policy.rs       lane selection and failure policy
      runner.rs       Runner trait; the real runner (adapter + driver)
      adapter/
        mod.rs        Adapter trait and its data types
        claude.rs
        agy.rs
        dsh.rs
      process.rs      subprocess driver (spawn, stream, watchdog, kill)
      fake.rs         scripted Runner for tests
      worktree.rs     git worktree, diff and commit operations
      store.rs        run directory, lock, log, state file
  onerig/             binary
    src/
      main.rs         argument parsing, signal handling, wiring
      plain.rs        line-per-record output
      tui/            terminal UI
```

A later GUI, or an MCP server front-end, depends on `onerig-core` only.

Dependencies: `serde`, `serde_json`, `toml`, `thiserror` and `libc` in the
library; `anyhow`, `clap`, `signal-hook`, `ratatui` and `crossterm` in the
binary. There is no async runtime: one child process runs at a time, so the
driver uses `std::process` with one reader thread per pipe and a channel.
`libc` is needed to signal a process group. Plans and verdicts are checked by
deserialising them and by hand-written checks, not by a JSON Schema library.
Git is driven through the `git` executable.

## 5. Run lifecycle

A run is always in one phase:

| Phase | What happens | Leaves to |
|-------|--------------|-----------|
| `planning` | One planner job produces the plan. | `executing`, `paused`, `failed` |
| `executing` | One worker attempt for the current task. | `reviewing`, `executing` (next attempt), `paused` |
| `reviewing` | One reviewer job judges the task diff. | `executing` (next task or a revise), `done`, `paused` |
| `paused` | Nothing runs. State is on disk. | the phase stored in `resume_phase`, by `onerig resume` |
| `done` | All tasks approved and committed. | — |
| `failed` | No usable plan. | — |

Rules:

1. **Planning.** A plan that passes the checks in section 7 is accepted
   without asking the user. If the planner job fails or its result fails the
   checks, it is retried once; a second failure ends the run as `failed`.
2. **Executing.** Tasks run one at a time, in plan order, in the run's
   worktree. Each task sees the commits of the tasks before it. When a task
   starts, onerig stores the worktree's `HEAD` as the task base. The lane for
   each attempt is chosen as section 6 describes.
3. **Reviewing.** After a worker job ends as `Done`, one reviewer job receives
   the task, its acceptance criteria and the task diff (section 10). It
   returns a verdict (section 7).
4. **Approve.** onerig commits the task (section 10) and moves to the next
   task, or to `done` after the last one.
5. **Revise.** The verdict's issues are stored and the task gets another
   attempt, whose prompt is the hand-over text including those issues.
6. **Attempt budget.** Every worker job that was started counts as an attempt.
   When a task has used `run.max_attempts` attempts (default 5) without an
   approval, the run pauses with reason `attempts_exhausted`.
7. **Reviewer failures.** A reviewer job that fails, or whose result fails the
   checks, is retried once and then pauses the run with reason `job_failed`.
   Reviewer failures do not count as attempts.
8. **Done.** onerig prints the branch name. It never merges into the user's
   branch and never deletes the worktree.

### Pausing and resuming

`paused` stores a reason, the phase to return to, and a message for the user:

| Reason | Cause | `resume_phase` |
|--------|-------|----------------|
| `no_lane` | No worker lane is available (section 6). The message says why for each lane and gives the earliest known reset time. | `executing` |
| `handover` | A manual lane was selected; the human is working. | `reviewing` |
| `quota` | The planner or the reviewer hit its quota. The message gives the reset time. | `planning` or `reviewing` |
| `auth` | A harness says the user is not logged in. The message names the login command. | the phase that was running |
| `attempts_exhausted` | Rule 6. | `executing` |
| `job_failed` | Rule 7. | `reviewing` |
| `git_error` | A git command failed. The message is git's own error text. | the phase that was running |
| `interrupted` | The user quit, a signal arrived, or onerig died. | the phase that was running |

`onerig resume`:

- accepts any run whose phase is not `done` or `failed`;
- treats a run found in `planning`, `executing` or `reviewing` with no live
  onerig process as `interrupted`;
- reads the configuration again, probes the lanes again, clears every
  exhausted mark that has no reset time, clears the task's skipped lanes and
  grants a fresh attempt budget;
- then enters `resume_phase`. An interrupted worker attempt is not continued:
  a new attempt starts with a hand-over whose reason is "interrupted". Files
  already changed in the worktree are kept.

On `q` in the terminal UI and on SIGINT, SIGTERM or SIGHUP, onerig kills the
child's process group, writes `paused` with reason `interrupted`, and exits
with code 3.

One run directory has one lock file holding onerig's process id and the
child's process group id. `run` and `resume` refuse to start while that
process is alive; if it is dead and the child group still exists, they kill
the group first.

## 6. Lane selection and failure policy

### Choosing the lane for a worker attempt

A worker lane is **available** when all of these hold:

1. its mode is not `off`;
2. it passed its probe when the run started or resumed;
3. it has no exhausted mark, or the mark's reset time has passed;
4. it is not in the current task's skipped set;
5. if its harness reports quota, the reported remaining fraction is at least
   `policy.min_remaining`.

Selection, before every worker attempt:

1. If the task's current lane is available, keep it.
2. Otherwise take the first available automatic lane in chain order.
3. Otherwise take the first available manual (`handover`) lane in chain order.
4. Otherwise pause with reason `no_lane`.

So a task stays on its lane across revise rounds, a manual lane is used only
when no automatic lane anywhere in the chain is available, and the chain order
serves both `agy` modes: with `worker = ["gemini", "deepseek"]`, `dsh` does the
work while `agy` is manual, and `agy` goes first once it is set to `direct`.

The skipped set belongs to one task and is emptied when the next task starts.

### What a failed worker job does

| Kind | Meaning | Action |
|------|---------|--------|
| `Quota` | The account's allowance is used up, or the provider refuses for load or balance. | Mark the lane exhausted until `reset_at`; with no reset time, until the next `onerig resume`. Select again. |
| `Auth` | The harness says the user is not logged in. | Pause with reason `auth`. |
| `Timeout` | The driver killed the job (section 8). | Add the lane to the task's skipped set. Select again. |
| `Unknown` | Anything else, including a result the adapter does not recognise. | Retry on the same lane; a second `Unknown` in a row on that lane adds it to the skipped set. Select again. |

Every one of these attempts counts against the attempt budget. An exhausted
mark lasts at least 60 seconds, so a wrong reset time cannot cause a tight
loop.

### Planner and reviewer jobs

The planner and reviewer chains have exactly one lane each in the first
release. For their jobs: `Quota` pauses with reason `quota`; `Auth` pauses
with reason `auth`; anything else follows rule 1 (planner) or rule 7
(reviewer) of section 5.

## 7. Prompts and formats

Every job is a fresh harness process. onerig never resumes a harness's own
conversation in the first release.

### Plan

The planner prompt is the goal plus fixed instructions: read the repository as
needed, split the goal into the fewest tasks that can each be reviewed alone,
and answer in the plan format. The planner must return:

```json
{
  "tasks": [
    {
      "id": "retry-helper",
      "title": "Add retry to the HTTP client",
      "instructions": "What to change and where, in full sentences.",
      "acceptance": ["Requests are retried three times on 5xx", "A unit test covers the retry"]
    }
  ]
}
```

Checks: 1 to 20 tasks; `id` matches `^[a-z0-9][a-z0-9_-]{0,31}$` and is
unique; `title` is one line of at most 100 characters; `instructions` is not
empty; at least one acceptance item per task.

### Worker prompt

| Attempt | Prompt |
|---------|--------|
| First attempt of a task | The Task part and the footer. |
| Any later attempt (revise, lane change, interruption, manual lane) | The full hand-over text. |

```text
# Task
<title>
<instructions>
Acceptance criteria:
- <item>

# State
Another agent started this task and stopped (<reason>).
Its changes are already in this directory. Continue from them; do not start over.

# Changes so far
<task diff --stat, then the task diff, cut at 60 KB with a "truncated" line>

# Last output of the previous agent
<last 4 KB of its text>

# Review issues to fix
- <file>: <note>

Work only inside this directory. Run the project's tests if there are any.
Do not run git commit.
```

`<reason>` is one of: quota exhausted, timed out, failed, interrupted, review
asked for changes. The State, Changes and Last-output parts are left out on a
first attempt; the Review part appears only after a `revise` verdict.

### Verdict

The reviewer prompt is the Task part, the task diff (cut at 60 KB, with a note
that the files can be read in the working directory), and fixed instructions:
judge only against the acceptance criteria, and answer in the verdict format.

```json
{
  "verdict": "revise",
  "summary": "One paragraph.",
  "issues": [{ "file": "src/http.rs", "note": "Retry also fires on 4xx." }]
}
```

Checks: `verdict` is `approve` or `revise`; `revise` has at least one issue.
A result that fails the checks is a failed reviewer job, never an approval.

## 8. Runner, adapter and driver

The state machine talks to one small interface, so it can be tested without
any harness:

```rust
pub trait Runner {
    /// Check that a lane can run. Makes no model call.
    fn preflight(&mut self, lane: &str) -> Result<Probe, ProbeError>;
    /// Remaining quota of a lane, when its harness can report it for free.
    fn quota(&mut self, lane: &str) -> Option<QuotaReport>;
    /// Run one job to its end, reporting events as they arrive.
    fn run(&mut self, lane: &str, job: &Job, on_event: &mut dyn FnMut(HarnessEvent)) -> Outcome;
}
```

The real runner combines an adapter with the process driver. The fake runner
replays a script (section 16).

```rust
pub struct Job {
    pub role: Role,
    pub prompt: String,
    pub cwd: PathBuf,              // the run's worktree
    pub run_dir: PathBuf,          // for files the harness call needs
    pub schema: Option<&'static str>, // JSON Schema text when a structured result is wanted
}

pub enum HarnessEvent {
    Started { model: Option<String> },
    Text(String),
    Tool { name: String, detail: String },
}

pub enum Outcome {
    Done { text: String, structured: Option<serde_json::Value> },
    Failed { kind: FailureKind, message: String, reset_at: Option<u64> }, // unix ms
}

pub enum FailureKind { Quota, Auth, Timeout, Unknown }

pub struct Probe { pub version: String }
pub enum ProbeError { Missing(String), Unsupported(String), NotLoggedIn(String) }
pub struct QuotaReport { pub remaining_fraction: f64, pub reset_at: Option<u64> }
```

There is one adapter value per lane, built from that lane's configuration.
Every adapter method is pure: it starts nothing and reads nothing.

```rust
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_remove: Vec<String>,          // a trailing `*` removes every variable with that prefix
    pub stdin: Option<String>,
    pub files: Vec<(PathBuf, String)>,    // written by the driver before the start
}

pub struct Output {
    pub exit_code: Option<i32>,
    pub stdout: Vec<String>,
    pub stderr_tail: String,
}

pub trait Adapter {
    fn probe_spawns(&self) -> Vec<Spawn>;
    fn probe_result(&self, outputs: &[Output]) -> Result<Probe, ProbeError>;
    fn quota_spawn(&self) -> Option<Spawn>;
    fn quota_result(&self, output: &Output) -> Option<QuotaReport>;
    fn spawn(&self, job: &Job) -> Spawn;
    /// One stdout line to zero or more events. Unknown lines yield nothing.
    fn parse(&self, line: &str) -> Vec<HarnessEvent>;
    /// The outcome of a job that ended by itself.
    fn outcome(&self, job: &Job, output: &Output) -> Outcome;
}
```

`outcome` only extracts `structured`; `plan.rs` deserialises it into a plan or
a verdict and applies the checks.

The driver (`process.rs`):

- Writes `Spawn.files`, then starts the child in its own process group with
  stdin, stdout and stderr piped. Stdin is written once and closed. The child
  inherits onerig's environment, changed by `env_set` and `env_remove`.
- Feeds each stdout line to `parse`, passes the events on, and keeps the lines
  and the last 16 KB of stderr for `outcome`.
- Kills the process group (SIGTERM, then SIGKILL after 2 s) when no stdout
  line arrives for `run.idle_timeout` (default 5 min) or when the job has run
  for `run.job_timeout` (default 30 min). The runner then returns
  `Failed { kind: Timeout }` itself and does not call `outcome`. A child that
  cannot be started gives `Failed { kind: Unknown }` with the system's error.
- For every job that ends as `Failed`, writes the raw stdout lines and the
  stderr tail to the run directory. These files are where quota fixtures come
  from the first time a real quota is hit.

onerig injects no secrets in the first release, so it has nothing of its own
to mask. A harness finds its login or API key the way it does when the user
runs it by hand.

## 9. Adapters

Each call below starts with the lane's `bin` (default: the harness's usual
name on PATH).

### 9.1 Claude Code — planner and reviewer

Call **[flags observed in `--help` of 2.1.330; behaviour unobserved]**:

```text
claude -p --safe-mode
  --model <lane.model> --effort <lane.effort>
  --output-format stream-json --verbose
  --json-schema <job.schema>
  --tools Read Glob Grep
  --permission-mode dontAsk
  --disallowedTools AskUserQuestion
  --no-session-persistence
  --append-system-prompt <role sentence>
```

- The prompt is written to stdin. The working directory is the worktree.
- `--safe-mode` keeps the user's plugins, hooks and MCP servers out of every
  call. If it cannot be combined with a subscription login in print mode, the
  fallback is `--setting-sources` with an empty list plus
  `--strict-mcp-config`.
- `env_remove`: `CLAUDECODE`, `AI_AGENT`, `CLAUDE_CODE_ENTRYPOINT` and
  `ANTHROPIC_*`, so that a run started from inside a Claude Code session, or
  from a shell with a custom endpoint, still uses the user's own login.
- `parse` **[unobserved]**: lines are JSON objects with a `type`.
  `system`/`init` gives the model. `assistant` messages give text and tool-use
  blocks. `result` is the last line.
- `outcome` **[unobserved]**: `Done` only when the last line has
  `type == "result"`, `subtype == "success"`, `is_error == false` and, when a
  schema was requested, a `structured_output` value. `Quota`: a
  `rate_limit_event` with status `rejected`, or a result with API status 429,
  with the reported reset time. `Auth`: a "not logged in" result. Everything
  else: `Unknown`.
- Probe: `claude --version` and `claude auth status`. No model call.
- Quota report: none. Exhaustion is seen only after a call.

Claude Code's documentation says `--bare` will become the default for print
mode, and bare mode cannot use a subscription login. If an update does that,
the first planner call fails as `Auth` before any worker has run, and the
message names this cause.

### 9.2 Antigravity (`agy`) — worker, manual by default

The lane has a `mode`:

- `off`: the lane is ignored.
- `handover` (default): onerig starts **no `agy` process of any kind**: no
  probe, no quota call, no quota display. When this lane is selected, onerig
  writes the hand-over text to a file in the run directory, prints three
  lines, and pauses with reason `handover`:

  ```text
  cd <worktree> && agy
  Give agy this file as your first message: <absolute path of the hand-over file>
  When you are done (do not commit): onerig resume <run id>
  ```

  `onerig resume` then goes to `reviewing`. A `revise` verdict selects a lane
  again, which is this lane again if no automatic lane has become available.
- `direct`: onerig starts `agy` itself. This needs
  `accept_vendor_terms_risk = true` on the lane, and the first run prints a
  warning (section 15).

Everything below applies to `direct` mode only.

Call **[flags observed in `--help`; behaviour unobserved]**:

```text
agy -p --output-format stream-json --input-format stream-json
  --model <lane.model> --mode accept-edits --sandbox
  --print-timeout 30m
```

- The prompt is one stdin line: `{"event":"user","message":{"content":"..."}}`
  **[unobserved]**.
- `env_set`: `AGY_CLI_DISABLE_AUTO_UPDATE=true` **[unobserved]**. `agy` updates
  itself in the background on ordinary invocations **[observed: a 1.4.7 binary
  replaced itself with 1.5.14 after one `agy models` call]**, and the version
  must not change during a run.
- `parse` **[unobserved]**: events are `init`, `step_update` (text deltas and
  tool steps) and `result`.
- `outcome` **[unobserved]**: `Done` when the exit code is 0, the last event
  is `result` with status `SUCCESS`, there is no `finished_reason` and
  `denied_actions` is empty. `Quota` when stderr has a line starting with
  `AGY_ERROR:` whose JSON has status `RESOURCE_EXHAUSTED` or code 429; the
  reset time is parsed from "Resets in 1h2m3s" when present. Everything else:
  `Unknown`.
- Probe: `agy --version` must be at least 1.5.10, else `Unsupported`.
- Quota report **[observed on 1.4.7]**: `agy -p /usage --output-format json`
  returns `num_turns: 0`, zero tokens, and `command.data.groups[].buckets[]`
  with `remaining_fraction` and `reset_time`. The lane's `quota_group` must
  equal a group's name exactly; an unknown group fails the probe. The report
  is the lowest `remaining_fraction` among that group's buckets and, when it
  is below `policy.min_remaining`, the latest `reset_time` of the low buckets.

### 9.3 DeepSeek Harness (`dsh`) — worker

Call **[flags observed in `--help` of 0.2.2-rc.5; behaviour unobserved]**:

```text
dsh --profile headless --json
  --patch <run dir>/dsh-overlay.yml
  --permission-mode workspace-write --approval-policy never
  -
```

- The prompt is written to stdin (`-`). The working directory is the worktree.
- `files`: `dsh-overlay.yml`, a settings overlay that applies to this call
  only. It turns off `session-log-deepseek` (upload of session logs to the
  vendor) and sets `agent-default-model` to the lane's model. Both ids exist
  in the installed version **[observed with `--dump-config`]**; that the
  overlay is accepted is **[unobserved]**.
- `env_set`: `DSH_TELEMETRY_MODE=DISABLED`.
- `dsh` reads its API key the way it does for the user: from
  `DEEPSEEK_API_KEY` in the environment, or from its own profile.
- `parse` and `outcome` **[unobserved]**: newline-delimited JSON run events;
  field names come from the first captured run. An empty balance or a
  provider refusal is `Quota` with no reset time; a missing key is `Auth`;
  everything else `Unknown`.
- Probe: `dsh --version`, and `dsh --profile headless --help` must mention
  `--json` and `--approval-policy`, else `Unsupported`.
- Quota report: none.

## 10. Git and worktrees

Before a run starts, onerig checks that the directory is a git repository
with at least one commit and that `user.name` and `user.email` are set;
otherwise it exits with code 2. The repository root is
`git rev-parse --show-toplevel`.

- Run id: `YYYYMMDD-HHMMSS-<4 hex>` in UTC.
- Branch `onerig/<run id>` is created from the current `HEAD`. Uncommitted
  changes in the user's checkout are not included; onerig says so when the
  checkout is dirty.
- The worktree is outside the project tree, at
  `$XDG_DATA_HOME/onerig/worktrees/<repo name>/<run id>` (default
  `~/.local/share/onerig/...`), so the user's editor, test runner and search
  tools never see a second copy of the code.
- Run files live in `<repo>/.onerig/`. On first use onerig appends `.onerig/`
  to `.git/info/exclude`, so the user's `.gitignore` is untouched.
- **Task base**: the worktree's `HEAD` when the task starts.
- **Task diff**: `git add -A` in the worktree, then
  `git diff --cached <task base>`. This includes new files. It is what the
  reviewer and the hand-over get.
- **Commit on approval**: `git add -A`, `git reset --soft <task base>`, then
  `git commit -m <title>`. The soft reset folds any commit a worker or a
  human made into the one task commit. When the staged tree equals the task
  base, onerig makes no commit and moves on.
- onerig does not bypass the user's hooks or commit signing. A git command
  that fails pauses the run with reason `git_error`.
- onerig never pushes, merges, rebases or deletes a branch or a worktree.

## 11. Configuration

One file: `$XDG_CONFIG_HOME/onerig/config.toml` (default
`~/.config/onerig/config.toml`), or the path given with `--config`. When the
file does not exist, onerig exits with code 2 and prints the path and a
minimal example.

```toml
[run]
max_attempts = 5
idle_timeout = "5m"
job_timeout = "30m"

[policy]
min_remaining = 0.05

[roles]
planner  = ["plan"]
worker   = ["gemini", "deepseek"]
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

[lanes.gemini]
harness = "agy"
mode = "handover"
model = "gemini-4-hydrogen-medium"
quota_group = "Gemini Models"
# accept_vendor_terms_risk = true    # required for mode = "direct"
```

| Key | Type | Default | Applies to |
|-----|------|---------|------------|
| `run.max_attempts` | integer 1–20 | 5 | — |
| `run.idle_timeout`, `run.job_timeout` | integer followed by `s`, `m` or `h` | `5m`, `30m` | — |
| `policy.min_remaining` | number 0–1 | 0.05 | — |
| `roles.planner`, `roles.reviewer` | list of exactly one lane name | required | lanes with harness `claude` |
| `roles.worker` | list of one or more distinct lane names | required | lanes with harness `dsh` or `agy` |
| `lanes.<name>.harness` | `claude`, `agy`, `dsh` | required | all |
| `lanes.<name>.bin` | string | the harness name | all |
| `lanes.<name>.model` | string | required | all |
| `lanes.<name>.effort` | `low`, `medium`, `high` | `medium` | `claude` |
| `lanes.<name>.mode` | `off`, `handover`, `direct` | `handover` | `agy` |
| `lanes.<name>.quota_group` | string | required when `mode = "direct"` | `agy` |
| `lanes.<name>.accept_vendor_terms_risk` | boolean | `false` | `agy` |

Validation at load, each failure exiting with code 2: a key not in this table;
a key on a lane whose harness it does not apply to; a role naming a lane that
does not exist; `mode = "direct"` without `accept_vendor_terms_risk = true`.
The file holds no secrets.

## 12. State on disk

```text
<repo>/.onerig/runs/<run id>/
  lock                       onerig pid and child process group id
  state.json                 the run's state; the only file resume reads
  plan.json                  the accepted plan
  log.jsonl                  append-only run log
  handover-<task index>-<attempt>.md
  raw-<task index>-<attempt>.log     only for failed jobs
  dsh-overlay.yml
```

`state.json` is replaced atomically (write a temporary file, then rename)
after every change, and the log record of a change is appended before it.

```json
{
  "v": 1,
  "run_id": "20261123-201500-a1f3",
  "goal": "add retry to the HTTP client",
  "repo": "/home/u/code/shop",
  "worktree": "/home/u/.local/share/onerig/worktrees/shop/20261123-201500-a1f3",
  "branch": "onerig/20261123-201500-a1f3",
  "phase": "paused",
  "paused": {
    "reason": "no_lane",
    "resume_phase": "executing",
    "message": "deepseek: quota exhausted; gemini: manual lane already tried",
    "until": 1764000000000
  },
  "task_index": 1,
  "task_base": "3f2a9c1e...",
  "attempt": 2,
  "lane": "deepseek",
  "unknown_streak": 0,
  "skipped": [],
  "issues": [{ "file": "src/http.rs", "note": "Retry also fires on 4xx." }],
  "last_output_tail": "...",
  "lanes": { "deepseek": { "exhausted_until": null, "exhausted": true }, "gemini": {} }
}
```

`phase` is one of `planning`, `executing`, `reviewing`, `paused`, `done`,
`failed`. `paused` is present only in that phase. Times are unix milliseconds.

Each `log.jsonl` line:

```json
{"ts":1763929000000,"kind":"state","data":{"type":"phase","phase":"executing","task":1}}
{"ts":1763929001000,"task":1,"attempt":1,"lane":"deepseek","kind":"harness","data":{"type":"tool","name":"edit","detail":"src/http.rs"}}
{"ts":1763929090000,"task":1,"attempt":1,"lane":"deepseek","kind":"outcome","data":{"type":"failed","kind":"quota","message":"balance is empty","reset_at":null}}
{"ts":1763929090001,"task":1,"kind":"policy","data":{"type":"lane_exhausted","lane":"deepseek","until":null}}
```

`kind` is `state`, `harness`, `outcome` or `policy`. Enum values are written
in snake_case under `type`. The log is for display and audit and is never
read back to rebuild state.

## 13. Command line

```text
onerig run "<goal>" [--plain] [--config <path>]
onerig resume [<run id>] [--plain] [--config <path>]
onerig doctor [--config <path>]
```

- `resume` without an id takes the run with the greatest id that is not
  `done` or `failed`.
- `doctor` probes every lane whose mode allows a probe (binary, version,
  login, quota) and prints one line per lane; a manual lane is printed as
  "manual, not probed". It makes no model call.
- `run` and `resume` show the terminal UI when stdout is a terminal and
  `--plain` is not given. Otherwise they print one line per log record:

  ```text
  20:15:01 plan      3 tasks
  20:15:02 t1 a1 deepseek  started (deepseek-flash)
  20:15:09 t1 a1 deepseek  tool edit src/http.rs
  20:16:40 t1 a1 deepseek  done
  20:17:05 t1 review       approve
  20:17:05 t1 commit       4c1d2aa Add retry to the HTTP client
  20:21:30 done      branch onerig/20261123-201500-a1f3
  ```

  The last line starts with `done`, `failed` or `paused` (followed by the
  reason and the message).

Exit codes: 0 done, 1 failed, 2 usage or configuration error, 3 paused.

## 14. Terminal UI

One screen, fed by the same records as `--plain`:

```text
+ onerig  run 20261123-201500-a1f3   goal: add retry to the HTTP client -----+
| Tasks                      | t2  attempt 1  lane: deepseek (dsh)            |
|  ok  t1 Add retry helper   | > read src/http.rs                             |
|  >>  t2 Wire into client   | > edit src/http.rs                             |
|  ..  t3 Update docs        | Applied the retry helper to send() ...         |
|----------------------------|                                                |
| Lanes                      |                                                |
|  plan     claude  ok       |                                                |
|  deepseek dsh     ok       |                                                |
|  gemini   agy     manual   |                                                |
+ q quit (the run can be resumed) -------------------------------------------+
```

The only key is `q`. A quota bar is shown for a lane whose harness reports
quota, which in the first release means an `agy` lane in `direct` mode.

## 15. Safety

- **Vendor terms.** `direct` mode for `agy` is off by default and needs an
  explicit key. When it is enabled, the first run prints: Google's Antigravity
  terms and FAQ treat driving `agy` from another agent on a consumer sign-in
  as a breach that can lead to account suspension, and the user enables it at
  their own risk. In `off` and `handover` modes onerig starts no `agy`
  process.
- **Credentials.** onerig never reads a harness's credential store or
  keychain entry, never performs a login, never changes a harness's global
  configuration files, and stores no secret. Child processes inherit the
  user's environment except for the variables each adapter removes.
- **Blast radius.** Workers run unattended with edits and shell commands
  auto-approved, inside the harness's own sandbox. Text in the repository can
  steer a worker (prompt injection). What limits the damage is the harness
  sandbox, whose exact limits are **[unobserved]**, and the fact that the
  work is on a separate branch in a separate worktree. If a worker commits
  despite being told not to, the soft reset on approval folds that into the
  task commit. Planner and reviewer jobs get read-only tools.
- **Untrusted text.** Worker output and diffs are untrusted input to the
  reviewer. The reviewer cannot write, and its answer counts only when it
  passes the verdict checks.
- **Data leaving the machine.** Each attempt's header and the Lanes panel
  name the harness, so the user sees which vendor receives the code.
- **Logs.** Run files stay in `.onerig/`, which git ignores. They can contain
  whatever a harness printed.

## 16. Testing

- **Adapters.** All adapter methods are pure, so each is tested with captured
  output kept under `tests/fixtures/<harness>/`. Personal paths and anything
  secret are removed from a fixture by hand before it is committed.
- **State machine and policy.** Tested with the fake runner. Its script is a
  JSON list of steps, consumed in order:

  ```json
  [
    { "lane": "plan", "outcome": { "type": "done", "structured": { "tasks": [] } } },
    { "lane": "deepseek", "writes": [["src/a.txt", "hello"]], "events": [{ "type": "text", "text": "..." }],
      "outcome": { "type": "failed", "kind": "quota", "message": "empty", "reset_at": null } }
  ]
  ```

  `writes` are applied to the worktree before the outcome is returned. A
  `preflight` or `quota` answer per lane can be given in the same file. A
  lane with `harness = "fake"` and `script = "<path>"` is accepted only when
  the environment variable `ONERIG_TEST` is set, so the binary can be tested
  end to end. Time comes from a clock value passed into the run, so tests
  control it.
- **Git.** `worktree.rs` is tested against temporary repositories.
- **Driver.** Tested with small shell scripts as the child: normal exit,
  silence until the idle watchdog fires, a job that outlives the job timeout,
  a child that ignores SIGTERM.

## 17. Build steps

| Step | Content | Done when | Live harness |
|------|---------|-----------|--------------|
| 0 | Capture fixtures by hand on a toy repository: one plan call, one review call, one `dsh` task (stdout, stderr, exit code). | The three captures are in `tests/fixtures/` and sections 9.1 and 9.3 are corrected to match them. | yes, three small calls; needs the owner's go-ahead |
| 1 | Workspace, config, store, lock, driver, fake runner, state machine, policy including the manual lane, worktree, `--plain`, `resume`, signal handling. | `cargo test` has one test per row of the failure table, per selection rule and per pause reason. An end-to-end test runs `onerig run --plain` in a temporary repository with fake lanes and checks the exit code, one commit per task on `onerig/<id>`, an untouched user checkout, and that a kill followed by `onerig resume` finishes the run. | no |
| 2 | Claude adapter. | Fixture tests pass; a real plan and a real review complete on a toy repository. | yes |
| 3 | `dsh` adapter. | A real task ends as one commit after an approved review. From here the tool does useful work. | yes |
| 4 | `agy` `direct` mode: probe, quota report, run. | With the opt-in key, a real task runs on `agy` and a simulated quota failure moves it to `dsh`. | yes |
| 5 | Terminal UI and `doctor`. | The screen in section 14 shows a fake run; `doctor` prints one line per lane. | no |

Step 1 does not wait for step 0. If the owner declines the live calls, step 0
is folded into steps 2 and 3.

## 18. Later

- A check command run by onerig between worker and reviewer. It needs a
  decision on sandboxing, because it executes code a worker wrote.
- Resuming a harness's own conversation instead of starting fresh.
- A second DeepSeek route: Claude Code against DeepSeek's Anthropic-compatible
  endpoint, reusing the Claude adapter with a separate configuration
  directory.
- API-key authentication for `agy`.
- A separate "rate limited, wait and retry" failure kind, once a fixture shows
  a harness reports it apart from quota.
- A per-repository configuration file, more than one planner or reviewer
  lane, parallel workers, ACP, other harnesses, Windows.

## 19. Open questions

1. Does `--safe-mode` work with a subscription login in print mode, and how
   many tokens does a minimal planner call cost with and without it?
2. Does `dsh` accept the generated overlay, and where does it find its key on
   a machine where `DEEPSEEK_API_KEY` is not set?
3. What does each harness print on quota exhaustion? No fixture exists yet
   for any of the three.
4. Licence: MIT is assumed, matching copass.

## 20. Review log

Five independent reviewers read draft 1 (scope, design, comprehension,
structure, implementation readiness). What draft 2 changed:

- **Manual lane starts nothing.** In `off` and `handover` modes onerig runs no
  `agy` process at all. Draft 1 said so and then probed and read quota anyway.
- **One lane-selection rule and one attempt budget.** Draft 1 had three rules
  that contradicted each other and no limit on failed attempts.
- **Every pause has a reason and a way back.** Interrupts, crashes, login
  problems, git errors and exhausted budgets all pause; only a failed plan
  fails a run. A lock file stops two processes working on one run.
- **No harness conversation resume.** Every attempt is a fresh job with the
  full hand-over text; `Session` and `RateLimit` failure kinds are gone.
- **Task diff defined once**, staged so that new files are included, and the
  commit folds stray worker commits.
- **Adapter contract rebuilt** around a `Runner` seam so the fake can exist;
  adapters are fully pure; the driver owns timeouts.
- **Cut from the first release:** the check command, the "no change means
  failure" rule, `extra_args`, key injection and masking, the per-repository
  config file, the user pause key, token-usage events.
- **Pinned:** the `state.json` and log formats, the configuration table, the
  prompts, exit codes, and a "done when" for each build step.

Not adopted: bypassing commit hooks and signing with `--no-verify` (a failed
commit pauses the run instead), and making `api_key_env` a required lane key
(onerig handles no secrets at all in the first release).
