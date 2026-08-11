# WEP: `NirExprKind::ArrayLiteral` — a NIR-Materialized List Node

## Context

NIR gives two of the three aggregate constructors a first-class, analyzable
value form: `StructLiteral` and `TupleLiteral`. Arrays were the missing third.

An array literal such as `[1, 2, 3] as List<i32>` never reaches NIR as a
literal. During elaboration it is coerced through the `SequenceLiteralBuilder`
trait path (see
[Iterator-Based Literal Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md)),
so once that path is inlined the construction arrives as an imperative builder
sequence over a fresh local — a `with_capacity` call bound by a `Let`, followed
by N `push` statements. The fixed-shape const-array form used to re-materialize
only at WIR.

The consequence was a blind spot: every NIR pass saw an array constant as opaque
imperative mutation, not as a value. NIR could not deduplicate it, index into it
at compile time, reason about its length, or globalize it. The capability
existed one layer too late.

## Decision

### The node

`NirExprKind::ArrayLiteral { elements }`, shaped to match its sibling
`TupleLiteral`.

- No `element_type` / `array_type` field. The node already carries a `type_id`,
  and the `List<T>` struct type is recoverable from it, exactly as `wir_build`
  recovers a tuple's struct type. A redundant type field would be a second
  source of truth every clone and rewrite must keep in sync.
- `elements` are expressions, not a constant-only payload. The collapse
  preserves the pushed expressions verbatim, and an `ArrayLiteral` evaluates
  them left-to-right — the same observable order as the push sequence it
  replaces. Const-ness unlocks the _consumers_; it is not a precondition of the
  _node_.

### The materializing pass

`optimize::array_literal` recognizes the canonical `List<T>` construction window
and rewrites it. It runs after `inline` in the fixed-point loop: the
`SequenceLiteralBuilder` methods — and, for custom builders wrapping a `List<T>`,
the `push_literal → self.field.push` delegation — must be inlined first so the
raw `array_new + push` window is exposed, direct or field-rooted. Matching
post-inline is what makes wrapper builders (`SeqVec { items: List<T> }`) collapse
uniformly with direct ones; an earlier design that matched the pre-inline builder
block needed a result-type gate and silently excluded them.

Three safety conditions bound the match:

- A target must receive exactly its `array_new(N)` capacity in pushes; otherwise
  the array is genuinely growable, not a literal.
- Only non-empty literals collapse. A capacity-0 `array_new(0)` is
  indistinguishable from a growable-array init, and collapsing it to a
  fixed-length array would break subsequent growth.
- A temp binding between pushes is consumed only if its value is pure and it is
  not read after the window, so folding it in is observationally neutral.

`lower` never emits `ArrayLiteral`. Like `Switch`, it is optimizer-materialized,
so no pre-`optimize` code needs to know about it. `optimize::string_push` is the
direct precedent — the same move for `String`, made for the same reason.

### Lowering and the retired WIR collapse

`wir_build` lowers `ArrayLiteral` to the existing `WirInstr::ArrayNewFixed`; no
new WIR node. `wir_optimize::array::collapse_array_push_sequences` existed only
because the fixed-array shape had nowhere to live before WIR, so it was deleted
rather than kept as a safety net — the same path `string_push` took when it
retired the WIR-level string collapse. The downstream WIR passes that consume
`ArrayNewFixed` (`promote_constant_arrays_to_data`, `split_large_array_literals`,
`rewrite_constant_array_indexing`) are unaffected: `wir_build` keeps emitting the
node they key on.

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
- The matcher is shape-sensitive: it depends on the inlined
  `SequenceLiteralBuilder` window, so `string_push` and `array_literal` must move
  together if that lowering changes. They already share the coupling.

## Alternatives considered

- Keep collapsing only at WIR. Rejected: WIR is past every NIR analysis, so it
  can serve none of the consumers above.
- A constants-only payload (folded element values rather than expressions).
  Rejected: it would not normalize the non-constant case, would diverge from
  `TupleLiteral`, and would force the node to carry a value representation NIR
  does not otherwise use.
- Fold the collapse into `string_push`. Rejected: the two match different builder
  methods and produce different nodes; one module per materialized node keeps
  each matcher legible.

## See also

- [Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md) — the parent WEP.
- [Iterator-Based Literal Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md)
  — why array literals reach NIR as builder push sequences.
- [Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)
  — the surface syntax this node ultimately represents.
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
  — a primary downstream consumer.
