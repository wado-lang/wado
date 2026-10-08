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
decision below serves this, and a CI check holds it: the corpus compiled with
one thread and with several, compared byte for byte.

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

### The package owns its functions

`NirPackage::functions` becomes a store of `NirFunction` values indexed by
`FuncId`. Nothing holds a function by `Rc`: a closure functor names its methods
by `FuncId`, and so do the interpreter's callees.

Mutable access to a function goes through the store, and the store counts it.
The per-body memos key on that count instead of on reported edits, so a pass
cannot rewrite a body behind a memo's back. `FunctionGate::mark_changed` stays,
for scheduling only: it decides which functions a pass revisits, which costs
precision when wrong, never correctness. The count is conservative: a visit
that takes a function mutably and changes nothing still invalidates its memo
entries. Roadmap step 2 measures what that costs.

### What a parallel visit may touch

A visit in a parallel sweep gets its own function mutably and everything else
read-only:

- The type table, shared. A visit does not intern a type. A pass interns what
  its visits need before the sweep, as `peephole` already does for the
  builtins it synthesizes. The per-function passes intern nothing today.
- Other functions, as of the sweep's start. The pending functions are taken out
  of the store for the sweep. One that another visit may read, which today
  means a callee the interpreter can run, is copied first, and readers see the
  copy.
- Whole-program facts built before the sweep, such as the mod/ref summaries and
  the heap effects. A memo filled lazily during the sweep holds pure values and
  sits behind a thread-safe cell, so which thread fills it changes nothing.

A visit does not add a function. A pass that mints functions plans them during
the sweep and appends them after it, in function order.

`Rc` inside `const_eval::Value` and inside the type table's trait records
becomes `Arc`, so that `Body`, `NirFunction` and `TypeTable` are `Send` and
`Sync`. A static assertion holds that.

### Who decides the thread count

`CompilerOptions` carries the degree of parallelism, and the compiler never
picks one itself. The library default is one. The CLI passes the available
cores for `compile`, `check`, `run` and `serve`, and one for `wado test`, which
parallelizes across files instead. A shared knob (`-j`) overrides it.

The compiler uses `rayon`, already in the workspace through `wasmtime`, behind
a `cfg` that leaves it out on `wasm32`. There the same code runs sequentially.

### What runs in parallel

In order of what each is worth on the table above:

1. The per-function loop passes, through `run_gated`.
2. The read-only walks every pass starts with: `BodyMemo` refreshes, the DCE
   walks, and `param_spec`'s `summarize_params` and `collect_sites`.
3. The per-function passes after the loop: `store_load_forward`'s fixpoint,
   `const_object_globalization`'s candidate collection, condition
   implication.
4. `inline`'s splicing, one caller per visit. The candidates are already
   copies, so a caller reads nothing another visit writes.

The decisions an interprocedural pass makes about the whole program, such as
which callees to clone or which parameters to drop, stay sequential.

## Roadmap

1. Make `Body`, `NirFunction` and `TypeTable` `Send` and `Sync`, and give the
   package its type table by value. Done when the static assertion compiles.
2. Replace `Vec<Rc<RefCell<NirFunction>>>` with the store, counting mutable
   access, and key every `BodyMemo` on that count. Done when no
   `Rc<RefCell<NirFunction>>` remains, the gate no longer counts edits, and the
   memo hit rate on `package-gale` is measured.
3. Adopt the sweep semantics in `run_gated`, still sequential, with reads of
   other functions served from the sweep-start copies. Done when the tests and
   `test-wado` pass and `benchmark/` and `wasm-size/` hold within noise.
4. Add the executor and the thread-count option, and run the per-function loop
   passes in parallel. Done when the thread-count invariance check runs in CI
   and `package-gale`'s loop time is measured against step 3.
5. Run the read-only whole-program walks in parallel.
6. Run the post-loop per-function passes and `inline`'s splicing in parallel.

## Known gaps

- Developer traces (`WADO_TRACE`) interleave across threads, so their order
  varies between runs.
- The frontend and the backend stay sequential.
- A sweep copies the callees the interpreter may read, so a parallel compile
  holds more memory than a sequential one. `test-wado` already peaks near
  14 GB.
