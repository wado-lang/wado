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

### A call lends what it returns

A function whose returned value holds a part of a parameter lends that part
rather than copying it, and its return convention says which parameter and
which part. The caller decides the copy at the call, as it does for a binding of
any other place it does not own: shared where nothing writes the parameter's
storage while the result is live, copied otherwise. A function that returns its
argument outright already works this way.

## Roadmap

- [ ] A frame-local aggregate holds a shared part: a struct, tuple or list
  literal element, and a value a `List` method stores into a local list.
- [ ] A returned value lends a part of a parameter, and the caller decides its
  copy.

## Known gaps

### Long-lived heap storage

A value read from or written to storage that outlives the body, such as a cache
held by `self`, is copied. Sharing it needs every write to that storage, by
every function, to be known not to reach the part.
