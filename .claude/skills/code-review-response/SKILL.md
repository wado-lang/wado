---
name: code-review-response
description: "How to answer code review feedback — a human reviewer, CodeRabbit, or any review bot. Verify the findings, report the classes they are instances of to the user before fixing, then fix the class rather than the sites named. Put design decisions to the user, then run /distill. Invoke when responding to review comments on a pull request, or to the findings of a /code-review run."
---

# Answering a Code Review

A finding is a claim, not an instruction. The goal is a better codebase, not a
cleared list.

1. Verify each finding against a primary source: the code, a script, a tool's
   actual output, a spec. Reasoning and memory are not verification, and a
   reviewer's claim about an external tool is checked like any other. A real
   one is fixed even when the suggested patch is wrong; one grounded in a
   project rule is fixed citing the rule; one not real, or already a known gap,
   is skipped with the reason. One you could not check is reported as
   unverified, never as fact. Whose defect it was does not matter, and a
   severity label is not evidence.
2. A finding that changes a public API, a language rule, or a phase's contract
   is a proposal: put it to the user and wait. Only these block.
3. Group the other survivors by the class each is an instance of, and report the
   classes to the user before fixing. It is a report, not a request: keep going.
   For each class, say what the findings are, what admits the class, and the
   fix.
4. Fix the class at the altitude that admits it, not the sites named. A fix is
   an unreviewed change: before committing, check what it breaks, such as a
   moved path colliding with a concurrent session or another procedure.
5. Commit, then invoke `/distill`. Always: a fix written for a reviewer arrives
   in the reviewer's framing.
6. Report once more, after `/distill`: what was fixed, and what was skipped and
   why. On a pull request it is one comment.

A test the review asks for has to catch something new. One that holds by
construction, or takes a branch the fixture never reaches, reads as coverage
and guards nothing: skip it and say so.

## When Reviews Do Not Converge

A class that an earlier review on this branch already raised means the analysis
or the fix was wrong or incomplete. Fixing the new sites repeats that. Analyze
the class again, find what admits it, and close it so no review can find
another instance.

A review samples; it does not tell how many instances remain. While new
findings keep coming, stop waiting for the next review and take stock of the
whole change that admitted them, mechanically: every line the diff removed, say,
checked one by one. Review again once that stock is empty.

## Reporting

The user does not see the review's numbering. Name a finding by its content
(file and defect), never by its number.
