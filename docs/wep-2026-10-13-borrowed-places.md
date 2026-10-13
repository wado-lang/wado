# WEP: Borrow Liveness and Assignment to a Borrowed Place

## Context

A reference names a place: a variable, a field, an element
([spec-memory § Reference Identity](./spec-memory.md#reference-identity)). Only
a field or element of a type that assignment replaces (a primitive, `enum`,
`flags`, `variant` or `fn`) gives a `&` a copy, which a later assignment leaves
behind. A `struct`, `List`, `String` or tuple is not in that list, so a
reference to one of its places sees a later assignment to that place.

The implementation diverges. A reference to an in-place type is the value's own
GC handle ([WEP: Reference Representation](./wep-2026-06-13-reference-representation.md)),
and an assignment points the place at a new object. Every reference taken
before stays on the old one:

```wado
let mut x = P { a: 1, b: 2 };
let r = &x;
x = P { a: 7, b: 8 };       // r.a is 1; the spec says 7
let get = || x.a;
x = P { a: 9, b: 9 };       // get() is 7; the spec says 9

let mut h = H { p: P { a: 1, b: 1 } };
let hp = &h.p;
h.p = P { a: 3, b: 3 };     // hp.a is 1; the spec says 3
```

Six fixtures pin the divergence as intended behavior. Each cites the "reads a
snapshot copy" rule of WEP: Reference Representation, which covers a
replace-on-assign field or element only.

Writing every such assignment in place is correct but slow. Value-copy elision
moves, shares and forwards storage on the premise that an assignment points the
place elsewhere. An assignment written in place then has to count as a write
inside the old storage, which forbids those elisions wherever it lands. A pass
that wrote every field assignment in place broke a tuple whose element shared
storage with the value it was read out of, and cost the `licm_*` and
`opt_hfs_*` fixtures their hoists.

Whether a reference to a place can still be read at a given point is the
question move, share and this assignment all ask. The parts of the answer were
written down where each client needed them, across WEP: Resource Ownership and
WEP: Value Semantics and Reference Retention. This WEP is where that analysis
is stated.

## Decision

### An assignment to a borrowed place writes in place

The semantics are the spec's: a reference names a place, and an assignment to
the place reaches every reference to it. Nothing in the spec changes.

An assignment `place = v` to an in-place type writes into the storage the place
holds when a holder of that storage may be read afterwards. It lowers as
`*&mut place = v` does
([D7](./wep-2026-06-13-reference-representation.md#known-implementation-divergences)):
through a `&mut` handle, leaf by leaf, following every field of an in-place type
into its own fields, so a reference into any part of the storage reads the new
value. A value made of the place's own parts (`x = X { f: x.g, g: x.f }`, the
copies elided) is read in full before any leaf is written. Anywhere else the
assignment stays a repoint, which is what makes it cheap: the old storage is
unread, and every elision that relies on a repoint still holds.

The ownership walk decides it at the assignment (`in_place_assigns`), and
lowering expands what it marked, in whatever position the assignment sits. The
walk counts the assignment as a write into the old storage and not as a
rebinding, so every decision it makes past the assignment already sees the
write. An answer the analysis cannot give is "readable", so imprecision costs a
repoint, never correctness: a borrow naming no field may hold any, and a place
no local roots writes in place. A function the walk does not analyze decides
without liveness: in place through a borrowed or captured local, and through a
reference as the observed fields say.

A body answers for its own holders from its liveness. What it cannot see is a
borrow another body holds while it runs, so a field assignment also writes in
place where some body's borrow through that field (`observed.rs`):

- outlives the borrowing body: returned, stored, or kept by a callee;
- is held in one of the borrowing body's locals while it calls something that
  may write the field;
- is lent to a call that may write the field.

What a call may write is the callee's own answer (`modref.rs`).

An element assignment `xs[i] = v` reaches the element through
`List::index_assign`, a call. The walk notes each such call whose elements are of
an in-place type and decides it as it decides a field. An element is borrowed
through a call (`index_ref`, an iterator's `next`), which names no field, so a
borrow of the whole list counts as one of the element. In bounds, lowering
writes the value into the element as above. Out of bounds it calls
`index_assign`, which traps with its message. `List`'s own code assigns its
backing array's slots, not a `List` element, so it keeps repointing them.

### What a call keeps of a borrow

Retention says which argument positions a call keeps and where. Whether what it
keeps can reach the storage a borrow names is read off the types
(`reach.rs`):

- A result that cannot hold the storage keeps no borrow of it.
  `rmsnorm(g, &x) -> List<&mut Value>` holds none of `x`'s storage.
- A reference reaches the borrowed storage only through an object of that
  storage's kind. What the object owns by value is its own: a `Value` owns a
  `List<&mut Value>`, which is not `x`'s. A sharing the walk made is the alias
  chains' to answer.
- A result that reaches only what the borrow's references lead to holds what
  the argument holds, not the argument. An iterator's `next` hands back an
  element of the list the iterator borrows, so whoever holds the element holds
  the list, and the iterator itself is free.

Retention records the types of what an escaping position hands out of sight. An
unknown callee keeps at most what its argument reaches, so `f(item)` in a
`map` lets the item escape and not the iterator it came out of. The caller pins
an argument's own storage only where those values reach it. Otherwise what the
argument holds escapes, and the argument itself is free.

A local declared without an initializer holds no storage until its first
assignment, so its first assignment cannot write in place. A local that a later
assignment writes in place carries a flag: the assignment repoints while the
flag is unset and writes in place once it is set.

### A borrow lasts while its holder is readable

A borrow that ends before the move does not pin its referent. A `&` call
argument to a non-storing callee does not. A `match` / `if let` over a `&place`
scrutinee is the same borrow and reads the same way, so
`if let Repeat(r) = &op { … }; ops.push(op)` moves `op` instead of deep-copying
it. What the arms bind names the scrutinee's storage, so an escape recorded
against a binding closes back over the binding's referent
(`propagate_escapes_to_referents`). A reference binding therefore pins the
scrutinee exactly as `&op` itself would wherever it outlives the match: a
retaining callee, an aggregate literal, a global, a write through a place, the
result.

A borrow stored in a local lasts as long as that local's value can be read.
This covers `let r = &a`, a call result a callee hands the borrow back in
(`let it = (&a).into_iter()`), and a binding of a `&a` scrutinee. A holder is
readable where it is live, where a copy read out of it is live, or anywhere once
its value has gone where no alias chain follows it, such as a literal, a result,
or a callee that keeps it. A scalar read out of a holder carries none of its
storage, so `P { a: r.a }` hands nothing on. So a double buffer that iterates
`&a` and then swaps `a` and `b` moves both lists every step.

A call result that is not stored is kept by whatever takes it. Passed straight
to another call, or borrowed for one, it lasts as long as that call keeps it, so
`view.chars().count()` pins nothing past the count. What a callee keeps is its
retention, inferred from its body
([WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md)).

The swap also needs flow-sensitive alias chains. `a = b` is an alias edge from
`a` to `b`. Where `b` is dead after every binding that reads it, it is rebound
before it is read again. A storage-sharing check walks through such an edge but
does not count `b` as holding the storage `a` took.

### What a root owns

Every rule asks who owns the storage a place names, and answers from the place's
root local. A `&` / `&mut` local is not that owner: its immutability says
nothing about whether the storage is written, its death frees nothing, and a
write the owner makes never mentions it. A root reached through a reference
resolves to the place it borrows (`let r = &a.b` answers at `a.b`). One this body
cannot resolve, a parameter or a reference a call returned, names storage the
body does not own and answers nothing.

### What is live at a point

Live means the storage is still readable, not that the local still is. A match
arm binding is its scrutinee's storage under a second name, and a `let` out of a
projection is its root's, so the live set at a write is closed over what each
live local took its value out of. Reading the local alone calls the storage dead
while a binding is still looking at it.

A `break` resumes at the loop's exit and a `continue` at its head, so the two
read different live sets. Answering both with the exit set drops every
loop-carried reader, and a write reached through a `continue` then looks
unobserved.

### How a write is placed

Two kinds of write reach different storage. An assignment that stays a repoint,
`p.f = x`, points `p.f` elsewhere. A reference already taken out of `p.f` is
unread there, by the rule above, so a repoint never conflicts with a binding
that read the place.

`*p = v` repoints nothing when the referent is an aggregate. The write is
expanded field by field into the storage the caller holds, so everything read
out of `*p` sees it. An assignment written in place is one of these.
Only a boxed borrow replaces a slot, and one predicate decides which of the two
an assignment is.

### What a call writes

What a call writes is read off the callee rather than assumed: `modref.rs`
collects each function's writes as fields of the type carrying them and closes
them over the call graph, so a read of one field survives a call that writes
another. The receiver is one such handle and every other `&mut` the call is
given is another, and the callee's own answer covers them alike. A callee with
no body reaches only the `&mut` arguments it is handed; anything this cannot
name writes everything.

Some handles the walk loses: one stored into an aggregate, and one given to a
callee with no body. Each writes the whole of what it was handed. That answer is
its own, never a field index standing for every field, because a caller
comparing paths cannot tell such an index from one naming a real field. It
reads the two as disjoint and ignores the very write it was told about. One
function mints the key both sides compare (`place::field_owner`), so what a
callee records and what a caller looks up cannot drift.

A `&mut` that outlives the expression taking it (stored, captured, or kept by
the callee's return) writes its root wherever its holder runs, which no point in
the walk names. So no binding shares storage read out of such a root. The
optimizer's alias query answers the same way: a write through a dereferenced
field, payload or element reaches whatever the reference was taken from.

### Paths

An `Index` or a `Variant` step names no type of its own, so a `Field` past one
answers for the element rather than for the container the caller holds. A path
reached through either writes the whole of what its root was lent. Keying it by
the element's field alone leaves a caller holding the container nothing to
match, and it shares storage the call goes on to write.

A path through a reference-typed field places nothing: it ends at storage its
root only borrows, so the root names the holder rather than the storage. A read
taken through one copies: the write that reaches its storage is rooted
elsewhere, where the root-keyed scan never looks. Two reference fields of one
root may name one storage as well, so splitting a read and a write at them
proves nothing. A write made through one is placed no better, and conflicts
with every binding live at it.

A reference bound out of such a field names no referent this walk can follow:
the field holds the reference, and the reference points elsewhere again. The
immutable-root rule abstains over one rather than answering about the holder.

### One resolver

Every analysis here asks one resolver what an expression names (`place.rs`).
Its answer separates a value of its own from a place the walk cannot follow, so
a shape no arm covers costs an elision rather than becoming a write nobody
records.

The clients are:

- Move and share in value-copy elision
  ([WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md#value-copy-elision-the-second-client)).
- An assignment to a borrowed place, above.
- A multi-value `&S` parameter: an immutable reference whose referent no write
  reaches while the call runs may arrive as one Wasm parameter per field.

## Roadmap

- [x] Write an assignment to an in-place place in place where a holder of its
  storage may be read afterwards, for a local and for a field
  (`ref_sees_reassigned_place`). Finishing means the six fixtures below follow
  the spec, and every benchmark's Wado row holds against `origin/main`: compare
  the wasm of every benchmark first, then time the rows that differ.
- [x] Rewrite the fixtures that pin the repoint: `capture_at_binding_inlined`,
  `copy_prop_refproj_capture`, `opt_ref_elim_call_replaces_referent`,
  `opt_ref_elim_loop_backedge`, `opt_ref_elim_mut_alias`,
  `value_copy_share_is_not_a_licence_to_skip`.
- [x] The same for a `List` element, `xs[i] = v`, which reaches the place
  through `index_assign`. `List`'s own code relies on repointing its backing
  array's slots (`push` fills an empty one, a swap moves handles), so the
  rewrite is the user-level assignment's, not every `Array` store's.
- [ ] Take a multi-value `&S` parameter, asking this analysis whether a write
  reaches the referent while the call runs.

## Known gaps

### An unanalyzed function writes in place on any borrow

A function the ownership walk does not analyze (`has_unsupported_form`, or no
local that needs a copy) reads no liveness. It writes in place through every
borrowed or captured local, even where the holder is already dead. That costs a
leaf-by-leaf store where a repoint would do.

### An in-place write writes every leaf

A borrow is recorded by the first field it takes, or by none, so what it reaches
below that is not known. An assignment that writes in place writes every leaf of
the value, where the leaves under the borrowed part would do. A borrow naming
no field (a whole-root `&h` among them) counts as one of every field, and a
borrow of the whole list as one of every element. An element of a `List`
reached through a reference writes in place wherever some body's borrow through
the field holding the list is observed (`observed.rs`), and always where the
reference is to the list itself.

### A destructured field's path is assumed to borrow

A pattern-destructured field's path is marked as borrowing, because the pattern
carries no type saying whether that field does, and the mark refuses those paths
a share outright. Every WIR golden is byte-identical with the mark removed, so
nothing measured pays for it.
