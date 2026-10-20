# WEP: One Ownership Decision — Where an Operand Moves, Said Once

## Context

[WEP: Affine Types](./wep-2026-10-17-affine-types.md) divides the work between
a static rule and a dynamic one. The move check refuses a second read of an
affine value. Drop elaboration writes null into the place a move leaves, and
drops at scope exit what is not null. Both need the same answer for every
operand: does this position take ownership of the value placed in it, or only
reference it?

Each pass answers it on its own:

| Pass             | File                                | IR                | Where it decides                                 |
| ---------------- | ----------------------------------- | ----------------- | ------------------------------------------------ |
| Move check       | `src/resource_move_check.rs`        | AST `Expr`        | `visit_value` (consumes) or `visit_expr` (reads) |
| Drop elaboration | `src/synthesis/resource_cleanup.rs` | TIR `TirExprKind` | the `consuming` flag of `elab_expr`              |

Two exhaustive matches over two IRs, written apart, drift apart. Where the
drop pass takes a value and the move check only reads it, a second use
compiles and reaches the null the first one left. An audit found four such
positions:

| Written                   | Move check | Drop elaboration    | Result                |
| ------------------------- | ---------- | ------------------- | --------------------- |
| `h as W` (newtype)        | reads      | follows the context | `h.id` after it traps |
| `[..t, 2]` (tuple spread) | reads      | follows the context | a second spread traps |
| `with E => h do { … }`    | reads      | consumes            | `h.t` after it traps  |
| `t[0]` (tuple element)    | reads      | consumes            | `t[0]` twice traps    |

The trap is incidental: a null GC reference traps where it is read. The WEP
leaves such a read unspecified, and the compiler adds no check. A position the
two passes disagree on in the other direction, where the check sees a move and
the drop pass does not take, would drop the value twice instead.

`method_call_consumes_receiver` already shows the shape a fix takes. The body
walk records the dispatch of each method call, and both the check and reify
read whether it takes `self` by value. Nothing else about ownership is
recorded once.

## Decision

### One table

Whether a position consumes its operand is a property of the construct and the
slot, never of the operand's type. `elaborator/ownership.rs` states it in one
exhaustive match over `ast::Expr` and `ast::Stmt`:

```rust
pub(crate) enum OperandMode {
    /// The position takes ownership: a `let`, a by-value argument, a
    /// `return`, an aggregate's element, a cast, a spread, a handler.
    Consume,
    /// The position references the operand: `&x`, a `&self` receiver,
    /// an operator operand, a template hole, a `matches` test.
    Borrow,
}
```

The annotate walk records the mode of every operand it visits, as the fact
`OPERAND_MODES` keyed by the operand's `AstId`. A call reads the recorded
dispatch for its receiver and the declared parameter types for its arguments,
so the table is where `consumes_self` is read too.

The mode holds for every type. A copyable value in a `Consume` position is
copied, so the table changes nothing for it. A body generic over `affine T` is
recorded once, and every instance reads the same modes.

### The move check reads the table

`MoveWalker` drops its choice between `visit_value` and `visit_expr`. It visits
each operand once and asks `Semantics::operand_mode`. A place in a `Consume`
position whose type is affine is consumed. Every other place is read.

### TIR carries the decision

Reify lowers a place in a `Consume` position to `TirExprKind::Move { place }`.
Drop elaboration loses its `consuming` flag. It empties a place exactly where a
`Move` names it, and spills a temporary exactly where a borrow takes it.

Synthesis that builds TIR without an AST builds a move with
`synthesis::common::move_of`, so a synthesized move is explicit too.

### A drift is a compiler panic, not a trap

Drop elaboration asserts that every place of an affine type in value position
is under a `Move`, a `&` or a `&mut`, or is a projection to a field that is not
affine. A new construct that the table leaves out then fails at the first
fixture that uses it, in the compiler, instead of reaching a null at run time.

The assertion states an invariant the compiler establishes: the move check has
already refused every program that would copy an affine value.

## Roadmap

- [ ] Fixtures for the four positions in the table above, each refused by the
  move check. They fail until the table lands.
- [ ] `OperandMode` and the `OPERAND_MODES` fact, recorded by the annotate
  walk, with `consumes_self` read there.
- [ ] `MoveWalker` reads `Semantics::operand_mode` and loses `visit_value`.
- [ ] `TirExprKind::Move`, emitted by reify and by `move_of`. Drop elaboration
  loses `consuming` and gains the assertion.
- [ ] Value-copy planning (`lower/plan/value_copy`) reads `Move` where it now
  infers that a read is the last one, if a measurement shows it gains from it.

## Known gaps

### What a value refers into is still tracked by one pass

The move check alone follows which locals a value refers into
(`owners_of`). Drop elaboration has no counterpart, so the table does not
cover it. A closure call's result carries none of what the closure refers
into, so `let r = f(); eat(t); r.id` compiles where `f` returns `&t`.

## References

- [WEP: Affine Types](./wep-2026-10-17-affine-types.md)
- [WEP: Elaborator Architecture](./wep-2026-05-26-elaborator-rearchitecture.md)
- [WEP: Declaration Identity](./wep-2026-08-12-declaration-identity.md)
