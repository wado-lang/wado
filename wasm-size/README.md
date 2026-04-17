# Wasm Size Comparison

Compares WebAssembly binary sizes across different languages.

## Languages

- **C** - via LLVM + wasi-libc
- **Rust** - wasm32-wasip1 target
- **Zig** - wasm32-wasi target
- **Moonbit** - wasm target with peter-jerry-ye/wasi
- **Wado** - WASI P3 component model (under development)

## Programs

- `hello_world` - Minimal "Hello, World!" program
- `pi_approx` - Pi approximation using Leibniz formula (1M iterations)
- `zlib` - gzip decompress from stdin (read gzip data from stdin, decompress, write to stdout)
- `sqlite_highlight` - SQL syntax highlighter (reads SQL from stdin, writes HTML to stdout).
  Wado uses the Gale-generated highlighter from `SQLite.g4`; Rust uses
  `tree-sitter` + `tree-sitter-sequel`.

## Results

### hello_world

| Language | Size (bytes) |
| -------- | -----------: |
| wado     |        1,795 |
| c        |        3,076 |
| zig      |        4,449 |
| moonbit  |       15,319 |
| rust     |       40,754 |

### pi_approx

| Language | Size (bytes) |
| -------- | -----------: |
| wado     |        8,927 |
| zig      |       10,608 |
| c        |       16,786 |
| moonbit  |       25,006 |
| rust     |       60,683 |

### zlib

Reads gzip data from stdin and decompresses it.

| Language | Size (bytes) | Notes                                  |
| -------- | -----------: | -------------------------------------- |
| wado     |       18,708 | stdin + gzip decompress (core:zlib)    |
| zig      |       20,072 | stdin + gzip decompress (std.compress) |
| c        |       33,439 | stdin + gzip decompress (zlib 1.3.1)   |
| rust     |       88,014 | stdin + gzip decompress (zlib-rs)      |

### sqlite_highlight

Reads SQL from stdin and writes syntax-highlighted HTML to stdout.

| Language | Size (bytes) | Notes                                          |
| -------- | -----------: | ---------------------------------------------- |
| wado     |      664,108 | Gale-generated highlighter from `SQLite.g4`    |
| rust     |    3,481,212 | tree-sitter + tree-sitter-sequel               |

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

| Language | WASI Version | Optimization Flags                                                            | Notes                                        |
| -------- | ------------ | ----------------------------------------------------------------------------- | -------------------------------------------- |
| C        | Preview 1    | `-Oz -Wl,--strip-all`                                                         | Aggressive size opt + strip all symbols      |
| Rust     | Preview 1    | `opt-level="z"`, `lto=true`, `codegen-units=1`, `strip=true`, `panic="abort"` | Configured in Cargo.toml `[profile.release]` |
| Zig      | Preview 1    | `-O ReleaseSmall`                                                             | Built-in size optimization mode              |
| Moonbit  | Preview 1    | `--release --strip`                                                           | Release mode + strip symbols                 |
| Wado     | Preview 3    | `-Os`                                                                         | Component model                              |

## Requirements

### Managed by mise

Run `mise install` to install:

- **Zig** - wasm32-wasi target
- **wasi-sdk** - clang + wasm-ld + wasi-sysroot for C (`github:WebAssembly/wasi-sdk`)

wasmtime is inherited from the root `mise.toml`.

### Manual installation

```sh
rustup target add wasm32-wasip1                                # Rust wasm target
curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # Moonbit
```
