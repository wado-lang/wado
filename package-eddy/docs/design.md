# Eddy Design Notes

Decisions taken so far, and the leanings not yet settled. The evidence behind
them is in [the research](./research-code-review-bots.md).

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
- [x] A learning found harmful is deleted without a person deciding it.

## Leanings

- [ ] Show `AGENTS.md` to the reproduction agent only. It holds how to build,
  test and run the project, which reproduction needs; to a reviewer most of
  it is noise that it would follow literally.
- [ ] Choose the verifier by measurement, not by vendor. Across vendors errors
  still correlate; what pays is the pair that rarely misses the same defect on
  Eddy's evaluation set, a verifier no weaker than the code's author, and
  judges that cannot see who wrote the code or the finding.

## Open Questions

### Outcome Signals

- "Addressed" can be gamed: a reviewer learns to post findings that are cheap
  to address. Weight it by severity, and count a finding addressed only when
  the follow-up commit touches the lines it named.
- A revert has many causes. Count one against a review only when the reverted
  or fixed lines overlap what the review saw, and as a miss when no finding
  named them.
- The threshold time trades signal delay against attribution: a short one
  misses late regressions, a long one blames the review for later changes.

### Deleting Harmful Learnings

Each option assumes a learning is data with provenance (the outcome that
produced it, the model that wrote it) and a scope (the paths it applies to).

1. Ablation on the evaluation set. Every time the set grows, rerun it with each
   learning removed, and delete those whose removal does not lower the score.
   It needs no live traffic, but costs one run per learning.
2. Held-out exposure. Apply each learning to a random half of the reviews in
   its scope and compare outcomes. It measures real effect, but withholds a
   good learning from half the reviews and needs volume to decide.
3. Expiry. A learning lapses after a fixed time unless outcomes earned it
   again. It is cheap and bounds the damage, but drops good learnings too.
4. Quarantine by model. A learning written from one model's run reaches the
   other models only after it passed 1 or 2, since reward hacking spreads
   through shared memory.

These combine: 1 as the gate, 3 as the backstop, 4 for the multi-model stage.
