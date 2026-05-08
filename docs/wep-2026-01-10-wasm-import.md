# WEP: WebAssembly Module Import Support

## Context

Wado aims to be a "Wasm only" language, maintaining zero abstraction over WebAssembly. To achieve this goal and enable interoperability with the broader Wasm ecosystem, we need a mechanism to import and integrate existing WebAssembly modules directly into Wado programs.

This capability is essential for:

1. **Standard library implementation**: Integrating deterministic math functions (see WEP-2026-01-10-deterministic-libm)
2. **Ecosystem integration**: Using existing Wasm libraries (cryptography, parsers, etc.)
3. **Multi-language projects**: Composing modules written in different languages (Rust, C, AssemblyScript, etc.)

### Current State

Wado supports imports from:

- `.wado` modules (local files, integrated at IR level during compilation)
- `core:*` namespace (core library, written in Wado)
- `wasi:*` namespace (WASI interfaces, mapped to WIT)
- `https:` URLs (remote modules)

Phase 1 of this proposal adds **core-wasm asset imports** via `use _ from "<path>" with { type: "wat" | "wasm" };`. Component Model imports are deferred to a later phase.

## Phase 1 (delivered)

Phase 1 covers core wasm imports — the minimum needed to migrate `lib/core/libm.wat` from a special-cased "bundled" path to a regular asset import.

### Syntax

Phase 1 supports both wildcard and named imports:

```wado
// Named imports — the loader synthesises Wado bindings from the wasm
// module's export signatures, so each name resolves to a normal Wado
// function with the right type.
use { libm_sin, libm_cos } from "./libm.wat" with { type: "wat" };
let s = libm_sin(1.5);

// Wildcard imports — same loading machinery, no symbols are bound. Useful
// when the asset is referenced indirectly through `pub use` re-exports.
use _ from "./helpers.wasm" with { type: "wasm" };
```

`with { type: "wat" }` and `with { type: "wasm" }` are the only forms recognised as wasm-asset imports. Without the `with` clause, `.wat` / `.wasm` paths fall through to the regular import resolution (which rejects non-`.wado` schemas via the existing Kiln-missing-with diagnostic).

### Semantics

1. The loader fetches the asset bytes (stdlib lookup for `core:*.wat`, `host.load_source` for user paths), runs `wat::parse_bytes` if `kind == Wat`, and validates the result.
2. The bytes are cached in `LoadResult::wasm_assets` keyed by the canonical namespace string `wasm:<canonical-path>` (e.g. `wasm:core:libm.wat`). Each `WasmAsset` also carries the function-export signatures extracted via `wasmparser`.
3. The loader synthesises a Wado source string from those signatures — one `pub fn name(...) -> ret;` declaration per export, each tagged `#[canonical("wasm:<path>", "<export>")]` — and runs it through the regular parse/bind/desugar pipeline. The resulting AST module is registered under `ModuleSource::Wasm { path, kind }`, so named imports (`use { libm_sin } from "./libm.wat" ...`) resolve through the same path as imports of any other Wado module.
4. `BuiltinRegistry::register_wasm_module` folds the synthesised declarations into the registry alongside `core:builtin`'s entries, and `FunctionRef::builtin_name` + DCE's `is_builtin_func` recognise `ModuleSource::Wasm` so calls into a wat asset's exports lower through the same `TirImport` path as `core:builtin` declarations.
5. Codegen looks up each asset by namespace (post-DCE), transforms the module to import its memory from `env.memory`, prunes to the union of exports actually referenced, and embeds it in the resulting component.

### Phase 1 limitations (enforced)

- **Imports.** A wasm asset may import only `env.memory`. Any other import (`env.foo`, multiple memories, non-memory imports) is a compile-time error.
- **Start sections.** Wasm assets may not contain a `start` section. (Side-effecting init at instantiation time is not supported in Phase 1.)
- **Single memory.** At most one memory definition.
- **Export shape.** Each function export must use only the core wasm subset `{i32, i64, f32, f64, v128}` for parameters and at most one result. Reference-typed parameters and multi-return are rejected up-front with a pointed diagnostic.
- **Re-exported imports.** A wasm export that aliases an imported function is rejected; only module-defined functions can be exposed to Wado.
- **Origin detection (`@custom "wado-compiler"` marker).** Not used in Phase 1; all assets go through the core-linking path.
- **WIT type extraction.** Not used in Phase 1; types come from `#[canonical(...)]` declarations on Wado-side functions, not from the wasm module itself.

### Migration of `lib/core/libm.wat`

The bundled libm path was the motivating use case. Phase 1 retires the previous special "bundled" namespace and the `core:builtin` libm declarations:

- `lib/builtins/wado-bundled-libm.wat` → `lib/core/libm.wat`
- `wado-compiler/src/bundled.rs` → folded into `stdlib.rs` (`get_stdlib_wasm_asset` returns the bytes by canonical path)
- `core:prelude/primitive.wado` name-imports the libm exports directly from `../libm.wat`, renaming each onto its Wado-side identifier (`libm_sin as f64_sin`, …, `libm_log as f64_ln`, etc.) at the import site, and calls them as ordinary functions in place of the previous `builtin::f64_sin(x)` style. There is no intermediate stdlib module — `primitive.wado` is the only consumer, so a `core:libm` re-exporter would be pure indirection.
- The libm function declarations have been removed from `lib/core/builtin.wado`.
- `embed_bundled_modules` in `codegen/component.rs` is generalised into `embed_imported_wasm_modules`, driven by post-DCE imports grouped by namespace.

There is no behaviour change at the user level — `f64::sin(x)` still routes through the same libm export. The wasm-import path is now the only mechanism the codegen uses for both stdlib and user wasm assets.

## Phase 2: Component Model imports

Phase 2 — handling external `.wasm` components that arrive with their own WIT
— is the consumer side of [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md).
The end-to-end design, current state, open questions, and roadmap for that
work live in [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md).

This WEP retains only the Phase 1 (core wasm asset import) decisions.
