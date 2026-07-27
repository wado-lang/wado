# WEP: Global Variables

## Context

Wado needs module-level state: configuration values, counters, caches, singletons.
WebAssembly provides globals for exactly this, and Wado's philosophy is that the
Wasm concept should stay visible rather than being wrapped.

### Wasm globals

| Aspect         | Local variable    | Wasm global                 |
| -------------- | ----------------- | --------------------------- |
| Scope          | Function          | Module                      |
| Lifetime       | Stack frame       | Module                      |
| Access         | Stack slot        | `global.get` / `global.set` |
| Initialization | On function entry | On module instantiation     |
| Mutability     | Always mutable    | Declared                    |

A Wasm global's initializer must be a _constant expression_ — a subset of Wasm
evaluable at instantiation without running code.

### Keyword choice

`global`, not `let` / `static` / `const`. `let` would conflate two concepts with
different initialization, lifetime, and access semantics; `static` implies a
memory model that does not apply; `const` is reserved for compile-time
constants. `global` names the Wasm concept it compiles to, which keeps the
initialization restriction and the access cost visible at the declaration site.

## Decision

### Syntax

```wado
global PI: f64 = 3.14159;
global mut counter: i32 = 0;

pub global VERSION: i32 = 1;
pub global mut state: bool = false;
```

Every type is allowed, including `String`, `List<T>`, and structs.

### Assignment

Only a `global mut` may be assigned. Immutability is a Wado-level property and
is enforced regardless of how the global is represented in Wasm.

```wado
global CONSTANT: i32 = 42;
global mut variable: i32 = 0;

fn example() {
    variable = 10;    // OK
    CONSTANT = 10;    // Error: cannot assign to immutable global
}
```

### What a constant expression can hold

Wado targets Wasm 3.0, so the GC and extended-const instructions are available.
The constant instructions are:

- `i32.const` / `i64.const` / `f32.const` / `f64.const` / `v128.const`
- `i32.add` / `sub` / `mul` and the `i64` forms
- `ref.null`, `ref.i31`, `ref.func`
- `struct.new`, `struct.new_default`
- `array.new`, `array.new_default`, `array.new_fixed`
- `any.convert_extern`, `extern.convert_any`
- `global.get` of an imported or previously declared global

This is much wider than a literal. A struct of constants is a `struct.new`; a
list or a short string is an `array.new_fixed` wrapped in the `{ repr, used }`
`struct.new`; a global derived from an earlier one is a `global.get` plus
arithmetic. Nearly every global a program declares is expressible directly.

### Direct and deferred initialization

A global is initialized one of two ways:

- Direct — the Wasm slot holds the value, produced by a constant expression at
  instantiation.
- Deferred — the slot starts at a placeholder and the module's initialization
  function assigns the value before any other code runs.

Deferral is for values that genuinely need to run code: a call the interpreter
cannot evaluate, a value read out of mutable state, or a payload too large to
inline as `array.new_fixed` — a long string literal lives in the data section
and is materialized at run time, so no constant expression can denote it.

### The decision is made on the value, not on the syntax

Whether a global is direct is decided from what its initializer _evaluates to_,
after the optimizer has folded it, and against the constant-instruction set
above. It is not decided from the shape of the declaration.

This matters because the two differ enormously. `global T: List<i32> = [1, 2, 3]`
is not a literal, but it evaluates to a sequence of constants, which is exactly
an `array.new_fixed`. Deciding syntactically would defer it; deciding on the
value does not.

The compile-time interpreter ([niri](./wep-2026-04-27-nir-interpreter.md))
already models the values this needs — scalars, aggregates, and sequences — so
the predicate is a mapping from its value model onto the instruction set, not a
separate evaluator.

### The declared initializer is never replaced

A global's recorded initializer is always the one the program declared. A
deferred global carries its placeholder alongside, never instead.

This is the invariant the representation must preserve. Anything asking "what is
this global's value" — constant folding, globalization, documentation — must get
a truthful answer, and a placeholder standing in for the initializer is a lie
that reads as a perfectly good constant. A `global A: i32 = 1 + 2` whose
recorded initializer has become `0` folds every read of `A` to `0`.

### Wasm slot shape is derived, not stored

Whether the Wasm slot is mutable, whether it is nullable, and whether reads need
narrowing are all consequences of the two facts above, and are derived when the
Wasm module is built:

- The slot is mutable when the global is `global mut`, or when it is deferred.
- The slot is nullable when it is deferred and reference-typed, or when the
  declared value is itself `null`.
- Reads are narrowed only in the first of those cases, since in the second
  `null` is a value the program can legitimately observe.

Neither the typed IR nor the normalized IR stores these. They describe the Wasm
representation, which is the Wasm builder's business.

### Multi-module initialization

Each module with deferred globals gets a `pub fn __initialize_module()`
assigning them, ordered topologically so a global is assigned after everything
it depends on. A cycle is a compile-time error.

The entry module gets a `fn __initialize_modules()` that calls each linked
module's in dependency order, guarded by a flag so repeated entry — an HTTP
handler invoked many times on one instance — initializes once. Every entry point
calls it first.

The initialization functions are ordinary functions in the normalized IR, so the
optimizer inlines, folds, and prunes them like any other. That is why they are
materialized before optimization rather than when the Wasm module is built.

A global's value must not be folded into a read that happens _inside_ an
initialization function: the topological order guarantees a dependency is
assigned first, but the interpreter does not model that order, so it declines
there rather than reasoning about it.

## Consequences

- Most globals initialize directly, so the initialization functions shrink to
  the values that truly need run-time work, and startup does less.
- A directly initialized global's value is visible to the optimizer, so reads of
  it fold, and the folds cascade into branch pruning and dead-global removal.
- Deciding late means the decision improves as the optimizer improves: a global
  whose initializer becomes constant through inlining or compile-time evaluation
  becomes direct without any special case.
- Globals are module-private unless `pub`; `pub` does not export them across the
  Component Model boundary.

## TODO

- [ ] Keep the declared initializer and carry the placeholder separately, so the
      recorded initializer is never a lie.
- [ ] Derive slot mutability, nullability, and read narrowing when building the
      Wasm module; drop them from the typed and normalized IRs.
- [ ] Decide direct-versus-deferred from the folded value against the
      constant-instruction set, replacing the syntactic literal test.
- [ ] Retire the Wasm-level pass that promotes a deferred global back to a
      constant one, which exists only because the decision is currently made too
      early to be right.
- [ ] Fold reads of a directly initialized immutable global in the interpreter,
      and decline inside the initialization functions.

## Future work

- Component Model export (`export global`).
- Thread-safe mutable globals, once Wasm threads are in scope.
