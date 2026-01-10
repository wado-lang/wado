# Floating-Point Math Functions in Wado

## Background

As discussed in [this article](https://zenn.dev/mod_poppo/articles/floating-point-portability), floating-point math functions (`sin`, `cos`, `log`, etc.) are implementation-dependent in traditional C/C++ environments, leading to portability issues.

WebAssembly provides deterministic basic arithmetic operations (IEEE 754-2019 compliant), but complex math functions remain host-dependent when called via WASI.

## Design Decision: Bundled Deterministic Math Library

**Goal**: Wado should provide deterministic math functions across all Wasm runtimes.

**Approach**: Bundle a fixed libm implementation compiled to Wasm, rather than delegating to WASI host functions.

## Implementation Strategy

### Option 1: Rust `libm` Crate (Recommended)

**Repository**: https://github.com/rust-lang/libm
**License**: MIT OR Apache-2.0 (dual license)

**Advantages**:
- Pure Rust implementation → Easy Wasm compilation
- `no_std` compatible → No WASI dependency
- Based on musl libm → Battle-tested
- High affinity with Wado compiler (Rust-based)
- Already used in Rust `no_std` ecosystem

**Architecture**:

```
wado-compiler/
├── lib/core/math.wado           # Wado API
└── src/
    └── codegen/
        └── intrinsics/
            └── libm.rs           # Rust libm wrapper
```

**Implementation sketch**:

```rust
// wado-compiler/src/codegen/intrinsics/libm.rs
use libm;

pub fn generate_math_intrinsics(module: &mut WasmModule) {
    // Export Rust libm functions as Wasm functions
    module.add_func("builtin.math_sin", |x: f64| libm::sin(x));
    module.add_func("builtin.math_cos", |x: f64| libm::cos(x));
    module.add_func("builtin.math_sqrt", |x: f64| libm::sqrt(x));
    module.add_func("builtin.math_log", |x: f64| libm::log(x));
    // ... other functions
}
```

```wado
// wado-compiler/lib/core/math.wado
pub fn sin(x: f64) -> f64 {
    builtin::math_sin(x)
}

pub fn cos(x: f64) -> f64 {
    builtin::math_cos(x)
}

pub fn sqrt(x: f64) -> f64 {
    builtin::math_sqrt(x)
}

// Usage in user code:
use {sin, cos} from "core:math";

fn calculate_angle(radians: f64) -> f64 {
    return sin(radians) + cos(radians);
    // ✅ Deterministic across all Wasm runtimes
}
```

### Option 2: musl libm

**License**: MIT
**Repository**: https://git.musl-libc.org/cgit/musl/tree/src/math

**Advantages**:
- MIT License
- Lightweight and high-quality
- Base of wasi-libc

**Disadvantages**:
- C language → Requires clang/wasi-sdk for Wasm compilation
- FFI needed to integrate with Rust compiler

**Build approach**:
```bash
# Compile musl libm to Wasm using wasi-sdk
clang --target=wasm32-wasi -O3 -c src/math/*.c
wasm-ld *.o -o libm.wasm --no-entry --export-all
```

### Option 3: fdlibm

**License**: Permissive (Sun/Oracle)
**Repository**: https://www.netlib.org/fdlibm/

**Note**: Used by Java's `StrictMath`, V8, etc. Requires license verification.

## Recommended Implementation Plan

### Phase 1: Integrate Rust `libm`

1. Add `libm` crate dependency to `wado-compiler/Cargo.toml`
2. Create `builtin::math_*` intrinsics in codegen
3. Implement `core:math` module in Wado standard library
4. Write tests to verify determinism across platforms

### Phase 2: Optimize Bundle Size

- Tree-shaking: Only include used math functions
- Consider f32 vs f64 variants (separate functions to reduce size)

### Phase 3: Performance Benchmarks

- Compare bundled libm vs WASI host calls
- Document performance characteristics

## Determinism Guarantees

With bundled libm:

| Operation | Determinism | Notes |
|-----------|-------------|-------|
| `+`, `-`, `*`, `/` | ✅ Full | IEEE 754-2019 |
| `sqrt` | ✅ Full | IEEE 754-2019 |
| `sin`, `cos`, `tan` | ✅ Full | Fixed libm implementation |
| `log`, `exp`, `pow` | ✅ Full | Fixed libm implementation |
| NaN payload | ⚠️ Partial | Wasm NaN canonicalization |

## License Compatibility

Rust `libm` is dual-licensed MIT/Apache-2.0, which is compatible with Wado's project license (assuming MIT or Apache-2.0).

## References

- [Rust libm crate](https://github.com/rust-lang/libm)
- [musl libc](https://musl.libc.org/)
- [Floating-point portability issues (Japanese)](https://zenn.dev/mod_poppo/articles/floating-point-portability)
- [WebAssembly Floating-Point Semantics](https://webassembly.github.io/spec/core/exec/numerics.html#floating-point-operations)
