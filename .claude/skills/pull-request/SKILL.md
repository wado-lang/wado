---
name: pull-request
description: Conventions and best practices for pull requests to the project
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

## Description

Describe the branch's changes as a whole, not commit by commit. Do not include trial-and-error history.

No need to include a test plan; CI runs the full test suite.
