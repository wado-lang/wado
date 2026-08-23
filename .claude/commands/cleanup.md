---
description: "Cut the branch down to what the code cannot say: delete redundant comments, turn invariants into asserts, and remove duplication."
argument-hint: "[extra instructions]"
---

# Cleanup

$ARGUMENTS

## Scope

```sh
git diff origin/main...HEAD --stat
```

Every file in that diff — Rust, Wado, Markdown — plus any doc it made stale.
Whatever you notice on the way in is in scope too: pre-existing is no
exemption.
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

- Delete outright what carries no information or repeats the code; trim only
  what survives that.
- Doc and module comments: 2 lines max. Say what it is, not how it works.

### Markdown

- Correct, fresh, concise. 3 lines max per topic.

## Cycle

Three passes over the diff, each re-reading what the last one left; surviving a
pass is no exemption. Stop when a pass finds nothing to cut.

## Finish

```sh
mise run format
```

Code edits: run the tests covering what you touched (`mise run test`,
`mise run test-wado`). Comment, doc, and Markdown edits alone need none.
