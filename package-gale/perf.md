# Gale Performance Notes

Standing performance findings for the Gale code generator and its
generated parsers: the current benchmark state, the live profile, the
directions that would move the needle, and the measured dead-ends. Read
with [`AGENTS.md`](./AGENTS.md) (architecture, LL-prediction design,
failed approaches) and [`antlr4-compatibility.md`](./antlr4-compatibility.md).

**Performance-related TODO items live here, not in `TODO.md`.**

## Benchmark state (measured 2026-06)

`benchmark/sqlite_parse`, 13366-byte realistic SQL fixture, guest run at
`-O2` under wasmtime:

| Parser                        |        per-iter | throughput |
| ----------------------------- | --------------: | ---------: |
| **Gale (generated)**          | **~20 ms/iter** |  ~660 KB/s |
| Rust `sqlparser-rs` (release) |   ~1.64 ms/iter |  8.15 MB/s |

Current gap ≈ **12×** vs `sqlparser-rs` release. (The earlier "5× gap"
figure compared against `sqlparser-rs` _debug_, ~6.7 ms/iter; ~3× on
that axis.) The fixture now parses **~6.6× faster** than the 137 ms/iter
recorded when this section first lived in `TODO.md` — the lexer rework
collapsed `tokenize` self-time, so the priorities below are re-derived
from a fresh profile, not the historical one.

Reproduce:

```sh
cd benchmark
# both baselines:
mise run sqlite-parse
# Gale alone, with a guest profile (self-time sampling):
wado run --no-cache --profile guest,/tmp/p.json,1 -O2 sqlite_parse/sqlite_parse.wado
```

(`wado` = `cargo run --bin wado --`. Analyze `p.json` with the
`profiling-wado` skill's script, or upload to profiler.firefox.com.)

## Live profile (guest sampler, 1177 combined samples @1–5 ms)

|   Pct | Symbol                     | role                                          |
| ----: | -------------------------- | --------------------------------------------- |
| 18.6% | `follow_yields`            | runtime FOLLOW gate (LL repair), parse + scan |
| 15.3% | `List<Token>::grow`        | token-array reallocation                      |
|  7.1% | `Parser::last_end`         | `Parser→List→Token→Span→end` load chain       |
|  6.1% | `_gale_kind_set_8`         | membership test over the big keyword set      |
|  5.0% | `List<Token>::push`        | per-token `struct.new Token`                  |
|  4.3% | `List<char>::grow`         | lexer char-buffer reallocation                |
|  4.2% | `char::to_ascii_lowercase` | case-insensitive keyword matching             |
|  3.6% | `scan_any_name`            | scan (prediction)                             |
|  3.2% | `scan_expr`                | scan (LR precedence climb)                    |
|  2.7% | `Parser::expect`           | token read                                    |
|  1.8% | `tokenize`                 | lexer driver (was 27.9% historically)         |

Rough buckets: token-stream construction (`grow`+`push`) ≈ **20%**; the
FOLLOW gate ≈ **19%**; `Parser` token reads ≈ **11%**; kind-set
membership ≈ **9–10%**; lexer char-level work
(`to_ascii_lowercase` + `List<char>` + `classify_keyword`) ≈ **13%**;
`scan_*` ≈ **8–10%**.

## What would move the needle

Ordered by current self-time. None are mutually exclusive; several
multiply rather than add.

### 1. The runtime FOLLOW gate — `follow_yields` (~19%)

New top cost since the LL repair moved to a runtime-threaded
`follow: &List<List<i32>>` argument (see _LL Prediction_ in
`AGENTS.md`). `follow_yields` runs at every tail-greedy `Repeat`
iteration on **both** the parse and scan sides, walking the caller
continuation at every depth. Levers, cheapest first:

- **Narrow where it is called.** It only needs to fire on `Repeat`s that
  actually have a caller-FOLLOW conflict; tighten the `gate_caller_follow`
  predicate in lowering so non-conflicting loops emit no gate at all
  (the `emit_follow` prune already removes it grammar-wide when there is
  no gate anywhere — this is the per-loop refinement).
- **Cheapen the check.** It compares token kinds against small per-depth
  sets; a flat `i32` representation or a depth-0 fast path (the common
  K=1 case) avoids the nested `List<List<i32>>` walk.
- **Hoist invariants.** The `follow` argument and `TK_EOF` are loop
  invariant; ensure the loop guard does not re-fetch them per iteration.

### 2. Token-stream construction — `grow` 15.3% + `push` 5.0% (~20%)

The dominant _category_, now reallocation-bound (`grow`) rather than
lexer-bound. Two non-overlapping paths (carried from the original
analysis, still valid):

1. **Pre-size the token list.** `grow` at 15% means the `List<Token>`
   reallocates repeatedly while tokenizing. Reserve capacity up front
   (e.g. proportional to input length) so the lex loop does at most one
   or two growths. Cheapest available win.
2. **SoA decomposition of `List<Token>`.** The deeper cost is Wasm GC
   `(array (ref Token))` indirection plus per-token `struct.new Token`.
   Decompose into parallel primitive arrays (`kinds` / `starts` / `ends`
   as `List<i32>`) so `peek_kind` becomes a single `array.get i32` (not
   `array.get (ref Token)` + `struct.get`) and per-token allocation
   disappears in the lex loop. Two ways to get there:
   - **Gale-side:** redesign `Token` so hot fields are flat primitives,
     with an opaque sidecar (or removal) for `text` / `leading_trivia`;
     keep the public `Token` API as a view handle if needed.
   - **Wado-side:** extend `container_sroa` to handle (a) struct fields
     (currently locals only), (b) inner structs with nested
     struct/reference fields, (c) cross-function rewrites for the
     `scan_*(&List<Token>, ...)` parameter pattern (1100+ sites in the
     SQLite parser pass `&p.tokens` as a bare reference, always
     escaping). Today the pass fires on zero candidates in
     Gale-generated parsers.

### 3. `Parser` token reads — `last_end` 7.1%, `expect`, `advance` (~11%)

`Parser::last_end` is a 4-step `Parser→List→Token→Span→end` load chain.
**Inlining / per-method micro-opt does not help** (see below): the cost
is the actual loads, which the SoA decomposition in (2) removes by making
the end offset a direct `array.get i32`.

### 4. Kind-set membership — `_gale_kind_set_*` (~9–10%)

`_gale_kind_set_8` alone is 6.1%: a membership test over the large SQLite
keyword set, called from scan dispatch and the parser's lookahead gates.
Generated today as a branch/compare cascade. A compile-time **perfect
hash** or a **bitset indexed by token kind** (`(kind >> 5)` word +
`1 << (kind & 31)`) turns it into O(1) with no branch cascade — worth it
because a handful of large sets dominate.

### 5. Lexer dispatch (~13%, independent secondary lever)

Inside lexing, work splits across `to_ascii_lowercase` (case-insensitive
matching), `List<char>` buffer building, and `classify_keyword`. Pick by
what profiling on the predicate-correct lexer says is hottest (after
Stage C makes predicates real — a fast tokenizer is meaningless if it
tokenizes incorrectly). Candidates:

- **Table-driven DFA** for the whole lexer (NFA → DFA → transition
  table). Replaces both per-character dispatch and `classify_keyword`;
  `mode` blocks become a DFA per mode plus mode-switch on accept; lexer
  commands attach as accept-state attributes. Semantic predicates are the
  only DFA-blocker (need a hybrid prefix + predicate gate).
- **Trie / nested-switch on bytes** for `classify_keyword` only. Shared
  prefixes (`IN` → `INSERT` / `INSTEAD` / `INTERSECT` / `INTO`). Smaller
  code-size impact than a full DFA.
- **Compile-time perfect hash** (`gperf`-style) for `classify_keyword`.
- **SIMD pre-scan** (Wasm `v128`) for token boundaries / character-class
  membership in bulk, if per-byte work is tiny but the byte loop is the
  bound.

## What does not work

- **Inlining hot `Parser` methods / any per-method micro-opt.** Caching
  `Parser::last_end` as a field or forcing inlining removes the named
  function from the profile but does not move wall time — the cost is the
  loads (`Parser→List→Token→Span→end`), not call overhead. wasmtime +
  Cranelift handle small Wasm calls cheaply enough that inlinability is
  not the lever. The real fix is removing the loads (SoA, §2).
- **Data-driven / bytecode-VM scan** (see below).

## Failed approaches (do not repeat)

### Data-driven (bytecode VM) scan — NO-GO (2026-06)

**Goal.** Replace the per-rule compiled `scan_*` functions with a single
bytecode interpreter over the already-clean scan IR
(`ScanBody`/`ScanElement` in `gir.wado`) + per-rule op tables, to shrink
the generated artifact (scan is ~21% of the compiled module / ~27% of
`wado compile -O2` time) and so speed up the full-build-to-test cycle.
Tempting because scan is only ~8–12% of parse self-time.

**Spike.** In a copy of the generated SQLite parser, the three hottest
_leaf_ scanners (`scan_keyword`, `scan_literal_value`, `scan_any_name`)
were rewritten to delegate to a faithful flat-`List<i32>` bytecode VM
(every op — `TOK`, `KINDSET`, `CALL`, `DISPATCH`, `OK`, `FAIL` — runs
through the interpreter loop with bounds-checked `SCAN_PROG[ip]`
fetches). Parse output was identical, so the timing is valid.

**Result (end-to-end, SQLite, `queries.sql`, 600 iters):**

| build                       |  per-parse | vs baseline |
| --------------------------- | ---------: | ----------: |
| baseline (compiled scan)    |  ~9,275 µs |           — |
| VM for 3 leaf scanners only | ~11,500 µs |    **+24%** |

Converting ~4.9% of self-time cost +24% wall time ⇒ implied slowdown
factor **K ≈ 5.9**. A full conversion projects to **+30% to +60%** parse
time.

**Why K is structurally high in Wado (not tunable away):**

1. **No `unsafe` / no raw pointers** — every bytecode fetch is a
   bounds-checked list index; the check cannot be dropped.
2. **Loss of inlining** — a 3-line `scan_keyword` is inlined into its
   caller at `-O2`; a VM call never is, and the hottest scanners are
   exactly the tiny ones that benefit most from inlining.
3. GC + value semantics add per-call overhead the compiled path avoids.

**Decision.** Keep the compiled scanner. A hybrid (some rules compiled,
some interpreted) was rejected on maintainability grounds — two
scanner-codegen backends to keep in lockstep is not worth it. The
artifact-size lever and runtime speed are in direct tension here, so the
compiled scanner stays and dev-cycle wins should come from the build
pipeline (subset/lower-opt inner-loop builds), not from interpreting
scan.

## Correctness items with a performance flavor

These are ATN-class prediction gaps tracked for compatibility, but each
is "Gale's static predictor commits where ANTLR4 defers" — relevant when
reasoning about the scan/predict hot path. Full context in `TODO.md`
("Stage A gaps") and `AGENTS.md` (soundness invariants).

- **LR operator-precedence chain** (`DropLoopEntryBranchInLRRule_4`):
  `scan_expr_lr_*` sees `and X` match and commits where ANTLR4 resolves
  the precedence via full-context prediction at the LR loop entry.
- **Recursive lexer rule with `.+?` / `.*?`**
  (`RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`): the static
  single-pass emitter over-consumes nested `/* … */` comments.
