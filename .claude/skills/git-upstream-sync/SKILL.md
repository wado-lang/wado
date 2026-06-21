---
name: git-upstream-sync
description: Sync current branch with the upstream origin/main when a GitHub PR has conflicts
---

# Overview

Resolve GitHub PR conflicts by merging origin/main into the current branch.

## Procedure

### 1. Fetch and merge with zdiff3

```sh
git fetch origin main
git -c merge.conflictstyle=zdiff3 merge origin/main
```

### 2. If conflicts exist, commit them as-is

If the merge produces conflicts, **commit the conflict markers without resolving them first**. This records the raw conflict state in a dedicated commit, separate from the resolution.

```sh
git add -A
git commit -m "merge origin/main (conflicts unresolved)"
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

- **On a golden / generated-file conflict, always regenerate and commit.**
  Re-run the generator (do not hand-resolve the markers) and commit its output.
  Do not skip them or defer them to `on-task-done` / CI.
- **When there is no conflict, follow the task's explicit instruction** for
  whether to regenerate. Regeneration is not always required; never let a generic
  "always regenerate" or "CI handles it" default override what you were asked to do.
- **Never silently discard generated output** (e.g. `git restore` to clean the
  tree or quiet a hook). Commit it, or ask. Discarding generated work without
  asking is always wrong.
