# Eddy Design Notes

Decisions taken so far, the leanings not yet settled, and the directions the
research points. The evidence behind them is in
[the research](./research-code-review-bots.md).

## Decisions

- [x] A reproduction agent is part of the pipeline. A severe finding is tried
  in a local environment before it is posted, and the post says whether it
  reproduced.
- [x] No agent calls `core:process`. Every capability an agent has reaches it as
  a tool or an MCP server, because current agentic models are trained to
  act through tool calls.
- [x] Learning ships in the first version.
- [x] Eddy learns from outcomes, not from what people say about its comments.
  It watches whether a finding was addressed by a later commit in the PR, and
  whether the merged change was reverted or needed a follow-up fix within a
  threshold time.
- [x] A learning found harmful is deleted without a person deciding it. A
  learning is data with provenance (the outcome that produced it, the model
  that wrote it) and a scope (the paths it applies to). Three mechanisms act
  on it:
  - Ablation is the gate. A new learning takes effect only once the
    evaluation set scores higher with it than without it. Every time the set
    grows, it is rerun with each learning removed, and a learning whose
    removal does not lower the score is deleted.
  - Expiry is the backstop. A learning lapses after a fixed time unless
    outcomes earned it again.
  - Quarantine by model. A learning written from one model's run reaches the
    other models only after it passed the gate, since reward hacking spreads
    through shared memory.
- [x] Code search is an MCP tool shaped for the agent, not the LSP protocol
  passed through. Its answers are built to raise the agent's precision and
  recall: exact references from the language service, and every textual
  occurrence too, each labelled as a definition, a reference, a comment, a
  string, or generated code. A location-only answer misses the comments and
  strings a rename must touch; a plain text search mixes them with unrelated
  symbols of the same name. Each language's tool is built on that language's
  own service: `wado query` for Wado, rust-analyzer for Rust, which much of
  the repository is written in. The method carries to any language with a
  language server.
- [x] The strongest model sits where findings merge, not where work is split.
  It receives structured, verified findings, so that its context holds
  evidence rather than transcripts.
- [x] Searching and reproducing are separate steps. Reviewers each search a
  batch of the change's units, reading only, one call merges their candidates
  and keeps the twenty most severe, and a reproducer per candidate tries it
  with three runs. A reviewer that reproduced as it searched spent its turns on its first
  suspicion and reported about one finding, and one reviewer confirming every
  candidate dropped most by reading. The searches overlap in what they read,
  but not in what they run, which costs the most.
- [x] A reviewer reads the definitions a change touched, whole, not the diff.
  Eddy picks them from the diff, so the reviewer's tokens go to logic rather
  than to a diff of hundreds of thousands of tokens, which cost each search
  most of its price. A defect the change left in place is a finding too, and
  is not told apart: whether to fix it with the change is each project's
  call. On #1697 the culprit's defect lay in a whole function's logic that no
  diff-reading search named.
- [x] Only a finding a test reproduced is reported. One nobody reproduced
  leaves the reproduction to whoever reads it, and when that reader fails to
  reproduce it the review cost them twice; the reviewer and the programmer are
  both models now, so the reviewer pays. The record keeps the rest.
- [x] Part of the offline evaluation set stays out of learning's reach. The
  outcomes learning feeds on are what the metrics count, so only a set the
  learning never saw tells improvement from fitting.
- [x] Eddy points at problems and proposes no fixes.
- [x] Every model call uses prompt caching.
- [x] The API holds every reply to the JSON Schema of the type it is read into
  (`output_config.format`), which Jade derives from the type. A reply is never
  searched for JSON: a model asked in the prompt alone writes prose around its
  answer, and that prose can hold braces of its own. Three things measured on
  the API shape how it is used:
  - A reviewer's conversation carries the format on every turn. The format is
    part of what the cache keys a conversation by, so adding it on the last
    turn alone bills the whole conversation again at the cache-write price.
  - The format does not slow generation, on the reviewer's model or the
    reporter's.
  - The reporter's model thinks more often under a format, and a reply takes
    longer by what it thought.
- [x] The thresholds (the outcome window, the expiry, the weights) start as
  guesses and are tuned in operation, on outcomes outside the held-out set.
- [x] The reproduction agent reads `AGENTS.md` from the base revision, inside
  its sandbox. A pull request can rewrite the file, and the agent runs what it
  says.
- [x] An outcome is tied to a finding by the symbol the language service
  resolves, with line overlap as supporting evidence.

## Leanings

- [ ] Show `AGENTS.md` to the reproduction agent only. It holds how to build,
  test and run the project, which reproduction needs; to a reviewer most of
  it is noise that it would follow literally.
- [ ] Choose the verifier by measurement, not by vendor. Across vendors errors
  still correlate; what pays is the pair that rarely misses the same defect on
  Eddy's evaluation set, a verifier no weaker than the code's author, and
  judges that cannot see who wrote the code or the finding.
- [ ] Run on a GitHub Actions runner, Google Cloud Run, or a Cloudflare
  Worker.

## Deferred

- How models of different providers exchange findings is worked out by trial
  during implementation.
- How learnings are stored is decided later.
- Which queries the code search tool takes is decided later.

## Outcome Signals

Each signal has a failure mode, and the counting rule guards against it.

- "Addressed" can be gamed: a reviewer learns to post findings that are cheap
  to address. Weight it by severity, and count a finding addressed only when
  the follow-up commit changes the symbol it named. Matching lines instead
  misses a fix in a caller or a moved function, and credits an unrelated edit
  that happens to overlap.
- A revert has many causes. Count one against a review only when the reverted
  or fixed symbols are among those the review saw, and as a miss when no
  finding named them.
- The threshold time trades signal delay against attribution: a short one
  misses late regressions, a long one blames the review for later changes.

## Set Aside

- Held-out exposure, applying each learning to a random half of the reviews in
  its scope and comparing outcomes. It measures the real effect, but withholds
  a good learning from half the reviews and needs volume to decide.

## Directions

The research points Eddy this way. What has been decided is under Decisions;
the rest is direction, not decision.

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

### Findings

- Post less. Every comment should survive a refutation step against the
  repository and carry its evidence.
- Reproduce what is severe. A reproducer written apart from the finding, run
  where the code runs, turns a claim into evidence; the post says which.
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
