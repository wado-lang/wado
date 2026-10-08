# WEP: Parallel Optimizer

## Context

The NIR optimizer takes most of a compile, and it runs on one core. Compiling
`package-gale/src/main.wado` with a dev build spends its time as follows,
measured with `wado compile --log-level debug` and its span trace:

| Where                                                                                                                                 | Seconds |
| ------------------------------------------------------------------------------------------------------------------------------------- | ------: |
| Whole compile                                                                                                                         |    75.5 |
| Frontend (load through lower)                                                                                                         |     7.0 |
| NIR fixed-point loop                                                                                                                  |    47.1 |
| ↳ per-function passes (`peephole`, `copy_prop`, `licm`, `const_fold`, `sroa`, `tmpl_hoist`, `let_block_flatten`, `value_copy_demote`) |    26.3 |
| ↳ interprocedural passes (`param_spec`, `inline`, `svr`, `sroa_param`, `dae`, `drve`, `container_sroa`)                               |    21.2 |
| NIR after the loop (`store_load_forward`, `const_object_globalization`, DCE, …)                                                       |    17.0 |
| WIR build, WIR optimize, codegen                                                                                                      |     3.6 |

The package has about 16,000 function bodies, so the work divides finely. A
per-function pass already visits functions independently through the gate
([WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md)).
What stops running those visits on several cores is how the package holds them:

- `NirPackage::functions` is `Vec<Rc<RefCell<NirFunction>>>`, and the type
  table is `Rc<RefCell<TypeTable>>`. Neither can cross a thread.
- A visit reads other functions. The compile-time interpreter evaluates callee
  bodies through `Rc` handles while the caller is borrowed mutably, and
  `inline` reads the bodies it splices.
- A sweep is ordered. `FunctionGate::run_gated` marks a rewritten function's
  neighbours dirty during the sweep, so a later function in the same sweep sees
  the earlier rewrite and may be visited because of it.
- A few memos shared by every visit sit behind `RefCell` (the heap-effect
  `reach_memo`, the interpreter's callee map).
- Every per-body memo trusts each pass to report the bodies it rewrites with
  `FunctionGate::mark_changed`. A missed report leaves a stale fact and wrong
  code, and only a rotating debug check looks for one.

Two hosts constrain the answer. The playground runs the O2 pipeline on
`wasm32-unknown-unknown`, which has no threads. `wado test` already compiles
one file per core, so a compile that also takes every core would oversubscribe
the machine.

## Decision

### Output does not depend on the thread count

A compile emits the same bytes on one thread, on many, and on `wasm32`. Every
decision below serves this. No dedicated check holds it: the compiler is
deterministic as a rule (`wado-compiler/AGENTS.md`), and a lapse shows as drift
in the WIR golden fixtures.

### What a change may cost

The output may change wherever these decisions change what the optimizer sees,
as the sweep semantics below do. The generated code may run slower, by at most
5% on any row of `benchmark/`, the price of a parallel pipeline, as ThinLTO
pays against full LTO. Code size is not a criterion.

### A sweep sees the state it started from

A gated sweep fixes its work set when it starts: the functions pending for the
pass at that moment. Each visit reads its own function and the other functions
as they stood when the sweep began, and the neighbours of the functions it
rewrote are marked once the sweep ends. The order of visits then cannot change
what a visit sees, so visits can run in any order and on any thread.

This changes which round a function is revisited in. Measured on the current
`main`, with only the marking deferred and visits still sequential: the 48
programs of `benchmark/` and `example/` compile to identical bytes at O2, and
`package-gale` converges in the same 12 rounds to a module 313 bytes larger.
The change is adopted for its own sake, sequentially, before any thread is
added, so a regression it brings is not confused with one threads bring.

### A function is a cell that counts its writes

`NirPackage::functions` holds each function in a `FuncCell` shared by `Arc`: a
reader-writer lock and a write count. `borrow` and `borrow_mut` fail at once on
a conflicting borrow, as `RefCell`'s do, so a visit that reaches a function
another thread is writing panics rather than races.

A `borrow_mut` guard bumps the count on drop when the function changed, so a
borrow that changes nothing counts nothing, whatever route it took. The body's
parts and the locals are each `Tracked`: a version that grows on every mutable
borrow, with an epoch drawn afresh on construction and clone, so a part put in
place of another never compares equal to it. The rest of the function is
compared against a copy the guard takes on the first whole-function mutable
borrow. `FuncWriteGuard::parts` hands a rewrite the body and locals without
that copy. The per-body memos key on the count instead of on reported edits, so
a pass cannot rewrite a body behind a memo's back.
`FunctionGate::mark_changed` stays, for scheduling only: it decides which
functions a pass revisits, which costs precision when wrong, never
correctness.

A mutable borrow that writes back what was there is still an edit, so a rewrite
writes only what it changed (`Engine::set_block_stmts` compares first), and a
pass that only looks for something to rewrite asks through a shared borrow
first (`NirFunction::calls_any`). Measured on `package-gale`, sequentially:

| Memo key                     | Loop seconds |
| ---------------------------- | -----------: |
| Edits each pass reported     |         47.1 |
| Mutable dereferences counted |         53.3 |
| Changes counted              |         48.6 |

The type table stays behind the package's `RefCell`. A sweep borrows it once
and hands each visit the shared reference.

### What a parallel visit may touch

A visit in a parallel sweep gets its own function mutably and everything else
read-only:

- The type table, shared. A visit is handed what it reads rather than the
  package, so it cannot intern a type. A pass interns what
  its visits need before the sweep, as `peephole` already does for the
  builtins it synthesizes. The per-function passes intern nothing today.
- Other functions, as of the sweep's start. The pending functions are taken out
  of the store for the sweep. One that another visit may read, which today
  means a callee the interpreter can run, is cloned first, and readers see the
  clone. A `Tracked` part is shared between clones until either side writes it,
  so a clone costs only the parts the function's own visit then rewrites.
- Whole-program facts built before the sweep, such as the mod/ref summaries and
  the heap effects. A memo filled lazily during the sweep holds pure values and
  sits behind a thread-safe cell, so which thread fills it changes nothing.

A memo that answers a cycle provisionally while the real verdict is in flight
answers by query order. Each visit keeps its own, as
`const_object_globalization`'s callee verdicts do. One that settles an exact
fixpoint, such as `SharedEscape`'s, is shared.

A visit does not add a function. A pass that mints functions plans them during
the sweep and appends them after it, in function order.

`Rc` inside `const_eval::Value` and inside the type table's trait records
becomes `Arc`, so that `Body`, `NirFunction` and `TypeTable` are `Send` and
`Sync`. A static assertion holds that.

### Who decides the thread count

`CompilerOptions` carries the degree of parallelism, and the compiler never
picks one itself. The library default is one. The CLI passes the available
cores for `compile`, `build`, `run`, `serve` and `dump`, and one for
`wado test`, which parallelizes across files instead. `--optimize-threads <n>`
overrides it on each of them.

The compiler uses `rayon`, already in the workspace through `wasmtime`, behind
a `cfg` that leaves it out on `wasm32`. There the same code runs sequentially.

### What runs in parallel

In order of what each is worth on the table above:

1. The per-function loop passes, through `run_gated_par`.
2. The read-only walks every pass starts with: `BodyMemo` refreshes, the DCE
   and mod/ref walks, the interpreter's callee map, `CallImmutability`'s
   receiver summaries, the stores into immutable globals `const_fold` reads,
   `param_spec`'s call constants, parameter facts and sites, and `inline`'s
   argument facts and candidate classification.
3. The per-function passes after the loop: `store_load_forward`'s fixpoint,
   the global constant folding, condition implication, `freeze_pure_arith`,
   and `const_object_globalization`'s candidate collection and hoists.
4. `inline`'s splicing, one caller per visit. The candidates are already
   copies, so a caller reads nothing another visit writes.
5. `sroa_variant_return`'s call-site rewrites.

A fact a whole-program walk reads off each body every round is taken once per
body version instead. The DCE reachability `param_spec` reads each round
carries each body's inspect signatures and `array_clone` element types.

The decisions an interprocedural pass makes about the whole program, such as
which callees to clone or which parameters to drop, stay sequential.

## Roadmap

- [x] Make `Body`, `NirFunction` and `TypeTable` `Send` and `Sync`. Done when
  the static assertion compiles.
- [x] Replace `Rc<RefCell<NirFunction>>` with `Arc<FuncCell>`, and key every
  `BodyMemo` on the cell's write count. Done when no
  `Rc<RefCell<NirFunction>>` remains, the gate no longer counts edits, and the
  memo hit rate on `package-gale` is measured.
- [x] Adopt the sweep semantics in `run_gated`, still sequential, with reads of
  other functions served from the sweep-start copies. Done when the tests and
  `test-wado` pass and no `benchmark/` row is more than 5% slower.
- [x] Add the executor and the thread-count option, and run the per-function
  loop passes in parallel. Done when `package-gale`'s loop time is measured
  against the sequential sweep.
- [x] Run the read-only whole-program walks in parallel.
- [x] Run the post-loop per-function passes and `inline`'s splicing in
  parallel.

`package-gale` compiled with a dev build, `wado compile --optimize-threads <n> src/main.wado`, emits the same bytes on every row:

| Build                   | Seconds |
| ----------------------- | ------: |
| `main`                  |    75.5 |
| This design, 1 thread   |    78.2 |
| This design, 16 threads |    38.3 |

## Known gaps

- On one thread a compile is slower than on `main`. The memos re-derive for
  every change, where `main`'s trusted the edits each pass reported and so
  kept facts across changes no pass reported (a value-graph build growing the
  pool, for one). `licm` still writes some bodies it does not change, and
  `const_object_globalization` memoizes per visit.
- `value_copy_demote`'s analysis memo answers a recursive call provisionally,
  `container_sroa` interns types during its visits, and `dae`, `drve`,
  `sroa_param` and the DCE closure decide over the whole program. All run on
  one thread.
- Developer traces (`WADO_TRACE`) interleave across threads, so their order
  varies between runs.
- The frontend and the backend stay sequential.
- A sweep clones the callees the interpreter may read, and a clone keeps every
  part its function's visit rewrites alive twice until the sweep ends.
  `test-wado` peaks near 16 GB.
