# WEP: `NirExprKind::ArrayLiteral` — a NIR-Materialized Array Node

## Context

This WEP realizes the `NirExprKind::ArrayLiteral` node proposed, but left
unspecified, in the "Additions" section of
[Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md). It fixes the node
shape, the pass that materializes it, the downstream consumers, and the
migration of the existing WIR-level collapse.

Status: landed. `NirExprKind::ArrayLiteral` is materialized by
`optimize::array_literal`, lowered by `wir_build` to `array.new_fixed`, and
the WIR-level `collapse_array_push_sequences` it subsumes has been retired.
Full e2e suite green at O0/O2 with the WIR pass removed.

### Why NIR cannot see array literals today

An array literal such as `[1, 2, 3] as Array<i32>` never reaches NIR as a
literal. During elaboration it is coerced through the
`SequenceLiteralBuilder` trait path (see
[Iterator-Based Literal Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md)).
After that path is inlined, the construction reaches NIR as an imperative
builder sequence over a fresh local:

```text
let arr = Array::<i32>::with_capacity(3);
arr.push_literal(1);
arr.push_literal(2);
arr.push_literal(3);
```

In NIR terms this is one `NirStmtKind::Let` binding `arr` to a
`with_capacity` call, followed by N `NirStmtKind::Expr(MethodCall …)`
push statements on that local. The fixed-shape, const-array form only
re-materializes much later, at WIR, where
`wir_optimize::array::collapse_array_push_sequences` rewrites the
`LocalSet(with_capacity) + N pushes` window into `WirInstr::ArrayNewFixed`.

The consequence is a blind spot: every NIR analysis and optimization pass
sees an array constant as opaque imperative mutation, not as a value. NIR
cannot deduplicate it, cannot index into it at compile time, cannot reason
about its length, and cannot globalize it. The capability exists one layer
too late.

### Why a node, and why in NIR

NIR already gives the other two aggregate constructors a first-class,
analyzable value form: `NirExprKind::StructLiteral` and
`NirExprKind::TupleLiteral`. Arrays are the missing third. Adding
`ArrayLiteral` is normalization in exactly the sense the NIR WEP defines:
not re-introducing a lowered concept, but giving a construct its canonical
value shape so that shared infrastructure can act on it once instead of
each pass re-deriving the fact from a statement window.

There is direct precedent. `optimize::string_push` is the string analog:
it collapses the `String::with_capacity` + `push_str_literal` builder
window at the NIR level _specifically so that later NIR passes (cse,
const_folding) see the normalized form_ — its module doc says so, and that
it mirrors the WIR `collapse_array_push_sequences` but runs earlier on
purpose. `ArrayLiteral` is the same move for `Array<T>`. And
`match_to_switch` is the precedent for the category: a node that `lower`
never emits and that exists only because an optimizer pass materializes it.

## Decision

### The node

Add one variant to `NirExprKind` (in `nir.rs`), shaped to match its
sibling `TupleLiteral`:

```rust
/// A fixed-length array value, materialized by `optimize::array_literal`
/// from an inlined `SequenceLiteralBuilder` push sequence. `lower` never
/// emits this; it is an optimizer-materialized normalization, like
/// `Switch`. The element type and array type are carried by the enclosing
/// `NirExpr::type_id` (the `Array<T>` struct type), exactly as
/// `TupleLiteral` relies on `type_id` for the tuple's struct type.
ArrayLiteral {
    elements: Vec<NirExpr>,
},
```

Rationale for the minimal shape:

- No `element_type` / `array_type` field. `NirExpr` already carries
  `type_id`, and the array's `Array<T>` struct type is recoverable from
  it, just as `wir_build` recovers a tuple's struct type from
  `expr.type_id` in `build_tuple_literal`. Adding a redundant type field
  would create a second source of truth that every clone and rewrite must
  keep in sync.
- `elements: Vec<NirExpr>` rather than a constant-only payload. The
  collapse preserves the pushed expressions verbatim; an `ArrayLiteral`
  evaluates them left-to-right at construction, the same observable order
  as the push sequence it replaces. Const-ness is what unlocks the
  _consumers_ (§Consumers), not a precondition of the _node_. This matches
  the WIR pass, which already collapses arbitrary push values, not only
  constants.

### The pass that materializes it: `optimize::array_literal`

A new NIR optimizer pass, modeled directly on `optimize::string_push`:

```rust
pub fn run(project: &mut NirProject, profiler: …) -> bool;
fn collapse_block(block: &mut NirBlock) -> bool;
fn try_match_array_build(stmts: &[NirStmt], start: usize) -> Option<ArrayBuild>;
fn recurse_into_children(…);
```

`collapse_block` walks each `NirBlock`'s statement vector with a sliding
window, calling `try_match_array_build` at each position, and
`recurse_into_children` descends into nested blocks and into block-valued
expressions, so literals built inside `if` / `match` / `LabeledBlock`
arms are also normalized.

`try_match_array_build` recognizes the canonical window:

1. `NirStmtKind::Let { local_index, value, .. }` whose `value` is a
   `with_capacity` call (free `Call` or `MethodCall`) on `Array<T>`, with
   a constant `i32` capacity `n`.
2. Exactly `n` immediately-following `NirStmtKind::Expr(MethodCall …)`
   statements, each a `push_literal` (or monomorphized `push`) whose
   receiver is `Local(local_index)` and whose single argument is the
   element expression.

On a match it replaces the `n + 1` statements with a single
`Let { local_index, value: NirExpr { kind: ArrayLiteral { elements }, type_id: <Array<T>>, .. }, .. }`,
preserving the binding's `local_index`, `type_id`, `is_mut`, and
`skip_value_copy`. The local stays an `Array<T>`; later mutation of it
(further `push`es elsewhere) remains valid — the node only describes the
_initial_ value, exactly as the WIR collapse does.

Placement: in the `run_optimization_passes` fixed-point loop, registered
as `run_pass("nir/array_literal", …)`, adjacent to `nir/string_push` and
early enough that `cse` / `const_folding` in the same loop see the
normalized form on the same or next iteration. Like every loop pass it
returns `bool` (changed), so the fixed point reconverges if a later pass
(e.g. `inline`) plants a fresh builder window.

Window-matching safety conditions (mirroring the WIR matcher, lifted to
NIR statements):

- The push statements are consecutive and immediately follow the `let`;
  any intervening statement aborts the match for that window.
- Capacity equals the push count; a mismatch aborts (the local is then a
  genuinely-growable array, not a literal).
- The empty case (`with_capacity(0)` with zero pushes) is matchable at
  NIR because the element type lives on `type_id`, not on the elements —
  unlike the WIR matcher, which bails on `expected == 0`. Whether to
  collapse empty literals is gated on whether `wir_build` (§Lowering)
  emits a valid zero-length array for the target; the conservative
  initial landing may leave empty arrays to the existing path.

### Lowering: `ArrayLiteral` → `ArrayNewFixed`

`wir_build::expr` gains an arm beside the `TupleLiteral` arm:
`ArrayLiteral { elements }` builds each element and emits
`WirInstr::ArrayNewFixed { type_id: <array struct type from expr.type_id>, elements }`.
This reuses the WIR node that already exists and is already handled by
every downstream WIR consumer (`wir_unparse`, `dce`,
`promote_constant_arrays_to_data`, `split_large_array_literals`). No new
WIR node is introduced.

### Migration of the WIR collapse

`wir_optimize::array::collapse_array_push_sequences` exists only because
the fixed-array shape previously had nowhere to live before WIR. Once
`ArrayLiteral` is materialized at NIR and `wir_build` lowers it to
`ArrayNewFixed`, that reason is gone: the array arrives at WIR already
collapsed, and the WIR matcher has nothing left to match. **The end state
is retirement, not coexistence** — `collapse_array_push_sequences` is
deleted once `ArrayLiteral` is in place.

This is not speculative. The string analog already made exactly this
move: there was once a WIR-level string-push collapse, and it was retired
when `optimize::string_push` took over at NIR — `string_push`'s own doc
calls it "the _former_ WIR pass". Arrays follow the same path. Keeping a
permanent WIR safety net would be the kind of duplicated, drift-prone
workaround the project's design rules reject; a "two options, decide by
measurement" hedge here is the same overcaution the parent NIR WEP flagged
and corrected in its own migration plan.

Sequencing (each step with a green checkpoint), as landed:

- [x] Land the NIR `ArrayLiteral` pass (before `inline`, mirroring
      `string_push`) and the `wir_build` → `ArrayNewFixed` lowering. The
      pass is gated on the result type being `Array<T>`, because
      `SequenceLiteralBuilder` is user-implementable and its other builder
      targets share the `__seq_lit:` shape but not the `Array<T>` `{ repr,
      used }` layout.
- [x] Verify WIR output is equivalent across the e2e suite. The array
      fixtures (`array_bounds_elim_const_wir`, `array_append_collapse`,
      `opt_crossmod_array`, `wir_optimize_dce_orphan_push`, …) still assert
      `array.new_fixed<...>` and the bounds-check-free `array_get` shapes,
      now produced solely by the NIR path. One fixture
      (`array_bounds_elim_const_wir`) was updated to the cleaner output: the
      literal binds directly to the user local (`arr.repr`) instead of the
      builder temp (`__b_0.repr`) the WIR collapse left behind.
- [x] Delete `collapse_array_push_sequences` and its helpers; keep
      `forward_struct_field_constants` (bounds-check elimination now keys on
      the `StructNew Array<T>` that `wir_build` emits directly).

No coexistence reached the branch: land, verify, and delete were sequenced
within it. The `Array<T>` gate is the one refinement the implementation
added over the original design — a real correctness fix, since matching the
builder trait alone would have mis-materialized custom builder targets.

What is **not** retired: the downstream WIR passes that consume
`ArrayNewFixed` — `promote_constant_arrays_to_data` (→ `ArrayNewData`),
`split_large_array_literals`, and `rewrite_constant_array_indexing`. They
key on `ArrayNewFixed`, which `wir_build` keeps emitting from
`ArrayLiteral`, so they are unaffected by the matcher's removal.

## Consumers

`ArrayLiteral` is shared infrastructure; it pays for itself across passes
rather than for one. Each consumer is additive and can land after the node:

- `cse` — currently returns `None` for aggregates (the comment at the
  `StructLiteral` / `TupleLiteral` site says so explicitly). With a
  structural key over `ArrayLiteral` elements, identical constant arrays
  deduplicate.
- `const_folding` / `niri` — fold `Index(ArrayLiteral, const)` to the
  indexed element when the index is a constant in range; `niri` evaluates
  `ArrayLiteral` to an array value directly.
- Bounds-check elimination — the static length `elements.len()` is now a
  first-class fact, so `Index(ArrayLiteral, k)` with `k < len` needs no
  runtime check.
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
  — a fully-constant `ArrayLiteral` becomes a globalizable constant value,
  the same treatment constant structs and strings get.

## Touch Points

A new `NirExprKind` variant is exhaustively matched across the NIR
machinery. As landed, the variant is handled in:

- [x] `nir.rs` — the variant definition (beside `TupleLiteral`).
- [x] `nir_visitor.rs` — visits each element expression, joined to the
      `TupleLiteral` arm (identical aggregate shape).
- [x] `nir_unparse.rs` — renders as `[e0, e1, …]`, joined to `TupleLiteral`.
- [x] `wir_build/translate.rs` — `build_array_literal` lowers to the
      `Array<T>` `{ repr: array.new_fixed, used: N }` struct.
- [x] `optimize/array_literal.rs` — the materializing pass, registered in
      `optimize.rs` (`mod array_literal;` + `step!("nir/array_literal", …)`).
- [x] Every other NIR exhaustive match (~30 optimize passes) — joined to
      the `TupleLiteral` arm, since `ArrayLiteral` is the same fresh
      aggregate of sub-expressions. The aggregate-opaque defaults in `cse` /
      `const_folding` / `container_sroa` / `const_global_promotion` remain
      conservatively correct; turning them into the consumers below is
      additive follow-up.

`niri` needs no dedicated arm yet: nothing evaluates an `ArrayLiteral`
through the interpreter on the landed paths. An `Index(ArrayLiteral, k)`
const-fold consumer would add one.

The discipline from the NIR WEP applies: because `ArrayLiteral` is
optimizer-materialized and never produced by `lower`, no pre-`optimize`
code (TIR, `lower`) needs to know about it.

## Consequences

### Benefits

- Constant arrays gain the same first-class, analyzable value shape that
  structs and tuples already have, one layer earlier than today.
- The capability is centralized: passes consume one normalized node
  instead of each re-detecting a statement window.
- WIR output is unchanged in the common case — `ArrayLiteral` lowers to
  the same `ArrayNewFixed` the WIR collapse produced — so the change is
  observably neutral until a consumer opts in.

### Trade-offs

- One more `NirExprKind` variant: every exhaustive match over NIR
  expressions grows an arm. This is the standing cost of any NIR node and
  is bounded by the touch-point list above.
- A transient period where both the NIR pass and the WIR collapse exist.
  Resolved by the migration checklist (retire or share-predicate).
- The matcher is shape-sensitive: it depends on the inlined
  `SequenceLiteralBuilder` window. If that lowering changes, both
  `string_push` and `array_literal` matchers must move together — they
  already share this coupling, so it is not new debt.

## Alternatives Considered

- Keep collapsing only at WIR (status quo). Rejected: WIR is past every
  NIR analysis, so it cannot serve cse / const_folding / globalization —
  the exact motivation in the NIR WEP.
- A constants-only `ArrayLiteral` payload (store folded element values,
  not expressions). Rejected: it would not normalize the non-constant
  array case, would diverge from `TupleLiteral`'s shape, and would force
  the node to carry a value representation NIR does not otherwise use.
- Fold the array collapse into the existing `string_push` pass. Rejected:
  the two match different builder methods and produce different nodes;
  one module per materialized node keeps each matcher legible, matching
  the existing one-pass-per-concern layout of `optimize/`.

## See Also

- [Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md) — the parent WEP;
  this design realizes its "Additions → `NirExprKind::ArrayLiteral`"
  proposal.
- [Iterator-Based Literal Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md)
  — why array literals reach NIR as builder push sequences.
- [Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)
  — the surface syntax and the `[…] as Array<T>` coercion this node
  ultimately represents.
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
  — a primary downstream consumer.
