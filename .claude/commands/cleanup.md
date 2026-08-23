---
description: "Cut the branch down to what the code cannot say: delete redundant comments, turn invariants into asserts, and remove duplication."
argument-hint: "[paths…] — defaults to the branch diff"
---

# Cleanup

## Scope

`$ARGUMENTS`, or when that is empty:

```sh
git diff origin/main...HEAD --stat
```

Every file in that scope — Rust, Wado, Markdown — plus any doc it made stale.
Follow the `rust` skill for Rust and the `wado` skill for Wado.

## Rules

### Code

- Duplication: one behaviour, one implementation. Merge copies that drifted;
  hoist the shared part of near-copies behind the difference — a parameter, a
  closure, an enum. Don't abstract a single use.
- Naming and structure: rename and decompose until the comment is redundant,
  then delete the comment. A comment explaining _what_ the code does marks the
  code to fix, not the comment.
- Invariants: state them as `assert!`, never as a comment. An assert is checked;
  a comment goes stale.

### Comments

- Delete what is readable from the code.
- Doc and module comments: 3 lines max. Say what it is, not how it works.

### Markdown

- Correct, fresh, concise. 2 lines max per topic.

## Cycle

Three passes over the scope, each re-reading what the last one left; surviving a
pass is no exemption. Stop when a pass finds nothing to cut.

## Finish

```sh
mise run format
```

Code edits: run the tests covering what you touched (`mise run test`,
`mise run test-wado`). Comment, doc, and Markdown edits alone need none.
