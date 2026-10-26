# Research: Code Review Bots, August to October 2026

This survey covers how code review bots are built, what raises their quality,
how they are evaluated, how they trade cost against quality, and how they learn
over time. It reads only work published from
2026-08-01 to 2026-10-26: the models changed enough in August 2026 that older
results say little about current ones.

## Method

- arXiv: the API query `abs:"code review"` with `submittedDate` in the window,
  newest first. The papers below were read from their abstracts, not their full
  text, so a figure here is the abstract's.
- Industry: web searches for posts in the window from GitHub, Anthropic,
  CodeRabbit, OpenAI and TypeSafe, and for community projects built on
  TypeSafe's Jev. These are vendor or author reports, and their figures are
  their own.

## Architecture

Review is a pipeline of separate stages rather than one prompt. The stages that
recur:

1. Dispatch: decide which files to review and against which criteria.
   OpenCodeReview does this with fixed rules rather than an agent, because an
   agent's triage varies from run to run ([2608.09290]).
2. Grounded review: a tool-using loop per file, with a small, curated tool set
   rather than free exploration. Files are reviewed in parallel, and
   cross-file context is fetched on demand ([2608.09290]).
3. Candidate generation by several reviewers. Anthropic's Code Review runs
   several bug-hunting agents in parallel ([claude-code-review]). GitHub's
   lite tier uses an ensemble of models ([reviewbench]).
4. Verification: a separate stage that tries to refute each candidate before it
   is posted. OpenCodeReview's reflector sees only the diff, not the reviewer's
   exploration, and is falsification-first ([2608.09290]). Anthropic's agents
   vote on each finding, and style and cleanup findings are dropped
   ([claude-code-review]).
5. Decision: turn the findings into a verdict. Estimating the risk and choosing
   the action are better kept apart (see [Cost and Quality](#cost-and-quality)).

### Triage with a System One Model

A System One model answers typed questions about an input instead of writing
text. TypeSafe announced the first, Jev, on 2026-09-15 ([jev-innfactory]):

- It takes up to 64,000 tokens of text and answers every question in one
  parallel pass. A question is a choice among up to 255 options, a score on an
  ordered scale, or a yes/no probability.
- It is trained to give calibrated probabilities (TypeSafe calls the method
  RLCD), and it cannot answer outside the schema. It cannot explain itself, and
  it can still pick the wrong option.
- It costs $0.042 per million input tokens, output free, and answers in
  70–500 ms from the US West Coast. Access is by waitlist, through its own
  endpoint, not a chat API.

The cases that bear on review triage:

- review-router decides whether a commit gets a full AI review or a quick one
  from what the diff adds. A path rule that sent auth, CI and migration files
  to full review sent 13 CVE fixes to the quick path, since they sat in files
  like `validators.py`. review-router sent all 13 to full review and skipped
  full review on 42% of 561 commits from FastAPI, Express and Django
  ([jev-engineering]). The sample is small, and its README also says 8 of 8.
- jev-gate (a GitHub Action) asks the same questions of every diff: sensitive
  security area, deleted tests, secret material, breaking change, comment
  drift. A finding at or above a per-rule threshold requests review, and only
  a repository's explicit policy makes it block. Its author says no one has
  calibrated the scores on these rules yet ([jev-gate]).
- Jev scored 62.6% on "is this phishing?" asked once, against 81.3% for Claude
  Haiku 4.5. Split into five narrow questions and combined by a logistic
  regression fitted on 1,000 labelled emails, it reached 95.0%; the split did
  not help Haiku. Calibration varied by question type, and on a question whose
  answer was not in the text Jev was right 44.7% of the time at a stated 0.74
  ([jev-beri]).
- On TypeSafe's own four-workflow benchmark, Jev matched GPT-5.6 Terra's
  accuracy (67.8% vs 67.9%) at about 1/76 the cost per case, 5–6 points below
  GPT-5.6 Sol and Claude Opus 5 ([jev-datacamp]).

What carries over: a System One model replaces a path rule well when the
signal is in the diff's content, if the question is split into narrow ones, the
thresholds are fitted on labelled history, and the version is pinned so the
calibration does not silently change ([jev-beri]). Nobody has published a
labelled evaluation of it as a review dispatcher.

### Vendor Pipelines

- CodeRabbit gathers context from many sources and runs verification agents on
  each suggestion before posting it ([coderabbit-context]). In the window it
  added a Reproducer Guide per Deep Scan finding, which its Runtime Analysis
  (closed beta, 2026-10-23) runs in a coding environment, keeping the evidence.
  Deep Scan can now verify a finding against linked repositories such as a
  shared auth library (2026-10-02), and a security check with a severity
  threshold can be made a required merge check (2026-10-05)
  ([coderabbit-changelog]).
- OpenAI's Codex Security Cloud (research preview, 2026-09-29) writes a threat
  model of the repository first, then scans commits against it. It tries to
  reproduce each likely issue in a clean container, attaching the logs, and
  marks the ones that reproduce as validated. Automatic PR reviews post only
  High and Critical findings by default ([codex-security-cloud]).
- Anthropic's Code Review went generally available on 2026-10-15
  ([claude-code-review]). Parallel agents hunt for bugs, a separate group votes
  on each finding, and style and cleanup findings are dropped.
- GitHub's lite tier runs an ensemble of models, tuned offline on ReviewBench
  ([reviewbench]).

### Multi-Agent Interaction

- Adversarial Review gives three roles: a coder, a reviewer, and a critic who
  audits the review before the coder acts. Without an explicit instruction to
  disagree, the agents fell into false consensus, agreeing without evidence.
  Three agents beat a five-agent baseline ([2608.18167]).
- Debate often does not beat majority voting. It helps only when some agent
  proposes the right answer and the aggregation can pick it out when the
  majority misses it. Choosing agents for coverage, so that together they
  propose more correct answers, raised accuracy ([2610.04686]).
- Bots already review each other's pull requests: 248,641 AI-authored PRs
  received an AI review. A reviewer posted 58–65% more comments when reviewing
  output from its own vendor's agent, though the effect size was small
  ([2608.21311]).

### Model Diversity

Two questions: whether a second model helps at all, and whether one from
another vendor helps more than another model from the same vendor.

| Source       | Measure                                                 | Result            |
| ------------ | ------------------------------------------------------- | ----------------- |
| [2609.22512] | Effective independent judges among ten                  | about 3.5         |
| [2609.22512] | Comparisons that are significant only if errors ignored | up to 28%         |
| [2605.29800] | Error correlation φ, same family vs across families     | 0.437 vs 0.389    |
| [2609.17857] | A judge's advantage for its own family                  | 3.4 to 8.4 points |
| [2607.21656] | Pass rate, Codex alone vs reviewed by Claude            | 71.6% vs 89.7%    |
| [2607.21656] | Pass rate, Codex reviewing itself                       | 84.5%             |
| [2607.21656] | Pass rate, Claude alone vs reviewed by Codex            | 91.4% vs 82.8%    |

- Errors correlate across vendors too. The dependence is strongest among the
  most accurate judges, including pairs from different providers. Which
  aggregation works depends on how the errors are shared, so the authors pick
  it on a small trusted set first ([2609.22512]).
- A different family decorrelates only a little. Family alone does not restore
  independence ([2605.29800], before the window), and a vision-language study
  found 2 to 4 effective voters among 17 models of 8 families ([2603.17111],
  before the window). Diversity measures mostly track capability; once that is
  controlled, the rate at which two models fail together still predicts a
  smaller gain from voting ([2607.20768], before the window).
- A judge favors its own family, measured on older open-weight families
  ([2609.17857]). Self-preference disappears when the judge cannot see who
  wrote what; a label saying "yours" or "another model's" moves the score
  either way ([2608.18091], submitted before the window, revised in it).
- In code review the pairing is asymmetric. A reviewer that cannot run tests
  helped a weaker writer and hurt a stronger one, whichever vendor it came
  from ([2607.21656], just before the window).

What carries over: another vendor is a modest gain, not independence. What
pays is measured: the reviewer and verifier whose misses overlap least on the
project's own evaluation set, no weaker than the code they judge, and blind to
who wrote it.

### The Coordinator

A coordinator splits the work, hands it to subagents, and merges what comes
back. Its quality matters most at the merge.

| Source       | Measure                                                          | Result         |
| ------------ | ---------------------------------------------------------------- | -------------- |
| [2608.30661] | Average score, ten orchestrators over one subagent pool          | 25.15 to 50.49 |
| [2608.30661] | Haiku 4.5 alone vs orchestrating                                 | 23.76 vs 33.05 |
| [2608.30661] | Gain from replaying past orchestration experience, per task      | +0.25 to +6.46 |
| [2406.04692] | Regression weight on final quality, aggregator vs proposer       | 0.588 vs 0.281 |
| [2512.08296] | Weak orchestrator with strong subagents vs all strong, one setup | 0.42 vs 0.32   |

- With the subagents fixed, the choice of orchestrator alone halves or doubles
  the score. Merging the subagents' results is the most consistent
  bottleneck, and weaker orchestrators suffer most there. They also spend more
  rounds and repeat work without scoring better, so a cheap coordinator is not
  a cheap system ([2608.30661]).
- Debate beats voting only when the readout uses evidence to pick a correct
  proposal the majority missed. Supplying correct proposals and reading them
  out are separate conditions ([2610.04686]).
- Before the window: in Mixture-of-Agents the aggregator's quality weighed
  twice the proposers' ([2406.04692]). Teams often score below their strongest
  member, even when told who it is ([2602.01011]). A reasoning-heavy model can
  fail as an orchestrator because its context fills with subagent output
  ([2606.01351]). One setup did better with a weak orchestrator over strong
  subagents than with strong models everywhere, and the effect differed by
  model family ([2512.08296]).
- Anthropic's Code Review has no single chief: a separate group votes on each
  finding ([claude-code-review]).

What carries over: the coordinator's judgment is worth the most where it
merges, and it is spent best on evidence rather than on prose. Two things
shrink what it has to judge: dispatch done in code, and findings that arrive
already verified or reproduced.

### Splitting the Change

How a change is cut into units decides what each reviewer can see, and what
the coordinator must stitch back together.

| Source       | Measure                                                      | Result         |
| ------------ | ------------------------------------------------------------ | -------------- |
| [2609.32057] | PRs where over half the changed functions share a call graph | about 40%      |
| [2610.11593] | Histories left unrunnable, file and hunk splits              | 27.5%, 37.4%   |
| [2610.11593] | Histories left unrunnable, splits by dependency analysis     | 20.6%, 26.2%   |
| [2610.11593] | Histories left unrunnable, split checked by test collection  | 0%             |
| [2608.09290] | Tokens, one subagent per file vs Claude Code                 | 5 to 15× fewer |

- Splitting by file is the cheap default. OpenCodeReview runs one subagent per
  file, tells it to comment only on its own file, and lets it read the others
  through bounded tools. It has no step that merges findings across files, and
  no ablation isolates the split from its other modules ([2608.09290]).
- Splitting by file cuts what belongs together. Changed functions are often
  joined by calls, and the callee often sits in another file. Grouping by call
  graph helped people orient in a PR, but functions repeated across groups
  confused them, and changes outside functions (globals, imports) got lost
  ([2609.32057]).
- A split proposed by a model and checked by the build holds. RucTangle groups
  changes by purpose, then applies the groups in order and checks that the
  tests still collect after each; a failing group is reordered, revised, or
  merged into the next. Every split by file, by hunk, or by dependency
  analysis left some histories unrunnable. More pieces is not better: hunk
  splitting cut three times as many and scored worse ([2610.11593]).
- Dependencies a call graph does not show need knowledge of the project:
  generated files and their generators, configuration, schemas, text matched
  by name. Before the window, an LLM judging such links leaned on words that
  co-occur rather than on real dependencies ([2511.00262]), and dense
  annotation paid only where dependencies were implicit ([2606.26979]).

### Language Services

No study in the window measures a language server inside a review bot. Two
just before it measure one inside coding agents.

- A language server buys precision, not recall or tokens. Finding references
  through one was exact where grep returned comments, strings and other
  symbols of the same name, but recall stayed where the agent's thoroughness
  left it. It saved tokens only for the weakest model; for the others it cost
  6% to 118% more. Left free to choose, models used grep for localization
  almost always. A rename solved by grep failed three times in four through a
  location-only server, which misses comments and strings. The authors
  propose routing between the two by task, model and noise ([2608.13568],
  dated 2026-06-29).
- Call and inheritance links written into the code as comments made agents
  steadier rather than smarter: localization rose 2.2 points, trajectories
  shortened, and run-to-run variance roughly halved, for about 10% more input
  tokens. Large repositories did better with only the links to callers
  ([2606.26979], 2026-06).
- Greptile builds a code graph and queries it on each review; reviews of it
  report more findings and more noise than CodeRabbit's ([sourcegraph-tools],
  a vendor comparison).

What carries over: a language service answers what a symbol is and who uses it
exactly, and its graph cuts a change better than file boundaries; text search
still sees what it cannot, comments, strings and generated code. A cut is
sound when the build accepts each unit.

### Context

- Diff-only review misses what lies outside the touched files. On security
  review, CodeRabbit found 16 of 24 target vulnerabilities whose evidence lay
  in the touched files, and 0 of 7 whose evidence lay outside them
  ([2608.25730]).
- Project history is usable context. DesignLore extracts design decisions from
  past review threads in the Linux kernel and raised F1 on design trade-off
  detection from 0.213 to 0.259 ([2610.21925]).
- Ericsson's reviewer combines specialized agent skills with project knowledge
  across four dimensions (readability, maintainability, reliability,
  performance). Developers judged 96% of its 200-plus issues correct, and 69% of
  those important ([2609.15877]).
- A wider review window hides more. Malicious changes spread across 24 PRs and
  reviewed as one window were caught 16–22% of the time, against 50–60% when
  each PR was reviewed alone ([2608.02693]).
- Presentation order matters to humans: latent bug rates rise with a file's
  position in the alphabetical list, and each extra file lowers the odds of any
  comment by 8.7% ([2609.22610]). Grouping changes by call graph helped
  reviewers orient ([2609.32057]). Whether either holds for a model is untested.

### Context Files

No study in the window measures what an `AGENTS.md` does to a reviewer. The
studies of coding agents, and what the vendors do:

- Context files did not raise task success and raised inference cost by over
  20%. Developer-written files gained about 4% and generated ones lost about
  3%. Agents follow the files' instructions closely, even where that hurts, and
  repository overviews did not help; the files help where they state
  non-standard practice ([2602.11988], revised 2026-09-29; first published
  before the window).
- The vendors read guideline files as review criteria, scoped to the files
  they govern, and say which ones they applied. CodeRabbit lists each guideline
  file used in its review summary (2026-09-30), and reads `.claude/rules/`,
  applying a rule with `paths` only to matching files (2026-10-15)
  ([coderabbit-changelog]). Anthropic's Code Review reads `CLAUDE.md` and a
  `REVIEW.md` that sets what to prioritize and ignore ([claude-code-review]).

What carries over: an `AGENTS.md` serves a reviewer as a list of the project's
non-standard rules, not as an overview. A reviewer that follows it literally
inherits its stale rules, and one written for authors (how to run the tests,
when to commit) is noise to a reviewer.

## Quality

The weak point is precision, not recall: a bot that is noisy gets ignored.

- Verify a comment against the repository before posting it. CRJudgeBench asks
  whether a comment's technical claim is true; general-purpose models judged
  poorly, and a 30B judge trained to gather code evidence reached 76.6%
  accuracy ([2609.37216]).
- Score the diagnosis, not the verdict. A reviewer can block a bad PR for an
  unrelated reason, and fixing what it reported leaves the real defect. PRGuard
  validates each candidate's premises with deterministic, non-executing tools
  ([2608.25730]).
- Tool output can mislead the verifier as well as the agent. Formatters that
  dropped search limits or result caps made verifiers accept wrong answers 90
  of 90 times; stating the omitted facts flipped it to 90 of 90 rejections
  ([2610.32110]).
- Multi-round review is harder than one round. On MCR-Bench, performance falls
  as rounds increase, from cross-round misalignment and weak long-range memory
  ([2608.27442]). Anthropic changed its re-reviews so they no longer open with
  approval while earlier blocking findings are open ([claude-code-releases]).
- Humans want evidence and provenance. 61.1% of 239 practitioners ask more
  evidence of an AI-authored PR, and the share who would block under residual
  doubt rises from 19.4% to 43.9%. They ask tools to separate generated claims
  from validated evidence and to show uncertainty ([2610.21144], [2610.11179]).
- Verification is the largest lever Anthropic reports: its team runs tests,
  the build and the linter, and a second agent reviews the diff in a fresh
  context, before the PR exists ([claude-code-review]).

### Reproducing Findings

The products that reproduce do it for their severe findings only, and label
the outcome rather than drop what does not reproduce:

- Codex Security Cloud runs each likely issue in a clean container. A finding
  that reproduces is marked validated; one that does not stays unvalidated,
  and the attempt is logged ([codex-security-cloud]).
- CodeRabbit writes a Reproducer Guide per finding and runs it in a coding
  environment, on demand or for each new finding ([coderabbit-changelog]).
- A test written in the same trajectory as the claim it checks can share the
  claim's error and lend it false confidence. ExecCritic has a separate agent
  write the tests, a fail-closed harness qualify and freeze them, and only then
  lets the fix use them. Tests from an untrained test agent lowered SWE-bench
  Verified from 61.2% to 57.3%; trained, the pair reached 72.6% ([2609.09133]).

What carries over: a reproduction proves a finding only when the reproducer is
written apart from the finding and must fail before the fix. Without the
environment to run it, a finding stays a claim.

## Evaluation

A review bot is measured two ways: once, offline, against a set of PRs whose
defects are known; and continuously, online, from what happens to its
comments and to the code after it reviewed.

| Source           | Measure                                                       | Result           |
| ---------------- | ------------------------------------------------------------- | ---------------- |
| [reviewbench]    | Senior engineers' agreement with the golden set               | 96.6%            |
| [dev-benchmarks] | ReviewBench golden findings that came from GitHub's own bot   | about 45%        |
| [2510.05450]     | Highest resolution rate by comment type, readability          | 43.3%            |
| [2607.01904]     | PRs with substantive human review, before and after a mandate | about 39% to 21% |

### One-Shot Benchmarks

ReviewBench (GitHub, 2026-10-05) is 219 PRs from 187 repositories in 19
languages, weighted toward large multi-file changes ([reviewbench]):

- Its golden set merges human comments, issues inferred from follow-up
  commits, static analysis, and several models, deduplicated and validated
  under one rubric: a finding counts if it is true, relevant and non-trivial.
- It reports grounded precision, recall and F1 against the golden set, and
  augmented scores where a judge credits valid findings the set lacks.
- The dataset, the judge and the matcher are versioned, and a 25-PR set for
  tuning is kept apart from the 219-PR run.

What goes wrong with such benchmarks ([dev-benchmarks], a comparison of nine
published on 2026-10-08):

- Most are run by a vendor, and rank that vendor first. "Catch rate",
  "precision" and "resolution rate" count different things.
- The answer key decides the result. A correct comment missing from it scores
  as a false positive, and about 45% of ReviewBench's golden findings came from
  GitHub's own reviewer.
- Some vendors tuned on the set they published. Small sets separate tools by a
  couple of catches.
- Its advice: test on your own merged PRs with known bugs (reverts and
  hotfixes), give every tool the same pre-fix diff, and count whether the bug
  was found, how many comments a person had to read, and the time and cost.

Matching a comment to a known defect is the core difficulty: comments are free
text with approximate lines. Before the window, one study matched in two
passes, a deterministic one (same file, lines within ±5, compatible type) and
then an LLM adjudicating the rest ([2606.15689]). Another scored a comment by
whether the fix it prompted passes tests, which avoids judging text
([2603.23448]). The judge needs validating in turn: general-purpose models
judged poorly whether a comment's claim was true ([2609.37216]). Multi-round
review needs its own set, since quality falls over rounds ([2608.27442]).

### Continuous Metrics

- GitHub calls its online precision the addressed rate: the share of comments
  an LLM judges to have prompted a matching code change, read from the diff,
  the thread, reactions, resolution state and the code after review. Online
  recall is how much human review was still needed. ReviewBench predicted the
  direction of every online change in its A/B test ([reviewbench]).
- Cursor's resolution rate counts comments addressed by merge, charted per
  repository over time ([dev-benchmarks]). Vendors pick different
  denominators, so the rates do not compare across tools.
- What is cheap to address gets addressed. At Atlassian, readability comments
  were resolved most often ([2510.05450], before the window).
- Coarse outcomes miss what review was for. Under a mandate to double merged
  PRs, substantive human review fell while silent approvals held near half
  ([2607.01904], before the window); a secondary summary reports merge and
  revert rates flat.
- Anthropic tracks the share of merged PRs later reverted, beside a daily run
  over its incident set ([claude-code-review]).

What carries over: an offline set built from the project's own reverts and
hotfixes, scored grounded and by a validated judge; online, the addressed rate
weighted by severity, defects that escaped review, and the comments and cost
per review. Offline and online should agree in direction, or one of them is
measuring something else.

## Cost and Quality

- Determinism is cheaper. OpenCodeReview's rule-based dispatch, bounded tools
  and parallel file agents used 5–15× fewer tokens than Claude Code and Codex,
  with up to 2.17× their semantic F1 across six backends ([2608.09290]).
- An ensemble need not cost more. GitHub's lite-tier ensemble raised precision
  and recall and lowered cost per review ([reviewbench]).
- Code quality drives review cost. Across 465 agent-authored PRs and three
  models, each CodeHealth point cut token cost by 3.5–6.1%, independent of PR
  size: agents iterate more on unhealthy code without writing more comments
  ([2610.21914]).
- Keep the cost policy out of the prompt. Telling a reviewer that a false
  accept costs 10× a false reject shifted its stated failure probability by
  13.6–16.9 points, and its actions were worse than rejecting everything.
  Eliciting the risk without the policy and applying the cost in code reduced
  loss ([2608.02677]).
- Fewer, structured agents beat many: three adversarial agents beat five
  ([2608.18167]), and debate pays only with coverage and a verifying readout
  ([2610.04686]).

| Source            | Measurement                                         | Result                    |
| ----------------- | --------------------------------------------------- | ------------------------- |
| [2608.09290]      | SEM-F1 on AACR-Bench (200 PRs, 10 langs)            | 25.10% vs 11.57% baseline |
| [2608.09290]      | Tokens vs Claude Code and Codex                     | 5–15× fewer               |
| [reviewbench]     | Ensemble, online A/B: addressed rate                | +8.0%                     |
| [reviewbench]     | Ensemble, online A/B: recall                        | +13.6%                    |
| [reviewbench]     | Ensemble, online A/B: comment volume                | +61%                      |
| [reviewbench]     | Ensemble, online A/B: cost per review               | −8.0%                     |
| [2610.21914]      | Token cost per CodeHealth point                     | −3.5% to −6.1%            |
| [2608.02677]      | Probability shift under a 10:1 cost prompt          | 13.6–16.9 points          |
| [2609.37216]      | Comment trustworthiness accuracy (Sentinel)         | 76.60%                    |
| [2610.21925]      | Design trade-off F1 (DesignLore)                    | 0.259 vs 0.213            |
| [2608.02693]      | Malicious-PR detection, 24-PR window                | 16–22% (50–60% per PR)    |
| [jev-engineering] | CVE fixes sent to full review vs a path rule        | 13 of 13 vs 0 of 13       |
| [jev-engineering] | Commits that skip full review                       | 42% of 561                |
| [jev-beri]        | Phishing, asked once vs five narrow questions       | 62.6% vs 95.0%            |
| [jev-datacamp]    | Accuracy, Jev vs GPT-5.6 Terra                      | 67.8% vs 67.9%            |
| [2609.09133]      | SWE-bench Verified, no tests vs both agents trained | 61.2% vs 72.6%            |
| [2602.11988]      | Inference cost added by context files               | over 20%                  |

## Learning

Three ways of improving over time appear, differing in what is changed and
what checks the change.

### Feedback Turned Into Rules

- CodeRabbit keeps "learnings": review preferences drawn from chat with it and
  applied to later reviews. Since 2026-08-26 an API can edit, delete and export
  them, filtered by file path, so a team can audit what was learned
  ([coderabbit-changelog]).
- Cursor's Bugbot turns reactions to its comments, a downvote marking a
  finding as unhelpful, into learned rules for later runs; Cursor counts more
  than 44,000 such rules over 110,000 repositories ([bugbot-learning],
  published before the window).
- A learned rule is a context-file instruction, and agents follow those
  literally, even where it hurts ([2602.11988]). A wrong rule keeps doing harm
  until someone deletes it.

### An Evaluation Set That Grows

Anthropic's Code Review does not learn from replies to its comments
([claude-code-review]):

- It keeps a set of past PRs that caused incidents and runs the reviewer over
  it daily. A change to the reviewer's prompts, tools or model is kept only if
  the score rises over the whole set.
- The set grows every time something goes wrong, and `REVIEW.md` is written by
  code owners, not learned. There is no loop from outcomes back into the gate.
- Between June and September 2026 merged PRs more than doubled while the share
  reverted fell by more than 40%.

### Self-Evolving Agents

- A survey sorts what such agents update: the framework, memory, skills and
  tools, the workflow, and the environment and context. Its open problems are
  feedback reliability, overfitting to the benchmark, reversibility, safety and
  cost ([2608.03392]).
- Ouroboros changes its own tools, prompts, context assembly and code, but
  only as reviewed commits. Benchmarks run on frozen snapshots, and the live
  deployment evolves on a separate lineage ([2608.08311]).
- The Mendel Gödel Machine rewrites its own source from its failure
  trajectories, from several tasks at once, and from other lineages
  ([2608.07645]).
- Reward hacking spreads between agents through shared code, shared memory
  and plans. A small open-weight agent can induce it in a stronger closed one,
  and it survives aggregation of candidates. Review by the agent itself or by a
  separate verifier reduced the hacks delivered ([2610.12822]).

What carries over: a learned item is safest as data a person can read, scope
and delete, and as a change that must pass a fixed evaluation set before it
takes effect. A memory shared by several models is also a channel for one
model's bad habit to reach the others.

## Sources

### Papers

- [2608.02677] When Policies Change Probabilities: Modular Decision-Making for
  LLM Code Review (2026-08-02)
- [2608.02693] PRWeaver: Evaluating LLM-Based Code Auditors against
  Long-Horizon Malicious Pull Requests (2026-08-03)
- [2608.03392] Self-Evolving Coding Agents (2026-08-04, revised 2026-09-24)
- [2608.07645] Mendel Gödel Machine: Recursive Self-Improving Coding Agents
  via Comparative Evolution (2026-08-07)
- [2608.08311] Ouroboros: A Self-Developing Frontier Coding Agent with
  Reviewed Core Evolution (2026-08-08)
- [2608.09290] OpenCodeReview: Determinism over Non-Determinism for
  Cost-Effective Agent-Based Code Review (2026-08-10)
- [2608.18167] Adversarial Review: Structured Disagreement for Grounded Agentic
  Code Review (2026-08-16)
- [2608.21311] AI-to-AI Code Reviews of GitHub Pull Requests (2026-08-21)
- [2608.25730] From Verdict to Diagnosis: Attributable Security Review of Pull
  Requests (2026-08-26)
- [2608.27442] From Static to Dynamic: Benchmarking Real-World Code Review with
  MCR-Bench (2026-08-27)
- [2406.04692] Mixture-of-Agents Enhances Large Language Model Capabilities
  (2024-06)
- [2512.08296] Towards a Science of Scaling Agent Systems (2025-12)
- [2510.05450] What Types of Code Review Comments Do Developers Most
  Frequently Resolve? (2025-10)
- [2511.00262] LLM-Driven Cost-Effective Requirements Change Impact Analysis
  (2025-11)
- [2602.01011] Multi-Agent Teams Hold Experts Back (2026-02)
- [2602.11988] Evaluating AGENTS.md: Are Repository-Level Context Files
  Helpful for Coding Agents? (2026-02, revised 2026-09-29)
- [2603.23448] Code Review Agent Benchmark (2026-03)
- [2606.15689] Bigger Isn't Always Better: A Comparative Evaluation of LLMs for
  Automated Code Review (2026-06)
- [2607.01904] AI Writes Faster Than Humans Can Review: A Longitudinal Study
  of an Enterprise 2x Mandate (2026-07)
- [2606.01351] Recognize Your Orchestrator: An Entropy Dynamics Perspective
  for LLM Multi-Agent Systems (2026-06)
- [2606.26979] How Much Static Structure Do Code Agents Need? A Study of
  Deterministic Anchoring (2026-06-25, revised 2026-07-02)
- [2608.13568] Does a Language Server Save Tokens for Coding Agents? A
  Measurement Methodology and Preliminary Study (2026-06-29)
- [2610.11593] Runnable Commit Untangling for Coding Agents (2026-10-08)
- [2608.30661] SwarmBench: Can Large Language Models Act as Agent Swarm
  Orchestrators? (2026-08-31)
- [2603.17111] Hidden Clones: Exposing and Fixing Family Bias in
  Vision-Language Model Ensembles (2026-03)
- [2605.29800] Nine Judges, Two Effective Votes (2026-05)
- [2607.20768] Are Diversity Metrics Measuring Diversity? A
  Capability-Controlled Audit of Majority-Vote Gain in LLM Ensembles (2026-07)
- [2607.21656] Cross-Model LLM Code Review: Should you use Claude to review
  Codex or vice versa? (2026-07-22)
- [2608.18091] Self- and Other-Labels Induce Bidirectional Bias in LLM Judges
  (2026-06-06, revised 2026-10-16)
- [2609.17857] Who Judges Matters: Measuring Family-Conditioned Preference in
  LLM-as-Judge Panels (2026-09-15)
- [2609.22512] Agreement Overstates Evidence: Error Dependence in LLM Judge
  Consensus (2026-09)
- [2609.09133] ExecCritic: Learn to Test, Test to Improve for Coding Agents
  (2026-09-08)
- [2609.15877] Using Agentic AI for contextualized and multifaceted code
  review at Ericsson (2026-09-14)
- [2609.22610] Does Order Matter? An Empirical Investigation into the Impact
  of File Ordering on Code Review Effectiveness (2026-09-18)
- [2609.32057] CG-Diff: Organizing Code Changes Around Call Graphs (2026-09-25)
- [2609.37216] CRJudgeBench: Can AI Detect Plausible but Invalid Code Reviews?
  (2026-09-29)
- [2610.04686] When Debate Helps: Proposal Supply and Verification-Aware
  Readout in Multi-Agent Reasoning (2026-10-03)
- [2610.11179] Who Pays the Review Cost? Triage, Fairness, and Accountability
  in AI-authored Pull Requests (2026-10-08)
- [2610.12822] Reward Hacking as a Transmissible Strategy in Multi-Agent
  Coding Systems (2026-10-09)
- [2610.21144] Plausible Is Not Enough: Trust and Evidence in AI-Authored Pull
  Request Review (2026-10-15)
- [2610.21914] CodeHealth Predicts the Token Cost of Agentic Code Review
  (2026-10-15)
- [2610.21925] Benchmarking and Enhancing Design-Aware Code Review in the Linux
  Kernel (2026-10-15)
- [2610.32110] Who Verifies the Tools? How Warped Formatters Mislead Agents and
  Their Verifiers (2026-10-23)

### Industry

- [reviewbench] GitHub, ReviewBench: An open benchmark for AI code review
  (2026-10-05)
- [claude-code-review] Anthropic's Claude Code team on how it does code review
  (2026-10-15), as reported by OrcaRouter
- [claude-code-releases] Claude Code release notes, October 2026
- [bugbot-learning] Cursor, Bugbot now self-improves with learned rules
  (2026-04)
- [coderabbit-changelog] CodeRabbit changelog, August to October 2026
- [dev-benchmarks] Should we trust AI code review benchmarks? (2026-10-08)
- [sourcegraph-tools] Sourcegraph, 13 Best Automated Code Review Tools in 2026
- [coderabbit-context] CodeRabbit, The art and science of context engineering
- [codex-security-cloud] eesel, Codex Security Cloud: how it works
  (2026-10-01)
- [jev-innfactory] innFactory, Jev: a System One model is a classifier, not an
  LLM (2026-09-21)
- [jev-datacamp] DataCamp, System One models: Jev
- [jev-beri] BERI, TypeSafe's Jev scores 62.6% asked once and 95% split five
  ways
- [jev-engineering] eugeniughelbur/jev-engineering: review-router and jev-gate
- [jev-gate] totally-tim/jev-gate: calibrated PR review gates on Jev

[2608.02677]: https://arxiv.org/abs/2608.02677
[2608.02693]: https://arxiv.org/abs/2608.02693
[2608.09290]: https://arxiv.org/abs/2608.09290
[2608.18167]: https://arxiv.org/abs/2608.18167
[2608.21311]: https://arxiv.org/abs/2608.21311
[2608.25730]: https://arxiv.org/abs/2608.25730
[2608.27442]: https://arxiv.org/abs/2608.27442
[2609.15877]: https://arxiv.org/abs/2609.15877
[2609.22610]: https://arxiv.org/abs/2609.22610
[2609.32057]: https://arxiv.org/abs/2609.32057
[2609.37216]: https://arxiv.org/abs/2609.37216
[2610.04686]: https://arxiv.org/abs/2610.04686
[2610.11179]: https://arxiv.org/abs/2610.11179
[2610.21144]: https://arxiv.org/abs/2610.21144
[2610.21914]: https://arxiv.org/abs/2610.21914
[2610.21925]: https://arxiv.org/abs/2610.21925
[2610.32110]: https://arxiv.org/abs/2610.32110
[reviewbench]: https://github.blog/ai-and-ml/github-copilot/reviewbench-an-open-benchmark-for-ai-code-review/
[claude-code-review]: https://www.orcarouter.ai/blog/how-the-claude-code-team-does-code-review
[claude-code-releases]: https://releasebot.io/updates/anthropic/claude-code
[2406.04692]: https://arxiv.org/abs/2406.04692
[2512.08296]: https://arxiv.org/abs/2512.08296
[2510.05450]: https://arxiv.org/abs/2510.05450
[2511.00262]: https://arxiv.org/abs/2511.00262
[2603.23448]: https://arxiv.org/abs/2603.23448
[2606.15689]: https://arxiv.org/abs/2606.15689
[2607.01904]: https://arxiv.org/abs/2607.01904
[dev-benchmarks]: https://dev.to/izgorodin/should-we-trust-ai-code-review-benchmarks-3p6n
[2602.01011]: https://arxiv.org/abs/2602.01011
[2606.26979]: https://arxiv.org/abs/2606.26979
[2608.13568]: https://arxiv.org/abs/2608.13568
[2610.11593]: https://arxiv.org/abs/2610.11593
[sourcegraph-tools]: https://sourcegraph.com/blog/automated-code-review-tools
[2602.11988]: https://arxiv.org/abs/2602.11988
[2606.01351]: https://arxiv.org/abs/2606.01351
[2608.30661]: https://arxiv.org/abs/2608.30661
[2603.17111]: https://arxiv.org/abs/2603.17111
[2605.29800]: https://arxiv.org/abs/2605.29800
[2607.20768]: https://arxiv.org/abs/2607.20768
[2607.21656]: https://arxiv.org/abs/2607.21656
[2608.18091]: https://arxiv.org/abs/2608.18091
[2609.17857]: https://arxiv.org/abs/2609.17857
[2609.22512]: https://arxiv.org/abs/2609.22512
[2608.03392]: https://arxiv.org/abs/2608.03392
[2608.07645]: https://arxiv.org/abs/2608.07645
[2608.08311]: https://arxiv.org/abs/2608.08311
[2610.12822]: https://arxiv.org/abs/2610.12822
[bugbot-learning]: https://cursor.com/blog/bugbot-learning
[2609.09133]: https://arxiv.org/abs/2609.09133
[coderabbit-changelog]: https://docs.coderabbit.ai/changelog
[coderabbit-context]: https://www.coderabbit.ai/blog/the-art-and-science-of-context-engineering
[codex-security-cloud]: https://www.eesel.ai/blog/codex-security-cloud
[jev-innfactory]: https://innfactory.ai/en/blog/jev-system-one-model-classifier-not-llm/
[jev-datacamp]: https://www.datacamp.com/de/blog/system-one-models-jev
[jev-beri]: https://www.beri.net/article/typesafe-jev-typed-decision-model-calibration-decomposition-shadow-eval
[jev-engineering]: https://github.com/eugeniughelbur/jev-engineering
[jev-gate]: https://github.com/totally-tim/jev-gate
