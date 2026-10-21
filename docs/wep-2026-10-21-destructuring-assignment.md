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
`builtin::array_take`. That covers a list, but no other container. It is also a
method on one container, not something the language says.

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

### Locals and fields move

A target that is a local or a field is assigned by the existing ownership
analysis. The temporaries the desugaring introduces are dead after their one
read, and each source is overwritten before it is read again. So a
transposition of locals, of fields, or of a local and a field moves both values.
That is the analysis already in place, applied to the desugared form.

### A transposition within one container is `IndexSwap`

The prelude gains a fifth indexing trait:

```wado
pub trait IndexSwap<IndexType> with () {
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
- `c` names the same container both times. It is a local, a field path from a
  local (`self.items`), or a dereference of one. A call or any other expression
  that could yield a different container falls back.
- Neither index can change when the first element is assigned. A local that is
  not a target, a literal, a constant, and arithmetic over them all qualify. An
  index read out of `c` (`c[c[0]]`) does not.
- `c`'s type implements `IndexSwap` at the index type.

The call evaluates `c`, `i` and `j` once each. Where the desugaring is defined,
an impl must leave the container as the desugaring would. `a == b` is the
impl's to handle, and the result must then be unchanged. `List::swap` already
handles it.

The standard containers implement it. Each exchanges the values in place and
copies nothing:

| Container       | `IndexSwap` | How                                          |
| --------------- | ----------- | -------------------------------------------- |
| `List<T>`       | every `T`   | `List::swap`                                 |
| `Array<T>`      | every `T`   | the same two `array_take`s                   |
| `HashMap<K, V>` | every `V`   | two lookups, then a swap in the values' list |
| `TreeMap<K, V>` | every `V`   | two lookups, then a swap in the values' list |

A map exchanges the values of two existing keys. The keys and their order stay
as they were. A missing key panics, as a read of `m[k]` does.

The `List` and `Array` impls take `affine T`. An affine element cannot be
copied, so `IndexValue` gives no read of one and the desugaring is not defined
for it. The transposition still is, as the call. A sort over a list of affine
values exchanges elements this way. A map holds no affine value yet
([WEP: Affine Types § Maps and sets of affine values](./wep-2026-10-17-affine-types.md#maps-and-sets-of-affine-values)),
so its impls take a copyable `V`.

### What is not guaranteed, and why

A rotation (`[a, b, c] = [b, c, a]`) and a transposition across two containers
(`[xs[i], ys[j]] = [ys[j], xs[i]]`) keep the desugaring's meaning but get no
copy-free guarantee.

Both would need a container to give up a value and leave its slot empty until
the value comes back. That breaks the desugaring's meaning when two targets
turn out to be one slot. In the rotation with `a` and `c` the same slot, the
desugaring copies one value into two places. Moving values cannot produce that
result. Two containers cannot be told apart either: two `&mut` to the same
list are legal, so `xs` and `ys` may be one list.

A transposition within one container avoids both problems. With `a == b`, the
desugaring leaves the container unchanged, and so does a correct `index_swap`.
The container sees both indices, so it can handle that case itself.

### A fallback that copies is reported

A destructuring assignment that copies a composite value is reported by the
surviving value-copy remark
([WEP: Optimizer Remarks](./wep-2026-06-03-optimizer-remarks.md)). That is the
remark's purpose, and a copy here is the cost the statement was written to
avoid. The remark anchors to the statement and adds a note that names the
cause:

- the shape is not a transposition (a rotation, or a right side that is not the
  targets swapped);
- the two subscripts name different containers, or a container the compiler
  cannot show to be the same;
- an index may change when the first element is assigned;
- the container's type does not implement `IndexSwap`.

A copy of a scalar costs nothing, so it is never reported.

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
- [ ] Show that a transposition of locals and of fields moves: a fixture whose
  `wir_expect` finds no value copy.
- [ ] Add `IndexSwap` to the prelude, with the impls on `List`, `Array`,
  `HashMap` and `TreeMap`.
- [ ] Recognize the transposition and lower it to `index_swap`, with fixtures
  for each condition that makes it fall back.
- [ ] Extend the surviving value-copy remark with the cause note for a
  destructuring assignment.
- [ ] Write the rules into `spec-expressions.md` (the assignment) and
  `spec-standard-traits.md` (`IndexSwap` and its dispatch), and add both to
  the cheatsheet.

## Known gaps

### Other target patterns

The left side takes only a flat tuple of targets. `_`, a rest `..`, a nested
tuple and a struct pattern (`{ x, y } = p`) are not accepted.

### Copy-free rotation and cross-container exchange

A rotation, or an exchange between two containers, copies its composite values.
The remark reports each one.
