# Research: Code Review Bots, August to October 2026

This survey covers how code review bots are built, what raises their quality,
and how they trade cost against quality. It reads only work published from
2026-08-01 to 2026-10-26: the models changed enough in August 2026 that older
results say little about current ones.

## Method

- arXiv: the API query `abs:"code review"` with `submittedDate` in the window,
  newest first. The papers below were read from their abstracts, not their full
  text, so a figure here is the abstract's.
- Industry: web searches for posts in the window from GitHub and Anthropic.
  These are vendor reports, and their figures are the vendor's own.

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

| Source        | Measurement                                 | Result                    |
| ------------- | ------------------------------------------- | ------------------------- |
| [2608.09290]  | SEM-F1 on AACR-Bench (200 PRs, 10 langs)    | 25.10% vs 11.57% baseline |
| [2608.09290]  | Tokens vs Claude Code and Codex             | 5–15× fewer               |
| [reviewbench] | Ensemble, online A/B: addressed rate        | +8.0%                     |
| [reviewbench] | Ensemble, online A/B: recall                | +13.6%                    |
| [reviewbench] | Ensemble, online A/B: comment volume        | +61%                      |
| [reviewbench] | Ensemble, online A/B: cost per review       | −8.0%                     |
| [2610.21914]  | Token cost per CodeHealth point             | −3.5% to −6.1%            |
| [2608.02677]  | Probability shift under a 10:1 cost prompt  | 13.6–16.9 points          |
| [2609.37216]  | Comment trustworthiness accuracy (Sentinel) | 76.60%                    |
| [2610.21925]  | Design trade-off F1 (DesignLore)            | 0.259 vs 0.213            |
| [2608.02693]  | Malicious-PR detection, 24-PR window        | 16–22% (50–60% per PR)    |

## What This Suggests for Eddy

These are directions the findings point to, not decisions.

- Spend the model where judgment is needed: rules for dispatch, bounded tools
  for review, a verifier with a narrower view than the reviewer.
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
