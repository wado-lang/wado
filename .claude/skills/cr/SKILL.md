---
name: cr
description: Review the branch with /code-review at xhigh, then answer the findings with /code-review-response.
---

# Code Review and Response

1. Invoke the `code-review` skill with `xhigh`, over `origin/main...HEAD`: once
   the branch is pushed its upstream diff is empty, and a stale local `main`
   pulls in merged work.
2. Answer its findings with the `code-review-response` skill. No findings ends
   the run.
