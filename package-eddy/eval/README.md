# Evaluation Set

Candidate entries for Eddy's offline evaluation set, mined from this
repository's own history. This repository fixes forward and almost never
reverts, so the defects come from fix pull requests rather than reverts.

## Files

- `szz.json` is the SZZ census. For every pull request titled `fix` that
  merged into `main`, it holds the pull requests that last wrote the lines the
  fix changed, by `git blame -w`, with line counts. `addsTest` says whether the
  fix touched a test. Generated and vendored files, tests and Markdown are left
  out of the blame.
- `judged.json` holds a model's verdict on each pair whose top pull request
  holds at least half of the blamed lines. Each verdict says whether the fix
  corrects a defect, whether the blamed pull request introduced it, and whether
  a reviewer reading only that pull request's diff could have pointed at it.

An entry is usable when all three verdicts hold. Its review input is the
culprit's diff, and its expected finding is the defect the fix names.

## Reproducing

```sh
git fetch --unshallow origin main   # in a shallow clone
node package-eddy/tools/szz.mjs . package-eddy/eval/szz.json
CLAUDE_API_KEY=... node package-eddy/tools/judge.mjs . package-eddy/eval/szz.json 0.5 package-eddy/eval/judged.json
```

## Findings So Far

- Blame alone is wrong more often than right. Of the pairs above the 50% share,
  most "fix" pull requests do correct a defect, but about half blame a pull
  request that only moved or reformatted the lines, such as a refactor that
  relocated a function.
- The usable yield is about the same at a 50% share as at 80%, so the share
  threshold does little; the model check is what filters.
- The judge is not deterministic: two runs over the same pairs differed by one
  usable entry. Its verdicts need a human sample before they count.

## To Do

- [ ] Mine `fix:` commits inside other pull requests, not only pull requests
  titled `fix`.
- [ ] Have a person check a sample of the judge's verdicts, and fix the share
  of entries that gets checked.
- [ ] Split off the held-out part that learning never sees.
- [ ] Port the census and the judge to Wado.
