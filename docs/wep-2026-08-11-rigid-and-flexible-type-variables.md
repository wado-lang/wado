# WEP: Rigid and Flexible Type Variables

## Context

The elaborator had one representation, `ResolvedType::TypeParam`, for two things a type check must treat oppositely:

- the parameter an item declares, inside the body that declares it. It stands for whatever a caller supplies, so it is opaque there: nothing but itself is assignable to it.
- the slot of a signature a use site is about to fill. It stands for a type the solver has yet to determine, so it must accept a value and record what that value says.

`TypeParam` is interned by `(name, index)`, so `fn f<T>`'s `T` and `fn g<T>`'s `T` are the same `TypeId`. A check meeting a bare `T` could not tell which of the two it had.

Every site that needed the distinction guessed at it — comparing against the enclosing scope's parameters, asking whether an argument could have pinned the slot, consulting the bindings map for whether anything had been inferred at all. The guesses disagreed with each other, and `let x: T = 5` in a generic body was accepted by all of them, producing TIR that failed WIR validation.

## Decision

### Two variants

`ResolvedType::TypeParam` is **rigid** and appears only inside the item that binds it.

`ResolvedType::InferVar(InferVarId)` is **flexible**. A use site of a polymorphic signature mints one per slot and rewrites the signature into them before anything is checked against it (`elaborator/instantiate.rs`). A callee's parameter therefore never reaches a check as itself.

### What a check defers

`check_assignable` defers only what is genuinely undecided — an inference variable, a type pack awaiting expansion, an associated-type projection awaiting its impl, `unknown` / `error` — and compares a rigid parameter nominally. `let x: T = 5` in a body declaring `T` is a type error.

### Where a value is checked

A callee's parameter types name its own slots until it is instantiated. A call site instantiates first, then checks its arguments once against the substituted types. The same holds for a struct literal's fields and for a parameter's default.

### What fixes a slot

A bare slot is not a constraint on the value that fills it: `struct Context<T> { fields: T }` accepts whatever the literal puts in `fields`. Where several values fix one slot — two fields naming it, or a sequence literal's elements — they are checked against each other, since each is the only evidence the others have.

### Lifecycle

A variable is local to the inference that minted it. `finalize_infer_holes` substitutes solved ones away, reports unsolved ones as "cannot infer" (naming every unsolved slot of one use site in a single message), and pins them to `error`. No variable reaches a recorded fact; `optimize/dce` asserts this.

The type table is an intern table, not a set of live types: the intermediate types built on a variable stay in it, as every type ever considered does. A pass enumerating `TypeTable::all_types` selects with `TypeTable::is_concrete`.

## Consequences

Programs that were accepted and then failed WIR validation are now rejected with a diagnostic. This is the point of the change, and it surfaced twelve latent elaborator defects that the shared representation had been hiding.

Positional guessing is gone from method dispatch. A signature reports the slots it declares and the parameters that wrote them, so no consumer counts an offset or re-finds a declaration by name — a name scan cannot tell which declaration dispatch chose.

Instantiation happens per use site, so two calls to one generic function no longer share slot identities. Anything keyed on a slot's `TypeId` across call sites would need revisiting; nothing in the compiler was.
