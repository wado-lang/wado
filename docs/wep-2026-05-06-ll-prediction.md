# WEP: Static-Analysis LL(\*) Repair via `__follow_<id>` Variants

## Context

[Gale](./wep-2026-03-02-gale.md) is Wado's parser generator targeting
ANTLR4 `.g4` compatibility. ANTLR4 uses **adaptive LL(\*) prediction**
(`ParserATNSimulator` in
`vendor/antlr4/runtime/Java/src/org/antlr/v4/runtime/atn/`): when SLL
prediction is ambiguous it switches to full-context analysis,
considering the call stack and what each caller expects to follow.

Gale's prediction is SLL-only. Real `.g4` grammars exhibit shapes
where SLL and LL diverge, and Gale picks a different alternative
than ANTLR4 would. The canonical reproducer (also `tests/grammars/
ll_basic.g4`):

```antlr
r : (a b | a) EOF ;
a : X Y? ;
b : Y ;
```

For input `X Y`, ANTLR4's LL prediction yields
`(r (a X) (b Y))` — alt 0 wins because `b` claims the trailing `Y`.
SLL/greedy yields `(r (a X Y))` because `a`'s `Y?` consumes `Y`
without knowing the caller (`b`) needs it.

A full ATN simulator is a significant engineering investment, and
Stage C (action / predicate execution — see
[`antlr4-compatibility.md`](../package-gale/antlr4-compatibility.md))
sits behind LL because the harder LL cases in real grammars use
predicates as a tiebreaker. We want a sound prediction story
*before* we land predicate execution.

## Decision

Implement static-analysis-driven LL repair as **per-call-site
variant functions**: when a `RuleRef R` call site has a follow set
that overlaps `R`'s tail-greedy set, generate a
`scan_R__follow_<id>` / `parse_R__follow_<id>` whose body suppresses
the overlapping tail-greedy consumption. Compute the analysis at
codegen time, never at runtime.

This is one design point on a spectrum. The two endpoints are:

- **No LL repair** — Gale stays SLL, real grammars diverge.
- **Runtime ATN simulator** — full ANTLR4 fidelity, large engineering
  cost.

Static variant emit lives in between: it closes specific shapes that
ANTLR4 LL handles via static FOLLOW analysis, leaves the rest for
the simulator. The shapes it handles are exactly those where ANTLR4
itself would resolve via FOLLOW lookahead before falling back to
context-sensitive DFA construction.

### Mechanism

`GenContext` exposes two analyses:

- `tail_greedy_first_of_rule(R)` — the set of token kinds that
  `R`'s body may **greedily** consume at tail position. Walks each
  alt from the tail, accumulating `first_of_element(inner)` for
  tail-position `Repeat` elements; stops at the first
  non-deeply-nullable element. Transitively follows tail RuleRefs.

- `intern_follow_variant(R, caller_follow, is_scan)` — registers a
  variant for `(R, mask)` where
  `mask = tail_greedy_first(R) ∩ caller_follow`. Same `(R, mask)`
  pair always returns the same `id`, so call sites with different
  but outside-the-intersection follows share a single variant.

Two `GenContext` fields thread the mask and per-call-site follow:

- `current_follow_mask` — the mask in effect when emitting a
  variant body. Subtracted from tail-position `Repeat` first sets in
  `gen_scan_repeat` / `gen_repeat`.

- `ruleref_call_follow` — the caller-local FOLLOW at the current
  `RuleRef` emission site. Set by alt-element walkers per position;
  read by `RuleRef` branches in `gen_scan_element` / `gen_element`
  to look up a variant via `intern_follow_variant`.

A fixed-point loop at the end of `gen_parser` emits all registered
variants. A variant body's own `RuleRef` calls may register
additional variants in cascade.

### Soundness conditions

The repair must not consume tokens the caller depends on, but it
must also not refuse to consume tokens that legitimately belong to
the callee. Three conditions decide whether the repair is sound at
a given site, and they are necessary — not v1-specific:

1. **Single-token tail-greedy inner.** A `Repeat` whose inner
   consumes more than one token per iteration cannot be safely
   suppressed by a follow mask. The mask suppresses the iteration's
   first-token check, but the inner's deeper tokens may legitimately
   re-enter on overlapping tokens (HTMLParser's `htmlContent` rule
   re-enters on TAG_OPEN; the closing `</div>`'s TAG_OPEN is the
   same token). Static analysis can't distinguish the two.
   `tail_greedy_first_of_element` enforces this: only `Repeat`s
   whose inner is `element_is_single_token` contribute.

2. **Strictly required next sibling.** When the caller's immediate
   next sibling is nullable, its first set might or might not be
   claimed at runtime, and the runtime decision depends on what
   comes after the nullable element. Suppressing the callee's
   tail-greedy unconditionally is unsound (CSS3's `selector :
   simpleSelectorSequence ws (combinator …)*` — `ws`'s follow
   includes Space from `combinator`'s first set, but `ws` should
   still consume Space when no combinator follows). Alt-element
   walkers only set `ruleref_call_follow` when
   `tail_is_nullable_deep(elements, i + 1)` is false.

3. **Variant emit can faithfully reproduce the callee body.** A
   `__follow_<id>` variant must emit a function with the same
   shape as the regular `scan_R` / `parse_R`, parameterised by
   the mask. Anything `gen_parse_fn` and `gen_scan_function` know
   how to emit, the variant emit must mirror.
   `intern_follow_variant` rejects rules the emit pass cannot
   reproduce, and `gen_scan_follow_variant` /
   `gen_parse_follow_variant` `panic!` on contract violation so a
   future relaxation cannot leak dangling references into
   generated source.

These are not arbitrary v1 cutoffs. (1) and (2) are inherent
limits of static FOLLOW analysis — closing them requires either a
multi-token lookahead extension or a runtime decision. (3) is a
fundamental invariant: the registry must not promise variants the
emit pass cannot deliver.

### Group dispatch: `sort_group_by_mandatory_count_desc`

Group-level scan dispatches (`gen_consume_group`, `gen_*_group_*`,
the prediction tree's `Ambiguous` case) try alts in
**most-mandatory-elements first** order so first-match-wins commits
to the longer match for prefix-overlap cases.

The pre-existing `sort_group_by_element_count` keyed on
`alt_sort_priority` (single-RuleRef catch-alls before
multi-element-RuleRef siblings) — correct for **rule-level**
dispatch where the catch-all alt is intentionally tried first, but
wrong for **group-level** prefix overlap where the longer match is
the LL-correct pick.

`sort_group_by_mandatory_count_desc` keys on
`mandatory_element_count` (count of non-deeply-nullable top-level
elements, computed via `ctx.element_is_nullable_deep`) and falls
back to `alt_sort_priority`, then source order. Deep nullability
is essential: a `RuleRef` to a fully-nullable rule must not inflate
the count, otherwise `(a c | a)` where `c : ;` ties incorrectly.

This is the LL distinguisher that makes `(a b | a)` (mandatory
lengths 2 and 1) sort the longer alt first, while
`(table_or_subquery (',' tor)* | join_clause)` (both mandatory
length 1) ties and falls to priority. Rule-level dispatch keeps
the priority-primary `sort_group_by_element_count` because its
catch-all semantics are intentional there.

### Call-site dispatch

`gen_scan_element` and `gen_element`'s `RuleRef` branches:

```
follow ← ctx.ruleref_call_follow
if follow.is_empty() OR use_prec:
    emit "scan_R(...)" / "parse_R(...)"
else:
    id ← ctx.intern_follow_variant(R, follow, is_scan)
    if id.is_some():
        emit "scan_R__follow_<id>(...)" / "parse_R__follow_<id>(...)"
    else:
        emit "scan_R(...)" / "parse_R(...)"
```

The `use_prec` exclusion is for LR-self-ref calls inside prefix
alts (`gen_element` consults `ctx.prefix_self_ref_min_prec`). Those
calls go through the precedence-climbing helper, which has its own
emit shape that variant emit doesn't mirror.

## Consequences

### Persistent

- Generated parsers contain `__follow_<id>` functions for each
  registered `(R, mask)` pair. The variant emit pass runs to a
  fixed point so cascades terminate without manual ordering.
- Group-level dispatch sort is split from rule-level dispatch sort.
  The rule-level sort retains priority semantics; the group-level
  sort is mandatory-count-first.
- `tail_greedy_first` is part of `GenContext`'s public surface and
  is also used by the analyses listed above for soundness gating.

### Snapshot of current coverage

The active set of fixtures and the gaps tracked in
`package-gale/TODO.md`'s LL section is the source of truth for
"what works today / what's next." This document does not duplicate
that table — see `TODO.md` for the moving target.

The architecture above admits the catalogued extensions (deeper
follow propagation, multi-alt variant emit, LR variant emit) as
incremental work behind the same `intern_follow_variant` /
variant-emit contract. None of them require revising the design;
they require lifting the soundness conditions in (1)–(3) by either:

- Extending the static analysis (e.g. compute `FOLLOW(R)` as a
  call-graph fixed point so caller-of-caller siblings count
  toward `caller_follow`); or
- Extending the variant emit to reproduce more body shapes (e.g.
  `gen_parse_follow_variant` learns the multi-alt body shape, or
  emits separate atom + LR-suffix helpers).

### Limits of the approach

There exist grammars where static FOLLOW cannot decide the
LL-correct alt — typically those where the decision depends on
arbitrary lookahead through ambiguous prefixes. Those grammars
require runtime ATN simulation, the same machinery ANTLR4 falls
back to. The decision to start with static repair is a cost-vs-
coverage trade-off; future work may revisit it.

For measurement against ANTLR4 behaviour, the JVM `tool/` is
vendored. A future Stage B' could shell out to it for descriptors
whose `[output]` is action-printed (the
`FullContextParsing/*` category, etc.) so they stop being
auto-skipped by `normalize_output_for_stage_b`. This is
infrastructure work, independent of the static-vs-runtime LL
question.
