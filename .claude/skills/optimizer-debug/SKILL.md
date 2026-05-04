---
name: optimizer-debug
description: Debug Wado optimizer (TIR/WIR pass) bugs using WADO_TRACE, WADO_DUMP_PASS_BEFORE/AFTER, and WADO_LIST_PASSES env vars. Use when an optimization pass produces wrong code, ICEs the WIR pipeline ("invalid core Wasm module: type mismatch ..."), or when you need to see how a specific pass transforms the IR.
---

# Optimizer pass debugging

The `optimize.rs` and `wir_optimize.rs` pipelines are big — many passes,
each rewriting the IR in place. When a pass is wrong, the symptom usually
shows up two passes later as "the WIR validates but produces the wrong
behaviour" or as "the codegen finds an invalid Wasm module" deep in the
final emitter. The debug hooks below let you diff the IR around any
single pass without sprinkling `eprintln!` through pass internals.

All three are env-var-driven so they work uniformly across `wado compile`,
`wado test`, `wado run`, and Kiln invocations from `package-gale`.

## Quick recipes

### Which pass changed the IR?

```sh
WADO_LIST_PASSES=1 cargo run --bin wado --quiet -- compile -O1 file.wado -o /tmp/out.wasm 2>&1 | grep '\[pass\]'
```

Prints every pass name in execution order. Lets you correlate the order
in source (`optimize.rs` and `wir_optimize.rs`) with what actually fires
under your `-Ox` choice.

### What does pass X produce?

```sh
WADO_DUMP_PASS_AFTER=wir/sroa_multi_value_returns \
  cargo run --bin wado --quiet -- compile -O1 file.wado -o /tmp/out.wasm 2>&1 \
  | sed -n '/=== WIR after/,/=== end WIR/p'
```

Dumps the full WIR (or TIR, depending on the pass) right after the named
pass, framed by `=== WIR after <name> ===` / `=== end WIR after <name> ===`.

### What does pass X consume?

```sh
WADO_DUMP_PASS_BEFORE=wir/sroa_multi_value_returns \
  cargo run --bin wado --quiet -- compile -O1 file.wado -o /tmp/out.wasm
```

Same framing, but printed before the pass runs. Combine with `--diff` of
the after-dump to see exactly what the pass rewrote.

### Multiple passes at once

The variables accept a comma-separated list:

```sh
WADO_DUMP_PASS_AFTER=tir/inline,wir/sroa_multi_value_returns \
  cargo run --bin wado --quiet -- compile -O1 file.wado -o /tmp/out.wasm 2>/tmp/dump.log
```

### Trace pass-internal decisions

For developer-only `eprintln`-style messages from inside a pass:

```sh
WADO_TRACE=sroa_return cargo run --bin wado --quiet -- compile -O1 file.wado -o /tmp/out.wasm 2>&1 | grep '\[sroa_return\]'
```

Output is framed `[target] message`. Targets are passed verbatim to
`compiler_trace!(target, ...)` calls inside the compiler. Use
`WADO_TRACE='*'` to enable every target at once.

To add a new tracing call inside a pass:

```rust
use crate::compiler_trace;
// ...
compiler_trace!("sroa_return", "candidates = {}", candidates.len());
compiler_trace!("sroa_return", "rewriting return at {span:?}");
```

The cost when the target is disabled is one `OnceLock` get + a linear
scan of the configured target list — fine for any rate that makes
sense in a compiler pass.

## Workflow for a "WIR pipeline generated invalid core Wasm module" ICE

The codegen-time validator catches type mismatches the optimizer
introduced. The error always points at codegen, but the bug is upstream.
Walk the pass pipeline like this:

1. Get the failing fixture compiling at `-O0` first to confirm it is an
   optimization-introduced bug (not a lower/codegen bug).
2. List the passes that run at the failing `-Ox` level:
   ```sh
   WADO_LIST_PASSES=1 cargo run --bin wado --quiet -- compile -O1 fixture.wado -o /tmp/out.wasm 2>&1 | grep '\[pass\]'
   ```
3. Bisect: pick a pass roughly mid-pipeline, dump after it, and check
   whether the IR is already broken. If it is, the bug is at or before
   that pass; otherwise it is later.
4. Once you have the suspect pass, dump before AND after it and read
   the diff. The mismatch will be visible — usually a function whose
   signature was rewritten but whose return sites weren't (the canonical
   shape of the SROA / signature-rewrite class of bugs), or a struct
   layout that changed in one place but not at consumers.
5. Add `compiler_trace!("<pass_name>", ...)` calls at the suspect rewrite
   site to confirm which subtrees the pass visits. The `*_mut` walkers
   in `wir_visitor.rs` and `WirInstr::for_each_boxed_child_mut` cover
   most rewrite needs.

## Pass-name conventions

| Prefix       | Phase                              |
| ------------ | ---------------------------------- |
| `tir/<name>` | TIR-level pass (`optimize.rs`)     |
| `wir/<name>` | WIR-level pass (`wir_optimize.rs`) |

`WADO_LIST_PASSES=1` is the source of truth — names there match exactly
the strings the env vars want.

## When to NOT reach for these

- For runtime bugs (program compiles cleanly but produces wrong output),
  use the `debugger` skill (`rust-gdb`) or read the WIR/Wasm directly.
- For LSP / annotate-time issues, these hooks fire only during the
  optimization phase. Add `tracing` calls in `annotate.rs` directly.
- For monomorphization or lowering issues, dump TIR with `wado dump --tir`
  / `--tir-resolved` / `--tir-monomorphized` / `--tir-lowered` instead;
  those are exposed as proper CLI subcommands.

## See also

- `wado-compiler/src/trace.rs` — `compiler_trace!` macro and filter
  parsing (with unit tests).
- `wado-compiler/src/optimize.rs` — `run_pass` for TIR passes; defines
  the env-var hook implementation in `mod pass_dump`.
- `wado-compiler/src/wir_optimize.rs` — `wir_pass` for WIR passes.
