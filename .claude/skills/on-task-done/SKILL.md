---
name: on-task-done
description: "Task-completion flow: `/cr` (an xhigh review and its response), update docs (spec/cheatsheet/compiler/optimizer), then run `mise run on-task-done` (build, clippy-fix, golden + format fixtures, doc-stdlib, format, tests; the longest task there is) and commit its generated changes. Invoke ONLY when the user asks for it by name or explicitly asks to run the completion flow — never on your own initiative, and not because a task looks finished."
---

# On Task Done

1. `/cr`.
2. Update what the branch made stale: `docs/spec-*.md`, `docs/cheatsheet.md`,
   and `docs/compiler.md` / `docs/optimizer.md` only when a phase, pass, IR, or
   cross-phase rule changed.
3. Run `mise run on-task-done` (the whole suite and every generator; see
   `mise.toml`) and
   commit what it generates. A Wado-only change needs only `mise run test-wado`;
   a docs-only change, `mise run format` and `mise run check`.

## Flaky Commands

`update-golden-fixtures` (`golden-dump`) and the e2e runner of `mise run test`
sometimes segfault. Regenerate a new fixture's golden by hand, and rerun the
e2e tests with `cargo test -p wado-compiler --test e2e -- --test-threads=4`, or
one at a time by fixture name.
