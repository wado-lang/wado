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

## Results

### hello_world

| Language       | Size (bytes) |
| -------------- | -----------: |
| wado           |        1,140 |
| zig            |        4,449 |
| assemblyscript |        6,913 |
| rust           |       42,587 |

### pi_approx

| Language       | Size (bytes) |
| -------------- | -----------: |
| zig            |       10,608 |
| assemblyscript |       11,372 |
| wado           |       53,707 |
| rust           |       62,952 |

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
| Moonbit        | Preview 1    | `--strip`                                                                     | Via peter-jerry-ye/wasi                      |
| Wado           | Preview 3    | `-Os`                                                                         | Component model                              |

## Requirements

### Managed by mise

Run `mise install` to install:

- **Node.js** - for AssemblyScript
- **Zig** - wasm32-wasi target

### Manual installation

```sh
rustup target add wasm32-wasip1                                # Rust wasm target
brew install tinygo                                            # TinyGo (macOS)
curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # Moonbit
brew install llvm lld wasi-libc wasi-runtimes                  # C (macOS)
```
