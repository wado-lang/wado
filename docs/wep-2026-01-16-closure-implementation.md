# WEP: Closure Implementation

## Context

Wado has closure literals:

```wado
let f = |x| x + 1;
let g = || count += 1;
```

This WEP defines the user-visible closure type system, capture semantics, and effect integration. Compiler-internal details (Wasm GC representation, internal `Fn` / `FnMut` trait machinery, canonical-closure vtable, migration plan from the current implementation) live in [Closure Implementation Internals](./wep-2026-01-25-closure-implementation-internals.md).

The original 2026-01-16 design captured by value, exposed a single `fn` type, and avoided any `Fn` / `FnMut` distinction. Iterator API ergonomics, optimizer needs at type-erased call sites, and consistency with Rust idioms led to the redesign documented here.

### Design Goals

1. Rust-like ergonomics: automatic capture-by-reference, surface syntax close to Rust.
2. Type-level mutation info: split `fn` (read-only) from `fn mut` (mutating) so the optimizer can exploit purity on type-erased call paths.
3. Compiler-managed dispatch: the compiler decides static vs dynamic dispatch by context (escape analysis); users do not annotate this choice. The LSP surfaces the decision as inline hints / hover information for those who care.
4. Compatibility with value semantics: no `Clone`, no `move`, no `FnOnce` — closures are values like everything else.

## Closure Types

### `fn` and `fn mut`

Two closure type constructors:

- `fn(T) -> U` — read-only captures.
- `fn mut(T) -> U` — read/write captures.

Sub-typing: `fn(T) -> U <: fn mut(T) -> U`. A read-only closure is usable wherever a `fn mut` is expected. The reverse is not allowed.

There is no `FnOnce` analog (see "No `FnOnce`" below).

### Type Positions

Closure types use a single bare form — `fn(T) -> U` or `fn mut(T) -> U` — in every position:

| Position                                                    | Form                                | Dispatch                                                                                                 |
| ----------------------------------------------------------- | ----------------------------------- | -------------------------------------------------------------------------------------------------------- |
| Function parameter                                          | `fn(...)` / `fn mut(...)`           | Monomorphized per call site                                                                              |
| Function return                                             | `fn(...)` / `fn mut(...)`           | Anonymous monomorphic when the concrete type unifies across branches; type-erased to canonical otherwise |
| Storage (struct field, local annotation, container element) | `fn(...)` / `fn mut(...)`           | Type-erased (canonical struct)                                                                           |
| Generic bound                                               | `<F: fn(...)>` / `<F: fn mut(...)>` | Generic parameter, monomorphized                                                                         |

The compiler chooses static (specialised) vs dynamic (canonical) dispatch via escape analysis. From the user's perspective, both forms behave identically — only the runtime representation differs. There is no user-visible `impl` / `dyn` distinction. The LSP surfaces the compiler's dispatch decision as an inline hint or hover annotation when the user cares about it.

### Generic Bound Syntax

`fn(...)` and `fn mut(...)` may appear as trait bounds. This is the way to name a closure type and reuse it across multiple positions in a generic function:

```wado
fn apply<F: fn(i32) -> i32>(f: F, x: i32) -> i32 {
    return f(x);
}

fn run<F: fn mut(i32)>(mut f: F) {
    f(1);
    f(2);
}

fn dup<F: fn(i32) -> i32>(f: F) -> [F, F] {
    return [f, f];
}
```

The underlying trait names are not user-visible. The `fn` keyword itself serves as the bound name. User-defined types cannot implement these traits — only closure literals produce callables. (May be revisited later; not in scope for the MVP.)

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
fn run(mut f: fn mut(i32)) { f(1); f(2); }
//      ^^^ required
```

Calling a `fn mut` closure through a non-`mut` binding is a compile error. This mirrors Rust's `FnMut` rule and corresponds to the `&mut self` calling convention.

The check applies to the _root_ of the callee place, not just the immediate identifier. `(h.f)()`, `(a.b.c.f)()`, `arr[i]()`, and `(arr[i].f)()` all require the underlying binding (`h`, `a`, `arr`) to be `mut`. A binding whose type is `&mut T` is treated as a mutable place, so `(self.f)()` inside a `&mut self` method is accepted. A temporary root — a call result such as `(make_counter().f)()` — has no binding and is always accepted.

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

Closures follow Wado value semantics: deep-copied on assignment, parameter passing, and return. Under auto-by-reference capture every env field is a reference (`&T` or `&mut T`), so copying a closure copies reference values — which alias per Wado's reference semantics. All copies of a closure observe the same captured bindings.

```wado
let mut count = 0;
let mut c1 = || count += 1;  // env: { &mut count }
let mut c2 = c1;              // env copied; both still hold &mut count
c1();
c2();
assert(count == 2);
```

### No `Clone` Trait

Wado has no `Clone` trait or `.clone()` method. Constructions like `[f, f]` auto-copy `f`:

```wado
fn dup<F: fn(i32) -> i32>(f: F) -> [F, F] {
    return [f, f];
}
```

### No `FnOnce`

Rust's `FnOnce` exists because consuming a closure can move captured values out, leaving the closure unusable. Under Wado's design:

- Calls never consume the closure.
- All captures are references (`&T` / `&mut T`); copying a closure copies those references, which continue to alias the same outer bindings.
- No captured state is "moved out" — every call reads or writes through references that remain valid for the lifetime of the closure.

No state of affairs requires single-use closures, so `FnOnce` is unmotivated and omitted.

## Effect System Integration

### Effect Annotations on Closure Types

Closure types carry effects with the same `with` syntax as functions:

```wado
let f: fn(i32) -> i32 with Stdout = |x| { println(`{x}`); x };
```

### Effect Generics

Functions that accept effectful closures use the `<effect E>` parameter form (per [Effect System Design WEP](./wep-2026-01-27-effect-system-design.md)):

```wado
fn map<T, B, effect E>(
    arr: Array<T>,
    f: fn mut(T) -> B with E,
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

Iterator methods use `fn mut(...) with E` for closure parameters and `Iterator<Item = ...> with E` for adapter returns:

```wado
trait Iterator {
    type Item;

    fn map<B, effect E>(self, f: fn mut(Self::Item) -> B with E)
        -> Iterator<Item = B> with E;

    fn filter<effect E>(self, p: fn mut(&Self::Item) -> bool with E)
        -> Iterator<Item = Self::Item> with E;

    fn fold<B, effect E>(self, init: B, f: fn mut(B, Self::Item) -> B with E)
        -> B with E;

    fn for_each<effect E>(self, f: fn mut(Self::Item) with E) with E;

    fn find<effect E>(self, p: fn mut(&Self::Item) -> bool with E)
        -> Option<Self::Item> with E;

    fn any<effect E>(self, p: fn mut(Self::Item) -> bool with E)
        -> bool with E;

    fn all<effect E>(self, p: fn mut(Self::Item) -> bool with E)
        -> bool with E;

    fn inspect<effect E>(self, f: fn mut(&Self::Item) with E)
        -> Iterator<Item = Self::Item> with E;
}
```

Choosing `fn mut` mirrors Rust's `FnMut`: by sub-typing it accepts both pure and mutating closures, maximizing caller flexibility.

Adapter struct types (the internal `Map<I, F>`, `Filter<I, F>`, etc.) are not user-namable; iterator chains see only the trait surface `Iterator<...>` in their return type.

## Implementation Overview

Each closure literal generates an anonymous functor struct paired with a function reference. The compiler picks one of two paths per use site:

- Specialised path: when the closure type is known concretely at the use site (parameter position, return position with unifiable branches, generic-bound parameter), the function is monomorphized for the specific functor. Calls are direct and inlinable.
- Canonical path: when the closure must be type-erased (struct field, mixed-branch return, container element, etc.), the value is wrapped in a canonical signature-keyed struct `(env, funcref)`. Calls dispatch indirectly through a funcref.

Escape analysis drives the choice. From the user's perspective both paths satisfy the same `fn(T) -> U` type. Captures resolve to references stored in the functor's environment; when a closure escapes its declaring scope, captured bindings are heap-promoted automatically.

`fn` vs `fn mut` lives entirely in the type system; both compile to identical Wasm calling conventions. The full Wasm GC layout, the internal trait machinery, and the migration plan from the current implementation are documented in [Closure Implementation Internals](./wep-2026-01-25-closure-implementation-internals.md).

## Component Model Boundary

Closures cannot cross the Component Model boundary (CM has no closure type). Compile error when attempting to export or import a function with a closure-typed parameter or return type, or with a closure-typed component carried through container types in the signature.

Future: resource adapter — wrap closures as CM resources with a `call` method backed by a runtime handle table.

## Comparison with Rust

| Aspect                     | Rust                            | Wado                                                    |
| -------------------------- | ------------------------------- | ------------------------------------------------------- |
| Closure literal            | `\|x\| ...`                     | `\|x\| ...`                                             |
| `move` keyword             | yes                             | no (use intermediate local)                             |
| Auto-capture               | `&` / `&mut` / move             | `&` / `&mut` only                                       |
| Read/write split           | `Fn` / `FnMut` traits           | `fn` / `fn mut` keywords                                |
| Consume-only variant       | `FnOnce`                        | none                                                    |
| Bound syntax               | `F: Fn(T) -> U`                 | `F: fn(T) -> U`                                         |
| Type in non-bound position | `impl Fn` / `dyn Fn` / `fn` ptr | `fn(T) -> U` (compiler picks dispatch; LSP surfaces it) |
| `mut` binding for mutating | required                        | required                                                |
| User-implementable trait   | unstable (`fn_traits`)          | not allowed                                             |
| `Clone`                    | required trait                  | none (auto-copy)                                        |
| Function pointer           | separate type                   | collapsed (closures with empty env)                     |
| Lifetimes                  | tied to captured borrows        | none (GC)                                               |

## Consequences

### Positive

1. Familiar to Rust users with minimal mental gap, while less verbose than Rust thanks to compiler-managed dispatch.
2. Optimizer has mutation/purity information at type-erased call sites.
3. Sub-typing `fn <: fn mut` — pure closures fit everywhere a mutating one does.
4. Effect transparency via `<effect E>` keeps iterator API readable.
5. Fewer concepts than Rust (no `Clone`, no `move`, no `FnOnce`, no separate `fn` pointer type, no user-facing `impl` / `dyn` distinction).
6. Single bare form `fn(T) -> U` in every type position — minimal syntactic surface.

### Negative

1. Dispatch choice (specialised vs canonical) is invisible in the source text.
   - Mitigation: LSP surfaces the compiler's decision as inline hint / hover so users who care can see it.
2. Cannot force type erasure for codesize control without explicit `dyn`-like syntax.
   - Mitigation: deemed unnecessary for the MVP; can be added later if a real use case emerges.
3. Adapter struct types are not user-namable.
   - Mitigation: `Iterator<Item = ...>` returns avoid the need to name them.
4. `mut` binding rule is "ceremonial" without a borrow checker.
   - Mitigation: kept for conceptual consistency with `&mut self` calling convention.
5. Closures cannot cross Component Model boundary (MVP).
   - Mitigation: documented; resource adapter as future work.

## Future Work

- User-defined callable types (some syntax for implementing the closure trait on user structs): low priority.
- Resource adapter for closures at the Component Model boundary.
- `with ..` effect-polymorphism shorthand if `<effect E>` verbosity proves painful.
- Inlining heuristics for closure parameters with small bodies.
- Explicit `dyn`-like syntax for forced type erasure (codesize control), should a real use case emerge.

## References

- [Closure Implementation Internals](./wep-2026-01-25-closure-implementation-internals.md) — Wasm GC layout, internal trait machinery, migration plan.
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Iterator Traits Design](./wep-2026-01-24-iterator-traits.md)
- [Rust Closure Implementation](https://doc.rust-lang.org/book/ch13-01-closures.html)
