---
name: git-upstream-sync
description: Sync current branch with origin/main when a GitHub PR has conflicts. Merges with zdiff3, commits conflict markers separately, then resolves conflicts.
---

# Git Upstream Sync

Resolve GitHub PR conflicts by merging origin/main into the current branch.

## Procedure

### 1. Fetch and merge with zdiff3

```sh
git fetch origin main
git merge --conflict=zdiff3 origin/main
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
2. Resolve the conflicts by editing the files (remove conflict markers, choose correct code)
3. Stage and commit the resolution as a separate commit:

```sh
git add -A
git commit -m "resolve merge conflicts"
```

## Important

- Always use `--conflict=zdiff3` so the merge base is visible in conflict markers
- Always commit the unresolved conflicts first, then resolve in a separate commit — this preserves a clear record of what the conflicts looked like vs how they were resolved
- Do NOT squash the two commits together
