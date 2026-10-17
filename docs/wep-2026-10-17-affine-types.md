# WEP: Affine Types — Swift's Noncopyable Discipline for Resources

## Context

### The guarantee

An affine resource is a handle the host owns: a CM `own`, a `stream`, a
`future`, a guest resource with a destructor. The program must keep one
invariant for each: at every point it has exactly one owner, that owner drops
it once or hands it to the next owner, and no use reaches it after the drop.

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
   alive. False for a handle, which is dead the moment it is dropped.
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

### What the Component Model puts in lists and records

WIT places handles where a Wado value type holds them, and the generated
bindings follow:

| WIT position               | Wado binding                                                                   |
| -------------------------- | ------------------------------------------------------------------------------ |
| `list<tuple<own<R>, …>>`   | `Preopens::get_directories() -> List<[Descriptor, String]>`, read by `core:fs` |
| `list<own<R>>`             | `GpuCompilationInfo::messages() -> List<GpuCompilationMessage>`                |
| `list<borrow<R>>`          | `GpuQueue::submit(command_buffers: List<&GpuCommandBuffer>)`                   |
| a record field `borrow<R>` | `GpuBufferBinding { buffer: &GpuBuffer, … }`, and the other descriptors        |
| a record field `own<R>`    | `GpuCanvasConfigurationOwned { device: GpuDevice, … }`                         |
| `stream<own<R>>`           | `TcpSocket::listen() -> Result<Stream<TcpSocket>, ErrorCode>`                  |

A descriptor holding a `borrow` is built before a call and passed to it, often
through a `let` and a `push` in a loop. A design that forbids a list of affine
values, or a reference to one outside a parameter, cannot bind these
signatures.

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
| A run-time cell around every handle       | Makes a use after drop a certain trap, but makes no drop happen. Adopted for references only, below.               |

For references alone, three more:

| Approach                                      | Why not                                                                                                                       |
| --------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| Second-class references (Swift's `borrowing`) | A descriptor could only be written as a literal in argument position. No `let`, no `push`.                                    |
| Non-escaping holders (Swift's `~Escapable`)   | A borrow check within each body, against the language's stated absence of one, and a second kind of parameter for `List<&R>`. |
| Reference-counted handles (Swift's Metal API) | Swift makes a GPU object a class. Reference counting is ruled out above.                                                      |

### The model: Swift's noncopyable types

Swift solved the ownership half of this problem for the same kind of value.
SE-0390's motivating example is
`struct FileDescriptor: ~Copyable { deinit { close(fd) } }`, which is a Wado
resource. Its rules, refined by SE-0427 (generics), SE-0429 (partial
consumption) and SE-0432 (`switch`), are:

- A type declares itself noncopyable. A struct or enum that holds a noncopyable
  value must declare it too, or it is an error.
- A type parameter is copyable by default. `T: ~Copyable` lets an argument be
  noncopyable, and the body is then checked, at its definition, to copy no
  `T`.
- A protocol's `Self` is copyable by default too. `protocol P: ~Copyable`
  admits a noncopyable conformer.
- A noncopyable type may declare a `deinit`. A copyable one may not, since
  every copy would run it.
- A parameter is `borrowing`, `inout` or `consuming`; a method's `self` is the
  same three.
- An unconsumed value is destroyed at the end of its scope.
- A local or a consumed `self` may give up one field; the rest are destroyed
  with it.
- `Optional` and `Result` accept noncopyable payloads. `Optional.take()` moves
  the payload out.

Wado already has most of the surface: `resource`, `fn drop(self)`, `&T` /
`&mut T` / `T` parameters, `self` / `&self` / `&mut self` receivers, the move
check, scope-exit drop. What it lacks is the generic rule and the ways out of a
place. Its references depart from Swift's, for the reason the table above
gives.

Swift's own gaps mark what is hard rather than what Swift forgot. `Array` does
not accept a noncopyable element; `InlineArray` and `Span` arrived in 6.2
instead.

## Decision

### The `affine` keyword

```wado
affine struct Holder { f: Fields, n: i32 }   // declaration: Holder is move-only
affine variant Slot { Empty, Held(Fields) }

pub variant Option<affine T> { None, Some(T) }  // permission: T may be affine
fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E { … }
fn dup<T>(x: T) -> [T, T] { … }              // unmarked: T is copyable

affine trait Iterator { affine type Item; … }  // a trait for its Self, and
                                                // an associated type
```

`affine` is a keyword, as `effect` is, and stands in the same place: before a
type parameter it says what kind of parameter it is.

A declaration with `affine` is move-only. A named `struct` or `variant` must be
declared `affine` when it holds an affine value, directly or through a field or
a payload, or when it implements `Drop`. Otherwise the declaration is an error
that names the field or the impl. A type that does neither may not be declared
`affine`: no use for one has appeared. A tuple, a newtype and an anonymous
struct have no declaration to mark, so they are inferred: each is affine when
something it holds is. A `resource` is affine unless it declares
`linearity = "unrestricted"`, as today.

A parameter with `affine` admits an affine argument. The body is checked at its
definition to treat the parameter as affine. A parameter without it is
copyable, and `dup::<Fields>` is a type error at the instantiation, which names
the parameter and says `affine` would admit it.

`affine` names a kind, not a bound. A bound adds what a body may do, and this
takes copying away, so a trait could not state it. A kind also composes with
bounds: `<affine T: Eq>`.

A trait's `Self` is a type parameter too, so a trait states its kind the same
way. A default method is written over an abstract `Self` and checked once,
like a generic body. Without a mark it may copy `Self`, and an affine type that
used it would copy itself:

```wado
trait Twice { fn twice(&self) -> [Self, Self] { return [*self, *self]; } }
impl Twice for Fields {}   // compiles today, and returns two owners
```

`affine trait` admits an affine implementor and checks every default body at
the trait's definition with `Self` affine. An affine type implementing an
unmarked trait is an error at the impl. A body bounded by an unmarked trait is
unaffected: its `T` is copyable already, unless it says `<affine T>`.

The word is exact in both positions. Affine means used at most once. A
copyable value can be used once too, so every type is an `affine T` and
`Option<i32>` is unchanged. `unique` reads as a demand on the argument, which
`i32` does not meet, and elsewhere it names a pointer with one referent
(`unique_ptr`, a unique borrow).

### What a body may do with an affine value

A binding of an affine type, or of an `affine` parameter, may be:

- moved: a `let`, a by-value argument, a `return`, a place in an aggregate, a
  by-value `self`, an assignment into a place;
- referenced: `&x` or `&mut x`;
- dropped: by scope exit, or by `x.drop()` on a type that implements `Drop`.

It may not be read twice. The move check already states that for a resource;
it now states it for every affine type, `affine` parameters included, at the
definition. The error is any read that would copy:

- a second use after a move;
- a `*r` or an `r.f` read by value through a reference;
- a `list[i]` read by value.

The check runs on the `Semantics` layer, so it has spans and reaches the
editor, and it needs no instantiation to run.

### References reach a handle through a cell

A reference to an affine value is first-class, as every reference is. It may be
bound, stored, returned and captured, and a list or a record may hold one.

What keeps it from dangling is the handle's representation. An affine resource
is a cell: a GC object holding the handle and whether it is live.

- A move hands the cell on.
- A drop marks it dead, and so does a transfer as `own` across the boundary.
- A `borrow` an export receives is a cell marked dead when the export returns.
- Every use reads the handle through the cell. A use of a dead cell traps with
  a message naming the resource.

So ownership is static and a use after drop is a run-time trap. The two halves
need different guarantees: ownership decides when a drop happens, which a trap
cannot, while a reference only needs never to reach a dead handle. A reused
index is never reached either, since a dead cell no longer holds one.

The cell is also the box D6 asks for. `*r = value` through a `&mut R` drops the
handle the cell holds and stores the new one.

The optimizer may represent a resource as its bare handle wherever it proves no
use reaches the cell dead, as it may keep a primitive unboxed. The trap is the
cell's only observable effect, so a proof that the trap cannot fire is all the
erasure needs.

### No cast between an affine resource and an integer

`as` converts neither way between an affine resource and an integer. A cast
was the one way a raw handle entered or left the type system, and it moved
nothing. `r as i32` left `r` its owner, so a raw handle handed to an import
that takes it left two owners. `h as R` minted an owner each time it ran, so
two casts of one integer left two. With a cell, either direction would also
hide an allocation or a read behind syntax that otherwise converts for free.

A cast between an unrestricted handle and an `f64` is unchanged.

What casts did, the following do instead:

- The CM ABI is lifted by binding synthesis. `stream-new` and `future-new`
  return both ends packed in one `i64` (`readable | writable << 32`).
  Synthesis splits it and lifts each handle, as it lifts every other one, so
  `core:rt` loses `cm_stream_pair` and `cm_future_pair`.
- A builtin that passes a handle across states it in its signature:
  `call_indirect_stdout_write_via_stream(rx: Stream<u8>) -> Future<…>`, not
  `(rx: i32) -> i32`. The move check then sees `rx` handed over.
- An identity that owns nothing is a `ResourceId`, read by
  `ResourceId::of(&r)`. It takes a reference to any affine resource, and
  anything else is a type error. It is a static function rather than a method
  so that it takes no name from any resource: a method `id` would collide with
  one a resource declares, and a WIT-generated binding cannot rename its own.
  A `ResourceId` is a copyable value with `Eq` and `Ord`, so it keys a map, and
  nothing converts it back into a resource. It names a live resource only: a
  dropped one's identity may come back for a later one, as a `Waitable`'s
  does.
- A test double returns a real handle, from `Stream::<u8>::new()`, where it
  used to return `0 as Stream<u8>`.

### Ways out of a place

- `Option<affine T>::take(&mut self) -> Option<T>` moves the payload out and
  leaves `None`. `replace(&mut self, value: T) -> Option<T>` swaps it.
- A field may be moved out of a local holder: `let f = h.f;` consumes `h.f`,
  and the holder is unusable as a whole afterwards. The other fields are
  dropped at scope exit, since a holder's drop is synthesized field by field.
- `..base` consumes `base`. A field the literal lists replaces the one `base`
  carried, which is dropped.
- An assignment into a field or an element drops what it replaces. Unchanged.

A type that implements `Drop` gives up no field on its own: neither a partial
move nor `..base` over it is allowed. Its `drop` relies on the fields it was
built with, as SE-0429 holds for a type with a `deinit`.

### Drop

When a value is dropped is unchanged in substance: an owned, unmoved affine
value is dropped at scope exit on every path; a store drops what it replaces; a
discarded value is dropped. The rule now has nothing to exclude, since no
copyable container holds an affine value. A panic drops nothing.

What a drop does is the type's `Drop`:

```wado
pub affine trait Drop with _ {
    fn drop(self);
}
```

- A type without an impl drops its fields one by one, in declaration order,
  as today.
- A type with an impl runs its `drop`. The body may move fields out of `self`,
  and whatever is left at its end is dropped field by field. `self` is never
  dropped as a whole again, which would run `drop` once more.
- A resource's `fn drop(self)` is its `Drop` impl, whether it calls the
  Component Model's `resource.drop` or runs a guest destructor.
- Only an affine type implements `Drop`. A copyable type with one would run it
  for every copy.
- Each impl brings its own effects (`with _`), and a scope exit that drops the
  value performs them, as
  [WEP: Resource Lifecycle](./wep-2026-01-12-resource-lifecycle.md) has a
  function owning a resource declare its destructor's effects.

A field-by-field drop is wrong for some types. `AsyncCall` is one: a
`Subtask` traps on `drop` until it resolves, so an unawaited call must cancel
it first, and its result buffer is linear memory no resource frees. A writer
that buffers ahead of a stream must flush before the stream closes.

### A write returns what it did not deliver

The Component Model leaves with the writer what a write did not deliver. Three
cases do not deliver everything:

- a `stream.write` the reader took only a prefix of, while the stream stays
  open;
- a write of either kind the reader dropped before (`Dropped`);
- a cancelled write of either kind (`Cancelled`).

A copyable writer still has its own copy of what was left. An affine one has
nothing, so the write hands it back:

```wado
fn write(&self, value: T) -> FutureWrite<T>;       // FutureWritable<affine T>
fn write(&self, data: List<T>) -> StreamWrite<T>;  // StreamWritable<affine T>

struct FutureWrite<affine T> { result: CopyResult, rest: Option<T> }
struct StreamWrite<affine T> { result: CopyResult, rest: List<T> }
```

The write takes what it is given and returns the rest. That is exactly the part
of the buffer the Component Model leaves in the writer's table. A write blocks
until its copy ends, so the write call reports every case, a cancellation
included, and `cancel_write` returns nothing.

A caller that ignores the result discards the rest, and the rule for a
discarded value drops it at the end of the statement. Nothing leaks, and
nothing obliges the caller to look. `StreamWrite` loses its `count`, which
`data.len() - rest.len()` states. The optimizer may skip building a `rest` no
one reads.

### Standard library

- `Option<affine T>` and `Result<affine T, affine E>`, with `take` and
  `replace` on `Option`. `unwrap`, `expect`, `unwrap_or`, `map`, `map_err` and
  `ok_or` keep their by-value `self`.
- `Future<affine T>` and `Stream<affine T>`, with their writable ends. A
  future carries an affine payload today
  (`Future<Result<Option<Trailers>, ErrorCode>>`), and a stream does too
  (`Stream<TcpSocket>`). The read side needs nothing new: `StreamChunk<T>`
  holds a `List<T>`.
- `AsyncCall<affine T>` holds a real `Subtask` rather than a raw `i32` it
  casts into one at each use:

  ```wado
  affine struct AsyncCall<affine T> {
      subtask: Option<Subtask>,   // None where the call completed synchronously
      status: i32,
      buffer: ResultBuffer,       // the linear memory the result is lifted from
      lift: fn(i32) -> T,
  }
  ```

  `wait(self)` and `cancel(self)` take the subtask out and consume it, so a
  second call is a compile error and the shared `__cm_ended` flag goes. Its
  `Drop` cancels a subtask nobody awaited before dropping it, and frees the
  buffer.
- `affine trait`: `Drop`, `Iterator`, `IntoIterator`, `Eq`, `Inspect` and
  `Display`. A resource implements `Eq` and `Inspect` today, and a list
  iterated by value is an affine iterator.
- `List<affine T>`. A method is available for an affine element when an
  `impl<affine T> List<T>` declares it, and the definition-site check holds
  its body to moving elements. The rest stay in `impl<T> List<T>`, for a
  copyable element only. Iterating a list by value consumes it, so
  `IntoIterator::into_iter` takes `self` by value and `Iterator::Item` is an
  `affine type`.
- `Slice<T>`, `TreeMap<K, V>`, `HashMap<K, V>`, `TreeSet<T>` and their
  iterators stay unmarked.
- `ResourceId`, with `ResourceId::of(&r)` over every affine resource.

`wado-from-idl` declares `affine` every record and variant it generates that
holds an affine resource.

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
- "No move out of a reference" becomes the body rule above: a by-value read
  through a reference is a copy.
- The move check and cleanup stay as the implementation. They are sound once
  the type system excludes what they could not see.
- Value-copy elision is unaffected: it serves copyable types, and an affine
  type never reaches a copy site.
- The known gap "An integer casts to an affine resource" is closed.

In [WEP: Reference Representation](./wep-2026-06-13-reference-representation.md),
D6 is settled: a resource is boxed, and the box is its cell.

In [WEP: Resource Lifecycle](./wep-2026-01-12-resource-lifecycle.md), the open
question of a user-defined drop for a type that is not a resource is answered:
an affine type implements `Drop`.

## Roadmap

1. [ ] Fix the `*r = value` panic through `&mut R` with a fixture. A compiler
   panic is P0 and does not wait for the rest.
2. [ ] `affine` as a keyword on `struct` and `variant`. A named declaration
   holding an affine value or implementing `Drop` without it is an error
   naming the field or the impl, and one doing neither with it is an error
   too; a tuple, a newtype and an anonymous struct infer. `wado-from-idl`
   emits it, and the generated stdlib is regenerated. `resource_*` fixtures
   declare their holders.
3. [ ] `affine` on a type parameter, an associated type and a trait, with the
   definition-site check: the move check runs over its bindings as over
   resource bindings, and over a marked trait's default bodies with `Self`
   affine. An affine type implementing an unmarked trait is an error at the
   impl. The prelude traits named under Standard library are marked.
4. [ ] `Drop`: a resource's `fn drop(self)` as its impl, a user impl on an
   affine type, the fields left at the end of `drop` dropped one by one, the
   effects a scope exit performs through it, and no partial move or `..base`
   over a type that implements it.
5. [ ] `List<affine T>`: the methods that move elements in
   `impl<affine T> List<T>`, `IntoIterator::into_iter(self)`, and
   `Iterator`'s `affine type Item`.
6. [ ] No cast between an affine resource and an integer: synthesis splits
   and lifts the `stream-new` / `future-new` pair, the raw-handle builtins
   take and return resources, `ResourceId::of`, and the test doubles in
   `core:kiln`, `core:cli` and `package-gale` return real handles.
   `AsyncCall` holds a `Subtask`, with its `Drop`. It comes before the cell,
   which a cast would have to allocate, and before cleanup drops a `Stream`,
   which `log_stdout` would then drop twice.
7. [ ] The handle cell: the representation, the trap, the `borrow` an export
   receives, and `*r = value` through it. Then the optimizer's erasure where
   it proves no use reaches a dead cell.
8. [ ] `Option::take`, `Option::replace`, partial move out of a local holder,
   `..base` over an affine holder.
9. [ ] `FutureWrite<T>` and `StreamWrite<T>`: a write returns what it did not
   deliver, in every case the result reports, and the stdlib's write loops
   (`write_all`) read `rest` rather than `count`.
10. [ ] Instantiation rule: an affine argument for an unmarked parameter is a
    type error naming the parameter. `Option`, `Result`, `Future`, `Stream`,
    `AsyncCall` and `List` are marked by then.
11. [ ] Withdraw the receiver-kind rule and the TIR copy check from #2364, and
    the `resource_copy_out_of_reference_error` fixture with it.
12. [ ] Cleanup drops a `Stream` and a `Future` the body does not move, which
    it excludes today
    ([A `Stream` or `Future` handle nothing drops](./wep-2026-05-21-resource-ownership.md#a-stream-or-future-handle-nothing-drops)).
13. [ ] Spec: rewrite
    [Resource Ownership](./spec-components.md#resource-ownership), the
    resource paragraph of [Memory Model](./spec-memory.md), and the cheatsheet.
    Amend the WEPs above.

## Known gaps

### Maps and sets of affine values

`TreeMap<K, V>`, `HashMap<K, V>` and `TreeSet<T>` with an affine element are
type errors. A program keyed on affine values keeps them in a `List` and the
keys beside it.

### A closure cannot own an affine value

A closure captures by reference, so it may use an affine value but never owns
one. A closure that outlives the value's owner traps on its first use after the
drop.

### The rest

Unchanged from
[WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md#known-gaps):
a panic drops nothing; a generic resource a user module declares is not
checked; web handles leak.

## References

- [SE-0390: Noncopyable structs and enums](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0390-noncopyable-structs-and-enums.md)
- [SE-0427: Noncopyable generics](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0427-noncopyable-generics.md)
- [SE-0429: Partial consumption of noncopyable values](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0429-partial-consumption.md)
- [SE-0432: Borrowing and consuming pattern matching for noncopyable types](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0432-noncopyable-switch.md)
- [SE-0446: Nonescapable types](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0446-non-escapable.md)
- [WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md)
- [WEP: Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
- [WEP: Resource Inheritance and Narrowing](./wep-2026-04-28-resource-inheritance.md)
- [WEP: Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)
- [WEP: Stream Copy Results](./wep-2026-08-30-stream-copy-result.md)
- #2367
