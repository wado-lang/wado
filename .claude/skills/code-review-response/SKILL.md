---
name: code-review-response
description: "How to answer code review feedback — a human reviewer, CodeRabbit, or any review bot. Verify each finding before acting, fix only what is real, put design decisions to the user, then run /distill. Invoke when responding to review comments on a pull request, or to the findings of a /code-review run."
---

# Answering a code review

A finding is a claim, not an instruction. The goal is a better codebase, never a
cleared comment list.

## Verify before fixing

Reproduce the defect against the current code first, and fix only what survives
that. Record which of these each finding was:

- Real → fix the cause. When the reviewer's patch is wrong, fix it anyway; the
  value was in the claim, not the suggestion.
- Grounded in a project rule the code breaks → fix it, and cite the rule.
- Not real, or already recorded as a known gap → skip, and say why.

A severity label and an aggregate "merge risk" verdict track neither the truth
nor what you have already answered. Neither is evidence of anything.

## Fix the class, not the finding

A finding points at one site. Before fixing it, raise the altitude: what class of
defect is this, and where else does the class hold? Fix what admits the class —
the missing invariant, the type that allows the state, the call site nobody has
to remember. A patch at the site the reviewer named leaves the rest of the class
in the tree.

## Tests are held to a higher bar

The question is what the test catches that it did not before. If the case the
reviewer names cannot be constructed where they point, say so and skip it —
after checking whether another test already covers it. An assertion that holds
by construction, or a branch the fixture never takes, passes CI and guards
nothing. It is worse than leaving the test alone: it reads as coverage.

## Design decisions go to the user

A finding that changes a public API, a language rule, or a phase's contract is a
proposal. Put it to the user with a recommendation and wait. Adopting it because
a reviewer asked is how a design drifts without anyone deciding.

## Then distill

Run `/distill` as its own step once the fixes land. A fix written to satisfy a
reviewer arrives in the reviewer's framing — their wording in its comments, an
explanation of the bug beside the code, a helper the codebase already had. Scope
is the whole branch, as always, not the fixes alone.

## Report

One comment on the pull request: what was fixed, and what was skipped with its
reason. The skips are half the answer, not an omission from it.
