---
name: jco
description: Debug and patch jco (JS Component Tooling) for Wado wasm transpilation. Use when jco transpile or jco-transpiled code fails at runtime.
---

# jco Patching Workflow

jco (`vendor/jco`) is bytecodealliance's tool for transpiling Wasm Components to JavaScript. Wado uses a patched fork because upstream jco has incomplete support for P3 async stream delivery and JSPI integration.

The pinned submodule points at a clean upstream commit; the Wado patches live
in `vendor/jco.patch` and are applied to the working tree before building.

> **Node 24+ required.** The runtime uses JSPI (`WebAssembly.Suspending`). Node
> 22 ships an older JSPI where `WebAssembly.Suspending` is not a constructor and
> fails. The repo pins Node 24 via `mise.toml`; make sure `node` resolves to it
> (paths outside the repo, e.g. `/tmp`, may pick up a system Node 22).

## Two paths: fork vs. released + runtime shim

There are two ways to transpile a Wado component:

1. **Patched fork** (`vendor/jco`) — the original path; build the Rust fork and
   use its CLI. Documented in the rest of this file.
2. **Released jco + post-process** (`scripts/jco`, `mise run jco-transpile-released`)
   — use the unmodified npm `@bytecodealliance/jco` as a _library_ and supply the
   missing P3 runtime as a thin JS layer, no Rust fork build. Prefer this for the
   CLI/test worlds; fall back to the fork for cases it can't yet cover (below).

### Released jco status (as of 1.24.3)

Verified against `example/hello.wado`:

- ✅ Transpile succeeds; the old `WasmFeatures` gaps (GC, wide-arithmetic) are gone.
- ✅ JSPI works natively on Node 24 (`--experimental-wasm-jspi`) — the
  `WebAssembly.Suspending` fork patch is no longer needed.
- ❌ Still emits code referencing `FutureReadableEnd` / `FutureWritableEnd`
  without defining them on the future-_drop_ path (the stdout write path). This
  is the one runtime-blocking gap; it maps to the fork's "missing intrinsic deps"
  patch (`vendor/jco.patch`, mod.rs).

### How the vendor-free path supplies the runtime

`scripts/jco/postprocess.mjs` applies two string transforms to the transpiled JS,
mirroring the runtime-affecting fork patches:

| Fork patch (vendor/jco.patch)               | Post-process equivalent                                                                                  | Needed for hello |
| ------------------------------------------- | -------------------------------------------------------------------------------------------------------- | ---------------- |
| missing intrinsic deps (mod.rs)             | inject `FutureEnd`/`FutureReadableEnd`/`FutureWritableEnd` at module scope before `class InternalFuture` | ✅               |
| stream write hook (async_stream.rs)         | insert the `_jcoStreamWriteHook` fast-path before `streamEnd.copy(...)`                                  | ✅ (stdout)      |
| async non-void export (function_bindgen.rs) | not replicated — left to the fork                                                                        | ❌ (void `run`)  |

Key facts that make this work:

- The injected classes **must go at module scope**, not `globalThis` — they
  reference module-scoped bindings (`NESTED_FUTURE_SYMBOL`, `getOrCreateAsyncState`,
  `FUTURES`). A bare undeclared identifier read still resolves through global
  scope, but the classes' own dependencies would not.
- The class source is **harvested from released jco itself**, version-matched: a
  trigger component that exercises `future.new` (`scripts/jco/future-trigger.wado`,
  a one-line `Future::<i32>::new()`) makes released jco emit the classes, which we
  extract. Released-harvested vs fork-built classes were byte-identical. Re-run
  `mise run jco-harvest-intrinsics` after a jco bump to refresh
  `scripts/jco/missing-intrinsics.js`.

### Vendor-free workflow

```sh
mise run jco-deps                       # install released jco under scripts/jco
mise run jco-harvest-intrinsics         # (re)generate scripts/jco/missing-intrinsics.js
mise run jco-transpile-released foo.wasm [out-dir]
mise run jco-hello-released             # compile+transpile+run hello end to end
```

The post-process layer is a stopgap: once the dep-edge fix lands upstream, drop
the class injection; the stream hook / async-export pieces may need separate
upstreaming. Delete `scripts/jco` transforms as upstream catches up.

### Known gaps (released path)

- HTTP service (`wasi:http/service`, async `handle` returning `Result`) does **not**
  transpile through released jco — it fails with `cannot represent this component
  in WIT: the type 'request' appears more than once`. Separate Wado↔jco WIT-interop
  issue; use the fork for HTTP until resolved. (This is also why the harvest
  trigger is a minimal CLI future program, not the HTTP example.)

## Setup

```sh
git submodule update --init vendor/jco
cd vendor/jco
git apply ../../vendor/jco.patch                     # apply the Wado patches
# Upstream jco is a pnpm workspace; install the CLI package + its deps:
NPM_TOKEN="" PUPPETEER_SKIP_DOWNLOAD=1 pnpm --filter "@bytecodealliance/jco" install
```

## Build Cycle

After editing jco Rust source:

```sh
cd vendor/jco

# 1. Build xtask (host target)
cargo build -p xtask --target x86_64-unknown-linux-gnu

# 2. Re-transpile the jco component to JS (bootstrap: uses existing jco to transpile itself)
rm -f packages/jco/obj/js-component-bindgen-component.*
./target/x86_64-unknown-linux-gnu/debug/xtask build debug

# 3. Test with a Wado program
cd /home/user/wado
cargo run --bin wado -- compile -o /tmp/hello.wasm example/hello.wado
rm -rf /tmp/hello-jco
# --no-wasi-shim disables jco's automatic WASI->preview3-shim rewrite, so the
# --map entries to the Wado shims in example/jco-shim/ take effect.
node vendor/jco/packages/jco/src/jco.js transpile /tmp/hello.wasm \
  -o /tmp/hello-jco \
  --no-wasi-shim \
  --map "wasi:cli/*=$(pwd)/example/jco-shim/cli.js#*"
echo '{"type": "module"}' > /tmp/hello-jco/package.json
node --experimental-wasm-jspi -e "
  import('/tmp/hello-jco/hello.js')
    .then(async m => { await m.run(); process.exit(0); })
    .catch(e => { console.error(e); process.exit(1); });
"
```

Or use mise tasks:

```sh
mise run build-jco
mise run hello
mise run jco-transpile example/hello.wasm
```

## Debugging Runtime Errors

jco-transpiled code is a single large JS file (~185KB for hello). Key functions to know:

| Wasm canonical built-in | jco JS function        | Trampoline                      |
| ----------------------- | ---------------------- | ------------------------------- |
| `stream.new`            | `streamNew()`          | `trampoline8`                   |
| `stream.write`          | `streamWrite()`        | `trampoline0` (JSPI Suspending) |
| `stream.drop-writable`  | `streamDropWritable()` | `trampoline7`                   |
| `canon lower (async)`   | `_lowerImport()`       | `trampoline6` (JSPI Suspending) |
| `task.return`           | `taskReturn()`         | `trampoline1`                   |
| `waitable-set.new`      | `waitableSetNew()`     | `trampoline4`                   |
| `waitable-set.wait`     | `waitableSetWait()`    | `trampoline5` (JSPI Suspending) |
| `waitable.join`         | `waitableJoin()`       | `trampoline2`                   |
| `subtask.drop`          | `subtaskDrop()`        | `trampoline9`                   |

Trampoline indices may vary per component. Check the bottom of the transpiled JS for `const trampoline{N} = ...` assignments.

### Debug technique: inject logging into transpiled JS

```sh
node -e "
const fs = require('fs');
let code = fs.readFileSync('/tmp/hello-jco/hello.js', 'utf-8');
code = code.replace(
  'function subtaskDrop(componentIdx, subtaskWaitableRep) {',
  'function subtaskDrop(componentIdx, subtaskWaitableRep) { console.error(\"subtaskDrop:\", { componentIdx, subtaskWaitableRep });'
);
fs.writeFileSync('/tmp/hello-jco/hello.js', code);
"
```

### Debug technique: catch unhandled rejections

jco's async machinery swallows errors via unhandled Promise rejections. Always add:

```js
process.on('unhandledRejection', e => { console.error('UNHANDLED:', e); process.exit(1); });
```

### Debug technique: enable JCO_DEBUG

```sh
JCO_DEBUG=1 node --experimental-wasm-jspi run.mjs
```

### Common error patterns

| Error                                               | Likely cause                                                     |
| --------------------------------------------------- | ---------------------------------------------------------------- |
| `rec group usage requires 'gc' proposal`            | `WasmFeatures` in `lib.rs` or `core.rs` missing `GC` / `WASM3`   |
| `wide arithmetic support is not enabled`            | Missing `WasmFeatures::WIDE_ARITHMETIC`                          |
| `X is not defined` (runtime)                        | Missing intrinsic dependency in `intrinsics/mod.rs`              |
| `cannot drop subtask before resolve is delivered`   | Subtask lifecycle issue — `deliverResolve()` not called          |
| `task.X is not a function`                          | Method not defined on `AsyncTask` class (jco implementation gap) |
| `invalid variant discriminant for expected`         | Async export returns `undefined` instead of result discriminant  |
| Hang / timeout                                      | JSPI Suspending missing on a trampoline, or rendezvous deadlock  |
| `symbolRscRep is not defined`                       | Missing `SymbolResourceRep` intrinsic dependency                 |
| `FUTURES is not defined`                            | Missing `GlobalFutureMap` intrinsic dependency for FutureDrop    |
| `invalid resource rep during remove, (cannot be 0)` | futureDropReadable called with handle 0 (stream hook bypass)     |

## Saving Patches

The submodule stays pinned at a clean upstream commit (currently `b1f93c27`);
the patches are never committed into the submodule, only saved to
`vendor/jco.patch`. After editing the jco Rust source, regenerate the patch
from the submodule's `HEAD` (the pinned upstream commit):

```sh
cd vendor/jco
git diff HEAD -- crates/js-component-bindgen/src/ > /home/user/wado/vendor/jco.patch
```

Then commit the patch file (and, when bumping upstream, the submodule ref):

```sh
cd /home/user/wado
git add vendor/jco.patch     # + `git add vendor/jco` only when bumping the pin
git commit -m "Update jco patches: <description>"
```

To bump the pinned upstream commit, `git checkout <commit>` inside `vendor/jco`,
`git add vendor/jco` in the parent, then re-apply/regenerate the patch.

## Key Source Locations in jco

| File                                                            | Purpose                                   |
| --------------------------------------------------------------- | ----------------------------------------- |
| `crates/js-component-bindgen/src/lib.rs`                        | Top-level `WasmFeatures` validator config |
| `crates/js-component-bindgen/src/core.rs`                       | Core module validation features           |
| `crates/js-component-bindgen/src/transpile_bindgen.rs`          | Trampoline generation (JSPI wrapping)     |
| `crates/js-component-bindgen/src/function_bindgen.rs`           | Function call/result codegen              |
| `crates/js-component-bindgen/src/intrinsics/mod.rs`             | Intrinsic dependency resolution           |
| `crates/js-component-bindgen/src/intrinsics/lower.rs`           | Value lowering (Result, Variant, etc.)    |
| `crates/js-component-bindgen/src/intrinsics/p3/async_stream.rs` | Stream classes and operations             |
| `crates/js-component-bindgen/src/intrinsics/p3/async_future.rs` | Future classes and drop operations        |
| `crates/js-component-bindgen/src/intrinsics/p3/waitable.rs`     | WaitableSet wait/poll                     |
| `crates/js-component-bindgen/src/intrinsics/p3/async_task.rs`   | AsyncTask class                           |

## WASI P3 Shims

`example/jco-shim/` contains minimal shims for Node.js:

- `cli.js` — stdout/stderr via `_jcoStreamWriteHook` (bypasses rendezvous)
- `random.js` — `wasi:random` via Node.js crypto
- `clocks.js` — `wasi:clocks` via `process.hrtime` / `Date.now`

The `--map` flag maps WASI interfaces to shim files during transpilation (with
`--no-wasi-shim` so the maps win over jco's built-in shims).

`cli.js`'s `writeViaStream` returns a `Promise` because `write-via-stream` is an
async WASI function: jco lowers its result as a future and expects the host to
return a Promise/Thenable (a sync return triggers `unrecognized future object`).

## Current Patches (on top of upstream main)

Saved in `vendor/jco.patch`, applied to the working tree before building. The
pinned upstream commit already provides a table-based `FutureDropReadable/Writable`
implementation, so only these three patches remain:

1. **Stream write hook** (`async_stream.rs`): `globalThis._jcoStreamWriteHook` delivers stream data directly from linear memory, bypassing the rendezvous mechanism.
2. **Missing intrinsic dependencies** (`mod.rs`): `StreamNewFromLift` → `SymbolResourceRep`; `FutureDropReadable/Writable` → `GlobalFutureMap`, `FutureEndClass`, `FutureReadableEndClass`, `FutureWritableEndClass`, `GetOrCreateAsyncState` (otherwise the stdout write path references undefined future-end classes, e.g. `FutureReadableEnd is not defined`).
3. **Async export void return** (`function_bindgen.rs`): when a JSPI-wrapped async export returns `undefined` (result delivered via `task.return`), fall back to `task.completionPromise()` instead of lifting `undefined` (which throws `invalid variant discriminant for expected`).
