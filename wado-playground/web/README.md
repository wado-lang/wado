# Wado Browser Playground

Compile **and** run Wado entirely in the browser — no server round-trip. The
Wado compiler and jco's component transpiler both run as WebAssembly in the
page.

## Pipeline

```
Wado source
   │  ① wado-playground.wasm   (the Wado compiler, built for wasm32-unknown-unknown)
   ▼
Component Model Wasm
   │  ② jco transpileBytes      (js-component-bindgen, itself Wasm, bundled for the browser)
   ▼
JS module + inlined core Wasm
   │  ③ dynamic import() + WASI browser shims + JSPI
   ▼
program output
```

All three stages run client-side. `playground.js` orchestrates them; `index.html`
is the UI.

- **①** `wado-playground` exposes a tiny C ABI (`wado_alloc` / `wado_compile`)
  over `compile_with_options` + `InMemoryCompilerHost`. The stdlib is embedded in
  the compiler (`include_str!`), so a single-file program needs no host I/O.
  Compiled with `no-wide-arithmetic` — V8 has no wide-arithmetic proposal.
- **②** `@bytecodealliance/jco-transpile`'s `transpileBytes` is bundled with
  `build-jco.mjs` (esbuild + `node:*` stubs). jco's bindgen already loads its
  `.core.wasm` via `fetch` in the browser.
- **③** `shims/{cli,clocks,random}.js` implement the WASI P3 imports against
  browser APIs (`console`/DOM, `performance.now`, `crypto.getRandomValues`). The
  released-jco P3 gaps (future-end classes, stdout write hook) are re-injected by
  the same post-process as the Node pipeline.

## Requirements

- A **JSPI-capable browser**: Chromium/Chrome **137+** (stable JSPI). Both the
  transpiled program *and* jco's own bindgen use `WebAssembly.Suspending`.
- WasmGC (Chrome 119+, Firefox 120+) — Wado uses GC.

## Build

```sh
mise run playground-web-build      # compiles the wasm, bundles jco, stages assets
```

This produces the git-ignored artifacts (`wado-playground.wasm`, `vendor/*`).

## Run

```sh
mise run playground-web-serve      # http://127.0.0.1:8088
```

Open the URL in a JSPI-capable browser and click **Run**.

## Limitations

- **Compute + stdout/stderr only.** Programs that read files (`wasi:filesystem` /
  preopens) hang on a known jco async read-stream gap — same blocker as the Node
  jco pipeline (see `.claude/skills/jco`).
- `wasi:http/service` programs don't transpile through released jco.
- Multi-file programs (relative `use "./x.wado"`) would need the host to feed
  those sources into `InMemoryCompilerHost`; the current UI compiles one file.
