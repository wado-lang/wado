---
name: jco
description: Debug and patch jco (JS Component Tooling) for Wado wasm transpilation. Use when jco transpile or jco-transpiled code fails at runtime.
---

# jco Patching Workflow

jco (`vendor/jco`) is bytecodealliance's tool for transpiling Wasm Components to JavaScript. Wado uses a patched fork because upstream jco has incomplete support for P3 async stream delivery and JSPI integration.

## Setup

```sh
git submodule update --init vendor/jco
cd vendor/jco
PUPPETEER_SKIP_DOWNLOAD=1 npm install --ignore-scripts
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
node vendor/jco/packages/jco/src/jco.js transpile /tmp/hello.wasm \
  -o /tmp/hello-jco \
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

| Error                                             | Likely cause                                                     |
| ------------------------------------------------- | ---------------------------------------------------------------- |
| `rec group usage requires 'gc' proposal`          | `WasmFeatures` in `lib.rs` or `core.rs` missing `GC` / `WASM3`  |
| `wide arithmetic support is not enabled`          | Missing `WasmFeatures::WIDE_ARITHMETIC`                          |
| `X is not defined` (runtime)                      | Missing intrinsic dependency in `intrinsics/mod.rs`              |
| `cannot drop subtask before resolve is delivered`  | Subtask lifecycle issue — `deliverResolve()` not called          |
| `task.X is not a function`                        | Method not defined on `AsyncTask` class (jco implementation gap) |
| `invalid variant discriminant for expected`       | Async export returns `undefined` instead of result discriminant  |
| Hang / timeout                                    | JSPI Suspending missing on a trampoline, or rendezvous deadlock  |
| `symbolRscRep is not defined`                     | Missing `SymbolResourceRep` intrinsic dependency                 |
| `FUTURES is not defined`                          | Missing `GlobalFutureMap` intrinsic dependency for FutureDrop    |
| `invalid resource rep during remove, (cannot be 0)` | futureDropReadable called with handle 0 (stream hook bypass)   |

## Saving Patches

After making changes in `vendor/jco`, save the patch relative to upstream:

```sh
cd vendor/jco
# Find upstream base (the commit before our patches)
git log --oneline | head -5
# Save patch from upstream base
git diff <upstream-commit> -- . > /home/user/wado/vendor/jco.patch
```

Then commit both the submodule ref update and the patch file:

```sh
cd /home/user/wado
git add vendor/jco vendor/jco.patch
git commit -m "Update jco patches: <description>"
```

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

The `--map` flag maps WASI interfaces to shim files during transpilation.

## Current Patches (on top of upstream main)

1. **Stream write hook** (`async_stream.rs`): `globalThis._jcoStreamWriteHook` delivers stream data directly from linear memory, bypassing the rendezvous mechanism.
2. **Missing intrinsic dependencies** (`mod.rs`): `StreamNewFromLift` → `SymbolResourceRep`; `FutureDropReadable/Writable` → `GlobalFutureMap`, `FutureEndClass`, etc.
3. **futureDropReadable/Writable** (`async_future.rs`, `transpile_bindgen.rs`): Pass `componentIdx` as bound parameter; accept single `futureEndIdx` from wasm; tolerate handle 0 (stream hook bypass).
4. **Async export void return** (`function_bindgen.rs`): Fall back to `task.completionPromise()` when JSPI returns `undefined`.
