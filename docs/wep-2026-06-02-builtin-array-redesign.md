# WEP: Redesign `builtin::array` into a First-Class `Array<T>`

## Status

Draft.

## Context

Wado has two array-like types today:

- `builtin::array<T>` — a raw Wasm GC array intrinsic. It is the storage layer
  behind `String` and the growable sequence, but it is exposed only through free
  functions (`builtin::array_new`, `builtin::array_get`, `builtin::array_set`,
  …). It is **internal** and not meant for users.
- `Array<T>` — the user-facing growable sequence (Rust's `Vec<T>`), implemented as
  `struct Array<T> { repr: builtin::array<T>, used: i32 }`.

This split has accumulated four problems:

1. The two are easy to confuse: a fixed Wasm GC array and a growable sequence both
   read as "array".
2. `builtin::array<T>` is the **only value-typed exception to value semantics**:
   it has reference semantics. Worse, its mutators are free functions that take
   the array **by value** yet mutate it in place (`array_set(arr, i, v)`), so the
   signature looks pure. This invisible mutation has already produced an
   optimizer bug, because the optimizer keyed off the signature rather than an
   explicit `&mut`.
3. The operations are free functions (`builtin::array_*()`), not methods, which
   is awkward to use.
4. There is no type definition for it, so it is not LSP-friendly.

The raw GC array is genuinely useful (fixed-length, no `used` bookkeeping, direct
`array.*` instructions) and we want to open it to users. But in its current shape
it is too special. This WEP makes it a normal, first-class value type.

### Naming

We rename so that the names match the Wasm reality and the C# / Java / Kotlin
convention (`Array` = fixed, `List` = growable). This also aligns with Rust
intuition, where _array_ means the fixed `[T; N]` and the growable type is
`Vec<T>`:

| Concept                   | Old name            | New name   |
| ------------------------- | ------------------- | ---------- |
| Raw fixed-length GC array | `builtin::array<T>` | `Array<T>` |
| Growable sequence         | `Array<T>`          | `List<T>`  |

The growable type is named `List<T>`, **not** `Vec` / `Vector`, on purpose. We
reserve the word _vector_ exclusively for its two domain meanings — the SIMD
`v128` vector types (see [WEP-2026-01-31](./wep-2026-01-31-simd-v128.md)) and
mathematical vectors — so a general-purpose growable container must not claim the
name. `List` carries no such overload and reads correctly across C# / Java /
Kotlin.

`Array<T>`'s length is a runtime value (`array.len`), not a compile-time
constant. It is therefore closer to a Java array / `Box<[T]>` than to Rust's
`[T; N]`; this is documented so users do not expect length-in-the-type.

## Decision

### 1. `Array<T>` is the raw GC array, as a value type

`Array<T>` becomes today's `builtin::array<T>` intrinsic, renamed and made
public, with two changes:

- It has **value semantics** like every other type. Deep copy on assignment,
  parameter passing, and return is synthesized exactly as it is today for any
  struct that embeds a raw GC array: `copy_value` already lowers an embedded GC
  array to an `array_clone` call. No new copy machinery is required.
- It carries an **`impl Array<T>` surface and trait impls** (see Method
  surface). These attach to its declaration site in the prelude (see
  Declaration).

`Array<T>` is the renamed type: the GC array intrinsic spelled `builtin::array<T>`
today becomes simply `Array<T>`, with a name, a declaration site, and an `impl`.
The `builtin::array_*()` operations stay as free-function intrinsics — they remain
the lowering layer that emits `array.new` / `array.get` / `array.set` /
`array.len` / `array.copy` / `array.fill` — but they are now typed in terms of
`Array<T>` and the Phase 1 borrows, e.g.

```wado
builtin::array_get<T>(arr: &Array<T>, index: i32) -> T
builtin::array_set<T>(arr: &mut Array<T>, index: i32, value: T)
```

`Array<T>`'s methods are thin Wado wrappers over these intrinsics (`get` calls
`builtin::array_get(self, index)`, and so on). The redesign renames the type and
adds a public interface; it does not re-implement the lowering, and it keeps the
`builtin::array_*` functions as the internal primitive layer — users call the
methods instead of the free functions.

The only type with reference semantics is now the reference itself (`&T` /
`&mut T`). The hidden "value type that is secretly a reference" is gone.

#### Declaration

`Array<T>` is a builtin type: its storage and instructions live in the compiler,
not in Wado source. It is bound to the compiler with a **definition-less `type`
declaration carrying `#[compiler_item("array")]`**, exactly like the tuple type
family (`#[compiler_item("tuple")] pub type [..T];`), which "establishes
`core:prelude` as the owner of all tuple types":

```wado
/// Fixed-length GC array — the builtin storage primitive.
#[compiler_item("array")]
pub type Array<T>;
```

This is what binds the builtin to its owning `module_source` (`core:prelude`),
gives it a real declaration site for LSP, and provides the named type that
`impl Array<T>` / trait-impl blocks attach to. No separate "allow impls on an
intrinsic" mechanism is needed: the type is declared, so impls attach the same
way they do for any prelude type.

Two pieces of plumbing follow from this:

- The parser currently accepts a definition-less `type` declaration only for the
  tuple form `type [..T];`; it must be generalized to accept a named
  definition-less `type Name<...>;` (resolved to the builtin by its
  `compiler_item`).
- The `compiler_item("array")` key is reassigned: it now denotes the raw GC
  array (this declaration) rather than the growable sequence. The growable sequence
  gets a new key, `compiler_item("list")` (see §2).

#### Method surface

Mutators take `&mut self`; pure operations take `&self`. This makes mutation
visible to both the type system and the optimizer — the root cause of the
optimizer bug in problem (2). Each body is a thin Wado wrapper over the matching
`builtin::array_*()` intrinsic (the trailing comment shows the `array.*`
instruction it ultimately lowers to); e.g. `get` calls
`builtin::array_get(self, index)` and `set` calls
`builtin::array_set(self, index, value)`.

```wado
impl<T> Array<T> {
    pub fn new(len: i32) -> Array<T>;          // array.new_default
    pub fn filled(len: i32, value: T) -> Array<T>;
    pub fn len(&self) -> i32;                   // array.len
    pub fn get(&self, index: i32) -> T;         // array.get (value copy)
    pub fn set(&mut self, index: i32, value: T);// array.set
    pub fn fill(&mut self, value: T);           // array.fill
    pub fn copy_from(&mut self, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32); // array.copy
    pub fn slice(&self, start: i32, end: i32) -> Slice<T> with stores[self];
    pub fn iter(&self) -> ArrayIter<T> with stores[self];
}

impl<T> IndexValue<i32> for Array<T> { type Output = T; /* array.get */ }
impl<T> IndexAssign<i32> for Array<T> { type Input = T; /* array.set */ }
```

Convenience operations (`map`, `filter`, `fold`, …) are reached through `iter()`,
exactly as for `List<T>`.

#### Sequence literals

Both `Array<T>` and `List<T>` are sequence-literal targets, and both go through
the **same uniform front-end path**: a `[…]` literal desugars to the
`SequenceLiteralBuilder` protocol (`new_literal` / `push_literal` / `build`),
exactly as today. The front-end and `lower` add **no** special casing for either
type — keeping the literal path uniform is the whole point. `Array<T>`
participates by being a `SequenceLiteralBuilder` target like any other type
(because a raw array has nowhere to hold a write cursor, its builder is a
separate builder via the `SequenceLiteral { type Builder }` path; the exact
builder shape is an implementation detail and does not leak into the front-end).

The fast `array.new_fixed` form is produced **only by the optimizer**, never by
the front-end. The optimizer keeps a single literal node,
[`NirExprKind::ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md), now wired to
the raw `Array<T>`: it materializes from a recognized
`array.new_default` + N× `array.set` window and lowers to
`array.new_fixed<T>(e0, …, eN)`. Like `Switch`, it is an optimizer-materialized
normalization that `lower` never emits.

No `ListLiteral` node is needed. `List<T>` is now a literal user-defined struct
(`{ repr: Array<T>, used }`) whose `SequenceLiteralBuilder` impl is ordinary Wado
code; once the builder methods are inlined, the `repr` construction _is_ a raw
`array.new_default` + `set` window, so the same `ArrayLiteral` pass collapses it
to `array.new_fixed`. The surrounding `List { repr: <ArrayLiteral>, used: N }` is
just a normal `StructNew` the optimizer and codegen already handle. The `List`
case thus falls out of the `Array` case for free — there is nothing
`List`-specific left to special-case.

This is a simplification over today, where the single `ArrayLiteral` carries the
growable struct's type and `wir_build` emits the `{ repr, used }` `StructNew`
itself. After the redesign `ArrayLiteral` carries the raw array type only, and
the struct wrap is left to the generic `StructNew` path.

### 2. `List<T>` is the growable sequence

`List<T>` is today's `Array<T>`, unchanged in behavior. It is an ordinary
prelude struct (it has a definition), so it keeps a `#[compiler_item]` — but
under the new key `"list"`, since `"array"` now names the raw GC array (§1):

```wado
#[compiler_item("list")]
pub struct List<T> {
    repr: Array<T>,   // value-typed; deep-copied with the List
    used: i32,
}
```

Internal mutators operate through `&mut self`, mutating `self.repr` in place
(in-place field mutation through `&mut self` does not copy). On `List` copy,
`copy_value` deep-copies `repr` via `array_clone`, identical to today.

All of `List<T>`'s API is the current `Array<T>` API from
[WEP-2026-03-29](./wep-2026-03-29-redesign-string-array-api.md) (push, pop,
insert, remove, sort, …), renamed only at the type level.

### 3. `String` is unchanged in structure

`String` keeps its own representation and logic:

```wado
pub struct String {
    repr: Array<u8>,  // was builtin::array<u8>
    used: i32,
}
```

Only the field type name changes (`builtin::array<u8>` → `Array<u8>`). The UTF-8
invariant and the entire String API are untouched by this WEP. String is not
rebased onto `List<u8>`.

### 4. Views borrow the whole array (`&Array<T>`)

Wasm GC has no interior references: there is no pointer to `array[i]`. Two
consequences fall out and are now first-class rules:

- **Element access is always a value copy.** `arr[i]` returns `T` by copy
  (`IndexValue`), never `&T`. The reference-returning `Index<I> -> &Self::Output`
  trait is _not_ implemented for `Array` / `List` / `Slice`. `iter_mut()` remains
  unavailable; in-place element mutation is done via indexed `set` / `[i] = v`.
- **A view borrows the whole backing array**, not a pointer+len. Every zero-copy
  view holds `&Array<T>` plus offsets:

```wado
pub struct Slice<T> {
    repr: &Array<T>,
    start: i32,
    end: i32,
}
```

`&Array<T>` where the referent is a GC reference type lowers transparently to the
same Wasm type (no boxing), so the borrow is zero-cost.

All view types are unified under this borrow model: `Slice<T>`, `ArrayIter<T>`,
`WindowsIter<T>`, `ChunksIter<T>` hold `&Array<T>` (or `&List<T>`'s `repr`), not a
by-value array. This matters because under value semantics, a view that held an
`Array<T>` by value would deep-copy the whole backing on construction.

#### Snapshot semantics

A view borrows the backing buffer at the moment it is created. If the source
`List<T>` later grows and reallocates its `repr`, the view keeps referring to the
**old** buffer. This is memory-safe (the GC keeps the old buffer alive) and gives
a simple rule: _a view is a stable snapshot of the buffer it was taken from._
This differs from Go slices and Rust borrows and must be documented in the spec.

### 5. `stores[self]` on view-returning methods

A method that returns a view stores `&self`'s reference into the returned struct,
so it must declare `with stores[self]` (the existing reference-escape mechanism,
which is implemented and enforced by `check_stores`). View-returning methods
(`slice`, `iter`, `windows`, `chunks`) therefore carry `with stores[self]`.

This is explicit for now. If it proves too noisy in practice, a later WEP may
infer `stores[self]` when a method's return type is a borrowing view type. We do
not add that inference here.

### 6. Documentation correctness fix

`docs/spec.md` currently states that `stores[...]` is "Not yet implemented"
(around the Reference Storage section and the type-system note). This is stale:
`stores` is implemented and enforced by `effect_check::check_stores` (runs after
synthesis, before optimization). This WEP corrects those statements as part of
the same change, since the new view API depends on `stores` being real.

## Consequences

### Positive

1. Value semantics is now uniform: the only reference-semantic thing is `&`. The
   hidden exception that caused the optimizer bug is removed.
2. Mutation is visible (`&mut self`), so the optimizer and the type system both
   see it. The class of "invisible mutator" bugs is closed.
3. `Array<T>` gains a real type definition, methods, and trait impls — usable and
   LSP-friendly.
4. Names match the runtime and common convention (`Array` fixed, `List`
   growable).
5. Zero-copy slices and iterators are preserved via `&Array<T>` borrows, now on a
   principled footing (`stores`) rather than a hidden intrinsic.

### Negative

1. Value `Array<T>` deep-copies (`array_clone`) on pass-by-value. Code that
   passes large arrays by value pays O(n). Mitigations:
   - Audit the stdlib to take `&Array<T>` / `&List<T>` where it does not mutate
     or move. This is the bulk of the "rewrite a part of the stdlib" work.
   - Verify the optimizer elides the copy for a value argument the callee does
     not mutate; add a copy-elision pass if it does not. This is a gating
     correctness/performance item, tracked below.
2. Large rename churn from `Array` → `List` (stdlib, tests/fixtures, the VS Code
   grammar, generated docs, and the `#[compiler_item("array")]` wiring). Exposing
   `builtin::array` as `Array` adds far less: a declaration plus a Wado `impl`,
   with the intrinsic layer untouched.
3. Snapshot view semantics is a new rule users must learn (a view does not track
   growth of its source).
4. Mutation-through-alias is observable: while a `Slice`/`ArrayIter` borrow is
   alive, the source can still be mutated through a separate path. There is no
   borrow checker to forbid it; it is memory-safe under GC but is a documented
   footgun. This is the same hazard as today, now localized to explicit `&`.

### Neutral

- Both `Array<T>` and `List<T>` map to CM `list<T>` at component boundaries.
  Internally `Array<T>` is GC `array T`; `List<T>` is a GC struct
  `{ array T, i32 }`.
- Element access semantics (value copy, no `iter_mut`) are unchanged from today;
  this WEP only makes the underlying reason (no GC interior references) explicit.
- Sequence-literal handling is unchanged in spirit and slightly simpler: the
  front-end still desugars every `[…]` to the `SequenceLiteralBuilder` protocol,
  and the optimizer still owns the single `ArrayLiteral` node — now wired to the
  raw `Array<T>` (→ `array.new_fixed`). `List<T>` literals need no dedicated node;
  they reduce to `StructNew { repr: <ArrayLiteral>, used }` because `List` is an
  ordinary struct.

## Implementation roadmap

The work is phased. Phase 0 (the rename) is **exclusive and atomic**: it must be
carried to completion before any other phase starts. The hazard it avoids is a
window in which the name `Array` denotes two types at once — anyone reading
`Array` mid-migration would have to ask "old or new?". Doing the rename
all-at-once removes that window: after Phase 0 the name `Array` (and the
`compiler_item` key `"array"`) are fully vacated, so the raw array can claim them
without collision.

Phase 1 is then the minimal, independent fix that makes `builtin::array` mutation
visible to the optimizer; it ships on its own and unblocks other tracks that the
hidden mutation was holding up. Phases 2+ are the longer first-classing tail,
which interferes little with the rest of the compiler and can proceed steadily.

### Phase 0 — Rename current `Array` → `List` (exclusive, atomic)

This phase introduces **no new behavior**; the build and all tests are green at
its end. It is purely the largest, most mechanical step. It is tractable because
`List` is currently an unused name (the only occurrence in the tree is the
English word "List" in a doc comment), so `Array` → `List` is unambiguous and
total. The generated `tests/generated/fixtures/*.wir.wado` snapshots regenerate
from the harness; the hand-edited surface is `lib/`, the hand-written fixtures,
`src/`, `docs/`, and the VS Code grammar.

- [ ] Rename the type `Array<T>` → `List<T>` everywhere: prelude
      (`array.wado` → `list.wado`), stdlib, hand-written fixtures, VS Code
      grammar, docs.
- [ ] Rust side: `CompilerItem::Array` → `CompilerItem::List`; reassign the key
      `"array"` → `"list"` (`attr_name`, `from_attr_name`, `ALL`,
      `expected_kind`); update the struct attribute to `#[compiler_item("list")]`.
- [ ] Regenerate the `*.wir.wado` snapshots; confirm no stray growable-`Array`
      reference remains (`grep -w Array` is clean except deliberate raw-array WIR
      `array.*` ops, which Phase 1 owns).
- [ ] Green gate: `mise run test` + `mise run test-wado` across `-O` levels
      (`WADO_FULL_TEST=1`), then `mise run format`.

### Phase 1 — Make `builtin::array` operations take `&` / `&mut` (unblocking fix)

The smallest change that pays off immediately, and independent of the rest.
Today the `builtin::array_*` operations are free functions that take the array
**by value** yet mutate it in place, so the mutation is invisible to the
optimizer (the root cause in Context, problem 2). Change the first parameter to a
borrow — `&mut` for the mutators, `&` for the readers — and update their call
sites inside `List<T>` / `String`. Nothing else in the redesign needs to land
first.

This makes mutation visible to the optimizer, closing the latent
invisible-mutator bug class, and **unblocks dependent work** that the hazard was
holding up — for example, const-global globalization could not proceed while
array mutation was invisible. The full first-classing (Phase 2+) is decoupled and
follows steadily.

- [ ] Change the `builtin::array` mutators (`set`, `fill`, `copy`) to take `&mut`
      as the first argument; the readers (`get`, `len`) to take `&`.
- [ ] Update call sites in `List<T>` / `String` to pass `&` / `&mut`.
- [ ] Confirm the optimizer now sees the mutation; add a red/green e2e fixture
      for the invisible-mutation case if one is missing.

This phase is independent of Phase 0 (it touches `builtin::array_*`, which the
rename does not), so the two could be sequenced either way; it is placed right
after the rename because it is the minimal step that unblocks other tracks.

### Phase 2 — Expose the raw GC array as a public `Array<T>`

The GC array intrinsic is renamed `builtin::array<T>` → `Array<T>`; its
`builtin::array_*()` operations stay as free-function intrinsics, now typed
against `&Array<T>` / `&mut Array<T>`. This phase adds the `Array<T>` type and a
Wado `impl` of wrapper methods over those intrinsics — interface tidying, not a
re-implementation of the lowering.

- [ ] Declare `Array<T>` in the prelude as a definition-less
      `#[compiler_item("array")] pub type Array<T>;`, binding the builtin to
      `core:prelude` and giving it a declaration site for impls and LSP. This is
      the type-level rename `builtin::array<T>` → `Array<T>`.
- [ ] Re-type the `builtin::array_*()` intrinsic signatures to spell `Array<T>`
      (e.g. `array_get<T>(arr: &Array<T>, index: i32) -> T`).
- [ ] Generalize the parser: accept a named definition-less `type Name<...>;`
      declaration, not only the tuple form `type [..T];`.
- [ ] Give `Array<T>` value semantics end to end (it already deep-copies when
      embedded; make it copy as a standalone value too).
- [ ] Add `impl<T> Array<T>` in Wado whose methods (`new`, `filled`, `len`,
      `get`, `set`, `fill`, `copy_from`) are thin wrappers calling the
      `builtin::array_*()` intrinsics — the intrinsics stay as the lowering layer.
      The Phase 1 `&` / `&mut` first parameters line up with `&self` /
      `&mut self`. Point `List<T>`'s `repr` at `Array<T>`.

### Phase 3 — Borrowing views

- [ ] Introduce `Slice<T>` and re-base `ArrayIter` / `WindowsIter` / `ChunksIter`
      onto `&Array<T>` borrows with `stores[self]`.

### Phase 4 — Sequence literals

- [ ] Make `Array<T>` a `SequenceLiteralBuilder` target (separate builder) so
      `[…]: Array<T>` uses the same uniform front-end path as `List<T>`; no
      special-casing in the front-end or `lower`.
- [ ] Re-wire the optimizer's `ArrayLiteral` node to the raw `Array<T>`
      (materialize from `array.new_default` + `set`, lower to `array.new_fixed`);
      drop the implicit `{ repr, used }` wrap from `wir_build` and let `List<T>`
      literals reduce to a generic `StructNew` over an `ArrayLiteral`. No
      `ListLiteral` node.

### Phase 5 — Performance and documentation

- [ ] Audit stdlib call sites to take `&Array<T>` / `&List<T>` where appropriate.
- [ ] Verify optimizer copy-elision for non-mutating value arguments; add a pass
      if missing (gates the performance story).
- [ ] Fix the stale "stores not yet implemented" statements in `docs/spec.md`.
- [ ] Update `docs/cheatsheet.md`, `docs/spec.md`, and the generated
      `docs/stdlib-core-prelude.md`.
- [x] Add the WEP to the index in `docs/CLAUDE.md`.
