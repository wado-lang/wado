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
- `judged.json` (Claude Opus 5.5) and `judged-sonnet.json` (Claude Sonnet 5.5)
  hold a model's verdict on each pair whose top pull request holds at least
  half of the blamed lines. Each verdict says whether the fix corrects a
  defect, whether the blamed pull request introduced it, and whether a reviewer
  reading only that pull request's diff could have pointed at it.

The model reads both diffs whole, generated, vendored and Markdown files left
out. A pair with a diff past `diff_limit` (2,000,000 bytes) gets an error
instead of a verdict, since clipping it may cut the part that shows the defect.
Every pair fits under that limit. A rerun judges only the pairs left unsettled.

An entry is usable when all three verdicts hold. Its review input is the
culprit's diff, and its expected finding is the defect the fix names.

## Reproducing

From the repository root:

```sh
git fetch --unshallow origin main   # in a shallow clone
wado run package-eddy/tools/szz.wado -- package-eddy/eval/szz.json
CLAUDE_API_KEY=... wado run package-eddy/tools/judge.wado -- \
    package-eddy/eval/szz.json 0.5 package-eddy/eval/judged.json
CLAUDE_API_KEY=... CLAUDE_MODEL=claude-sonnet-5-5 wado run package-eddy/tools/judge.wado -- \
    package-eddy/eval/szz.json 0.5 package-eddy/eval/judged-sonnet.json
```

Both run `git` through `core:process`, which needs no flag: `git` is under
`/usr`, and the repository is the directory `wado run` grants.

## Findings So Far

- Blame alone is wrong more often than right. Of the pairs above the 50% share,
  most "fix" pull requests do correct a defect, but about half blame a pull
  request that only moved or reformatted the lines, such as a refactor that
  relocated a function.
- `git blame -M -C`, which follows moved and copied lines, does not take the
  movers out. It changes the top culprit of 26 fix pull requests but only 3 of
  the 54 pairs a run over all 97, long diffs clipped, found moved rather than
  introduced, and it shrinks the
  pairs above a 50% share from 97 to 84. The census blames with `-w` alone.
- The usable yield is about the same at a 50% share as at 80%, so the share
  threshold does little; the model check is what filters.
- The judge is not deterministic. Two runs over the 43 pairs above an 80% share
  found 16 and 17 usable entries, and three entries changed sides. Its verdicts
  need a human sample before they count.
- The two models agree on the yield but not on the entries. The table counts
  the 96 pairs both models judged. They disagree mostly on whether the culprit
  introduced the defect and whether its diff shows it, rarely on whether the
  fix fixes a defect (7 pairs).

  | Pairs                       | Opus 5.5 | Sonnet 5.5 | Both |
  | --------------------------- | -------: | ---------: | ---: |
  | Defect fix                  |       83 |         90 |      |
  | Introduced by the culprit   |       46 |         61 |      |
  | Visible in the culprit diff |       47 |         44 |      |
  | Usable                      |       38 |         43 |   26 |
  | Usable, fix adds a test     |       30 |         29 |   18 |

  The models give the same usable-or-not answer on 67 of the 96, while each
  model alone keeps its answer on 86 (Opus) and 84 (Sonnet) of the 96 across
  two runs, one of them on diffs that ignored whitespace. The 26 both call
  usable are the strongest candidates for the human sample.
- Sonnet 5.5 sometimes answers with malformed JSON, such as a field given
  twice. Such a pair gets an error, and a rerun judges it again.

## To Do

- [ ] Mine `fix:` commits inside other pull requests, not only pull requests
  titled `fix`.
- [ ] Have a person check a sample of the judge's verdicts, and fix the share
  of entries that gets checked.
- [ ] Split off the held-out part that learning never sees.
