---
name: cleanup-comments-and-docs
description: "Cut the branch's comments, doc comments, and Markdown down to the minimum — delete anything the code already says, cap doc/module comments at 3 lines and each Markdown topic at 2. Mandatory: run it after finishing a requested task, before reporting back to the user."
---

## Scope

```sh
git diff origin/main...HEAD --stat
```

Every file in that diff — Rust, Wado, Markdown — plus any doc it made stale.

## Rules

- Comments: delete what is readable from the code. Express intent through naming,
  structure, and assertions instead; an assert is checked, a comment goes stale.
- Doc and module comments: 3 lines max. Say what it is, not how it works.
- Markdown: correct, fresh, concise. 2 lines max per topic.

## Cycle

Three passes over the diff, each re-reading what the last one left; surviving one
pass is no exemption. Stop when a pass finds nothing to remove.

## Finish

```sh
mise run format
```

Re-run the tests only when a refactor touched the code itself; comment and doc
edits alone need none.
