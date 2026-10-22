# WEP: Destructuring Assignment and Copy-Free Swap

## Context

Assignment copies, so exchanging two values the obvious way copies both:

```wado
let t = xs[i];   // copies the element out
xs[i] = xs[j];   // copies again
xs[j] = t;
```

For a composite element (a `struct`, `String`, `List`, tuple) each copy is a
deep copy, and a sort written this way spends most of its time copying. The
ownership analysis turns the same three lines into moves where the places are
locals ([WEP: Reference Liveness](./wep-2026-10-13-referenced-places.md) says
the double buffer swap moves both lists). It cannot do so for an element. `xs[i]`
reads through `IndexValue`, which hands back a copy, and `xs[j] = t` writes
through `IndexAssign`. Nothing tells the analysis that the two calls reach one
slot.

`List::swap(a, b)` already exchanges two elements without a copy, through
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
tuple type whose arity matches. A mismatched arity is a compile error.

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
The desugaring states the result, not the number of copies. The rest of this
decision says where the result is guaranteed to come without a copy.

### Which targets can be one slot

Moving instead of copying keeps the desugaring's meaning only while no two
targets are one slot. Where two are, the desugaring may copy one value into
both, and moving values cannot produce that result.

A target is _owned_ when it is a local, or a field path from a local, reached
through no reference and no subscript. Two owned targets that differ as paths
are never one slot. Any other target may be: `*r`, `r.f` and `self.f` reach
through a reference, two `&mut` to one value are legal, and two subscripts may
name one element.

### Owned targets move

A destructuring assignment whose targets are all owned is assigned by the
existing ownership analysis. The temporaries the desugaring introduces are dead
after their one read, and each source is overwritten before it is read again.
So any permutation of owned targets moves its values: a transposition
(`[a, b] = [b, a]`), a rotation (`[a, b, c] = [b, c, a]`), or one over fields
(`[s.x, s.y] = [s.y, s.x]`). That is the analysis already in place, applied to
the desugared form.

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
  (`self.items`), or a dereference of one. Evaluated once, it names one
  container whatever references it passes through. A call, or any other
  expression that could yield a different container, falls back.
- Neither index can change when the first element is assigned. A literal, a
  constant, a local the body never takes a reference to, and arithmetic over
  them qualify. A local with a reference taken does not, since an
  `index_assign` may write through that reference. Neither does an index read
  out of `c` (`c[c[0]]`).
- `c`'s type implements `IndexSwap` at the index type.

The call evaluates `c`, `i` and `j` once each. The transposition is recognized
before the desugared right side is typed. Typing `c[j]` as a read first would
report a copy of an affine element, so the order matters.

An impl must behave as the desugaring does wherever the desugaring is defined.
It must leave the container as the desugaring would, `a == b` included, and fail
where the desugaring fails. The desugaring reads `c[b]` before `c[a]`, so an
impl checks `b` first, then `a`, each as `IndexValue` checks a read.

The standard containers implement it. Each exchanges the values in place and
copies nothing:

| Container       | `IndexSwap`           | How                                             |
| --------------- | --------------------- | ----------------------------------------------- |
| `List<T>`       | every `T`, affine too | two `array_take`s, as `List::swap` does         |
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

### What is not guaranteed, and why

A destructuring assignment with a target that is not owned keeps the
desugaring's meaning, but gets no copy-free guarantee unless it is the
transposition above. That covers a rotation among subscripts, a transposition
across two containers (`[xs[i], ys[j]] = [ys[j], xs[i]]`), and any permutation
reached through a reference.

Those targets may be one slot, as above. Moving values there would also need a
container to give up a value and leave its slot empty until the value comes
back. A transposition within one container is the exception: with `a == b` the
desugaring leaves the container unchanged, and the container sees both indices,
so `index_swap` can do the same.

### A fallback that copies is reported, an affine one is an error

A destructuring assignment that copies a composite value is reported by the
surviving value-copy remark
([WEP: Optimizer Remarks](./wep-2026-06-03-optimizer-remarks.md)). That is the
remark's purpose, and a copy here is the cost the statement was written to
avoid. The remark covers what it covers elsewhere, the entry package's
functions. It anchors to the statement and adds a note that names the cause:

- a target is not owned, and the statement is not a transposition within one
  container;
- the two subscripts name different containers, or a container the compiler
  cannot show to be the same;
- an index may change when the first element is assigned;
- the container's type does not implement `IndexSwap`.

A copy of a scalar costs nothing, so it is never reported.

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

- [ ] Parse and type a destructuring assignment, desugar it, and state its
  evaluation order and arity error in fixtures.
- [ ] Show that a permutation of owned targets moves, a rotation included: a
  fixture whose `wir_expect` finds no value copy.
- [ ] Add `IndexSwap` to the prelude, with the impls on `List`, `Array`,
  `HashMap` and `TreeMap`.
- [ ] Recognize the transposition before typing the desugared form and lower
  it to `index_swap`. Fixtures cover each condition that makes it fall back,
  an affine list, and an out-of-range index failing as the desugaring does.
- [ ] Extend the surviving value-copy remark with the cause note for a
  destructuring assignment.
- [ ] Write the rules into `spec-expressions.md` (the assignment) and
  `spec-standard-traits.md` (`IndexSwap` and its dispatch), and add both to
  the cheatsheet.

## Known gaps

### Other target patterns

The left side takes only a flat tuple of targets. `_`, a rest `..`, a nested
tuple and a struct pattern (`{ x, y } = p`) are not accepted.
