# WEP: Redesign `builtin::array` into a First-Class `Array<T>`

## Status

Draft.

## Context

Wado has two array-like types today:

- `builtin::array<T>` — a raw Wasm GC array intrinsic, the storage layer behind
  `String` and the growable sequence. It is **internal**, exposed only through
  free functions (`builtin::array_get`, `builtin::array_set`, …).
- `Array<T>` — the user-facing growable sequence (Rust's `Vec<T>`), implemented as
  `struct Array<T> { repr: builtin::array<T>, used: i32 }`.

This split has four problems:

1. Both read as "array": a fixed GC array and a growable sequence are easy to
   confuse.
2. `builtin::array<T>` is the **only value-typed exception to value semantics** —
   it has reference semantics. Worse, its mutators are free functions that take
   the array **by value** yet mutate it in place (`array_set(arr, i, v)`), so the
   signature looks pure. This invisible mutation has already produced an optimizer
   bug, because the optimizer keyed off the signature rather than an explicit
   `&mut`.
3. The operations are free functions, not methods — awkward to use.
4. There is no type definition, so it is not LSP-friendly.

The raw GC array is genuinely useful (fixed-length, no `used` bookkeeping, direct
`array.*` instructions) and we want to open it to users. This WEP makes it a
normal, first-class value type.

### Naming

We rename to match the Wasm reality and the C# / Java / Kotlin convention
(`Array` = fixed, `List` = growable), which also matches Rust intuition (the fixed
`[T; N]` is an "array"; the growable type is `Vec<T>`):

| Concept                   | Old name            | New name   |
| ------------------------- | ------------------- | ---------- |
| Raw fixed-length GC array | `builtin::array<T>` | `Array<T>` |
| Growable sequence         | `Array<T>`          | `List<T>`  |

The growable type is `List<T>`, **not** `Vec` / `Vector`: we reserve _vector_ for
its domain meanings — the SIMD `v128` types (see
[WEP-2026-01-31](./wep-2026-01-31-simd-v128.md)) and mathematical vectors — so a
general-purpose container must not claim the name.

`Array<T>`'s length is a runtime value (`array.len`), not a compile-time constant;
it is closer to a Java array / `Box<[T]>` than to `[T; N]`. This is documented so
users do not expect length-in-the-type.

## Decision

### 1. `Array<T>` is the raw GC array, as a value type

`Array<T>` is the GC array intrinsic spelled `builtin::array<T>` today, renamed and
made public. It is a normal value type — deep-copied on assignment, argument
passing, and return like everything else (`copy_value` already lowers an embedded
GC array to an `array_clone` call, so no new copy machinery is needed) — and it
gains a declaration site, an `impl`, and trait impls.

The `builtin::array_*()` operations stay as free-function intrinsics — they remain
the lowering layer emitting `array.new` / `get` / `set` / `len` / `copy` / `fill` —
but are now typed against `Array<T>` and the Phase 1 references:

```wado
builtin::array_get<T>(arr: &Array<T>, index: i32) -> T
builtin::array_set<T>(arr: &mut Array<T>, index: i32, value: T)
```

`Array<T>`'s methods are thin Wado wrappers over them. The redesign renames the
type and adds a public interface; it does not re-implement the lowering. After it,
the only thing with reference semantics is `&` itself — the hidden "value type that
is secretly a reference" is gone.

#### Declaration

`Array<T>` is a builtin type — its storage and instructions live in the compiler —
so it is declared definition-less, like the tuple family, carrying its
`compiler_item`:

```wado
/// Fixed-length GC array — the builtin storage primitive.
#[compiler_item("array")]
pub type Array<T>;
```

This binds the builtin to its owning `module_source` (`core:prelude`), gives it a
declaration site for LSP, and provides the type that `impl` / trait-impl blocks
attach to — no special "impls on an intrinsic" mechanism is needed. Two
consequences: the parser must accept a named definition-less `type Name<...>;`
(today only the tuple form `type [..T];` is allowed), and the
`compiler_item("array")` key now denotes the raw array, so the growable type takes
a new key `"list"` (§2).

#### Method surface

Mutators take `&mut self`, pure operations take `&self` — making mutation visible
to both the type system and the optimizer (the root cause of problem 2). Wado has
no ownership or borrow checking; `&` / `&mut` are plain references with Java-like
semantics, and the only purpose of the `mut` distinction is to surface mutation.
Each body is a one-line wrapper over the matching `builtin::array_*()` intrinsic.

```wado
impl<T> Array<T> {
    pub fn new(len: i32) -> Array<T>;            // array.new_default
    pub fn filled(len: i32, value: T) -> Array<T>;
    pub fn len(&self) -> i32;                     // array.len
    pub fn get(&self, index: i32) -> T;           // array.get (value copy)
    pub fn set(&mut self, index: i32, value: T);  // array.set
    pub fn fill(&mut self, value: T);             // array.fill
    pub fn copy_from(&mut self, dst: i32, src: &Array<T>, src_off: i32, len: i32); // array.copy
    pub fn slice(&self, start: i32, end: i32) -> Slice<T> with stores[self];
    pub fn iter(&self) -> ArrayIter<T> with stores[self];
}

impl<T> IndexValue<i32> for Array<T> { type Output = T; /* array.get */ }
impl<T> IndexAssign<i32> for Array<T> { type Input = T; /* array.set */ }
```

Convenience operations (`map`, `filter`, …) go through `iter()`, as for `List<T>`.

#### Sequence literals

Both `Array<T>` and `List<T>` are `[…]` targets through the same uniform front-end
path: a literal desugars to the `SequenceLiteralBuilder` protocol
(`new_literal` / `push_literal` / `build`), with no special casing in the front-end
or `lower`.

The fast `array.new_fixed` form is produced only by the optimizer. The single
[`NirExprKind::ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md) node, now wired
to the raw `Array<T>`, materializes from a recognized `array.new_default` + `set`
window and lowers to `array.new_fixed`; like `Switch`, `lower` never emits it. No
`ListLiteral` node is needed: a `List<T>` literal inlines to that same raw-array
window wrapped in an ordinary `StructNew { repr, used }`, so the `List` case falls
out of the `Array` case.

### 2. `List<T>` is the growable sequence

`List<T>` is today's `Array<T>`, unchanged in behavior — an ordinary prelude struct,
now under the `compiler_item` key `"list"`:

```wado
#[compiler_item("list")]
pub struct List<T> {
    repr: Array<T>,   // value-typed; deep-copied with the List
    used: i32,
}
```

Its mutators operate through `&mut self`, mutating `self.repr` in place; on copy,
`copy_value` deep-copies `repr` via `array_clone`, identical to today. The full API
(push, pop, insert, remove, sort, …) is the current `Array<T>` API from
[WEP-2026-03-29](./wep-2026-03-29-redesign-string-array-api.md), renamed only at the
type level.

### 3. `String` is unchanged in structure

`String` keeps its own representation and logic; only the field type name changes
(`builtin::array<u8>` → `Array<u8>`). The UTF-8 invariant and the entire String API
are untouched, and String is not rebased onto `List<u8>`.

```wado
pub struct String {
    repr: Array<u8>,
    used: i32,
}
```

### 4. Views reference the whole array (`&Array<T>`)

Wasm GC has no interior references — there is no pointer to `array[i]` — so two
rules become first-class:

- **Element access is always a value copy.** `arr[i]` returns `T` by copy
  (`IndexValue`), never `&T`. The reference-returning `Index<I> -> &Self::Output` is
  _not_ implemented for `Array` / `List` / `Slice`, and `iter_mut()` stays
  unavailable; in-place element mutation goes through `set` / `[i] = v`.
- **A view references the whole backing array**, not a pointer+len. Every zero-copy
  view holds `&Array<T>` plus offsets — `Slice<T>`, `ArrayIter<T>`,
  `WindowsIter<T>`, `ChunksIter<T>` alike. (Held by value, under value semantics a
  view would deep-copy the whole backing on construction.)

```wado
pub struct Slice<T> {
    repr: &Array<T>,
    start: i32,
    end: i32,
}
```

`&Array<T>` lowers transparently to the same Wasm type (the referent is already a
GC reference), so the reference is zero-cost.

A view captures the buffer as it is at creation. If the source `List<T>` later grows
and reallocates its `repr`, the view keeps referring to the **old** buffer — safe
under GC, and a simple rule: _a view is a stable snapshot of the buffer it was taken
from_ (it never observes later growth of its source). This must be documented.

### 5. `stores[self]` on view-returning methods

A method that returns a view stores `&self` into the returned struct, so it declares
`with stores[self]` — the existing reference-escape mechanism, implemented and
enforced by `check_stores`. This is explicit for now; a later WEP may infer it for
methods that return a view type.

### 6. Documentation correctness fix

`docs/spec.md` currently states that `stores[...]` is "Not yet implemented". This is
stale — `stores` is implemented and enforced by `effect_check::check_stores`. Since
the view API depends on it being real, this WEP corrects those statements.

## Consequences

### Positive

1. Value semantics is uniform — the only reference-semantic thing is `&`. The
   hidden exception behind the optimizer bug is removed, and mutation is visible
   (`&mut self`), closing the "invisible mutator" bug class.
2. `Array<T>` gains a type definition, methods, and trait impls — usable and
   LSP-friendly — with names matching the runtime and common convention.
3. Zero-copy slices and iterators are preserved via `&Array<T>` references, now on a
   principled footing (`stores`) rather than a hidden intrinsic.

### Negative

1. A value `Array<T>` deep-copies (`array_clone`) on pass-by-value, so passing large
   arrays by value costs O(n). Mitigated by auditing the stdlib to take `&Array<T>` /
   `&List<T>`, and by verifying/strengthening optimizer copy-elision for value
   arguments the callee does not mutate.
2. Large rename churn from `Array` → `List` (stdlib, fixtures, grammar, docs,
   `compiler_item` wiring). Exposing `builtin::array` as `Array` adds far less — a
   declaration plus a Wado `impl`, with the intrinsic layer untouched.
3. Two documented footguns that nothing statically forbids (both memory-safe under
   GC): a view does not track growth of its source (snapshot semantics), and the
   source can be mutated through a separate path while a view is alive.

### Neutral

- Both map to CM `list<T>` at component boundaries. Internally `Array<T>` is GC
  `array T`; `List<T>` is a GC struct `{ array T, i32 }`.
- Element access (value copy, no `iter_mut`) is unchanged from today; this WEP only
  makes the underlying reason (no GC interior references) explicit.

## Implementation roadmap

Phase 0 (the rename) is **exclusive and atomic**: it must complete before anything
else. It avoids a window in which `Array` denotes two types at once — "old or new?".
After it, the name `Array` and the `compiler_item` key `"array"` are vacated, so the
raw array can claim them. Phase 1 is then a small, independent fix that unblocks
other tracks; Phases 2+ are the first-classing tail, which interferes little with the
rest of the compiler and can proceed steadily.

The checklists below are coarse intent, not a fixed script — the actual steps will
adapt as the work proceeds.

### Phase 0 — Rename current `Array` → `List` (exclusive, atomic)

No new behavior; green at the end. The largest, most mechanical step, but tractable
because `List` is currently an unused name, so `Array` → `List` is unambiguous and
total. Generated `tests/generated/fixtures/*.wir.wado` snapshots regenerate from the
harness; the hand-edited surface is `lib/`, hand-written fixtures, `src/`, `docs/`,
and the VS Code grammar.

- [x] Rename the growable `Array<T>` → `List<T>` everywhere — type, `compiler_item`
      key `"array"` → `"list"`, `CompilerItem::Array` → `List`, stdlib, fixtures,
      grammar, docs — regenerate the WIR snapshots, and gate on green across `-O`
      levels. Done: `mise run test` and `mise run test-wado` both green; the raw
      `builtin::array` intrinsic and its WIR ops are untouched, awaiting Phase 1.

### Phase 1 — Reference-typed `builtin::array` operations (unblocking fix)

The smallest, independent change with immediate payoff: give the `builtin::array_*`
operations `&` / `&mut` first parameters so their mutation becomes visible to the
optimizer. This closes the invisible-mutator bug class and **unblocks dependent
work** the hazard was holding up (e.g. const-global globalization).

- [ ] Give the `builtin::array_*` mutators `&mut` / readers `&` first parameters,
      fix their call sites in `List` / `String`, and confirm the optimizer now sees
      the mutation (add a red/green fixture if missing).

### Phase 2 — Expose the raw GC array as a public `Array<T>`

Rename the type `builtin::array<T>` → `Array<T>`; the `builtin::array_*()`
operations stay as intrinsics, re-typed against `&Array<T>` / `&mut Array<T>`. Adds
the type and a Wado wrapper `impl` — interface tidying, not a re-implementation of
the lowering.

- [ ] Declare the definition-less `Array<T>` (`compiler_item("array")`), generalize
      the parser for named definition-less types, re-type the `builtin::array_*`
      intrinsics, give `Array<T>` standalone value semantics, add the wrapper `impl`,
      and point `List`'s `repr` at it.

### Phase 3 — Reference views

- [ ] Introduce `Slice<T>` and re-base `ArrayIter` / `WindowsIter` / `ChunksIter`
      onto `&Array<T>` references with `stores[self]`.

### Phase 4 — Sequence literals

- [ ] Make `Array<T>` a `SequenceLiteralBuilder` target and re-wire the optimizer's
      `ArrayLiteral` node to the raw array (no `ListLiteral`; `List` literals reduce
      to a generic `StructNew`).

### Phase 5 — Performance and documentation

- [ ] Audit the stdlib for `&` parameters, verify/strengthen copy-elision, and fix
      the docs (`spec.md` `stores`, cheatsheet, generated prelude).
