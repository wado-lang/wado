# Recovery Re-entry — fragment-entry rules

Status: implemented (the `fragment_entry` generator option). Opt-in; empty is
byte-identical.

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

## Design: name the unit rules

The fragment a snippet forms is a _sequence of units_ — statements, or
top-level items. Which rule is "a unit" is a policy the grammar author encodes,
so the feature takes it as configuration rather than guessing it:

**`fragment_entry`** — a generator option naming the rule(s) a fragment is a
sequence of. For a statement language it is usually just `"statement"`. Set at
the `use … with { generator: { options } }` site, the same surface that carries
`trace` etc., so it needs **no `.g4` edit** (reading the grammar to name its
statement rule is not editing it — this honors the no-grammar-edit constraint).
`gale-highlight-wado` sets `fragment_entry: "statement"`. Kiln options are
scalars, so several rules are given comma-separated (`"statement,item"`);
unknown names warn (`DiagnosticKind::FragmentEntry`) and are skipped.

When the start rule's `expect(EOF)` is reached with input it could not derive,
the sweep parses each entry rule whose FIRST set holds the current token,
building its full subtree; a token no entry accepts is skipped one at a time.

**A single entry covers Wado.** `fragment_entry: "statement"` handles bare
statements _and_ bare expressions (a bare `foo(x) + 1` parses as an
`exprStatement` whose missing `;` is inserted by the rule's own recovery), and
because `statement → exprStatement → expression` reaches template strings and
block expressions, one entry also recovers `` `${name}` `` interpolations and
`{ … }` bodies. So Wado's whole fragment story is one configured rule.

### Why not auto-derive the entry set

A tempting zero-config variant derives the entries — repetition-body rules
reachable from the start rule (`item` in `item*`, `statement` in a block's
`statement*`). But that also sweeps up inner list-elements (`fieldDecl`,
`param`, `enumCase`) that are not top-level units, and choosing among candidates
that share a FIRST token re-introduces the guess the 2026-05 notes flag. Naming
the entries keeps the decision with the author, where it belongs.

### An earlier iteration: delimiter islands (superseded)

The first cut descended, with no configuration, into a rule whenever the current
token was a **delimiter** exclusive to it (a token occurring in exactly one
rule's body where it leads that rule — a template backtick, `${`). It recovered
template islands for free but nothing statement-led (`let …`), the common case.
`fragment_entry: "statement"` subsumes it — a backtick is in `FIRST(statement)`,
so the whole statement (interpolation and all) is recovered, more richly than a
bare `templateString` island — so the delimiter machinery was dropped in favor
of the single configured mechanism.

### Multiple entries

With several entries, the current dispatch is **declaration-order priority**:
the first listed entry whose FIRST holds the token wins (deterministic, no
backtracking). That is exact when the entries have disjoint FIRST sets, and a
sensible fallback order when they overlap (list the broader unit first). A
grammar that needs true prediction among overlapping entries would reuse the
parser's own alternation prediction — via a synthetic `e1 | e2 | …` rule
lowered/predicted **in isolation** so injecting it never perturbs the real
grammar's FIRST/FOLLOW (and thus a clean parse). Not needed for a statement
language; left as a follow-up.

## Integration points (as built)

The sweep hooks the start rule's own `expect(TK_EOF, …)` rather than running
after the entry closes the tree — so the `<error>` region nests **under the
root**, where `to_string_tree`, LSP, and highlight all see it.

- **`expect` (`parser_gen.wado`), gated on a non-empty `fragment_entry`.** After
  the speculative-parse guard, when `kind == TK_EOF` and the current token is
  not EOF: report one `UnexpectedToken`, open a `K_ERROR` region, loop
  `_recover_try_descend(self)` until EOF, close the region, and match EOF. Since
  `expect(EOF)` runs inside the still-open start rule, the region lands under the
  root. Empty `fragment_entry` ⇒ this block is not emitted (byte-identical).

The helpers (gated, emitted by `gen_recover_helpers`):

```
global _FRAGMENT_FIRST_0: List<i32> = [TK_LIT_LET, TK_LIT_IF, …];  // FIRST(entry 0)
fn _recover_fragment(p: &mut Parser) -> bool {
    let _k = p.kinds[p.pos];
    if p.in_set(_k, &_FRAGMENT_FIRST_0) { _parse_statement(p); return true; }
    // …one arm per entry, in declaration order…
    return false;
}
fn _recover_try_descend(p: &mut Parser) {
    let before = p.pos; let was = p.recovering; p.recovering = false;
    let did = _recover_fragment(p);
    p.recovering = was;
    if !did || p.pos == before {                    // progress guard
        let sk = p.advance(); p.tokens.mark_skipped(sk); p.b.skip(sk);
    }
}
```

`compute_fragment_entries` resolves the configured names to their
`_parse_<rule>` call and FIRST set at generation time.

## Why the 2026-05 trap does not apply

Those failures changed the **clean-parse** code path — static per-rule variants
baked into normal rules — so an over-broad guard silently mis-parsed valid
input. The fragment path runs **only at a stuck `expect(EOF)`** on already-broken
input, never on a clean parse; the rejection corpus proves clean trees are
byte-identical on/off. A mis-picked entry can only mis-structure an
already-invalid fragment (cosmetic for highlighting), never a valid program.
Opt-in **plus** recovery-only confines the blast radius — the crucial difference
from baking variants into the normal path.

## Termination, tree, diagnostics

- Each entry parse matches its FIRST token first → consumes ≥1; the progress
  guard skips one on the defensive zero-consume case. O(n) overall.
- Fragment units nest inside the one `<error>` region under the root (they are
  recovered, not grammatical children of the start rule); their `statement`
  subtrees are real nodes a walker sees.
- One `UnexpectedToken` for the fragment; each unit adds its own recovery
  diagnostics, capped by `max_errors`.

## Interaction with the highlight walk

Complementary. The fragment entries put full structure in the tree; the
already-shipped `highlight_walk` + `hl_cover_unvisited` then picks up the entry
subtrees' rule-context overrides automatically and default-classifies any
skipped remainder. Text still round-trips. No change to
`highlight_gen`/`highlight.wado` is needed — this only changes what the tree
contains.

## TDD

Hit-case — `package-gale-highlight-wado/src/lib_test.wado` (`fragment_entry:
"statement"`): a bare `let s = \`hi ${name} ${name:0.2}\`;` fragment highlights
`${name}`→`variable`and`0.2` muted, and round-trips; truncated / nested /
junk-surrounded template probes stay text-preserving and bounded.

Hit + rejection — `package-gale/tests/driver_cst_fragment_entry_test.wado`, a
foreign grammar whose start rule derives only `set`-items and `main` blocks with
an inner `stmt`:

- **Rejection (the 2026-05 mandate):** generated twice (on `stmt` / off); clean
  inputs parse to the **same** tree — the sweep never fires without a stuck
  position.
- **Hit:** `do x ; do y ;` → `(prog (<error> (stmt do x ;) (stmt do y ;)))`
  under the root with the option on, and just `prog` with it off; a token no
  entry accepts is skipped and the sweep stays bounded.

## Follow-ups

- Multiple entries with overlapping FIRST via isolated alternation prediction
  (above), if a grammar needs it.
- Auto-derived entry set with caveats, if a zero-config consumer appears.
