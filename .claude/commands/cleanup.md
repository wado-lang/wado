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
Whatever you notice on the way in is in scope too, pre-existing or not.
Generated files and WEPs defer to their directory's `AGENTS.md`: don't touch a
generated corpus, and don't cut a WEP's required sections.

## Rules

### Code

- Duplication: one behaviour, one implementation. Hoist the shared part of
  near-copies behind the difference — a parameter, a closure, an enum. Don't
  abstract a single use.
- Naming and structure: a comment explaining _what_ the code does marks the code
  to fix. Rename and decompose until it is redundant, then delete it.
- Invariants: state them as `assert!`, never as a comment.

### Comments

- Delete outright what carries no information or repeats the code; trim only
  what survives that.
- Doc and module comments: 2 lines max. Say what it is, not how it works.

### Markdown

- Correct and fresh. Cut the narration; keep the facts.

## Cycle

Three passes over the diff; surviving one is no exemption. Stop when a pass
finds nothing to cut.

## Finish

```sh
mise run format
```

Code edits: run the tests covering what you touched (`mise run test`,
`mise run test-wado`). Comment, doc, and Markdown edits alone need none.
