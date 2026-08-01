---
name: pull-request
description: The rules for opening a PR you must read before creating or editing any pull request.
---

## Before writing

Read `git diff origin/main...HEAD` (three dots). The description comes from that
diff, not from the session that produced it.

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

## PR Title

Use the Conventional Commits style for pull request titles:

- `feat`: add a new feature
- `feat!`: add a new feature with a breaking change
- `fix`: bug fix
- `fix!`: bug fix with a breaking change
- `docs`: documentation-only changes
- `perf`: code change that improves performance
- `refactor`: code change that neither fixes a bug nor adds a feature
- `chore`: anything else (e.g. CI, build process, dependencies)

It may include a scope, e.g. `feat(optimizer)`.

## PR Description

Describe the outcome of the whole branch — what holds once it is merged.

Do not include trial-and-error history in the branch; the commit history is the
SSoT. That is any sentence which only parses against the pre-branch state:
"previously X, now Y", "an earlier approach", "X was replaced by Y", a count
given as a delta ("2 -> 0"). Read each sentence back and ask whether it works
for someone who sees only the merged tree. If it needs the old state, cut it.

- No: "Codegen looked the global up by name; it now compares the read's type."
- Yes: "Codegen compares the read site's `result_ty` against the slot's type."

If the branch obviously closes a known issue, add a closing keyword
(`Closes #N`). Do not go looking for one to attach.

No need to include a test section. CI runs the full test suite.

## After opening & Periodic status checks

Check mergeability (`mergeable_state`). If conflicting, resolve it with the
`git-upstream-sync` skill.
