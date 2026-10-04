---
name: optimizer-debug
description: "Debug the Wado optimizer with the WADO_TRACE, WADO_DUMP_PASS_BEFORE/AFTER, WADO_LIST_PASSES, and WADO_SKIP_PASS env vars. Use whenever a NIR/WIR pass is in question — wrong code, a WIR pipeline ICE, or just to see what a pass did to the IR. For guest-side slowness see wado-performance."
---

# Optimizer Pass Debugging

A wrong pass usually shows up passes later, as wrong behaviour or as an invalid
module at codegen. These env vars, read by every `wado` subcommand and by Kiln
runs, isolate one pass.

## The Hooks

| Variable                       | Effect                                                                     |
| ------------------------------ | -------------------------------------------------------------------------- |
| `WADO_LIST_PASSES=1`           | Prints `[pass] <name>` for each pass run, in order                         |
| `WADO_DUMP_PASS_BEFORE=<pass>` | Dumps the IR before it, framed `=== WIR before <name> ===`                 |
| `WADO_DUMP_PASS_AFTER=<pass>`  | The same, after it                                                         |
| `WADO_SKIP_PASS=<pass>[@N]`    | Skips it; `@N` only on the Nth fixed-point invocation                      |
| `WADO_TRACE=<target>`          | Enables `compiler_trace!(target, …)` lines, framed `[target]`; `*` for all |

Each takes a comma-separated list. Names are `nir/<name>` (`optimize.rs`) and
`wir/<name>` (`wir_optimize.rs`); a `#![wasm_module]` core module runs its own
WIR list as `wir/<module>:<name>` (`wir/mem:run_peephole`). `WADO_LIST_PASSES`
is the source of truth for names. Rules folded into the peephole session
(`ref_elim`, `value_copy_elide`, …) are not addressable; skip `nir/peephole`.

```sh
WADO_DUMP_PASS_AFTER=wir/sroa_multi_value_returns \
  cargo run --bin wado --quiet -- compile -O1 file.wado -o /tmp/out.wasm 2>/tmp/after.log
```

The crate denies `eprintln!`; trace with
`compiler_trace!("target", "…")` (`wado-compiler/src/trace.rs`), which costs
next to nothing when disabled.

A pass whose skip makes a bug vanish is a participant, not proven guilty: diff
its output across the working and broken configurations.

## A Fixed Point That Never Converges

`--log-level debug` says whether the NIR loop converged and which passes still
report changes.

1. `WADO_TRACE=opt_loop` lists each iteration's changing passes; the tail is the
   culprit set. `const_fold` names the functions it rewrote; `inline_sites`
   names the callees spliced per round.
2. Diff before/after one late round. Nothing rewritten is a false change report;
   a rewrite another pass undoes is two passes fighting; shrinking real work is
   a pass taking one step per round where one sweep would do.

## Output Far Larger Than the Level Below

That is the inliner, not the loop, once `opt_loop` shows few iterations.

1. Sweep `--optimize-inline-threshold`; a cliff is one callee crossing the
   budget.
2. `WADO_TRACE=inline` reports what the cold discount alone admitted, and its
   growth; `WADO_TRACE=cold_outline` says why such a callee was not split.
3. `--optimize-inline-growth <pct>` caps growth, and `--log-level debug` names
   what the cap refused.

## An Optimization That Stopped Firing

A `wir_expect` that flips with an unrelated knob is a pass that does not
recognise a shape another pass now produces. Bisect on the knob one step at a
time (one step is one callee, so the `dump --nir` diff is small), trace the pass
that stopped firing, and find the shape it accepts in one position and declines
in another.

## "WIR Pipeline Generated Invalid Core Wasm Module"

The bug is upstream of codegen. Confirm `-O0` compiles, bisect the pass list
with dumps until the IR first breaks, then diff around that pass. The usual
shape is a signature rewritten without its return sites, or a layout changed
without its consumers.

## Elsewhere

- Wrong runtime output from a clean compile: the `debugger` skill.
- Before optimization: `wado dump --tir-resolved`, `--tir-monomorphized`,
  `--nir-lowered`.
