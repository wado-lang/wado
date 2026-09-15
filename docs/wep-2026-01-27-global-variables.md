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

### An initializer performs no effect

An initializer runs at module instantiation. Nothing is installed for it then,
and it runs in dependency order rather than one the program wrote. It declares
no `with` clause, and has nowhere to declare one, so it behaves as a function
body that declares no effect: calling a function that declares one is a compile
error. A default-value expression carries the same rule, and one checker answers
for both positions, naming the position in its diagnostic.

A closure literal's body is read where it is written, as it is inside a
function: the `fn() with E` a global's annotation gives it grants its body
nothing.

An initializer may install its own handler. `with E => h do` answers the
operations its body dispatches:

```wado
global COUNTED: i32 = hold: {
    let mut tally = Tally { value: 10 };
    with Counter => &mut tally do {
        break hold: Counter::next()
    }
    break hold: 0
};
```

### A dispatch is not an effect the position holds

A user-defined effect's operation is answered by an installed handler, and traps
where none is (see [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md)).
That is a runtime outcome rather than a demand on the position, and it is the
same outcome in an initializer as in a function body, which is why an
initializer may write the dispatch:

```wado
global COUNTED: i32 = Counter::next();   // compiles; traps at module init
```

The purity check would otherwise have to hold only for a dispatch written
directly in the initializer: a signature carries no record that a function
dispatches an operation, so the same dispatch one call away is invisible to it.
A rule that holds for `Counter::next()` and not for `indirect()` is worse than
no rule, and the runtime answer already covers both.

An operation backed by the host, or by a component that reaches the host, is a
different matter: it demands a capability the position must already hold, and an
initializer holds none, so dispatching one is a compile error. A purely
computational component's operation demands nothing, so an initializer may call
it.

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

Two steps put a value in the slot, and both finish before the module is emitted.
The syntactic classifier below runs at every optimization level. The promotion on
the lowered Wasm value then takes back what the optimizer folded.

### An initializer is a body, and a body is a function

An initializer that needs to run code is a body: statements, locals, a
`with … do`, calls to rewrite. Everything that walks bodies — template
expansion, effect-dispatch desugaring, CM import binding, monomorphization —
walks the module's functions. A body reachable any other way is a body those
passes miss, and each miss is its own bug.

So reify puts it there. A global whose initializer is not a Wasm constant gets
`$init$<NAME>`, a parameterless function returning the declared value, and the
global's slot holds the placeholder. A global carries no locals of its own, so
it cannot hold a body at all. Lowering splices those functions into the module's
initialization function in dependency order and drops them.

One classifier decides, at reify. `Direct` then means "the Wasm slot can hold
this" at every phase after it. Deciding that early is safe because nothing on the
way can change the answer: the typed IR folds no constant, and turns no literal
into code. Lowering asserts that rather than trusting it.

### The decision is made on the value, not on the syntax

Whether a global is direct is decided from what its initializer _evaluates to_,
after the optimizer has folded it, and against the constant-instruction set
above. It is not decided from the shape of the declaration.

This matters because the two differ enormously. `global T: List<i32> = [1, 2, 3]`
is not a literal, but it evaluates to a sequence of constants, which is exactly
an `array.new_fixed`. Deciding syntactically would defer it; deciding on the
value does not.

Deferral is therefore provisional: reify defers anything that is not
syntactically constant, and a single classifier later promotes back everything
the optimizer reduced to a constant expression. It runs once the value is
lowered to its Wasm shape, because that is where variant representation and
non-null field wrapping are settled and the constant-instruction test is exact.

The cost of deciding there is that the normalized IR never learns the answer, so
the compile-time interpreter cannot read a constant global's value — see the
value-snapshot entry below.

### A placeholder never passes for the declared value

A deferred global holds a placeholder, and says so. Asking it for its declared
value answers "assigned elsewhere", never the placeholder.

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

Each module with deferred globals gets a `pub fn $initialize_module()`
assigning them, ordered topologically so a global is assigned after everything
it depends on. A cycle is a compile-time error.

The entry module gets a `fn $initialize_modules()` that calls each linked
module's, guarded by a flag so repeated entry — an HTTP handler invoked many
times on one instance — initializes once. Every entry point calls it first.
Those calls are ordered the same way, by the globals each module's initializers
read, so a global crossing a module boundary is assigned before it is read; the
entry module goes last.

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

- [x] Record what a Wado-immutable deferred global is assigned, so the
      interpreter can fold its reads without waiting for the Wasm-level
      classifier. Initialization functions need no exception: initializers are
      ordered by dependency, so a read there follows the assignment it folds
      from.
- [x] Represent the two initialization kinds as one choice rather than a
      placeholder standing in for the initializer, so a deferred global's
      recorded initializer can never be mistaken for its value.
- [x] Derive slot mutability, nullability, and read narrowing when building the
      Wasm module; drop them from the typed and normalized IRs.
- [x] Widen the syntactic test lowering uses to defer, as far as it can honestly
      go: a literal, and `add` / `sub` / `mul` over literals at the widths Wasm
      admits. An aggregate or a sequence stays with the classifier that runs on
      the lowered Wasm value, because whether the builder sequence producing it
      collapsed is not knowable before the optimizer runs.

## Future work

- Component Model export (`export global`).
- Thread-safe mutable globals, once Wasm threads are in scope.
