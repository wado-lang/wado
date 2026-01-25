# Instructions to Review for Refactoring

Review the changes in the branch `git diff $(git symbolic-ref refs/remotes/origin/HEAD | sed 's@^refs/remotes/@@')...HEAD` carefully, which is intended to refactor the code without changing its external behavior.:

- Find logic flaws
- Find low-quality code: useless branches, defensive programming, duplicated code
- Find comments that are not helpful: trivial, obvious, or stale
