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

Measured 2026-08-05 with rustc 1.97.1, Zig 0.15.2, Moonbit 0.1.20260803, and
2026-08-03 with wasi-sdk 25.0 for the rows below marked as carried over. Sizes
are toolchain- but not host-dependent, so a row whose toolchain has not moved
does not need remeasuring.

The `c` rows and `sqlite_highlight`'s `rust` row carry over: this run had no
wasi-sdk (`sqlite_highlight` needs it for tree-sitter's C parser), so
`report-wasm-size` skipped them.

### hello_world

| Language | Size (bytes) |
| -------- | -----------: |
| wado     |        1,974 |
| c        |        3,829 |
| zig      |        4,449 |
| moonbit  |        9,227 |
| rust     |       40,365 |

### pi_approx

| Language | Size (bytes) |
| -------- | -----------: |
| wado     |        6,034 |
| zig      |       10,608 |
| c        |       18,105 |
| moonbit  |       22,986 |
| rust     |       59,753 |

### zlib

Reads gzip data from stdin and decompresses it.

| Language | Size (bytes) | Notes                                  |
| -------- | -----------: | -------------------------------------- |
| wado     |       16,237 | stdin + gzip decompress (core:zlib)    |
| zig      |       20,072 | stdin + gzip decompress (std.compress) |
| c        |       34,484 | stdin + gzip decompress (zlib 1.3.1)   |
| rust     |       89,069 | stdin + gzip decompress (zlib-rs)      |

### sqlite_highlight

Reads SQL from stdin and writes syntax-highlighted HTML to stdout.

| Language | Size (bytes) | Notes                                       |
| -------- | -----------: | ------------------------------------------- |
| wado     |      271,633 | Gale-generated highlighter from `SQLite.g4` |
| rust     |    3,487,646 | tree-sitter + tree-sitter-sequel            |

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

wasmtime is inherited from the root `mise.toml`.

`wasi-sdk` (used for C and the Rust `sqlite_highlight` tree-sitter parser) is
commented out in `mise.toml`. To build those outputs, uncomment the
`github:WebAssembly/wasi-sdk` line in `[tools]` and re-run `mise install`, or
provide a wasi-sysroot another way (apt on Linux, Homebrew on macOS).

### Manual installation

```sh
rustup target add wasm32-wasip1                                # Rust wasm target
curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # Moonbit
```
