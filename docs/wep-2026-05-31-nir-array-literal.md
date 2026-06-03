# WEP: `NirExprKind::ArrayLiteral` — a NIR-Materialized List Node

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

An array literal such as `[1, 2, 3] as List<i32>` never reaches NIR as a
literal. During elaboration it is coerced through the
`SequenceLiteralBuilder` trait path (see
[Iterator-Based Literal Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md)).
After that path is inlined, the construction reaches NIR as an imperative
builder sequence over a fresh local:

```text
let arr = List::<i32>::with_capacity(3);
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
purpose. `ArrayLiteral` is the same move for `List<T>`. And
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
/// `NirExpr::type_id` (the `List<T>` struct type), exactly as
/// `TupleLiteral` relies on `type_id` for the tuple's struct type.
ArrayLiteral {
    elements: Vec<NirExpr>,
},
```

Rationale for the minimal shape:

- No `element_type` / `array_type` field. `NirExpr` already carries
  `type_id`, and the array's `List<T>` struct type is recoverable from
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

A new NIR optimizer pass that recognizes the canonical `List<T>`
construction window and rewrites it to `ArrayLiteral`. It runs **after**
`nir/inline` in the fixed-point loop: the `SequenceLiteralBuilder`
`new_literal` / `push_literal` / `build` methods — and, for custom builders
that wrap an `List<T>`, the `push_literal → self.field.push` delegation —
must be inlined first so the raw `List<T> { array_new(N) } + List::push`
window is exposed. Later `cse` / `const_fold` in the same loop then see the
normalized literal.

The matched window, on a block's statement list, is:

1. An init statement (`Let` or `Assign`-to-local) whose value embeds one or
   more `List<T> { repr: array_new(N), used: 0 }` structs. The struct may be
   the bound value directly (a direct `List<T>` literal) or a field of a
   wrapper struct (`SeqVec { items: List<T> }`, `Bag { keys, values }`),
   reached by a field-index **path** from the bound local. The matcher also
   descends the `{ …; *__b }` block tail that direct literals carry.
2. The following statements, until each target array has received exactly its
   `array_new` capacity in `List::push` calls rooted at the bound local
   (`place.push(e)` or `place.field.push(e)`, peeling the `&mut`). Pushes to
   different array fields may interleave (`Bag`). Inlining `push_literal`
   leaves single-use, _pure_ element temps (`let v = e; place.push(v)`)
   between the pushes; these are resolved to their value and consumed with
   the window.

On a match, each `List<T> { array_new(N), used: 0 }` struct is rewritten in
place to `ArrayLiteral { elements }`, and the consumed push (and resolved
temp) statements are removed. The enclosing init keeps its shape, so a
wrapper builder's output struct is preserved with its array field now a
literal.

`List::push` is matched by membership in the set of its
`CompilerItem::ListPush` monomorphizations (each element type is a distinct
`NirFunction`), not by canonical path — mirroring how `string_push`
identifies its methods. `array_new` is matched by its builtin generic name.

Safety conditions:

- Each target must receive exactly its `array_new(N)` capacity in pushes;
  otherwise the array is genuinely growable, not a literal.
- Only non-empty literals collapse. A capacity-0 `array_new(0)` is
  indistinguishable from a growable-array init (`let mut v = []; v.push(…)`);
  collapsing it to a fixed-length `array.new_fixed()` would break subsequent
  growth (and traps for reference element types — caught by
  `opt_container_sroa_edge`). Empty literals stay in the growable form.
- A temp binding is consumed (resolved + dropped) only if its value is pure
  and the temp is not read after the window. Purity guarantees that cloning
  the value into the literal — or eliding the binding — is observationally
  neutral even if the temp feeds more than one push.

Placement is `step!("nir/array_literal", …)` immediately after
`step!("nir/inline", …)`. Like every loop pass it returns `bool` (changed),
so the fixed point reconverges if a later pass plants a fresh window.

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

- [x] Land the NIR `ArrayLiteral` pass and the `wir_build` → `ArrayNewFixed`
      lowering, then delete `collapse_array_push_sequences` and its helpers,
      keeping `forward_struct_field_constants` (bounds-check elimination now
      keys on the `StructNew List<T>` that `wir_build` emits directly).
- [x] Verify WIR output is equivalent across the e2e suite. The array
      fixtures (`array_bounds_elim_const_wir`, `array_append_collapse`,
      `opt_crossmod_array`, `wir_optimize_dce_orphan_push`, …) assert
      `array.new_fixed<...>` and the bounds-check-free `array_get` shapes,
      now produced solely by the NIR path.

The implementation went through two designs; the second is what landed:

- A first cut matched the _pre-inline_ `__seq_lit:` `SequenceLiteralBuilder`
  block and gated on the result type being `List<T>`. That gate was needed
  because the pre-inline block shape is identical for every builder, so the
  type was the only discriminator — but it also _excluded_ custom builders
  that wrap an `List<T>` (`SeqVec`, `FrozenVec`), whose inner array the old
  WIR pass collapsed (its `ArrayAccessPath::Field` case). Retiring the WIR
  pass under that design was a silent performance regression: those wrappers
  reverted to imperative `List::push` chains, a gap the existing fixtures
  did not assert against.
- The landed design matches the _post-inline_ `List<T> { array_new(N) } +
List::push` window at any place (direct local or `local.field`), so direct
  and wrapper arrays collapse uniformly to `array.new_fixed` — reproducing
  the old WIR pass's full coverage, including its field case, without a type
  gate. `array_append_collapse` gains
  `array.new_fixed<i32>(5, 10, 15)` plus a `wir_not_expect` on the wrapper's
  inner `List<i32>::push` to lock the wrapper case in;
  `array_bounds_elim_const_wir` is unchanged from `main` (the post-inline
  pass reproduces the old `__b_0` output exactly).

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
      `List<T>` `{ repr: array.new_fixed, used: N }` struct.
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

Five aggregate walkers handled `TupleLiteral` by recursing into its elements
but let `ArrayLiteral` fall into a `_ =>` catch-all — silent under-
approximation, because the catch-all suppressed the non-exhaustive-match
error that flagged the other sites. Each now descends into `ArrayLiteral`
elements identically: `store_load_forward`'s two modified-locals walkers,
`const_folding`'s aliased-field invalidation, `value_copy_demote`'s
self-taint, and `labeled_block_fusion`'s free-loop-exit check. (`elide_local`
and `cse` keep their conservative `_ => false` / `None` defaults — a missed
optimization for arrays, not a bug.)

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
- [Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)
  — the surface syntax and the `[…] as List<T>` coercion this node
  ultimately represents.
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
  — a primary downstream consumer.
