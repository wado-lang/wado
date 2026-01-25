# Closure Parameter Monomorphization

## Status: Proposal (Partially Implemented)

## Context

When closures are passed as function parameters, there's a type compatibility challenge:

```wado
fn apply(f: fn(i32) -> i32, x: i32) -> i32 {
    return f(x);
}

fn run() {
    let result = apply(|x| x * 2, 5);
}
```

After closure lowering, `|x| x * 2` becomes a functor struct `__Closure_0` with a `__call` method. However, `apply` expects a `fn(i32) -> i32` type, creating a type mismatch.

## Options Considered

### Option A: Dynamic Dispatch with CanonicalClosure

Transform closures to a canonical closure format at the call site:

- `CanonicalClosure` struct: `(env: ref struct, funcref: ref func)`
- Generate wrapper functions that bridge functor `__call` to the canonical calling convention
- `IndirectCall` uses `call_ref` with the funcref

Pros:

- Single function implementation regardless of closure type
- Smaller code size

Cons:

- Runtime overhead from indirect calls
- Cannot inline closure bodies
- Complex wrapper generation in codegen

### Option B: Trait Objects (`dyn Fn`)

Introduce `dyn Trait` support with vtables:

- Define `Fn<Args, Ret, Effects>` trait
- `fn(A) -> B` becomes `Box<dyn Fn<[A], B, []>>`
- Closures implement `Fn` and are boxed when passed

Pros:

- General solution for all trait objects
- Unified model for dynamic dispatch

Cons:

- Requires full vtable implementation
- Runtime overhead from vtable lookup
- Memory overhead from boxing
- Significant implementation effort

### Option C: Monomorphization (Recommended)

Desugar `fn(A) -> B` parameters to generic parameters with trait bounds:

```wado
// User writes:
fn apply(f: fn(i32) -> i32, x: i32) -> i32 {
    return f(x);
}

// Desugars to:
fn apply<F: Fn<[i32], i32, []>>(f: F, x: i32) -> i32 {
    return f.call([x]);
}
```

At call sites, the concrete closure type is known, triggering monomorphization:

```wado
apply(|x| x * 2, 5)
// Generates: apply$__Closure_0(closure_instance, 5)
```

Pros:

- No runtime overhead (static dispatch)
- Enables inlining of closure bodies
- Leverages existing monomorphization infrastructure
- Conceptually clean (closures are just structs with methods)

Cons:

- Code size increase from specialization
- Requires trait bounds implementation

## Decision

Implement Option C (Monomorphization) as the primary mechanism.

## `Fn` Trait Design

### Signature

```wado
trait Fn<Args, Ret, Effects = []> {
    fn call(&self, args: Args) -> Ret with Effects;
}
```

The `Effects` parameter defaults to `[]` (pure), so `Fn<[i32], bool>` is shorthand for `Fn<[i32], bool, []>`.

The name `Fn` mirrors the `fn` keyword, creating a natural mapping:

- `fn(T) -> R` (function type) ↔ `Fn<[T], R>` (trait bound)

### Type Parameter Semantics

| Parameter | Description                                  | Example                   |
| --------- | -------------------------------------------- | ------------------------- |
| `Args`    | Tuple of argument types using `[...]` syntax | `[i32, String]`           |
| `Ret`     | Return type                                  | `bool`                    |
| `Effects` | Tuple of effect types using `[...]` syntax   | `[Stdout]`, `[]` for pure |

### Mapping from `fn` Types

| Function Type               | Fn Bound (short) | Fn Bound (full)           |
| --------------------------- | ---------------- | ------------------------- |
| `fn() -> R`                 | `Fn<[], R>`      | `Fn<[], R, []>`           |
| `fn(A) -> R`                | `Fn<[A], R>`     | `Fn<[A], R, []>`          |
| `fn(A, B) -> R`             | `Fn<[A, B], R>`  | `Fn<[A, B], R, []>`       |
| `fn(A) -> R with E`         | —                | `Fn<[A], R, [E]>`         |
| `fn(A, B) -> R with E1, E2` | —                | `Fn<[A, B], R, [E1, E2]>` |

### Example: Effectful Closure

```wado
// User writes:
fn for_each(items: Array<i32>, f: fn(i32) with Stdout) with Stdout {
    for let item of items {
        f(item);
    }
}

// Desugars to:
fn for_each<F: Fn<[i32], (), [Stdout]>>(items: Array<i32>, f: F) with Stdout {
    for let item of items {
        f.call([item]);
    }
}
```

## Future: `stores` and Captures

The `stores` annotation and closure captures are special forms of "storage effects":

```wado
// stores indicates the function stores a reference parameter
fn register(data: &Data) -> Handle with stores[data] { ... }

// Closure captures are similar - they "store" outer references
let x = 42;
let closure = || x + 1;  // captures x
```

These will be modeled as internal effect types in the future:

- `stores[N]` → internal `Stores<N>` effect type
- Captures → implicit `Captures<...>` effect on closure type

For now, these are not part of the `Fn` trait and handled separately.

## Implementation Status

### Completed

- [x] Parser support for trait bounds (`T: Trait`)
- [x] Type checking with bounds (struct type parameters)
- [x] Closure lowering to functor structs with `__call` methods
- [x] Default type parameters (`T = DefaultType` syntax)
- [x] Define `Fn` trait in `core:prelude`

### In Progress

- [ ] Type checking with bounds (function type parameters)
- [ ] Desugar `fn(...)` parameters to `Fn` bounds
- [ ] Monomorphization respects bounds

### Future

- [ ] Effect types as first-class values
- [ ] `stores` as internal effect type
- [ ] Capture effects for closures

## Implementation Phases

1. **Phase 1**: ~~Implement trait bounds (`T: Trait`)~~ **DONE** (struct params)
2. **Phase 2**: ~~Define `Fn` trait with effect parameter~~ **DONE**
3. **Phase 3**: Desugar `fn(...)` parameters to bounded generics
4. **Phase 4**: Effect propagation through monomorphization
5. **Phase 5**: Remove legacy closure handling from codegen

## Consequences

- Functions with `fn(...)` parameters become generic, increasing monomorphization
- Closure calls inside such functions are static method calls, enabling optimization
- Code size may increase but runtime performance improves
- Effects are preserved through the `Fn` trait's effect parameter
- Future `stores`/captures can be integrated as effect types

## See Also

- [Closure Implementation](./wep-2026-01-16-closure-implementation.md)
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Effect System and Randomness](./wep-2026-01-20-effect-system-randomness.md)
