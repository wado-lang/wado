---
name: jco
description: Transpile Wado Wasm components to JS with jco and run/benchmark them on Node. Use when running Wado on Node or browsers via jco, debugging jco-transpiled runtime failures, or benchmarking Wado on Node.
---

# Running Wado on Node via jco

jco (bytecodealliance) transpiles a Wasm **component** into JS + core Wasm so it
runs on a plain Wasm engine (V8/Node, browsers) instead of a Component Model
runtime. Wado targets WASI P3, and jco's P3 support is still incomplete, so this
doc covers what works today, the workarounds, and what is still blocked.

## TL;DR

- Use the **released npm jco as a library + a thin JS post-process layer**
  (`scripts/jco`, `mise run jco-*`). No Rust fork build needed for what works.
- Compile Wado with **`-f no-wide-arithmetic`** — V8 has no wide-arithmetic
  proposal, and float formatting / `i128` emit it.
- Run on **Node 26+** — stable JSPI, no flag.
- **Compute programs work** (including float formatting). **Filesystem programs
  hang** on a jco read-stream gap (diagnosed below) — deferred.
- Quick check: `mise run jco-hello-released`. Benchmark:
  `mise run jco-bench <program.wado>`.

## Environment

- **Node 26+ required.** Node 26 (V8 14.6) ships **stable JSPI**
  (`WebAssembly.Suspending`), so no flag is needed; the repo pins `node = "26"`
  in `mise.toml`. Node 24 needs `--experimental-wasm-jspi`; Node 22's older JSPI
  fails (`WebAssembly.Suspending` is not a constructor).
- **`/tmp` pitfall:** outside the repo, `node` may resolve to a system Node 22
  (mise activation is path-scoped). Run inside the repo, or use the pinned
  binary's absolute path.
- **V8 has no wide-arithmetic** in any version (no flag exists). This is a V8
  gap, not a jco one — handled by `-f no-wide-arithmetic` (see below).

## Vendor-free pipeline (`scripts/jco`)

Preferred path: released `@bytecodealliance/jco` as a library + post-process.

| File                     | Role                                                                                               |
| ------------------------ | -------------------------------------------------------------------------------------------------- |
| `transpile-released.mjs` | `transpile()` (released jco) + `--no-wasi-shim` + `--map` to `example/jco-shim`, then post-process |
| `postprocess.mjs`        | Inject the future-end classes + the stream-write hook into the transpiled JS                       |
| `harvest-intrinsics.mjs` | Extract the future-end classes from released jco's own output (version-matched)                    |
| `missing-intrinsics.js`  | The harvested classes (generated; regenerate after a jco bump)                                     |
| `future-trigger.wado`    | One-line `Future::<i32>::new()` that makes released jco emit those classes                         |

mise tasks:

```sh
mise run jco-deps                       # npm install released jco under scripts/jco
mise run jco-harvest-intrinsics         # regenerate scripts/jco/missing-intrinsics.js
mise run jco-transpile-released foo.wasm [out-dir]
mise run jco-hello-released             # compile + transpile + run hello on Node
mise run jco-bench <program.wado> [runs] # compile -f no-wide-arithmetic, transpile, self-time
```

### Released jco status (verified at 1.24.3)

| Capability                   | Status                                                                                                                                               |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Transpile (GC component)     | ✅ works                                                                                                                                             |
| JSPI                         | ✅ native (Node 26 no flag; Node 24 needs the flag)                                                                                                  |
| Wide-arithmetic component    | ❌ `transpile` rejects it (`wide arithmetic support is not enabled`); even if forced, V8 rejects the opcode at runtime → use `-f no-wide-arithmetic` |
| Future-end intrinsic classes | ❌ not emitted on the future-drop path → inject (post-process)                                                                                       |
| Stdout via stream            | ❌ needs the write hook + `cli.js` (post-process + shim)                                                                                             |
| Filesystem read stream       | ❌ deadlocks (jco async gap, see Known blockers)                                                                                                     |

### The post-process transforms

`postprocess.mjs` applies two string transforms that mirror the fork's
runtime-affecting patches, so released jco can run what it otherwise can't:

1. **Inject future-end classes** (`FutureEnd` / `FutureReadableEnd` /
   `FutureWritableEnd`) at module scope before `class InternalFuture`. Released
   jco references them on the future-drop path (stdout write) but never emits
   their definitions → `FutureReadableEnd is not defined`. They **must** be at
   module scope (they reference module-scoped `NESTED_FUTURE_SYMBOL`,
   `getOrCreateAsyncState`, `FUTURES`), not on `globalThis`. Sourced from
   released jco itself via `harvest-intrinsics.mjs`, so they stay version-matched.
2. **Stream-write hook**: insert a `globalThis._jcoStreamWriteHook` fast-path
   before `streamEnd.copy(...)` in `streamWrite`, so `cli.js` receives stdout
   bytes directly from linear memory (jco's byte-at-a-time rendezvous does not
   deliver them on its own).

Not replicated: the async-non-void-export fix (fork patch #3). Void exports
(`run`, `test` blocks) don't need it; result-returning async exports (HTTP
`handle`) do — but those don't transpile through released jco yet anyway.

## wide-arithmetic (`-f no-wide-arithmetic`)

Wado emits the Wasm wide-arithmetic proposal (`i64.mul_wide_u/s`, `i64.add128`,
`i64.sub128`) for float formatting (`core:prelude/fpfmt.wado`) and `i128`. **No
V8 implements it** (checked through Node 26; no flag, `--wasm-staging` no help),
so any component containing those opcodes fails `WebAssembly.compile` with
`invalid numeric opcode: 0xfc16`.

`-f no-wide-arithmetic` lowers them to plain 32-bit-limb i64 sequences in
`wado-compiler/src/codegen/emit/wide_arith_downlevel.rs` (delete that file once
V8 ships the proposal). The WIR still shows the wide ops; only the final Wasm
changes. **Compile every Node-bound Wado program with this flag** — a bare
`println` of a float needs it.

## WASI shims (`example/jco-shim`)

Minimal Node shims, selected with `--no-wasi-shim` + `--map` so they win over
jco's built-in shim (`transpile-released.mjs` wires these automatically):

- `cli.js` — stdout/stderr via `globalThis._jcoStreamWriteHook` (bypasses the
  rendezvous). `writeViaStream` returns a `Promise` because `write-via-stream`
  is async — jco lowers its result as a future and expects a Promise/Thenable.
- `clocks.js` — `wasi:clocks` via `process.hrtime` / `Date.now`.
- `random.js` — `wasi:random` via Node crypto.

### `@bytecodealliance/preview3-shim`

BA ships a `preview3-shim` that implements P3 `cli` / `clocks` / `filesystem` —
but it does not currently substitute for the shims above:

- Its `cli` stdout does **not** deliver output standalone with released jco (same
  rendezvous gap the write hook works around). Keep `cli.js`.
- Its `filesystem` read goes through a worker + `TransformStream` whose
  `StreamReader` jco does not recognise as a lowerable stream — and even a
  correct shim deadlocks (see Known blockers).

## Benchmarking on Node

`mise run jco-bench <program.wado> [runs]` compiles with `-f no-wide-arithmetic`,
transpiles via the released pipeline, and runs the program self-timed `runs`
times (default 3; keep the best). The benchmark programs already self-time via
`core:benchmark` + `MonotonicClock` (served by `clocks.js`) and print their own
throughput line, so no host timing is needed.

```sh
mise run jco-bench benchmark/count_prime/count_prime.wado
```

**Works today** (compute-only — `Stdout` + `MonotonicClock`):

| Benchmark   | Wado on Node (jco) | Wado on wasmtime        |
| ----------- | ------------------ | ----------------------- |
| count-prime | ~4.3 M numbers/s   | ~4.6 M                  |
| mandelbrot  | ~4.0 M px/s        | ~4.2 M                  |
| sieve       | ~150 M numbers/s   | ~64 M (V8 ~2.3× faster) |
| fts         | ~12 M conv/s       | —                       |

Compute throughput on V8 lands within ~5–10% of wasmtime (sieve is much faster
on V8). Numbers are indicative on a noisy cloud VM; keep best-of-3.

**Blocked** (the other benchmarks — `Preopens` / `wasi:filesystem`): zlib,
json-{twitter,canada,catalog}, sqlite-parse, syntax-highlight, cbor. They read
input data from preopened files and hang on the jco read-stream gap below. The
data load sits **outside** the timed loop, so once jco's filesystem read works
(or the data is embedded at compile time — deferred) these run unchanged.

## Known blockers (jco / V8 gaps)

### wide-arithmetic (V8)

Not jco. Handled by `-f no-wide-arithmetic`.

### Filesystem read-stream deadlock (jco)

`open_at` → `read_via_stream` → `stream.read` hangs. Diagnosed precisely:

- A correct shim returning `read-via-stream` as an async-iterable **is**
  recognised — jco's binding generates `readFn` from the iterator and calls
  `readEnd.setHostInjectFn(...)` (the `hostInjectFn: undefined` seen at
  `StreamReadableEnd` construction is normal; it is set right after).
- But when the guest calls `stream.read`, the task **suspends and the
  `hostInjectFn` is never driven** — the host read fn is never invoked (verified
  with a marker), so the read blocks forever.

So the gap is jco's **async stream-read rendezvous** (delivering a host
injection to a suspended read task), not the shim or a missing hook. Fixing it
needs fork-level work on jco's async task/waitable machinery — bigger than the
stdout write hook. The data-embedding workaround for benchmarks is deferred until
jco's filesystem path works.

### HTTP service (jco)

`wasi:http/service` (async `handle` returning `Result`) does **not** transpile
through released jco (`cannot represent this component in WIT: the type
'request' appears more than once`) and is a separate server harness anyway.

## The fork path (`vendor/jco`)

Only needed for cases the released pipeline can't cover (and to develop the jco
patches). `vendor/jco` is pinned at a clean upstream commit; the Wado patches
live in `vendor/jco.patch` and are applied to the working tree before building.

### Setup

```sh
git submodule update --init vendor/jco
cd vendor/jco
git apply ../../vendor/jco.patch
NPM_TOKEN="" PUPPETEER_SKIP_DOWNLOAD=1 pnpm --filter "@bytecodealliance/jco" install
```

### Build cycle

```sh
cd vendor/jco
cargo build -p xtask --target x86_64-unknown-linux-gnu
rm -f packages/jco/obj/js-component-bindgen-component.*
./target/x86_64-unknown-linux-gnu/debug/xtask build debug   # re-transpile jco itself
```

Or `mise run build-jco`, then `mise run jco-transpile <file.wasm>`.

### Current patches (`vendor/jco.patch`)

1. **Stream write hook** (`async_stream.rs`): `globalThis._jcoStreamWriteHook`
   delivers stream data directly from linear memory, bypassing the rendezvous.
2. **Missing intrinsic dependencies** (`mod.rs`): `StreamNewFromLift` →
   `SymbolResourceRep`; `FutureDropReadable/Writable` → `GlobalFutureMap`,
   `FutureEndClass`, `FutureReadableEndClass`, `FutureWritableEndClass`,
   `GetOrCreateAsyncState` (otherwise the stdout write path references undefined
   future-end classes, e.g. `FutureReadableEnd is not defined`).
3. **Async export void return** (`function_bindgen.rs`): when a JSPI-wrapped
   async export returns `undefined` (result delivered via `task.return`), fall
   back to `task.completionPromise()` instead of lifting `undefined`.

The released pipeline reproduces patches #1 and #2 as JS post-process (see
`postprocess.mjs`); #3 is fork-only.

### Saving patches

The submodule stays at the pinned upstream commit; patches are never committed
into it, only to `vendor/jco.patch`:

```sh
cd vendor/jco
git diff HEAD -- crates/js-component-bindgen/src/ > /home/user/wado/vendor/jco.patch
cd /home/user/wado && git add vendor/jco.patch   # + `git add vendor/jco` only when bumping the pin
```

### Key source locations in jco

| File                                                            | Purpose                                                     |
| --------------------------------------------------------------- | ----------------------------------------------------------- |
| `crates/js-component-bindgen/src/lib.rs`                        | Top-level `WasmFeatures` validator config                   |
| `crates/js-component-bindgen/src/core.rs`                       | Core module validation features (multi-memory augmentation) |
| `crates/js-component-bindgen/src/transpile_bindgen.rs`          | Trampoline generation (JSPI wrapping)                       |
| `crates/js-component-bindgen/src/function_bindgen.rs`           | Function call/result codegen                                |
| `crates/js-component-bindgen/src/intrinsics/mod.rs`             | Intrinsic dependency resolution                             |
| `crates/js-component-bindgen/src/intrinsics/p3/async_stream.rs` | Stream classes and operations                               |
| `crates/js-component-bindgen/src/intrinsics/p3/async_future.rs` | Future classes and drop operations                          |
| `crates/js-component-bindgen/src/intrinsics/p3/async_task.rs`   | AsyncTask class (read rendezvous lives near here)           |

## Debugging jco runtime errors

Transpiled output is one large JS file. Useful canonical-builtin → JS mappings:

| Wasm builtin          | jco JS function                                      | Notes             |
| --------------------- | ---------------------------------------------------- | ----------------- |
| `stream.write`        | `streamWrite()`                                      | JSPI Suspending   |
| `stream.read`         | `streamRead()`                                       | JSPI Suspending   |
| `canon lower (async)` | `_lowerImportBackwardsCompat()`                      | JSPI Suspending   |
| `task.return`         | `taskReturn()`                                       |                   |
| `future.new` (lift)   | `_genStreamHostInjectFn` / `createReadableStreamEnd` | host→guest wiring |

### Techniques

- **Catch swallowed errors** — jco's async machinery loses errors as unhandled
  rejections:
  ```js
  process.on('unhandledRejection', e => { console.error('UNHANDLED:', e); process.exit(1); });
  ```
- **`JCO_DEBUG=1 node run.mjs`** — verbose trace of every instruction/trampoline.
  A trailing `[ComponentAsyncState#suspendTask()]` with no progress = a
  rendezvous deadlock.
- **Inject logging** by string-replacing a function header in the transpiled JS
  (e.g. add `console.error(...)` to `streamRead`/`generatedStreamHostInject`).
- **Timeout hangs**: `timeout 12 node run.mjs` so a deadlock doesn't wedge.

### Common error patterns

| Error                                                | Likely cause                                                                                                                        |
| ---------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `invalid numeric opcode: 0xfc16`                     | Wide-arithmetic — recompile with `-f no-wide-arithmetic`                                                                            |
| `WebAssembly.Suspending is not a constructor`        | Node < 24, or Node 22 picked up outside the repo (use Node 26)                                                                      |
| `FutureReadableEnd is not defined`                   | Future-end classes not injected (run via `transpile-released.mjs`)                                                                  |
| stdout empty                                         | Missing the stream-write hook or `cli.js` map                                                                                       |
| `rec group usage requires 'gc' proposal`             | Fork `WasmFeatures` missing `GC` / `WASM3`                                                                                          |
| `wide arithmetic support is not enabled` (transpile) | Fork `core.rs` validates with `WasmFeatures::default()` — but prefer `-f no-wide-arithmetic` over forcing it, since V8 can't run it |
| `invalid variant discriminant for expected`          | Async export returns `undefined` (fork patch #3)                                                                                    |
| Hang / timeout                                       | JSPI Suspending missing on a trampoline, or a stream rendezvous deadlock (the filesystem read gap)                                  |
