# WEP: WIT Bundling in Component Binaries

## Context

### The Component Model Distribution Problem

The WebAssembly Component Model (CM) introduces a shared-nothing linking model where components interact through well-defined interfaces specified in WIT (WebAssembly Interface Type). When a component is published for reuse, consumers need two things:

1. **The component binary** (`.wasm`) — the executable code
2. **The WIT interface definition** — the type-level contract describing the component's imports and exports

This raises a fundamental distribution question: how should these two artifacts be delivered?

### WIT as an Interface Description Language

WIT serves the same role as C header files or Protocol Buffer `.proto` files: it describes the interface contract without containing implementation. A WIT package declares:

- **Interfaces**: named collections of types and function signatures
- **Worlds**: concrete component shapes (which interfaces a component imports and exports)
- **Types**: records, variants, enums, flags, resources, and other composite types

For example, a Wado library that exports a `parse` function would have a corresponding WIT world describing the function's signature and types, which consumers need in order to generate bindings in their language.

### Current State of the Ecosystem

The CM specification itself does not prescribe how WIT is distributed. The spec defines a binary format for components (Binary.md) and a text format for interfaces (WIT.md), but treats them as independent artifacts. In practice, the ecosystem has converged on two distribution mechanisms:

**1. Separate WIT files**

WIT packages are published as standalone `.wit` files to registries (e.g., warg registries). Consumers download the WIT package separately from the component binary. This is the model described in the CM spec's WIT.md:

> "A WIT package represents a unit of distribution that can be published to a registry."

This works for standard interfaces like `wasi:http` or `wasi:cli` where the interface definition is shared across many implementations. However, for application-specific libraries, it creates a coordination problem: the consumer must find and download the correct version of the WIT package that matches the component binary.

**2. Embedded WIT via custom sections**

The reference toolchain (`wasm-tools`, `wit-bindgen`) has established a convention of embedding WIT metadata into Wasm binaries using a custom section named `"component-type"`. This section contains a component binary that encodes the world's type information, plus a nested `"wit-component-encoding"` section specifying the string encoding (UTF-8, UTF-16, or CompactUTF-16).

The toolchain workflow using this approach:

1. **wit-bindgen** generates language bindings from WIT and embeds the WIT metadata into the core module via `"component-type"` custom sections
2. Core WebAssembly build tools (linker, etc.) propagate custom sections opaquely
3. **wasm-tools component new** reads the embedded WIT metadata to create a proper component

As the CM Linking.md spec describes:

> "WIT type information that is needed later to build a component can be stored in a custom section that will be opaquely propagated to the component-specific tooling by the intervening Core WebAssembly build steps."

This approach is not part of the formal CM specification but is a de facto standard in the reference tooling. The `"component-type"` section name, the binary encoding format, and the version byte (`0x04`) are all defined by the `wit-component` crate in `wasm-tools`.

### Why Bundling Matters for Wado

Wado's package system (WEP: Package Manifest) defines library packages that export functions at the CM boundary:

```toml
[package]
namespace = "myorg"
name = "markdown"
lib = "src/lib.wado"
```

When this package is compiled to a `.wasm` component and published to a registry, consumers in any CM-compatible language need the WIT interface to generate bindings. Without bundled WIT, consumers face these problems:

1. **Discovery**: They must separately locate the WIT package that matches the component binary version
2. **Synchronization**: The WIT and binary can drift out of sync across versions
3. **Self-description**: A standalone `.wasm` file cannot describe its own interface; tools like `wasm-tools component wit` cannot extract interface information

Wado already generates WIT internally during compilation (the CM binding synthesis phase derives the component's type structure). Bundling this generated WIT into the output binary is a natural extension.

### How Other Toolchains Handle This

| Toolchain                            | Approach                                                                                                 |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------- |
| Rust (wit-bindgen + cargo-component) | Embeds WIT in `"component-type"` custom sections; `wasm-tools component new` creates the final component |
| C/C++ (wit-bindgen-c)                | Same `"component-type"` embedding; opaquely propagated through clang/wasm-ld                             |
| Go (wit-bindgen-go)                  | Same approach via `wasm-tools`                                                                           |
| JavaScript (jco)                     | Can extract WIT from components via `"component-type"` sections                                          |

Every major CM toolchain uses the `"component-type"` custom section convention. This is the interoperable standard.

## Decision

Wado bundles WIT metadata into compiled component binaries by default, using the `"component-type"` custom section convention established by the reference toolchain.

### What Is Embedded

The compiler emits a `"component-type"` custom section containing:

1. A component binary that encodes the world's type definition (all imports and exports with their full type signatures)
2. A `"wit-component-encoding"` nested custom section with the version byte and string encoding (UTF-8, matching Wado's native string encoding)
3. A `"producers"` custom section identifying the Wado compiler

This is the same format produced by `wit-component::metadata::encode()` in `wasm-tools`, ensuring full interoperability with the existing toolchain ecosystem.

### When It Applies

| Output Type                            | WIT Bundled   | Rationale                                                                               |
| -------------------------------------- | ------------- | --------------------------------------------------------------------------------------- |
| Library package (`lib` in `wado.toml`) | Yes (default) | Consumers need the interface to generate bindings                                       |
| Command (`wasi:cli/command`)           | Yes (default) | Self-describing binaries; tools can inspect the component                               |
| Service (`wasi:http/service`)          | Yes (default) | Same as command                                                                         |
| `-Os` (strip symbols)                  | Yes           | WIT is interface metadata, not debug symbols; stripping it would break interoperability |

### CLI Control

```sh
wado compile file.wado                    # WIT bundled (default)
wado compile --no-wit file.wado           # WIT omitted (smaller binary)
```

The `--no-wit` flag is an escape hatch for size-sensitive deployments where the consumer already has the WIT definition through other means.

### Extraction and Verification

Bundled WIT can be extracted using standard tooling:

```sh
wasm-tools component wit output.wasm      # extract WIT from a Wado-compiled component
```

This enables any CM-compatible toolchain to consume Wado libraries without Wado-specific tooling.

### Relationship to Wado-to-Wado Optimization

As described in WEP: Package Manifest, when both producer and consumer are Wado, the compiler can skip the CM canonical ABI and share Wasm GC types directly. WIT bundling does not affect this optimization — it provides the interface metadata that non-Wado consumers need, while Wado consumers can use the embedded Wado provider metadata for the fast path.

| Consumer                     | Uses WIT?                                 | Uses Wado metadata?   |
| ---------------------------- | ----------------------------------------- | --------------------- |
| Non-Wado (Rust, C, JS, etc.) | Yes — generates bindings from bundled WIT | No                    |
| Wado (source dependency)     | No — compiles from source                 | No                    |
| Wado (binary dependency)     | Fallback — if no Wado metadata            | Yes — GC type sharing |
