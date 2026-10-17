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

| Approach                                  | Why not                                                                                                             |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Hand-written `drop` (Go, C#, Java)        | A `?` path forgets its drop. For a stream that is a hang, exactly where the guarantee matters most.                 |
| Reference counting (Swift ARC, Nim, Koka) | Needs a release point. A closure, a `Box` cell or an iterator state is reclaimed by the collector, which has none.  |
| Arena or scope object                     | Drops at the scope, not at the point the stream needs.                                                              |
| Finalizers                                | Wasm GC has none, and a finalizer's timing is undefined where one exists.                                           |
| Linear types (Austral)                    | Removes cleanup synthesis, but leaves the generic problem in place.                                                 |
| Scoped use (`bracket`, `with`)            | A resource cannot be held in a structure past the scope: no pool, no body written later.                            |
| Post-monomorphization check               | Errors point into the stdlib's body, with no span the editor shows.                                                 |
| Finish the signature inference            | Unsound by construction: see above.                                                                                 |
| A run-time check on every use             | Makes a use after drop a certain trap, but makes no drop happen. Adopted below for what the static rule cannot see. |

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
- A parameter is `borrowing`, `inout` or `consuming`; a method is
  `borrowing`, `mutating` or `consuming` of its `self`.
- An unconsumed value is destroyed at the end of its scope.
- A local or a consumed `self` may give up its fields one at a time. It is no
  longer usable as a whole, and what it still holds is destroyed with it.
- `Optional` and `Result` accept noncopyable payloads. `Optional.take()` moves
  the payload out.

Wado already has most of the surface: `resource`, `fn drop(self)`, `&T` /
`&mut T` / `T` parameters, `self` / `&self` / `&mut self` receivers, the move
check, scope-exit drop. What it lacks is the generic rule and the ways out of a
place. Its references depart from Swift's, for the reason the table above
gives: Swift keeps a reference from outliving its value with a borrow check,
and Wado has none. So Wado takes Swift's static rule for copying, and a
run-time rule for what a reference reaches.

Swift's own gaps mark what is hard rather than what Swift forgot. `Array` does
not accept a noncopyable element; `InlineArray` and `Span` arrived in 6.2
instead.

## Decision

Three rules carry the design. A static rule says what a body may do with an
affine value, and it is Swift's. A representation rule says what an affine
value is at run time: an object, held by nullable places. A dynamic rule says
what a move, a drop and a use do to those places. Everything else in this
section follows from the three.

### The `affine` keyword

```wado
affine struct Holder { f: Fields, n: i32 }   // declaration: Holder is move-only
affine variant Slot { Empty, Held(Fields) }

pub variant Option<affine T> { None, Some(T) }  // permission: T may be affine
fn apply<affine T, effect E>(f: fn(T) -> T with E, x: T) -> T with E { … }
                                             // `affine` stands where `effect` does
fn dup<T>(x: T) -> [T, T] { … }              // unmarked: T is copyable

affine trait ForOf { affine type Item; … }  // a trait for its Self, and
                                             // an associated type
```

`affine` is a keyword, as `effect` is, and stands in the same place: before a
type parameter it says what kind of parameter it is.

A declaration with `affine` is move-only. A named `struct` or `variant` must be
declared `affine` when it holds an affine value, directly or through a field or
a payload, or when it implements `Drop`. Otherwise the declaration is an error
that names the field or the impl. A type that does neither may not be declared
`affine`: no use for one has appeared. A tuple and an anonymous struct have no
declaration to mark, and a newtype has the kind of the type it wraps, so each
is inferred: it is affine when something it holds is. `affine type` is the
spelling of an associated type only. A `resource` is affine unless it declares
`linearity = "unrestricted"`, as today.

A generic type is affine or copyable per instantiation. Its `affine`
parameters declare which: `Option<affine T>` is affine where `T` is and
copyable where it is not, so `Option<Fields>` is move-only and `Option<i32>` is
not. A field typed by such a parameter is not "an affine value" for the rule
above, which asks about a field whose type is affine in every instantiation.
`affine struct` on a generic type makes every instantiation move-only, as
`AsyncCall` needs.

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
  by-value `self`, an assignment into a place, a by-value pattern;
- referenced: `&x` or `&mut x`;
- dropped: by scope exit, or by `x.drop()` on a type that implements `Drop`.

It may not be read twice. The move check already states that for a resource;
it now states it for every affine type, `affine` parameters included, at the
definition. The error is any read that would copy:

- a second use of a local path (`x`, `h.f`, `t.0`) after a move;
- a `*r` or an `r.f` read by value through a reference;
- a `list[i]` read by value;
- a read by value of a global, which no check follows across functions.

A by-value pattern consumes its scrutinee: each binding takes its part, and a
part no binding names is dropped as the arm is entered. `match &x`, and the
match ergonomics that bind references out of a `&x` scrutinee, consume nothing.
`matches` reads its scrutinee by reference. `if let`, `while let`, `let-else`
and `?` are matches and follow the same rule.

A field may be moved out of a local holder, as SE-0429 allows: `let f = h.f;`
consumes `h.f`, after which `h` is not usable as a whole, each other field may
still be moved out once, and what `h` still holds drops at scope exit.
`..base` consumes `base`: a field the literal lists
replaces the one `base` carried, which is dropped. A type that implements
`Drop` gives up no field this way, as SE-0429 holds for a type with a `deinit`:
its `drop` relies on the fields it was built with.

The check runs on the `Semantics` layer, so it has spans and reaches the
editor, and it needs no instantiation to run.

### An affine value is an object, and a place is a nullable reference to it

At run time an affine value is a mutable GC object. A struct is its object, as
today. A resource is a box holding its handle. A variant or a tuple that is
affine is wrapped in a box. A place that holds an affine value, whether a
local, a field or an element, is a nullable reference to the object.

Four operations act on places and objects:

- A move stores the reference in the destination and writes null into the
  source.
- A store through a reference, `*r = value`, moves the content of the value's
  object into the object `r` names, after dropping what that object held, and
  zeroes the value's object, as the write-back of `*r = s2` does for a struct
  today. A reference to the value's old object then traps, and `r` keeps
  naming the object it did. Where the value's object is the one `r` names
  (`let r = &mut x; let v = x; *r = v;`), the store drops it: the move into
  `v` nulled the only place that owned it, and a reference owns nothing. The
  next use through `r` traps.
- A drop runs the type's `Drop` and then zeroes the object: a box's handle
  becomes 0, which the Component Model never issues, since index 0 of a handle
  table is reserved; an aggregate's fields become null.
- A use of a null place or a zeroed object traps. For a reference the trap is
  the engine's: a `struct.get` on a null reference traps in Wasm GC, so the
  compiler adds no check.

Scope exit drops every affine local that is not null. A container drops every
slot that is not null. A store into a place drops what the place held. That is
all of cleanup: no path is analysed, since a moved local is null on the path
that moved it. A closure body, a temporary (`make().a`), a generic `Node<T>`
and an element are reached by the same rule, because the rule is about places
and every one of those is a place.

The static rule above and this dynamic one divide the work. Copying is silent,
so only the static rule can stop it. A use after a move or a drop is a trap
wherever the static rule did not already report it, which is everywhere the
value was reached through a reference, an element or a closure.

What follows for references:

- A reference is to the object, as `&s` is to a struct today. It follows the
  object through a move, and it sees the drop: `let r = &f; eat(f); r.has()`
  traps in `has`, because `eat` dropped `f` and zeroed its box.
- A `borrow` an export receives arrives in a box that synthesis zeroes when
  the export returns. A reference that escaped the export traps.
- `&mut` of an affine variant field or element is the variant's box, a real
  place. `opt.take()` on `h.slot` or on `list[i]` writes into it. For a
  copyable variant the rule of
  [Mutable References to Fields and Elements](./spec-memory.md#mutable-references-to-fields-and-elements)
  is unchanged.
- An affine value is `Ref` and `RefMut` in every instantiation where it is
  affine, since it is an object there. `Option<Fields>` is `RefMut` and
  `Option<i32>` is not, and `IndexRefMut`'s `Output: RefMut` needs no change.
- `assert` records the values it prints by reference, and prints a moved one
  as moved.

What follows for the standard library: one builtin moves a value out through a
reference. `builtin::replace(r: &mut T, v: T) -> T` stores `v` where `r` points
and returns what was there, and the move check trusts it. It is what
`ptr::read` and `ptr::write` are to Rust's `mem::replace`, and the one place
`core` is trusted rather than checked. `Option::take` is
`replace(self, None)`, and `Option::replace` is `replace` itself. The storage
builtins of `Array` move for an affine element: `array_get` writes null into
the slot it read, and `array_set` drops what it replaces. So `pop`, `remove`,
`insert` and the cursor `for-of` walks are written as they are for a copyable
element, and the definition-site check holds their bodies to moves.

The optimizer may drop the null store of a move it proves dead, and may
represent a resource by its bare handle where it proves no reference reaches
the box, as it keeps a primitive unboxed.

### No cast between an affine resource and an integer

`as` converts neither way between an affine resource and an integer. A cast
was the one way a raw handle entered or left the type system, and it moved
nothing. `r as i32` left `r` its owner, so a raw handle handed to an import
that takes it left two owners. `h as R` minted an owner each time it ran, so
two casts of one integer left two. With a box, either direction would also
hide an allocation or a read behind syntax that otherwise converts for free. A
cast between an unrestricted handle and an `f64` is unchanged, and a newtype
cast moves, as it converts for free.

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
  used to return `0 as Stream<u8>`. A double that returns a future, as
  `MockStdio::write_via_stream` does, keeps the writable end, and the test ends
  it: a handler runs only until its `resume`, a write inside it would block on
  a reader nobody runs, and an end dropped unwritten traps.

### Drop

What a drop does is the type's `Drop`:

```wado
pub affine trait Drop with () {
    fn drop(self);
}
```

- A type without an impl drops its fields one by one, in declaration order,
  as today. A `List<affine T>` drops its elements, which a `List` holding a
  resource does not do today.
- A type with an impl runs its `drop`. The body may move fields out of `self`,
  and whatever is left at its end is dropped field by field. `self` is never
  dropped as a whole again, which would run `drop` once more.
- A resource's `fn drop(self)` is its `Drop` impl, whether it calls the
  Component Model's `resource.drop` or runs a guest destructor.
- Only an affine type implements `Drop`. A copyable type with one would run it
  for every copy.
- A body generic over `affine T` drops a `T` through `Drop`, resolved at
  monomorphization like any trait call. Every type answers: an impl, a
  field-by-field drop synthesized for an affine type without one, and nothing
  for a copyable type.
- A drop performs no effect. A generic body drops a `T` wherever it discards
  one (`unwrap_or` discards its fallback), and a drop with effects would
  perform them in a function that declares none.
- A drop that must perform an effect no caller can observe names it with
  [`#[benign(E)]`](./spec-attributes.md#benigne-), which any function may
  carry already. `AsyncCall`'s drop is `#[benign(Subtask)]`. A flush is output
  a caller observes, so a writer that buffers ahead of a stream flushes by an
  explicit call, not in its drop.

A type whose consuming methods take fields out of `self`, and which implements
`Drop`, holds those fields in an `Option` and takes them. `drop` then finds
`None` where a method took the field. That is Rust's idiom for the same case.

A `FutureWritable` dropped before its write traps, with a message naming it.
The Component Model traps on `future.drop-writable` before the end is done,
and nothing a drop could write stands in for the value the reader waits for.
A trap there is better than the alternative, a reader that waits forever. So a
`?` or an early `return` between `Future::new()` and the write is a trap
rather than a leak.

### A write returns what it did not deliver

The Component Model leaves with the writer what a write did not deliver. Two
cases do not deliver everything:

- a `stream.write` the reader took only a prefix of, while the stream stays
  open;
- a write of either kind the reader dropped before (`Dropped`).

A cancelled write would be a third, but
[cancellation stays unreachable](./wep-2026-07-25-async-stream-canonical.md#cancellation-stays-unreachable):
the bindings return only once a copy has settled, and `cancel_write` can only
trap.

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
until its copy ends, so the write call reports every case.

A caller that ignores the result discards the rest, and the rule for a
discarded value drops it at the end of the statement. Nothing leaks, and
nothing obliges the caller to look. `StreamWrite` loses its `count`: the
caller knows how many it offered, and `rest.len()` says how many came back. An
affine caller reads the length before the write, since `data` is moved by it.
The optimizer may skip building a `rest` no one reads.

`StreamWritable::write_raw_all(Slice<T>)` is for a copyable element only. The
Component Model lifts the elements a slice views while the writer keeps them,
which leaves two owners.

### `for-of` dispatches to `ForOf`

`for-of` is syntax, and like `+` it dispatches to a trait named for it:

```wado
pub affine trait ForOf with _ {
    affine type Item;
    affine type State;
    fn start(self) -> Self::State;
    fn next(state: &mut Self::State) -> Option<Self::Item>;
}

impl<affine T> ForOf for List<T> { … }  // State = ListCursor<T>, Item = T
impl<I: IntoIterator> ForOf for I { … }  // every other iterable
```

`for let x of e` passes `e` to `ForOf::start` by value, as any argument is
passed, and then calls `ForOf::next` on the state until it returns `None`. The
loop owns what it walks. An affine operand moves into the loop, and leaving the
loop early drops the elements it did not reach. A copyable operand the program
reads after the loop is copied, as an argument is. `List`'s impl names its
head, so it ranks above the blanket for every element type.

Removing that copy is the job of
[value-copy elision](./wep-2026-05-21-resource-ownership.md#value-copy-elision-the-second-client):

| After the loop, the operand is | The body writes it | Elision   | Against today                                  |
| ------------------------------ | ------------------ | --------- | ---------------------------------------------- |
| not read                       | ―                  | move      | no copy, and each element moves into `x`       |
| read                           | no                 | share     | the same: the loop reads the operand's storage |
| read                           | yes                | deep copy | one more copy of the list                      |

Sharing holds because the state never writes the list. A `ListCursor<T>` is the
list and a position: `next` reads the element at the position through
`array_get`, which moves it out for an affine element, and advances. The
cursor's drop drops the list, and the list drops the slots that are not null.

So a loop walks the value its operand held when the loop began. A body that
writes an element the loop has not reached no longer sees the write, and a body
that shrinks the list no longer breaks the contract of its views.

A tuple is not walked through `ForOf`: `for-of` over a tuple stays unrolled at
compile time, one body per element. The operand still passes by value, so over
a tuple holding an affine value the loop consumes the tuple and moves each
element into the binding once.

`Iterator` and `IntoIterator` stay unmarked. Their adapters take `&self` and
copy it, and `find`, `filter`, `min_by_key` and `max_by_key` hand an element to
a closure by value and then keep it. Marking the traits would make every one of
those an error, and the fix, a predicate over `&Item`, rewrites the closures
passed to them and turns `iter_ref()` chains into `&&T`. Swift's `Sequence`
does not accept a noncopyable element either.

An affine element is walked one of two ways:

| Written                             | Element | API               | Why                                                                                                                         |
| ----------------------------------- | ------- | ----------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `for x of list`                     | `T`     | `for-of`          | The loop consumes the list.                                                                                                 |
| `for x of &list`, `list.iter_ref()` | `&T`    | all of `Iterator` | A reference iterator holds only references, so it is copyable whatever its element, and implements the unmarked `Iterator`. |

The second row is where `find` and `filter` reach an affine element: the
iterator's `Item` is `&T` already, so a predicate takes it by value with one
reference, not two.

### Standard library

The rules above decide what the standard library marks. A trait an affine type
implements is an `affine trait`, and so is a trait whose associated type is
affine. A generic type that holds an affine argument marks the parameter, and
so does a method whose own parameter takes one (`Option::map<U>`). A method of
`List<T>` that only moves elements, or touches none (`len`, `push`, `pop`,
`insert`, `remove`), moves from `impl<T> List<T>` to `impl<affine T> List<T>`,
which reaches every element type; one that reads an element by value stays,
for a copyable element only. Two inherent blocks may not both declare one
name for a receiver they share
([Where the order applies](./wep-2026-09-01-trait-resolution.md#where-the-order-applies)),
so a method lives in one of the two. The instantiation rule names every generic the corpus
instantiates with an affine argument, and each is marked or rewritten.

What the rules leave unmarked, and why:

- `Iterator` and `IntoIterator`, above.
- `Sequence`, `AsSlice` and `IndexValue`: `at`, `get`, `first`, `last` and
  `index_value` hand an element out by value. `is_empty` and `iter_ref`, which
  `Sequence` and `AsSlice` provide as defaults today, become inherent methods
  of `List` in `impl<affine T> List<T>`. The body rule reports a by-value
  `list[i]` on an affine element before resolution looks for `IndexValue`, so
  the diagnostic names the copy, not a missing impl.
- `TreeMap<K, V>`, `HashMap<K, V>` and `TreeSet<T>`: a known gap below.

What changes shape:

- `Option<affine T>` gains `take` and `replace`, over `builtin::replace`.
- `AsyncCall<affine T>` becomes an `affine struct` holding a `Subtask` rather
  than a raw `i32` it casts into one at each use:

  ```wado
  affine struct AsyncCall<affine T> {
      subtask: Option<Subtask>,       // None where the call completed synchronously
      buffer: Option<ResultBuffer>,   // the linear memory the result is lifted from
      lift: fn(i32) -> T,
  }
  ```

  `wait(self)` and `cancel(self)` take the subtask and the buffer and consume
  them, so a second call is a compile error and the shared `__cm_ended` flag
  goes. Its `Drop` leaves any set the subtask was joined to, as `cancel` does
  today, and cancels it, since a `Subtask` traps on `drop` until it resolves;
  then it lifts and drops a result the callee returned, which owns handles
  only a lift reaches, and frees the buffer. A call a set already reported
  cannot be ended, today or after this WEP, and its drop traps: that is
  [a gap of the `AsyncCall` WEP](./wep-2026-04-22-subtask-generic.md#known-gaps).
- `ResourceId`, with `ResourceId::of(&r)` over every affine resource.
- `wado-from-idl` declares `affine` every record and variant it generates that
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
- The move check stays, as the diagnostic. Cleanup synthesis retires: the
  scope-exit drop of what is not null replaces its path analysis.
- Value-copy elision is unaffected: it serves copyable types, and an affine
  type never reaches a copy site.
- Its known gaps on resources close: "A call through `&mut` that copies", "A
  generic body copies a type parameter", "Some resource holders are not
  move-checked or dropped", "A resource field moves out of its holder", "An
  integer casts to an affine resource", "A `Stream` or `Future` handle nothing
  drops" and "A generic resource a user module declares". "What cleanup does
  not reach" closes but for the panic. The rest of that section stands, and so
  do its gaps on value-copy elision.

In [WEP: Reference Representation](./wep-2026-06-13-reference-representation.md),
D6 is settled: a resource is boxed, and the box is its object.

In [Memory Model](./spec-memory.md#mutable-references-to-fields-and-elements),
the unsettled rule for `&mut` of a variant field or element is settled for an
affine variant: the reference is a place.

In [WEP: Resource Lifecycle](./wep-2026-01-12-resource-lifecycle.md), the open
question of a user-defined drop for a type that is not a resource is answered:
an affine type implements `Drop`. Its rule that a function owning a resource
declares the destructor's effects is withdrawn: a drop performs none, or only
benign ones.

## Roadmap

- [ ] Fix the `*r = value` panic through `&mut R` with a fixture. A compiler
  panic is P0 and does not wait for the rest.
- [ ] `affine` as a keyword on `struct` and `variant`. A named declaration
  holding an affine value or implementing `Drop` without it is an error naming
  the field or the impl, and one doing neither with it is an error too; a
  tuple, a newtype and an anonymous struct infer. `wado-from-idl` emits it, and
  the generated stdlib is regenerated. `resource_*` fixtures declare their
  holders.
- [ ] `affine` on a type parameter, an associated type and a trait, with the
  definition-site check: the move check runs over its bindings as over
  resource bindings, over patterns, over globals, and over a marked trait's
  default bodies with `Self` affine. An affine type implementing an unmarked
  trait is an error at the impl. The prelude traits an affine type implements
  are marked.
- [ ] No cast between an affine resource and an integer: synthesis splits and
  lifts the `stream-new` / `future-new` pair, the raw-handle builtins take and
  return resources, `ResourceId::of`, and the test doubles in `core:kiln`,
  `core:cli` and `package-gale` return real handles. This comes before the
  next item, whose scope-exit drop would otherwise drop a second time the
  `Stream` that `log_stdout` hands over through a cast today.
- [ ] The object and its places: an affine variant or tuple is boxed, a move
  writes null into its source, a drop zeroes the object, scope exit drops what
  is not null, a container drops the slots that are not null, and a store
  drops what it replaces. The cleanup pass retires, and with it its exclusion
  of a `Stream` and a `Future` the body does not move
  ([A `Stream` or `Future` handle nothing drops](./wep-2026-05-21-resource-ownership.md#a-stream-or-future-handle-nothing-drops)).
  An export's `borrow` is zeroed at return. Fixtures: a reference used after
  the drop traps; a resource bound in a closure body, one under `make().a`,
  one in a `Node<T>` and an unmoved `Stream` are dropped; `*r = value` through
  `&mut R`.
- [ ] `builtin::replace`, `Option::take` and `Option::replace`, the storage
  builtins moving for an affine element, the methods of `List` that read no
  element by value moved into `impl<affine T> List<T>` with `is_empty` and
  `iter_ref` beside them, and `&mut` of an affine variant element and field as
  a place. `opt.take()` on
  `list[i]` is the fixture.
- [ ] `Drop`: a resource's `fn drop(self)` as its impl, a user impl on an
  affine type, the fields left at the end of `drop` dropped one by one, a drop
  that performs no effect but a `#[benign]` one, no partial move or `..base`
  over a type that implements it, a generic `T` dropped through `Drop`, and a
  `FutureWritable` dropped before its write trapping with a message naming
  it. A fixture pins the `?` between `Future::new()` and the write.
- [ ] `ForOf`: `for-of` passes its operand by value to `ForOf::start`, with
  `List`'s impl over `ListCursor<T>` and the blanket impl over `IntoIterator`.
  A fixture pins that elision shares a list the loop's body does not write: no
  `$value_copy$` in the output. The fixtures that relied on a body's write
  being seen by the loop are rewritten.
- [ ] `AsyncCall` holds an `Option<Subtask>` and an `Option<ResultBuffer>`,
  with `wait(self)`, `cancel(self)` and its `Drop`. The fixture drops a call
  nobody awaited.
- [ ] `FutureWrite<T>` and `StreamWrite<T>`: a write returns what it did not
  deliver, in every case the result reports, and the stdlib's write loops
  (`write_all`) read `rest` rather than `count`.
- [ ] Instantiation rule: an affine argument for an unmarked parameter is a
  type error naming the parameter. It names every generic the corpus
  instantiates with an affine argument, and each is marked or rewritten.
- [ ] Withdraw the receiver-kind rule and the TIR copy check from #2364. The
  `resource_copy_out_of_reference_error` fixture stays, its expectations
  rewritten to the rules that now catch each case: `list[0]` on a
  `List<Fields>` is a copy through a reference under the body rule, a generic
  `&self` getter instantiated with `Fields` is an affine argument for an
  unmarked parameter, and `list.last()` is not found, since `Sequence`, which
  declares it, is implemented for a copyable element only.
- [ ] `assert` records the values it prints by reference, and prints a moved
  one as moved.
- [ ] Spec: rewrite [Resource Ownership](./spec-components.md#resource-ownership),
  the resource paragraph and the `&mut`-to-a-field rule of
  [Memory Model](./spec-memory.md),
  [For-Of Loop](./spec-control-flow.md#for-of-loop) over `ForOf` and the
  value the loop began with, and the cheatsheet. Amend the WEPs above.

## Known gaps

### Maps and sets of affine values

`TreeMap<K, V>`, `HashMap<K, V>` and `TreeSet<T>` with an affine element are
type errors. A program keyed on affine values keeps them in a `List` and the
keys beside it.

### No adapter takes an affine element by value

`map`, `filter`, `collect` and the rest are `Iterator`'s, which an affine
element does not reach by value. A program that transforms the elements it
consumes writes the loop.

### A closure cannot own an affine value

A closure captures by reference, so it may use an affine value but never owns
one. A closure that outlives the value's owner traps on its first use after the
drop.

### A panic drops nothing

A trap unwinds nothing in Wasm, so a resource owned at a panic is held until
the instance ends. For a stream, the other side hangs.

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
- [WEP: Generic `AsyncCall<T>` for CM async imports](./wep-2026-04-22-subtask-generic.md)
- [WEP: Stream Copy Results](./wep-2026-08-30-stream-copy-result.md)
- #2367
