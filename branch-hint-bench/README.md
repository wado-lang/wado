# branch-hint-bench

An A/B benchmark for the WebAssembly branch-hinting proposal as implemented in
[wado-lang/wasmtime @ `gfx/branch-hinting-v2`](https://github.com/wado-lang/wasmtime/tree/gfx/branch-hinting-v2)
(wasmtime tracking issue
[#9463](https://github.com/bytecodealliance/wasmtime/issues/9463)).

It runs the **same** wado-compiled component twice — once with
`Config::wasm_branch_hinting(false)` and once with `true` — so the only
difference is whether wasmtime acts on the `metadata.code.branch_hint` section
wado emits. wado's optimization is held constant (identical bytes), which
isolates the cold-block layout effect.

This crate is excluded from the wado workspace because it depends on a patched
wasmtime 46, not the workspace-pinned 44.

## Run

```sh
# from the wado repo root
cargo run -p wado-cli -- compile benchmark/branch_hint/branch_hint.wado -o /tmp/bh.wasm
cargo run --release --manifest-path branch-hint-bench/Cargo.toml -- /tmp/bh.wasm 10
```

The first build fetches and compiles the wasmtime branch, which is large.

## Findings (Apple Silicon, M-series)

- **Codegen — the hint works as intended.** The hot path is tightened (one fewer
  taken branch per iteration) and the cold body is hoisted out of line. This is
  asserted by the disas tests in the wasmtime PR; an objdump A/B on this
  component confirms the same on real wado output.
- **Wall time — within noise (~0%)** on a tight loop here: the eliminated forward
  branch is perfectly predicted and the loop fits in L1i, so layout barely
  matters. `branch_hint_big.wado` (16 cold branches, produced by `gen_big.py`)
  was an attempt to add i-cache pressure; still within noise on this CPU.
- **Conclusion.** The optimization is correct and free at runtime; measurable
  wins need i-cache-pressured / large hot code and are better measured on x86.
  This is published as a reproducible experiment, not a perf claim.
