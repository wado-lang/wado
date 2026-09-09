---
name: pull-request
description: The rules for opening a PR you must read before creating or editing any pull request.
---

## Before writing

Read `git diff origin/main...HEAD` (three dots). The title and description come
from that diff, not from the session that produced it.

Revise the branch while you are there: clean up comments and docs according to
the project rules.

Check mergeability by exit status (after `git fetch origin main`):

```sh
git merge-tree --write-tree --no-messages --name-only HEAD origin/main
```

Exit 0 = mergeable; exit 1 = conflicts, printing the merged tree OID followed
by one conflicted path per line. This runs the real (ort) merge in memory and
touches neither the worktree nor the index.

If conflicting, resolve with the `git-upstream-sync` skill.

## Title

One line saying what the branch is worth. Not what was edited — a reader
scanning a list of PRs is deciding whether to care, and an inventory of the
change does not help them decide.

`<type>(<scope>): <the value>`

- No: `refactor(nir): thread a frame stack through the arena visitors`
- Yes: `perf(nir): 7 % off a debug compile of the SQLite parser`
- No: `fix(elaborator): add an arity check to impl resolution`
- Yes: `fix(elaborator): an impl on [] no longer matches ()`

Where the value is a number, the number is the title. Where it is a behaviour,
name the behaviour. Where a branch does two unrelated things worth naming, name
both and keep it to one line; where it does five, the title names the largest
and the description carries the rest.

`type` is `feat`, `fix`, `docs`, `perf`, `refactor` or `chore`, with `!` for a
breaking change (`feat!`, `fix!`). The scope is optional.

## Description

Open with the outcome, in a paragraph a reader can stop after. What holds once
this is merged, and what it is worth — the numbers if the value is a number, the
behaviour if it is a behaviour. Mechanism comes after, under headings.

Do not include trial-and-error history in the branch; the commit history is the
SSoT. That is any sentence which only parses against the pre-branch state:
"previously X, now Y", "an earlier approach", "X was replaced by Y", a count
given as a delta ("2 -> 0"). Read each sentence back and ask whether it works
for someone who sees only the merged tree. If it needs the old state, cut it.

- No: "Codegen looked the global up by name; it now compares the read's type."
- Yes: "Codegen compares the read site's `result_ty` against the slot's type."

The same test applies to the opening paragraph, and it is where the temptation
is worst: a speedup is worth stating, the struggle to find it is not.

If the branch obviously closes a known issue, add a closing keyword
(`Closes #N`). Do not go looking for one to attach.

No need to include a test section. CI runs the full test suite.

Angle brackets need nothing but a code span: `` `t_<Name>` `` renders as
written. The GitHub MCP server mangles the text it reads back — it drops the
brackets and HTML-escapes quotes — so check the web UI before believing the
description is broken, and never rewrite prose to work around it.

## After opening & Periodic status checks

Subscribe to the PR with `subscribe_pr_activity`. Handle every event it
delivers; skipping one is a decision you state. If the tool is unavailable, say
so when reporting the PR rather than implying you are watching it.

Check mergeability (`mergeable_state`). If conflicting, resolve it with the
`git-upstream-sync` skill.

Answer a review, human or bot, with the `code-review-response` skill.
