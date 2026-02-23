# args-env

Prints command-line arguments and environment variables.

## Build

```bash
ROOT=/path/to/skills
EX="$ROOT/mbt-wasip1-tools/assets/args-env"
moon build -C "$EX"
```

## Run

```bash
ROOT=/path/to/skills
EX="$ROOT/mbt-wasip1-tools/assets/args-env"
wasmtime run --dir .::. --env HELLO=world "$EX/target/wasm/release/build/args-env.wasm" one two
```
