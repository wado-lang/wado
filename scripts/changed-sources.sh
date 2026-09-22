#!/usr/bin/env bash
# The files a branch changed, minus the ones `.gitattributes` marks
# `linguist-generated` or `linguist-vendored`. One repository-relative path per
# line, for a review that reads sources: the rest is regenerated or fetched.
# Run the `git diff` from the root.
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
} | git check-attr -z --stdin linguist-generated linguist-vendored |
    # One NUL-separated `path attr value` triple per file per attribute, in the
    # order asked. `unspecified` is the value for a path no rule names; any
    # other means a rule marked it, and either mark drops the path.
    tr '\0' '\n' | paste - - - - - - |
    grep -e $'\tunspecified\t.*\tunspecified$' | cut -f1
