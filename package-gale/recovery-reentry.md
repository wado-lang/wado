# Recovery Re-entry — Design (grammar-agnostic fragment structure)

Status: Tier 1 implemented (the `recover_islands` generator option). Tier 2
deferred. Opt-in; off is byte-identical.

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

### Tier 1 — delimiter-trigger island descent (implemented)

A terminal `t` is a **delimiter trigger** for rule `R` iff `t` occurs in exactly
one parser rule's body across the whole grammar (`R`'s), and there it is a
_direct_ leading terminal (`R`'s own leading token, not one inherited by
expanding a `RuleRef`). The generator emits, as gated free helpers,
`_recover_descend` (an `if`-chain over the trigger token constants → the sole
`_parse_<rule>(p)`) and `_recover_try_descend` (the recovering-flag dance + a
progress guard, falling back to skipping one token).

Two refinements were forced by real false positives and are load-bearing:

- **Direct, not FIRST.** `BACKTICK` is in FIRST of every rule that can derive a
  template (`literal`, `primary`, `expression`, …), so plain FIRST-uniqueness
  never fires for it. Only `templateString` has it as a _direct_ leading token.
- **Exclusive to one rule, not merely direct-unique.** `=` is a direct leading
  terminal of the `forTail` _continuation_ rule (its `(':' typeRef)? '='` alt),
  but is used pervasively elsewhere; descending into `forTail` on a stray `=`
  ran off the input. Requiring `t` to occur in exactly one rule keeps genuine
  delimiters (a template backtick, `${`) — tokens the lexer only ever emits at
  their one construct — and drops common tokens.

During a recovery sweep, at each position: if `kinds[pos]` is a delimiter
trigger, call the mapped `_parse_<rule>(p)` to build that subtree in place;
otherwise skip the one token into the surrounding `K_ERROR` region.

Why this is safe: a delimiter trigger has exactly one grammatical meaning, so
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

## Integration points (as built)

The sweep hooks the start rule's own `expect(TK_EOF, …)` rather than running
after the entry closes the tree — so the `<error>` region nests **under the
root**, where `to_string_tree`, LSP, and highlight all see it (an after-the-fact
sweep in `_run_parse_entry` would leave it a top-level sibling the root's
subtree excludes).

- **`expect` (`parser_gen.wado`), gated on `recover_islands`.** After the
  speculative-parse guard, when `kind == TK_EOF` and the current token is not
  EOF: report one `UnexpectedToken`, open a `K_ERROR` region, loop
  `_recover_try_descend(self)` until EOF, close the region, and match EOF. Since
  `expect(EOF)` is called from inside the (still-open) start rule, the region
  lands under the root. Off, this whole block is not emitted — byte-identical.

The helpers (gated, emitted by `gen_recover_helpers`):

```
fn _recover_descend(p: &mut Parser) -> bool {       // if-chain over trigger constants
    let _k = p.kinds[p.pos];
    if _k == TK_BACKTICK { _parse_template_string(p); return true; }
    if _k == TK_INTERP_OPEN { _parse_interpolation(p); return true; }
    // …one arm per delimiter trigger…
    return false;
}
fn _recover_try_descend(p: &mut Parser) {
    let before = p.pos; let was = p.recovering; p.recovering = false;
    let did = _recover_descend(p);
    p.recovering = was;
    if !did || p.pos == before {                    // progress guard
        let sk = p.advance(); p.tokens.mark_skipped(sk); p.b.skip(sk);
    }
}
```

The dispatch table (delimiter-trigger → `_parse_<rule>` call) is computed at
generation time by `compute_recover_dispatch` from the exclusive-terminal +
direct-first analysis above.

Mid-parse error-region descent (extending `expect`'s `!sync.is_empty()` sweep)
is a natural follow-up but was **not** needed for the highlight goal — a
partially-parsed construct already builds its own subtree — so it is left out to
keep the change minimal.

## Safety, termination, cost

- **Progress guaranteed.** A delimiter trigger is a direct leading terminal, so
  the descended rule consumes ≥1 token; the `pos == before` guard skips one on
  the (defensive) zero-consume case. Every sweep iteration advances `pos`, so a
  sweep is O(remaining tokens); total parse stays O(n).
- **No backtracking.** Dispatch is a static `if`-chain; nested error regions
  inside an island are handled by ordinary rule recovery.
- **`speculating` / `recovering`.** The hook sits after `expect`'s speculative
  guard, so it never fires during a probe. It clears `recovering` around each
  island so the called rule actually parses, then restores it.
- **Diagnostics.** The sweep reports one `UnexpectedToken` for the unparsed
  input; island rules add their own (capped by `max_errors`).

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

## TDD (as landed)

Hit-case — `package-gale-highlight-wado/src/lib_test.wado` (`recover_islands`
on): a bare `let s = \`hi ${name} ${name:0.2}\`;` fragment highlights
`${name}`→`variable`and`0.2` muted, and round-trips; plus truncated / nested /
junk-surrounded template probes stay text-preserving and bounded.

Hit + rejection — `package-gale/tests/driver_cst_recover_islands_test.wado`, a
foreign grammar whose start rule only derives `set`-led items with a `[`-
delimited `group`:

- **Rejection (the 2026-05 mandate):** the grammar is generated twice (option
  on / off); clean inputs parse to the **same** tree — dispatch never fires
  without a stuck position.
- **Hit:** the fragment `[ x ]` yields `(prog (<error> (group [ x ])))` under
  the root with the option on, and just `prog` with it off; trailing junk stays
  bounded and still builds the island.

## Phasing

1. **Tier 1 (done):** `compute_recover_dispatch` (delimiter-trigger set from the
   exclusive-terminal + direct-first analysis) + gated `_recover_descend` /
   `_recover_try_descend`, wired into the `expect(EOF)` hook. Hit + rejection
   fixtures. Enabled in `gale-highlight-wado`.
2. **Tier 2 (deferred):** anchor set + ATN-resolved dispatch for full fragment
   structure (parse a bare `let …` as a statement, not just its islands).
   Separate gate, its own rejection corpus. Reuses all of Tier 1's plumbing;
   only the dispatch decision changes.

## Recommendation

Tier 1 is sufficient for correct fragment _highlighting_ and is low-risk
(deterministic, delimiter-trigger only, opt-in). Tier 2 is a separate, riskier
capability for full fragment structure; pursue only on a concrete consumer need.
