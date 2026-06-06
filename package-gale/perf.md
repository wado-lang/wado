# Gale Performance Notes

Standing performance findings for the Gale code generator and its
generated parsers. The intent is the same as the "Failed Approaches"
section of [`AGENTS.md`](./AGENTS.md): record what was measured so the
next contributor does not re-run a dead-end spike.

## Generated-parser anatomy (where the cost is)

Measured on the SQLite grammar (`tests/grammars/SQLite.g4`, ~26 k lines
generated) — representative of the larger corpus grammars.

| Axis | scan_* share | notes |
| ---- | ------------ | ----- |
| Generated source / wasm size | ~21 % of the compiled module | parse_* is the bulk |
| `wado compile -O2` time | ~27 % | scan is fully reachable (live), so it is not dead-code-eliminated |
| **Parse runtime self-time** | **~12.5 %** | the rest is `tokenize` + `List<Token>::push` + parse_* |

The two axes pull in opposite directions: scan is **expensive to
compile / large in the artifact** but **cheap at runtime**. Runtime scan
time is also highly concentrated — on SQLite/queries.sql:
`scan_expr` 3.9 %, `scan_any_name` 3.4 %, `scan_keyword` 1.0 %,
`scan_literal_value` 0.5 % (guest profiler, `profiling-wado` skill).

## Data-driven (bytecode VM) scan — measured NO-GO (2026-06)

**Goal.** Replace the per-rule compiled `scan_*` functions with a single
bytecode interpreter over the already-clean scan IR
(`ScanBody`/`ScanElement` in `gir.wado`) + per-rule op tables, to cut
generated-artifact size and compile time (the ~21 % / ~27 % above) and
thereby speed up the full-build-to-test dev cycle.

**Why it was tempting.** The scan IR is already op-structured, and scan
is only ~12.5 % of parse self-time, so a-priori the runtime cost of
interpreting it looked affordable.

**Spike.** In a copy of the generated SQLite parser, the three hottest
*leaf* scanners (`scan_keyword`, `scan_literal_value`, `scan_any_name`
— together 4.9 % of parse self-time) were rewritten to delegate to a
faithful flat-`List<i32>` bytecode VM (every op — `TOK`, `KINDSET`,
`CALL`, `DISPATCH`, `OK`, `FAIL` — goes through the interpreter loop,
with bounds-checked `SCAN_PROG[ip]` fetches as Wado requires). Parse
output was identical (`acc` unchanged), so the measurement is valid.

**Result (end-to-end, SQLite, `benchmark/sqlite_parse/queries.sql`,
600 iters):**

| build | per-parse | vs baseline |
| ----- | --------- | ----------- |
| baseline (compiled scan) | ~9,275 µs | — |
| VM for 3 leaf scanners only | ~11,500 µs | **+24 %** |

Converting just 4.9 % of self-time cost +24 % wall time ⇒ implied
slowdown factor **K ≈ 5.9** for leaf/dispatch scanning. Projecting a
*full* conversion (12.5 % self-time, with larger rules amortizing
better) lands at roughly **+30 % to +60 %** parse-time regression.

**Why K is structurally high in Wado (not fixable by tuning the VM):**

1. **No `unsafe` / no raw pointers** — every bytecode fetch is a
   bounds-checked list index. There is no way to drop the check.
2. **Loss of inlining** — a 3-line `scan_keyword` is inlined into its
   caller at `-O2`; a VM call never is. The hottest scanners are
   precisely the tiny ones that benefit most from inlining today.
3. GC + value semantics add per-call overhead the compiled path avoids.

This confirmed the a-priori worry that a table/interpreter scanner
trades too much runtime speed. A separately reproducible upper bound:
stubbing **all** `scan_*` bodies to one opaque non-foldable helper (the
best case for "scan code removed") cut `-O2` compile time 18.2 s → 13.3 s
(−27 %) and wasm 435 KB → 341 KB (−21 %) — that is the *most* a
data-driven scanner could save, before adding back the interpreter and
tables.

**Decision.** Keep the compiled scanner. A **hybrid** (some rules
compiled, some interpreted) was rejected on maintainability grounds:
two scanner-codegen backends to keep in lockstep is not worth it.

## Implication for dev-cycle speed

The only large generated-size lever (removing scan code) is the same
lever that costs runtime speed, so **"shrink the artifact to speed up
the build" and "keep parse speed" are in tension** and cannot both be
won on the scan side. Pure compiled-side scan dedup (sharing near-
duplicate bodies that differ only in token constants, parameterized by
i32 args) caps at ~4 % of scan lines on the corpus — small.

Therefore dev-cycle speedups should come from the **build pipeline**,
which touches neither generated code nor runtime speed:

- compile only a representative grammar subset at `-O2` in the inner
  loop (full corpus on CI);
- a per-module (eventually per-function) opt-level control so the
  generated scanner can be built at `-O0` while parse stays `-O2`.

## Reproducing these measurements

- Generate a parser: `wado run package-gale gen tests/grammars/SQLite.g4`
  (or reuse a driver test's Kiln output).
- Compile-time / size: append a tiny `export fn run()` that calls the
  root `parse_*` so nothing is tree-shaken, then time `wado compile -O2`.
- Parse runtime: loop `parse_*` over `queries.sql` under an in-guest
  `MonotonicClock` timer; for self-time attribution use the
  `profiling-wado` skill (`wado run --profile guest,prof.json,5`).
