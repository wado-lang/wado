# Eddy

Eddy is a code review bot written in Wado. It runs on the Anthropic API for
now; later, models from several providers will review a change together,
exchanging opinions. It is not implemented yet.

- [Design notes](./docs/design.md): the decisions taken, and the leanings not
  yet settled.
- [Research](./docs/research-code-review-bots.md): the evidence behind them,
  from work published since August 2026.

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
