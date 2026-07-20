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

### Tier 2 — fragment-entry re-entry (design)

Goal: structure a bare fragment as a **sequence of statement/item units**, not
just delimiter islands. `let x = 1; foo(x); if y { g() }` at top level should
yield three real `statement` subtrees, not skipped tokens with a couple of
islands. Not required for correct highlighting (Tier 1 already recovers every
context island); it pays off for consumers that want full fragment structure —
an LSP outline of a snippet, selection/folding, a formatter over a partial file.

#### Mechanism: reuse Tier 1's sweep, richer step

Tier 2 reuses Tier 1's integration verbatim — the `expect(EOF)` hook, the
`<error>` region under the root, the progress-guarded loop. Only the per-position
step changes, layering three strategies (widest structure first):

1. **Fragment-entry parse** — if the current token can begin a configured
   fragment-entry rule, parse that whole rule (a statement / item), building its
   full subtree.
2. **Delimiter island (Tier 1)** — else if it is a delimiter trigger, descend
   into that one rule.
3. **Skip one** — else skip a token into the region.

Nothing from Tier 1 is discarded: it is the inner fallback for a token no entry
rule accepts. The three strategies share the one progress guard.

#### Entry-rule set: explicit config (recommended)

A new option `fragment_entry: List<String>` names the rule(s) a fragment is a
sequence of — for a statement language, usually just `["statement"]`. It is set
at the `use … with { generator: { options } }` site, the same surface that
already sets `recover_islands`, so it needs **no `.g4` edit** (reading the
grammar to name its statement rule is not editing it — this honors the
no-grammar-edit constraint). `gale-highlight-wado` would set `["statement"]`.
Unknown names warn and are skipped, as the highlight query does.

**Single entry is the common, safest case and needs no dispatch at all:**
`fragment_entry: ["statement"]` → the sweep loops `_parse_statement(p)` until
EOF. Fully deterministic. For Wado this already covers bare statements _and_
bare expressions (a bare `foo(x) + 1` parses as an `exprStatement` whose missing
`;` is inserted by the rule's own recovery), and it subsumes Tier 1's template
island because a backtick is in `FIRST(statement)`. So Wado's whole fragment
story is one configured entry rule.

Why explicit and not auto-derived: "which rule is a top-level unit" is a policy
the grammar author encodes. Auto-guessing it is exactly the class of decision
the 2026-05 LL(\*) notes flag — and a naive "repetition-body rule reachable from
the start rule" set also sweeps up inner list-elements (`fieldDecl`, `param`,
`enumCase`) that are not top-level units.

#### Multiple entries → reuse the parser's own prediction (extension)

When >1 entry is configured (a grammar with genuinely distinct top-level units),
choosing among them at a token is the same decision the parser already makes for
an alternation. Do not hand-roll it. Two viable realizations:

- **Synthetic dispatch rule.** Inject `__fragment_unit : e1 | e2 | … ;`, emit it
  through the normal pipeline, and call `_parse___fragment_unit(p)`; alternative
  prediction (static k-lookahead → ATN escalation, exactly as any alternation)
  picks the entry, no new algorithm and no backtracking. **Caveat:** injecting a
  rule into the shared grammar perturbs FIRST/FOLLOW/ATN of the referenced rules
  (`__fragment_unit` contributes EOF to `FOLLOW(statement)`), which could change
  a FOLLOW-gated rule's _clean_-parse output. So the synthetic rule must be
  lowered/predicted in **isolation** (its own analysis pass, not folded into the
  real grammar's FOLLOW sets) — the load-bearing design constraint here.
- **FIRST dispatch + narrow ATN.** Precompute `FIRST(e_i)`; a token unique to one
  entry dispatches directly; only a genuine collision consults an isolated ATN
  decision built for just those entries. More code, zero perturbation.

Recommend shipping single-entry first (no dispatch, no perturbation risk) and
treating multi-entry as a follow-up with the isolation constraint above.

#### Why the 2026-05 trap does not apply

Those failures changed the **clean-parse** code path — static per-rule variants
baked into normal rules — so an over-broad guard silently mis-parsed valid
input. Tier 2's fragment path runs **only at a stuck `expect(EOF)`** on
already-broken input, never on a clean parse; the rejection corpus proves clean
trees are byte-identical on/off. A mis-picked entry can only mis-structure an
already-invalid fragment (cosmetic for highlighting), never a valid program.
Opt-in **plus** recovery-only confines the blast radius — the crucial difference
from baking variants into the normal path.

#### Termination, tree, diagnostics

- Each entry parse matches its FIRST token first → consumes ≥1; the progress
  guard skips one on the defensive zero-consume case. O(n) overall.
- Fragment units nest inside the one `<error>` region under the root (they are
  recovered, not grammatical children of the start rule); their `statement`
  subtrees are real nodes a walker sees.
- One `UnexpectedToken` for the fragment; each unit adds its own recovery
  diagnostics, capped by `max_errors`.

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
2. **Tier 2a — single fragment entry (designed):** add `fragment_entry:
   List<String>`; when it names one rule, the sweep step tries
   `_parse_<entry>(p)` before the Tier 1 descend. No dispatch, no synthetic
   rule, no analysis perturbation. Covers Wado (`["statement"]`). Reuses all of
   Tier 1's plumbing — only the sweep step gains a first strategy.
3. **Tier 2b — multiple entries (designed, follow-up):** synthetic
   `__fragment_unit` alternation lowered/predicted **in isolation** (or FIRST
   dispatch + a narrow isolated ATN), so clean-parse FIRST/FOLLOW is untouched.

Tier 2's TDD mirrors Tier 1's: a hit fixture (`let x = 1; foo(x); if y { g() }`
→ nested `statement` subtrees under the `<error>` region) and the mandatory
rejection fixture (grammar generated on/off, incl. `fragment_entry` set → clean
inputs byte-identical, since the fragment path never runs on a clean parse),
plus an interaction case (a token no entry accepts still gets a Tier 1 island).

## Recommendation

Tier 1 is sufficient for correct fragment _highlighting_ and is low-risk
(deterministic, delimiter-trigger only, opt-in). Tier 2a (single explicit entry)
is the natural next step for full fragment structure — safe (no dispatch, no
perturbation, recovery-only) and enough for a statement language like Wado.
Tier 2b (multiple entries) is worthwhile only for a grammar with several
distinct top-level units, and must lower the dispatch in isolation to keep the
clean parse byte-identical.
