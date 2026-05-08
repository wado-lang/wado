# Gale TODO

Known gaps to close on the road to **full ANTLR4 compatibility**. The
compatibility principle is documented in [AGENTS.md](./AGENTS.md): the
g4 parser must **accept** any well-formed grammar that upstream `antlr4`
accepts. Today's intentional carve-out is at the _syntax-recognition_
layer — the parser sees `{ ... }` action bodies and `{ ... }?` semantic
predicates as opaque text and does not parse the host-language source
inside them. That is fine for getting grammars _loaded_; it is not fine
for getting them _executed correctly_ by the generated lexer/parser.
Reproducing the semantics of those bodies (so a Gale-generated parser
matches what `antlr4` would do at runtime) is open work — see the
**Correctness** section below.

The g4 parser already accepts the full ANTLR4 surface syntax under that
recognition rule. The remaining work splits into:

- **Propagating** parsed information into the IR and **using** it in
  the code generator so generated parsers are semantically correct, not
  just syntactically accepted.
- **Executing** action bodies and semantic predicates so grammars that
  rely on them (Rust, TypeScript) tokenize/parse correctly.

## Generated Parser Bugs

(none currently)

## Correctness: full-context LL(\*) prediction (in progress)

Gale's prediction is essentially SLL — it picks an alt by inspecting
the current and (for some sites) one or two lookahead tokens, then
commits. ANTLR4's prediction is LL(\*): when SLL is ambiguous it
considers the full call-stack context, including what the caller
expects to follow. Real-world `.g4` files rely on this in subtle
places, and grammars that hit those cases tokenize the same input but
choose a different alt than ANTLR4 would.

Stage C (action/predicate execution) sits behind LL because the harder
LL cases in real grammars depend on predicates as a tiebreaker, so the
prediction story has to be settled first.

### What works (v1, 2026-05)

One-level static-analysis LL repair via `__follow_<id>` variants. At a
`RuleRef R` call site whose immediate next sibling is **strictly
required** (suffix not deep-nullable), Gale computes
`FOLLOW(call) = first(suffix)`; if `tail_greedy_first(R) ∩ FOLLOW`
is non-empty, an `intern_follow_variant`-registered variant
`scan_R__follow_<id>` / `parse_R__follow_<id>` is generated that
subtracts the mask from tail-position Optional/Star/Plus first-sets.
The minimal reproducer `(a b | a) EOF; a : X Y?; b : Y` parses
correctly: `tests/grammars/ll_basic.g4` plus
`tests/antlr4-compat/.../ParserExec/PredictionMode_LL` are green.

Implementation references:

- `package-gale/src/gen_context.wado` — `tail_greedy_first_of_rule`,
  `intern_follow_variant`, `FollowVariantEntry`.
- `package-gale/src/parser_gen.wado` — `gen_scan_follow_variant`,
  `gen_parse_follow_variant`, the `current_follow_mask` /
  `ruleref_call_follow` threading, and
  `sort_group_by_mandatory_count_desc` for LL-correct group dispatch.

### What does not work yet (v1 limitations)

Each gap has a regression grammar under `tests/grammars/ll_*.g4` plus
a `#[TODO]` driver test. Closing a gap means making the test pass
without breaking the others.

| Gap                           | Fixture                 | Why v1 punts                                                                                                                                                                                                                           |
| ----------------------------- | ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Nullable next sibling         | `ll_nullable_suffix.g4` | `gen_alt_elements` skips variant registration when `is_suffix_nullable`. Closing requires CTX_FOLLOW propagation through the outer rule's follow.                                                                                      |
| Multi-alt callee              | `ll_multi_alt.g4`       | `intern_follow_variant` rejects multi-alt rules. `gen_parse_follow_variant` only emits single-alt `gen_alt_body`; needs the multi-alt body / bt-helper duplication.                                                                    |
| Left-recursive callee         | `ll_lr_atom.g4`         | `intern_follow_variant` rejects LR rules. Needs `scan_R__follow_<id>_atom` + per-LR-alt suffix helpers + a precedence-climbing variant dispatcher.                                                                                     |
| Caller-of-caller follow       | `ll_ctx_follow.g4`      | `ruleref_call_follow` is computed from the immediate alt's siblings only. Needs a per-rule FOLLOW fixed point that propagates through nullable / passthrough rules.                                                                    |
| Multi-token tail-greedy inner | (no fixture yet)        | `tail_greedy_first_of_element` only counts Repeats whose inner is a single token (TokenRef / Literal / Wildcard / Not / single-token RuleRef). Needed for `(A B)?` style tail-greedy patterns where the inner sequence is multi-token. |

### Beyond v1: ANTLR4 oracle integration

Static analysis can only get LL so far. For full ANTLR4 LL(\*)
behaviour we eventually need a runtime ATN simulator (the closure /
predict / DFA cache loop in
`vendor/antlr4/runtime/Java/src/org/antlr/v4/runtime/atn/ParserATNSimulator.java`),
not just static FOLLOW sets. Sequencing this is open: the static
gaps above each unblock specific real-world grammars; an ATN
simulator is the long pole if those don't suffice.

For measurement, the `vendor/antlr4` submodule has the JVM tool plus
`runtime-testsuite/`. A future Stage B' could shell out to ANTLR4's
JVM `tool` to compute oracle parse trees for descriptors whose
`[output]` is action-printed (FullContextParsing/\*, etc.) and would
otherwise be auto-skipped by `normalize_output_for_stage_b`.

## Correctness: full ANTLR4 compatibility (action / predicate execution)

Gale currently **recognizes** but **silently discards** the contents of
`{ ... }` action blocks and `{ ... }?` semantic predicates. The g4 parser
accepts them, so grammars that contain them (`ANTLRv4Lexer`, `RustLexer`,
`RustParser`, `TypeScriptLexer`, `TypeScriptParser`) load cleanly — but
the generated lexer/parser behaves as if every predicate were `true` and
every action were a no-op. That is wrong for:

- `RustLexer.RAW_STRING_LITERAL` (the closing `#` count must match the
  opening `#` count — a predicate enforces this; without it Gale
  mistokenizes Rust raw strings).
- TypeScript's regex-vs-division disambiguation and other context-
  sensitive lexer rules (3 predicates) and parser rules (17).

This is a hard prerequisite for several things:

- Treating Gale as a drop-in ANTLR4 replacement (the stated principle in
  [AGENTS.md](./AGENTS.md)).
- Any lexer-level optimization work — claiming a tokenizer is fast is
  meaningless if it tokenizes incorrectly.
- `Grammar.options.superClass` and `tokenVocab`, which become wireable
  once action bodies are real.

Sketch:

- Extend the IR so `OptionValue::Action` and per-alt action / predicate
  elements carry a language-tagged source fragment instead of being a
  placeholder string.
- Add a pluggable "action translator" interface; ship at minimum an
  identity translator for Wado-written action bodies (so Wado-authored
  grammars work natively).
- Generate a `SuperClass` trait (name derived from `superClass = Foo`)
  and require callers to `impl` it; emit action bodies as method calls
  on `self` that resolve through that trait.
- `tokenVocab` falls out at that point — another grammar's generated
  `TokenKind` enum can be imported by name rather than merged at IR
  time.

Translating Java/Rust/Python action bodies to Wado is the obvious hard
part. A reasonable first cut is to require callers to provide hand-
written Wado equivalents for non-Wado grammars (a sidecar file mapping
predicate / action ID → Wado snippet), with a migration path to
automatic translation later.

## Performance: where 5× actually lives

Investigation against `benchmark/sqlite_parse` (Wado/Gale at `-O2`,
~137 ms/iter on a 13 KB SQL fixture; Rust `sqlparser-rs` at debug for
reference is ~6.7 ms/iter — release would be far less).

Profile (guest sampler, 5 ms interval) self-time top:

|   Pct | Symbol                                                    |
| ----: | --------------------------------------------------------- |
| 27.9% | `tokenize`                                                |
| 26.0% | `Array<Token>::push` (per-token `struct.new Token`)       |
| 17.2% | `Parser::last_end` (4-step `Parser→Array→Token→Span→end`) |
|  4.4% | `Array<Token>::grow`                                      |

Combined: token-stream construction (`tokenize` + `Array<Token>::push` +
`grow`) is 58% of self-time. Token reads via `Parser` are next.

### What does not work

- **Inlining hot Parser methods.** `Parser::last_end` accounted for 17%
  self-time; both caching it as a field and forcing `#[inline]`
  eliminate the named function from the profile but do not move wall
  time. The cost was the actual loads (`Parser→Array→Token→Span→end`),
  not call overhead — inlining merely redistributes it into the
  callers (`parse_expr`, `Parser::expect`, …). wasmtime + Cranelift
  handles small Wasm function calls cheaply enough that hunting for
  inlinability is not a productive lever here.
- **Any micro-optimization on individual Parser methods.** Same reason:
  the bytes loaded are unchanged, so the work is unchanged.

### What would actually move the needle

The dominant cost is **Wasm GC `(array (ref Token))` indirection plus
per-token `struct.new Token` allocation**. A 5× improvement requires
decomposing `Array<Token>` into parallel primitive arrays
(`kinds`/`starts`/`ends` as `Array<i32>`, packed in Wasm GC) so that:

- `peek_kind` / `tokens[i].kind` becomes a single `array.get i32`
  instead of `array.get (ref Token)` + `struct.get`.
- Per-token struct allocation disappears in the lex loop.

Two non-overlapping paths to get there:

1. **Gale-side**: redesign `Token` so the hot fields are flat
   primitives, with an opaque sidecar (or removal) for `text` /
   `leading_trivia`. Keep the public `Token` API as a view handle if
   needed for compatibility.
2. **Wado-side**: extend `container_sroa` to handle (a) struct fields
   (currently locals only — see `wado-compiler/src/optimize/container_sroa.rs`
   "Future directions"), (b) inner structs with nested struct or
   reference fields, (c) cross-function rewrites for the
   `scan_*(&Array<Token>, ...)` parameter pattern (1100+ sites in the
   SQLite parser pass `&p.tokens` as a bare reference, currently always
   escaping). Today the pass fires on zero candidates in Gale-generated
   parsers.

### Lexer dispatch (independent secondary lever)

Inside the 27.9% `tokenize` self-time, the work splits roughly into
per-character branch dispatch and keyword classification. Several
techniques can replace the current hand-rolled cascade — pick by what
profiling on the predicate-correct lexer says is hottest:

- **Table-driven DFA** for the whole lexer (NFA → DFA subset
  construction → state-transition table). Replaces both per-character
  dispatch and `classify_keyword`. ANTLR4 `mode` blocks become a DFA
  per mode plus mode-switch on accept; lexer commands (`skip`, `more`,
  `type(N)`, `channel(HIDDEN)`) attach as accept-state attributes.
  Semantic predicates are the only DFA-blocker — the resolution lives
  in the **Correctness** section above; once predicates are real,
  predicate-bearing rules need a hybrid (DFA-friendly prefix +
  predicate gate) or a per-rule fallback.
- **Trie / nested-switch on bytes** for `classify_keyword` only.
  Targets the keyword cascade (~140 SQL keywords today, length-
  bucketed nested `if`-chain). Branches share prefixes (`IN` →
  `INSERT` / `INSTEAD` / `INTERSECT` / `INTO`), so a trie is a clean
  fit. Smaller code-size impact than a full DFA.
- **Compile-time perfect hash** for `classify_keyword`. `gperf`-style
  build-time generated hash from identifier bytes → `TokenKind`. O(1)
  lookup with no comparisons after hashing. Best when keyword count is
  large enough that linear / trie lookup is the bottleneck.
- **SIMD-based pre-scan** (Wasm `v128`) for finding token boundaries
  and character-class membership in bulk. Effective if the per-byte
  work is tiny but the byte loop is the bound.

The choice depends on which sub-cost in `tokenize` dominates after the
**SoA + correctness** work above is done. None of these are useful
in isolation — they multiply with the SoA win, not replace it.
