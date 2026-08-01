# WEP: Variant Return ABI Decided at NIR

## Context

Tuple and user-struct return ABIs are already decided at NIR:
`optimize::multi_value_return` classifies a function, records
[`ReturnAbi::MultiValue`] on it, and `wir_build` emits the widened signature
and the call-site split locals. The variant case never made that move. It
still lives entirely in `wir_optimize::sroa_variant_return` — at 4171 lines the
largest WIR pass — because a variant's result-vector packing
(`compute_variant_layout`: shared-vs-per-case payload slots) is expressed over
`WirVariantType` and `WirPackage::variant_case_info`.

Status: proposed. Not implemented.

## Why it matters

Every NIR pass sees a `Result<T, E>`-returning call as one opaque boxed value.
The scalarization that would let `inline`, `copy_prop`, `dae`, `drve`,
`const_folding`, and `condition_implication` reason about the discriminant and
the payload separately happens after all of them have run. Concretely, a
`match f() { Ok(v) => …, Err(e) => … }` where `f` always returns `Ok` cannot
fold at NIR: the discriminant is inside a heap struct NIR never opens.

This is the same blind spot [`NirExprKind::ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md)
removed for constant arrays, and it is the largest one left.

Measured worth of the pass as an *output-size* optimization (skip-scan,
`-O2`): 2535 bytes on `gale_gen`, 1344 on `json_canada`, 30 on `sqlite_parse`,
0 on `count_prime` / `fts`. The size win is not the reason to move it — the
pass interactions it unblocks are.

## Shape of the change

Three pieces, in order:

1. **A type-level layout helper.** `compute_variant_layout` decides eligibility
   (payload types are eligible value types; the result vector fits
   `MAX_SHARED_RESULT_FIELDS` / `MAX_PER_CASE_RESULT_FIELDS`) and the slot
   assignment. Both inputs are derived from the variant's NIR type, so the
   helper can be phrased over `TypeId` + `TypeTable` and consulted from either
   layer. WIR keeps using it for materialization; NIR gains access for
   classification.

2. **`ReturnAbi::Variant`.** A NIR classification pass — sibling to
   `multi_value_return`, sharing its eligibility discipline — marks a function
   whose every `Return` produces a `VariantConstruct` of its own return type
   and whose every call site consumes the result by `match` / `if let` /
   field access. The NIR-level analysis is markedly simpler than the WIR one it
   replaces: `all_returns_are_variant_struct_new`, `validate_call_sites`,
   `resolve_wrapped_result`, and the br-target scans exist to re-discover, in a
   lowered instruction tree, structure NIR still has syntactically.

3. **`wir_build` materialization.** The widened signature, the
   `MultiValueLocalBind` at each call site, and the per-case payload padding
   (`pad_variant_fields` / `build_variant_replacement`) move to the builder,
   which is where the tuple/struct ABI already materializes.

`flatten_variant_slots` — the fix-point that peels a nested `ref W` result slot
— falls out of (3): with the decision made per function at NIR, a nested
eligible variant is classified on its own rather than recovered by re-running a
WIR rewrite.

## Migration and risk

The pass carries behaviour that must survive the move, and each piece has a
specific home:

- **Tail-call propagation** through widened callees — a `wir_build` concern,
  since it is about the emitted call shape.
- **Return-only temp scalarization / elision** (`scalarize_return_only_temps`,
  `elide_return_only_temps`) — cleanup of shapes the WIR widening itself
  creates; if the widening happens at build time these shapes never appear.
- **Trap and effect ordering** at a relocated payload
  (`relocated_value_may_trap`, `operand_needs_spill`) — must be re-derived over
  NIR, where `arena_query`'s purity and trap classification already answer it.

Land it behind the existing skip-scan discipline: the e2e suite green at
O0/O2, and the emitted Wasm compared byte-for-byte across the benchmark,
example, and fixture corpus before the WIR pass is retired.
