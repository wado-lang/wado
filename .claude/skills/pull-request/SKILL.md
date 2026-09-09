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

One line saying what the branch is worth, not what was edited. A reader scanning
a list of PRs is deciding whether to care.

`<type>(<scope>): <the value>`

- No: `refactor(nir): thread a frame stack through the arena visitors`
- Yes: `perf(nir): 7 % off a debug compile of the SQLite parser`
- No: `fix(elaborator): add an arity check to impl resolution`
- Yes: `fix(elaborator): an impl on [] no longer matches ()`

Where the value is a number, the number is the title. Name two things if two are
worth it, still on one line. If five are, name the largest and leave the rest to
the description.

`type` is `feat`, `fix`, `docs`, `perf`, `refactor` or `chore`, with `!` for a
breaking change. The scope is optional.

## Description

Open with the outcome, in a paragraph a reader can stop after: what holds once
this is merged, and what it is worth. Mechanism comes after, under headings.

Do not include trial-and-error history in the branch; the commit history is the
SSoT. That is any sentence which only parses against the pre-branch state:
"previously X, now Y", "an earlier approach", "X was replaced by Y", a count
given as a delta ("2 -> 0"). Read each sentence back and ask whether it works
for someone who sees only the merged tree. If it needs the old state, cut it.

- No: "Codegen looked the global up by name; it now compares the read's type."
- Yes: "Codegen compares the read site's `result_ty` against the slot's type."

The opening paragraph is the hardest place to hold that line: a speedup is worth
stating, the struggle to find it is not.

If the branch obviously closes a known issue, add a closing keyword
(`Closes #N`). Do not go looking for one to attach.

No need to include a test section. CI runs the full test suite.

Angle brackets need nothing but a code span: `` `t_<Name>` `` renders as
written. The GitHub MCP server drops them and HTML-escapes quotes in the text it
reads back, so check the web UI before believing the description is broken, and
never rewrite prose to work around it.

Cut the draft before posting. A first draft follows the shape of the work: a
heading for each thing that happened, at the length it took to do. Read it back
and cut every sentence a reader would skip.

## After opening

Subscribe to the PR with `subscribe_pr_activity`. Handle every event it
delivers; skipping one is a decision you state. If the tool is unavailable, say
so when reporting the PR rather than implying you are watching it.

Keep checking mergeability (`mergeable_state`). If conflicting, resolve it with
the `git-upstream-sync` skill.

Answer a review, human or bot, with the `code-review-response` skill.
