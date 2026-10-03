# WEP: WebAssembly Module Import Support

## Context

Wado aims to be a "Wasm only" language, maintaining zero abstraction over WebAssembly. To achieve this goal and enable interoperability with the broader Wasm ecosystem, we need a mechanism to import and integrate existing WebAssembly modules directly into Wado programs.

This capability is essential for:

1. Standard library implementation: integrating deterministic math functions (see [Deterministic Math Library (libm) Integration](./wep-2026-01-10-deterministic-libm.md)).
2. Ecosystem integration: using existing Wasm libraries (cryptography, parsers, etc.).
3. Multi-language projects: composing modules written in different languages (Rust, C, AssemblyScript, etc.).

## Decision

A core Wasm module is imported as an asset with `with { type: "wat" | "wasm" }`. Component Model components are the subject of [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md); this WEP covers core modules only.

### Syntax

```wado
// Named imports: each name is a function exported by the module, typed by
// its export signature.
use { f64_sin, f64_cos } from "./libm.wat" with { type: "wat" };
let s = f64_sin(1.5);

// Wildcard import: the module is loaded, but no name is bound.
use _ from "./helpers.wasm" with { type: "wasm" };
```

`with { type: "wat" }` and `with { type: "wasm" }` are the only forms recognised as wasm-asset imports. Without `type`, the extension decides nothing: the file is read by its generator, or as Wado source ([How an Import Is Read](./spec-modules.md#how-an-import-is-read)).

### Semantics

1. Each function export of the module becomes a Wado function of the same name, so a named import resolves as an import of any Wado module does.
2. A call to one is a call into that asset, whatever its name. An export named like an intrinsic (`cold_path`, `select`) or like another asset's export (`f64_sin`) is still that asset's function.
3. The asset is embedded in the resulting component, sharing the program's memory and pruned to the exports the program calls.

### Restrictions

- A module may import only `env.memory`, and define at most one memory.
- A module may not contain a `start` section.
- A function export takes only `i32`, `i64`, `f32`, `f64` and `v128` parameters, and returns at most one result.
- A function export may not alias an imported function.

### The bundled libm

The bundled libm was the motivating use case. It is an ordinary asset, `core:libm.wat`, which `core:prelude` imports like any other. Its exports carry the names the prelude calls them by, so `f64::sin(x)` reaches libm with nothing in between.
