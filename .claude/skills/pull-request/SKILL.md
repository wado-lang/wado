---
name: pull-request
description: The rules for opening a PR you must read before creating or editing any pull request.
---

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

Describe the outcome of the whole branch (`origin/main...HEAD` -- use three dots).

Do not include trial-and-error history in the branch; the commit history is the SSoT.

Add closing keywords for any issues that are resolved by this PR.

No need to include a test section. CI runs the full test suite.

## Before opening

Revise the branch with `git diff origin/main...HEAD` and clean up comments and docs according to the project rules.

Check mergeability with `git merge-tree $(git merge-base HEAD origin/main) HEAD origin/main`. If conflicting, resolve it with the `git-upstream-sync` skill.

## After opening & Periodic status checks

Check mergeability (`mergeable_state`). If conflicting, resolve it with the
`git-upstream-sync` skill.
