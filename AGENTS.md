# Notes for coding agents

onerig is a Rust workspace (`crates/onerig-core` library, `crates/onerig`
binary) that drives coding-agent CLIs as subprocesses: Claude Code plans and
reviews, cheaper harnesses do the work in a separate git worktree, and when
one runs out of quota the next one continues.

Start here:

1. `docs/STATUS.md`: what is done, what is next, and the rules for
   continuing (tests first, quality gates, commit style without attribution
   lines, live runs only with the owner's go-ahead).
2. `docs/designs/chat/design_draft.md`: the design being built now.
3. `docs/designs/design_draft.md`: the shipped design.

Update `docs/STATUS.md` in the same commit as the work it describes.
