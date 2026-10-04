---
name: jco
description: Transpile Wado Wasm components to JS with jco, then run, debug, and benchmark them on Node. Use for anything that involves jco, Node, or a browser as the host.
---

# Running Wado on Node via jco

jco transpiles a component into JS and core Wasm, so it runs on V8 rather than a
Component Model runtime. The released npm jco (verified at 1.30.0) runs Wado's
compute, filesystem, and `wasi:http/service` programs.

## Requirements

- Node 26+, for stable JSPI. The repo pins it; outside the repo `node` may be a
  system Node 22, whose JSPI fails (`WebAssembly.Suspending is not a constructor`).
- Compile with `-f no-wide-arithmetic`. No V8 implements wide arithmetic, which
  float formatting and `i128` emit, so without it the module fails with
  `invalid numeric opcode: 0xfc16` (or transpile refuses it). The flag lowers
  those builtins to software forms in `core:rt` before NIR.

## Pipeline

```sh
mise run jco-deps                        # npm install jco under scripts/jco
mise run jco-transpile-released foo.wasm [out-dir]
mise run jco-hello-released              # compile, transpile, run hello
mise run jco-bench <program.wado> [runs] # self-timed; keep the best
```

`transpile-released.mjs` is a plain `transpile()`; jco's `preview3-shim` serves
every import, linked through a `node_modules` symlink beside the output. Two
things a runner must do:

```js
import { _setPreopens } from "@bytecodealliance/preview3-shim/filesystem";
_setPreopens({ ".": "/abs/host/dir" }); // before the import; through the symlinked shim
const m = await import("./out/prog.js");
await m.run.run();
await new Promise((r) => setTimeout(r, 1000)); // the shim's stdout worker flushes late
```

A second copy of the shim keeps its own preopen table the program never reads.
`jco-bench` takes the preopen from `JCO_PREOPEN` (default: the repository root;
the benchmarks want `JCO_PREOPEN=benchmark`).

The shim's browser `cli` is unimplemented, so the playground keeps its own.

## Numbers

Node's rate over wasmtime's, best of three on one machine:

| Benchmark  | Node (jco) / wasmtime |
| ---------- | --------------------- |
| mandelbrot | ~0.95×                |
| sieve      | ~2.3×                 |

## Known Blocker

A reused instance serves a couple of calls, then suspends on a stream read whose
host injection never runs (`JCO_DEBUG=1` ends at `[StreamEnd#copy()] blocked`).
`cloudflare-worker/` builds one per request.

## Debugging

- jco's async machinery loses errors: add
  `process.on('unhandledRejection', e => { console.error(e); process.exit(1); })`.
- `JCO_DEBUG=1` traces every trampoline; a trailing
  `[ComponentAsyncState#suspendTask()]` with no progress is a rendezvous
  deadlock. Run under `timeout 12` so one doesn't wedge.
- The canonical builtins are `streamWrite()`, `streamRead()`,
  `_lowerImportBackwardsCompat()` (async lower), `taskReturn()`, and
  `_genStreamHostInjectFn` / `createReadableStreamEnd` (host-to-guest futures);
  string-replace a header in the transpiled JS to log one.
- A bare `unreachable` from a program reading files is a missing preopen or a
  path outside it. `FutureReadableEnd is not defined` means it was not
  transpiled through `transpile-released.mjs`.
