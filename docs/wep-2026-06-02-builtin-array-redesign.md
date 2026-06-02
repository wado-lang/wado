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

There is no separate "internal intrinsic" name anymore: `Array<T>` is both the
user-facing type and the lowering target. The `array.new` / `array.get` /
`array.set` / `array.len` / `array.copy` / `array.fill` instructions are emitted
by `Array<T>`'s methods.

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
optimizer bug in problem (2).

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
2. Large rename churn: `Array` → `List` and `builtin::array` → `Array` touch
   stdlib, tests/fixtures, the VS Code grammar, generated docs, and the
   `#[compiler_item("array")]` wiring.
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

## Implementation TODOs

- [ ] Declare `Array<T>` in the prelude as a definition-less
      `#[compiler_item("array")] pub type Array<T>;`, binding the builtin to
      `core:prelude` and giving it a declaration site for impls and LSP.
- [ ] Generalize the parser: accept a named definition-less `type Name<...>;`
      declaration, not only the tuple form `type [..T];`.
- [ ] Reassign the `compiler_item` keys: `"array"` → raw GC array (above), new
      `"list"` → growable sequence. Add `CompilerItem::List` (enum variant, `ALL`,
      `attr_name`, `expected_kind`) in `compiler_item.rs`.
- [ ] Give `Array<T>` value semantics end to end (it already deep-copies when
      embedded; make it copy as a standalone value too).
- [ ] Port the `builtin::array_*()` free functions to `Array<T>` methods
      (`new`, `filled`, `len`, `get`, `set`, `fill`, `copy_from`); keep the
      `array.*` WIR lowering.
- [ ] Rename the growable sequence `Array<T>` → `List<T>` (type, stdlib, fixtures,
      grammar, docs).
- [ ] Introduce `Slice<T>` and re-base `ArrayIter` / `WindowsIter` / `ChunksIter`
      onto `&Array<T>` borrows with `stores[self]`.
- [ ] Audit stdlib call sites to take `&Array<T>` / `&List<T>` where appropriate.
- [ ] Verify optimizer copy-elision for non-mutating value arguments; add a pass
      if missing (gates the performance story).
- [ ] Fix the stale "stores not yet implemented" statements in `docs/spec.md`.
- [ ] Update `docs/cheatsheet.md`, `docs/spec.md`, and the generated
      `docs/stdlib-core-prelude.md`.
- [x] Add the WEP to the index in `docs/CLAUDE.md`.
- [ ] `mise run format`.
