---
name: cr
description: Review the branch with /code-review at high, then answer the findings with /code-review-response.
---

# Code Review and Response

1. Invoke the `code-review` skill with `high`, over `origin/main...HEAD`: once
   the branch is pushed its upstream diff is empty, and a stale local `main`
   pulls in merged work.
2. Answer its findings with the `code-review-response` skill. With no findings,
   invoke `/distill` alone: the response would have ended with it.
