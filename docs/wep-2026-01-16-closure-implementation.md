# WEP: Closure Implementation

## Context

Wado has closure literals:

```wado
let f = |x| x + 1;
let g = || count += 1;
```

This WEP defines the closure type system, capture semantics, and Wasm GC representation.

The original 2026-01-16 design captured by value, exposed a single `fn` type, and avoided any `Fn`/`FnMut` distinction. Iterator API ergonomics, optimizer needs at type-erased call sites, and consistency with Rust idioms led to the redesign documented here.

### Design Goals

1. Rust-like ergonomics: automatic capture-by-reference, surface syntax close to Rust.
2. Type-level mutation info: split `fn` (read-only) from `fn mut` (mutating) so the optimizer can exploit purity on type-erased call paths.
3. Explicit dispatch: users choose static (`impl`) vs dynamic (`dyn`) at every type position.
4. Compatibility with value semantics: no `Clone`, no `move`, no `FnOnce` — closures are values like everything else.

## Closure Types

### `fn` and `fn mut`

Two closure type constructors:

- `fn(T) -> U` — read-only captures. Internal calling convention: `__call(&self, ...)`.
- `fn mut(T) -> U` — read/write captures. Internal calling convention: `__call(&mut self, ...)`.

Sub-typing: `fn(T) -> U <: fn mut(T) -> U`. A read-only closure is usable wherever a `fn mut` is expected. The reverse is not allowed.

There is no `FnOnce` analog (see "No `FnOnce`" below).

### Required Qualifiers: `impl` or `dyn`

Closure types must always appear with a dispatch qualifier:

| Position                                      | Allowed forms                       |
| --------------------------------------------- | ----------------------------------- |
| Function parameter                            | `impl fn(...)` / `impl fn mut(...)` |
| Function return                               | `impl fn(...)` / `impl fn mut(...)` |
| Storage (struct field, local type annotation) | `dyn fn(...)` / `dyn fn mut(...)`   |
| Generic bound                                 | `<F: fn(...)>` / `<F: fn mut(...)>` |

Bare `fn(T) -> U` (without `impl`/`dyn`) is a parse error in type positions.

`impl fn(...)` introduces an anonymous generic parameter, monomorphized at each call site (no indirection, inlinable). `dyn fn(...)` is type-erased, dispatched indirectly through a canonical struct (env + funcref).

### Generic Bound Syntax

`fn(...)` and `fn mut(...)` may appear as trait bounds. This is the only way to name a closure type and reuse it across multiple positions:

```wado
fn apply<F: fn(i32) -> i32>(f: F, x: i32) -> i32 { f(x) }
fn run<F: fn mut(i32)>(mut f: F) { f(1); f(2); }
fn dup<F: fn(i32) -> i32>(f: F) -> (F, F) { (f, f) }
```

The underlying trait names (`Fn`/`FnMut`) are not user-visible. The `fn` keyword itself serves as the bound name. User-defined types cannot implement these traits — only closure literals produce callables. (May be revisited later; not in scope for the MVP.)

### Mutability Binding Requirement

Calling a `fn mut` closure requires its binding to be `mut`:

```wado
let mut count = 0;
let mut c = || count += 1;   // type: fn mut() -> (); `mut c` required
c();
c();
```

For function parameters:

```wado
fn run(mut f: impl fn mut(i32)) { f(1); f(2); }
//      ^^^ required
```

Calling a `fn mut` closure through a non-`mut` binding is a compile error. This mirrors Rust's `FnMut` rule and corresponds to the `&mut self` calling convention.

`fn` closures do not require `mut`:

```wado
let count = 0;
let c = || count;             // type: fn() -> i32; no `mut`
```

Wado has no borrow checker, so this rule is conceptual rather than safety-driven. It is kept to make the calling convention visible at use sites and to maintain a one-to-one correspondence with Rust's idiom.

## Capture Semantics

### Auto-capture by Reference

Closures auto-capture outer bindings. The reference kind is inferred from body usage:

- Body only reads ⇒ `&T` capture, closure type is `fn`.
- Body writes ⇒ `&mut T` capture, closure type is `fn mut`.

```wado
let mut count = 0;
let s = "hello";

let inc = || count += 1;     // captures &mut count; type: fn mut() -> ()
let get = || count;           // captures &count; type: fn() -> i32
let greet = || println(s);    // captures &s; type: fn() with Stdout
```

Any `&mut` capture promotes the closure type to `fn mut`. Pure read-only captures keep it `fn`.

### No `move` Keyword

Wado does not have a `move` keyword. To force a value-copy snapshot at closure creation, introduce an intermediate local:

```wado
let snapshot = original;     // value semantics: deep copy
let f = || snapshot * 2;     // captures &snapshot; independent of `original`
```

### Closures Are Values

Closures follow Wado value semantics: deep-copied on assignment, parameter passing, and return.

- env fields holding reference values alias on copy.
- env fields holding value types are deep-copied with the closure.

```wado
let mut count = 0;
let c1 = || count += 1;     // env: { &mut count }
let c2 = c1;                 // env copied; both still hold &mut count
c1();
c2();
assert(count == 2);
```

### No `Clone` Trait

Wado has no `Clone` trait or `.clone()` method. Constructions like `(f, f)` auto-copy `f`:

```wado
fn dup<F: fn(i32) -> i32>(f: F) -> (F, F) { (f, f) }
```

### No `FnOnce`

Rust's `FnOnce` exists because consuming a closure can move captured values out, leaving the closure unusable. Under Wado value semantics:

- Calls never consume the closure.
- Captured values are deep copies; "moving them out" is just another copy.
- Reference captures (including resources) alias on copy.

No state of affairs requires single-use closures, so `FnOnce` is unmotivated and omitted.

## Effect System Integration

### Effect Annotations on Closure Types

Closure types carry effects with the same `with` syntax as functions:

```wado
let f: impl fn(i32) -> i32 with Stdout = |x| { println(`{x}`); x };
```

### Effect Generics

Functions that accept effectful closures use the `<effect E>` parameter form (per [Effect System Design WEP](./wep-2026-01-27-effect-system-design.md)):

```wado
fn map<B, effect E>(
    arr: Array<T>,
    f: impl fn mut(T) -> B with E,
) -> Array<B> with E { ... }
```

At most one `<effect E>` parameter per function. Effects of all closure-typed parameters are unioned into `E` at the call site.

### Effect List Parsing in Bounds

`with` consumes a single identifier by default. Multiple effects in a bound require explicit parens:

```wado
F: fn() with E                  // single effect
F: fn() with (Stdout, Stderr)    // multiple effects; parens required
F: fn() with E + Debug          // combined with another trait bound
```

The `+` separates `fn` from other bounds; `with` is greedy up to (but not including) the next `+`, `,`, or `>`.

## Iterator API Example

Iterator methods use `impl fn mut(...) with E` for closure parameters and `impl Iterator<Item = ...> with E` for adapter returns:

```wado
trait Iterator {
    type Item;

    fn map<B, effect E>(self, f: impl fn mut(Self::Item) -> B with E)
        -> impl Iterator<Item = B> with E;

    fn filter<effect E>(self, p: impl fn mut(&Self::Item) -> bool with E)
        -> impl Iterator<Item = Self::Item> with E;

    fn fold<B, effect E>(self, init: B, f: impl fn mut(B, Self::Item) -> B with E)
        -> B with E;

    fn for_each<effect E>(self, f: impl fn mut(Self::Item) with E) with E;

    fn find<effect E>(self, p: impl fn mut(&Self::Item) -> bool with E)
        -> Option<Self::Item> with E;

    fn any<effect E>(self, p: impl fn mut(Self::Item) -> bool with E)
        -> bool with E;

    fn all<effect E>(self, p: impl fn mut(Self::Item) -> bool with E)
        -> bool with E;

    fn inspect<effect E>(self, f: impl fn mut(&Self::Item) with E)
        -> impl Iterator<Item = Self::Item> with E;
}
```

Choosing `fn mut` mirrors Rust's `FnMut`: by sub-typing it accepts both pure and mutating closures, maximizing caller flexibility.

Adapter struct types (the internal `Map<I, F>`, `Filter<I, F>`, etc.) are not user-namable; iterator chains see only `impl Iterator<...>`.

## Wasm GC Representation

Closures use Option 1: an environment struct paired with a funcref. Among alternatives considered:

- Option 1 (chosen): closure struct + funcref. Native Wasm GC fit, shared mutable state via captured references, efficient field access.
- Option 2: flat closure + trampoline. Rejected: cannot share mutable state without additional indirection that re-implements Option 1.
- Option 3: defunctionalization. Rejected: interpreter dispatch overhead, scales poorly.
- Option 4: CM resource handles. Rejected: explicit drop required, handle-table indirection, awkward for internal closures.

### `impl fn` (Static Dispatch)

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

When passed to a function with an `impl fn(...)` parameter, the function is monomorphized for the specific `__Closure_N`. Calls compile to `call_ref` with a known signature, enabling inlining.

### `dyn fn` (Dynamic Dispatch)

`dyn fn(...)` uses a canonical, signature-keyed struct (env + funcref). One shape per `(arity, return type)` pair. Per-literal wrappers cast the canonical `env` back to the literal's `__ClosureEnv_N` and call `__call`. See "Canonical Closure as Vtable" below for the full layout.

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

When a closure escapes (returned, stored in a `dyn fn` field, etc.), its captured bindings must outlive the declaring function. The compiler heap-promotes them: the bindings live in a heap-allocated struct that the closure env references.

For non-escaping closures, the compiler MAY skip heap promotion and reuse locals (with the env referencing those locals' stack slots). This is an optimization, not part of the language semantics.

### Calling Convention vs Wasm

Wasm has no notion of shared vs exclusive references. The `fn` / `fn mut` distinction is type-system only, not Wasm-level. Both compile to identical `call_ref` instructions. The type checker enforces:

- `fn` callees: caller may have shared access to the closure binding.
- `fn mut` callees: caller must hold a `mut` binding (giving exclusive access conceptually).

## Canonical Closure as Vtable

A `dyn fn` value is wrapped in a canonical, signature-keyed struct so any holder of an `Fn<N, Ret>` value can dispatch through a uniform shape. The lower-phase escape analysis decides per local: a closure local that only appears as the callee of `IndirectCall` / receiver of `MethodCall` keeps the specialised `&__Closure_N` form; every other position forces canonicalisation.

The canonical struct comes in two shapes, selected per-`(N, Ret)` by a pre-WIR scan of `Fn<N, Ret>^Inspect[Alt]` call sites:

Slim shape (default — `Fn::call` only):

```wat
(type $CanonicalClosure_K (struct
  (field $env  (ref null struct))
  (field $func (ref $canonical_fn_K))))
```

Inspectable shape (`Fn^Inspect` / `Fn^InspectAlt` referenced for this signature):

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

The subtype keeps the shared prefix — `env`, `inspect`, `inspect_alt` — and adds `func` last. `$canonical_callback_fn` has signature `(param $env (ref null struct)) (param $f (ref null struct))` (uniform across all signatures); `$canonical_fn_K` has signature `(param $env (ref null struct)) (param $p_0 ...) ... (result ...)` (per-`K`, typed). The shared base means a single dispatch stub serves every parameter shape with the same `(N, Ret)`: distinct function types like `fn(i32) -> i32` and `fn(String) -> i32` cast to one common type, eliminating any need for per-signature dispatch tables.

Per-literal wrappers (registered in WIR build for every functor `N` whose signature is inspectable):

1. `__closure_wrapper_N` — casts `env` to `(ref $__Closure_N)` and forwards to `__call`.
2. `__closure_inspect_wrapper_N` — casts `env`, calls `__Closure_N^Inspect::inspect`.
3. `__closure_inspect_alt_wrapper_N` — casts `env`, calls `__Closure_N^InspectAlt::inspect_alt`.

Trait dispatch from the generic `Fn<N, Ret>^InspectAlt::inspect_alt` impl:

```wat
;; Fn<N, Ret>^InspectAlt::inspect_alt(self, f)
(local $b (ref $canonical_inspectable_base))
(local.set $b (ref.cast (ref $canonical_inspectable_base) (local.get $self)))
(call_ref $canonical_callback_fn
  (struct.get $canonical_inspectable_base $env         (local.get $b))
  (local.get $f)
  (struct.get $canonical_inspectable_base $inspect_alt (local.get $b)))
```

The dispatch stub is auto-derived as a `FunctionKind::FnCanonicalDispatch` TIR placeholder with no body — WIR build supplies the instructions above. A bodyless TIR function is naturally skipped by the inliner, monomorphisation, and other body walkers, so the placeholder costs nothing during optimisation.

Programs that never inspect closures emit the slim shape and incur no extra fields, wrappers, or source-string constants. Programs that inspect closures of some `(N, Ret)` pay two refs per canonical value of that signature plus per-literal wrappers and source-string constants for the affected literals only. The `__Closure_N^Inspect[Alt]` impls are TIR-rooted from `ClosureToCanonical` only when the corresponding `(N, Ret)` is inspected.

The specialised path (closure local stays as `&__Closure_N`) does not use the vtable: a redirect at the lowering stage rewrites `Fn<N, Ret>^Inspect[Alt]` calls on known-local receivers to direct calls on `__Closure_N^Inspect[Alt]`, and standard DCE removes those impls when unused.

## Component Model Boundary

Closures cannot cross the Component Model boundary (CM has no closure type). MVP: compile error when attempting to export or import a function with a closure-typed parameter.

Future: resource adapter — wrap closures as CM resources with a `call` method backed by a runtime handle table.

## Comparison with Rust

| Aspect                     | Rust                             | Wado                                |
| -------------------------- | -------------------------------- | ----------------------------------- |
| Closure literal            | `\|x\| ...`                      | `\|x\| ...`                         |
| `move` keyword             | yes                              | no (use intermediate local)         |
| Auto-capture               | `&` / `&mut` / move              | `&` / `&mut` only                   |
| Read/write split           | `Fn` / `FnMut` traits            | `fn` / `fn mut` keywords            |
| Consume-only variant       | `FnOnce`                         | none                                |
| Bound syntax               | `F: Fn(T) -> U`                  | `F: fn(T) -> U`                     |
| Static dispatch param      | `impl Fn(T) -> U`                | `impl fn(T) -> U`                   |
| Type-erased                | `dyn Fn(T) -> U`                 | `dyn fn(T) -> U`                    |
| Bare type position         | `fn(T) -> U` is function pointer | parse error                         |
| `mut` binding for mutating | required                         | required                            |
| User-implementable trait   | unstable (`fn_traits`)           | not allowed                         |
| `Clone`                    | required trait                   | none (auto-copy)                    |
| Function pointer           | separate type                    | collapsed (closures with empty env) |
| Lifetimes                  | tied to captured borrows         | none (GC)                           |

## Implementation Plan

1. Capture analysis: per-binding, decide `&T` vs `&mut T` from body usage; classify closure type as `fn` or `fn mut`.
2. Type system:
   - `fn` and `fn mut` as distinct type constructors with `fn <: fn mut` sub-typing.
   - Enforce `impl` / `dyn` qualifier requirement in type positions.
   - Enforce `mut` binding rule for `fn mut` callees.
   - Allow `fn` / `fn mut` in bound positions (parser + resolver).
   - Integrate `<effect E>` effect generics in closure parameters.
3. Codegen:
   - Generate `__Closure_N` struct + funcref.
   - Monomorphize `impl fn(...)` parameters per call site.
   - For `dyn fn`, generate canonical shape + per-literal wrappers.
   - Heap promotion for escaping captures.
4. Parser:
   - Accept `fn mut` as a two-token type/bound form.
   - Accept `impl` / `dyn` qualifiers.
   - Parse `with` in bounds, with parens required for multi-effect lists.

## Migration Plan

The current implementation reflects an earlier design: bare `fn(T) -> U` everywhere, capture-by-value with explicit `&mut || ...` syntax for mutating closures, a single `Function` type constructor with no `fn` / `fn mut` split, and escape analysis (not user qualifiers) driving static vs dynamic dispatch. The Wasm codegen layer (`__Closure_N` struct + funcref, canonical closure with optional inspectable supertype, `FnCanonicalDispatch` stub) is already aligned with this design and continues to apply unchanged below the dispatch-choice point.

The transition proceeds in phases, each leaving the compiler in a green state. Granular task tracking with current file / line references lives in [Closure Parameter Monomorphization](./wep-2026-01-25-closure-parameter-monomorphization.md).

### Phase 1: Parser surface (permissive)

Accept `impl fn(...)`, `dyn fn(...)`, `fn mut(...)`, and bound-position `<F: fn(...)>` / `<F: fn mut(...)>`, while still allowing bare `fn(...)` in type positions. Add `dyn` to the lexer keyword table; remove the unused `move` reservation.

### Phase 2: Internal `FnMut` trait

Declare `FnMut<Args, Ret, Effects>` as a sub-trait of `Fn` in `core:prelude`. Wire bound `fn` / `fn mut` syntax to resolve to these internal trait references. (`Fn` already exists but is unused as a bound today.)

### Phase 3: Type-system split

Add an `is_mut` flag to `ResolvedType::Function`. Implement `fn <: fn mut` sub-typing in `check_assignable`.

### Phase 4: Auto-capture by reference

In the resolver, walk the closure body and classify each captured binding as `&T` (read-only) or `&mut T` (mutating). Tag the closure type as `fn` or `fn mut` based on whether any capture is mutating. Retire the existing `&mut || ...` desugar.

### Phase 5: `mut` binding enforcement

In `IndirectCall` / `MethodCall` resolution, require the callee binding to be `let mut` (or `mut f:` parameter) when the closure type is `fn mut`.

### Phase 6: Effect-check fix

Closure bodies are checked against the closure's own declared effect set, not the enclosing function's. Resolves the leak documented in `closure_escapes_effect_todo.wado`.

### Phase 7: Stdlib `Iterator` migration

Convert iterator methods to `impl fn mut(...) with E` with `<effect E>` parameters. Add `for_each`. Adopt `impl Iterator<Item = ...>` returns where adapter-struct naming is not needed externally.

### Phase 8: Codegen qualifier honoring

Thread user-written `impl` vs `dyn` qualifier into the lower-phase `specializable` decision so the user's intent (not just escape analysis) drives static vs dynamic dispatch. The mature codegen below this layer (`__Closure_N`, `ClosureToCanonical`, `FnCanonicalDispatch`) requires no changes.

### Phase 9: Strict bare-`fn(...)` rejection

Flip the parser / resolver from permissive to strict — bare `fn(T) -> U` in type positions becomes a compile error. Migrate remaining fixtures and stdlib references (`String::find_char`, `Array::sort_by` / `sorted_by`, `Benchmark::run`, serde / json / router `lookup` parameters).

### Phase 10: CM boundary error

Compile-error fixture for exporting or importing a function with a closure-typed parameter across the Component Model boundary.

### Phase Ordering Rationale

- Phases 1-2 are infrastructure that unblock everything else and ship without behaviour changes.
- Phases 3-5 form the semantic core; they must be done together because the `mut`-binding rule requires the type-system split, which requires the capture classification.
- Phase 6 is independent of 3-5 and can be parallelised; it has its own TODO fixture as a regression gate.
- Phase 7 (stdlib migration) is large but mechanical, gated by 3-5 being in place.
- Phase 8 is the hand-off to the existing codegen; small and localised.
- Phase 9 is the breaking change; deferring it last allows incremental migration of fixtures and external callers.
- Phase 10 is purely additive.

## Consequences

### Positive

1. Familiar to Rust users with minimal mental gap.
2. Optimizer has mutation/purity information at type-erased call sites.
3. Explicit `impl` / `dyn` — no implicit dispatch surprises.
4. Sub-typing `fn <: fn mut` — pure closures fit everywhere a mutating one does.
5. Effect transparency via `<effect E>` keeps iterator API readable.
6. Fewer concepts than Rust (no `Clone`, no `move`, no `FnOnce`, no separate `fn` pointer type).

### Negative

1. Verbose `impl` / `dyn` annotations in API signatures.
   - Mitigation: sugar covers common cases; named `<F: fn(...)>` bound only when needed.
2. Adapter struct types are not user-namable.
   - Mitigation: `impl Iterator<...>` returns avoid the need to name them.
3. `mut` binding rule is "ceremonial" without a borrow checker.
   - Mitigation: kept for conceptual consistency with `&mut self` calling convention.
4. Closures cannot cross Component Model boundary (MVP).
   - Mitigation: documented; resource adapter as future work.

## Future Work

- User-defined callables (`impl fn(...) for SomeStruct`): low priority.
- Resource adapter for closures at the Component Model boundary.
- `with ..` effect-polymorphism shorthand if `<effect E>` verbosity proves painful.
- Inlining heuristics for `impl fn(...)` parameters with small bodies.
- Auto-switch between type-erased (`dyn fn`) and monomorphized (`impl fn`) forms based on whether the closure escapes (iterator-chain fusion).

## References

- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Closure Parameter Monomorphization](./wep-2026-01-25-closure-parameter-monomorphization.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Iterator Traits Design](./wep-2026-01-24-iterator-traits.md)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc)
- [Rust Closure Implementation](https://doc.rust-lang.org/book/ch13-01-closures.html)
