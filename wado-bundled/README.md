# wado-bundled

Bundled libraries for the Wado programming language, compiled to WebAssembly.

## Overview

This crate provides bundled functionality that is compiled to WebAssembly and embedded into the Wado compiler. The primary goal is to provide deterministic, portable implementations of common operations across all platforms.

## Current Implementation: ryu (Float Formatting)

The first bundled library is [ryu](https://github.com/dtolnay/ryu), a fast and deterministic float-to-string conversion library.

### Features

- ✅ Deterministic float formatting across all platforms
- ✅ Fast conversion using the ryu algorithm
- ✅ Support for both `f32` and `f64`
- ✅ Minimal binary size overhead
- ✅ Pure Rust implementation

### API

```rust
pub fn format_f64(value: f64) -> String;
pub fn format_f32(value: f32) -> String;
```

### Example

```rust
use wado_bundled::{format_f64, format_f32};

assert_eq!(format_f64(3.14159), "3.14159");
assert_eq!(format_f32(3.14_f32), "3.14");
assert_eq!(format_f64(core::f64::consts::PI), "3.141592653589793");
```

## Building

### Standard Build (for testing)

```bash
cargo test -p wado-bundled
```

### WebAssembly Build

```bash
cargo build -p wado-bundled --target wasm32-unknown-unknown --release
```

The compiled Wasm file will be located at:

```
target/wasm32-unknown-unknown/release/wado_bundled.wasm
```

## Architecture

### Current Status (Minimal Verification)

The current implementation is a minimal verification that demonstrates:

1. Integration of the ryu crate
2. Compilation to WebAssembly
3. Deterministic float-to-string conversion

### File Structure

```
wado-bundled/
├── Cargo.toml           # Crate configuration
├── README.md            # This file
└── src/
    └── lib.rs           # Float formatting implementation

wado-compiler/embedded/
└── ryu.wit              # WIT interface definition (for future use)
```

### WIT Interface

The WIT interface is defined in `wado-compiler/embedded/ryu.wit`:

```wit
package wado:ryu@1.0.0;

interface format {
    format-f64: func(value: f64) -> string;
    format-f32: func(value: f32) -> string;
}

world ryu {
    export format;
}
```

## Future Work

### Phase 1: Component Model Integration (TODO)

Convert the current Core Wasm module to a Component Model component:

- [ ] Use `cargo-component` or `wit-bindgen` for proper Component Model exports
- [ ] Convert Core Wasm to Component using `wasm-tools component new`
- [ ] Implement proper canonical ABI (cabi_realloc, etc.)

### Phase 2: Compiler Integration (TODO)

Integrate the bundled Wasm into wado-compiler:

- [ ] Embed the Wasm component in the compiler binary
- [ ] Implement `builtin:ryu` namespace resolution
- [ ] Link bundled Wasm into final output

### Phase 3: Wado API (TODO)

Create Wado wrapper API in the standard library:

- [ ] Implement `core:fmt` module
- [ ] Expose float formatting functions
- [ ] Add string interpolation support

Example usage (future):

```wado
use {format_f64} from "core:fmt";

let pi = 3.14159;
let s = format_f64(pi);  // "3.14159"

// Or with string interpolation
let message = `Pi is approximately {pi}`;
```

### Phase 4: Additional Libraries (Future)

Following the same pattern, add other bundled libraries:

- [ ] `libm` - Deterministic math functions (see [ADR](../docs/adr-2026-01-10-deterministic-libm.md))
- [ ] Date/time formatting
- [ ] Regular expressions
- [ ] Compression algorithms

## Design Rationale

### Why Bundle Libraries?

Bundling libraries provides several benefits:

1. **Determinism**: Same results across all platforms and runtimes
2. **Portability**: No dependency on host implementations
3. **Version Control**: Pin library versions for reproducible builds
4. **Zero Configuration**: Users don't need to install external dependencies

### Why ryu?

For float-to-string conversion, ryu offers:

- Deterministic output (same value → same string, always)
- Fast performance (faster than standard library implementations)
- Small binary size overhead (~18KB for Core Wasm)
- Pure Rust implementation (easy Wasm compilation)
- Well-tested and widely used

### Comparison with Alternatives

| Approach              | Determinism | Performance | Binary Size | Portability |
| --------------------- | ----------- | ----------- | ----------- | ----------- |
| **Bundled ryu**       | ✅ Yes      | ✅ Fast     | ✅ Small    | ✅ Perfect  |
| Host sprintf/dtoa     | ❌ No       | ⚠️ Varies   | ✅ Zero     | ❌ Platform |
| JavaScript toString() | ❌ No       | ⚠️ Varies   | ✅ Zero     | ❌ Runtime  |
| Manual implementation | ⚠️ Complex  | ❌ Slow     | ⚠️ Large    | ✅ Good     |

## References

- [ryu repository](https://github.com/dtolnay/ryu)
- [ryu paper: "Ryū: fast float-to-string conversion"](https://dl.acm.org/doi/10.1145/3192366.3192369)
- [ADR: Deterministic libm](../docs/adr-2026-01-10-deterministic-libm.md)
- [Component Model specification](https://github.com/WebAssembly/component-model)
