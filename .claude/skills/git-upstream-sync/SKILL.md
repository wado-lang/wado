---
name: git-upstream-sync
description: The only way to merge origin/main into a branch, conflicts or not. Use it every time main comes in — a PR reported conflicting or DIRTY, a branch behind origin/main, a CI failure to reproduce on the merged tree. Never run `git merge origin/main` or `git pull origin main` by hand.
---

# Upstream Sync

Every step runs whether or not the merge conflicts.

## 1. Merge

If the branch is on `origin`, merge it first: CI's `tidy` job may have pushed
onto it.

```sh
git pull --no-rebase origin "$(git branch --show-current)"   # pushed branches only
git fetch origin main
git -c merge.conflictstyle=zdiff3 merge origin/main
```

## 2. Commit the conflicts unresolved

The raw conflict gets its own commit, so the resolution reads as a diff against
it. Never squash the two.

```sh
git add -A
git diff --cached origin/main -- vendor
git commit -m "merge origin/main (conflicts unresolved)"
```

The `vendor` diff prints nothing unless the branch bumps a submodule itself.
Otherwise `git add -A` staged a stale checkout and reverts main's bump; restore
main's pointer first:

```sh
git update-index --cacheinfo 160000,<sha from origin/main>,<path>
git submodule update --init --recommend-shallow <path>
```

## 3. Resolve

Resolve code by reading both sides. Regenerate a generated file by re-running
its generator; never hand-resolve, defer, or discard one. Then:

```sh
git add -A
git commit -m "resolve merge conflicts"
```

## 4. Sanity check

`mise run test`. CI applies clippy and format.
