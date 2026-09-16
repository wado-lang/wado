#!/usr/bin/env bash
# The files a branch changed, minus the ones `.gitattributes` marks
# `linguist-generated`. One repository-relative path per line, for a review that
# reads sources and regenerates the rest. Run the `git diff` from the root.
#
#   scripts/changed-sources.sh                       # list them
#   scripts/changed-sources.sh <base>                # against another base
#   git diff $(git merge-base origin/main HEAD) -- $(scripts/changed-sources.sh)
set -e -o pipefail

cd "$(dirname "$0")/.."

base="${1:-$(git merge-base origin/main HEAD)}"

# `git diff <commit>` reads the working tree, so uncommitted edits are in;
# untracked files are not, and are added here.
{
    git diff "${base}" --name-only -z
    git ls-files --others --exclude-standard -z
} | git check-attr -z --stdin linguist-generated |
    # One NUL-separated `path attr value` triple per file. `unspecified` is the
    # value for a path no rule names; any other means a rule marked it.
    tr '\0' '\n' | paste - - - | grep -P '\tunspecified$' | cut -f1
