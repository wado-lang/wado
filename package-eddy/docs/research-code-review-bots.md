# Research: Code Review Bots, August to October 2026

This survey covers how code review bots are built, what raises their quality,
and how they trade cost against quality. It reads only work published from
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

### Measuring It

ReviewBench (GitHub, 2026-10-05) is 219 PRs from 187 repositories in 19
languages, weighted toward large multi-file changes ([reviewbench]). Its golden
set merges human comments, issues inferred from follow-up commits, static
analysis, and several models. It reports two scores:

- Grounded precision, recall and F1, against the golden set only.
- Augmented scores, where a judge credits valid findings the golden set lacks.

Senior engineers who relabeled the golden set agreed with it 96.6% of the time.
Its offline predictions for GitHub's ensemble matched the online A/B test in
direction (see the table below).

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

## What This Suggests for Eddy

These are directions the findings point to, not decisions.

- Spend the model where judgment is needed: a fast dispatcher, bounded tools
  for review, a verifier with a narrower view than the reviewer. A System One
  model may beat rules at dispatch, but only shadow-evaluated against labelled
  history first.
- Reproduce what is severe. A reproducer written apart from the finding, run
  where the code runs, turns a claim into evidence; the label says which.
- Read `AGENTS.md` as criteria, not as a briefing: pick the rules that govern
  the touched files, and report which were applied.
- Make disagreement explicit and grounded. Choose models for what they find
  that the others do not, and aggregate by evidence rather than by vote count.
- Post less. Every comment should survive a refutation step against the
  repository and carry its evidence.
- Keep risk estimation and the accept/block policy in separate stages, with the
  policy in code.
- Review one PR at a time, and track open findings across rounds.
- Measure with a grounded benchmark: ReviewBench's split of grounded and
  augmented scores is the model to follow.

## Sources

### Papers

- [2608.02677] When Policies Change Probabilities: Modular Decision-Making for
  LLM Code Review (2026-08-02)
- [2608.02693] PRWeaver: Evaluating LLM-Based Code Auditors against
  Long-Horizon Malicious Pull Requests (2026-08-03)
- [2608.09290] OpenCodeReview: Determinism over Non-Determinism for
  Cost-Effective Agent-Based Code Review (2026-08-10)
- [2608.18167] Adversarial Review: Structured Disagreement for Grounded Agentic
  Code Review (2026-08-16)
- [2608.21311] AI-to-AI Code Reviews of GitHub Pull Requests (2026-08-21)
- [2608.25730] From Verdict to Diagnosis: Attributable Security Review of Pull
  Requests (2026-08-26)
- [2608.27442] From Static to Dynamic: Benchmarking Real-World Code Review with
  MCR-Bench (2026-08-27)
- [2602.11988] Evaluating AGENTS.md: Are Repository-Level Context Files
  Helpful for Coding Agents? (2026-02, revised 2026-09-29)
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
- [coderabbit-changelog] CodeRabbit changelog, August to October 2026
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
[2602.11988]: https://arxiv.org/abs/2602.11988
[2609.09133]: https://arxiv.org/abs/2609.09133
[coderabbit-changelog]: https://docs.coderabbit.ai/changelog
[coderabbit-context]: https://www.coderabbit.ai/blog/the-art-and-science-of-context-engineering
[codex-security-cloud]: https://www.eesel.ai/blog/codex-security-cloud
[jev-innfactory]: https://innfactory.ai/en/blog/jev-system-one-model-classifier-not-llm/
[jev-datacamp]: https://www.datacamp.com/de/blog/system-one-models-jev
[jev-beri]: https://www.beri.net/article/typesafe-jev-typed-decision-model-calibration-decomposition-shadow-eval
[jev-engineering]: https://github.com/eugeniughelbur/jev-engineering
[jev-gate]: https://github.com/totally-tim/jev-gate
