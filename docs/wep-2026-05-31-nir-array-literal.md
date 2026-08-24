# WEP: `NirExprKind::ArrayLiteral` — a NIR-Materialized List Node

## Context

NIR gives two of the three aggregate constructors a first-class, analyzable
value form: `StructLiteral` and `TupleLiteral`. Arrays were the missing third.

An array literal such as `[1, 2, 3] as List<i32>` did not reach NIR as a
literal. Elaboration coerced it through a builder-trait path, so once that path
was inlined the construction arrived as an imperative sequence over a fresh
local — a `with_capacity` call bound by a `Let`, followed by N `push`
statements. The fixed-shape const-array form re-materialized only at WIR.

The consequence was a blind spot: every NIR pass saw an array constant as opaque
imperative mutation, not as a value. NIR could not deduplicate it, index into it
at compile time, reason about its length, or globalize it. The capability
existed one layer too late.

## Decision

### The node

`NirExprKind::ArrayLiteral { elements }`, shaped to match its sibling
`TupleLiteral`. No `element_type` / `array_type` field: the node carries a
`type_id` and the element type is recoverable from it, exactly as `wir_build`
recovers a tuple's struct type. A redundant type field would be a second source
of truth every clone and rewrite must keep in sync.

### Where it comes from

`lower` emits the node: a `[e0, e1, …]` literal denotes an `Array<T>` of exactly
its length, which the target's `From<Array<T>>` impl then consumes
([Literal Coercion as `From<Array<…>>`](./wep-2026-08-24-literal-from-array.md)).
`elements` are expressions, not a constant-only payload: an `ArrayLiteral`
evaluates them left to right, and const-ness unlocks the _consumers_ rather than
being a precondition of the _node_.

`wir_build` lowers `ArrayLiteral` to the existing `WirInstr::ArrayNewFixed`; no
new WIR node. The downstream WIR passes that consume `ArrayNewFixed`
(`promote_constant_arrays_to_data`, `split_large_array_literals`,
`rewrite_constant_array_indexing`) key on what `wir_build` keeps emitting.

## Consumers

`ArrayLiteral` is shared infrastructure — it pays for itself across passes rather
than for one.

- Deduplication of identical constant arrays, which falls out of value hash-consing.
- `const_folding` / `niri` — fold `Index(ArrayLiteral, const)` to the element.
- Bounds-check elimination — `elements.len()` is a first-class static fact.
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
  — a fully-constant `ArrayLiteral` globalizes like a constant struct or string.

## Consequences

- Constant arrays gain the analyzable value shape structs and tuples already
  have, one layer earlier, and the capability is centralized: passes consume one
  normalized node instead of each re-detecting a statement window.
- One more NIR expression kind, so every exhaustive match over NIR expressions
  grows an arm. Most join the `TupleLiteral` arm — it is the same fresh
  aggregate of sub-expressions. Watch for aggregate walkers that reach
  `TupleLiteral` through a `_ =>` catch-all: the catch-all suppresses the
  non-exhaustive-match error, so an `ArrayLiteral` silently under-approximates
  there instead of failing to compile.
- Consumers read the node's type to know what it denotes: an `Array<T>` is the
  sequence itself, and anything else is a container over one.

## Alternatives considered

- Keep collapsing only at WIR. Rejected: WIR is past every NIR analysis, so it
  can serve none of the consumers above.
- A constants-only payload (folded element values rather than expressions).
  Rejected: it would not normalize the non-constant case, would diverge from
  `TupleLiteral`, and would force the node to carry a value representation NIR
  does not otherwise use.
- Reconstruct the node in the optimizer from an inlined `array_new(N) + N ×
  push` window. Rejected: `lower` emits the node directly, so the matcher would
  only be re-deriving what it was already told, and a shape-sensitive matcher
  breaks whenever the lowering it keys on moves. Every benchmark's `-O2` WIR is
  identical without it.

## See also

- [Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md) — the parent WEP.
- [Literal Coercion as `From<Array<…>>`](./wep-2026-08-24-literal-from-array.md)
  — what emits this node.
- [Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)
  — the surface syntax this node ultimately represents.
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
  — a primary downstream consumer.
