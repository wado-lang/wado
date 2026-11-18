# Eddy

Eddy is a code review bot written in Wado. It runs on the Anthropic API for
now; later, models from several providers will review a change together,
exchanging opinions.

What runs today is one reviewer over one branch, from the command line; the
rest of the pipeline below is not implemented yet.

## Reviewing a Branch

From the repository root, with `CLAUDE_API_KEY` set and the branch committed:

```sh
mise run eddy-review             # the reviewer reads, writes tests and runs them
mise run eddy-review --no-run    # it reads the diff and the tree, and runs nothing
```

The findings go to stdout as JSON. A finding the reviewer reproduced carries
its `repro`:

- `patch` is the test it wrote, as a diff against the reviewed commit.
- `run` is the command that runs it: `cargo`, `mise` for a task of the
  project, or a tool `mise` installed for it such as `node`, and its
  arguments.
- `expect` is a text the failing run prints. It tells an assertion that failed
  from a test that did not compile.
- `verified` says whether the run failed with that text when Eddy tried the
  patch alone in a clean checkout, after the review. The reviewer's word is not
  taken for it.
- `output` is what that run reported.

The test asserts the right behaviour, so `git apply` and that command give
the failing test to fix against.

Each finding is also written under `scratchpad/eddy/<head>/`, numbered from 1:

- `<n>.patch` is that patch, for `git apply`.
- `<n>.md` is the body of a pull request comment for the finding. It says
  whether the finding was reproduced, and holds one shell block that checks
  out the reviewed commit, applies the patch and runs the test, so a reader
  with the repository alone reproduces it.
- `review.md` is one comment for the whole review: a table of the findings and
  what the review cost, then each finding's comment folded. It is written when
  the review fails too, with the error.

A finding's `line` is a line the diff shows, which is where a pull request
takes a comment on a line, or 0.

The reviewer is asked for none of this. Its instructions hold the review
alone: what to look for, and to reproduce a finding with a test that asserts
the right behaviour. A second, smaller model (`--reporter`, Claude Haiku 5.5)
then reads the findings, the files the reviewer wrote and the runs it made,
and names each finding's line, its test and the run that failed. Anything
about how a finding is reported goes into that second prompt, so it cannot
cost the review its attention. `--report-again` reports the last recorded
review of a commit once more, which tries another reporter on the same
findings.

Each run is also recorded as a file of its own under `reviews/<head>/` in the
repository's wiki, which holds every review of that commit: outcomes are what Eddy learns from, and
an outcome needs the finding it answers. `mise run eddy-review` clones the
wiki into `scratchpad/wiki` first and writes the record there; it pushes
nothing, and the `eddy` skill's steps push the record.

A run ends by printing what it cost to stderr, the total and each model's
share, in US dollars:

```text
cost: $0.6020 (claude-opus-5-5 $0.6000, claude-haiku-5-5 $0.0020)
```

The record holds the tokens each model took, not the dollars: `src/cost.wado`
prices them at Anthropic's list prices when the line is printed, so a price
that changes is corrected in one table. A model the table does not list is
named as unpriced, and the total is then a lower bound.

In a Claude Code session the `eddy` skill runs
the review and hands the findings to `code-review-response`.

## Reviewing a Pull Request

A repository admin comments `@wado-bot review` on a pull request, or
dispatches the `Eddy` workflow with its number. The workflow reviews the pull
request's head and posts `review.md` as a comment. Eddy itself, `mise.toml`
and every script come from the default branch, so the pull request's code runs
only inside Eddy's confinement. The workflow pushes the record to the wiki,
and the patches stay in the run's artifact for 90 days, as
`eddy-<pr>-<head>`.

The workflow holds no API key. With `ANTHROPIC_FEDERATION_RULE_ID` and the
rest of a Workload Identity Federation rule in its environment, Eddy trades
the job's GitHub OIDC token for a short-lived one, which the rule grants only
to `eddy.yml` on main, and mints the next before each one expires. Its
children, the reviewed code among them, get none of its environment, so they
cannot mint one.

`src/main.wado` states the options.

- [Design notes](./docs/design.md): the decisions taken, and the leanings not
  yet settled.
- [Research](./docs/research-code-review-bots.md): the evidence behind them,
  from work published since August 2026.
- [Tools](./docs/tools.md): what a reviewer agent reads and checks a change
  with, built and wanted.
- [TODO](./TODO.md): open work that is not a direction, such as performance.

## Directions

The research points Eddy this way. What has been decided is in the design
notes; the rest is direction, not decision.

### Pipeline

- Spend the model where judgment is needed: a fast dispatcher, bounded tools
  for review, a verifier with a narrower view than the reviewer. A System One
  model may beat rules at dispatch, but only after shadow evaluation against
  labelled history.
- Keep risk estimation and the accept/block policy in separate stages, with the
  policy in code.
- Review one PR at a time, and track open findings across rounds.

### Splitting the Change

- Cut the change along the language service's call and reference graph, not by
  file. The graph gives the first cut deterministically.
- Check the cut with the build: a unit is sound when it compiles and its tests
  collect on its own.
- Give a reviewer the callers of what changed more readily than the callees.
- Implicit dependencies (generators and what they generate, configuration,
  names matched as text) are project knowledge. They come from rules the
  repository states, and from learning: a defect that landed in another unit
  is a link the cut missed.

### Models

- Make disagreement explicit and grounded. Choose models for what they find
  that the others do not, measured as how rarely they miss the same defect,
  and aggregate by evidence rather than by vote count.
- Hide authorship from every judge.
- Put the strongest model where findings merge, not where work is split, and
  hand it structured, verified findings, so that its context holds evidence
  rather than transcripts.

### Findings

- Post less. Every comment should survive a refutation step against the
  repository and carry its evidence.
- Reproduce what is severe. A reproducer written apart from the finding, run
  where the code runs, turns a claim into evidence; the post says which.
- Give `AGENTS.md` only to the reproduction agent, which builds and runs the
  project as it says; a reviewer would follow the rest of it literally. Read it
  from the base revision, since a pull request can rewrite it.
- Estimate severity before reproduction, to choose what to reproduce, and
  decide it after, from what was demonstrated and what it reaches, read from
  structured fields. A calibrated classifier such as Jev fits the decision,
  with thresholds fitted on the project's own findings; it may lower a
  severity, never revive a finding a check rejected.
- Read what a reviewer covered from its transcript, not from its summary.

### Security

- The agent that reads untrusted text (titles, bodies, comments, code) holds
  no secret and can neither post nor act. It returns structured data, and code
  validates and posts it.
- The reproduction runs with no token, and with no network beyond what it
  needs.
- Screen untrusted text and tool output with a classifier such as Jev, as one
  layer that fails closed. It catches with high precision and misses much at
  its default threshold, so it narrows the attack rather than stopping it.

### Learning and Measurement

- Learn into what can be reviewed: an evaluation set grown from missed
  defects, and rules a person can read and delete.
- Measure once with a grounded benchmark built from the project's own reverts
  and hotfixes, given the pre-fix diff. ReviewBench's split of grounded and
  augmented scores is the model to follow, with the judge validated.
- Measure continuously: the addressed rate weighted by severity, the defects
  that escaped review, and the comments and cost per review. The offline and
  online numbers should move in the same direction.
- Tie an outcome to a finding by the symbol the language service resolves, with
  line overlap as supporting evidence. A fix in a caller, or a moved function,
  touches none of the lines a finding named.
- Keep part of the offline set out of learning's reach. The outcomes learning
  feeds on are also what the metrics count, so a set the learning never sees
  is what tells improvement from fitting.
