# onerig chat — concepts brief

Status: agreed with the owner on 2026-11-25. The design is in
`design_draft.md` next to this file.

## The problem

`onerig run "<goal>"` works end to end, but it is a one-shot command with a
dashboard. The owner wants onerig to feel like the coding agents it drives
(Claude Code, Codex, pi, opencode, Antigravity): open it in a repository, type
to it, read the answers, and watch the work happen in one place.

## What the owner asked for

1. **An interactive terminal app.** `onerig` with no arguments opens a chat
   screen in the current repository. Prompts are typed there.
2. **A record of who did what.** Every message, plan, tool call, review and
   commit is kept on disk and shown in the conversation, labelled with who
   sent it and to whom.
3. **Agents know where a prompt came from.** A prompt that onerig writes on
   an agent's behalf says so; a prompt typed by the owner says that it comes
   from a person. The screen marks agent-written prompts the same way.
4. **Microsoft 365 Copilot as a worker**, through
   [m365-copilot-proxy](https://github.com/cramt/m365-copilot-proxy).

## Decisions (owner, 2026-11-25)

| Question | Decision |
|----------|----------|
| Layout | The conversation on the left; the tasks and the lanes in a panel on the right; the input box along the bottom. |
| Where a message goes | To the planner (Claude Code) by default. `@<lane> <text>` sends it straight to that lane. Every agent is told whether the message came from the owner or from another agent. |
| When workers start | Only after the owner approves the plan. A correction typed while a plan waits goes back to the planner, which revises the plan. |
| M365 Copilot | A worker lane that drives `pi` pointed at the proxy, the path the proxy itself documents and verifies. Opt-in with an explicit risk flag, like `agy` direct mode, because it uses a work account through an unofficial client. |

## Constraints carried over

- onerig drives harness CLIs; it never calls a model API itself. The proxy is
  reached only through `pi`.
- `onerig run`, `resume` and `doctor` keep working unchanged for scripts.
- Rust, a single binary, ratatui. No new runtime.
- Workers stay in their own git worktree; the owner's checkout is never
  touched.
- copass stays independent and keeps reading the run files it reads today.

## Out of scope for this step

- More than one run working at the same time in one session.
- Steering a worker while its process runs (pi's RPC mode could allow it
  later).
- Images and file attachments in the chat.
- A second planner harness: the planner stays Claude Code.
