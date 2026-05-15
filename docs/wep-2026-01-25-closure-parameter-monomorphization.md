# Closure Parameter Monomorphization

## Status: Partially Implemented

The design choices in this WEP are folded into [Closure Implementation](./wep-2026-01-16-closure-implementation.md). This document is retained for implementation-phase tracking and to record the internal `Fn` trait machinery that backs the user-visible `fn` / `fn mut` syntax.

## Context

When closures are passed as function parameters, there's a type compatibility challenge:

```wado
fn apply(f: impl fn(i32) -> i32, x: i32) -> i32 {
    return f(x);
}

fn run() {
    let result = apply(|x| x * 2, 5);
}
```

After closure lowering, `|x| x * 2` becomes a functor struct `__Closure_0` with a `__call` method. However, `apply` expects an `impl fn(i32) -> i32` parameter, creating a type mismatch that must be resolved by monomorphization.

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

Desugar `impl fn(A) -> B` parameters to generic parameters with trait bounds:

```wado
// User writes:
fn apply(f: impl fn(i32) -> i32, x: i32) -> i32 {
    return f(x);
}

// Internally desugars to (Fn trait is not user-visible):
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

## Internal `Fn` / `FnMut` Trait Design

These traits are compiler-internal and not exposed to user code (per [Closure Implementation](./wep-2026-01-16-closure-implementation.md)). Users write `impl fn(...)`, `dyn fn(...)`, or `<F: fn(...)>`; the compiler desugars these to bounds on the internal traits below.

### Signatures

```wado
trait Fn<Args, Ret, Effects = []> {
    fn call(&self, args: Args) -> Ret with Effects;
}

trait FnMut<Args, Ret, Effects = []>: Fn<Args, Ret, Effects> {
    fn call_mut(&mut self, args: Args) -> Ret with Effects;
}
```

The `Effects` parameter defaults to `[]` (pure), so `Fn<[i32], bool>` is shorthand for `Fn<[i32], bool, []>`.

Sub-typing `fn <: fn mut` is realized as `Fn` being a sub-trait of `FnMut`.

The internal names mirror the `fn` / `fn mut` keywords, creating a natural mapping:

- `fn(T) -> R` ↔ internal bound `Fn<[T], R>`
- `fn mut(T) -> R` ↔ internal bound `FnMut<[T], R>`

### Type Parameter Semantics

| Parameter | Description                                  | Example                   |
| --------- | -------------------------------------------- | ------------------------- |
| `Args`    | Tuple of argument types using `[...]` syntax | `[i32, String]`           |
| `Ret`     | Return type                                  | `bool`                    |
| `Effects` | Tuple of effect types using `[...]` syntax   | `[Stdout]`, `[]` for pure |

### Mapping from `fn` / `fn mut` Types

| User-visible type                  | Internal bound (short) | Internal bound (full)     |
| ---------------------------------- | ---------------------- | ------------------------- |
| `impl fn() -> R`                   | `Fn<[], R>`            | `Fn<[], R, []>`           |
| `impl fn(A) -> R`                  | `Fn<[A], R>`           | `Fn<[A], R, []>`          |
| `impl fn(A, B) -> R`               | `Fn<[A, B], R>`        | `Fn<[A, B], R, []>`       |
| `impl fn(A) -> R with E`           | —                      | `Fn<[A], R, [E]>`         |
| `impl fn(A, B) -> R with (E1, E2)` | —                      | `Fn<[A, B], R, [E1, E2]>` |
| `impl fn mut(A) -> R`              | `FnMut<[A], R>`        | `FnMut<[A], R, []>`       |
| `impl fn mut(A) -> R with E`       | —                      | `FnMut<[A], R, [E]>`      |

### Example: Effectful Closure

```wado
// User writes:
fn for_each<effect E>(items: Array<i32>, f: impl fn mut(i32) with E) with E {
    for let item of items {
        f(item);
    }
}

// Internally desugars to:
fn for_each<F: FnMut<[i32], (), [E]>, effect E>(items: Array<i32>, mut f: F) with E {
    for let item of items {
        f.call_mut([item]);
    }
}
```

## Captures are Implicit

Closure captures (auto-by-reference per the [Closure Implementation WEP](./wep-2026-01-16-closure-implementation.md)) are not encoded in the closure type. The previously proposed `captures[...]` clause on function types is obsolete — capture information lives in the closure's environment struct and is invisible to the type system. Escape analysis handles heap promotion when captured bindings outlive the declaring scope.

The `stores` annotation for non-closure reference parameters remains as a separate mechanism (see [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)); it is not unified with the `Fn` / `FnMut` trait family.

## Implementation Status

### Completed

- [x] Parser support for trait bounds (`T: Trait`)
- [x] Type checking with bounds (struct type parameters)
- [x] Closure lowering to functor structs with `__call` methods
- [x] Default type parameters (`T = DefaultType` syntax)
- [x] Define internal `Fn` trait in `core:prelude`

### In Progress

- [ ] Internal `FnMut` trait with `Fn <: FnMut` sub-trait relation
- [ ] Parser support for `impl fn(...)` / `dyn fn(...)` / `fn mut(...)` syntax
- [ ] Reject bare `fn(T) -> U` in type positions with a clear diagnostic
- [ ] Type checking with bounds (function type parameters)
- [ ] Desugar `impl fn(...)` parameters to `Fn` / `FnMut` bounds
- [ ] Monomorphization respects bounds
- [ ] Auto-capture by reference (infer `&T` vs `&mut T` from body usage)
- [ ] Enforce `mut` binding requirement when calling `fn mut` closures
- [ ] Canonical closure synthesis on `dyn fn` boundary

## Implementation Phases

1. ~~Implement trait bounds (`T: Trait`)~~ DONE (struct params)
2. ~~Define internal `Fn` trait with effect parameter~~ DONE
3. Parser: accept `impl` / `dyn` qualifiers and `fn mut`; reject bare `fn(...)` in type positions
4. Type checker: desugar `impl fn(...)` parameters to bounded generics
5. Capture analysis: classify each captured binding as `&T` or `&mut T`; classify closure as `fn` or `fn mut`
6. Codegen: monomorphization respects bounds; `dyn fn` uses canonical closure shape
7. Remove legacy closure handling from codegen

## Consequences

- Functions with `impl fn(...)` parameters become generic, increasing monomorphization
- Closure calls inside such functions are static method calls, enabling optimization
- Code size may increase but runtime performance improves
- Effects are preserved through the internal trait's effect parameter
- `dyn fn(...)` provides explicit type erasure when monomorphization is not desired

## See Also

- [Closure Implementation](./wep-2026-01-16-closure-implementation.md)
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
