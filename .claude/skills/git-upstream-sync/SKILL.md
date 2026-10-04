---
name: git-upstream-sync
description: The only way to merge origin/main into a branch, conflicts or not. Use it every time main comes in — a PR reported conflicting or DIRTY, a branch behind origin/main, a CI failure to reproduce on the merged tree. Never run `git merge origin/main` or `git pull origin main` by hand.
---

# Overview

Merge origin/main into the current branch. Follow every step whether or not the
merge conflicts: a clean merge still ends with the sanity check.

## Procedure

### 1. Fetch and merge with zdiff3

Pull the branch's own remote first. CI's `tidy` job may have pushed a
`chore: tidy` commit onto it, and a push after the sync would then be rejected.

```sh
git pull origin "$(git branch --show-current)"
git fetch origin main
git -c merge.conflictstyle=zdiff3 merge origin/main
```

### 2. If conflicts exist, commit them as-is

If the merge produces conflicts, **commit the conflict markers without resolving them first**. This records the raw conflict state in a dedicated commit, separate from the resolution.

```sh
git add -A
git diff --cached origin/main -- vendor
git commit -m "merge origin/main (conflicts unresolved)"
```

The `git diff` must print nothing unless the branch bumps a submodule itself. When main bumps a `vendor/*` submodule, the
merge leaves its checkout at the old commit, and `git add -A` stages that,
silently reverting main's bump. Restore main's pointer before committing:

```sh
git update-index --cacheinfo 160000,<sha from origin/main>,<path>
git submodule update --init --recommend-shallow <path>
```

### 3. Resolve conflicts

After committing the unresolved state:

1. Read each conflicted file and understand both sides of the conflict
2. Resolve conflicts **in code files** (remove conflict markers, choose correct code)
3. For **generated files** (golden fixtures, generated parsers, …) that conflict,
   **regenerate them** by re-running their generator and commit the fresh output —
   do not hand-resolve the markers or defer them (see "Generated files" below)
4. Stage and commit the resolution:

```sh
git add -A
git commit -m "resolve merge conflicts"
```

### 4. Run `mise run test` for sanity check

CI applies clippy and format, so a quick sanity check is sufficient.

## Important

- Always use `-c merge.conflictstyle=zdiff3` so the merge base is visible in conflict markers (with zealous zdiff3 reducing noise)
- Always commit the unresolved conflicts first, then resolve in a separate commit — this preserves a clear record of what the conflicts looked like vs how they were resolved
- Do NOT squash the two commits together

## Generated files

- **Always regenerate golden / generated files and commit.**
  Re-run the generator (do not hand-resolve the markers) and commit its output.
  Do not skip them or defer them to `on-task-done` / CI.
- **Never silently discard generated output** (e.g. `git restore` to clean the
  tree or quiet a hook). Commit it, or ask. Discarding generated work without
  asking is always wrong.
