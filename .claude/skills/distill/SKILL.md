---
name: distill
description: "Cut the branch down to what the code cannot say: reuse what exists, remove duplication, dead code, and wasted work, turn invariants into asserts, and delete the comments the code already speaks. Run it after answering review feedback too — a fix written to satisfy a reviewer is the least distilled code on the branch."
---

# Distill

## Scope

```sh
git diff "$(git merge-base origin/main HEAD)" --stat
git status --short
```

Every file those report, whatever its type, plus any doc it made stale. Whatever
you notice on the way in is in scope too, pre-existing or not. Generated files
are the one exclusion — `.gitattributes` marks them. A WEP keeps the sections
`docs/AGENTS.md` requires.

This is the scope on every run. Distilling again means the whole branch again,
never the diff since the last distill: an earlier pass is not a clean bill, and
what the code between the two commits made stale is spread across everything the
branch touched.

Answering review feedback is one of the times to run it. Such a fix is written
to satisfy a reviewer rather than to fit the code, so it arrives with the
reviewer's framing in its comments, an explanation of the bug beside the fix,
and often a helper the codebase already had.

## Rules

### Code

- Reuse: don't re-implement what the codebase already has. Grep the shared
  modules and the files next to the change, and call the existing helper.
- Duplication: one behaviour, one implementation. Hoist the shared part of
  near-copies behind the difference — a parameter, a closure, an enum. Don't
  abstract a single use.
- Altitude: a special case layered on shared infrastructure means the fix sits
  too shallow. Generalize the mechanism instead.
- Dead code: delete what the change left behind.
- Efficiency: cut wasted work — recomputation, repeated I/O, independent work
  run in sequence. A stored closure pins everything it captured; prefer a struct
  holding only the fields it needs.
- Naming and structure: a comment explaining _what_ the code does marks the code
  to fix. Rename and decompose until it is redundant, then delete it.
- Invariants: state them as an assertion — `assert!` in Rust, `assert` in Wado
  — never as a comment.

### Comments

- Delete outright what carries no information or repeats the code; trim only
  what survives that.
- Doc and module comments: 2 lines max. Say what it is, not how it works.

### Markdown

The goal is prose a reader understands on the first pass. Everything below
serves that.

- Plain words. One idea per sentence. The plain statement first, the reason for
  it after.
- Three habits make a reader decode instead of read: a second clause hung off a
  dash, an abstract noun standing where a verb would do, and the clever phrasing
  of a point arriving before the obvious one. Undo each where you find it.
- Correct and fresh. Keep the facts.
- Cutting narration and redundancy is one way to get there. It is not the point.
  A passage that came out shorter and harder to follow has failed.

## Cycle

Three passes over that scope; surviving one is no exemption. Stop when a pass
finds nothing to cut.

## Finish

```sh
mise run format
```

Code edits: run the tests covering what you touched (`mise run test`,
`mise run test-wado`). Comment, doc, and Markdown edits alone need none.
