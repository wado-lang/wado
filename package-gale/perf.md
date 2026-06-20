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
| **Gale (generated)**          | **~2.29 ms/iter** | ~5.83 MB/s |
| Rust `sqlparser-rs` (release) |     ~1.62 ms/iter |  8.25 MB/s |

Current gap ≈ **1.42×** vs `sqlparser-rs` release (gap is the
hardware-robust metric: this run measured `sqlparser-rs` at 1.62 ms; an
earlier run on another host put it at 1.90 ms — compare ratios, not
absolutes across runs). Tokens are stored struct-of-arrays
(`TokenStream`: parallel `i32` `kinds`/`starts`/`ends` + flat trivia
arrays); a token is a bare `i32` index — no per-token / per-terminal
aggregate is allocated, and every scan/dispatch read is a single
`array.get i32`. The fixture now parses **~60× faster** than the
137 ms/iter recorded when this section first lived in `TODO.md`.

> **Measurement note (read before trusting the percentages).** The
> headline table is from the release benchmark (`mise run sqlite-parse`).
> The **profile below was captured with the dev-profile `wado`** (`cargo
> run`, per the inner-dev-loop guidance in the root `CLAUDE.md` — no
> release rebuild). `Cargo.toml` raises `opt-level` on `cranelift-codegen`,
> so the JIT-compiled **guest code is near-release quality**, but the
> wasmtime runtime, GC, and allocator run at dev speed — making the dev
> host ~4–5× slower per iter (~10.8 ms vs ~2.29 ms release), with the
> slack concentrated in **allocation/GC**. So the profile inflates
> allocation-bound frames relative to pure-compute ones (`scan_*`,
> `follow_yields`, kind-set). Read the percentages as relative, and the
> alloc-vs-compute split as approximate.

Reproduce:

```sh
cd benchmark
# both baselines (release host — slow rebuild):
mise run sqlite-parse
# Gale alone, dev host, with a guest profile (self-time sampling):
wado run --no-cache --profile guest,/tmp/p.json,1 -O2 sqlite_parse/sqlite_parse.wado
```

(`wado` = `cargo run --bin wado --`. Analyze `p.json` with the
`profiling-wado` skill's script — count **leaf** frames for self-time —
or upload to profiler.firefox.com. The table below merges **5 dev-host
runs @1 ms = 4177 samples** to damp per-run sampling noise.)

## Live profile (guest sampler, 5 runs merged, 4177 samples @1 ms)

Post-SoA shape. The old per-token `struct.new Token` frame
(`List<Token>::push`, was 24%) is gone; the per-call rule-name `String`
allocation now leads outright.

|   Pct | Symbol                     | role                                                            |
| ----: | -------------------------- | --------------------------------------------------------------- |
| 24.3% | `_gale_rule<AnyNameNode>`  | per-call rule-name `String` alloc at the wrapper                |
|  7.6% | `Parser::last_end`         | `tokens.ends[pos-1]` — one `array.get i32`, huge call count     |
|  6.7% | `List<i32>::grow`          | `TokenStream` array growth (the `/4` pre-size under-shoots SQL) |
|  6.4% | `_kind_set_8`         | membership test over the big keyword set                        |
|  4.5% | `scan_any_name`            | scan (prediction)                                               |
|  4.4% | `follow_yields`            | runtime FOLLOW gate (LL repair), parse + scan                   |
|  3.6% | `char::to_ascii_lowercase` | case-insensitive keyword matching                               |
|  3.4% | `scan_expr`                | scan (LR precedence climb)                                      |
|  2.9% | `List<i32>::push`          | `TokenStream` token push (`push_token`)                         |
|  2.4% | `Parser::expect`           | token read                                                      |
|  2.3% | `try_IDENTIFIER`           | lexer identifier matcher                                        |
|  1.4% | `_parse_expr__inner`       | LR expr body                                                    |
|  1.1% | `classify_keyword`         | keyword vs identifier disambiguation                            |
|  1.1% | `StrCharIter::collect`     | one `input.chars().collect()` (lexer)                           |
|  0.8% | `tokenize`                 | lexer driver                                                    |
|  0.7% | `TokenStream::push_token`  | SoA token writer                                                |

Rough buckets (self-time): the per-call rule-name `String` allocation at
the `_gale_rule` boundary (all `_gale_rule<*>` variants summed) ≈ **~27%**
— now the dominant single cost (§1); `scan_*` ≈ **~13%**; kind-set
membership (`_kind_set_*`) ≈ **~11%**; token-stream construction (now
flat `i32`-array building: `List<i32>::grow`+`push`+`push_token`) ≈
**~10%** — down from ~25% pre-SoA, and dominated by `grow` because the
`chars.len()/4` pre-size under-shoots SQL token density (§4); `Parser`
token reads (`last_end`+`expect`) ≈ **~10%**; lexer char-level work
(`to_ascii_lowercase` + `classify_keyword` + `try_*` + `List<char>` +
`collect`) ≈ **~10%**; the FOLLOW gate ≈ **~4%**.

`Parser::last_end` is still ~7.6% **by call frequency**, not per-call cost:
the SoA already collapsed it to a single `array.get i32` (the old 4-step
`Parser→Token→Span→end` chain is gone), and inlining/precomputation
measured zero wall-time change — see "What does not work".

## What would move the needle

Ordered by profile self-time. None are mutually exclusive; several
multiply rather than add. (The token-stream SoA decomposition that led
this list pre-2026-06 is done — tokens are now flat `i32` arrays; see the
benchmark state and `git log`.)

### 1. Per-call rule-name `String` allocation — the `_gale_rule` boundary (~27% self-time)

**The single largest reducible cost, and now the top profile frame** (the
SoA rework removed the per-token `struct.new` that used to sit above it).
Every parser rule is emitted as
`_parse_X(p, follow) = _gale_rule(_parse_X__inner(p, follow), "X")`.
`_gale_rule<T>` records the rule name on the `ParseError.rule_stack` on
the **error** path only:

```wado
fn _gale_rule<T>(r: Result<T, ParseError>, rule: String) -> Result<T, ParseError> {
    if let Err(mut e) = r { e.rule_stack.push(rule); return Result::Err(e); }
    return r;
}
```

The profile puts ~24% self-time on `_gale_rule<AnyNameNode>` (the
most-entered rule — identifiers are everywhere in SQL), but it is **not a
copy**. The WIR shows the success path is copy-free — `Result<…>` is a
boxed `ref`, so `return r` returns a reference, and the only
`$value_copy$` is on the cold `Err` branch:

```text
fn _gale_rule<AnyNameNode>(r, rule) {
    if ref.test …::Err(r) { … $value_copy … }   // cold: only on parse failure
    return r;                                     // returns a ref — no copy
}
```

The real cost lives at the **call site**, in the wrapper `_parse_X`: the
rule-name `"X"` is rebuilt on **every** rule entry as
`struct.new String { repr: array.new_data("any_name"), used: 8 }` —
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

(Release allocates faster so the share is smaller than dev-host, but the
per-parse allocation-count drop is real and host-independent.)

### 2. Kind-set membership — `_kind_set_*` (~11%)

`_kind_set_8` alone is 6.4%: a `k matches { TK_… | TK_… | … }`
membership test over the large SQLite keyword set (~125 kinds), called
from scan dispatch and the parser's lookahead gates. Generated today as a
branch/compare cascade (71 such helpers in the SQLite parser). A
compile-time **perfect hash** or a **bitset indexed by token kind**
(`(kind >> 5)` word + `1 << (kind & 31)`) turns it into O(1) with no
branch cascade — worth it because a handful of large sets dominate. This
is a pure-compute frame the dev host does _not_ inflate, so its release
share is likely a touch higher than 11%.

### 3. Token-array pre-size — `List<i32>::grow` 6.7%

The SoA `tokenize` pre-sizes each `TokenStream` array to
`chars.len()/4 + 1`, but SQL is token-dense (short keywords/punctuation),
so the arrays still `grow`. Pre-sizing closer to the real token count —
a denser divisor, or a cheap first-pass token-count estimate — would
reclaim most of this `grow` self-time. Cheap, self-contained, in
`gen_tokenize_fn` (`lexer_gen.wado`).

### 4. Lexer char-level work (~10%, independent secondary lever)

Inside lexing, work splits across `to_ascii_lowercase` (case-insensitive
matching, 3.6%), `List<char>` buffer building, and `classify_keyword`
(1.1%). (The `Parser`'s separate `input.chars().collect()` is **gone**:
the SoA rework had the `TokenStream` borrow the lexer's chars, so the
program now collects
the source once, not twice.) Pick by what
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
  `Parser::last_end` is high (~7.6%) only because of its huge call count,
  not per-call expense — the SoA rework already made it a single
  `tokens.ends[pos-1]` (`array.get i32`). Precomputing it (caching the
  value in a field) or forcing inlining removes the named function from
  the profile but measured **no wall-time change** — there is no call
  overhead or redundant work to remove; the cost is performing that many
  bounds-checked `array.get`s. wasmtime + Cranelift handle small Wasm
  calls cheaply enough that inlinability is not the lever. What is left
  here is pure call frequency, which only a caller-side restructuring (not
  a micro-opt) could reduce.
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
