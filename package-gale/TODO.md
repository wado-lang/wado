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

### Multi-token tail-greedy inner — closed (2026-05-17)

The single-token-mask path (`tail_greedy_first_of_element`,
`intern_follow_variant` keyed on `Array<String>`) is now joined by a
parallel **K-prefix mask** path that admits multi-token-inner Repeats
under the same soundness contract as the 1-token version. The variant
body's iter-entry gate checks the next K tokens against the caller's
deterministic K-prefix (`Array<Array<String>>`, indexed by input
depth), so:

- `a : N (X Y)+ ;` with caller `c : X Y Z` correctly yields the
  trailing `[X, Y, Z]` to `c` regardless of how many surplus
  `(X Y)` blocks precede it (the per-depth gate only fires when `Z`
  actually arrives at depth 2).
- HTMLParser's `htmlContent` keeps re-entering on `TAG_OPEN` because
  the closing-tag mask `[TAG_OPEN, '/', TagName]` differs from the
  iter prefix `[TAG_OPEN, TagName, …]` at depth 1.

See:

- `tests/grammars/ll_multi_token_tail.g4` and
  `tests/driver_ll_multi_token_tail_test.wado` for the regression
  fixture (shape derived from upstream
  `runtime-testsuite/.../ParserExec/PredictionMode_LL.txt`).
- `gen_context::tail_greedy_k_prefix_of_rule`,
  `gen_context::compute_call_site_k_prefix_mask`,
  `gen_context::compute_k_prefix_position_mask`,
  `gen_context::deep_position_first_sets_from` — the analysis
  surface.
- `RepeatOp.k_prefix_mask` /
  `ScanRepeatElem.k_prefix_mask` — IR threading.
- `parser_gen::emit_k_prefix_yield_gate` — emit-side gate.

Deferred work:

- [ ] Iter-body K-prefix for `Repeat` inner `RuleRef`s. The fixed-
      point "next iter | exit-to-caller" computation is sound but the
      gate inside an iter body is not yet plumbed — RuleRefs sitting
      inside a Repeat fall back to the existing 1-token mask path.
      Few real grammars need it; revisit when an upstream descriptor
      surfaces a regression.
- [ ] Multi-alt `RuleRef` expansion in
      `deep_position_first_sets_from`. The current implementation
      halts at a multi-alt rule (per-depth union of multi-alt
      prefixes would over-yield by matching cross-alt sequences that
      no real alt admits). A per-alt sequence representation could
      extend this without losing soundness — useful when a caller's
      continuation passes through a multi-alt rule like
      `expr : literal | name`.
- [ ] Multi-alt variant dispatcher emit. `parse_<rule>__follow_<id>`
      for multi-alt rules currently dispatches to the regular
      `parse_<rule>_bt_<n>` per-alt helpers instead of the variant's
      `parse_<rule>__follow_<id>_bt_<n>` versions, so the variant
      per-alt helpers are emitted but unreachable from the
      dispatcher. This is a pre-existing emit shape that limits the
      cascade through multi-alt rules — the K-prefix path stops at
      `RuleRef` (see `tail_greedy_k_prefix_of_element`) partly
      because of this. Fixing the dispatcher would let K-prefix flow
      through multi-alt rules cleanly; the K-prefix `RuleRef` recursion
      gate can then be relaxed.

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
