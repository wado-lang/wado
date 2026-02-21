# wasm-size

Wasm binary size comparison across languages.

## Setup

```sh
mise install                                                   # node, zig, go, wasmtime, tinygo, wasi-sdk
rustup target add wasm32-wasip1                                # Rust wasm target
curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # Moonbit (optional)
```

## Tasks

```sh
mise run report-wasm-size  # build all + validate + report
mise run build-all         # build only
mise run clean             # remove build artifacts
```

## Structure

Each program directory (`hello_world/`, `pi_approx/`) contains source files for all languages side by side. Language-specific config files (`Cargo.toml`, `moon.mod.json`, etc.) live in the same directory. AssemblyScript npm config is shared in `assemblyscript/`.
