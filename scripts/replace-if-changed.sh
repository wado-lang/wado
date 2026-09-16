#!/usr/bin/env bash
# Move a freshly generated file over the committed one, and say which happened.
# Leaving an unchanged file alone keeps its mtime, so a rebuild that produced
# the same bytes does not make everything downstream of it look stale.
set -e -o pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: replace-if-changed.sh <new-file> <committed-file> <label>" >&2
  exit 2
fi
new=$1
committed=$2
label=$3

if cmp -s "$new" "$committed"; then
  rm "$new"
  echo "  $label unchanged, skipping update"
else
  mv "$new" "$committed"
  echo "  $label updated"
fi
