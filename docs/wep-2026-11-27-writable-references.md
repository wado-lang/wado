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
since none of them copies.

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

### `&` of a type replaced on assignment is its value

A primitive, `enum`, `flags`, `fn` or reference has no storage a reference
could share. A `&P` of one is a copy of the value, wherever it is taken from: a
local, a field or an element. An assignment to the place afterwards does not
reach it.

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
always a mistake under the rule above. The compiler warns where it is certain:
the reference, the assignment and the later read are in one body, and the type
is known to be replaced on assignment. It stays silent in a body generic over
the type, and across a call. The warning is best effort and promises to find
nothing, so no program is refused over it.

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

## Known gaps

### The shared cell has no spelling

Nothing is chosen. It is a compiler-known type, so a keyword or a sigil is
open to it as a name is. The spelling may sit at the use (a field's type says
it is shared) or at the declaration (a type whose every value is shared, as
Swift's `class` is and as an unrestricted resource already is). Under the
second, `let b = a` copies or shares, and the declaration that decides is
elsewhere.

### What a shared cell is made from

Whether a cell can be taken from an existing place, or only made by putting a
value into a new one, is not decided. Under the second, a `let mut` variable
changes only through its name or a `&mut` visible at a call.

### Reading, writing and comparing a cell

Not decided: whether a cell is dereferenced implicitly, and whether `==` on
two cells compares what they hold or which cell they are. `Signal<T>` writes
the second by hand with `ref_eq`.

### `fn mut` closures

A `fn mut` closure holds the `&mut` it captured, so a copyable one is the same
escape. Rill keeps such closures in lists and calls them later.

### `&mut` of a type replaced on assignment

A primitive, `enum`, `flags`, `fn` or reference has nothing a write could
reach in place. Two mechanisms stand behind `&mut` of one today, and the place
decides which
([Reference Representation](./wep-2026-06-13-reference-representation.md)):

| Taken from         | Kept past the call            | Used within the call            |
| ------------------ | ----------------------------- | ------------------------------- |
| A local            | the local becomes a heap cell | the local becomes a heap cell   |
| A field or element | an error                      | a temporary, written back after |

The first row is a shared cell and the second is copy-in, copy-out. A generic
body meets the difference as a `RefMut` bound. `core:value`'s
`ValueSerializer { out: &mut v }` depends on the first row.

Ada passes an elementary type by copy for `in out` and a composite one
possibly by reference, a split by kind of type. Swift defines `inout` as
copy-in, copy-out for every type, and exclusivity keeps the difference
unobservable. Without exclusivity it is observable: a read of the place by
another path during the call sees the old value.

### `variant`

A `variant` is replaced on assignment, yet its payload is written in place.
Much of the corpus writes `if let Some(e) = &mut self.f` and depends on that.
It fits neither row above.

### The stale-read warning has no name

It is a lint like `shadowed_name`, waived by `allow`. Its name is not chosen,
and neither is whether a `variant` counts, given the gap above.

### Migration

Every copyable struct holding a `&mut` either becomes affine or moves to a
cell. How many there are is not measured.

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
