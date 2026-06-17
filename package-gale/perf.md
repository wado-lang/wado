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

| Parser                        |          per-iter | throughput |
| ----------------------------- | ----------------: | ---------: |
| **Gale (generated)**          | **~3.57 ms/iter** | ~3.75 MB/s |
| Rust `sqlparser-rs` (release) |     ~1.90 ms/iter |  7.05 MB/s |

Current gap ≈ **1.9×** vs `sqlparser-rs` release — down from the **12×**
this table recorded one revision ago. Two things closed it: the lexer
rework collapsed `tokenize` self-time, and the token list is now
pre-sized (`List::with_capacity(chars.len()/4 + 1)` in `tokenize`), which
all but eliminated `List<Token>::grow`. The fixture now parses **~38×
faster** than the 137 ms/iter recorded when this section first lived in
`TODO.md`, and **~5.6× faster** than the ~20 ms/iter of the previous
revision — so the priorities below are re-derived from a fresh profile,
not the historical one.

> **Measurement note (read before trusting the percentages).** The
> headline table above is from the release benchmark (`mise run
> sqlite-parse`, host built `--release`). The **profile below was
> captured with the dev-profile `wado`** (`cargo run`, per the
> inner-dev-loop guidance in the root `CLAUDE.md` — no release rebuild).
> `Cargo.toml` raises `opt-level` on `cranelift-codegen`, so the
> JIT-compiled **guest code is near-release quality**, but the wasmtime
> runtime, GC, and allocator host paths run at dev speed. Net effect: the
> dev host is ~10× slower per iter than the release headline, and that
> slack is concentrated in **allocation/GC host work**. So the profile
> **over-weights allocation-heavy guest frames** (`List<Token>::push`,
> the `_gale_rule` per-call rule-name allocation) relative to pure-compute frames
> (`scan_*`, `follow_yields`, the kind-set tests), which it
> **under-weights**. Read the table as relative self-time with that
> directional skew in mind; the _ordering within_ the compute frames and
> within the allocation frames is reliable, the alloc-vs-compute split is
> a soft upper bound on the alloc share.

Reproduce:

```sh
cd benchmark
# both baselines (release host — slow rebuild):
mise run sqlite-parse
# Gale alone, dev host, with a guest profile (self-time sampling):
wado run --no-cache --profile guest,/tmp/p.json,1 -O2 sqlite_parse/sqlite_parse.wado
```

(`wado` = `cargo run --bin wado --`. Analyze `p.json` with the
`profiling-wado` skill's script, or upload to profiler.firefox.com. The
table below merges **7 dev-host runs @1 ms = 1620 samples** to damp the
per-run noise of a ~3.5 ms-of-guest-work-per-iter workload.)

## Live profile (guest sampler, 7 runs merged, 1620 samples @1 ms)

|   Pct | Symbol                       | role                                             |
| ----: | ---------------------------- | ------------------------------------------------ |
| 24.0% | `List<Token>::push`          | per-token `struct.new Token` + array store       |
| 18.1% | `_gale_rule<ResultColumn..>` | per-call rule-name `String` alloc at the wrapper |
|  5.7% | `Parser::last_end`           | `Parser→List→Token→Span→end` load chain          |
|  4.3% | `_gale_kind_set_8`           | membership test over the big keyword set         |
|  4.1% | `follow_yields`              | runtime FOLLOW gate (LL repair), parse + scan    |
|  3.9% | `scan_any_name`              | scan (prediction)                                |
|  3.2% | `char::to_ascii_lowercase`   | case-insensitive keyword matching                |
|  2.7% | `scan_expr`                  | scan (LR precedence climb)                       |
|  2.5% | `StrCharIter::collect`       | `input.chars().collect()` into `List<char>`      |
|  1.7% | `Parser::expect`             | token read                                       |
|  1.7% | `tokenize`                   | lexer driver (was 27.9% historically)            |
|  1.5% | `List<char>::push`           | lexer char-buffer fill                           |
|  1.0% | `List<Token>::grow`          | token-array reallocation (now pre-sized away)    |
|  0.9% | `classify_keyword`           | keyword vs identifier disambiguation             |

Rough buckets (self-time): token-stream construction (`push`+`grow`) ≈
**25%**; the per-call rule-name `String` allocation at the `_gale_rule`
boundary (`_gale_rule<*>`, all variants) ≈ **21%** — but see §2: a spike
that removes only that allocation cuts ~40% of dev-host wall time, so the
profile **under**-attributes it; lexer char-level work
(`to_ascii_lowercase` + `List<char>` +
`classify_keyword` + `collect` + `try_*`) ≈ **13%**; `scan_*` ≈ **11%**;
kind-set membership ≈ **9%**; `Parser` token reads
(`last_end`/`expect`/`advance`) ≈ **8%**; the FOLLOW gate ≈ **4%**.

This is a different shape from the previous revision, where `follow_yields`
led at 18.6% and `List<Token>::grow` at 15.3%. The pre-size killed `grow`,
the per-loop FOLLOW prune dropped the gate to ~4%, and two
allocation-bound costs rose to the top: per-token construction and the
per-call rule-name `String` allocation at the generic rule wrapper
(neither present in the old profile at the same weight). Per the
measurement note, the two leaders are exactly the frames the dev host
inflates, but both are real allocations (per-token `struct.new`, §1, and
per-rule `struct.new String`, §2 — the latter confirmed by a spike, not
just the sampler), so the _ordering_ holds. Note §2 is **not** a subtree
copy (a WIR-level fact an earlier draft got wrong): the wrapper returns a
`ref`; the cost is the rule-name allocation at the call site.

## What would move the needle

Ordered by current self-time. None are mutually exclusive; several
multiply rather than add.

### 1. Token-stream construction — `push` 24.0% + `grow` 1.0% (~25%)

The dominant _category_, and now **allocation-bound** (`push`) rather
than reallocation-bound — the pre-size (`List::with_capacity` in
`tokenize`) already collapsed `grow` from 15.3% to ~1%, so that earlier
"cheapest win" is **done**. What remains is the Wasm GC
`(array (ref Token))` indirection plus a per-token `struct.new Token` on
every `push` (each `Token` carries a `LexerSlice`, a `Span`, and a
`leading_trivia: List<Token>`, so the struct is not trivial). The lever is
**SoA decomposition of `List<Token>`**: parallel primitive arrays
(`kinds` / `starts` / `ends` as `List<i32>`) so `peek_kind` becomes a
single `array.get i32` (not `array.get (ref Token)` + `struct.get`) and
per-token allocation disappears in the lex loop. Two ways to get there:

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

### 2. Per-call rule-name `String` allocation — the `_gale_rule` boundary (~40% dev-host)

**The single largest reducible cost, and not what the profile name
suggests.** Every parser rule is emitted as
`_parse_X(p, follow) = _gale_rule(_parse_X__inner(p, follow), "X")`.
`_gale_rule<T>` records the rule name on the `ParseError.rule_stack` on
the **error** path only:

```wado
fn _gale_rule<T>(r: Result<T, ParseError>, rule: String) -> Result<T, ParseError> {
    if let Err(mut e) = r { e.rule_stack.push(rule); return Result::Err(e); }
    return r;
}
```

The profile attributes ~18% self-time to `_gale_rule<ResultColumnNode>`,
which **misled an earlier draft into calling it a "subtree copy."** It is
not. The WIR proves the success path is free of any copy — `Result<…>` is
a boxed `ref`, so `return r` returns a reference and the only
`$value_copy$` is on the cold `Err` branch:

```text
fn _gale_rule<ResultColumnNode>(r, rule) {
    if ref.test …::Err(r) { … $value_copy … }   // cold: only on parse failure
    return r;                                     // returns a ref — no copy
}
```

The real cost lives at the **call site**, in the wrapper `_parse_X`: the
rule-name `"X"` is rebuilt on **every** rule entry as
`struct.new String { repr: array.new_data("result_column"), used: 13 }` —
a fresh GC allocation on the hot success path, consumed only on the cold
error path. Thousands of these per parse dominate the allocation traffic.

**Measured (dev host, same host throughout — the comparison is valid even
though absolute times are dev-inflated; spike copies the generated parser
and drives `queries.sql`):**

| variant                                     | per-iter | throughput |  vs base |
| ------------------------------------------- | -------: | ---------: | -------: |
| base (as generated)                         | ~39.1 ms |  ~342 KB/s |        — |
| rule-name hoisted to a single shared global | ~22.9 ms |  ~582 KB/s | **−41%** |
| `_gale_rule` bypassed entirely              | ~21.9 ms |  ~610 KB/s | **−44%** |

The middle row is the finding: keeping the wrapper call **and** its
`ref.test`, changing only the per-call `String` literal into one pre-built
global, recovers ~93% of the full removal. So:

- **Reducing the call count is not the lever.** Removing the wrapper call
  and `ref.test` on top of the hoist buys only ~3% more (22.9 → 21.9 ms);
  the wrapper itself is nearly free. (This contradicts the intuition that
  the per-rule call overhead matters — it does not.)
- **Reducing the per-call allocation is the whole win.** Build each
  distinct rule name once, not on every rule entry.

**Future direction.** Wado already has a const-aggregate → global hoist
(`const_object_globalization`, see `docs/optimizer.md`), but it does not
fire here for two reasons, both fixable: (a) its gate `is_globalizable_const`
does not list `ExprKind::StringLiteral`, and (b) a string only takes the
inline `array.new_fixed` const repr (vs the opaque `array.new_data` data
segment) when its byte length is `<= string_inline_max_bytes`, which is
**4** at `-O2` (`optimize::string_inline_max_bytes`) — far below the
13–25-byte rule names. Even raising that threshold alone is not enough:
the strings are inline call arguments, not `let` bindings, and the pass
only hoists `let`-bound aggregates, so the `struct.new String` stays at
the call site. Either path closes the gap:

- **Wado-side:** teach the const-aggregate hoist to also globalize a
  constant `String` (or any constant aggregate) appearing as an inline
  argument, not just a `let` binding — and let the eager-const-string
  threshold cover typical identifier-length literals. Fixes this for every
  generated parser and any Wado program that passes string literals on a
  hot path, with no Gale change.
- **Gale-side:** emit each rule name as a module-level
  `global RULE_<id>: String = "…"` once and pass that to `_gale_rule`
  (`gen_rule_entry_wrapper` in `parser_gen.wado`), instead of inlining the
  literal at every call. Lower-leverage but self-contained; the stronger
  variant passes an `i32` rule-id and turns `rule_stack` into
  `List<i32>`, eliminating hot-path string work entirely.

(Per the measurement note the dev host over-weights this allocation-bound
cost; the release share is smaller, but the per-parse allocation-count
reduction is real and host-independent.)

### 3. `Parser` token reads — `last_end` 5.7%, `expect`, `advance` (~8%)

`Parser::last_end` is a 4-step `Parser→List→Token→Span→end` load chain.
It ranks high **purely because of call frequency** — it is invoked an
enormous number of times — not because any single call is expensive.
That is why **inlining and precomputation both measured zero improvement**
(see below): there is no call overhead or redundant work to remove; the
only thing that moves the needle is making each load cheaper. The SoA
decomposition in (1) does exactly that, turning the end offset into a
direct `array.get i32`. This is the same lever as §1 — SoA pays off in two
places at once.

### 4. Kind-set membership — `_gale_kind_set_*` (~9%)

`_gale_kind_set_8` alone is 4.3%: a `k matches { TK_… | TK_… | … }`
membership test over the large SQLite keyword set (~125 kinds), called
from scan dispatch and the parser's lookahead gates. Generated today as a
branch/compare cascade (71 such helpers in the SQLite parser). A
compile-time **perfect hash** or a **bitset indexed by token kind**
(`(kind >> 5)` word + `1 << (kind & 31)`) turns it into O(1) with no
branch cascade — worth it because a handful of large sets dominate. This
is a pure-compute frame the dev host does _not_ inflate, so its release
share is likely a touch higher than 9%.

### 5. Lexer char-level work (~13%, independent secondary lever)

Inside lexing, work splits across `to_ascii_lowercase` (case-insensitive
matching, 3.2%), `List<char>` buffer building, `classify_keyword` (0.9%),
and the up-front `input.chars().collect()` into `List<char>`
(`StrCharIter::collect`, 2.5%, in `_gale_new_parser`). Pick by what
profiling on the predicate-correct lexer says is hottest (after Stage C
makes predicates real — a fast tokenizer is meaningless if it tokenizes
incorrectly). Candidates:

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

- **Inlining hot `Parser` methods / any per-method micro-opt.**
  `Parser::last_end` is high only because of its huge call count, not
  per-call expense. Precomputing it (caching the value in a field) or
  forcing inlining removes the named function from the profile but
  measured **no wall-time change** — there is no call overhead or
  redundant work to remove; the cost is performing the loads
  (`Parser→List→Token→Span→end`) that many times. wasmtime + Cranelift
  handle small Wasm calls cheaply enough that inlinability is not the
  lever. The real fix is making each load cheaper (SoA, §1).
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

### `core:cbor`/`core:serde` for the ATN blob codec — NO-GO (2026-06)

**Goal.** Replace the hand-written ATN wire-format codec (`serialize_atn`
in `atn.wado` + `atn_decode` in `runtime/atn.wado`) with derived
`Serialize`/`Deserialize` + `core:cbor` `to_bytes`/`from_bytes`, so the
field layout is generated from the struct (maximal DRY — no hand codec,
no shared constants) instead of two hand-kept-in-lockstep functions.

**Spike.** Measured the wasm-size cost in isolation (the codec linked for
a `List<i32>`-heavy struct), -O2:

| variant                                   |     wasm |
| ----------------------------------------- | -------: |
| hand `i32`-array decode (isolated)        |  7,740 B |
| `cbor` encode+decode + `serde` (isolated) | 20,622 B |

⇒ the cbor+serde codec adds **~+12.9 KB** (decode-only is less,
est. +6–9 KB net per parser). For reference the whole Binding2
parser+driver is **~33.6 KB** at -O2, so this is a **+18–40%** size hit
on every ATN-using parser. `report-wasm-size` is a tracked budget.

**Other costs.** (1) Decode runs once per `parse()` (per-`Parser`); a
generic `Deserializer` + base64/byte parse is structurally slower than a
single linear `array.get i32` walk — the wrong direction for parse speed.
(2) the runtime is inlined verbatim into every generated parser, so a
`core:serde`/`core:cbor`-based codec would carry those imports plus the
derived impls into every ATN-using parser — the size hit measured above —
where the hand codec stays dependency-free.

**Decision.** Keep the hand-written codec. After the wire-format DRY
refactor it is one writer (`serialize_atn`) + one reader (`atn_decode`)

- one shared constant set in `runtime/atn.wado` — fast, zero-dependency, and
  round-trip-tested (`atn_test.wado` drives `serialize_atn` → `atn_decode`
  field-for-field). The codec/reader pair is the irreducible minimum;
  serde would trade ~95 LOC for a heavyweight per-parser dependency.
  (Separable, still open: embedding the blob as base64/`#data` rather than
  an `i32`-array literal — a source-size/compile-time lever, codec-agnostic,
  worth revisiting only once a _large_ grammar's ATN literal is a measured
  problem. Today only small grammars carry an ATN.)

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
