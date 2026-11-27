# WEP: Writable References — A Borrow That Is Not Copied, and a Shared Cell

## Context

A `&T` is meant to be a read-only view. It is not one when `T` holds a `&mut F`:
a copy of the struct carries the `&mut F` with it, and so does a copy of the
field, so `&&mut F` yields a `&mut F`. A function handed `&T` can write `F`.

The copy is not a bug in one rule. A `&mut T` is an ordinary copyable value
that writes wherever it is reached from, which is what Go's `*T` is and not
what Rust's `&mut T` is. Code relies on it in two different ways:

| Role          | Example                                     | What it needs                               |
| ------------- | ------------------------------------------- | ------------------------------------------- |
| A borrow      | a parameter; `out: &mut String` in a parser | write access handed down, for a while       |
| A shared cell | `cell: &mut Cell<T>` in Rill's `Signal<T>`  | one object, many copies, written by `&self` |

`Signal<T>` documents that a copy is the same signal, and `set(&self)` writes
through the field. It builds the cell as a reference to a local of the function
that returns it. That is a heap cell spelled as a borrow.

### What other languages do

Languages with value semantics take one of three positions on a writable
reference held inside a value.

| Position                  | Languages         | Rule                                                                            |
| ------------------------- | ----------------- | ------------------------------------------------------------------------------- |
| It cannot be held         | Swift, Hylo, Move | A copyable value has no writable reference among its fields                     |
| It weakens along the path | Rust, Pony, D     | Reached through a read-only view, it reads but does not write                   |
| It is shallow             | C++, C#, Zig, Go  | A read-only view of the holder promises nothing about what the holder points to |

- Swift's `inout` is a mode, guarded by the law of exclusivity. Its one stored
  writable view, `MutableSpan`, is noncopyable and nonescapable, and its
  writing operations are `mutating`. A pitch for an `exclusive` modifier says
  `mutating` is too strong there: it is used to demand exclusive access, not
  to change the span.
- Hylo lets a mutable value be reached through one binding at a time. `inout`
  and `let` are projections. A note from its predecessor Val describes
  `remote inout` fields; the current documentation has no such feature.
- Move forbids a reference as a struct field outright, and treats `&mut T` as
  a subtype of `&T`.
- Rust's `&mut` is not `Copy`, so it cannot be copied out from behind a `&`,
  and a reborrow through `&` is a `&`. Shared mutation has its own types
  (`Cell`, `RefCell`), and `&T` promises nothing past one.
- Pony combines the capability of the path with the capability of the field at
  every field read: a `ref` field read through a `box` is a `box`.
- D's `const` is transitive, and a `const(S)` converts to a mutable `S` only
  when `S` holds no mutable indirection. Its users still ask for head const.
- C# allows a `ref` field only in a `ref struct`, and the field says whether
  it writes (`ref` or `ref readonly`). A `readonly ref struct` must not rebind
  its `ref` fields, yet may write through them.

Only D and Pony get the transitive rule without a borrow checker, and both do
it by making the permission part of the type on every path. No language found
closes the escapes one at a time.

## Decision

A writable reference is a borrow, and shared mutable state has a type of its
own. The two roles `&mut T` plays today get one spelling each.

### `&mut T` is affine

A `&mut T` moves and is never copied, and so is every type holding one. The
existing rule that no affine value moves out of a reference then refuses
`&&mut T` to `&mut T`, whether the copy is of the reference or of a struct
around it.

Generic code needs nothing new. `&mut F` instantiates only a type parameter
declared `affine`, so a body that copies a `T` out of a `&List<T>` never sees
one.

This is not exclusivity. Two `&mut x` of one place stay legal. The rule stops
a read-only view from yielding write access, and says nothing about how many
writers a place has.

### A place writes only through writable steps

A place reached through a dereference is writable only when every dereference
on the way is of a `&mut`. Through `rr: &&mut T`, an assignment to `rr.x`, a
`&mut **rr` and a `&mut self` call are errors. Affinity does not cover these,
since none of them copies. The dereference of a shared cell starts the path
again: the place a cell holds is writable whatever reached the cell.

### A `&mut` is reborrowed where one is expected

It follows from affinity: without it, passing a `&mut` parameter to two calls
in a row would be a use after a move. Where the expected type is `&mut T`, a
`&mut T` reached through a writable place is reborrowed, and the original
stays usable. There are no lifetimes, so the reborrow is a second reference to
the same referent.

### A shared cell is its own type

A value that is one object under every copy, and that a `&` may write, is a
shared cell. A copy of a cell is the same cell. `&T` is read-only as deep as
the first cell, and promises nothing past it, as `&R` already promises nothing
about a resource ([Components](./spec-components.md)).

`Signal<T>` holds a cell. A parser's `out: &mut String` stays a borrow, and
the struct holding it becomes affine.

### The cell is `Shared<T>`

`Shared<T>` is a prelude type the compiler knows, declared without a
definition as `Array<T>` is: `pub type Shared<T>;`.

- `Shared::new(value)` puts a value into a new cell, and nothing else makes
  one. No cell is taken from a variable or a field, so a `let mut` variable
  changes only through its name or a `&mut` written at a call.
- `*c` is the place the cell holds. It reads and writes whatever path reached
  `c`, a `&` included, and `c` needs no `mut`. A field access and a method call
  dereference a cell as they do a reference.
- A copy of a cell is the same cell, so the deep copy of a value stops at one.
  `Shared::new(*c)` makes a separate cell.
- `==` asks whether two cells are one. Two cells holding equal values are not
  interchangeable, since a write to one leaves the other, and `*a == *b`
  compares what they hold. So a struct holding a cell derives the `Eq` that
  `Signal<T>` writes by hand today. A cell has no order, so it is not `Ord`.
- A cell is not `Hash`: `==` is identity, and Wasm GC gives a reference no
  address to hash. So a struct holding a cell derives no `Hash`. A type that
  wants one writes it, hashing what it holds, as `StrSlice` writes its own over
  the text it views. Such a hash agrees with `==`, since one cell holds one
  value.
- `T` is not affine. An affine value has one owner, which drops it, and a cell
  every copy shares has none. Counting the copies would find the last one, and
  [Affine Types](./wep-2026-10-17-affine-types.md#why-not-reference-counting)
  declined to count. Code that shares an affine value keeps it in one owner and
  hands a `&mut` down, or lets a closure refer to it inside the owner's scope.

It is a type and not a `struct`, because a `struct` is a value: its copy is
deep, an assignment to it writes field by field, and `Eq`, `Serialize` and the
rest derive from its fields. Each would need an exception. A type with no
definition also keeps the representation a `&mut T` has today, the object
itself for an in-place `T` and a heap cell for one replaced on assignment,
where a `struct` would add an object in front of every in-place `T`.

It is a name and not a keyword or a sigil. `shared` is already a variable name
across the corpus, a keyword would need an expression form and its precedence,
and every other compiler-known type (`List<T>`, `Option<T>`, `String`) is a
name.

### A closure stays copyable, and `&T` stops at one

A closure captures variables, and a variable two copies of a closure share is
a cell in all but name. A `fn` closure may also capture a `Shared<T>` and write
it. So what `&T` promises stops at a closure as it stops at a cell, whichever
of `fn` and `fn mut` it is, and making `fn mut` affine would restore nothing.
`fn mut` keeps its meaning: the closure writes a variable it captured, and
calling it needs a `mut` root.

A closure that captures a `&mut` refers into an affine local, so the rule that
already holds such a closure inside the local's scope
([Affine Types](./spec-affine.md#references-into-affine-values)) holds this
one. That covers a binding a pattern scopes too: a closure cannot carry it past
the arm that stores it back.

### `&` of a type replaced on assignment is its value

A primitive, `enum`, `flags`, copyable `variant`, `fn` or `&` reference has no
storage a reference could share. A `&P` of one is a copy of the value, wherever
it is taken from: a local, a field or an element. An assignment to the place
afterwards does not reach it, and for a `variant` neither does a write to its
payload.

An affine value is never copied, a `&mut` and an affine `variant` included, so
a `&` of one names its place and sees what is written there.

Today a `&i32` of a local sees a later assignment and one of a field does not.
Making every `&P` see it is not open: Wasm GC has no pointer into a struct or
an array, so it would take a heap cell for every primitive field and element.

A program Rust accepts cannot tell the difference, since Rust forbids the
assignment while the `&` is live. `&T` still shares the storage of a `struct`,
`List`, `String` or tuple, so a generic body that holds a `&T` across an
assignment to its place answers by the kind of `T`.

A closure's capture is not a `&P`. A closure captures the variable, and reads
what the variable holds when it runs, as it does today.

### A write that leaves a `&P` behind warns

An assignment to a place while a `&P` taken from it is still read is almost
always a mistake under the rule above. The lint `stale_reference` warns at the
read, naming the assignment, and `allow` waives it. It fires where the compiler
is certain:
the reference, the assignment and the later read are in one body, and the type
is known to be replaced on assignment. It stays silent in a body generic over
the type, and across a call. The warning is best effort and promises to find
nothing, so no program is refused over it.

### `&mut` of a type replaced on assignment lasts for a scope

A `&mut P` of such a type is taken only where the syntax fixes the point its
value goes back to the place:

- A call argument or a receiver: `f(&mut place)`, `place.take()`. The value is
  copied in before the call and copied out after it.
- The scrutinee of a pattern: `if let Some(p) = &mut place { … }`. A binding of
  an in-place type is the object the place holds. A binding of a type replaced
  on assignment holds the value for the arm, and the arm's end rebuilds what
  was matched around it and stores that back.
- A `for let v of &mut xs`, where one turn of the loop is the scope.

Binding one to a variable, storing one in a value and returning one are errors,
and a shared cell is what such code wants. The rule is by type alone: a local,
a field and an element take the same `&mut`, so a local no longer becomes a
heap cell for a reference to keep.

A generic body that only uses a `p: &mut T` takes every `T`. One that stores or
returns a `&mut T` needs `T: RefMut`, as `IndexRefMut::Output` already does.

A `variant` falls under this rule whole, and is no case of its own. Its payload
of an in-place type is reached through a pattern, which is the one thing a
`variant` adds to a primitive. Giving every `variant` slot an object of its
own, as an affine `variant` has, would make it an aggregate like a `struct` and
need no rule at all. It was declined for its cost: `Option<String>` stops being
a nullable reference, and every `variant` field and element gains an
allocation.

The line between the two kinds of `&mut` is `RefMut`, which the specification
already draws. A type with an object that is its place can be borrowed, and one
without lasts for a scope:

| The type                                                                    | `&mut` of it                   |
| --------------------------------------------------------------------------- | ------------------------------ |
| `RefMut`: a `struct`, a tuple, `List`, `String`, `i128`, any affine value   | an affine borrow of the place  |
| not `RefMut`: a primitive, `enum`, `flags`, a copyable `variant`, `fn`, `&` | scoped, as this section states |

An affine `variant` stays on the first row. A field, an element or a payload
holds it in an object of its own
([Affine Types](./wep-2026-10-17-affine-types.md#an-affine-value-is-an-object-and-a-place-is-a-nullable-reference-to-it)),
so `&mut` of one names a place and is stored and returned. Giving it the scoped
rule would move the value out for the scope where a copyable one is copied, and
leave the place empty meanwhile. Another route would then read a moved place,
which is unspecified, where it reads a stale value of a copyable one. Swift
moves in and out this way and has exclusivity to forbid the other route. The
object costs an allocation only where a `variant` wraps a resource.

A function cannot hand such a `&mut` back, and nothing is added to make one.
The scoped forms work because the place is written where it is used: the
compiler reads `map[k]`, and stores to `map[k]` after. A function's result is a
value with no place attached. An index and a field cover every container, on
`IndexValue` and `IndexAssign`, which `List`, `HashMap` and `TreeMap` implement
for every element. `IndexRefMut` keeps its `Output: RefMut`, and no other
standard library function returns a `&mut`. A named accessor is written one of
three ways: return the in-place object that holds the place, move the value out
and back, or take a closure the accessor calls between its read and its store.
An accessor that yields a scoped place, as Swift's `_modify` and Hylo's
subscripts do, waits for a need the three do not meet.

A scope that stores back does so at every exit, whether or not its binding was
written: its end, a `return`, a `break`, a `continue` and a `?`. A trap is no
exit. It does not unwind, so it stores nothing back, as it runs no drops
([Affine Types](./spec-affine.md#drop)). A handler always resumes or diverges,
so an operation dispatched inside a scope adds no exit of its own. A write to the place by another route inside the scope is therefore
overwritten: `if let Some(n) = &mut self.count { self.count = null; *n += 1 }`
leaves `Some`. A binding of an in-place type stores nothing back, so none of
this reaches it.

- An assignment inside the scope, in the same body, to the scope's place, to a
  place holding it or to a part of it is a compile error. The place is read off
  the syntax, so the loss is certain.
- A write the compiler cannot place warns under the lint `lost_write`, at the
  write: one a callee makes, one through a reference that may name the same
  object, one to an element whose index may be the current one. It is best
  effort, as `stale_reference` is.
- A read of the place inside the scope sees the value from before the binding
  was written. It warns under `stale_reference` where one body shows both,
  since it is the same mistake seen from the place, and is no error: a read
  ahead of any write through the binding is harmless.

Swift splits the same way. An overlap it can prove is a compile error, and the
rest is checked at run time, which Wado does not do. Ada leaves the overlap
unspecified.

### Declined

- A parameter mode in place of `&mut` (Swift's `inout`). It removes the copy,
  the generic instantiation and `&&mut` by construction, since a mode is not a
  value. It also removes every API that returns write access: `IndexRefMut`,
  `get_mut`, `for let p of &mut ps`. Swift and Hylo answer that with yielding
  accessors and projections, a mechanism Wado does not have and affine types
  make unnecessary.
- One copyable handle, with `&T` declared shallow. It needs no migration, and
  leaves `&T` promising what it promises today.
- Keeping the spelling `&mut` was weighed against its drift from Rust. Under
  this decision the drift narrows to the absence of exclusivity, so the
  spelling stays.

## Roadmap

The corpus is rewritten to the rules, whatever it takes: no rule above bends to
spare a call site. Each step leaves the tree building and its tests passing.

- [ ] The specification states every rule above, each marked not yet
  implemented until its step lands.
- [ ] `Shared<T>` exists: the type, `Shared::new`, `*c`, the automatic
  dereference, and `==` as identity.
- [ ] Every cell written as a `&mut` moves to `Shared<T>`: `Signal<T>` in Rill,
  `ValueSerializer { out: &mut v }` in `core:value`, and each `&mut` to a
  type replaced on assignment that a variable, a value or a `return` holds.
- [ ] The scoped forms store back from every place: a call argument and a
  receiver on an element, a pattern binding of a type replaced on
  assignment, and `for let v of &mut xs`.
- [ ] A `&mut` to a type replaced on assignment is refused outside the scoped
  forms, a local's included, and a local stops becoming a heap cell for one.
- [ ] A `&P` is its value from every place, and `stale_reference` warns.
- [ ] An assignment to a scoped place inside its scope is an error, and
  `lost_write` warns.
- [ ] A place is writable only through writable steps.
- [ ] `&mut T` is affine and reborrowed where one is expected, and every
  copyable struct holding one becomes affine.

## Known gaps

- Two scoped `&mut` of one place in one call, `f(&mut x, &mut x)` for a scalar
  `x`, are each stored back after the call, and no rule orders the stores. The
  value the place ends with is undecided.

## References

- [Hylo: bindings](https://docs.hylo-lang.org/language-tour/bindings)
- [Val: Remote Parts](https://github.com/val-lang/val/blob/8d4dadc3ecb3d8f098a4e4f12139eb8bcd3950e4/Docs/RemoteParts.md?plain=1)
- [SE-0467: MutableSpan](https://forums.swift.org/t/se-0467-mutablespan/78454)
- [`exclusive` parameter ownership modifier](https://forums.swift.org/t/exclusive-parameter-ownership-modifier/83503)
- [C#: Low Level Struct Improvements](https://learn.microsoft.com/dotnet/csharp/language-reference/proposals/csharp-11.0/low-level-struct-improvements)
- [Pony: Combining Capabilities](https://tutorial.ponylang.org/capabilities/combining-capabilities.html)
- [D: const FAQ](https://dlang.org/const-faq.html)
- [Move: References](https://move-book.com/reference/primitive-types/references)
- [Research: Swift's Ownership Decisions](./research-swift-ownership.md)
- [Research: Rust Reference Semantics Edge Cases](./research-rust-reference-semantics.md)
