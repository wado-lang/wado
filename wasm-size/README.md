# Wasm Size Comparison

Compares WebAssembly binary sizes across different languages.

## Languages

- **C** - via LLVM + wasi-libc
- **Rust** - wasm32-wasip1 target
- **Zig** - wasm32-wasi target
- **TinyGo** - wasip1 target
- **AssemblyScript** - 0.28.x with @assemblyscript/wasi-shim
- **Moonbit** - wasm target with peter-jerry-ye/wasi
- **Wado** - WASI P3 component model (under development)

## Programs

- `hello_world` - Minimal "Hello, World!" program
- `pi_approx` - Pi approximation using Leibniz formula (1M iterations)
- `zlib` - zlib decompress (inflate 286B -> 1KB patterned data)

## Results

### hello_world

| Language       | Size (bytes) |
| -------------- | -----------: |
| wado           |        1,352 |
| c              |        2,275 |
| zig            |        4,449 |
| assemblyscript |        6,913 |
| moonbit        |       21,103 |
| rust           |       42,587 |
| tinygo         |      162,341 |

### pi_approx

| Language       | Size (bytes) |
| -------------- | -----------: |
| wado           |        6,695 |
| zig            |       10,608 |
| assemblyscript |       11,372 |
| c              |       14,275 |
| moonbit        |       31,133 |
| rust           |       62,952 |
| tinygo         |      187,167 |

### zlib

| Language | Size (bytes) | Notes                          |
| -------- | -----------: | ------------------------------ |
| wado     |       17,080 | decompress only (core:zlib)    |
| zig      |       20,513 | decompress only (std.compress) |
| c        |       42,963 | decompress only (zlib 1.3.1)   |
| rust     |       81,876 | decompress only (zlib-rs 0.6)  |

## Testing

Each `.wado` program includes `test` blocks for correctness verification. Tests run via `make test-wado` alongside other Wado tests.

## Usage

```sh
# Install mise-managed tools
mise install

# Show other dependency requirements
mise run install-deps

# Build all and report sizes
mise run report-wasm-size

# Clean build artifacts
mise run clean
```

## Size Optimization Flags

All languages are compiled with size optimization and symbol stripping enabled:

| Language       | WASI Version | Optimization Flags                                                            | Notes                                        |
| -------------- | ------------ | ----------------------------------------------------------------------------- | -------------------------------------------- |
| C              | Preview 1    | `-Oz -Wl,--strip-all`                                                         | Aggressive size opt + strip all symbols      |
| Rust           | Preview 1    | `opt-level="z"`, `lto=true`, `codegen-units=1`, `strip=true`, `panic="abort"` | Configured in Cargo.toml `[profile.release]` |
| Zig            | Preview 1    | `-O ReleaseSmall`                                                             | Built-in size optimization mode              |
| TinyGo         | Preview 1    | `-opt=z -no-debug`                                                            | Size opt + strip debug info                  |
| AssemblyScript | Preview 1    | `--optimizeLevel 3 --shrinkLevel 2`                                           | Via @assemblyscript/wasi-shim                |
| Moonbit        | Preview 1    | `--release --strip`                                                           | Release mode + strip symbols                 |
| Wado           | Preview 3    | `-Os`                                                                         | Component model                              |

## Requirements

### Managed by mise

Run `mise install` to install:

- **Node.js** - for AssemblyScript
- **Zig** - wasm32-wasi target
- **Go** - needed by TinyGo
- **wasmtime** - for validation (v41+ required for Wado's WASI P3)
- **TinyGo** - wasip1 target (`github:tinygo-org/tinygo`)
- **wasi-sdk** - clang + wasm-ld + wasi-sysroot for C (`github:WebAssembly/wasi-sdk`)

### Manual installation

```sh
rustup target add wasm32-wasip1                                # Rust wasm target
curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # Moonbit
```
