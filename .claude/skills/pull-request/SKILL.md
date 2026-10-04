---
name: pull-request
description: The rules for opening a PR you must read before creating or editing any pull request.
---

# Pull Request

## First: An Open Question Is a Stop

If a question you put to the user is unanswered, do not open or edit a pull
request. End the turn by asking for the answer. The user decides what is minor.

## Before Writing

Read the branch's own changes:

```sh
git diff origin/main...HEAD -- $(scripts/changed-sources.sh)
```

The title and description come from that diff, not from the session. Say that
generated output was regenerated, not what moved inside it. Tidy the branch's
comments and docs while there.

After `git fetch origin main`,
`git merge-tree --write-tree --no-messages --name-only HEAD origin/main` exits 1
on a conflict, touching nothing; resolve one with the `git-upstream-sync` skill.

## Title

`<type>(<scope>): <the value>`, naming the value the branch creates, not what it
edited; the largest one if there are several. The scope is optional, and `!`
marks a breaking change. The type is one of:

- `feat`
- `fix`
- `docs`
- `perf`
- `refactor`
- `chore`

## Description

Open with the outcome, in a paragraph a reader can stop after; mechanism goes
under headings below it.

Write for someone who sees only the merged tree. A sentence that parses only
against the old state ("previously X, now Y", "2 -> 0", an earlier approach) is
history, which the commits hold. Cut it.

- No: "Codegen looked the global up by name; it now compares the read's type."
- Yes: "Codegen compares the read site's `result_ty` against the slot's type."

Add `Closes #N` when the branch obviously closes an issue; don't go looking. No
test section: CI runs the suite. The GitHub MCP server drops angle brackets and
escapes quotes in what it reads back, so check the web UI before rewriting.

Cut the draft before posting: a first draft follows the shape of the work.

## After Opening

Subscribe with `subscribe_pr_activity` and handle every event; skipping one is a
decision you state. Without the tool, check status and reviews every 10 minutes
until the review settles and CI passes. Keep the branch mergeable with
`git-upstream-sync`, and answer reviews with `code-review-response`.
