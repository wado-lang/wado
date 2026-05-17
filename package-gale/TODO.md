# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget
they imply. The compatibility contract, stage layering, descriptor
test pipeline, and current pass/fail numbers live in
[`antlr4-compatibility.md`](./antlr4-compatibility.md). The
permanent design constraints (LL soundness invariants etc.) live
there as well; this file is for what is _not yet done_.

## Generated parser bugs

(none currently)

## LL prediction — remaining gaps

### Multi-token tail-greedy inner

`tail_greedy_first_of_element` only adopts a `Repeat` whose inner
consumes exactly one token (single TokenRef / Literal / Wildcard /
Not / single-token RuleRef / single-token Group). The first-exact
discriminator excludes multi-element groups for the same reason —
soundness invariant 1 in `antlr4-compatibility.md`. Patterns like
`(A B)?` where the inner is multi-token therefore get no variant
emitted today.

Action items:

- [x] Add a regression fixture (`tests/grammars/ll_multi_token_tail.g4`)
      and `#[TODO]` driver test that exercises the gap.
      `r : a c EOF ; a : N (X Y)+ ; c : X Y Z ;` on input
      `N X Y X Y Z` — `a`'s greedy `(X Y)+` eats the trailing iter that
      `c` needs. Sourced shape from upstream
      `runtime-testsuite/.../ParserExec/PredictionMode_LL.txt`.
- [ ] Decide whether to (a) extend the static analysis to track
      multi-token-prefix sequences in the follow mask, or (b) emit a
      2-token-lookahead variant. Option (a) is consistent with the
      existing one-shot mask suppression; option (b) requires runtime
      lookahead at the variant entry.

### ATN-class grammars

Grammars whose alt selection requires arbitrary-length lookahead
through ambiguous prefixes cannot be decided by static FOLLOW. ANTLR4
handles them with a runtime ATN simulator (closure / predict / DFA
cache) — see `vendor/antlr4/runtime/Java/src/org/antlr/v4/runtime/atn/
ParserATNSimulator.java`. Gale's static path will always have edges.

Two complementary directions, neither scoped yet:

- **Runtime ATN simulator** in Gale. Large investment; matches ANTLR4
  semantics one-for-one.
- **Stage B′ via the JVM ANTLR4 oracle.** Shell out to the vendored
  `antlr4` JVM tool (already available in the submodule plus
  `runtime-testsuite/`) to compute oracle parse trees for descriptors
  whose `[output]` is action-printed (`FullContextParsing/*`,
  composite descriptors, etc.) and would otherwise be auto-skipped by
  `normalize_output_for_stage_b`. Cheaper to land; gives us a
  measurement axis for any future runtime simulator.

## LL prediction — architecture cleanup

These do not move the LL coverage envelope; they reduce coupling
between the codegen walk and the analysis layer.

- [ ] **Move variant registration to a `FollowEnv` pre-pass.** Today
      `intern_follow_variant` is called from inside the codegen walk
      (parse-side `gen_element` and scan-side `gen_scan_element`).
      Pre-computing every `(rule, mask)` pair as part of `FollowEnv`
      would let codegen do a pure lookup. Architecturally cleaner;
      no behaviour change.
- [x] **Retire `current_follow_mask` and `current_outer_follow` from
      `GenContext`.** Done in issue #1043 step (5d) —
      `lower_variant_rules` threads the surrounding variant's mask as
      `variant_mask: &Array<String>` through every lowering function;
      `compute_call_site_follow` takes it as an explicit `outer_follow`
      parameter for the deep-nullable suffix propagation; emit's
      `gen_op_repeat_optional_rulecall` consumes the baked
      `RepeatOp.caller_follow_with_mask` rather than re-combining the
      mask at emit time.

## Stage B follow-on — composite descriptors (Stage C dependency)

All 17 `CompositeLexers` / `CompositeParsers` upstream descriptors
auto-skip today. The bottleneck is _not_ multi-input plumbing
(`extract_antlr4_descriptors.wado`'s `parsed.slave_grammars.len() > 0`
short-circuit could be lifted; Kiln already supports multi-input).
Every composite descriptor's `[output]` is a host-side artefact —
`<writeln(...)>` action-body prints (`S.a`, `M.b`, `T.y`),
`Token.toString` dumps (`[@0,0:2='abc',<1>,1:0]`), or empty `[output]`.
None survive `normalize_output_for_stage_b`. Re-evaluate this entry
once Stage C lands.

## Stage C — action / predicate execution

Gale currently **recognizes** but **silently discards** the contents
of `{ ... }` action blocks and `{ ... }?` semantic predicates. The g4
parser accepts them, so grammars that contain them (`ANTLRv4Lexer`,
`RustLexer`, `RustParser`, `TypeScriptLexer`, `TypeScriptParser`) load
cleanly — but the generated lexer/parser behaves as if every
predicate were `true` and every action were a no-op. That is wrong
for:

- `RustLexer.RAW_STRING_LITERAL` — the closing `#` count must match
  the opening `#` count, enforced by a predicate; without it Gale
  mistokenizes Rust raw strings.
- TypeScript's regex-vs-division disambiguation and other context-
  sensitive lexer rules (3 predicates) and parser rules (17).

Stage C is a hard prerequisite for several things:

- Treating Gale as a drop-in ANTLR4 replacement (the stated principle
  in [AGENTS.md](./AGENTS.md)).
- Any lexer-level optimization work — claiming a tokenizer is fast is
  meaningless if it tokenizes incorrectly.
- `Grammar.options.superClass` and `tokenVocab`, which become wireable
  once action bodies are real.

Sketch:

- Extend the IR so `OptionValue::Action` and per-alt action /
  predicate elements carry a language-tagged source fragment instead
  of being a placeholder string.
- Add a pluggable "action translator" interface; ship at minimum an
  identity translator for Wado-written action bodies (so
  Wado-authored grammars work natively).
- Generate a `SuperClass` trait (name derived from
  `superClass = Foo`) and require callers to `impl` it; emit action
  bodies as method calls on `self` that resolve through that trait.
- `tokenVocab` falls out at that point — another grammar's generated
  `TokenKind` enum can be imported by name rather than merged at IR
  time.

Translating Java/Rust/Python action bodies to Wado is the obvious
hard part. A reasonable first cut is to require callers to provide
hand-written Wado equivalents for non-Wado grammars (a sidecar file
mapping predicate / action ID → Wado snippet), with a migration
path to automatic translation later. See
[`docs/wep-2026-03-02-gale.md`](../docs/wep-2026-03-02-gale.md) for
the design direction.

## Performance — where the 5× gap actually lives

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

Combined: token-stream construction (`tokenize`, `Array<Token>::push`,
and `Array<Token>::grow`) is 58% of self-time. Token reads via
`Parser` are next.

### What does not work

- **Inlining hot Parser methods.** `Parser::last_end` accounted for
  17% self-time; both caching it as a field and forcing `#[inline]`
  eliminate the named function from the profile but do not move wall
  time. The cost was the actual loads
  (`Parser→Array→Token→Span→end`), not call overhead — inlining
  merely redistributes it into the callers (`parse_expr`,
  `Parser::expect`, …). wasmtime + Cranelift handles small Wasm
  function calls cheaply enough that hunting for inlinability is not
  a productive lever here.
- **Any micro-optimization on individual Parser methods.** Same
  reason: the bytes loaded are unchanged, so the work is unchanged.

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
   (currently locals only — see
   `wado-compiler/src/optimize/container_sroa.rs` "Future
   directions"), (b) inner structs with nested struct or reference
   fields, (c) cross-function rewrites for the `scan_*(&Array<Token>,
   ...)` parameter pattern (1100+ sites in the SQLite parser pass
   `&p.tokens` as a bare reference, currently always escaping). Today
   the pass fires on zero candidates in Gale-generated parsers.

### Lexer dispatch (independent secondary lever)

Inside the 27.9% `tokenize` self-time, the work splits roughly into
per-character branch dispatch and keyword classification. Several
techniques can replace the current hand-rolled cascade — pick by what
profiling on the predicate-correct lexer (after Stage C) says is
hottest:

- **Table-driven DFA** for the whole lexer (NFA → DFA subset
  construction → state-transition table). Replaces both per-character
  dispatch and `classify_keyword`. ANTLR4 `mode` blocks become a DFA
  per mode plus mode-switch on accept; lexer commands (`skip`, `more`,
  `type(N)`, `channel(HIDDEN)`) attach as accept-state attributes.
  Semantic predicates are the only DFA-blocker — once predicates are
  real (Stage C), predicate-bearing rules need a hybrid (DFA-friendly
  prefix + predicate gate) or a per-rule fallback.
- **Trie / nested-switch on bytes** for `classify_keyword` only.
  Targets the keyword cascade (~140 SQL keywords today, length-
  bucketed nested `if`-chain). Branches share prefixes (`IN` →
  `INSERT` / `INSTEAD` / `INTERSECT` / `INTO`), so a trie is a clean
  fit. Smaller code-size impact than a full DFA.
- **Compile-time perfect hash** for `classify_keyword`. `gperf`-style
  build-time generated hash from identifier bytes → `TokenKind`. O(1)
  lookup with no comparisons after hashing. Best when keyword count
  is large enough that linear / trie lookup is the bottleneck.
- **SIMD-based pre-scan** (Wasm `v128`) for finding token boundaries
  and character-class membership in bulk. Effective if the per-byte
  work is tiny but the byte loop is the bound.

The choice depends on which sub-cost in `tokenize` dominates after
the SoA + correctness work above is done. None of these are useful
in isolation — they multiply with the SoA win, not replace it.
