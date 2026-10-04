---
name: distill
description: "Cut the branch down to what the code cannot say: reuse what exists, remove duplication, dead code, and wasted work, turn invariants into asserts, and delete the comments the code already speaks. Run it after answering review feedback too — a fix written to satisfy a reviewer is the least distilled code on the branch."
---

# Distill

## Scope

```sh
B=$(git merge-base origin/main HEAD)
git diff "$B" --stat -- $(scripts/changed-sources.sh)
git diff "$B" -- $(scripts/changed-sources.sh)
```

The whole branch every time, never the diff since the last pass, plus any doc it
made stale and anything you notice on the way, pre-existing or not.
`changed-sources.sh` leaves out the generated files; regenerate those.

## Rules

Distilling keeps behaviour. Its one exception is a bug fix, which starts from a
failing test.

### Code

- Reuse what the codebase already has.
- One behaviour, one implementation: hoist near-copies behind their difference.
  Don't abstract a single use.
- A special case layered on shared infrastructure sits too shallow; generalize
  the mechanism.
- Delete dead code and wasted work. A stored closure pins all it captured.
- Contracts, not defences. A default or fallback whose validity you cannot argue
  turns a broken call into a wrong answer: `assert!` what the caller owes,
  `unreachable!` the arm that cannot happen. A branch that can happen is control
  flow and stays.

### Comments

A comment saying what the code does is a rename or a decomposition to make; one
stating an invariant is an assert to write; one carrying nothing is deleted.

### Markdown

Apply the `markdown` skill. In an instruction file (an `AGENTS.md`, a skill),
also cut a rule that is trivially deduced from a principle the file or the root
`AGENTS.md` already states: detail dilutes the rules that matter. Keep one that
fences a principle against a reading broader than intended. Cut words, never a
fact: grep for what links to a passage before removing it, and keep a code
block runnable on its own, by running it.

## Sweep by Shape

A copy does not share a name. For every helper the branch adds or moves, grep
for two or three tokens of its body, and fix every hit in the same pass.

## Cycle

Repeat over the whole scope until a pass finds nothing to cut.

## Finish

`mise run format`. Code edits run the tests covering them (`mise run test`,
`mise run test-wado`); doc edits alone need `mise run check`.
