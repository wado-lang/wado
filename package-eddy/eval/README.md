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
- `checked.json` holds a second check of the 26 pairs both judges call usable,
  by Claude reading the diffs and the repository history. Each answer is
  `yes`, `no`, `unsure` (left for a person), or `na`. An `unsure` pair carries
  the `ask` class that kept it open.
- `baseline.json` holds a one-pass review of each of the 15 usable entries,
  three runs each: the reviewer reads the culprit pull request's whole diff,
  and a grader says whether a finding names the defect the fix corrected.
- `agentic.json` holds the same review by an agent that also reads the
  repository as it stood once the culprit merged, through Eddy's Read, Grep and
  Glob tools. Each run records the tool calls and the token usage.

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
CLAUDE_API_KEY=... wado run package-eddy/tools/baseline.wado -- \
    package-eddy/eval/szz.json package-eddy/eval/checked.json \
    package-eddy/eval/judged.json package-eddy/eval/baseline.json
CLAUDE_API_KEY=... wado run -D tools=true package-eddy/tools/baseline.wado -- \
    package-eddy/eval/szz.json package-eddy/eval/checked.json \
    package-eddy/eval/judged.json package-eddy/eval/agentic.json
```

They run `git` and `rg` through `core:process`, which needs no flag: both are
under `/usr`, and the repository is the directory `wado run` grants. The
agent's tree is a `git worktree` under `scratchpad/`, removed once its entry
is done.

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
- The history check finds what the judges cannot. They read two diffs, so
  they take a culprit's lines as its own work. Of the 26, the check keeps 15,
  rejects 4 and leaves 7 open:

  | Pairs                                                                                         | Count |
  | --------------------------------------------------------------------------------------------- | ----: |
  | Usable                                                                                        |    15 |
  | Not a defect fix (a feature)                                                                  |     1 |
  | Defect from another pull request (a later redesign, refactor, or code the culprit only moved) |     3 |
  | Open: the culprit's new check left a gap that predates it                                     |     3 |
  | Open: a dev-environment cache miss, not wrong behaviour                                       |     2 |
  | Open: a conformance sweep fixing several defects at once                                      |     2 |

  The open pairs are not noise. They mark the cases a single-diff verdict cannot
  settle, so they stay as data whichever way a person decides them.
- Sonnet 5.5 sometimes answers with malformed JSON, such as a field given
  twice. Such a pair gets an error, and a rerun judges it again.
- A one-pass review by Opus 5.5 finds the defect in 23 of 45 runs, and in 10
  of the 15 entries at least once. It reported 200 findings in all, about 4 a
  run. The table counts runs that hit, out of 3:

  | Hits | Entries                                 |
  | ---: | --------------------------------------- |
  |    3 | #686, #1142, #1190, #1204, #1529, #1915 |
  |    2 | #1672                                   |
  |    1 | #1223, #1585, #2310                     |
  |    0 | #1088, #1112, #1313, #1680, #1697       |

  A miss is not one kind. On #1112 and #1313 the defect needs knowledge outside
  the diff: tagpr's config format, and the names the wasm-size runner writes.
  On #2310 the reviewer fills its 10 slots with other formatter bugs. Only one
  run in three hits for a third of the entries, so a single run says little
  about a change to the reviewer. Whether the other findings are real is not
  measured yet.
- An agent that reads the tree with tools finds the defect in 29 of 45 runs,
  and in 12 of the 15 entries at least once, against 23 and 10 for one pass.
  Both reviews ran Opus 5.5 at its default effort, once each over the set. The
  table counts runs that hit, out of 3:

  | Entry | One pass | Agent |
  | ----- | -------: | ----: |
  | #686  |        3 |     3 |
  | #1088 |        0 |     1 |
  | #1112 |        0 |     1 |
  | #1142 |        3 |     3 |
  | #1190 |        3 |     3 |
  | #1204 |        3 |     3 |
  | #1223 |        1 |     3 |
  | #1313 |        0 |     3 |
  | #1529 |        3 |     3 |
  | #1585 |        1 |     2 |
  | #1672 |        2 |     2 |
  | #1680 |        0 |     0 |
  | #1697 |        0 |     0 |
  | #1915 |        3 |     0 |
  | #2310 |        1 |     2 |

  The agent made 384 tool calls over 373 turns, most of them Grep, and the
  45 runs read 37 million cached input tokens. #1313 turns from never to always:
  the names the wasm-size runner writes are in the tree, not the diff. #1915
  turns the other way. Its runs spend over 20 calls each tracing how a call
  passes its receiver, and then report only one or two findings, none of them
  the shifted labels. Reading can talk the reviewer out of a finding that was
  right.

## To Do

- [ ] Mine `fix:` commits inside other pull requests, not only pull requests
  titled `fix`.
- [ ] Have a person settle the open pairs in `checked.json`, then sample the
  ones Claude settled.
- [ ] Split off the held-out part that learning never sees.
- [ ] Measure the precision of the baseline's other findings, not only whether
  it hits the known defect.
- [ ] Give the agent a tool over the language service (`wado query`) built at
  the culprit's revision, and measure it against `agentic.json`.
