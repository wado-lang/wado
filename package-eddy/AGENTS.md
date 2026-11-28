# Eddy Development Guide

Eddy is a code review bot in Wado, on the Anthropic API for now. The
`wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) says how to run a review,
[docs/design.md](./docs/design.md) holds the decisions and directions, and
[TODO.md](./TODO.md) the open work.

## Architecture

Reviewer agents, one per focus, read the branch through Eddy's tools
(reading, searching and symbol lookup) and report candidates. One call merges
them and keeps the most severe, and a reproducer agent per candidate writes
and runs a test that shows it. A smaller reporter model then turns each
reproducer's finding into a structured record. Eddy tries each reproduction
again alone in a clean checkout to set `verified` and reports only the
verified findings, writes the patches and comments under
`scratchpad/eddy/<head>/`, records the run in the wiki, and prices the tokens
each model took.

## Evaluation Set

`eval/` holds the offline evaluation set, mined from this repository's history;
[its README](./eval/README.md) says what each file holds. The scripts in
`tools/` write those files. A run of one calls a paid API, so it runs only when
a person asks for it.
