---
name: pull-request
description: Project rules for opening a PR: a Conventional Commits title, and a description scoped to origin/main...HEAD (three-dot) with no trial-and-error history and no test section. Read before creating or editing any pull request.
---

## Title

Use the Conventional Commits style for pull request titles:

- `feat`: add a new feature
- `feat!`: add a new feature with a breaking change
- `fix`: bug fix
- `fix!`: bug fix with a breaking change
- `docs`: documentation-only changes
- `perf`: code change that improves performance
- `refactor`: code change that neither fixes a bug nor adds a feature
- `chore`: anything else (e.g. build process, dependencies)

It may include a scope, e.g. `feat(optimizer)`.

## Description

Describe the outcome of the whole branch (origin/main...HEAD -- use three dots).

Do not include trial-and-error history; the commits does.

No need to include a test section. CI runs the full test suite.
