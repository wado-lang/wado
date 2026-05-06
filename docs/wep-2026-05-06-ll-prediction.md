# WEP: Static-Analysis LL(\*) Repair via `__follow_<id>` Variants

## Context

[Gale](./wep-2026-03-02-gale.md) is Wado's parser generator targeting
ANTLR4 `.g4` compatibility. ANTLR4 uses **adaptive LL(\*) prediction**
(`ParserATNSimulator` in
`vendor/antlr4/runtime/Java/src/org/antlr/v4/runtime/atn/`): when SLL
prediction is ambiguous it switches to full-context analysis,
considering the call stack and what each caller expects to follow.

Gale's prediction has historically been SLL-only. Real `.g4`
grammars exhibit shapes where SLL and LL diverge, and Gale picks a
different alternative than ANTLR4 would. The seed reproducer is
`tests/antlr4-compat/grammars/ParserExec/PredictionMode_LL.g4`:

```antlr
r : (a b | a) EOF ;
a : X Y? ;
b : Y ;
```

For input `X Y`, ANTLR4's LL prediction yields
`(r (a X) (b Y))` (alt 0 wins because `b` claims the trailing `Y`).
SLL/greedy yields `(r (a X Y))` because `a`'s `Y?` consumes `Y`
without knowing the caller (`b`) needs it.

A full ATN simulator is a significant engineering investment.
Stage C (action / predicate execution) sits behind LL because the
hard LL cases in real grammars use predicates as a tiebreaker —
prediction has to be sound first.

## Decision

Implement a **one-level static-analysis LL repair**: when a
`RuleRef R` call site has a strictly required next sibling whose
first set overlaps `R`'s tail-greedy set, emit a
`scan_R__follow_<id>` / `parse_R__follow_<id>` variant that
suppresses the overlapping tail-greedy consumption inside `R`.

This is intentionally narrow. It closes `PredictionMode_LL` and is
the minimum viable LL behavior — every other LL gap remains as
documented `#[TODO]` regression fixtures. Stage B coverage is
preserved (no descriptor regresses).

### Mechanism

Two new pieces of analysis on `GenContext`:

- `tail_greedy_first_of_rule(R)` — the set of token kinds that
  `R`'s body may **greedily** consume at tail position. Walks each
  alt from the tail, accumulating `first_of_element(inner)` for any
  `Repeat (Optional|Star|Plus)` whose inner is **a single token**
  (TokenRef / Literal / Wildcard / Not / single-token RuleRef);
  stops at the first non-deeply-nullable element. Transitively
  follows tail RuleRefs.

- `intern_follow_variant(R, caller_follow, is_scan)` — registers a
  variant for `(R, mask)` where
  `mask = tail_greedy_first(R) ∩ caller_follow`. Returns `None`
  when:
  - `R` is unknown, or
  - `R` is left-recursive (variant emit doesn't mirror the LR-helper
    split yet), or
  - `R` has more than one alternative (parse-side variant emit only
    handles single-alt `gen_alt_body`), or
  - `caller_follow` is empty, or
  - the intersection is empty.

  Otherwise returns `Some(id)`; the same `(R, mask)` always returns
  the same `id`, so call sites with different but
  outside-the-intersection follows share a single variant.

Two ctx fields thread the mask and per-call-site follow:

- `current_follow_mask: Array<String>` — the mask currently in
  effect when emitting a variant body. Empty for the regular
  `scan_R` / `parse_R` paths. Subtracted from tail-position
  `Optional` / `Star` / `Plus` first sets in `gen_scan_repeat` /
  `gen_repeat`.

- `ruleref_call_follow: Array<String>` — the caller-local FOLLOW
  set at the current `RuleRef` emission site. Set by alt-element
  walkers (`gen_scan_elements`, `gen_alt_elements`,
  `gen_scan_elements_in_block`, `gen_elements_with_non_greedy`,
  `gen_alt_body_skip`, the second loop of
  `gen_group_scan_dispatch`) per position, then cleared. Read by
  `RuleRef` branches in `gen_scan_element` / `gen_element` to
  decide whether to call the variant.

A fixed-point loop at the end of `gen_parser` emits all registered
variants (a variant body's own `RuleRef` calls may register
additional variants in cascade).

### Group-dispatch sort: `sort_group_by_mandatory_count_desc`

Group-level scan dispatches (the 5 `gen_*_group_*` paths plus the
prediction tree's `Ambiguous` case) need to try the alt with the
most **mandatory** elements first, so first-match-wins commits to
the longer match for prefix-overlap cases. The prior
`sort_group_by_element_count` used `alt_sort_priority` as the
primary key (single-RuleRef catch-alls before multi-element-RuleRef
siblings) — correct for rule-level dispatch but wrong for
prefix-overlap groups.

`sort_group_by_mandatory_count_desc` uses
`mandatory_element_count` (count of non-deeply-nullable top-level
elements, computed via `ctx.element_is_nullable_deep` — _not_ the
shallow `is_nullable`) as the primary key, then `alt_sort_priority`
as a tiebreaker, then source order. This preserves the
SQLite-style `(table_or_subquery (',' tor)* | join_clause)` where
both alts have mandatory length 1 and priority picks `join_clause`,
while `(a b | a)` (mandatory lengths 2 and 1) sorts the longer alt
first.

Rule-level dispatch (`gen_scan_multi_alt`, `gen_parse_fn`) keeps
the priority-primary `sort_group_by_element_count`. PrefixAndOtherAlt
descriptors stay green.

### Conservative guards (and why each one is there)

Three guards keep the repair from breaking grammars where static
analysis can't tell "tail-greedy that should yield" from
"tail-greedy that legitimately re-enters." See AGENTS.md "Failed
Approaches" for the historical attempts each guard prevents from
recurring.

| Guard | Where | Why |
| --- | --- | --- |
| Single-token inner only | `tail_greedy_first_of_element`'s `Repeat` arm | HTMLParser's `htmlContent` re-enters on TAG_OPEN; multi-token inners are unsafe to suppress. |
| Suffix non-nullable only | `gen_alt_elements` and friends | CSS3's `selector : … ws (combinator …)*` — `ws`'s follow at position 1 includes Space (from combinator's first set), but `ws` should still consume Space when no combinator follows. |
| Single-alt non-LR callee only | `intern_follow_variant` | Variant emit doesn't yet handle multi-alt bodies (`gen_parse_fn` shape) or LR helper splits (`gen_scan_lr_functions`); rejecting at intern time keeps the registry's invariant aligned with the emit pass's capabilities. |

### Call-site dispatch

`gen_scan_element` and `gen_element`'s `RuleRef` branches consult
`ctx.ruleref_call_follow`. If non-empty,
`ctx.intern_follow_variant(rule, follow, is_scan)` is called; on
`Some(id)` the call site emits `scan_R__follow_<id>` /
`parse_R__follow_<id>` instead of the regular
`scan_R` / `parse_R`.

The contract `intern_follow_variant` enforces is consumed by
`gen_scan_follow_variant` and `gen_parse_follow_variant`: those
emit functions `panic!` on contract violation (rule must exist,
must be non-LR, must be single-alt for parse) so a future
relaxation in `intern_follow_variant` cannot leak dangling
`__follow_<id>` references into generated source.

## Consequences

### What works in v1

- Direct tail-greedy Optional/Star/Plus on a single-token element
  in a single-alt non-LR rule, called from a position whose
  immediate next sibling is strictly required.
- Concretely: `PredictionMode_LL`, `ll_basic.g4`, every existing
  Stage B descriptor (no regressions).
- Variant emit cascades correctly via the fixed-point loop.

### What does not work in v1

Tracked as `#[TODO]` driver tests under `tests/grammars/ll_*.g4`
and tabulated in `package-gale/TODO.md`'s LL section:

- `ll_nullable_suffix.g4` — nullable next sibling.
- `ll_multi_alt.g4` — multi-alt callee.
- `ll_lr_atom.g4` — left-recursive callee.
- `ll_ctx_follow.g4` — passthrough rule between caller and
  tail-greedy callee (needs CTX_FOLLOW propagation).
- Multi-token tail-greedy inner (no fixture yet).

### Risks documented in code

- `gen_scan_repeat` / `gen_repeat` save and restore
  `ruleref_call_follow` around the body so callers (e.g.
  `gen_scan_optional_with_lookahead`) keep their context. The
  earlier "alt walker re-clears next iteration" contract was
  fragile.
- `gen_parse_follow_variant` saves and restores
  `current_rule_name`, `group_counter`, `alt_gc_starts`, and
  `current_follow_mask`. The fixed-point loop and trailing
  `gen_parse_entry` see the same context they would without the
  variant detour.
- `mandatory_element_count` uses deep nullability via
  `ctx.element_is_nullable_deep` — a `RuleRef` to a deeply-nullable
  rule does not contribute to the count, so a rule like `r : a*`
  where `a` itself can match zero tokens correctly ties on
  mandatory length with a sibling alt.

### Beyond v1

Each `#[TODO]` fixture is independently actionable:

1. `ll_ctx_follow` — compute a per-rule `FOLLOW(R)` as the fixed
   point of all call sites' caller-local follows; route through
   `intern_follow_variant`. Pure addition, no variant emit changes.
   Likely also closes `ll_nullable_suffix`.
2. `ll_multi_alt` — extend `gen_parse_follow_variant` to emit
   multi-alt bodies (mirror `gen_parse_fn`'s overlap handling).
3. `ll_lr_atom` — emit `scan_R__follow_<id>_atom` and per-LR-alt
   suffix helpers; mirror `gen_scan_lr_functions` and the
   precedence-climbing dispatcher.

For the long pole — full ANTLR4 LL(\*) — a runtime ATN simulator
is the correct answer; the static repair is a stopgap that closes
the highest-value cases first.

For oracle integration when measuring against ANTLR4's behaviour,
the JVM `tool/` is vendored. A Stage B' could shell out to it for
descriptors whose `[output]` is action-printed (the
`FullContextParsing/*` category, etc.) so they stop being
auto-skipped by `normalize_output_for_stage_b` in the descriptor
extractor.
