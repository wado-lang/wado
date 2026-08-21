---
name: jco
description: Transpile Wado Wasm components to JS with jco and run/benchmark them on Node. Use when running Wado on Node or browsers via jco, debugging jco-transpiled runtime failures, or benchmarking Wado on Node.
---

# Running Wado on Node via jco

jco (bytecodealliance) transpiles a Wasm **component** into JS + core Wasm so it
runs on a plain Wasm engine (V8/Node, browsers) instead of a Component Model
runtime. Wado targets WASI P3; this doc covers what the released jco does for
Wado today and what is still blocked.

## TL;DR

- Use the **released npm jco as a library** (`scripts/jco`, `mise run jco-*`).
- Compile Wado with **`-f no-wide-arithmetic`** — V8 has no wide-arithmetic
  proposal, and float formatting / `i128` emit it.
- Run on **Node 26+** — stable JSPI, no flag.
- **Compute programs work** (including float formatting). Filesystem programs
  run to completion; reading through a preopen is unverified (see below).
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

Released `@bytecodealliance/jco` as a library. `transpile-released.mjs` is a
plain `transpile()` — jco's own `preview3-shim` serves every import a Wado
program makes, and the output links to it through a `node_modules` symlink the
script writes beside the files.

The shim flushes stdout **after** `run()` resolves, so a runner that calls
`process.exit` on that promise loses the output. Let the event loop drain.

mise tasks:

```sh
mise run jco-deps                       # npm install released jco under scripts/jco
mise run jco-transpile-released foo.wasm [out-dir]
mise run jco-hello-released             # compile + transpile + run hello on Node
mise run jco-bench <program.wado> [runs] # compile -f no-wide-arithmetic, transpile, self-time
```

### Released jco status (verified at 1.30.0)

| Capability                | Status                                                                                                                                               |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Transpile (GC component)  | ✅ works, `wasi:http/service` included                                                                                                               |
| JSPI                      | ✅ native (Node 26 no flag; Node 24 needs the flag)                                                                                                  |
| Wide-arithmetic component | ❌ `transpile` rejects it (`wide arithmetic support is not enabled`); even if forced, V8 rejects the opcode at runtime → use `-f no-wide-arithmetic` |
| Stdout via stream         | ✅ jco's own shim delivers it, flushed after `run()` resolves                                                                                        |
| Filesystem read stream    | ⚠️ no longer deadlocks; reading through a preopen is unverified                                                                                      |

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

## WASI shims

<<<<<<< HEAD
BA ships a `preview3-shim` implementing P3 `cli` / `clocks` / `filesystem` /
`http`, with a browser build beside the Node one. A plain `jco transpile` wires
it, and stdout, float formatting, `wasi:random`, `MonotonicClock` and an HTTP
`handle` all run through it unaided. Its **browser** `cli` is unimplemented
(`throw new Todo()`), which is why the playground keeps a hand-written one.
||||||| f46102aade
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
=======
Minimal Node shims, selected with `--no-wasi-shim` + `--map` so they win over
jco's built-in shim (`transpile-released.mjs` wires these automatically):

- `cli.js` — stdout/stderr via `globalThis._jcoStreamWriteHook` (bypasses the
  rendezvous). `writeViaStream` returns a `Promise` because `write-via-stream`
  is async — jco lowers its result as a future and expects a Promise/Thenable.
- `clocks.js` — `wasi:clocks` via `process.hrtime` / `performance.timeOrigin`
  (the wall clock keeps sub-millisecond precision).
- `random.js` — `wasi:random` via Node crypto.

### `@bytecodealliance/preview3-shim`

BA ships a `preview3-shim` that implements P3 `cli` / `clocks` / `filesystem` —
but it does not currently substitute for the shims above:

- Its `cli` stdout does **not** deliver output standalone with released jco (same
  rendezvous gap the write hook works around). Keep `cli.js`.
- Its `filesystem` read goes through a worker + `TransformStream` whose
  `StreamReader` jco does not recognise as a lowerable stream — and even a
  correct shim deadlocks (see Known blockers).
>>>>>>> origin/main

## Benchmarking on Node

`mise run jco-bench <program.wado> [runs]` compiles with `-f no-wide-arithmetic`,
transpiles via the released pipeline, and runs the program self-timed `runs`
times (default 3; keep the best). The benchmark programs already self-time via
`core:benchmark` + `MonotonicClock` and print their own
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

**Unported** (the other benchmarks — `Preopens` / `wasi:filesystem`): zlib,
json-{twitter,canada,catalog}, sqlite-parse, syntax-highlight, cbor. They read
input data from preopened files, which the pipeline does not set. The data load
sits **outside** the timed loop, so wiring preopens runs these unchanged.

## Known blockers (jco / V8 gaps)

### wide-arithmetic (V8)

Not jco. Handled by `-f no-wide-arithmetic`.

### Filesystem reads (jco)

A file-reading program no longer hangs — `example/cat.wado` runs to completion
against `preview3-shim`. What it reads is unconfirmed: the shim needs its
preopens set (`_setPreopens` in `filesystem/descriptor.js`), and the
benchmarks that load data from a preopen are still unported.

### Reusing an instance (jco)

An instance serves a couple of calls, then the next suspends on a stream read
whose host injection is never driven (`JCO_DEBUG=1` ends at
`[StreamEnd#copy()] blocked`). `cloudflare-worker/` builds one per request.

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

| Error                                                | Likely cause                                                                                       |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `invalid numeric opcode: 0xfc16`                     | Wide-arithmetic — recompile with `-f no-wide-arithmetic`                                           |
| `WebAssembly.Suspending is not a constructor`        | Node < 24, or Node 22 picked up outside the repo (use Node 26)                                     |
| `FutureReadableEnd is not defined`                   | Future-end classes not injected (run via `transpile-released.mjs`)                                 |
| stdout empty                                         | Missing the stream-write hook or `cli.js` map                                                      |
| `wide arithmetic support is not enabled` (transpile) | Compile with `-f no-wide-arithmetic`; V8 cannot run the opcodes either                             |
| Hang / timeout                                       | JSPI Suspending missing on a trampoline, or a stream rendezvous deadlock (the filesystem read gap) |
