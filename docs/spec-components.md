# Components

A Wado program compiles to a Component Model component. This chapter says how
Wado values cross a component boundary, how a component waits, and how Wado
declarations link to Component Model definitions and resources. Which world a
component targets, and what its entry point is, is in
[Worlds and Entry Points](./spec-worlds.md).

## Type Mapping at Component Boundaries

A Wado value that crosses a component boundary is lowered to a Component Model value, and lifted back on the other side, by the Canonical ABI.

The table below is the Wado↔CM correspondence, read in both directions: Wado→CM when generating a component's exported interface, and CM→Wado when importing an external component (`use { Iface } from "./c.wasm" with { type: "wasm" }`, see [Wasm Module and Component Imports](./spec-modules.md#wasm-module-and-component-imports)). CM types are written in their WIT spelling.

| Wado Type                 | CM Type at Boundary       | Notes                                                                                      |
| ------------------------- | ------------------------- | ------------------------------------------------------------------------------------------ |
| `bool`                    | `bool`                    | Boolean value                                                                              |
| `char`                    | `char`                    | Unicode scalar value                                                                       |
| `i8`, `i16`, `i32`, `i64` | `s8`, `s16`, `s32`, `s64` | Signed integers                                                                            |
| `u8`, `u16`, `u32`, `u64` | `u8`, `u16`, `u32`, `u64` | Unsigned integers                                                                          |
| `i128`, `u128`            | `record { low, high }`    | Prelude structs, so each crosses as its own record                                         |
| `f32`, `f64`              | `f32`, `f64`              | Floating point                                                                             |
| `String`                  | `string`                  | UTF-8 string                                                                               |
| `List<T>`                 | `list<T>`                 | Dynamic array; a WASI binding names `list<u8>` as `ByteList`                               |
| `TreeMap<K, V>`           | `map<K, V>`               | `K` is `bool`, `char`, `String`, or an integer; a repeated key takes the last pair's value |
| `[T1, T2, ...]`           | `tuple<T1, T2, ...>`      | Tuple types                                                                                |
| `Option<T>`               | `option<T>`               | Optional value                                                                             |
| `Result<T, E>`            | `result<T, E>`            | Result type; `result<ok>` and bare `result` are the payload-elided forms                   |
| `struct { ... }`          | `record { ... }`          | Record                                                                                     |
| `enum { ... }`            | `enum { ... }`            | Enumeration without payloads                                                               |
| `variant { ... }`         | `variant { ... }`         | Variant/sum type with payloads                                                             |
| `flags { ... }`           | `flags { ... }`           | Bit flags                                                                                  |
| `type T = U`              | `type t = U`              | Newtype, as a named alias of its base; a stdlib one too (`ByteList` is `byte-list`)        |
| `resource`                | `resource`                | Resource handle; owned and borrowed handles both map here                                  |
| `Stream<T>`               | `stream<T>`               | Component Model async stream                                                               |
| `Future<T>`               | `future<T>`               | Component Model async future                                                               |

A function type has no row: it crosses only as a [callback](#callbacks).

These types have no Component Model type, so they do not cross a component
boundary:

- `f16`, `bf16` ([Half Precision](./spec-types.md#half-precision-f16-bf16)) and
  `v128`, and a SIMD lane type such as `f32x4`, since each is a newtype over
  `v128`. A component that carries half precision data exports its bits, as a
  `List<u16>`.
- `Array<T>` and the newtypes over it (`ByteArray`). A `List<T>` crosses in its
  place.
- A reference to anything but a resource, such as `&List<i32>`. A reference to
  a resource crosses as a `borrow` handle
  ([Handles at the Boundary](#handles-at-the-boundary)).
- A view: `Slice<T>`, `StrSlice`, the newtypes over them (`ByteSlice`), and any
  struct with a field referencing what is not a resource.

A reference and a view both point into storage they do not own; pass an owned
value such as a `List<T>` or a `String` instead.

A signature that names one of them, anywhere within it, is a compile error at
that parameter, or at the function's declaration for its result. This holds for
every `export fn`, not only a world's entry point, because each one lands on the
component's surface, and for every imported operation.

An anonymous struct never crosses either: no signature can name one yet
([Anonymous Structs](./spec-types.md#anonymous-structs)).

Rationale: [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

### List Arguments

A synchronous imported resource method or world function, from WASI or from a
component, takes a `list<T>` parameter, for `T` a number type (`u8`…`u64`, `s8`…`s64`, `f32`,
`f64`), as a type parameter `affine L: ListArg<Elem = T>` from `core:linear`. A
`List<T>`, or a newtype over one such as `ByteList`, copies its elements for the
call. A `&LinearArray<T>` lends its own block, which the callee reads in place.
Either way, what the call copied is freed once it returns.

An asynchronous import, and an interface's operation, take the `List<T>`. The
host reads an asynchronous call's arguments after it returns, and an operation
declares no type parameters.

Rationale: [WEP: Linear Arrays](./wep-2026-10-26-linear-arrays.md).

### Published Types

An `export fn` puts every named type its signature reaches, through fields and
payloads too, into the package's Component Model interface. The package
publishes each one, in one of two ways:

- It declares the type `export`: `export struct`, `export enum`,
  `export variant`, `export flags`, or `export type`.
- It adopts the type with `export use { T } from "…";`, whichever package
  declares it. This is how a package publishes a type a Kiln generator wrote
  for it, where no `export` can be written. An `export use` names a
  non-generic struct, enum, variant, flags or newtype. It never names a `#[cm]`
  type, whose own interface publishes it.

An adopted type takes the name the `export use` gives it:
`export use { FileType as Kind } from "core:fs";` publishes `variant kind`, and
every reference to the type in the interface says `kind`, a field of another
adopted type included. A package publishes a type under one name, so adopting
it under another name than an earlier `export use`, or than its own where it is
declared `export`, is a compile error.

A reached type the package does not publish is a compile error at the
parameter, or at the function's declaration for its result. Two kinds of type
need no publishing: a `#[cm]` type, whose own interface publishes it, and a
standard library newtype such as `ByteList`, which crosses as the alias it is.

A WIT record shows every field, so each field of a published struct is `pub`.
A field that is not is a compile error, at the field for an `export struct` and
at the `export use` for an adopted one.

### Callbacks

A function type crosses the Component Model boundary in one place: as a
parameter of a `#[cm]` import, where it is a callback. Anywhere else it is a
compile error: in an `export fn`'s parameters or result, in an import's result,
and inside a type that crosses, as a field or a payload (`Option<fn(..)>`
included).

<!-- {"fixture":"spec_functions_cm_callback.wado"} -->

```wado
#[cm("example:demo/target", linearity = "unrestricted")]
resource Target {
    #[cm("example:demo/target#listen")]
    #[cm_params("self", "listener")]
    fn listen(&self, listener: fn mut(i32));
}

test {
    let target = mock_target();
    let mut seen: List<i32> = [];
    with Target => &mut EchoTarget {} do {
        target.listen(|v| seen.push(v));
    }
    assert seen == [7];                 // `EchoTarget` calls the listener with 7
}
```

- A callback's parameters are scalars and handles, and it returns nothing. Any
  other function type on a `#[cm]` declaration is a compile error.
- The closure stays in the guest. The call registers it and passes a `u32` key
  in its place. One closure value has one key.
- The host calls the closure back through the `wado:callback/callback`
  interface, which the component exports. It holds one function per argument
  shape, named `call` followed by one word per argument: the argument's
  primitive type, or `handle` for a handle (`call`, `call-i32`, `call-handle`).
  Each takes the key first, is lifted synchronously, and returns nothing.
  `wado wit` lists the interface among the world's exports.
- The host may call a callback during an import call, which reenters the
  component, or while `run` is suspended.
- A callback runs as a task of its own. It may perform an effect a world import
  backs.
- Where a Wado handler answers the `#[cm]` interface instead of the host, the
  handler receives the closure itself and may call it.

Rationale: [WEP: The Web Interface for Wado](./wep-2026-04-01-web.md#callbacks).

## Concurrency Model

Wado follows the Component Model's concurrency model. It has no `await`: a
wait blocks the current task until the value is ready, so a function that waits
is not itself async.

### Async Imports

A Component Model `async func` import is an interface operation declared
`async fn op(...) -> AsyncCall<T>`
([Async Operations](./spec-effects.md#async-operations)). Calling it starts the
call and returns an `AsyncCall<T>` at once. The caller decides when to wait:

- `.wait()` blocks until the call returns, then yields its `T`.
- `.cancel()` abandons the call.
- `set.insert(call)` moves the call into a `WaitableSet`, so one wait covers
  several calls and streams.

A function or closure that starts an async call also holds the effects these
methods declare, so it may later wait on that call without declaring them. A
closure is a body of its own: a call it starts grants nothing to the function
around it. A call owns its `Subtask`, so a function handed one through its
signature holds them too
([Resources as Effects](./spec-effects.md#resources-as-effects)).

<!-- {"fixture":"spec_components_async_import.wado"} -->

```wado
use { Client, Request, Response, ErrorCode } from "wasi:http";

fn fetch(req: Request) -> Result<Response, ErrorCode> with Client {
    let call = Client::send(req);   // the request starts; nothing waits yet
    // ... work here runs while the host handles the request ...
    return call.wait();             // blocks until the response arrives
}

test {
    let mut mock = MockClient {};
    let fetched = with Client => &mut mock do { fetch(mock.new_request()) matches { Ok(_) } };
    assert fetched;
    mock.end_writes();
}
```

An `AsyncCall<T>` is affine: `wait` and `cancel` take it by value, and
assigning one moves it ([Affine Types](./spec-affine.md)). A
call that drops unawaited is cancelled, and its result buffer freed.

<!-- {"fixture":"async_call_moves.wado"} -->

```wado
with Fetch => &Canned {} do {
    let call = Fetch::get("/abc");
    let moved = call;
    assert call.wait() == 4;   // error: `call` moved to `moved`
    println(`${moved.wait()}`);
}
```

#### Waiting on Several

A `WaitableSet<W>` owns what it waits on. `insert` moves a member in, and
`wait` blocks until one has an event, then moves that member back out in a
`Ready<W>` beside the event. `poll` asks without blocking. The event is a
`ReadyEvent`: `Call` with the `CallState` a call moved to, `Copied` with the
`CopyResult` a started stream copy ended on, or `Unreported` for a member that
neither has a handle for the host to report nor an event of its own. A member
whose event is already known enters ready with that event, whether the host
reported it to another set or the call or copy settled as it started. A set
that drops drops its remaining members first.

A member is any type implementing `Waitable`, whose `wait_on` answers either the
handle the host reports the member by or the event it is ready with. The trait
is sealed: the standard library implements it on the calls and on the started
stream copies (`ReadCopy`, `WriteCopy`), since a hand-written `wait_on` could
name a handle its value does not own. A bare stream or future end is no member: no
copy on it is in flight, so the host has no event to report for it. A set
waiting on two kinds takes a variant over them, which
derives the trait with the marker `impl Waitable for V;`. Each case holds one member, or none for one that is
ready without the host:

<!-- {"fixture":"waitable_set_variant_members.wado"} -->

```wado
affine variant Timer {
    Short(AsyncCall<()>),
    Long(AsyncCall<()>),
}

impl Waitable for Timer;

fn first_ready() -> String with MonotonicClock {
    let mut set = WaitableSet::<Timer>::new();
    set.insert(Timer::Long(MonotonicClock::wait_for(60_000_000_000)));
    set.insert(Timer::Short(MonotonicClock::wait_for(0)));
    let ready = set.wait();
    let name = match ready.member {
        Short(call) => {
            call.wait();
            "short"
        },
        Long(call) => {
            call.wait();
            "long"
        },
    };
    return `${name} first, ${set.len()} left`;
}

export fn run() with (Stdout, MonotonicClock) {
    assert first_ready() == "short first, 1 left";
    println("set dropped");
}
```

The host reports a call's resolution once, but a call a set handed back
after its resolution still ends as any call does. `wait` returns its result,
`cancel` and a drop end it, and a set it enters again reports it at once with
the same event.

#### Handling an Async Operation

A [handler](./spec-effects.md#handlers) method for an async operation is a
plain `fn` that returns `T`, and it resumes with a `T`. The caller still
receives an `AsyncCall<T>`, one that has already completed, so its `.wait()`
returns the value at once.

<!-- {"fixture":"spec_components_async_import.wado"} -->

```wado
impl Client for MockClient {
    fn send(&mut self, request: Request) -> Result<Response, ErrorCode> {
        resume Result::<Response, ErrorCode>::Ok(self.canned_response());
    }
}

test {
    let mut mock = MockClient {};
    with Client => &mut mock do {
        let call = Client::send(mock.new_request());
        assert call.wait() matches { Ok(_) };   // completed already
    };
    mock.end_writes();
}
```

Rationale: [WEP: Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md).

### Async Exports

An `export async fn` uses the Component Model async calling convention. Its
body delivers the result with [`task return`](./spec-worlds.md#task-return-statement) and may
keep running afterwards, for example to write a response's trailers.

### Streams and Futures

`Stream<T>` and `Future<T>` are unbuffered channels. A write settles once the
other end reads, and a read once the other end writes. A task that waits on one
end before it starts the copy on the other blocks forever, so it either starts
both copies before waiting, or hands one end to another task: an async import's
subtask, or the caller of an `export async fn`.

A future's ends are each used once, so both consume their end:
`Future::read(self)` and `FutureWritable::write(self, value)`. A second read or
write is a use after move. A `FutureWritable` dropped before its write traps
with a message naming it, since the reader would otherwise wait forever.

A stream copy is started and finished in two steps, so a task can wait on
several at once. `Stream::start_read(self, max)` returns a `ReadCopy<T>` and
`StreamWritable::start_write(self, data)` a `WriteCopy<T>`. Each moves the end
into the copy, since the Component Model traps on a second copy on an end that
is copying. `finish` waits where the copy has not settled, and hands back the
end with what the copy moved. Both copies are `Waitable`, and a set reports a
settled one as `ReadyEvent::Copied`. Dropping a copy that has not settled
cancels it. `read_to_end`, `read_to_end_bytes`, `write_all` and `write_raw_all`
borrow the end and copy until the stream ends.

A write hands back what it did not deliver: the part of the value a reader
dropped before taking, or the suffix of a stream write the reader did not
take. `FutureWritable::write` returns a `FutureWrite<T> { rest: Option<T>, result }`, and `WriteCopy::finish` a `StreamWrite<T> { rest: List<T>, result }`. A caller that ignores the result drops the rest.

A handler for `Stream<T>` or `StreamWritable<T>` answers the operations a copy
starts with, `begin_read` and `begin_write`. It returns a copy that has already
settled, built by `StartedRead::settled` or `StartedWrite::settled`, or one it
started on another end, so every copy a program makes goes through it, a
started one included. A copy dropped in flight is cancelled by `cancel_read` or
`cancel_write` on the end the program holds. A handler that started the copy on
another end answers that operation by cancelling there.

A future write's `result` is a `FutureWriteResult`. It is `Pending` where the
write returned before its reader read: the host then holds the value and hands
it to the reader, so nothing comes back in `rest`.

## Linking and Resources

### Attribute Syntax for Component Model Linking

`#[cm(...)]` links a Wado definition to the Component Model definition it stands for:

<!-- {"fixture":"spec_components_cm_attributes.wado"} -->

```wado
// Link an effect interface to a CM interface
#[cm("example:cli/console@0.1.0")]
pub interface Console {
    #[cm("example:cli/console@0.1.0#write-via-stream")]
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}

// Link a resource to a CM resource
#[cm("example:cli/terminal-output@0.1.0#terminal-output")]
pub resource TerminalOutput;

// Link an enum to a CM enum, and each case to its WIT case
#[cm("example:cli/types@0.1.0#error-code")]
pub enum ErrorCode {
    #[cm("io")]
    Io,
    #[cm("illegal-byte-sequence")]
    IllegalByteSequence,
    #[cm("pipe")]
    Pipe,
}

test {
    assert ErrorCode::IllegalByteSequence != ErrorCode::Pipe;   // the CM name changes no Wado name
}
```

A type declared in an interface
([Effect Definition](./spec-effects.md#effect-definition)) takes its CM name
from that interface: the interface's name with the type's name in kebab case,
so `Geo::Point` in `a:x/geo@0.1.0` is `a:x/geo@0.1.0#point`. A
`#[cm("…#name")]` on a module-level type is an error, so that form names only
an operation.

> Not yet implemented: a type is not yet declared inside an interface, so
> `TerminalOutput` and `ErrorCode` above sit at module level and name their
> interface through `#[cm]`
> ([WEP](./wep-2026-10-22-component-export-names.md)).

`#[cm_params("name", ...)]` on an operation gives the CM-side names of its parameters. Without it, each parameter's CM name is its Wado name in kebab-case.

#### Resource Linearity

A `#[cm(...)]` resource may declare what may be done with its handle: `linearity = "affine"` or `linearity = "unrestricted"`. Omitting the field reads as `"affine"`, and a resource without `#[cm(...)]` is affine.

An affine resource is move-only and carries a drop obligation (see [Affine Types](./spec-affine.md)). An unrestricted one owns nothing, so it is an ordinary copyable value. Assigning or passing one leaves the original usable, and nothing is dropped at the end of a scope.

The representation follows from the linearity. An affine resource crosses the Component Model boundary as an `own` / `borrow` handle, an unrestricted one as a plain `f64` the host interprets (see [Handle Encoding](#handle-encoding)). `as` converts an unrestricted handle to or from `f64`, keeping every bit, or upcasts it to a resource it extends. No other cast accepts one, so an affine resource and an unrestricted one never convert into each other.

An unrestricted resource is not a Component Model `resource`. Its operations are plain CM functions that take the handle as an ordinary parameter, so a `#[cm(...)]` name written in the `[constructor]T`, `[method]T.m` or `[static]T.m` form is an error on one. `classes = "..."` (see [Resource Inheritance](#resource-inheritance)) is accepted only beside `linearity = "unrestricted"`.

### Resource Ownership

An affine resource follows [Affine Types](./spec-affine.md). A resource's `fn drop(self)` is its `Drop` impl, and an imported resource's ends the handle through the Component Model's `resource.drop`. Lowering a resource to the host as an `own`, as an argument, a result or what a stream or future write carries, moves it: the host owns the handle from then on.

#### Handles at the Boundary

| Wado position                                      | CM handle                           |
| -------------------------------------------------- | ----------------------------------- |
| By-value `R` parameter or result                   | `own<R>`                            |
| `&R` or `&mut R` parameter, `&self` or `&mut self` | `borrow<R>`                         |
| Bare `self` receiver                               | `own<R>`, transferring the receiver |

Rationale: [WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md).

### Resource Inheritance

`resource Child extends Parent` declares that a child handle is usable wherever the parent is. The clause stands between the resource's name and its body, and names one resource as the parent. Both resources must declare `linearity = "unrestricted"`, because an upcast copies the handle and an affine one may not be copied. Single inheritance only, and a cycle is an error.

<!-- {"fixture":"spec_components_resource_extends.wado"} -->

```wado
#[cm("example:ui/target", linearity = "unrestricted", classes = "0..=1")]
resource Target {
    #[cm("example:ui/target#add-listener")]
    fn add_listener(&self, kind: String);
}

#[cm("example:ui/widget", linearity = "unrestricted", classes = "1..=1")]
resource Widget extends Target {
    #[cm("example:ui/widget#label")]
    fn label(&self) -> Option<String>;
}

fn use_it(w: Widget) {
    w.add_listener("click");   // inherited, no cast
    let t: Target = w;         // upcast is implicit
}

test {
    let w = 137438953472.0 as Widget;   // class 1, object 0
    let t: Target = w;
    assert t == w && t matches { _: Widget };
}
```

A generic resource takes no part in `extends`, on either side.

#### Subtyping

`extends` induces `Child <: Parent`. The relation is reflexive and transitive, and resources `extends` does not relate are incomparable.

- The upcast is implicit wherever a value, a `return`, or a `&T` referent is expected, and where branches of an `if` or `match` meet.
- A type parameter is solved to the most specific type, and the upcast happens later, at a use. So a constructor's payload is not upcast: `Option::Some(el)` against `Option<Node>` is an `Option<Element>`, and is written `Option::Some(el as Node)`.
- `&mut T` is invariant, and so is every generic type, a tuple's elements, and a struct's fields. A write through any of them could install a parent where a child is required. There is no variance annotation.
- A function type is invariant in its parameters and its result.
- Narrowing back to a child is never implicit. It is written as a [type pattern](./spec-patterns.md#type-patterns).

<!-- {"fixture":"spec_components_option_invariant.wado"} -->

```wado
let r: Option<HtmlInputElement> = Option::Some(input);
let n: Option<Node> = r;                    // ERROR: Option is invariant
```

<!-- {"fixture":"spec_components_type_patterns.wado"} -->

```wado
let r: Option<HtmlInputElement> = Option::Some(input);
let n: Option<Node> = r.map(|el| el as Node);  // OK: each element is upcast
assert n matches { Some(_: HtmlInputElement) };
```

#### Methods

A method call resolves statically. `recv.m()` gathers every `m` declared along `recv`'s `extends` chain and every `m` of a trait impl that applies to it. None is an error, and one resolves the call, upcasting the receiver to the resource that declares it. Two or more is ambiguous: write `Declaring::method(&value)` or `Trait::method(&value)` to pick one.

- A child may not redeclare a method it inherits.
- Static methods (no `&self`) are not inherited.
- `Self` in an inherited method names the resource that declares it, not the receiver's type.

#### Handle Encoding

An unrestricted handle is a number the host mints: an integer-valued `f64` below 2^53, equal to `class * 2^37 + index`. The class takes 16 bits and the index into the host's object table takes 37. The host hands out one handle per object, and tags each object with the class of the nearest ancestor of its runtime type that the program declares. A type the program does not name therefore reads as its nearest named ancestor.

`classes = "lo..=hi"` on the `#[cm(...)]` numbers the classes of an `extends` tree. A resource's own class is `lo`, and the resources extending it hold the rest of the range:

- A child's range lies inside its parent's, above the parent's own class.
- Sibling ranges do not overlap.
- A tree declares `classes` on every resource or on none.
- A range may leave gaps, which stand for classes the program does not declare.

A type pattern narrowing to `T` tests whether the handle's class lies in `T`'s range. A negative value, an infinity and a NaN lie in no range. A type pattern narrows only to a resource that declares `classes`.

#### Traits on Handles

- `Eq`: every unrestricted resource is `Eq`, so a type holding one derives `Eq` too. `==` and `!=` compare two handles when one type extends the other. Equal handles name one object (see [Handle Encoding](#handle-encoding)). Handles compare by bits, so a NaN handle equals itself and `-0.0` differs from `0.0`.
- `Ord`: none. Handles have no order.
- `Inspect`: `${x:?}` renders the dynamic type the handle's class names, as [Inspect Output](./spec-literals.md#inspect-output) states.
- `Serialize` and `Deserialize`: a resource, and a struct or variant that holds one, cannot derive either. A handle means something only inside its running instance. A hand-written impl may serialize what it reads from the host object.

Rationale: [WEP: Resource Inheritance and Narrowing](./wep-2026-04-28-resource-inheritance.md).
