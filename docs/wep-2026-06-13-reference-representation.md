# WEP: Reference Representation and Mutation Write-Back

## Context

Wado has no raw pointers and no borrow checker. A reference (`&T` / `&mut T`) is
always a GC-managed handle (see
[Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)).
How that handle is _represented_ on Wasm GC has never been written down, even
though the choice differs by referent type and is what makes mutation through a
`&mut` observable at the original place.

This WEP specifies the _intended_ representation and `&mut` semantics, and
catalogs where the current implementation diverges. The divergences are bugs to
fix so the implementation conforms — not behavior to preserve. The motivating
symptom is a silent miscompile (issue #1333): a write through `&mut xs[i]` or
`&mut s.f` of certain element/field types is discarded with no diagnostic.

## Design (normative)

### The dividing line: in-place mutation vs replace-on-assign

A `&mut T` must let a write be observed at the original place. The representation
follows from _how_ the value is mutated through the reference:

- In-place interior mutation — a struct field write (`r.f = v`) or an array
  element write (`r[i] = v`). `T` is a GC object with addressable interior; the
  reference is `T`'s own shared GC handle, and the write lands on the one shared
  object. No extra cell is needed.
- Replace-on-assign — `*r = v` swaps the whole value, because `T` has no
  addressable interior to mutate through the reference. The reference must point
  at a stable heap cell that _owns_ the current value: `Box<T>`, a
  compiler-internal one-field struct `{ value: T }`. `*r` reads the cell, `*r = v`
  writes it.

The axis is mutation mode, not "scalar vs heap". A `variant` is a heap GC struct
yet is replace-on-assign (see below). Mutability (`&` vs `&mut`) is a type-level
distinction, not a representational one: a reference to a replace-on-assign type
is `Box<T>` whether `&` or `&mut`.

Since both collapse onto one wrapper, the resolved type no longer says which is
which. The rewrite records the shared-`&T` origin separately, and anything asking
whether a boxed handle can be written through MUST consult that record, not the
type. A `Box<T>` parameter is the caller's storage, not a copy of it: reading it
as a by-value struct snapshots an aliased reference and loses the write.

### Classification of types

| Category                        | Types                                                                             | `&T` / `&mut T`                                        | Mutation through the reference             |
| ------------------------------- | --------------------------------------------------------------------------------- | ------------------------------------------------------ | ------------------------------------------ |
| In-place (reference types)      | `struct`; `List<T>` (raw GC `Array<T>`); `String`; `i128` / `u128`                | shared GC handle                                       | field / element write on the shared object |
| Replace-on-assign (value/boxed) | `primitive` (except `i128` / `u128`); `enum`; `flags`; `variant`; `fn` / `fn mut` | `Box<T>`                                               | `*r = v` replaces the box's content        |
| Handle                          | `resource` (opaque `i32`; conceptually `i32` / `externref`)                       | see [divergence D6](#known-implementation-divergences) | replace-on-assign conceptually             |

Notes on the non-obvious entries:

- `struct`, `List<T>`, and `String` are heap GC objects whose interior is mutated
  in place, so the reference is the shared handle. (`String` content is immutable
  today, so no `&mut` write applies — but a reference to it is still the shared
  handle, never a box.)
- `variant` is a GC struct, yet replace-on-assign: its case subtype hierarchy has
  no fixed mutable field to deref-assign into, so `*e = E::B(99)` replaces the
  whole variant value. Hence it is boxed.
- `fn` / `fn mut` are funcref-backed values; `*r = other_fn` replaces, so they are
  boxed. (Note: the issue #1333 enumeration omits `fn`; the implementation boxes
  it — see [D3](#known-implementation-divergences).)
- `i128` / `u128` are GC structs (a low/high `i64` pair), so they are treated as
  `struct` (shared handle), and are deliberately excluded from boxing despite
  their scalar value semantics.
- `resource` is an opaque handle (`i32` today). Assignment replaces the handle, so
  it is conceptually replace-on-assign, but the implementation does not box it —
  see [D6](#known-implementation-divergences).

### Connecting a `&mut` to a place: the box must _be_ the place's storage

For a replace-on-assign type, a write through `&mut` is observable at the original
place only when that place's storage _is_ the box cell. The compiler establishes
this for locals via the address-taken-locals boxing pass:

- `reify` marks `TirFunction::address_taken_locals` on `&x` / `&mut x` for a local
  `x`.
- `lower::plan::boxing` promotes that local's slot from `T` to `Box<T>`; reads of
  the local auto-unbox, and `&mut x` hands out the shared box.

```wado
let mut c = Color::Red;
let r = &mut c;   // c's slot promoted to Box<Color>; r shares that box
*r = Color::Blue; // writes the box
code(c);          // reads c (auto-unboxed) -> Blue
```

A non-local place — a `List` / array element `xs[i]` or a struct field `s.f` — has
no local to promote.

### The boxed-as-value set is a single predicate

The set of types boxed by reference (the replace-on-assign category) MUST be
defined by one predicate (e.g. `is_boxed_reference_target(T)`), and the boxing
pass, the forbid rule, and the carve-out below MUST all consume that predicate.
No component re-lists the member types by hand; that is how `fn` and `flags`
already drifted out of sync (see [D2](#known-implementation-divergences)–
[D4](#known-implementation-divergences)).

### `&mut` to a non-local replace-on-assign place

- Default: a `&mut <place>` is a compile error when the place is a non-local
  location (a `List` / index element, or a struct field) whose type is a
  replace-on-assign type per the predicate. The error names the workaround:
  assign the whole element / field (`xs[i] = …`, `s.f = …`). Covers both `Index`
  and `FieldAccess` operands and generalizes (replaces) the existing partial
  primitive-struct-field guard.
- `&` (immutable, read-only) to such a place is permitted: it reads a snapshot
  copy, and there is no write to lose.
- Carve-out: when the `&mut <place>` is a call argument to a parameter that does
  not escape (no `stores[param]`, enforced by `check_stores_semantic`), the
  reference provably cannot outlive the call, so it is desugared to a temp +
  write-back:

  ```text
    f(&mut xs[idx])
  ⇒ { let __mr_idx = idx;
      let mut __mr_t = xs[__mr_idx];
      f(&mut __mr_t);                  // mutates the temp's box
      xs[__mr_idx] = __mr_t;           // write-back to the place
    }
  ```

  The temp is a real local, so the address-taken boxing promotes it exactly as for
  `&mut <local>`. The forbid stays permanently for the escaping case (param in
  `stores`, or a `&mut` bound to a variable / returned), which has no sound
  write-back point.

In-place places — `&mut <local>` of any type, and `&mut` of a struct / `List` /
`String` reference mutated in place — are always allowed and unaffected.

## Decision

- [x] The representation (in-place shared handle vs `Box<T>`) is normative, keyed
      on the in-place-vs-replace dividing line, not scalar-vs-heap.
- [ ] Extract the boxed-as-value classification into one predicate shared by
      boxing / forbid / carve-out.
- [ ] Forbid `&mut` to a non-local replace-on-assign place (compile error),
      subsuming the partial primitive-struct-field guard. Ship `compile_error`
      fixtures: variant / primitive / enum / flags / `fn` list element, and
      variant / enum struct field.
- [ ] Carve out the `stores`-gated temp + write-back, one call path at a time:
  - [ ] `List` index element (`&mut xs[i]`) — validated by a throwaway prototype
        on the free-function / static-dispatch path; reuses the existing
        `index_assign` dispatch.
  - [ ] struct field (`&mut s.f`) — write-back is a plain field assign.
  - [ ] remaining call paths (method-call / indirect-call arguments).

Each carve-out narrows the forbid for the case it handles; the escaping case is
never carved out.

## Known implementation divergences

These are gaps between the design above and the current tree. Each is a bug to
fix to conform; none should be preserved.

- [ ] D1 — silent write-back drop. `&mut` to a non-local replace-on-assign place
      compiles but discards the write, for _every_ replace type. Verified by
      probe (HEAD):

  | place                             | result  |
  | --------------------------------- | ------- |
  | `&mut xs[i]` — primitive element  | dropped |
  | `&mut xs[i]` — enum element       | dropped |
  | `&mut xs[i]` — flags element      | dropped |
  | `&mut xs[i]` — variant element    | dropped |
  | `&mut fns[i]` — `fn` element      | dropped |
  | `&mut s.f` — enum / variant field | dropped |

  The same operations on a value-type _local_ all work. Resolved by the forbid +
  carve-out.

- [ ] D2 — no shared predicate. The boxed set is inlined in
      `lower/plan/boxing.rs::create_needed_box_types` as
      `is_prim || is_enum(non-variant-case) || is_variant || is_fn`. Extract it so
      forbid / carve-out cannot drift from boxing.
- [ ] D3 — `fn` coverage. The implementation boxes `fn` / `fn mut`, but issue
      #1333's type list omits them; any forbid written from that list would miss
      `&mut fns[i]`. The predicate (D2) is the source of truth.
- [ ] D4 — `flags`. There is no explicit `flags` arm in the box predicate; it
      works only because `flags` lowers to its `u32` primitive earlier. Make the
      predicate name `flags` explicitly.
- [ ] D5 — overloaded `ResolvedType::Enum`. A standalone `enum` and a variant's
      payload-less case subset are both `ResolvedType::Enum`, disambiguated only by
      `name ∉ variant_names`. This overloading is fragile and a latent bug source;
      a distinct representation for variant-case discriminants should be
      considered (possibly as separate work).
- [ ] D6 — `resource`. A resource handle is replace-on-assign but is not in the box
      predicate, so `&mut resource` has no stable cell. Decide and document: either
      box resource handles like other replace types, or explicitly reject `&mut`
      of a resource. (`&mut resource` is currently unverified / effectively
      unsupported.)
- [x] D7 — whole-value `*ref = v` write-back for `List<T>` and tuples. An in-place
      `&mut T` makes `*r = v` a field-wise write-back onto the shared handle,
      lowered by `try_expand_deref_aggregate_assign`. That expansion only
      recognised `ResolvedType::Struct` (`String`, and monomorphized generics like
      `TreeMap<K,V>`), so `List<T>` and tuples (`[A, B]`) — in-place
      `GenericInstance`s that are never monomorphized into their own struct — fell
      through and the assignment was silently dropped at every opt level
      (`*xs = []` was a no-op). Fixed by decomposing `List<T>` through its
      canonical `SeqField` `{repr, used}` layout and a tuple through its positional
      fields, both with concrete element types.
- [x] D8 — `*ref = v` did not deep-copy the RHS. The write-back decomposition in
      `try_expand_deref_aggregate_assign` moved the RHS's fields into the shared
      handle without a value copy, because deref-expansion `Let`s are synthesized
      after `value_copy::insert`'s walk and `wrap_value_copy_operand` found no
      registered helper for the referent type at that site. So `*r = v` aliased
      `v`'s interior — e.g. `*list_ref = other; other[0] = 9` also mutated the
      referent's element. Fixed by seeding a copy helper for the deref-target RHS
      type in the `analyze` walker and requesting the copy at the expansion site
      through the fold's `should_wrap_value_copy` predicate, so a live RHS is
      copied while a fresh / moved one (`*xs = []`/literal cases) stays free with
      no copy inserted at all.
      Note: a _separate_ pre-existing gap remains — a tuple literal does not copy
      its element variables (`a = [inner, 1]; inner[0] = 9` mutates `a.0`), which
      is tuple-literal construction, not deref-assign, and is out of scope here.

## Consequences

### Positive

- The representation is documented and normative, with one stated axis
  (in-place vs replace) instead of folklore.
- The silent miscompile becomes a compile error, then progressively a working
  write-back for the sound cases — across _all_ replace types, including `fn`,
  because forbid/carve-out derive from the shared predicate.
- The carve-out reuses the existing `stores` analysis and local-boxing machinery;
  no new runtime representation, allocation, or indirection.
- A single classification predicate removes the fn / flags drift class of bug.

### Negative

- A value-type `&mut` whose param escapes (`stores`), or one bound / returned
  outside a call, stays forbidden. Workaround: assign the whole element / field,
  or restructure functionally. This is the genuinely-unsound case under the
  current representation.
- `&mut` representation visibly depends on whether the referent is in-place or
  replace-on-assign; generic code over `&mut T` is monomorphized per `T` (already
  true today — this WEP only records it).

## References

- Issue #1333 (closed in favor of this WEP)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Closure Implementation](./wep-2026-01-16-closure-implementation.md)
- [Indexing Traits Design](./wep-2026-01-20-indexing-traits.md)
- [Variant Wasm GC Representation](./wep-2026-02-08-variant-representation.md)
- [128-bit Integer Types (i128/u128)](./wep-2026-01-24-i128-u128-types.md)
- [Redesign `builtin::array` into a First-Class `List<T>`](./wep-2026-06-02-builtin-array-redesign.md)
