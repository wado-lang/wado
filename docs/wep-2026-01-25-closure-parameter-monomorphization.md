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

Phase headings map one-to-one to those in [Closure Implementation](./wep-2026-01-16-closure-implementation.md#migration-plan). File references below point at the current state of `claude/review-closure-design-1pSAO`; line numbers are approximate.

### Already in place

- [x] Parser support for trait bounds (`T: Trait`)
- [x] Type checking with bounds (struct type parameters)
- [x] Closure lowering to functor structs with `__call` methods (`lower/plan/closure.rs`)
- [x] Default type parameters (`T = DefaultType` syntax)
- [x] Internal `Fn<Args, Ret, Effects>` trait declared in `lib/core/prelude/traits.wado:289-305` (currently unused as a bound)
- [x] `__Closure_N` struct + funcref codegen, stable `functor_id`, specialised path (`MethodCall` on `__Closure_N`)
- [x] Canonical closure path: `NirExprKind::ClosureToCanonical`, per-`(N, Ret)` `CanonicalClosure_K`, inspectable supertype `$canonical_inspectable_base`, per-functor wrapper triple
- [x] `FnCanonicalDispatch` synthesised dispatch stubs (`synthesis/traits.rs:1565-1599`)
- [x] Heap promotion via boxing pass over `address_taken_locals` (`lower/plan/boxing.rs:476-499`)
- [x] Effect generic parameter `<effect E>` on free / impl functions (`parser.rs:3972-3978`)

### Phase 1: Parser surface (permissive)

- [ ] Add `Dyn` keyword to lexer/token (`lexer.rs:675-700`, `token.rs:20-43,160-183`)
- [ ] Remove the unused `move` keyword reservation (`lexer.rs:691`, `token.rs:36`)
- [ ] Parse `impl fn(...)` and `dyn fn(...)` in type positions (`parser.rs:3736-3783`); add `qualifier: ImplDynQualifier` to `FunctionType` (`ast.rs:2422-2554`)
- [ ] Parse `fn mut(...)` as a two-token form; add `is_mut: bool` to `FunctionType`
- [ ] Parse `fn(...)` / `fn mut(...)` in trait bound position (`parser.rs:4043-4078`)
- [ ] Parse `with (E1, E2)` parens-grouped multi-effect in bound contexts

### Phase 2: Internal `FnMut` trait

- [ ] Add `pub trait FnMut<Args, Ret, Effects>: Fn<Args, Ret, Effects>` to `lib/core/prelude/traits.wado` with `fn call_mut(&mut self, args: Args) -> Ret with Effects`
- [ ] Re-export via prelude
- [ ] Resolve bound `fn(...)` → internal `Fn<...>`, bound `fn mut(...)` → internal `FnMut<...>` in resolver

### Phase 3: Type-system split

- [ ] Add `is_mut: bool` to `ResolvedType::Function` (`tir.rs:377-383`)
- [ ] Sub-typing rule in `check_assignable` (`resolver/typecheck.rs:139-171`): `actual.is_mut == false && expected.is_mut == true` → Compatible; reverse → Incompatible
- [ ] Update `make_function` / TIR creation sites; type stringification

### Phase 4: Auto-capture by reference

- [ ] Walk closure body in `resolve_closure` to classify each outer-name use as read vs read/write (`resolver/closure.rs:74-344`, `resolver/types.rs:1124-1201`)
- [ ] Set `TirCapture.is_mut` per body usage, not per outer-local declaration
- [ ] Tag closure type `is_mut = any(capture.is_mut)`
- [ ] Retire `MutRef::Closure` and the `&mut ||` desugar (`resolver/operators.rs:711-718`, `resolve_mutable_closure` in `resolver/closure.rs:84-133`)
- [ ] Migrate fixtures `closure_2.wado`, `closure_3.wado`, `closure_iflet_template_collision.wado` away from `&mut ||`

### Phase 5: `mut` binding enforcement

- [ ] In `IndirectCall` / `MethodCall` construction (`resolver/call.rs:90-167`, `resolver/expr.rs`), check whether the local holding the callee was bound `let mut`; emit a diagnostic if not and the callee is `fn mut`
- [ ] Same check for function parameters: `fn run(f: impl fn mut(i32))` must have `mut f`
- [ ] Add compile-error fixtures (`closure_mut_binding_required_error.wado`, `closure_mut_param_required_error.wado`)

### Phase 6: Effect-check fix

- [ ] In `effect_check.rs:689-692,1201`, walk closure body under the closure's _own_ declared effect set, not the enclosing function's `current_effects`
- [ ] Lift TODO marker from `closure_escapes_effect_todo.wado`

### Phase 7: Stdlib `Iterator` migration

- [ ] Convert `lib/core/prelude/traits.wado:308-449` iterator methods to `impl fn mut(...) with E` with `<effect E>` parameters: `map`, `filter`, `fold`, `find`, `any`, `all`, `position`, `reduce`
- [ ] Add `for_each` method
- [ ] Consider `impl Iterator<Item = ...>` returns vs named adapter structs
- [ ] Migrate other bare-`fn(...)` stdlib references: `String::find_char` (`string.wado:742`), `Array::sort_by` / `sorted_by` (`array.wado:547,589`), `Benchmark::run` (`benchmark.wado:57`), serde `lookup` (`serde.wado:244`, `json*.wado`, `router.wado`)

### Phase 8: Codegen qualifier honoring

- [ ] Thread the parser-attached `impl` vs `dyn` qualifier through to `lower/plan/closure.rs::specializable` (`closure.rs:228-231`)
- [ ] `dyn fn(...)` always routes through `ClosureToCanonical`; `impl fn(...)` always specialises (subject to escape)

### Phase 9: Strict bare-`fn(...)` rejection

- [ ] Flip parser/resolver from permissive to strict: bare `fn(T) -> U` in type positions becomes a parse/resolve error
- [ ] Sweep `tests/fixtures/*closure*.wado` and other fixtures using bare `fn(...)` (closure_1/2/3, fn_ref_parameter, default_arg_fn_type_erases, global_closure_field, inspect_closure_indirect, newtype_closure_coercion, etc.)
- [ ] Add compile-error fixtures: bare `fn(...)` in parameter / return / let-annotation / struct field

### Phase 10: CM boundary error

- [ ] Compile-error fixture for exporting a function with a closure-typed parameter
- [ ] Compile-error fixture for importing a function with a closure-typed parameter

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
