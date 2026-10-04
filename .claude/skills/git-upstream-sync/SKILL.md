---
name: git-upstream-sync
description: The only way to merge origin/main into a branch, conflicts or not. Use it every time main comes in — a PR reported conflicting or DIRTY, a branch behind origin/main, a CI failure to reproduce on the merged tree. Never run `git merge origin/main` or `git pull origin main` by hand.
---

# Upstream Sync

A clean merge skips steps 2 and 3 but still ends with step 4.

## 1. Merge

If the branch is on `origin`, merge it first: CI's `tidy` job may have pushed
onto it.

```sh
git pull --no-rebase origin "$(git branch --show-current)"   # pushed branches only
git fetch origin main
git -c merge.conflictstyle=zdiff3 merge origin/main
git submodule update --init --recommend-shallow
```

## 2. If It Conflicts, Commit the Conflicts Unresolved

The raw conflict gets its own commit, so the resolution reads as a diff against
it. Never squash the two. Stage, then check the submodules before committing;
this check follows every `git add -A` here, step 3's included:

```sh
git add -A
git diff --cached origin/main -- vendor
```

That prints nothing unless the branch bumps a submodule itself. Anything else is
a stale checkout `git add -A` staged, reverting main's bump; restore main's
pointer for each such path:

```sh
git update-index --cacheinfo 160000,"$(git rev-parse origin/main:<path>)",<path>
```

Then commit:

```sh
git commit -m "merge origin/main (conflicts unresolved)"
```

## 3. Resolve

Resolve code by reading both sides. Regenerate a generated file by re-running
its generator; never hand-resolve, defer, or discard one. Then:

```sh
git add -A
git diff --cached origin/main -- vendor   # as in step 2
git commit -m "resolve merge conflicts"
```

## 4. Sanity Check

`mise run test`. CI applies clippy and format.
