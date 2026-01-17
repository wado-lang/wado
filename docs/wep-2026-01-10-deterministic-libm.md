# ARD: Deterministic Math Library (libm) Integration

## Context

### Problem Statement

Floating-point math functions (`sin`, `cos`, `log`, `exp`, etc.) are implementation-dependent in traditional programming environments. As documented in [this analysis](https://zenn.dev/mod_poppo/articles/floating-point-portability), the same code can produce different results across:

- Different C standard library implementations (glibc, musl, MSVC CRT)
- Different CPU architectures (x86, ARM, RISC-V)
- Different operating systems (Linux, macOS, Windows)

This non-determinism stems from:

1. **Implementation freedom**: IEEE 754 only specifies basic arithmetic (`+`, `-`, `*`, `/`, `sqrt`), not transcendental functions
2. **Approximation algorithms**: Different libraries use different polynomial approximations
3. **Platform-specific optimizations**: CPU-specific instructions with varying precision
4. **Compiler optimizations**: Constant folding vs runtime evaluation can yield different results

### WebAssembly's Partial Solution

WebAssembly provides **deterministic basic arithmetic**:

- ✅ IEEE 754-2019 compliant for `+`, `-`, `*`, `/`, `sqrt`, `min`, `max`, `abs`, etc.
- ✅ Fixed rounding mode (round-to-nearest, ties-to-even)
- ✅ No intermediate precision variation (unlike x87 80-bit extended precision)

However, **complex math functions remain host-dependent**:

- ❌ If delegated to WASI host functions, results vary by platform
- ❌ No standard WIT interface for math functions in WASI P3

### Design Philosophy Alignment

Wado's core principles demand determinism:

1. **"Wasm only"**: Zero abstraction over WebAssembly
2. **Portability**: Same code, same results, everywhere
3. **Explicit imports**: All dependencies are clear and controlled

**Goal**: Math functions should be as deterministic as basic arithmetic.

## Decision

**Bundle a fixed, deterministic math library compiled to WebAssembly** rather than delegating to host implementations.

### Choice: Rust `libm` Crate

**Selected Library**: [Rust `libm`](https://github.com/rust-lang/libm)
**License**: MIT OR Apache-2.0 (dual license)
**Version**: 0.2.x (track upstream stable releases)

**Rationale**:

| Criterion             | Rust `libm`                                         | musl libm               | fdlibm                     |
| --------------------- | --------------------------------------------------- | ----------------------- | -------------------------- |
| **License**           | ✅ MIT/Apache-2.0                                   | ✅ MIT                  | ⚠️ Permissive (verify)     |
| **Language**          | ✅ Pure Rust                                        | ❌ C (requires clang)   | ❌ C                       |
| **Wasm compilation**  | ✅ Native (cargo → wasm32-unknown-unknown)          | ⚠️ Requires wasi-sdk    | ⚠️ Requires wasi-sdk       |
| **no_std support**    | ✅ Yes (no libc/WASI dependencies)                  | ❌ No                   | ❌ No                      |
| **Upstream**          | ✅ Based on musl, actively maintained               | ✅ High quality         | ⚠️ Less active maintenance |
| **Rust integration**  | ✅ Native Cargo dependency                          | ⚠️ FFI required         | ⚠️ FFI required            |
| **Production usage**  | ✅ Used in Rust `no_std` ecosystem (embedded, Wasm) | ✅ Alpine Linux default | ✅ Java StrictMath, V8     |
| **Compiler affinity** | ✅ Wado compiler is Rust-based                      | ⚠️ Neutral              | ⚠️ Neutral                 |

### Integration Architecture

```
wado-compiler/
├── embedded/
│   ├── libm.wasm          # Pre-built Rust libm component
│   └── libm.wit           # WIT interface definition
├── lib/core/
│   └── math.wado          # Wado wrapper API
└── scripts/
    └── build-libm.sh      # Build script for libm.wasm
```

**Build Process**:

```bash
# scripts/build-libm.sh

# 1. Create Rust wrapper crate
cargo new --lib wado-libm
cd wado-libm

# 2. Add dependency
cat >> Cargo.toml <<EOF
[dependencies]
libm = "0.2"

[lib]
crate-type = ["cdylib"]
EOF

# 3. Implement WIT-compatible exports
# src/lib.rs - export C-compatible functions

# 4. Compile to Core Wasm
cargo build --target wasm32-unknown-unknown --release

# 5. Convert to Component Model
wasm-tools component new \
    target/wasm32-unknown-unknown/release/wado_libm.wasm \
    --wit ../embedded/libm.wit \
    -o ../embedded/libm.wasm
```

**WIT Definition** (`embedded/libm.wit`):

```wit
package wado:libm@1.0.0;

interface math {
    // Trigonometric functions
    sin: func(x: f64) -> f64;
    cos: func(x: f64) -> f64;
    tan: func(x: f64) -> f64;
    asin: func(x: f64) -> f64;
    acos: func(x: f64) -> f64;
    atan: func(x: f64) -> f64;
    atan2: func(y: f64, x: f64) -> f64;

    // Hyperbolic functions
    sinh: func(x: f64) -> f64;
    cosh: func(x: f64) -> f64;
    tanh: func(x: f64) -> f64;

    // Exponential and logarithmic
    exp: func(x: f64) -> f64;
    exp2: func(x: f64) -> f64;
    log: func(x: f64) -> f64;
    log2: func(x: f64) -> f64;
    log10: func(x: f64) -> f64;

    // Power functions
    pow: func(x: f64, y: f64) -> f64;
    sqrt: func(x: f64) -> f64;  // Note: Also available as Wasm instruction
    cbrt: func(x: f64) -> f64;

    // Rounding and remainder
    ceil: func(x: f64) -> f64;
    floor: func(x: f64) -> f64;
    trunc: func(x: f64) -> f64;
    round: func(x: f64) -> f64;

    // Other
    fabs: func(x: f64) -> f64;
    fmod: func(x: f64, y: f64) -> f64;

    // f32 variants (selected subset)
    sinf: func(x: f32) -> f32;
    cosf: func(x: f32) -> f32;
    sqrtf: func(x: f32) -> f32;
    // ... (add as needed)
}

world libm {
    export math;
}
```

**Wado API** (`lib/core/math.wado`):

```wado
// Imports the bundled libm.wasm using the new Wasm import feature
use {
    sin as libm_sin,
    cos as libm_cos,
    tan as libm_tan,
    sqrt as libm_sqrt,
    log as libm_log,
    exp as libm_exp,
    pow as libm_pow,
    // ... other functions
} from "builtin:libm" with { type: "wasm" };

// Public API with Wado naming conventions
pub fn sin(x: f64) -> f64 {
    return libm_sin(x);
}

pub fn cos(x: f64) -> f64 {
    return libm_cos(x);
}

pub fn tan(x: f64) -> f64 {
    return libm_tan(x);
}

pub fn sqrt(x: f64) -> f64 {
    return libm_sqrt(x);
}

pub fn log(x: f64) -> f64 {
    return libm_log(x);
}

pub fn exp(x: f64) -> f64 {
    return libm_exp(x);
}

pub fn pow(x: f64, y: f64) -> f64 {
    return libm_pow(x, y);
}

// ... other functions
```

**User Code**:

```wado
use {sin, cos, pow} from "core:math";

fn calculate_orbit(angle: f64, radius: f64) -> [f64, f64] {
    let x = radius * cos(angle);
    let y = radius * sin(angle);
    return [x, y];
}

// ✅ Deterministic: Same results on all platforms
// ✅ No WASI dependency: Pure Wasm computation
```

### Special Handling: `builtin:libm` Namespace

The `builtin:libm` import source is a **compiler-internal namespace** that resolves to the embedded `libm.wasm`:

```rust
// wado-compiler/src/resolver.rs
match import_source {
    "builtin:libm" => {
        // Load embedded libm.wasm from binary
        let wasm_bytes = include_bytes!("../embedded/libm.wasm");
        resolve_wasm_module(wasm_bytes, import_items)
    }
    path if path.ends_with(".wasm") => {
        // User-provided .wasm file
        resolve_external_wasm(path, import_items)
    }
    // ... other cases
}
```

## Consequences

### Positive

✅ **Full determinism**: Math functions produce identical results across all platforms
✅ **No host dependency**: Pure Wasm, no WASI calls for math
✅ **Portability**: Wado programs are truly portable
✅ **License compatibility**: MIT/Apache-2.0 matches typical open source projects
✅ **Quality**: Rust libm is based on musl, widely tested
✅ **Build simplicity**: Rust → Wasm is straightforward
✅ **Maintenance**: Track upstream Rust libm releases

### Negative

⚠️ **Bundle size**: Increases output `.wasm` size (estimate: ~50-100 KB for full libm)
⚠️ **Performance**: May be slower than native CPU instructions in some cases
⚠️ **Maintenance burden**: Must rebuild libm.wasm when updating Rust libm
⚠️ **Limited to IEEE 754**: Cannot leverage platform-specific extended precision

### Mitigation Strategies

**Bundle size**:

- Tree-shaking: Only include math functions actually used in the program
- Separate f32/f64 variants to avoid duplicating both
- Provide `--math-backend=host` compiler flag for users who prefer host functions

**Performance**:

- For most applications, determinism > raw speed
- Profile and optimize hot paths if needed
- Consider SIMD variants (future: `f32x4`, `f64x2` operations)

**Maintenance**:

- Automate `libm.wasm` rebuild in CI/CD pipeline
- Pin Rust libm version in releases, update deliberately

## Implementation Plan

### Phase 1: Build Infrastructure

- [ ] Create `wado-libm` Rust wrapper crate
- [ ] Write `scripts/build-libm.sh` build script
- [ ] Generate `embedded/libm.wasm` and `embedded/libm.wit`
- [ ] Embed `libm.wasm` in compiler binary

### Phase 2: Compiler Integration (depends on ARD-2026-01-10-wasm-import)

- [ ] Implement `builtin:libm` namespace resolution
- [ ] Link bundled `libm.wasm` into final component

### Phase 3: Standard Library API

- [ ] Write `lib/core/math.wado` wrapper
- [ ] Document all math functions in API docs
- [ ] Add usage examples

### Phase 4: Testing

- [ ] Cross-platform determinism tests (Linux, macOS, Windows, wasmtime, browser)
- [ ] Precision tests against known constants
- [ ] Performance benchmarks

### Phase 5: Optimization

- [ ] Implement tree-shaking for unused math functions
- [ ] Add `--math-backend=host|bundled` compiler option
- [ ] Measure and document bundle size impact

## Alternatives Considered

### Alternative 1: WASI Host Functions

**Rejected**: Non-deterministic, defeats portability goals

### Alternative 2: Software Floating-Point (softfloat)

**Rejected**: Extreme precision but too slow, massive complexity

### Alternative 3: Lookup Tables + Interpolation

**Rejected**: Limited precision, large table size, not suitable for general-purpose library

### Alternative 4: Component Model Math Interface Standard (future)

**Deferred**: If WASI defines a standard `wasi:math` interface with determinism guarantees, we can switch to it. For now, bundled libm is the pragmatic choice.

## References

- [Rust libm repository](https://github.com/rust-lang/libm)
- [musl libc](https://musl.libc.org/)
- [Floating-point portability issues (Japanese)](https://zenn.dev/mod_poppo/articles/floating-point-portability)
- [IEEE 754-2019 Standard](https://ieeexplore.ieee.org/document/8766229)
- [WebAssembly Floating-Point Semantics](https://webassembly.github.io/spec/core/exec/numerics.html#floating-point-operations)
- Related ARD: [ARD-2026-01-10-wasm-import](./ard-2026-01-10-wasm-import.md)
