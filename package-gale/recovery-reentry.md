# Recovery Re-entry — Design (grammar-agnostic fragment structure)

Status: design. Not implemented. Opt-in; off is byte-identical.

## Problem

The resilient parser highlights and structures _broken_ input, but only what
the grammar's productions can reach. A start rule like

```antlr
sourceFile : innerAttribute* item* EOF ;
```

derives no top-level statement or expression, so a bare snippet —

```wado
let s = `hi ${name}`;
```

— matches zero `item`s, then `expect(TK_EOF, EMPTY_SYNC)` sees `let` with an
empty sync set and **unwinds**: every token is left unconsumed, so nothing
enters the CST. The highlighter (which always walks the partial tree and then
default-classifies the rest, see `resilient-parser.md`) still colors keywords
and strings by token kind, but the `interpolation` node is never built, so the
`${name}` identifier is not recognized as a `variable` and a `${x:.2}` format
spec is not muted. Recovery _syncs / skips / sweeps_; it never _invents_ a
production, so no parser/highlight change alone reaches nested-only constructs
in a bare fragment.

## Goal / non-goals

Goal: when the parser cannot consume a run of tokens, still build the real
subtrees for the constructs those tokens _do_ form, so tooling (highlight, LSP
outline, selection) sees genuine structure — **without** a grammar edit and
**without** backtracking.

Non-goals: making broken input _parse cleanly_ (diagnostics still fire);
changing any clean-parse output (opt-in, byte-identical when off); inventing
tree shapes the grammar can't express.

## Constraints (Gale invariants)

- **No backtracking, ever.** Re-entry must pick a rule by static lookahead and
  commit — never try-fail-rewind (`CLAUDE.md`, "Standing codegen rules").
- **Infallible rules.** A called rule recovers internally; it never returns
  `Result`. Re-entry composes with that, it does not bypass it.
- **Over-broad repair is a known trap.** The failed LL(\*) variants (2026-05)
  silently broke real grammars by guessing when a rule should re-enter. Any
  re-entry must be _deterministic_ (no guess) and paired with **rejection-case**
  fixtures, not just hit-case ones.

## Design: two tiers

Re-entry is a _dispatch_, not a search: at a stuck position, consult a
generator-built table keyed by the current token; if it names a rule, call that
rule's `_parse_<rule>`; else skip one token. Determinism comes from only
listing tokens whose interpretation is unambiguous.

### Tier 1 — unique-trigger island descent (recommended)

A terminal `t` is a **unique trigger** for rule `R` iff `t ∈ FIRST(R)` as a
_hard_ (consumed, non-nullable) first token and no other rule has `t` in its
FIRST. The generator emits

```
RECOVER_DISPATCH: token_kind -> rule_id   // only unique triggers
```

During any recovery sweep, at each position: if `kinds[pos]` is in
`RECOVER_DISPATCH`, call the mapped `_parse_<rule>(p)` to build that subtree in
place; otherwise skip the one token into the surrounding `K_ERROR` region.

Why this is safe: a unique trigger has exactly one grammatical meaning, so
descending is not a guess — it is the only production that terminal can begin.
For Wado this captures precisely the context-island rules highlighting needs:

| Trigger token                                | Rule descended into | Highlight win                       |
| -------------------------------------------- | ------------------- | ----------------------------------- |
| `INTERP_OPEN` (`${`)                         | `interpolation`     | `${name}` → `variable`, `:.2` muted |
| `BACKTICK` (`` ` ``)                         | `templateString`    | whole template + its interps        |
| distinctive keyword/punct unique to one rule | that rule           | its subtree                         |

It intentionally does **not** fire on shared triggers (`IDENTIFIER` starts many
rules; `if` starts both `ifStatement` and `ifExpr`), so it never mis-structures.
The bare `let s = ...` snippet above highlights fully: a `keyword` class on
`let`, defaults for `s = … ;`, and a real `templateString` → `interpolation`
island around the backtick run.

### Tier 2 — anchor re-entry (optional, higher reach / higher risk)

To structure a bare fragment as _statements_ (not just islands), re-enter at
the grammar's **repeated-constituent anchors**: rules `A` that appear in a
`*`/`+` loop in some rule (`item` in `item*`, `statement` in a block's
`statement*`). At a stuck position, dispatch among anchors whose FIRST contains
the current token. Shared-FIRST collisions (e.g. `statement` vs `item` both on
`IDENTIFIER`) are resolved by the **existing ATN simulator** (`atn.wado`) —
the same full-context prediction the normal parser uses — never by a new
try-fail loop. This effectively auto-derives a `statement* | item*` fragment
alternative from the grammar's own loop structure.

Tier 2 is strictly opt-in beyond Tier 1 because "which anchor at top level"
is a policy the grammar author normally encodes; auto-deriving it is exactly
the class of decision the 2026-05 notes warn about. It is **not required for
correct highlighting** (Tier 1 already recovers every context island). Ship it
only if a consumer needs full fragment structure (e.g. an outline of a snippet),
and gate it separately.

## Integration points (exact)

1. **Top-level remainder sweep** — `_run_parse_entry` (`parser_gen.wado:467`),
   after `entry(&mut p)`: if `p.pos` is not at `TK_EOF`, open a `K_ERROR`
   region and sweep the remainder to EOF using the dispatch below, then close
   it. This is where the unconsumed bare-fragment tail is captured. The region
   is a top-level sibling after the start rule's node (the start rule already
   closed); flat scans (highlight) see it, and consumers that iterate top-level
   rows see it. (A single-root variant would require the start rule to sweep
   before its `finish_node`; deferred — the sibling form needs no start-rule
   change and suits tooling.)

2. **Existing error-region sweep** — `expect`'s `!sync.is_empty()` branch
   (`parser_gen.wado:1150`): replace the bare `advance/skip` in the loop with
   the shared descent step, so mid-parse error regions also grow real islands.

Both call one emitted helper:

```
// gated on the `recover_islands` option; absent otherwise (byte-identical)
fn recover_step(p: &mut Parser) -> bool {          // true = made progress
    let t = p.kinds[p.pos];
    match recover_dispatch(t) {                     // static match, unique triggers
        Some(rule_id) => {
            let before = p.pos;
            let was = p.recovering; p.recovering = false;
            call_rule(p, rule_id);                   // match rule_id { RK_X => _parse_x(p), ... }
            p.recovering = was;
            if p.pos == before { skip_one(p); }      // progress guard
        }
        None => skip_one(p),
    }
    return true;
}
```

## Safety, termination, cost

- **Progress guaranteed.** A unique trigger is a hard first token, so the
  descended rule consumes ≥1 token; the `pos == before` guard skips one on the
  (defensive) zero-consume case. Every sweep iteration advances `pos`, so a
  sweep is O(remaining tokens); total parse stays O(n).
- **No backtracking.** Dispatch is a table lookup / static match; nested error
  regions inside an island are handled by ordinary rule recovery.
- **`speculating` / `recovering`.** Descent runs only in real recovery
  (`!p.speculating`). It clears `recovering` around each island so the called
  rule actually parses, then restores it so the sweep continues.
- **Diagnostics.** The sweep emits at most one region diagnostic
  (`UnparsedInput`), respecting `max_errors`; island rules add their own,
  capped the same way.

## Interaction with the shipped highlight walk

Complementary. Tier 1 puts more _structure_ in the tree (islands); the
already-shipped `highlight_walk` + `hl_cover_unvisited` then picks up the island
rule-context overrides automatically and default-classifies the flat-skipped
remainder. Text still round-trips. No change to `highlight_gen`/`highlight.wado`
is needed for B — B only changes what the tree contains.

## Gating

New generator option `recover_islands: bool` (default `false`), independent of
`highlight` so an LSP can enable it without HTML. Every emit site is gated;
with it off, the parser is byte-identical to today. `gale-highlight-*` packages
turn it on.

## TDD plan

Hit-case (driver tests, `recover_islands` on):

- Bare `let s = \`hi ${name}\`;` → tree contains `templateString`/`interpolation`;
  highlight marks `${name}`→`variable`,`${x:.2}` muted; text round-trips.
- Mid-parse error region (unterminated call) grows a template island.

Rejection-case (mandatory, per the 2026-05 notes):

- Every existing driver grammar (JSON, SQLite, calc, LR, …) parses to the **same
  tree with the option on** for clean input — dispatch never fires on a clean
  parse (there is no stuck position). Assert tree equality on/off.
- A grammar where a unique-trigger token also appears legitimately: confirm the
  clean path never reaches `recover_step`, and a _broken_ input near it makes
  bounded progress (no runaway, one region).

## Phasing

1. Emit `RECOVER_DISPATCH` + `recover_step` + `call_rule` match, gated
   (unique-trigger set from existing FIRST analysis). Wire the top-level sweep
   and the `expect` sweep. Hit + rejection fixtures. — Tier 1.
2. (Optional) Anchor set + ATN-resolved dispatch for full fragment structure. —
   Tier 2, separate gate, its own rejection corpus.

## Recommendation

Tier 1 is sufficient for correct fragment _highlighting_ and is low-risk
(deterministic, unique-trigger only, opt-in). Tier 2 is a separate, riskier
capability for full fragment structure; pursue only on a concrete consumer need.
