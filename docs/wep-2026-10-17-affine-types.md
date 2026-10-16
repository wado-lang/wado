# WEP: Affine Types — Swift's Noncopyable Discipline for Resources

## Context

### The guarantee

An affine resource is a handle the host owns: a CM `own`, a `stream`, a
`future`, a guest resource with a destructor. The program must keep one
invariant for each: at every point it has exactly one owner, that owner drops
it once or hands it to the next owner, and no reference to it outlives the
owner.

The invariant has four parts, and they are not equally serious:

| Part                  | What breaking it does                                                                                           |
| --------------------- | --------------------------------------------------------------------------------------------------------------- |
| No double drop        | The second drop traps, unless the handle index was reused, in which case it drops another resource.             |
| No use after drop     | The same.                                                                                                       |
| No leak               | A table slot is held until the instance ends.                                                                   |
| Drop at a known point | For a `stream`, a `future` or an HTTP body, the drop is a signal (`DROPPED`). A late drop hangs the other side. |

The last row sets the baseline. A stream flows through `?`, through
`Option<Trailers>` and through the tuple `Future::new()` returns, so the
discipline that keeps a stream safe must reach generic code, references and
aggregates. A rule that is strict for streams and lax for other handles saves
nothing: the strict machinery has to exist, and once it exists it costs nothing
to apply everywhere. One discipline, then, for every affine resource.

### Why the current design cannot keep it

[WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md) made
resources move-only and built a move check and a cleanup pass. Both are sound
for what they see. What they cannot see follows from three premises of the
value-semantics model that an affine value contradicts:

1. Reading a value is a copy, and a generic `T` is copyable. Nothing in a
   signature says otherwise, so the check reads the receiver kind as a proxy
   for what the body does (#2364). A body that copies through `&mut`
   (`TreeMap::get_or_insert`), or one that copies a `T` it took by value
   (`fn dup<T>(x: T) -> [T, T]`), breaks the proxy with no diagnostic.
2. A reference never dangles. True for a GC value, which the collector keeps
   alive. False for a handle, which is dead the moment it is dropped. Wado's
   `&R` is first-class, so it is stored, returned and captured freely.
3. The only way out of a place is a copy. There is no `take`, `replace` or
   partial move, so `h.f`, `*r`, `..base`, `list[i]` and `for x of list` all
   copy.

Each of these compiles today and traps at run time with
`unknown handle index`, or worse, reaches a reused index:

```wado
fn dup<T>(x: T) -> [T, T] { return [x, x]; }
let [a, b] = dup(Fields::new());         // two owners

let a: List<Fields> = [Fields::new()];
let b = a;                               // two owners of every element
for let x of a { eat(x); } eat_list(a);  // the loop copies, the list keeps

let v = m.get_or_insert("k", Fields::new());  // the map keeps what it returned
fn take(r: &Fields) { let x = *r; eat(x); }   // copied out of a reference

let r = &f; eat(f); r.has(name);         // dangling reference
let g = || f.has(name); eat(f); g();     // dangling capture
struct V { r: &Fields } let v = V { r: &f }; eat(f); v.r.has(name);
fn mk() -> &Fields { let f = Fields::new(); return &f; }

let c = h.f; eat(c); eat_h(h);           // a field moved, the holder kept
let h2 = H { ..h, n: 2 };                // the spread copies h.f
```

And `*r = Fields::new()` through a `&mut Fields` panics the compiler
([D6](./wep-2026-06-13-reference-representation.md#known-implementation-divergences)).

The issue is not any one gap. Every rule that infers ownership from a signature
has a counterexample, because the signature does not carry the fact.

### Approaches considered

| Approach                                  | Why not                                                                                                            |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Hand-written `drop` (Go, C#, Java)        | A `?` path forgets its drop. For a stream that is a hang, exactly where the guarantee matters most.                |
| Reference counting (Swift ARC, Nim, Koka) | Needs a release point. A closure, a `Box` cell or an iterator state is reclaimed by the collector, which has none. |
| Arena or scope object                     | Drops at the scope, not at the point the stream needs.                                                             |
| Finalizers                                | Wasm GC has none, and a finalizer's timing is undefined where one exists.                                          |
| Linear types (Austral)                    | Removes cleanup synthesis, but leaves the generic problem in place.                                                |
| Scoped use (`bracket`, `with`)            | A resource cannot be held in a structure past the scope: no pool, no body written later.                           |
| Post-monomorphization check               | Errors point into the stdlib's body, with no span the editor shows.                                                |
| Finish the signature inference            | Unsound by construction: see above.                                                                                |
| A run-time cell around every handle       | Turns a double drop into a certain trap. A safety net, not a design: it does not make a drop happen.               |

### The model: Swift's noncopyable types

Swift solved this problem for the same kind of value. SE-0390's motivating
example is `struct FileDescriptor: ~Copyable { deinit { close(fd) } }`, which
is a Wado resource. Its rules, refined by SE-0427 (generics), SE-0429 (partial
consumption) and SE-0432 (`switch`), are:

- A type declares itself noncopyable. A struct or enum that holds a noncopyable
  value must declare it too, or it is an error.
- A type parameter is copyable by default. `T: ~Copyable` lets an argument be
  noncopyable, and the body is then checked, at its definition, to copy no
  `T`.
- A parameter is `borrowing`, `inout` or `consuming`; a method's `self` is the
  same three. A borrow is second-class: it is a parameter and nothing else.
- An unconsumed value is destroyed at the end of its scope.
- A local or a consumed `self` may give up one field; the rest are destroyed
  with it.
- `Optional` and `Result` accept noncopyable payloads. `Optional.take()` moves
  the payload out.

Wado already has most of the surface: `resource`, `fn drop(self)`, `&T` /
`&mut T` / `T` parameters, `self` / `&self` / `&mut self` receivers, the move
check, scope-exit drop. What it lacks is the generic rule, the second-class
reference, and the ways out of a place.

Swift's own gaps are worth knowing, because they mark what is hard rather than
what Swift forgot. `Array` does not accept a noncopyable element after two
years; `InlineArray` and `Span` arrived in 6.2 instead. A local borrow binding
is still a pitch. An escaping closure could not capture a noncopyable value at
first, and may now only by consuming it.

## Decision

### One name, two positions

The marker is a sealed prelude trait, `Affine`, written with syntax that
exists. The word is the one the spec already uses for the linearity.

```wado
impl Affine for Holder;              // declaration: Holder is move-only
pub variant Option<T: Affine> { … }  // permission: T may be an affine type
fn dup<T>(x: T) -> [T, T] { … }      // unmarked: T is copyable
```

In declaration position, `impl Affine for X;` says `X` is move-only. A struct
or a variant that holds an affine resource, directly or through a field or a
payload, must write it, or the declaration is an error that names the field. A
type that holds none may write it too, and is then a move-only value type. A
tuple and a newtype cannot write it, so they are inferred: a tuple is affine
when an element is, a newtype when its base is.

In permission position, `T: Affine` says an argument for `T` may be affine.
Every type satisfies it, so `Option<i32>` is unchanged. What changes is the
body: it is checked at its definition to treat `T` as affine. Without the
bound, `T` is copyable, and `dup::<Fields>` is a type error at the
instantiation, which names the parameter and the bound that would admit it.

The two readings differ, as Swift's `~Copyable` differs between
`struct S: ~Copyable` and `func f<T: ~Copyable>`. The alternative, a
suppression syntax (`?Copy`, `~Copy`), was rejected: it adds syntax to say the
same thing.

### What a body may do with an affine value

A binding whose type is affine, or is a `T: Affine`, may be:

- moved: a `let`, a by-value argument, a `return`, a place in an aggregate, a
  by-value `self`, an assignment into a place;
- referenced: `&x` or `&mut x` as an argument or a receiver;
- dropped: by scope exit, or by `x.drop()` on a resource.

It may not be read twice. The move check already states that for a resource;
it now states it for every affine type, generic `T` included, at the
definition. A read that would copy — a second use after a move, a `*r`, an
`r.f`, a `list[i]`, a capture by a closure — is the error.

The check runs on the `Semantics` layer, so it has spans and reaches the
editor, and it needs no instantiation to run.

### Second-class references

A `&R` or `&mut R` whose referent type is affine exists only between a call's
argument and the parameter that receives it. It cannot be bound by a `let`,
stored in a field or an element, returned, or captured by a closure, and `*r`
is not a by-value expression. A `&T` whose referent is not affine is unchanged
and stays first-class.

`&mut R` keeps its one write: `*r = value` replaces the referent and drops what
it held. That is the store rule cleanup already applies to a binding.

A `match` over a reference binds references, so an arm may read a payload
where it stands. A `match` over an affine value by value consumes it, and each
arm owns what it binds. The two forms are SE-0432's borrowing and consuming
`switch`, and they are told apart by the scrutinee's type, not by a keyword.

### Ways out of a place

- `Option<T: Affine>::take(&mut self) -> Option<T>` moves the payload out and
  leaves `None`. `replace(&mut self, value: T) -> Option<T>` swaps it.
- A field may be moved out of a local holder: `let f = h.f;` consumes `h.f`,
  and the holder is unusable as a whole afterwards. The other fields are
  dropped at scope exit, since a holder's drop is synthesized field by field.
- `..base` consumes `base`. A field the literal lists replaces the one `base`
  carried, which is dropped.
- An assignment into a field or an element drops what it replaces. Unchanged.

### Drop

Unchanged in substance: an owned, unmoved affine value is dropped at scope exit
on every path; a store drops what it replaces; a discarded value is dropped.
The rule now has nothing to exclude, since no copyable container holds an
affine value. A panic drops nothing.

### Standard library

- `Option<T: Affine>` and `Result<T: Affine, E: Affine>`, with `take` and
  `replace` on `Option`. `unwrap`, `expect`, `unwrap_or`, `map`, `map_err` and
  `ok_or` keep their by-value `self`.
- `Future<T: Affine>`, `Stream<T>`, `AsyncCall<T: Affine>`: a future carries an
  affine payload today (`Future<Result<Option<Trailers>, ErrorCode>>`).
- `List<T>`, `Slice<T>`, `TreeMap<K, V>`, `HashMap<K, V>`, `TreeSet<T>` and
  every iterator stay unmarked. `List<Fields>` is a type error. A collection
  of affine values is a later WEP, under
  [Known gaps](#a-collection-of-affine-values).
- Stdlib generics that hold a `T` by value in state the type system cannot see
  (an iterator's `repr`, a map's log) are the reason the list above is short.

### Web handles

A `linearity = "unrestricted"` handle is outside this discipline, as it is
today: copyable, compared by bits, upcast by copy, never dropped. The host
interns one handle per object, so the guest cannot own a slot alone; the fit
would be reference counting, which the collector rules out. The representation
that fits is `externref`, and the Component Model admits none
([the handle is an index, not a reference](./wep-2026-04-28-resource-inheritance.md#known-gap-the-handle-is-an-index-not-a-reference)).
The leak stays a known gap of [the web WEP](./wep-2026-04-01-web.md).

### What this replaces

In [WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md):

- "A call through a shared reference copies" and "A call through `&mut`
  extracts" are withdrawn. The signature now carries the fact, so there is
  nothing to infer.
- "No move out of a reference" becomes a consequence of second-class
  references, which refuse the `let` and the `return` alike.
- The move check and cleanup stay as the implementation. They are sound once
  the type system excludes what they could not see.
- Value-copy elision is unaffected: it serves copyable types, and an affine
  type never reaches a copy site.

In [WEP: Resource Lifecycle](./wep-2026-01-12-resource-lifecycle.md), "Arrays
of resources" is withdrawn: `List<File>` does not exist.

## Roadmap

1. [ ] Fix the `*r = value` panic through `&mut R` with a fixture. A compiler
   panic is P0 and does not wait for the rest.
2. [ ] `Affine` in the prelude, sealed. Declaration rule: a struct or variant
   holding an affine value without the marker is an error naming the field;
   a tuple and a newtype infer. Fixtures for each, and `resource_*` fixtures
   updated to declare their holders.
3. [ ] Instantiation rule: an affine argument for an unmarked parameter is a
   type error naming the parameter. `Option`, `Result`, `Future`, `Stream`,
   `AsyncCall` marked. Every fixture and stdlib site that builds a
   `List<Resource>` is rewritten or becomes a `compile_error` fixture.
4. [ ] Definition-site check of a body whose `T: Affine`: the move check runs
   over `T` bindings as over resource bindings. The stdlib's marked bodies
   pass it, or are rewritten until they do.
5. [ ] Second-class `&R`: `let`, field, element, `return`, closure capture and
   `*r` by value refused at the semantic layer, with the borrowing `match`
   kept. `&mut R` store through `*r` works and drops.
6. [ ] `Option::take`, `Option::replace`, partial move out of a local holder,
   `..base` over an affine holder.
7. [ ] Withdraw the receiver-kind rule and the TIR copy check from #2364, and
   the `resource_copy_out_of_reference_error` fixture with it.
8. [ ] Cleanup drops a `Stream` and a `Future` the body does not move, which
   it excludes today
   ([A `Stream` or `Future` handle nothing drops](./wep-2026-05-21-resource-ownership.md#a-stream-or-future-handle-nothing-drops)).
9. [ ] Spec: rewrite
   [Resource Ownership](./spec-components.md#resource-ownership), the resource
   paragraph of [Memory Model](./spec-memory.md), and the cheatsheet. Amend
   the two WEPs above.

## Known gaps

### A collection of affine values

`List<R>` is a type error, and nothing replaces it. A program that opens many
files holds them in a struct, an `Option` per slot, or opens them one at a
time. What a list of affine values needs — an element moved in and out with
no copy, iteration that borrows, a drop over every element — is a WEP of its
own. Swift has no `Array` of noncopyable values either, and shipped a fixed
`InlineArray` and a borrowing `Span` first.

### A closure cannot hold an affine value

A closure captures by reference, and a reference to an affine value is
second-class, so a closure may not capture one. Swift admits a consuming
capture; Wado does not yet, and a program hands the value in as a parameter
instead.

### The rest

Unchanged from
[WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md#known-gaps):
a panic drops nothing; a generic resource a user module declares is not
checked; `5 as Counter` mints a handle nothing owns; web handles leak.

## Open questions

- Whether a closure should be allowed to take an affine value by moving it in,
  and whether the closure is then itself affine.
- Whether a trait may bound `Self` with `Affine`, so that a trait method can
  be written over an affine `Self`.
- Whether `impl Affine for X;` on a type that holds no resource is worth
  admitting now, or later when a use appears.

## References

- [SE-0390: Noncopyable structs and enums](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0390-noncopyable-structs-and-enums.md)
- [SE-0427: Noncopyable generics](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0427-noncopyable-generics.md)
- [SE-0429: Partial consumption of noncopyable values](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0429-partial-consumption.md)
- [SE-0432: Borrowing and consuming pattern matching for noncopyable types](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0432-noncopyable-switch.md)
- [WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md)
- [WEP: Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
- [WEP: Resource Inheritance and Narrowing](./wep-2026-04-28-resource-inheritance.md)
- [WEP: Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)
- #2367
