---
name: code-review-response
description: "How to answer code review feedback — a human reviewer, CodeRabbit, or any review bot. Verify the findings, report the classes they are instances of to the user before fixing, then fix the class rather than the sites named. Put design decisions to the user, then run /distill. Invoke when responding to review comments on a pull request, or to the findings of a /code-review run."
---

# Answering a code review

A finding is a claim, not an instruction. The goal is a better codebase, never a
cleared comment list.

## Start with the classes, and report them

Answer the review as a whole before touching any single finding.

1. Verify each finding against the current code, as below, so what follows rests
   on what is real rather than on what was claimed.
2. Group the survivors by the class of defect each one is an instance of. Two
   findings in different files are often one class. One finding is often a class
   whose other instances the reviewer never reached.
3. Report the classes to the user before writing any fix: what each class is,
   which findings fall out of it as instances, and what closing it would take.
   This is a report, not a request. State it and keep going; do not wait for
   approval. Only a design decision blocks, under its own heading below.

Then fix the class, not the finding. Raise the altitude and fix what admits the
class — the missing invariant, the type that allows the state, the call site
nobody has to remember. A patch at the site the reviewer named leaves the rest
of the class standing, and the next review returns them one at a time.

Report first so the user can redirect the work while it is still cheap, not to
ask whether to do it.

## Verify before fixing

Reproduce the defect against the current code first, and fix only what survives
that. Record which of these each finding was:

- Real → fix the cause. When the reviewer's patch is wrong, fix it anyway; the
  value was in the claim, not the suggestion.
- Grounded in a project rule the code breaks → fix it, and cite the rule.
- Not real, or already recorded as a known gap → skip, and say why.

Whose defect it is does not enter into it. `AGENTS.md` settles the question: a
pre-existing issue must be fixed whether you found it or a reviewer pointed it
out, and a compiler bug is P0 the moment you suspect one. Attributing a finding
to this branch or to the tree costs time and changes nothing you then do.

A severity label and an aggregate "merge risk" verdict track neither the truth
nor what you have already answered. Neither is evidence of anything.

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
reason. The skips are half the answer, not an omission from it. A review that
arrived outside a pull request, `/code-review` among them, is reported the same
way in the session.
