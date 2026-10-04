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
   which findings fall out of it as instances, what admits it, and what closing
   it would take. The user does not see the review's numbering, so name each
   finding by its content (the file and the defect), never by its number.
   This is a report, not a request. State it and keep going; do not wait for
   approval. Only a design decision blocks, under its own heading below.

Then fix the class, not the finding. Raise the altitude and fix what admits the
class — the missing invariant, the type that allows the state, the call site
nobody has to remember. A patch at the site the reviewer named leaves the rest
of the class standing, and the next review returns them one at a time.

Report first so the user can redirect the work while it is still cheap, not to
ask whether to do it.

A fix is itself an unreviewed change. Before committing it, check what it
breaks: a path moved to a shared location collides with a concurrent session,
and a procedure rewritten from memory drops a step.

## Verify before fixing

Reproduce the defect against the current code first, and fix only what survives
that. Verify against a primary source: the code, a script, a tool's actual
output, a spec. Reasoning and memory are not verification, and a reviewer's
claim about an external tool is checked like any other. A finding you could not
check is reported as unverified, never as fact. Record which of these each
finding was:

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

## When reviews do not converge

A class that an earlier review on this branch already raised means the analysis
or the fix was wrong or incomplete. Fixing the new sites repeats that. Analyze
the class again, find what admits it, and close it so no review can find
another instance.

A review samples; it does not say how many instances remain. While new findings
keep coming, stop waiting for the next review and take stock of the whole
change that admitted them, mechanically: every line the diff removed, say,
checked one by one. Review again once that stock is empty.

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

## Then invoke `/distill`

Commit the fixes, then invoke the `/distill` skill. Always start the skill
itself: a pass from memory of its rules is not one. This is not a decision. Do
not ask whether to run it, do not offer it as a next step, and do not stop
before it: there is no case where the answer is no, and the moment you wonder is
the moment to run it. A response that ends without it is unfinished.

Where it sits is §"The Cycle" in `AGENTS.md`: commit, `/distill`, test.

Run it even when the fixes were small and even when you are confident there is
nothing to cut. Finding nothing is the outcome that ends the cycle, and you only
know it by running.

A fix written to satisfy a reviewer arrives in the reviewer's framing: their
wording in its comments, an explanation of the bug beside the code, a helper the
codebase already had. That is what the pass is for. Scope is the whole branch,
as always, not the fixes alone.

## Close with a report

This is the second of the two reports, not the first. The class report goes out
before any fix is written, under §"Start with the classes". This one is written
after `/distill`, so what it describes is the code as it stands.

One comment on the pull request: what was fixed, and what was skipped with its
reason. The skips are half the answer, not an omission from it. A review that
arrived outside a pull request, `/code-review` among them, is reported the same
way in the session.
