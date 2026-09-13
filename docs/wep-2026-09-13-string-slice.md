# WEP: String Views — `StrSlice` and `AsStrSlice`

## Context

Rust lets one signature accept `String`, `&String`, and `&str` through
`AsRef<str>`. Wado answers two thirds of that already. Value semantics plus
value-copy elision ([WEP: Ownership Analysis](./wep-2026-05-21-resource-ownership.md)) mean an owned `String` and a
`&String` both reach a `&String` parameter without copying, and static trait
dispatch monomorphizes a generic bound away at `-O2`.

The missing third is the view: a subrange of a string that is neither a copy nor
a reference to the whole. Without one, an API that works on part of a string
passes the part as arguments. The standard library did exactly that.
`FromStr::from_str_range(s, start, end)` took the range beside the string, and
every caller repeated the triple.

`ByteSlice` already views a `String`'s bytes with no copy, but it views bytes,
not text: nothing holds its ends on character boundaries. A newtype cannot hold
one either. `as` converts in both directions for free, so
`type ByteSlice = Slice<u8>` admits exactly what `Slice<u8>` admits
([WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md)).

Zero overhead here means zero after `-O2`, not zero in the source. A view is a
three-field aggregate built and consumed inside one function; the optimizer is
what has to make it vanish.

## Decision

A string view is a library type, not a compiler-known one. The mechanism must
be one a user could build for their own type, so nothing in the compiler knows
the name `StrSlice`.

A newtype is not the shape for it, since it cannot hold the UTF-8 boundary
invariant. `docs/cheatsheet.md` and `docs/spec.md` say so where they introduce
newtypes.

`core:prelude` gains `StrSlice`, a view over a string's bytes whose ends are
character boundaries, and `AsStrSlice`, the conversion that lets one signature
take a `String`, a reference to one, or a view of one. Where such a view still
costs something at `-O2`, the optimizer is fixed rather than worked around.

What follows:

- `AsStrSlice` is an ordinary trait whose method returns a view of the
  receiver, so it declares `stores[self]` — the same shape `AsByteSlice` has.
  Static dispatch monomorphizes it, so a `fn f<S: AsStrSlice>(s: &S)` carries no
  dispatch at `-O2`.
- The view must scalarize: a three-field struct built and read in one function
  leaves no `struct.new` behind. That is a property of the optimizer, so it is
  tested as one.

## Roadmap

- [x] Record in `docs/cheatsheet.md` and `docs/spec.md` that a newtype carries
      no invariant of its own. Done when both say where an invariant belongs
      instead.
- [x] Let the escape analysis see the reference a generic instance holds in a
      field, so a view of a `&String` parameter that stays local needs no
      `stores`. Done when a local view compiles and an escaping one is still
      rejected.
- [x] Make a view scalarize at `-O2`. Done when a byte-scanning loop over a
      view of a `&String` emits no `struct.new`.
- [x] Add `StrSlice` to `core:prelude`: a struct over a string's bytes plus a
      range, with private fields and constructors that reject an end off a
      character boundary. Done when a view can be built, compared, printed, and
      iterated by character.
- [x] Add `AsStrSlice` with impls for `String` and `StrSlice`. Done when one
      generic signature accepts an owned string, a reference to one, and a
      subrange view.
- [x] Migrate the standard library's `(text, start, end)` triples to `StrSlice`,
      replacing the triple signatures rather than keeping both. Done when
      `FromStr` names `from_str_slice` and no triple form remains.

## Known gaps

- A view passed to a function the inliner leaves alone is still materialized.
  Closing it needs argument promotion, which `docs/optimizer.md` already lists
  as not implemented: a callee taking an aggregate by value and only reading its
  fields would take the fields instead.
- The `StrSlice` API surface is open: which of `String`'s methods it carries,
  which prelude traits it implements, and whether `String`'s own methods start
  taking `impl AsStrSlice`.
- Whether `AsStrSlice` joins the prelude's auto-imported set, as `AsByteSlice`
  has not.
- `StrSlice` carries no string search or comparison beyond `Eq` / `Ord`:
  `contains`, `starts_with`, `split` and the trims stay on `String`, so working
  on part of a string still copies it out for those. Closing it means porting
  each one to a view and having `String`'s own delegate.
- A cast between two references whose referents share one representation head
  (`&ByteSlice` to `&Slice<u8>`) is not dropped, so it still hides the operand's
  shape from the rules that match on one. Only the unreferenced case is covered.
