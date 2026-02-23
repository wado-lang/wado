# async-wc

Count lines, words, and bytes for a file.

## Build

```bash
ROOT=/path/to/skills
EX="$ROOT/mbt-wasip1-tools/assets/async-wc"
moon build -C "$EX"
```

## Run

```bash
ROOT=/path/to/skills
EX="$ROOT/mbt-wasip1-tools/assets/async-wc"
wasmtime run --dir .::. "$EX/target/wasm/release/build/async-wc.wasm" input.txt
```
