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

A parameter that only reads its text names `AsStrSlice` and takes it by value,
across the standard library and the packages alike. A call site passes the
literal bare.

What follows:

- `AsStrSlice` is an ordinary trait whose method returns a view of the
  receiver, so it declares `stores[self]`, the same shape `AsByteSlice` has.
  Static dispatch monomorphizes it, so a `fn f<S: AsStrSlice>(s: S)` carries no
  dispatch at `-O2`. The parameter is taken by value, so a call site writes the
  literal bare — `f("banana")`, never `f(&"banana")` — and a `&String` still
  passes through the blanket `impl<T: AsStrSlice> AsStrSlice for &T`.
- Every `StrSlice` method that returns another view declares `stores[self]` too.
  A view holds its bytes in a reference field, and the spec's reference-storage
  rule counts reading one out of the receiver as the receiver escaping.
- The view must scalarize: a three-field struct built and read in one function
  leaves no `struct.new` behind. That is a property of the optimizer, so it is
  tested as one.
- `StrSlice` carries the search and split surface — `contains`, `starts_with`,
  `find`, `split`, `split_once`, the trims and `strip_*` — and `String`'s own
  delegate to it. The ones that answer with part of their input return a view,
  so working on part of a string does not copy it out. `to_string` is where a
  caller that wants an owned string asks for one.

## Known gaps

- A view passed to a function the inliner leaves alone is still materialized.
  Closing it needs argument promotion, which `docs/optimizer.md` already lists
  as not implemented: a callee taking an aggregate by value and only reading its
  fields would take the fields instead.
- `String::push_str_range_unchecked` still takes a `(text, start, end)` triple
  rather than a view, and Kiln's generated parsers call it.
- A cast between two references whose referents share one representation head
  (`&ByteSlice` to `&Slice<u8>`) is not dropped, so it still hides the operand's
  shape from the rules that match on one. Only the unreferenced case is covered.
