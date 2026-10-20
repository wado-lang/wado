# WEP: Affine Types — Swift's Noncopyable Discipline for Resources

## Context

### The guarantee

An affine resource is a handle the host owns: a CM `own`, a `stream`, a
`future`. The program must keep one invariant for each: at every point it has
exactly one owner, that owner drops it once or hands it to the next owner, and
no use reaches it after the drop.

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
| A run-time check on every use             | Makes a use after drop a certain trap, but makes no drop happen, and costs a check on every use.                   |

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
- a captured binding read by value in a closure body, since a closure captures
  by reference: `|| eat(f)` is a copy through a reference;
- a `list[i]` read by value;
- a read by value of a global, which no check follows across functions.

A global may hold an affine value. It is taken out with `builtin::replace`
through `&mut`, and nothing drops what it still holds when the instance ends.

A by-value pattern consumes its scrutinee: each binding takes its part, and a
part no binding names is dropped as the arm is entered. A guard sees the
bindings by reference, since the arm is entered only once the guard holds. A
refutable pattern that does not match consumes nothing, so the `else` of an
`if let` may still use the scrutinee. `match &x`, and the match ergonomics that
bind references out of a `&x` scrutinee, consume nothing. `matches` reads its
scrutinee by reference. `if let`, `while let`, `let-else` and `?` are matches
and follow the same rule.

A field may be moved out of a local holder, as SE-0429 allows: `let f = h.f;`
consumes `h.f`, after which `h` is not usable as a whole, each other field may
still be moved out once, and what `h` still holds drops at scope exit.
`..base` consumes `base`: a field the literal lists
replaces the one `base` carried, which is dropped. A type that implements
`Drop` gives up no field this way, as SE-0429 holds for a type with a `deinit`:
its `drop` relies on the fields it was built with.

### A reference is read only while its local holds the value

A reference into an affine local, and a closure capturing one, are read only
while the local holds what they read. The move check follows them within a
body, with no lifetime in any signature. A reference or a closure carries the
local it names, a value built from one carries what that one carries, and a
call's result carries what every operand carries, since no lifetime says which
one it returns.

The declared types say it in one case. A borrow handed to a parameter declared
a reference stays out of the result where the declared return type holds no
reference that could point into the borrowed type. A body generic over the rest
cannot make one, so such a result holds only what the borrowed place already
held. This is how `set.wait()` hands back a member that outlives the set, and
how `router.match_request(&request)` leaves `request` free to move.

A call may store any operand through each `&mut` it takes, so what that `&mut`
reaches carries them all. The same declared-type test exempts a borrow from a
`&mut Q` parameter whose `Q` holds no reference that could point into it, which
is how `array_move(&mut dst, &src, …)` leaves `src` free to move. The check refuses a read after the
local moved or its scope ended, a `return` of such a value, a `&mut` parameter
or a capture left holding one at the end of the body, and a store into a global.

A `&mut` use of the local other than through the reference, such as `pop` or
`take`, may move a part of it out, so it ends what references taken before it
read, as Rust's exclusive borrow does. A use through the reference itself does
not.

This refuses programs a lifetime system would accept: a call handed `&t` and
`&mut v`, where `v`'s type could hold a reference into `t`'s, is taken to store
`&t` into `v`, whether it does or not. Refusing more
was preferred to behaviour left unspecified at run time.

The check runs on the `Semantics` layer, so it has spans and reaches the
editor, and it needs no instantiation to run. It runs over the standard library
too, whose generic bodies this design relies on it to hold to moves; today's
move check skips every module the user did not write.

### An affine value is an object, and a place is a nullable reference to it

At run time an affine value is a mutable GC object. A struct or a tuple is its
object, as a struct is today. A resource is a box holding its handle. A variant
is immutable, so a field, a tuple element, a payload or an element holding an
affine variant holds it in a box, which is the slot's object. A place that holds
an affine value, whether a local, a field or an element, is a nullable reference
to the object.

Five operations act on places and objects:

- A move stores the reference in the destination and writes null into the
  source.
- A store through a reference, `*r = value`, moves the content of the value's
  object into the object `r` names, after dropping what that object held, and
  zeroes the value's object, as the write-back of `*r = s2` does for a struct
  today. `r` keeps naming the object it did. Where the value's object is the
  one `r` names, the store traps with a message naming the resource: the move
  out of the place nulled the only place that owned the object, a reference
  owns nothing, so no place could own it after the store. The static rule
  refuses `let r = &mut x; let v = x; *r = v;`, so only a reference the check
  does not follow reaches the trap.
- An assignment `x = value` to a place that holds an object is `*&mut x = value`, so every reference to the place sees the new value, as
  [WEP: Reference Liveness](./wep-2026-10-13-referenced-places.md) has an
  assignment to a referenced place do for a copyable value. To a null place,
  one a move emptied, it stores the reference.
- A drop runs the type's `Drop` and then zeroes the object: a box's handle
  becomes 0, which the Component Model never issues, since index 0 of a handle
  table is reserved; an aggregate's affine fields become null.
- Lowering a handle to the host as an `own` (an argument, a result, a
  `task return`, an element a stream or future write carries) zeroes its box,
  as a drop does. The host owns the handle from then on, and a box still
  holding it would reach the slot the host reuses.

What a use of a zeroed object does is unspecified: it may trap, or read what
the object held before. A field that cannot be null, such as an `i32`, keeps
its value, and a box holding 0 traps only where the host checks the index. The
compiler adds no check. Neither a local nor a value referring into it is read
after the local moved or dropped, which the static rule reports.

Scope exit drops every affine local that is not null, in reverse declaration
order, and a temporary at the end of its statement. A container drops every
slot that is not null. A store into a null place drops nothing. That is all of
cleanup: no path is analysed, since a moved local is null on the path that
moved it. A closure body, a temporary (`make().a`), a generic `Node<T>` and an
element are reached by the same rule, because the rule is about places and
every one of those is a place.

The static rule above and this dynamic one divide the work. Copying is silent,
so only the static rule can stop it. A use after a move or a drop that the
static rule does not report reaches a zeroed object: one through a reference a
callee kept where no body follows it.

What follows for references:

- A reference is to the object, as `&s` is to a struct today. It follows the
  object through a move, which is why the static rule refuses a read after one:
  in `let r = &f; eat(f); r.has()`, `has` would reach the box `eat` zeroed.
- A `borrow` an export receives arrives in a box that synthesis zeroes when
  the export returns. A reference that escaped the export reaches a zeroed
  box.
- `&mut` of a field, a tuple element, a payload or an element holding an affine
  variant is the slot's box, a real place. `opt.take()` on `h.slot` or on
  `list[i]` writes into it. For a copyable variant the rule of
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
`ptr::read` and `ptr::write` are to Rust's `mem::replace`. `Option::take` is
`replace(self, None)`, and `Option::replace` is `replace` itself. The storage
builtins of `Array` move for an affine element: `array_take` writes null into
the slot it read, `array_set` drops what it replaces, `array_move` writes null
into the slots it leaves, and `array_release` drops what it releases.
`array_get_value` stays a read, since `&self` methods such as `Inspect`'s reach
it, and moving there would empty the container being looked at. These builtins
are the places `core` is trusted rather than checked. So `pop`, `remove`,
`insert`, `swap` and the cursor `for-of` walks are written once for both kinds
of element, reading with `array_take`, and the definition-site check holds
their bodies to moves. A generic body is elaborated once per instance, after
monomorphization, so what it owns drops the way its instance's type drops.

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
  dropped one's identity may come back for a later one, since the host reuses
  a freed handle index.
- A test double returns a real handle, from `Stream::<u8>::new()`, where it
  used to return `0 as Stream<u8>`. A handler that traps on the operations it
  leaves out (`..trap`) traps on `new` too, so the test makes the handle
  outside its `with`. A double that told inputs apart by the handle's number
  (`package-gale`'s `main_test.wado`) keys them by `ResourceId` instead. A
  double that returns a future, as
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
  as today. A `List<affine T>` and an `Array<affine T>` drop their elements,
  which a `List` holding a resource does not do today.
- `builtin::drop<affine T>(value: T)` drops any value where it is called. It
  takes `value` and ends, so the scope-exit drop of the parameter does the
  work. `x.drop()` names a type's `Drop` impl, so a type without one, and an
  `affine T` in a generic body, is dropped by name this way.
- A type with an impl runs its `drop`. The body may move fields out of `self`,
  and whatever is left at its end is dropped field by field. `self` is never
  dropped as a whole again, which would run `drop` once more.
- A resource's `fn drop(self)` is its `Drop` impl, which calls the Component
  Model's `resource.drop`.
- Only an affine type implements `Drop`. A copyable type with one would run it
  for every copy.
- A body generic over `affine T` drops a `T` the way the type drops, resolved
  at monomorphization like any trait call: its `Drop` impl, a field-by-field
  drop synthesized for an affine type without one, and nothing for a copyable
  type.
- A drop at scope exit, at a store, or of a discarded value is the same call as
  a written `x.drop()`. A resource's drop is an operation of the resource's
  effect, so either one reaches an installed handler, and a test double that
  counts drops sees both. A double ending in `..trap` therefore answers `drop`
  itself, taking the handle by value and handing it to the host's drop, since
  `..trap` traps on every operation it leaves out.
- A drop's effects are benign: no function declares an effect for a drop it
  performs. A generic body drops a `T` wherever it discards one (`unwrap_or`
  discards its fallback), and a declared effect would reach a function that
  declares none. A resource's drop is benign in its own effect, and a type
  whose `drop` performs another names it with
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

A future's ends are each used once. The Component Model traps on a second read
of a readable end and a second write of a writable end, and leaves each to be
dropped after it. So both consume their end:

```wado
fn read(self) -> Option<T>;                   // Future<affine T>
fn write(self, value: T) -> FutureWrite<T>;   // FutureWritable<affine T>
```

A second read or write is a use after move, and the end drops when the call
returns.

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
fn write(self, value: T) -> FutureWrite<T>;        // FutureWritable<affine T>
fn write(&self, data: List<T>) -> StreamWrite<T>;  // StreamWritable<affine T>

struct FutureWrite<affine T> { rest: Option<T>, result: FutureWriteResult }
struct StreamWrite<affine T> { rest: List<T>, result: CopyResult }
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
}

impl<affine T> ForOf for List<T> { … }  // State = ListCursor<T>, Item = T
```

`for let x of e` over an affine `e` passes `e` to `ForOf::start` by value, as
any argument is passed, and then calls the state's
`next(&mut self) -> Option<Item>` until it returns `None`. The loop owns what it
walks: the operand moves into the loop, and leaving the loop early drops the
elements it did not reach. A `ListCursor<T>` is the list and a position: `next`
moves the element at the position out through `array_take` and advances. The
cursor's drop drops the list, and the list drops the slots that are not null.

`next` is the state's own method rather than a member of the trait, so that
`ListCursor<T>` need not implement `Iterator`, whose `Item` is copyable, with
an affine `Item`. `Array<T>` walks through a `ListCursor<T>` as well, and an
affine iterator through `impl<affine I: Iterator> ForOf for I`, as its own
state. No blanket impl over `IntoIterator` exists. It would only forward to
`into_iter`, and resolving through it leaves the item type of a generic
iterator as an unnormalized projection.

A copyable operand, or one whose type does not implement `ForOf`, walks its
`IntoIterator` as before. Moving a copyable list into the loop costs a copy of
any list the program reads after the loop:
[value-copy elision](./wep-2026-05-21-resource-ownership.md#value-copy-elision-the-second-client)
does not share a parameter the callee stores into the value it returns, here
the cursor. So a loop over a copyable list still sees a write its body makes to
an element it has not reached.

A tuple is not walked through `ForOf`: `for-of` over a tuple stays unrolled at
compile time, one body per element. The operand still passes by value, so over
a tuple holding an affine value the loop consumes the tuple and moves each
element into the binding once.

`Iterator` and `IntoIterator` are affine traits, so an affine type may
implement them. An adapter (`map`, `filter`, `zip` and the rest) takes `self`
and moves it into the adapter, whose parameter is `affine I`. Their `Item`
stays unmarked: `find`, `filter`, `min_by_key` and `max_by_key` hand an element
to a closure by value and then keep it. Marking `Item` would make every one of
those an error, and the fix, a predicate over `&Item`, rewrites the closures
passed to them and turns `iter_ref()` chains into `&&T`. Swift's `Sequence`
does not accept a noncopyable element either.

An affine element is walked one of two ways:

| Written                             | Element  | API               | Why                                                                                               |
| ----------------------------------- | -------- | ----------------- | ------------------------------------------------------------------------------------------------- |
| `for x of list`                     | `T`      | `for-of`          | The loop consumes the list.                                                                       |
| `for x of &list`, `list.iter_ref()` | `&T`     | all of `Iterator` | A reference iterator's `Item` is a reference, which is copyable whatever its element.             |
| `for x of &mut list`                | `&mut T` | all of `Iterator` | The same, for a body that changes an element in place, such as `x.take()` on an `Option` element. |

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

- The `Item` of `Iterator` and `IntoIterator`, above.
- `Sequence`, `AsSlice` and `IndexValue`: `at`, `get`, `first`, `last` and
  `index_value` hand an element out by value. `is_empty` and `iter_ref`, which
  `Sequence` and `AsSlice` provide as defaults today, become inherent methods
  of `List` in `impl<affine T> List<T>`. The body rule reports a by-value
  `list[i]` on an affine element before resolution looks for `IndexValue`, so
  the diagnostic names the copy, not a missing impl.
- `TreeMap<K, V>`, `HashMap<K, V>` and `TreeSet<T>`: a known gap below.
- `Stream::read_to_end`, which extends its result from `&chunk.items`, a copy
  through a reference. `StreamChunk<affine T>` carries an affine element, and
  a reader of one drains the chunks itself.
- `StructField::get`, which hands a field out of `&T` by value. `Inspect` works
  for every type and `assert` prints through it, so its derivation walks an
  affine struct's fields through a by-reference accessor instead.

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
  goes. `join` goes too: a set owns the call it waits on (below). Its `Drop`
  leaves any set the subtask was joined to, as `cancel` does
  today, and cancels it, since a `Subtask` traps on `drop` until it resolves;
  then it lifts and drops a result the callee returned, which owns handles
  only a lift reaches, and frees the buffer. A call a set already reported
  skips the cancel, which would trap: the set recorded the resolution it
  delivered
  ([WEP: Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md#decision)).
- `ResourceId`, with `ResourceId::of<R: AffineResource>(&r)`. `AffineResource`
  is a sealed marker the compiler states for every affine resource, so the
  bound is what makes anything else a type error. It names the resource
  rather than the affinity: an `affine struct` holds no handle and does not
  implement it.
- `wado-from-idl` declares `affine` every record and variant it generates that
  holds an affine resource.

### A waitable set owns what it waits on

A waitable set holds the members joined to it, and a wait hands back the member
that is ready:

```wado
pub affine trait Waitable {                       // sealed
    fn waitable_id(&self) -> Option<ResourceId>;  // None: ready without the host
}

pub affine struct WaitableSet<affine W: Waitable> { … }

impl<affine W: Waitable> WaitableSet<W> {
    pub fn new() -> WaitableSet<W>;
    pub fn join(&mut self, member: W);
    pub fn wait(&mut self) -> Ready<W>;
    pub fn poll(&mut self) -> Option<Ready<W>>;
}

pub struct Ready<affine W> { pub member: W, pub event: ReadyEvent }

pub variant ReadyEvent {
    Unreported,                  // no handle for the host to report
    Call(CallState),
}
```

`join` moves the member into the set. `wait` blocks until a member has an event,
moves it out, and returns it with the event, decoded into a `ReadyEvent`
variant. A raw code and payload would leave every caller to decode the
Component Model's numbering, and a caller that read a stream's count as a
call's state would drop a call still in flight. `Subtask` and `AsyncCall`
implement `Waitable`. An `AsyncCall` that completed synchronously has no
subtask, so its `waitable_id` is `None`, and `wait` hands it back first without
asking the host.

A stream or future end does not implement it. Each copy on one waits inside the
binding (`cm_await_blocked`), so no copy is ever left pending for the host to
report, and a set of ends would wait forever. Nor does `ReadyEvent` carry a copy
event, since no member can produce one.

The host reports an event by handle index only, so something in the guest has
to map the index back to the value. The set is that table, and the ownership
follows the Component Model's own rules:

- A waitable joins one set at a time, which is a move.
- A member dropped while joined leaves its set, which is the drop of a value
  the set owns.
- The Component Model traps on dropping a set that still has members. The set's
  `Drop` drops its members first, so that cannot happen.

`Waitable = i32`, `WaitEvent.handle`, `Subtask::join` and `AsyncCall::join` go.
An event loop no longer compares an integer with the resource it meant. A
member returns moved out. A set has one member type. A loop that waits on several kinds
declares a variant of its own and derives `Waitable` on it. The resource behind
the set becomes internal to `core`.

`Waitable` is sealed. The set trusts `waitable_id`: it joins the handle named
there, and treats the member as ready without the host where there is none. A
hand-written impl could name a handle another value owns, so that value's
events would arrive for the wrong member, or one that owns a handle could say
it has none. The standard library implements the trait on the values that own
a waitable handle, and nothing else may. A loop's variant derives it with the
marker `impl Waitable for V;`, which the compiler writes out case by case: a
case with a payload asks the payload, and one without is ready without the
host. `Subtask` stays public, since a function waiting on a call it was neither
handed nor started names it as an effect, but no value of it can be had
outside `core`.

Since that resource is internal, no caller can install a handler for it, and a
caller asked for it as an effect could only pass it on. The set's methods
therefore declare its operations `#[benign(RawWaitableSet)]`, the way a drop's
effects are benign, and `AsyncCall::wait` performs only `Subtask`.

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

In [WEP: A Function Without `with` Performs No Effects](./wep-2026-09-30-effect-free-functions.md),
"Waiting where the call starts" widens. An `AsyncCall` holds its `Subtask`,
and a resource grants its effects to whatever holds it, so a function handed a
call through its signature waits on it without declaring them, as one starting
it does.

In [WEP: Reference Representation](./wep-2026-06-13-reference-representation.md),
D6 is settled: a resource is boxed, and the box is its object.

In [Memory Model](./spec-memory.md#mutable-references-to-fields-and-elements),
the unsettled rule for `&mut` of a variant field or element is settled for an
affine variant: the reference is a place. Its
[Reference Identity](./spec-memory.md#reference-identity) gains the affine
case: a reference is to the object and follows it through a move, and an
assignment to a place that holds one writes into it.

In [WEP: Resource Lifecycle](./wep-2026-01-12-resource-lifecycle.md), the open
question of a user-defined drop for a type that is not a resource is answered:
an affine type implements `Drop`. Its rule that a function owning a resource
declares the destructor's effects is withdrawn: a drop's effects are benign.

## Roadmap

- [x] Fix the `*r = value` panic through `&mut R` with a fixture. A compiler
  panic is P0 and does not wait for the rest.
- [x] `affine` as a keyword on `struct` and `variant`. A named declaration
  holding an affine value without it is an error naming the field, and one
  holding none with it is an error too; a
  tuple, a newtype and an anonymous struct infer. `wado-from-idl` emits it, and
  the generated stdlib is regenerated. `resource_*` fixtures declare their
  holders.
- [x] `affine` on a type parameter, an associated type and a trait, with the
  definition-site check: the move check runs over its bindings as over
  resource bindings, over patterns (a guard by reference, a failed match
  consuming nothing), over globals, over closure bodies, over the standard
  library, and over a marked trait's default bodies with `Self` affine. An
  affine type implementing an unmarked trait is an error at the impl. The
  prelude traits an affine type implements are marked, and `Inspect`'s
  derivation walks an affine struct by reference. A trait's method may take
  `self` by value, so `Iterator`'s adapters take `self` rather than copying
  `*self`.
- [x] No cast between an affine resource and an integer: synthesis splits and
  lifts the `stream-new` / `future-new` pair, the raw-handle builtins take and
  return resources, `ResourceId::of`, and the test doubles in `core:kiln`,
  `core:cli` and `package-gale` return real handles, made outside a `..trap`
  handler's `with`. This comes before the
  next item, whose scope-exit drop would otherwise drop a second time the
  `Stream` that `log_stdout` hands over through a cast today.
- [x] The object and its places: a move writes null into its source, a drop
  zeroes the object, an `own` lowered to the host zeroes its box, scope exit
  drops what is not null in reverse declaration order, a container drops the
  slots that are not null, and an assignment to a place holding an object
  writes into it. The cleanup pass retires, and with it its exclusion of a
  `Stream` and a `Future` the body does not move
  ([A `Stream` or `Future` handle nothing drops](./wep-2026-05-21-resource-ownership.md#a-stream-or-future-handle-nothing-drops)).
  No export receives a `borrow` yet, since `&R` has no value representation in
  an export's signature, so none is zeroed at return. An affine variant's box
  is the next item's, since only `&mut` of its field or element needs one: a
  local or a field holding it is already a nullable reference to a GC object.
  Fixtures: a resource bound in a closure body, one under `make().a`, one in a
  `Node<T>` and an unmoved `Stream` are dropped; `*r = value` through `&mut R`;
  `x = value` seen through a reference taken before it.
- [x] `builtin::replace`, `Option::take` and `Option::replace`, the storage
  builtins moving for an affine element, the methods of `List` that read no
  element by value moved into `impl<affine T> List<T>` with `is_empty` and
  `iter_ref` beside them, and `&mut` of a slot holding an affine variant as a
  place, the slot holding it in a box. Fixtures: `opt.take()` on `list[i]`,
  on a field, on a payload bound by `&mut` and on a tuple element.
- [x] `Drop`: a resource's `fn drop(self)` as its impl, a user impl on an
  affine type and an error naming the impl on any other, the fields left at the end of `drop` dropped one by one, a
  drop's effects benign, no partial move or `..base` over a type that
  implements it, a generic `T` dropped the way its type drops,
  `builtin::drop`, and a
  `FutureWritable` dropped before its write trapping with a message naming
  it. A fixture pins the `?` between `Future::new()` and the write, and one
  pins that a handler counting a resource's drops sees a scope-exit drop.
- [x] `Future::read(self)` and `FutureWritable::write(self, value)`. A fixture
  pins a second read as a use after move.
- [x] `ForOf`: `for-of` passes an affine operand by value to `ForOf::start`,
  with `List`'s and `Array`'s impls over `ListCursor<T>` and an affine
  iterator as its own state, and walks any other through `IntoIterator`. The
  move check consumes the operand and refuses `*r`. Fixtures pin a list
  consumed with an early `break`, an affine iterator through adapters, the use
  after the move, `*r`, and a copyable list read after the loop with no
  `$value_copy$` in the output.
- [x] `AsyncCall` holds an `Option<Subtask>` and an `Option<ResultBuffer>`,
  with `wait(self)`, `cancel(self)` and its `Drop`. The fixture drops a call
  nobody awaited.
- [x] `WaitableSet<W>` owns its members: `Waitable`, `join`, `wait` and `poll`
  returning `Ready<W>`, and its `Drop` dropping the members first.
  `Waitable = i32`, `WaitEvent.handle` and the `join` methods go. Fixtures: a
  set waiting on two calls through a variant, a set dropped with members, and
  a call a set delivered, then waited, cancelled, dropped and joined again.
- [x] `FutureWrite<T>` and `StreamWrite<T>`: a write returns what it did not
  deliver, in every case the result reports, and the stdlib's write loops
  (`write_all`) read `rest` rather than `count`.
- [x] Instantiation rule: an affine argument for an unmarked parameter is a
  type error naming the parameter. It names every generic the corpus
  instantiates with an affine argument, and each is marked or rewritten.
- [x] Withdraw the receiver-kind rule and the TIR copy check from #2364. The
  `resource_copy_out_of_reference_error` fixture stays, its expectations
  rewritten to the rules that now catch each case: `list[0]` on a
  `List<Fields>` is a copy through a reference under the body rule, a generic
  `&self` getter instantiated with `Fields` is an affine argument for an
  unmarked parameter, and `list.last()` is not found, since `Sequence`, which
  declares it, is implemented for a copyable element only.
- [x] `assert` records the values it prints by reference, and prints a moved
  one as moved.
- [x] A reference read only while its local holds the value: the move check
  follows what each value refers into, and refuses a read after a move, a scope
  end or another `&mut` use, a returned reference into a local, a `&mut`
  parameter or a capture left holding one, and a store into a global. The
  `*r = value` fixture whose store trapped is refused at compile time.
- [x] `Waitable` is sealed, and `impl Waitable for V;` derives it on a variant.
  Fixtures pin a hand-written impl as an error and a set over derived variants.
- [x] `Ready<W>` carries a `ReadyEvent` rather than a raw code and payload.
- [x] A future write reports a `FutureWriteResult`, `Pending` where the reader
  has not read yet, rather than claiming `Completed`.
- [x] Spec: rewrite [Resource Ownership](./spec-components.md#resource-ownership),
  the resource paragraph, Reference Identity and the `&mut`-to-a-field rule
  of [Memory Model](./spec-memory.md), the drop of a resource in
  [Resources as Effects](./spec-effects.md#resources-as-effects),
  [For-Of Loop](./spec-control-flow.md#for-of-loop) over `ForOf` and the
  value the loop began with, and the cheatsheet. Amend the WEPs above.

## Known gaps

### A loop over a copyable list shares it

`for-of` moves only an affine operand into the loop. Over a copyable list it
walks `IntoIterator`, whose iterator reads the list's storage, so a body's
write to an element the loop has not reached is seen, and a body that shrinks
the list breaks the contract of its views. Moving every operand would make the
loop walk the value it began with, at the cost of a copy of each list read
after the loop, since value-copy elision does not share a parameter stored into
a returned value.

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
one, and it is refused where it would outlive the value's owner.

### A callee may keep a reference no body follows

The check follows a reference within the body holding it. A callee handed one
may store it in a global, or hand a closure to the Component Model, which
`cm_callback_key` keeps for the life of the instance. Neither is visible at the
call, so a use after the owner drops reaches a zeroed object, and what that does
is unspecified.

### A scalar future write does not wait for its reader

A write blocks until its copy ends only for the HTTP trailers and transmission
results. Every other `FutureWritable` write returns once the copy starts, since
a reader in the same instance would deadlock a writer waiting on it. One the
reader has not read yet reports `Pending` with no `rest`: the value is the
host's to deliver, and nothing says whether the reader takes it or drops first.
An end whose copy is still pending cannot be dropped, so the write leaves it to
the host, and the handle lives until the instance ends.

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
