# async-echo

Echo stdin to stdout, line by line. Stops on EOF.

## Build

```bash
ROOT=/path/to/skills
EX="$ROOT/mbt-wasip1-tools/assets/async-echo"
moon build -C "$EX"
```

## Run

```bash
ROOT=/path/to/skills
EX="$ROOT/mbt-wasip1-tools/assets/async-echo"
wasmtime run --dir .::. "$EX/target/wasm/release/build/async-echo.wasm"
```
