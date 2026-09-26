---
name: distill
description: "Cut the branch down to what the code cannot say: reuse what exists, remove duplication, dead code, and wasted work, turn invariants into asserts, and delete the comments the code already speaks. Run it after answering review feedback too — a fix written to satisfy a reviewer is the least distilled code on the branch."
---

# Distill

## Scope

```sh
B=$(git merge-base origin/main HEAD)
scripts/changed-sources.sh              # the files, one per line
git diff "$B" --stat -- $(scripts/changed-sources.sh)
git diff "$B" -- $(scripts/changed-sources.sh)
```

Every file that reports, whatever its type, plus any doc it made stale. Whatever
you notice on the way in is in scope too, pre-existing or not. A WEP keeps the
sections `docs/AGENTS.md` requires.

Generated files are the one exclusion, and the script is what applies it:
`.gitattributes` marks them `linguist-generated`, so a new generated path is
excluded the moment it is marked and nothing here lists paths a second time.
A plain `git diff` buries the branch under them — on a generator change they
outnumber the sources five to one. Read the sources; regenerate the rest.

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
- Contracts, not defences: a function states what it requires and trusts its
  callers. Defensive programming is banned. A default or a fallback whose
  validity you cannot argue is the smell. It turns a broken call into a wrong
  answer that no test will catch. Write `assert!` for what the caller owes and
  `unreachable!` for the arm that cannot happen, and say which caller
  establishes it where that is not obvious. A branch that genuinely can happen
  is control flow and stays; the ban is on inventing an answer for a case you
  have not shown to be reachable.

### Comments

Apply AGENTS.md § General Rules to every comment in scope. A comment saying
what the code does is a rename or a decomposition to make. A comment stating an
invariant is an assert to write. A comment that carries no information is
deleted outright.

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

## Sweep by shape

A copy does not share a name, so grepping the name finds nothing.

For every predicate or helper the branch adds or moves, grep the tree for what
its body looks like rather than what it is called. Two or three tokens of the
body carry further than a signature.

Finish the sweep in the same pass. Fixing the sites a reviewer named and calling
it a class fix leaves the rest standing, and the next review returns them one at
a time.

## Cycle

Three passes over that scope; surviving one is no exemption. Stop when a pass
finds nothing to cut.

## Finish

```sh
mise run format
```

Code edits: run the tests covering what you touched (`mise run test`,
`mise run test-wado`). Comment, doc, and Markdown edits alone need none.
