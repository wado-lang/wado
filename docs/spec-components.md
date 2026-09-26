# Components and Worlds

## Type Mapping at Component Boundaries

Wado types lift and lower to Component Model types when they cross a component boundary (the Canonical ABI). The compiler performs this conversion automatically.

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
| `List<T>`                 | `list<T>`                 | Dynamic array                                                                              |
| `TreeMap<K, V>`           | `map<K, V>`               | `K` is `bool`, `char`, `String`, or an integer; a repeated key takes the last pair's value |
| `[T1, T2, ...]`           | `tuple<T1, T2, ...>`      | Tuple types                                                                                |
| `Option<T>`               | `option<T>`               | Optional value                                                                             |
| `Result<T, E>`            | `result<T, E>`            | Result type; `result<ok>` and bare `result` are the payload-elided forms                   |
| `struct { ... }`          | `record { ... }`          | Record                                                                                     |
| `enum { ... }`            | `enum { ... }`            | Enumeration without payloads                                                               |
| `variant { ... }`         | `variant { ... }`         | Variant/sum type with payloads                                                             |
| `flags { ... }`           | `flags { ... }`           | Bit flags                                                                                  |
| `resource`                | `resource`                | Resource handle; owned and borrowed handles both map here                                  |
| `Stream<T>`               | `stream<T>`               | Component Model async stream                                                               |
| `Future<T>`               | `future<T>`               | Component Model async future                                                               |

`f16` and `bf16` have no Component Model type, so they do not cross a component
boundary. An `export fn` whose signature names either one is a compile error.
Every `export fn` is checked, not only a world's entry point, because each one
lands on the component's surface. A component that carries half precision data
exports its bits, as a `List<u16>`.

Rationale: [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

## Concurrency Model

Wado follows the Component Model's concurrency model. It has no `await`: a
wait blocks the current task until the value is ready, so an ordinary function
may wait without saying so in its signature.

### Async Imports

A Component Model `async func` import is an interface operation declared
`async fn op(...) -> AsyncCall<T>`. Calling it starts the call and returns an
`AsyncCall<T>` at once. The caller decides when to wait:

- `.wait()` blocks until the call returns, then yields its `T`.
- `.cancel()` abandons the call.
- `.join(&set)` adds the call to a `WaitableSet`, so one wait covers several
  calls and streams.

```wado
use { Client, Request, Response, ErrorCode } from "wasi:http";

fn fetch(req: Request) -> Result<Response, ErrorCode> with Client {
    let call = Client::send(req);   // the request starts; nothing waits yet
    // ... work here runs while the host handles the request ...
    return call.wait();             // blocks until the response arrives
}
```

An `AsyncCall<T>` is used once: after `wait` or `cancel` it must not be touched
again. `join` only registers the call with the set, so the caller still owes the
`wait` or `cancel` that ends it.

#### Handling an Async Operation

A [handler](./spec-effects.md#handlers) implements an async operation as a plain
`fn` that returns `T` and resumes with a `T`. The caller still receives an
`AsyncCall<T>`, one that has already completed, so its `.wait()` returns the
value at once.

```wado
impl Client for MockClient {
    fn send(&mut self, request: Request) -> Result<Response, ErrorCode> {
        resume Result::<Response, ErrorCode>::Ok(canned_response());
    }
}
```

Rationale: [WEP: Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md).

### Async Exports

An `export async fn` uses the Component Model async calling convention. Its
body delivers the result with [`task return`](#task-return-statement) and may
keep running afterwards, for example to write a response's trailers.

### Streams and Futures

`Stream<T>` and `Future<T>` are unbuffered channels. A `write` blocks until the
other end reads, and a `read` blocks until the other end writes, so the two ends
must be driven by different tasks.

## World System

### What is a World?

A world in Wado corresponds directly to the Component Model's `world` concept. A world defines the contract between a Wasm component and its environment:

1. Imports: Which capabilities the component requires (provided by the host or by other components)
2. Exports: Which functions and types the component provides

Worlds are classified into two categories:

- Hosted world: A world that a runtime knows how to instantiate and drive. The runtime provides all imports and invokes the exports according to a defined lifecycle. Examples: `wasi:cli/command` (executed by `wado run`), `wasi:http/service` (executed by `wado serve`). Informally called a "well-known world."
- Library world: A world that defines a component's public API for composition. It is not directly executed by a runtime; instead, other components import its exports. Example: a `json` library that exports parsing functions.

This distinction is not part of the Component Model specification, which treats all worlds uniformly. In Wado, the distinction matters for tooling: `wado run` and `wado serve` select a hosted world, while `wado.toml`'s `[package].lib` field defines a library world.

### World Declaration

A world imports whole interfaces and exports interfaces or functions:

```wado
#[cm("example:app/plugin@0.1.0")]
pub world Plugin {
    import Stdout;
    import Environment;

    export Run;                                              // an interface
    export fn transform(input: String) -> String;            // a function
    export async fn fetch(url: String) -> Result<String, String>;
}
```

- `import Iface;` and `export Iface;` name a `pub interface`. The interface's own `#[cm(...)]` gives its Component Model name and version.
- `export [async] fn name(...) -> T;` exports a freestanding function. `async` marks an export that maps to a WIT `async func`.
- `#[cm("namespace:package/world@version")]` on the world gives its Component Model name.

### WASI CLI World Example

The standard WASI CLI `command` world, as `wasi:cli` declares it:

```wado
#[cm("wasi:cli/command@0.3.0")]
pub world Command {
    import Environment;
    import Exit;
    import Stdin;
    import Stdout;
    import Stderr;
    import TerminalStdin;
    import TerminalStdout;
    import TerminalStderr;
    import MonotonicClock;
    import SystemClock;
    import Timezone;
    import Preopens;
    import IpNameLookup;
    import Random;
    import Insecure;
    import InsecureSeed;

    export Run;
}
```

`Run` declares `async fn run() -> AsyncCall<Result<(), ()>>`. A program implements it with an `export fn run()`:

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello, WASI world!");
}
```

### Selecting a World

A program does not name its world in source. The package's `wado.toml` maps each world it targets to an entry module, or the `--world` option of `wado compile` selects the world for a single file. Without either, `wado compile` and `wado run` target `wasi:cli/command`, and `wado serve` targets `wasi:http/service`.

The manifest declares worlds in two places:

- The `[world]` table maps a hosted world, keyed by its fully qualified Component Model name, to its entry file. The path is relative to `wado.toml`.
- `[package].lib` names the entry module of the package's library world.

A package declares at least one world, and may declare several:

```toml
[package]
namespace = "acme"
name = "markdown"
version = "0.1.0"
lib = "src/lib.wado"

[world]
"wasi:cli/command" = "src/cli.wado"
"wasi:http/service" = "src/server.wado"
```

A hosted world's entry module exports the entry point that world requires (see [Entry Points](#entry-points)). The library world requires none: every `export` item of its entry module becomes part of an interface named after the package, `<namespace>:<name>/<name>@<version>`. A library world therefore needs `[package].namespace` to be built. The world itself is named `root`, so no package may take that name, in any letter case.

A dependency specifier (`"ns:pkg"` or `"lib:nick"`, see [Module Path Validation](./spec-modules.md#module-path-validation)) imports from the dependency's library world entry module. A dependency without `[package].lib` cannot be imported. A `path` dependency that names a single `.wado` file has that file as its entry module.

Rationale: [WEP: Package Manifest](./wep-2026-02-14-package-manifest.md).

### Design Notes

- Interface imports: a world imports whole interfaces, as a WIT world does.
- Versions: the `#[cm(...)]` of each interface and of the world carries its version (`@0.3.0`).
- Exports: an interface export takes its signatures from the interface. A function export spells its own.

## WASI / Browser Support

Wado targets WASI Preview 3 (0.3.0), which introduces native `stream<T>` and `future<T>` types that map directly to Wado's `Stream<T>` and `Future<T>`.

All Wado types map directly to Component Model (WIT) types. See the [Type Mapping at Component Boundaries](#type-mapping-at-component-boundaries) table in the Type System section for the complete mapping reference.

### WASI P3 CLI Interfaces

Wado effects map to WASI P3 interfaces:

| Wado Effect   | WASI Interface         | Key Functions                                                           |
| ------------- | ---------------------- | ----------------------------------------------------------------------- |
| `Stdout`      | `wasi:cli/stdout`      | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stderr`      | `wasi:cli/stderr`      | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stdin`       | `wasi:cli/stdin`       | `read-via-stream() -> tuple<stream<u8>, future<result<_, error-code>>>` |
| `Environment` | `wasi:cli/environment` | `get-arguments()`, `get-environment()`                                  |
| `Exit`        | `wasi:cli/exit`        | `exit(result)`, `exit-with-code(u8)`                                    |

### Entry Points

Each hosted world defines its entry point:

| World                 | Entry Point                                                               | Driver       |
| --------------------- | ------------------------------------------------------------------------- | ------------ |
| `wasi:cli/command`    | `export fn run()`                                                         | `wado run`   |
| `wasi:http/service`   | `export async fn handle(request: Request) -> Result<Response, ErrorCode>` | `wado serve` |
| `core:kiln/generator` | `export fn generate(...)`                                                 | Kiln         |
| `test`                | the entry module's `test` blocks                                          | `wado test`  |

`test` is a synthetic world: it exports the entry module's `test` blocks and nothing else. See [Selecting a World](#selecting-a-world) for how a program's world is chosen.

### `task return` Statement

`task return expr;` is a statement valid only inside `export async fn` bodies. It calls the Component Model `task.return` instruction, delivering the function's result to the CM runtime without terminating the Wasm function. Execution continues after `task return`, allowing the function to fulfill outstanding futures (e.g. trailers) or perform cleanup.

#### Motivation

HTTP handlers return a `Response` that contains a `Future`-based trailers channel. With a regular `return`, the Wasm function exits immediately, making it impossible to write to that channel. `task return` separates result delivery from function termination:

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_future, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Headers::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_future);

    task return Result::<Response, ErrorCode>::Ok(response); // deliver result; function continues
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null)); // fulfill trailers
}
```

#### Rules

- `task return` is only valid inside `export async fn` bodies.
- An `export async fn` body must carry a `task return`. One that carries none can never deliver, so every call of it would reach the boundary with the task unfinished; the compiler rejects it instead. A body whose every path provably exits first (`panic`, an endless loop) has no delivery to make and is exempt.
- Whether a `task return` under a branch is reached is not checked. A path that misses it traps at the boundary, the same as a declared result the body never binds.
- Regular `return` is forbidden in `async fn` bodies. It would exit the Wasm function without notifying the CM runtime.
- The `task return` expression is type-checked against the declared return type of the enclosing `export async fn`.
- `task return` names the function's result, and where it goes depends on who entered the function. The Component Model runtime receives it when the export binding did; a Wado caller receives it as an ordinary return value.
- The `async` of an `export async fn` asks nothing of a Wado call site, which calls it as any other function. It selects the CM async calling convention at the component boundary.

### Attribute Syntax for Component Model Linking

Use `#[cm(...)]` attributes to link Wado definitions to Component Model interfaces:

```wado
// Link an effect interface to a CM interface
#[cm("wasi:cli/stdout@0.3.0")]
pub interface Stdout {
    #[cm("wasi:cli/stdout@0.3.0#write-via-stream")]
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}

// Link a resource to a CM resource
#[cm("wasi:cli/terminal-output@0.3.0#terminal-output")]
pub resource TerminalOutput;

// Link an enum to a CM enum, and each case to its WIT case
#[cm("wasi:cli/types@0.3.0#error-code")]
pub enum ErrorCode {
    #[cm("io")]
    Io,
    #[cm("illegal-byte-sequence")]
    IllegalByteSequence,
    #[cm("pipe")]
    Pipe,
}
```

`#[cm_params("name", ...)]` on an operation gives the CM-side names of its parameters. Without it, each parameter's CM name is its Wado name in kebab-case.

#### Resource linearity

A `#[cm(...)]` resource may declare what may be done with its handle: `linearity = "affine"` or `linearity = "unrestricted"`. Omitting the field reads as `"affine"`, and a resource without `#[cm(...)]` is affine.

An affine resource is move-only and carries a drop obligation (see [Resource Ownership](#resource-ownership)). An unrestricted one owns nothing, so it is an ordinary copyable value. Assigning or passing one leaves the original usable, and nothing is dropped at the end of a scope.

The representation follows from the linearity. An affine resource crosses the Component Model boundary as an `own` / `borrow` handle, an unrestricted one as a plain `f64` the host interprets (see [Handle Encoding](#handle-encoding)). `as` converts an unrestricted handle to or from `f64`, keeping every bit, or upcasts it to a resource it extends. No other cast accepts one, so an affine resource and an unrestricted one never convert into each other.

An unrestricted resource is not a Component Model `resource`. Its operations are plain CM functions that take the handle as an ordinary parameter, so a `#[cm(...)]` name written in the `[constructor]T`, `[method]T.m` or `[static]T.m` form is an error on one. `classes = "..."` (see [Resource Inheritance](#resource-inheritance)) is accepted only beside `linearity = "unrestricted"`.

### Resource Ownership

An affine resource is move-only. Assigning it, passing it by value, returning it, placing it in an aggregate, and calling a method that takes `self` by value each move it. There is no `move` keyword: the transfer happens at the use. Using a binding after it has moved is a compile error.

```wado
pub resource Counter {
    fn bump(&self);
    fn consume(self);
}

fn eat(c: Counter) { ... }

fn misuse(c: Counter, d: Counter) {
    eat(c);          // moves `c`
    c.bump();        // ERROR: resource `c` used after it was moved
    d.consume();     // a by-value `self` moves `d` too
}
```

The check follows control flow. A move on one branch of an `if` or `match` counts after the branches meet, unless that branch diverges. A move inside a loop body is a use after move on the next iteration. A new `let` of the name, or an assignment to it, makes it usable again.

A value that holds an affine resource, directly or through a field, element or payload, is move-only in the same way. It moves as a whole: a resource is not moved out of it on its own, so a consuming method (below) is how one is taken out.

A method consumes a resource through a bare `self` receiver, and borrows it through `&self` or `&mut self` (see [Method Receiver: `self` by Value](./spec-memory.md#method-receiver-self-by-value)). `Option` and `Result` take `self` by value in `unwrap`, `expect`, `unwrap_or`, `unwrap_err` and `expect_err`, so extracting a resource consumes the container and leaves the resource with one owner.

#### No Move Out of a Borrow

A borrow leaves its referent with its owner, so a resource read out of a borrowed place would have two owners. A function whose result is a resource reached through a `&` or `&mut` parameter, `&self` included, is a compile error. That covers a field read, a dereference, a `match` binding over the borrowed value, and a `let` bound from any of these. A resource the function produces itself may be returned.

```wado
struct Holder { f: Fields }

impl Holder {
    fn peek(&self) -> Fields {
        return self.f;       // ERROR: cannot move resource `Fields` out of a borrow
    }

    fn into_fields(self) -> Fields {
        return self.f;       // OK: the holder is consumed
    }
}
```

#### Drop

An owned resource that has not moved is dropped when its scope ends, on every path out of it. A move suppresses that drop, so each handle is dropped exactly once. An imported resource drops through the Component Model's `resource.drop`. A value holding resources drops each of them, a struct's fields in declaration order. A resource value discarded as a statement (`Fields::new();`, `let _ = ...`) is dropped there.

A resource's `fn drop(self)` consumes its receiver, so the scope does not drop it again, and a use after it is a use after move.

A panic does not unwind, so it runs no drops.

#### Handles at the Boundary

| Wado position                                      | CM handle                                |
| -------------------------------------------------- | ---------------------------------------- |
| By-value `R` parameter or result                   | `own<R>`                                 |
| `&R` or `&mut R` parameter, `&self` or `&mut self` | `borrow<R>`                              |
| Bare `self` receiver                               | `own<R>`, transferring the receiver      |

A reference crosses a Component Model import only as a borrowed resource handle. An import taking any other reference, such as `&String`, is a compile error.

Rationale: [WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md).

### Resource Inheritance

`resource Child extends Parent` declares that a child handle is usable wherever the parent is. The clause stands between the resource's name and its body, and names one resource as the parent. Both resources must declare `linearity = "unrestricted"`, because an upcast copies the handle and an affine one may not be copied. Single inheritance only, and a cycle is an error.

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
```

A generic resource takes no part in `extends`, on either side.

#### Subtyping

`extends` induces `Child <: Parent`. The relation is reflexive and transitive, and resources `extends` does not relate are incomparable.

- The upcast is implicit wherever a value, a `return`, or a `&T` referent is expected, and where branches of an `if` or `match` meet.
- A type parameter is solved to the most specific type, and the upcast happens later, at a use. So a constructor's payload is not upcast: `Option::Some(el)` against `Option<Node>` is an `Option<Element>`, and is written `Option::Some(el as Node)`.
- `&mut T` is invariant, and so is every generic type, a tuple's elements, and a struct's fields. A write through any of them could install a parent where a child is required. There is no variance annotation.
- `Future<T>` and `Stream<T>` only hand out a `T`, so they are covariant in it. `FutureWritable<T>` and `StreamWritable<T>` only take one, so they are contravariant.
- A function type is invariant in its parameters and its result.
- Narrowing back to a child is never implicit. It is written as a [type pattern](#type-patterns).

```wado
let r: Option<HtmlInputElement> = ...;
let n: Option<Node> = r;                    // ERROR: Option is invariant
let n: Option<Node> = r.map(|el| el as Node);  // OK: each element is upcast
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

- `Eq`: every unrestricted resource is `Eq`, so a type holding one derives `Eq` too. `==` and `!=` compare two handles when one type extends the other. Equal handles name one object, because the host hands out one handle per object. Handles compare by bits, so a NaN handle equals itself and `-0.0` differs from `0.0`.
- `Ord`: none. Handles have no order.
- `Inspect`: `${x:?}` renders the dynamic type the class names, with the class and the index: `Element { type_id: 1, object_id: 7 }`. A class no resource in the tree owns keeps the static type's name. A resource without `classes`, or an `f64` no host minted, renders its number: `Node { handle: 1.5 }`.
- `Display`: `${x}` asks the host, through an imported formatter.
- `Serialize` and `Deserialize`: a resource, and a struct or variant that holds one, cannot derive either. A handle means something only inside its running instance. A hand-written impl may serialize what it reads from the host object.

Rationale: [WEP: Resource Inheritance and Narrowing](./wep-2026-04-28-resource-inheritance.md).

### Type Patterns

A pattern may ascribe a type: `p: T` matches when the subject is a `T`, and `p` binds it. The ascription on a `let` is this pattern, so one rule covers both spellings.

Whether the pattern can fail is decided statically, from the subject's type `S`:

| Relation          | Meaning                                                              |
| ----------------- | -------------------------------------------------------------------- |
| `S <: T`          | irrefutable — an upcast, or an ordinary type annotation              |
| `T <: S`, `T ≠ S` | refutable — a runtime test, and only where `extends` relates the two |
| otherwise         | a type error, as a mismatched annotation is                          |

An irrefutable ascription still drives type context, so `let x: i64 = 42` coerces the literal. A refutable one needs a pattern position that admits failure, so `let` and a `for` binding reject it exactly as they reject `Some(x)`:

```wado
let n: Node = el;                                   // Element <: Node — irrefutable upcast
let input: HtmlInputElement = el;                   // ERROR: refutable pattern in `let`
let input: HtmlInputElement = el else { return; };  // the guard form
if let input: HtmlInputElement = el { ... }
if node matches { _: Element } { ... }              // the predicate form

match node {
    input: HtmlInputElement => input.value(),
    elem: Element => elem.tag_name(),
    _ => "other",                                   // required: the hierarchy is open
}
```

A type match over resources always needs a final `_` arm, because the host may hand back a type the program does not name. An unguarded arm whose type is a supertype of a later arm's makes that later arm unreachable, which is an error, as [any unreachable arm](./spec-control-flow.md#exhaustiveness) is.

A refutable ascription tests a handle, so it binds a name or `_` and nothing deeper, and its subject is the value rather than a reference to it. `T` must be a concrete type: a type parameter says nothing about whether it narrows.

A type pattern narrows a value at runtime, unlike `match type`, which narrows a type parameter at compile time, is exhaustive, and takes no `_`.

## Known Gaps

### `AsyncCall<T>` Is Not Move-Only

[Async Imports](#async-imports) says an `AsyncCall<T>` is used once. Nothing checks it: `AsyncCall<T>` is a copyable struct, and `wait`, `cancel` and `join` all borrow it. A second `wait` or `cancel`, on the value or on a copy of it, compiles and reads a result buffer the first one freed.

### Only an Imported Async Operation Can Be Handled

A handler for an async operation that no Component Model import backs, one declared by a user `interface` or `resource`, is a compile error. Such an operation may not carry a default body either, so dispatching it always traps.

### Some Resource Holders Are Not Move-Checked or Dropped

[Resource Ownership](#resource-ownership) makes every value holding an affine resource move-only and dropped. Only a struct, a tuple and a `Result` are. An `Option`, a user variant or a `List` holding one can be moved twice with no diagnostic, and is not dropped at scope exit.

### A Resource Field Moves Out of Its Holder

`let c = h.c;` over a struct `h` holding a resource compiles, and `h` stays usable. Moving `h` afterwards leaves two owners of one handle.

### A Borrow Moved Out Through a Generic Body

[No Move Out of a Borrow](#no-move-out-of-a-borrow) is checked where the result type is a concrete resource type. A generic `fn get(&self) -> T { return self.v; }` instantiated with a resource type is accepted, and its result aliases the handle its receiver still owns.

### An Integer Casts to an Affine Resource

`5 as Counter` compiles for an affine `Counter` in any module. The result is a handle nothing minted, which the program then owns and drops.

### `Stream` and `Future` Handles Are Not Dropped

[Drop](#drop) drops an owned, unmoved resource at scope exit. `Stream<T>`, `StreamWritable<T>`, `Future<T>` and `FutureWritable<T>` are exempt: every path out of a scope holding one must call `drop` itself, and nothing reports a path that does not. A `?` or an early `return` in that scope leaks the handle.

### A Generic User Resource Is Not Move-Checked

A generic `resource Handle<T>` a user module declares is not move-checked: passing a `Handle<i32>` by value twice compiles. No program can obtain such a handle today, since a user-declared `#[cm]` resource has no import binding.

### Unrestricted Handles Are Never Released

Nothing frees an unrestricted handle. Each object the host hands out keeps a slot in its table, and the table keeps the object alive, for the lifetime of the instance. The host interns handles, so repeated calls naming one object cost one slot. Every distinct object costs one, such as each event a dispatch creates.

### `Display` on an Unrestricted Handle

[Traits on Handles](#traits-on-handles) gives `${x}` a host formatter. The import does not exist, so `${x}` on an unrestricted handle is a compile error.

### `Future` and `Stream` Are Invariant

[Subtyping](#subtyping) makes `Future<T>` and `Stream<T>` covariant, and their writable ends contravariant. The compiler treats them as invariant like every other generic type, so a `Future<Element>` does not pass where a `Future<Node>` is expected.
