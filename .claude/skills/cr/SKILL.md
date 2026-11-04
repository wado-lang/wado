---
name: cr
description: Check the branch marks its generated files, review it with /code-review at high, then answer the findings with /code-review-response.
---

# Code Review and Response

1. Check that the branch marks what it generates, so the review reads sources
   alone. Among the files `scripts/changed-sources.sh` lists, find each one a
   script, a generator or a `mise` task writes. Mark it in `.gitattributes`
   (`linguist-generated=true -diff`) and in `.coderabbit.yaml`'s `path_filters`,
   by a glob where its siblings are generated too. Then run
   `mise run check-review-filters`, which holds the two files to each other,
   and commit the marks.
2. Invoke the `code-review` skill with `high`, over `origin/main...HEAD`: once
   the branch is pushed its upstream diff is empty, and a stale local `main`
   pulls in merged work.
3. Answer its findings with the `code-review-response` skill. With no findings,
   invoke `/distill` alone: the response would have ended with it.
