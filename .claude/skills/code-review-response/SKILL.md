---
name: code-review-response
description: "How to answer code review feedback — a human reviewer, CodeRabbit, or any review bot. Verify the findings, report the classes they are instances of to the user before fixing, then fix the class rather than the sites named. Put design decisions to the user, then run /distill. Invoke when responding to review comments on a pull request, or to the findings of a /code-review run."
---

# Answering a Code Review

A finding is a claim, not an instruction. The goal is a better codebase, not a
cleared list.

1. Verify each finding against the current code. A real one is fixed even when
   the suggested patch is wrong; one grounded in a project rule is fixed citing
   the rule; one not real, or already a known gap, is skipped with the reason.
   Whose defect it was does not matter, and a severity label is not evidence.
2. A finding that changes a public API, a language rule, or a phase's contract
   is a proposal: put it to the user and wait. Only these block.
3. Group the other survivors by the class each is an instance of, and report the
   classes to the user before fixing. It is a report, not a request: keep going.
   For each class, say what the findings are, what admits the class, and the
   fix.
4. Fix the class at the altitude that admits it, not the sites named.
5. Commit, then invoke `/distill`. Always: a fix written for a reviewer arrives
   in the reviewer's framing.
6. Report once more, after `/distill`: what was fixed, and what was skipped and
   why. On a pull request it is one comment.

The user does not see the review's numbering. Name a finding by its content
(file and defect), never by its number.

A test the review asks for has to catch something new. One that holds by
construction, or takes a branch the fixture never reaches, reads as coverage
and guards nothing: skip it and say so.
