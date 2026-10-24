# WEP: Destructuring Assignment and a Swap Without Value Copies

## Context

A _value copy_ is the deep copy value semantics calls for: a call to the
synthesized `$value_copy$T` helper, or an `array_clone` of a backing array
([WEP: Optimizer Remarks](./wep-2026-06-03-optimizer-remarks.md)). Its cost
grows with the value. Writing a scalar, or repointing a handle, is not one.

Exchanging two values the obvious way makes a value copy of each:

```wado
let t = xs[i];   // a value copy of the element
xs[i] = xs[j];   // another
xs[j] = t;
```

For a composite element (a `struct`, `String`, `List`, tuple), a sort written
this way spends most of its time in value copies. The ownership analysis turns
the same three lines into moves where the places are locals
([WEP: Reference Liveness](./wep-2026-10-13-referenced-places.md) says the
double buffer swap moves both lists). It cannot do so for an element. `xs[i]`
reads through `IndexValue`, which hands back a value copy, and `xs[j] = t`
writes through `IndexAssign`. Nothing tells the analysis that the two calls
reach one slot.

`List::swap(a, b)` already exchanges two elements without a value copy, through
`builtin::array_take`. That covers a list, but no other container.

Rust's answer is `std::mem::swap(&mut a, &mut b)`. That shape does not fit Wado:

- `&mut` of a primitive, `enum` or `flags` element is an error
  ([spec-memory § Mutable References to Fields and Elements](./spec-memory.md#mutable-references-to-fields-and-elements)),
  so `swap(&mut xs[i], &mut xs[j])` cannot be written on a `List<i32>`.
- `&mut` of a `variant` element points to a copy, so replacing the value
  through it would not reach the list.

A place is not a concept the language surface has. What it has is the
assignment target. Each form a target takes already dispatches to a trait of
its own
([spec-standard-traits § Dispatch](./spec-standard-traits.md#dispatch)): `c[i]`
reads through `IndexValue`, `c[i] = v` writes through `IndexAssign`, and
`c[i].f = v` writes through `IndexRefMut`. A swap can be one more form on that
list, with a trait of its own, rather than an operation on places.

## Decision

### Destructuring assignment

A tuple of assignment targets on the left of `=` assigns each target its
element of the right side:

```wado
[a, b] = [b, a];
[s.x, s.y] = [s.y, s.x];
[xs[i], xs[j]] = [xs[j], xs[i]];
[q, r] = divmod(n, d);
```

Each target is anything that may stand alone on the left of `=`: a local, a
field, a subscript, or a dereference. The right side is any expression of a
tuple type whose arity matches. A mismatched arity is a compile error. The
tuple of the targets' types is the right side's expected type, so
`[a, b] = [1, 2]` with `i64` targets types the literals as `i64`, and
`[xs, ys] = [[], []]` types them as the lists' types.

The statement means what this desugaring means, as Rust's destructuring
assignment does:

```wado
let [$0, $1] = [b, a];   // the whole right side first
a = $0;                  // then each target, left to right
b = $1;
```

The right side is evaluated in full before any target is written, so
`[a, b] = [b, a]` exchanges the values. The operands of a target, such as a
subscript's container and index, are evaluated when that target is assigned.

Copies are as-if ([spec-memory § Value Semantics](./spec-memory.md#value-semantics)).
The desugaring states the result, not how many value copies produce it. The
rest of this decision says where the result is guaranteed to come with none.

### Which targets can be one slot

Exchanging values instead of copying them keeps the desugaring's meaning only
while no two targets are one slot. Where two are, the desugaring may put one
value in both, and exchanging values cannot produce that result.

A target is _owned_ when it is a local, or a field path from a local, reached
through no reference and no subscript. Two owned targets that differ as paths
are never one slot. Any other target may be one slot: `*r`, `r.f`,
and `self.f` under a `&self` or `&mut self` receiver reach through a reference,
two `&mut` to one value are legal, and two subscripts may name one element.

### A permutation of owned targets makes no value copy

A permutation of owned targets with no reference to any of them readable after
the statement is assigned without a value copy: a transposition
(`[a, b] = [b, a]`), a rotation (`[a, b, c] = [b, c, a]`), or one over fields
(`[s.x, s.y] = [s.y, s.x]`).

The ownership analysis moves the values. The temporaries the desugaring
introduces are dead after their one read, and each source is overwritten before
it is read again. A field's liveness is its own: a read of `s.y` or of `s` after
`s.x` is overwritten reads no value `s.x` held, so `s.x` moves while `s` stays
live.

### A transposition within one container is `IndexSwap`

The prelude gains a fifth indexing trait. It is an `affine trait`, as
`IndexRef`, `IndexRefMut` and `IndexAssign` are, so an impl may take an affine
element:

```wado
pub affine trait IndexSwap<IndexType> with () {
    fn index_swap(&mut self, a: IndexType, b: IndexType);
}
```

A destructuring assignment of this shape is a call to it:

```wado
[c[i], c[j]] = [c[j], c[i]];   // c.index_swap(i, j)
```

It is that call where all of these hold:

- The right side is a tuple literal whose two elements are the two targets, in
  the other order.
- `c` is one path written twice: a local, a field path from a local
  (`self.items`), or a dereference of one. A call, or any other expression that
  could yield a different container, falls back.
- Each index is a literal, a constant, a local, or arithmetic over them. An
  index read out of `c` (`c[c[0]]`) falls back.
- No index local, and no reference local that `c` dereferences (`r` in `*r`),
  is referenced or captured anywhere in the body. The desugaring's first
  `index_assign` could otherwise write through that reference, and its second
  target would then name another slot, or another container. A closure
  capturing the local counts, though it writes no `&`. A reference to the
  container itself does not: a write through it changes the container's
  contents, never which container `c` names.
- `c`'s type implements `IndexSwap` at the index type.

The call evaluates `c`, `j` and `i` once each, in that order: the order in which
the desugaring's right side first reads them, so an index that traps (a
division by zero) fails as it would there. The transposition is recognized
before the desugared right side is typed. Typing `c[j]` as a read first would
report a value copy of an affine element, so the order matters.

An impl must behave as the desugaring does wherever the desugaring is defined.
It must leave the container as the desugaring would, `a == b` included, and fail
where the desugaring fails. The desugaring reads `c[b]` before `c[a]`, so an
impl checks `b` first, then `a`, each as `IndexValue` checks a read.

The standard containers implement it. Each exchanges the values in place and
makes no value copy:

| Container       | `IndexSwap`           | How                                             |
| --------------- | --------------------- | ----------------------------------------------- |
| `List<T>`       | every `T`, affine too | two `array_take`s                               |
| `Array<T>`      | every `T`, affine too | two `array_take`s                               |
| `HashMap<K, V>` | every copyable `V`    | two lookups, then two `array_take`s on `values` |
| `TreeMap<K, V>` | every copyable `V`    | two lookups, then two `array_take`s on `values` |

A map exchanges the values of two existing keys. The keys and their order stay
as they were. A missing key panics, as a read of `m[k]` does.

The `List` and `Array` impls take `affine T`. An affine element cannot be
copied, so `IndexValue` gives no read of one and the desugaring is not defined
for it. The transposition still is, as the call. A sort over a list of affine
values exchanges elements this way. A map holds no affine value yet
([WEP: Affine Types § Maps and sets of affine values](./wep-2026-10-17-affine-types.md#maps-and-sets-of-affine-values)),
so its impls take a copyable `V`.

`List::swap` moves elements the same way, but it is not that impl: its contract
checks `a` before `b`, the reverse of the desugaring's order.

### What is not guaranteed, and why

A destructuring assignment with a target that is not owned keeps the
desugaring's meaning, but is not guaranteed to make no value copy unless it is
the transposition above. That covers a rotation among subscripts, a transposition
across two containers (`[xs[i], ys[j]] = [ys[j], xs[i]]`), and any permutation
reached through a reference.

Those targets may be one slot, as above. Moving values there would also need a
container to give up a value and leave its slot empty until the value comes
back. A transposition within one container is the exception: the container
sees both indices, so `index_swap` can tell `a == b` from `a != b` and do what
the desugaring does in each case, which the impl contract above requires.

### A fallback that makes a value copy is reported, an affine one is an error

A destructuring assignment that makes a value copy is reported by the surviving
value-copy remark
([WEP: Optimizer Remarks](./wep-2026-06-03-optimizer-remarks.md)). That is the
remark's purpose, and a value copy here is the cost the statement was written
to avoid. The remark covers what it covers elsewhere, the entry package's
functions. It anchors to the statement and adds a note that names the cause:

- the right side is not a permutation of the targets (`[a, b] = [b, b]`,
  `[a, b] = [c, a]`);
- a reference to an owned target is readable after the statement;
- a target is not owned, and the statement is not a transposition within one
  container;
- the two subscripts name different containers, or a container the compiler
  cannot show to be the same;
- an index local, or a reference local the container is reached through, is
  referenced or captured;
- the container's type does not implement `IndexSwap`.

A scalar has no value copy, so a scalar target is never reported.

An affine value cannot be copied, so an affine destructuring assignment that
falls back is not a remark. It is the compile error that copying an affine
value is, with the same note naming the cause.

### No new token

A dedicated operator (`a <-> b`) and a function on references
(`builtin::swap(&mut a, &mut b)`) were considered. The operator adds a token
and covers nothing destructuring assignment does not. The function runs into
the `&mut` rules above. Destructuring assignment needs no new token, reads as
TypeScript's does, and assigns a tuple result (`[q, r] = divmod(n, d)`) as
well.

## Roadmap

- [x] Parse and type a destructuring assignment, desugar it, and state its
  evaluation order and arity error in fixtures. Write the assignment into
  `spec-expressions.md` and the cheatsheet.
- [x] Show that a permutation of owned targets with no readable reference
  moves, a rotation included: a fixture whose `wir_expect` finds no value copy.
- [ ] Add `IndexSwap` to the prelude, with the impls on `List`, `Array`,
  `HashMap` and `TreeMap`.
- [ ] Recognize the transposition before typing the desugared form and lower
  it to `index_swap`. Fixtures cover each condition that makes it fall back,
  an affine list, and an out-of-range index failing as the desugaring does.
- [ ] Extend the surviving value-copy remark with the cause note for a
  destructuring assignment, and report an affine fallback as the compile error
  carrying the same note. Fixtures cover each cause, for the remark and for
  the error.
- [ ] Write `IndexSwap` and its dispatch into `spec-standard-traits.md`, and
  add it to the cheatsheet.

## Known gaps

### Other target patterns

The left side takes only a flat tuple of targets. `_`, a rest `..`, a nested
tuple and a struct pattern (`{ x, y } = p`) are not accepted. Neither is a
compound assignment to a tuple (`[a, b] += [1, 2]`).

### `unchecked` on a transposition

`unchecked xs[i]` vouches for a `List` index contract. Nothing says where
`unchecked` goes on `[xs[i], xs[j]] = [xs[j], xs[i]]`, so its `index_swap`
always checks both indices.

### A permutation of owned targets with a readable reference copies

Where a reference to an owned target is readable after the statement, an
assignment to that target writes in place
([WEP: Reference Liveness § An assignment to a referenced place writes in place](./wep-2026-10-13-referenced-places.md#an-assignment-to-a-referenced-place-writes-in-place)).
The desugaring then makes a value copy of the target's old value before writing
over it. The reference reads the new value, so the result is correct. The cost
is that one value copy. An affine target is not copied but moved, so the move
check refuses a read of a reference taken before the statement, as it refuses
one after any move
([WEP: Affine Types § A reference is read only while its local holds the value](./wep-2026-10-17-affine-types.md#a-reference-is-read-only-while-its-local-holds-the-value)).
