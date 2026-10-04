---
name: cr
description: Review the branch with /code-review at xhigh, then answer the findings with /code-review-response.
---

# Code Review and Response

1. Invoke the `code-review` skill with `xhigh`.
2. Answer its findings with the `code-review-response` skill. No findings ends
   the run.

A class that an earlier review on this branch already raised means the analysis
or the fix was wrong or incomplete. Fixing the new sites repeats that. Analyze
the class again, find what admits it, and close it so no review can find
another instance.
