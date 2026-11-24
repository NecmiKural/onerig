# onerig

One rig for every agent harness: Claude Code plans and reviews, cheaper coding
agents do the work, and when one runs out of quota the next one carries on.

onerig is a single Rust binary that drives the coding-agent CLIs you already
use (`claude`, `codex`, `agy`) as subprocesses. It never calls a model API
itself, so every model runs in the harness built for it, on your own login or
API key.

> **Status: early.** The whole loop (plan, work, review, commit, pause,
> resume) runs end to end with real harnesses: Claude Code plans and reviews,
> DeepSeek works through Codex, and `agy` works as a manual lane or, opt-in,
> driven directly (see the warning below).

## How a run works

```text
onerig run "add retries to the HTTP client"
```

1. **Plan.** Claude Code reads the repository (read-only tools) and splits the
   goal into small tasks with acceptance criteria.
2. **Work.** Each task goes to the first available worker lane, in a separate
   git worktree on its own branch, `onerig/<run id>`. Your checkout is never
   touched.
3. **Review.** Claude Code judges the task's diff against its acceptance
   criteria: approve, or revise with issues.
4. **Commit.** An approved task becomes one commit on the run branch. onerig
   never merges, pushes or deletes anything; when the run is done it prints
   the branch name.

When a worker's quota runs out, the task moves to the next worker lane with a
hand-over: the task, the diff so far, the previous agent's last output and any
review issues. When no lane is left, the run pauses and `onerig resume`
continues it later. Every hand-over is also written to
`<worktree>/.onerig/handover.md`, so you can give the task to any agent by
hand.

## Lanes

A lane is one harness plus its model and options. Roles name lanes:

```toml
[roles]
planner  = ["plan"]
reviewer = ["review"]
worker   = ["gemini", "deepseek"]   # the order is the fail-over order

[lanes.plan]
harness = "claude"
model = "sonnet"

[lanes.review]
harness = "claude"
model = "sonnet"
effort = "low"

[lanes.deepseek]
harness = "codex"                   # Codex CLI pointed at DeepSeek's API
model = "deepseek-flash"

[lanes.gemini]
harness = "agy"
mode = "handover"                   # off | handover | direct
model = "gemini-4-hydrogen-medium"
quota_group = "Gemini Models"
```

The file lives at `$XDG_CONFIG_HOME/onerig/config.toml`
(`~/.config/onerig/config.toml` by default); `--config` points elsewhere.

- **Claude Code** plans and reviews through `claude -p` on your own login.
- **DeepSeek** works through `codex exec` with its own `CODEX_HOME`, so your
  `~/.codex` and its login are left alone. Codex reads the key from
  `DEEPSEEK_API_KEY`; onerig never sees it.
- **Antigravity (`agy`)** is a manual lane by default: onerig writes the
  hand-over, pauses and tells you how to continue in `agy`, then reviews what
  you did after `onerig resume`.

### A warning about `agy` in `direct` mode

Google's Antigravity terms and FAQ treat driving `agy` from another agent or
orchestrator on a consumer (Google AI Pro) sign-in as a breach that can lead to
account suspension. `mode = "direct"` does exactly that, so it is off by
default and needs `accept_vendor_terms_risk = true` on the lane. Use it only if
you accept that risk for your own account.

## Install

```text
cargo install onerig
```

onerig needs git and the harness CLIs your lanes use (`claude`, `codex`,
`agy`), each already signed in. It reads `$XDG_CONFIG_HOME/onerig/config.toml`
(or `--config <file>`); when there is none, it prints an example to start from.

## Commands

```text
onerig run "<goal>"        start a run in the current repository
onerig resume [<run id>]   continue the latest (or the given) unfinished run
onerig doctor              check every lane; makes no model call
```

In a terminal, `run` and `resume` show a live screen with the tasks, the lanes
and the activity; `q` stops the run so that `resume` can continue it later.
`--plain` prints one line per event instead. Exit codes: 0 done, 1 failed,
2 usage or configuration error, 3 paused.

## Working with copass

[copass](https://github.com/NecmiKural/copass) hands a coding session from one
agent to another. The two tools are independent: onerig never calls copass,
and copass reads onerig's run files (`.onerig/runs/<run id>/state.json` and
`plan.json`, a versioned format) through its own adapter. From a run's
worktree, `copass --from onerig --to codex` continues the current task in
another agent.

## Development

```text
cargo test --workspace
```

The design, including what is still open, is in
[`docs/designs/design_draft.md`](docs/designs/design_draft.md). The work in
progress and the next step are tracked in [`docs/STATUS.md`](docs/STATUS.md).

## License

MIT
