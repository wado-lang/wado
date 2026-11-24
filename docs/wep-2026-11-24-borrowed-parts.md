# WEP: Borrowed Parts — Sharing a Copy Held by an Aggregate or Handed Out by a Call

## Context

Value semantics copy a value wherever it gains a second owner. Lowering skips the
copy where it can prove nothing observes the difference: a final read moves, a
fresh value needs no copy, and a read-only binding shares its source while
nothing writes either side
([Resource Ownership](./wep-2026-05-21-resource-ownership.md#sharing)). The
specification already allows the result: two places may be stored once
([Memory Model](./spec-memory.md)).

The share is decided per binding. A value read into anything else is copied
whatever happens to it next:

- a part of an aggregate this body builds: a struct, tuple or list literal
  element, or an element `push` stores into a local `List`;
- a value a call returns that holds a part of its argument.

Gale's generator spends 15% of its samples in `$value_copy$` (a lower bound:
copies inlined into their caller do not count), spread over many call sites with
none above 1%. Read at those sites, the copies fall into three shapes:

1. A body builds a temporary out of parts it borrowed and only reads it:
   `elements_from(elements, i + 1)` extended by a suffix, `&[rep.element]`,
   `RepeatElement { ..*rep, kind: Star }` pushed into a list the walk reads.
2. A function returns a value holding a part of its argument:
   `predicate_site` returns a fresh `PredicateSite` whose `bindings` field is the
   receiver's, and `stamp_alternatives` rebuilds each `Alternative` with
   `..*alt`.
3. A value moves in or out of long-lived heap storage: a cache read, a cache
   write, `self.parser_rules[rid]`.

The other benchmarks spend under 0.1% in copies.

## Decision

Copy elision stays static. A runtime copy-on-write representation was weighed
and declined: it adds a check to every in-place write of every program to pay
for copies most programs no longer make.

### A part shares as a binding does

A value stored into a part of an aggregate is a binding whose holder is that
aggregate. It shares its source under the rules a binding shares under, read
against the holder:

- nothing writes the source while the holder is live;
- nothing writes the part through the holder;
- nothing hands the part, or the holder, to a new owner.

Whether a write reaches the part is the question a binding's share already asks
of a write, by type: a `push` replaces a slot of the holder's array and writes
nothing inside an element, so it does not reach an element's storage.

A part can leave its holder without a copy in two ways, and either refuses the
share:

- a call through `&mut` that returns a value able to hold the part (`pop`,
  `remove`) extracts it;
- the holder itself read as a value, other than at its final use, is a second
  owner of everything it holds.

### Where the decision is made

The optimizer decides it, in `nir/copy_elision`, on the body after inlining.
There the readers of the aggregate are in the body, and the heap analysis tells
the objects the body allocates from the ones it was handed.

The inliner keeps each `$value_copy$T` helper a call until the optimizer's loop
first converges, so the pass finds every copy whole in the body it lands in.
It drops the call where three things hold:

- every object the copy holds is allocated in the body and stays in it;
- nothing in the body writes an object of a type the copy reaches, through the
  copy or the aggregate holding it;
- nothing in the body writes such an object through the source.

It runs ahead of `nir/value_copy_demote`, which would otherwise turn a copy the
pass can drop whole into a copy of the list's spine.

The heap analysis keeps objects apart by depth: what a load from an object
reads, a field, an element or a payload, is one depth below it, and a call's
summary says how deep below each parameter it writes. So a `push` into a local
`List<Tree>` writes the list and its array, not the `Tree` parts it holds,
though a `Tree` reaches both types.

### A call lends what it returns

A function whose returned value holds a copy of a part of a parameter lends that
part rather than copying it. The caller decides the copy at the call, as it does
for a binding of any other place it does not own. A function that returns its
argument outright already works this way.

`nir/copy_elision` gives such a function a `$lend` clone without those copies.
The clone records how deep below the result the lent parts sit, and how deep
below each parameter they come from. A call moves to the clone where three
things hold in the caller:

- nothing writes the result at or below the lent depth;
- nothing writes an argument at or below the depth its parts are lent from;
- no object the result holds is returned, held by a global, or stored into an
  object the caller did not allocate.

The caller may still grow the list it was handed, or extend it with a
parameter's elements: those write the spine above the lent parts, and the
store's direction is into the result, not out of it.

A copy whose source is local to the function and reaches the result only
through the copy is lent too, not dropped in place. The function cannot tell
whether one source object reaches the result through more than one copy, as in
a loop pushing the same value each turn. Dropping those copies would make the
result's parts alias each other, while the caller's check keeps them unwritten.

A `$value_copy$T` hands a unit case of a variant on as it is, since the case
holds nothing. Its summary takes what it shares from `T`, so such a copy still
reads as fresh.

## Roadmap

- [x] A frame-local aggregate holds a shared part: a struct, tuple or list
  literal element, and a value a `List` method stores into a local list.
- [x] A returned value lends a part of a parameter, and the caller decides its
  copy.

## Known gaps

### Long-lived heap storage

A value read from or written to storage that outlives the body, such as a cache
held by `self`, is copied. Sharing it needs every write to that storage, by
every function, to be known not to reach the part.
