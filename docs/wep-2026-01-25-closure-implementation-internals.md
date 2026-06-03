# WEP: Closure Implementation Internals

## Status: Implemented

Implementation details for the user-visible closure design specified in [Closure Implementation](./wep-2026-01-16-closure-implementation.md). This WEP covers the internal `Fn` / `FnMut` trait machinery, the Wasm GC representation (specialised functor structs and canonical-closure vtable), and the migration plan from the current capture-by-value implementation.

## Context

When closures are passed as function parameters, there is a type compatibility challenge:

```wado
fn apply(f: fn(i32) -> i32, x: i32) -> i32 {
    return f(x);
}

fn run() {
    let result = apply(|x| x * 2, 5);
}
```

After closure lowering, `|x| x * 2` becomes a functor struct `__Closure_0` with a `__call` method. However, `apply` expects a `fn(i32) -> i32` parameter, creating a type mismatch that must be resolved by some form of dispatch.

## Options Considered

### Option A: Dynamic Dispatch with CanonicalClosure

Transform closures to a canonical closure format at the call site:

- `CanonicalClosure` struct: `(env: ref struct, funcref: ref func)`
- Generate wrapper functions that bridge functor `__call` to the canonical calling convention
- `IndirectCall` uses `call_ref` with the funcref

Pros: single function implementation regardless of closure type; smaller code size.

Cons: runtime overhead from indirect calls; cannot inline closure bodies; complex wrapper generation in codegen.

### Option B: Trait Objects (`dyn Fn`)

Introduce `dyn Trait` support with vtables:

- Define `Fn<Args, Ret, Effects>` trait
- `fn(A) -> B` becomes `Box<dyn Fn<[A], B, []>>`
- Closures implement `Fn` and are boxed when passed

Pros: general solution for all trait objects; unified model for dynamic dispatch.

Cons: requires full vtable implementation; runtime overhead from vtable lookup; memory overhead from boxing; significant implementation effort.

### Option C: Monomorphization

Desugar `fn(A) -> B` parameters to generic parameters with trait bounds:

```wado
// User writes:
fn apply(f: fn(i32) -> i32, x: i32) -> i32 {
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

Pros: no runtime overhead (static dispatch); enables inlining of closure bodies; leverages existing monomorphization infrastructure; conceptually clean (closures are just structs with methods).

Cons: code size increase from specialization; requires trait bounds implementation.

## Decision

The implementation uses a hybrid driven by escape analysis:

- Option C (monomorphization) is the primary path for parameter-position and other type-known uses.
- Option A (canonical closure) is used automatically for type-erased contexts: struct fields, mixed-branch returns, container elements.

Option B (`dyn Fn` boxed trait objects) is rejected — Wado has GC and does not need `Box`. The canonical struct from Option A serves the role of `dyn Fn` without an explicit user-visible syntax.

## Internal `Fn` / `FnMut` Trait Design

These traits are compiler-internal and not exposed to user code (per [Closure Implementation](./wep-2026-01-16-closure-implementation.md)). Users write `fn(...)`, `fn mut(...)`, or `<F: fn(...)>`; the compiler desugars these to bounds on the internal traits below.

### Signatures

```wado
trait FnMut<Args, Ret, Effects = []> {
    fn call_mut(&mut self, args: Args) -> Ret with Effects;
}

trait Fn<Args, Ret, Effects = []>: FnMut<Args, Ret, Effects> {
    fn call(&self, args: Args) -> Ret with Effects;
}
```

The `Effects` parameter defaults to `[]` (pure), so `Fn<[i32], bool>` is shorthand for `Fn<[i32], bool, []>`.

Sub-typing `fn <: fn mut` is realized as `Fn` being a sub-trait of `FnMut`.

### Type Parameter Semantics

| Parameter | Description                                  | Example                   |
| --------- | -------------------------------------------- | ------------------------- |
| `Args`    | Tuple of argument types using `[...]` syntax | `[i32, String]`           |
| `Ret`     | Return type                                  | `bool`                    |
| `Effects` | Tuple of effect types using `[...]` syntax   | `[Stdout]`, `[]` for pure |

### Mapping from `fn` / `fn mut` Types

| User-visible type             | Internal bound (short) | Internal bound (full)     |
| ----------------------------- | ---------------------- | ------------------------- |
| `fn() -> R`                   | `Fn<[], R>`            | `Fn<[], R, []>`           |
| `fn(A) -> R`                  | `Fn<[A], R>`           | `Fn<[A], R, []>`          |
| `fn(A, B) -> R`               | `Fn<[A, B], R>`        | `Fn<[A, B], R, []>`       |
| `fn(A) -> R with E`           | —                      | `Fn<[A], R, [E]>`         |
| `fn(A, B) -> R with (E1, E2)` | —                      | `Fn<[A, B], R, [E1, E2]>` |
| `fn mut(A) -> R`              | `FnMut<[A], R>`        | `FnMut<[A], R, []>`       |
| `fn mut(A) -> R with E`       | —                      | `FnMut<[A], R, [E]>`      |

### Example: Effectful Closure

```wado
// User writes:
fn for_each<effect E>(items: List<i32>, mut f: fn mut(i32) with E) with E {
    for let item of items {
        f(item);
    }
}

// Internally desugars to:
fn for_each<F: FnMut<[i32], (), [E]>, effect E>(items: List<i32>, mut f: F) with E {
    for let item of items {
        f.call_mut([item]);
    }
}
```

## Captures are Implicit

Closure captures (auto-by-reference per the user-visible spec) are not encoded in the closure type. The previously proposed `captures[...]` clause on function types is obsolete — capture information lives in the closure's environment struct and is invisible to the type system. Escape analysis handles heap promotion when captured bindings outlive the declaring scope.

The `stores` annotation for non-closure reference parameters remains as a separate mechanism (see [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)); it is not unified with the `Fn` / `FnMut` trait family.

## Wasm GC Representation

Closures use Option 1: an environment struct paired with a funcref. Among alternatives considered: flat closure + trampoline (rejected: cannot share mutable state without re-implementing Option 1), defunctionalization (rejected: interpreter dispatch overhead), CM resource handles (rejected: explicit drop required, indirection overhead).

### Static Dispatch (Specialised Path)

Each closure literal generates an anonymous struct type `__Closure_N`:

```wat
;; Environment with captured references
(type $__ClosureEnv_N (struct
  (field $cap_0 (ref $T_0))      ;; &T   (immutable capture)
  (field $cap_1 (ref $T_1))      ;; &mut T (mutability of the referenced cell)
  ...))

(type $__ClosureFn_N (func
  (param $env (ref $__ClosureEnv_N))
  (param $p_0 <T>) ... (result <R>)))

(type $__Closure_N (struct
  (field $env (ref $__ClosureEnv_N))
  (field $func (ref $__ClosureFn_N))))
```

When the compiler determines that a closure use is type-known (parameter position, return position with unifiable branches, generic-bound parameter), the function is monomorphized for the specific `__Closure_N`. Calls compile to `call_ref` with a known signature, enabling inlining.

### Dynamic Dispatch (Canonical Path)

When the compiler determines that a closure use must be type-erased (struct field, mixed-branch return, container element, etc.), the value is wrapped in a canonical, signature-keyed struct `(env, funcref)`. One canonical struct per full Wasm function signature (parameter types + return type). Per-literal wrappers cast the canonical `env` slot back to the literal's `__Closure_N` functor struct and forward to `__call`. See "Canonical Closure as Vtable" below for the full layout.

### Dispatch Choice

The choice between specialised and canonical paths is made by escape analysis in the lower phase: a closure local that only appears as the callee of a direct/indirect call keeps the specialised `__Closure_N` form; every other position (storage, mixed-branch return, container insertion, escape across a `dyn`-shaped trait) forces canonicalisation. This decision is invisible at the source level — both paths satisfy the same `fn(T) -> U` type.

### Closure Creation

```wat
;; let f = |x| x + count
(struct.new $__ClosureEnv_N
  (local.get $count_ref))           ;; capture &count
(local.set $env)

(struct.new $__Closure_N
  (local.get $env)
  (ref.func $__closure_impl_N))
(local.set $f)
```

### Closure Invocation

```wat
;; f(10)
(local.get $f)
(struct.get $__Closure_N $env)
(i32.const 10)
(local.get $f)
(struct.get $__Closure_N $func)
(call_ref $__ClosureFn_N)
```

### Capture Lowering

For each captured binding:

1. The compiler analyzes the closure body and decides `&T` (read-only) or `&mut T` (mutating).
2. The env field is typed accordingly.
3. At closure creation, the reference to the outer binding is loaded and stored in the env.

Mutability lives at the referenced cell, not in the env field type itself. The `fn` vs `fn mut` distinction is type-system only — both compile to the same `call_ref` instruction.

### Heap Promotion for Escaping Captures

When a closure escapes (returned, stored in a struct field, etc.), its captured bindings must outlive the declaring function. The compiler heap-promotes them: the bindings live in a heap-allocated struct that the closure env references.

For non-escaping closures, the compiler MAY skip the heap-allocated environment entirely by inlining the call into the surrounding function and accessing the captured bindings directly as locals (no env struct allocated). Wasm GC has no notion of stack-slot references, so the optimization must be expressed as direct-call rewriting (or equivalent inlining), not as an env field pointing to a local. This is an optimization, not part of the language semantics.

### Calling Convention vs Wasm

Wasm has no notion of shared vs exclusive references. The `fn` / `fn mut` distinction is type-system only, not Wasm-level. Both compile to identical `call_ref` instructions. The type checker enforces:

- `fn` callees: caller may have shared access to the closure binding.
- `fn mut` callees: caller must hold a `mut` binding (giving exclusive access conceptually).

## Canonical Closure as Vtable

A closure value that needs to be type-erased is wrapped in a canonical, signature-keyed struct so any holder of a canonical closure of a given signature can dispatch through a uniform shape. The lower-phase escape analysis decides per local: a closure local that only appears as the callee of `IndirectCall` / receiver of `MethodCall` keeps the specialised `&__Closure_N` form; every other position forces canonicalisation.

Three indices appear in this section:

- `N` — per-literal closure index, naming the specialised functor (`__Closure_N`) and its per-literal wrappers.
- `K` — full Wasm signature key (parameter WIR types + result WIR types), naming the canonical struct (`CanonicalClosure_K`) and its typed function reference (`canonical_fn_K`). Closures of different `N` with identical Wasm signatures share the same `K`.
- `(arity, return type)` — the inspectable-shape gate. The decision to use the slim shape or the inspectable shape is made per `(arity, ret)`, not per `K`. All `CanonicalClosure_K` whose signatures share the same `(arity, ret)` use the same gate decision and the same `$canonical_inspectable_base` supertype.

The canonical struct comes in two shapes, selected per `(arity, return type)` by a pre-WIR scan of `Inspect` / `InspectAlt` call sites:

Slim shape (default — call-only):

```wat
(type $CanonicalClosure_K (struct
  (field $env  (ref null struct))
  (field $func (ref $canonical_fn_K))))
```

Inspectable shape (when `Inspect` / `InspectAlt` is referenced for any closure of this `(arity, ret)`):

```wat
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

The subtype keeps the shared prefix — `env`, `inspect`, `inspect_alt` — and adds `func` last. `$canonical_callback_fn` has signature `(param $env (ref null struct)) (param $f (ref null struct))` (uniform across all signatures); `$canonical_fn_K` has signature `(param $env (ref null struct)) (param $p_0 ...) ... (result ...)` (per-`K`, typed). Because the inspectable supertype `$canonical_inspectable_base` is shared across all `K` of the same `(arity, ret)`, the inspect dispatch stub for those signatures is also shared: distinct function types like `fn(i32) -> i32` and `fn(String) -> i32` (different `K`, both `(arity=1, ret=i32)`) `ref.cast` to the same `$canonical_inspectable_base` for inspect calls, even though their `func` fields and `call` dispatch remain per-`K`.

Per-literal wrappers (registered in WIR build for every closure literal `N` whose signature is inspectable):

1. `__closure_wrapper_N` — casts `env` to `(ref $__Closure_N)` and forwards to `__call`.
2. `__closure_inspect_wrapper_N` — casts `env`, calls `__Closure_N^Inspect::inspect`.
3. `__closure_inspect_alt_wrapper_N` — casts `env`, calls `__Closure_N^InspectAlt::inspect_alt`.

Trait dispatch from the canonical `InspectAlt::inspect_alt` impl (one per `(arity, ret)`, shared across all `K` with that signature shape):

```wat
;; CanonicalInspectable^InspectAlt::inspect_alt(self, f)
(local $b (ref $canonical_inspectable_base))
(local.set $b (ref.cast (ref $canonical_inspectable_base) (local.get $self)))
(call_ref $canonical_callback_fn
  (struct.get $canonical_inspectable_base $env         (local.get $b))
  (local.get $f)
  (struct.get $canonical_inspectable_base $inspect_alt (local.get $b)))
```

The dispatch stub is auto-derived as a `FunctionKind::FnCanonicalDispatch` TIR placeholder with no body — WIR build supplies the instructions above. A bodyless TIR function is naturally skipped by the inliner, monomorphisation, and other body walkers, so the placeholder costs nothing during optimisation.

Programs that never inspect closures emit the slim shape and incur no extra fields, wrappers, or source-string constants. Programs that inspect closures of some `(arity, ret)` pay two refs per canonical value of any signature in that shape, plus per-literal wrappers and source-string constants for the affected literals only. The `__Closure_N^Inspect[Alt]` impls are TIR-rooted from `ClosureToCanonical` only when the corresponding `(arity, ret)` is inspected.

The specialised path (closure local stays as `&__Closure_N`) does not use the vtable: a redirect at the lowering stage rewrites canonical `Inspect[Alt]` calls on known-local receivers to direct calls on `__Closure_N^Inspect[Alt]`, and standard DCE removes those impls when unused.

## Migration Plan

The current implementation reflects an earlier design: capture-by-value with explicit `&mut || ...` syntax for mutating closures, a single `Function` type constructor with no `fn` / `fn mut` split. The Wasm codegen layer (`__Closure_N` struct + funcref, canonical closure with optional inspectable supertype, `FnCanonicalDispatch` stub, escape-analysis-driven specialised vs canonical dispatch) is already aligned with the new design and continues to apply unchanged.

The transition proceeds in phases, each leaving the compiler in a green state. File references below point at the current main tree; line numbers are approximate and may drift as unrelated changes land.

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

### Phase 1: Parser surface

Accept `fn mut(...)` as a two-token type form. Accept `fn(...)` / `fn mut(...)` in bound position (`<F: fn(...)>` / `<F: fn mut(...)>`). Parse `with (E1, E2)` parens-grouped multi-effect in bound contexts. Remove the unused `move` keyword reservation.

Tasks:

- [x] Parse `fn mut(...)` as a two-token form; add `is_mut: bool` to `FunctionType` (`ast.rs`, `parser.rs::parse_type`)
- [x] Parse `fn(...)` / `fn mut(...)` in trait bound position; carried via `TraitBound.fn_signature` (`parser.rs::parse_trait_bound`)
- [x] Parse `with (E1, E2)` parens-grouped multi-effect in bound contexts (`parser.rs::parse_fn_type_for_bound`, `parse_bound_with_clause`); `unparse` mirrors the form via `unparse_fn_signature_in_bound`
- [x] Remove the unused `move` keyword reservation (`lexer.rs`, `token.rs`, `syntax.rs`)

### Phase 2: Internal `FnMut` trait

Add `FnMut<Args, Ret, Effects>` as the base trait in `core:prelude`, and re-declare `Fn` as a sub-trait of `FnMut` so that `fn <: fn mut` holds at the trait-bound level. Wire bound `fn` / `fn mut` syntax to resolve to these internal trait references. The current prelude `Fn` declaration is `pub trait Fn<Args, Ret, Effects>` (no `Effects` default, no supertrait); both are added by this phase.

Wado's parser does not yet support a supertrait clause on trait declarations (no existing stdlib trait uses `trait X<...>: Y<...>` form). Adding parser support is part of this phase. If supertrait syntax proves invasive, an alternative is to keep `Fn` and `FnMut` as two independent traits and emit the `Fn ⇒ FnMut` blanket impl from the compiler (per closure-literal lowering), with `check_assignable` enforcing the `fn <: fn mut` rule directly without going through trait-bound inheritance. Either path satisfies the design.

Status: the implementation followed the alternative path — `check_assignable` enforces `fn <: fn mut` directly on `ResolvedType::Function`, and `<F: fn(...)>` bounds are realised at `register_generic_params` time by binding `F` to the bound's function type. The internal `Fn` / `FnMut` traits as separate user-namable items are unused; closure literals never need to implement them.

Tasks:

- [x] `check_assignable` enforces `fn <: fn mut` directly (`elaborator/typecheck.rs`)
- [x] `register_generic_params` resolves `<F: fn(...)>` to the bound's function type (`elaborator/trait_env.rs`)
- [ ] Optional follow-up: re-introduce the stdlib `Fn` / `FnMut` traits with `Effects = []` default and supertrait inheritance for user-defined callable types (deferred — not needed for closure literals)

### Phase 3: Type-system split

Add an `is_mut` flag to `ResolvedType::Function`. Implement `fn <: fn mut` sub-typing in `check_assignable`.

Tasks:

- [x] Add `is_mut: bool` to `ResolvedType::Function` (`tir.rs`)
- [x] Sub-typing rule in `check_assignable` (`elaborator/typecheck.rs`)
- [x] Update `make_function` / TIR creation sites; type stringification (`tir.rs::type_name`, `unparse.rs`)

### Phase 4: Auto-capture by reference

In the elaborator, walk the closure body and classify each captured binding as `&T` (read-only) or `&mut T` (mutating). Tag the closure type as `fn` or `fn mut` based on whether any capture is mutating. Retire the existing `&mut || ...` desugar.

Tasks:

- [x] Walk closure body in `resolve_closure` to classify outer-name usage; emit the `&mut __ref_*` ref desugar inline (`elaborator/closure.rs`)
- [x] Tag closure type `is_mut = any(capture.is_mut)`
- [x] Retire `&mut || ...` source form. The special-case dispatch to `resolve_mutable_closure` is gone, and `resolve_mutable_closure` itself is deleted. The syntax now falls through to the ordinary unary path, where the natural type / binding error covers it (e.g. `&mut <closure value>` produces `&mut fn mut(...)` which then trips the regular type-mismatch / `mut`-binding checks downstream). A dedicated `AmpMutClosureSyntaxRetired` diagnostic was prototyped and dropped: no Wado users exist yet, so a migration window is unnecessary.
- [x] Migrate fixtures `closure_2.wado`, `closure_3.wado` away from `&mut ||`

### Phase 5: `mut` binding enforcement

In `IndirectCall` / `MethodCall` resolution, require the callee binding to be `let mut` (or `mut f:` parameter) when the closure type is `fn mut`.

Tasks:

- [x] In `IndirectCall` construction (`elaborator/call.rs`), check whether the local holding the callee was bound `let mut`; emit `ClosureMutBindingRequired` if not and the callee is `fn mut`
- [x] Same check applies to function parameters via the same code path (`mut f:` makes the param a `mut` local)
- [x] Compile-error fixtures (`closure_mut_binding_required_error.wado`, `closure_mut_param_required_error.wado`)

### Phase 6: Effect-check fix

Closure bodies are checked against the closure's own declared effect set, not the enclosing function's. Resolves the leak documented in `closure_escapes_effect_todo.wado`.

Tasks:

- [x] `TirExprKind::Closure` gains `declared_effects: Option<Vec<EffectRef>>` populated from the use-site's expected type. `effect_check.rs` swaps `current_effects` to those when entering the body
- [x] `extract_expected_fn` only adopts effect sets that are entirely in-scope concrete effects — `EffectRef::Param` and stale `Concrete` entries from generic-effect call sites fall back to inheriting outer effects, preserving the `<effect E>` generic-call path
- [x] Lift TODO marker from `closure_escapes_effect_todo.wado`

### Phase 7: Stdlib `Iterator` migration

Convert iterator methods to `fn mut(...)` closure parameters. Add `for_each`. Update return types to `Iterator<Item = ...>` where adapter-struct naming is not needed externally.

Tasks:

- [x] Convert `lib/core/prelude/traits.wado` iterator methods to `fn mut(...)`: `map`, `filter`, `fold`, `find`, `any`, `all`, `position`, `reduce`
- [x] Add `for_each` method
- [ ] Update return types from named adapter structs (`IterMap<Self, U>` etc.) to `Iterator<Item = ...>` where the adapter type does not need to be user-named (deferred — requires the elaborator to elaborate the trait-object-style return type, out of scope for this PR)
- [x] Migrate other bare-`fn(...)` stdlib references where the closure should be `fn mut`: `String::find_char`, `List::sort_by` / `sorted_by`, `Benchmark::run`, serde `lookup` (`serde.wado`, `json.wado`, `router.wado`)
- [ ] Add `<effect E>` effect-polymorphism to iterator methods (deferred — closure literals already inherit caller effects, so this is additive convenience rather than a correctness fix)

### Phase 8: CM boundary error

Compile-error fixtures for any closure-typed component crossing the Component Model boundary — parameter, return type, or buried inside container types (`Option<fn(...)>`, struct fields in CM-exported records, etc.).

Tasks:

- [x] Reject exporting a function with a closure-typed parameter or return type, including closures buried inside refs / arrays / generic containers (`elaborator/item.rs::type_contains_closure`)
- [x] Reject importing a function (`#[canonical(...)]` / `#[cm(...)]` declaration with no body) carrying a closure type in any signature position
- [x] Reject closure-typed fields in CM-exported record types — `type_contains_closure` recurses through named-struct fields via `lookup_struct_fields`. Variant payloads currently route through generic-arg containers, so the same check fires for them
- [x] Fixtures: export-param, export-return, struct-field, and import side rejection cases

### Phase 9 (Optional): LSP dispatch hints

Surface the compiler's specialised vs canonical dispatch decision as an inline hint / hover annotation in `wado-lsp/`. This is purely additive UX; the language design does not depend on it.

Tasks:

- [ ] In `wado-lsp/`, surface the specialised-vs-canonical dispatch decision from `lower/plan/closure.rs::specializable` as an inline hint or hover annotation on closure-typed expressions
- [ ] Purely additive UX; not required for the language design to be complete

### Phase Ordering Rationale

- Phases 1-2 are infrastructure that unblock everything else and ship without behaviour changes.
- Phases 3-5 form the semantic core; they must be done together because the `mut`-binding rule requires the type-system split, which requires the capture classification.
- Phase 6 is independent of 3-5 and can be parallelised; it has its own TODO fixture as a regression gate.
- Phase 7 (stdlib migration) is large but mechanical, gated by 3-5 being in place.
- Phase 8 is purely additive.
- Phase 9 ships any time after Phase 3 lands.

Codegen needs no changes: dispatch choice is already done by escape analysis, and the new `fn` / `fn mut` distinction lives entirely in the type system, not in calling convention or wire format.

## Consequences

- Functions with `fn(...)` / `fn mut(...)` parameters become generic, increasing monomorphization
- Closure calls inside such functions are static method calls, enabling optimization
- Code size may increase but runtime performance improves
- Effects are preserved through the internal trait's effect parameter
- The canonical `(env, funcref)` path is used automatically for type-erased contexts; no user syntax is required

## See Also

- [Closure Implementation](./wep-2026-01-16-closure-implementation.md) — user-visible language spec.
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc)
