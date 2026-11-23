# Captured harness output

Real output captured on 2026-11-24 with the commands below, run on a toy
Python repository (`calc.py` with `add`, goal: add `subtract` with a test).
Personal paths are replaced with `/home/user` and `/work`, and the `init`
event's lists of the capturer's plugins, skills and commands are removed.
Every job wrote nothing to stderr and exited with code 0, except the
deliberate authentication failure.

| File | Command |
|------|---------|
| `claude/plan.jsonl` | `claude -p --safe-mode --model sonnet --effort medium --output-format stream-json --verbose --json-schema <PLAN_SCHEMA> --permission-mode dontAsk --no-session-persistence --append-system-prompt <role> --tools Read Glob Grep --disallowedTools AskUserQuestion` (Claude Code 2.1.330, prompt on stdin) |
| `claude/review.jsonl` | the same with `--effort low` and `<VERDICT_SCHEMA>` |
| `agy/usage.json` | `agy -p /usage --output-format json --print-timeout 60s` (agy 1.5.14, `AGY_CLI_DISABLE_AUTO_UPDATE=true`); zero tokens, no conversation |
| `agy/task.jsonl` | `agy -p <prompt> --dangerously-skip-permissions --sandbox --output-format stream-json --model gemini-4-hydrogen-medium --print-timeout 30m` (agy 1.5.14, same environment); conversation id replaced |
| `codex/deepseek-auth-failure.jsonl` | the same, with `DEEPSEEK_API_KEY` set to an invalid test key; exit code 1 |
| `codex/deepseek-task.jsonl` | `codex exec --json --skip-git-repo-check --sandbox workspace-write --model deepseek-flash -` (Codex 0.171.0, `CODEX_HOME` with a DeepSeek provider: `base_url = "https://api.deepseek.com/"`, `wire_api = "responses"`, `env_key = "DEEPSEEK_API_KEY"`, no model catalog) |

Notes for the adapters:

- Claude emits a `rate_limit_event` on a successful call too, with
  `utilization` and `resetsAt` (unix seconds) for the five-hour and
  seven-day windows.
- Claude's final `result` line has `stop_reason: "tool_use"` because the
  structured answer arrives through the `StructuredOutput` tool;
  `subtype: "success"` and `is_error: false` decide.
- Codex reports a missing model catalog as an `item.completed` event of type
  `error` and carries on; it is not a failure of the job.
- Codex retries an HTTP error five times, each reported as a top-level
  `error` event starting with `Reconnecting...`; only `turn.failed` is final.
- `agy`'s usage report gives each quota group's buckets with
  `remaining_fraction` and an ISO `reset_time`; an unused bucket reports
  `1970-01-01T00:00:00Z`, which means no reset time.
- `agy`'s task stream names each line by `event`: `init`, `step_update`,
  `result`. A tool step arrives as `ACTIVE`, then `DONE` with the same
  `step_index`; file tools name their target in `parameters.Path`, the
  terminal tool in `parameters.CommandLine`. Answer text streams as
  `agent_response` deltas, and the final `result.response` repeats it whole.
