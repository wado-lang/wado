# WEP: Closure Implementation Internals

How the closure design in [Closure Implementation](./wep-2026-01-16-closure-implementation.md) is realized: dispatch, the `fn` / `fn mut` split, and the Wasm GC representation.

## Context

A closure literal lowers to a functor struct with a `__call` method, but a parameter that receives it is typed `fn(i32) -> i32`:

```wado
fn apply(f: fn(i32) -> i32, x: i32) -> i32 {
    return f(x);
}

fn run() {
    let result = apply(|x| x * 2, 5);
}
```

Some form of dispatch has to reconcile the two.

## Decision

### Monomorphize where the type is known, canonicalize where it is erased

A closure whose concrete type is known at the use site — parameter position, unifiable return, generic-bound parameter — monomorphizes onto that functor. Dispatch is static and the body is inlinable.

A closure whose use erases its type — struct field, mixed-branch return, container element — is wrapped in a canonical `(env, funcref)` struct instead.

Escape analysis in the lower phase decides this per local: a local appearing only as a call's callee keeps the specialised form; every other position forces canonicalisation. The choice is invisible at the source level, since both satisfy the same `fn(T) -> U`.

Boxed trait objects (`dyn Fn`) were rejected. Wado has GC and needs no `Box`, and the canonical struct already fills that role without user-visible syntax.

### `fn` / `fn mut` is a type-system distinction only

There are no `Fn` / `FnMut` traits. `ResolvedType::Function` carries an `is_mut` flag, `check_assignable` enforces `fn <: fn mut` structurally, and a `<F: fn(...)>` bound binds `F` directly to the bound's function type rather than to a trait.

Wasm has no shared-versus-exclusive reference distinction, so both compile to the same `call_ref`. What the split buys is a check at the call site: a `fn mut` callee requires the binding holding it to be `mut`.

A closure is `fn mut` when its body assigns to a captured `mut` binding. Capturing a `mut` binding it only reads leaves it `fn`.

### Captures are implicit

Captures are auto-by-reference and are not part of the closure type. They live in the environment struct, invisible to the type system, so no `captures[...]` clause on function types is needed. Mutability lives at the referenced cell, not in the env field's type.

A closure that escapes outlives the bindings it captured, so a boxing pass promotes those bindings to the heap. For a non-escaping closure the environment may be skipped entirely by inlining the call and reading the bindings as locals — an optimization, not a semantic guarantee, and one that has to be expressed as call rewriting because Wasm GC has no stack-slot references.

The `stores` annotation for non-closure reference parameters stays a separate mechanism (see [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)).

### Wasm GC representation

A closure is an environment struct paired with a funcref. Flat closure plus trampoline, defunctionalization, and CM resource handles were all rejected — respectively for not sharing mutable state, interpreter dispatch overhead, and explicit drop plus indirection.

Two structs, on two different keys:

- `__Closure_N` — one per closure literal, `(env, func)`, the specialised form.
- `CanonicalClosure_K` — one per full Wasm signature, the type-erased form.

The canonical struct takes one of two shapes, chosen per `(arity, return type)` by whether any closure of that shape is inspected:

```wat
;; Slim (default — call-only)
(type $CanonicalClosure_K (struct
  (field $env  (ref null struct))
  (field $func (ref $canonical_fn_K))))

;; Inspectable
(type $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))
  (field $inspect_alt (ref $canonical_callback_fn))))

(type $CanonicalClosure_K (sub $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))
  (field $inspect_alt (ref $canonical_callback_fn))
  (field $func        (ref $canonical_fn_K)))))
```

The subtype keeps the base's prefix and adds `func` last. `$canonical_callback_fn` is uniform across signatures; `$canonical_fn_K` is typed per `K`.

Because `$canonical_inspectable_base` is shared by every `K` of one `(arity, ret)`, every such closure casts to a single type to be inspected. The `^Inspect` / `^InspectAlt` dispatch stubs are not shared: a stub is named after the type it dispatches for, so `fn(i32) -> i32` and `fn(String) -> i32` get their own even though both cast to the same base.

A stub is synthesized as a bodyless `FunctionKind::FnCanonicalDispatch` and WIR build supplies its body, a `call_ref` through the vtable slot. Bodyless functions are skipped by the inliner and the other body walkers, so the placeholder costs nothing during optimization.

Per-literal wrappers cast the canonical `env` back to `__Closure_N` and forward to `__call`, `^Inspect`, or `^InspectAlt`. The specialised path never touches the vtable: lowering rewrites an `Inspect[Alt]` call on a known-local closure receiver into a direct call, and DCE removes the impls when nothing calls them.

### Closures cannot cross the Component Model boundary

An exported or imported signature may not carry a closure type in any position, including one buried in a container, a named struct's field, or a variant payload.

## Consequences

- A function taking `fn(...)` becomes generic, so code size grows with the number of distinct closures reaching it, in exchange for static dispatch and inlining.
- A program that never inspects a closure emits the slim shape and pays nothing for the vtable.
- A program that inspects closures of some `(arity, ret)` pays two refs on every canonical value of that shape, plus wrappers and source strings for the affected literals only.
- Effects ride the closure type's `with` clause, so a closure body is checked against its own effect set rather than the enclosing function's.

## Open

- [ ] **Detect mutation through a method call.** A closure is tagged `fn mut` from assignments to captured bindings, so mutating a capture via a `&mut self` method leaves it typed `fn`. Nested closures are not walked either.
- [ ] **Return `Iterator<Item = ...>` from adapters.** They return named structs (`IterMap<Self, U>`, `IterFilter<Self>`); the anonymous form needs the elaborator to elaborate a trait-object-style return type.
- [ ] **Effect-polymorphic iterator methods** (`<effect E>`). Closure literals already inherit caller effects, so this is convenience rather than a correctness fix.
- [ ] **`Fn` / `FnMut` as user-namable traits**, for user-defined callable types. Closure literals never need them.
- [ ] **Surface the dispatch choice in the LSP** as an inline hint on closure-typed expressions. Purely additive.

## See Also

- [Closure Implementation](./wep-2026-01-16-closure-implementation.md) — user-visible language spec.
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc)
