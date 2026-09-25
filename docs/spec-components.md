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
See [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

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
again. A handler for an async operation resumes with the `T` itself, and the
caller's `.wait()` returns it at once. See
[WEP: Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md) and
[WEP: Effect Handler](./wep-2026-04-11-effect-handler.md).

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

A package's `wado.toml` maps each hosted world it targets to an entry file in its `[world]` table, and `[package].lib` names the entry of its library world. See [WEP: Package Manifest](./wep-2026-02-14-package-manifest.md).

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

A program does not name its world in source. The `--world` option of `wado compile` selects it, or the `[world]` table of `wado.toml` maps each world to its entry file. Without either, `wado compile` and `wado run` target `wasi:cli/command`, and `wado serve` targets `wasi:http/service`.

A package may target several worlds, one entry file each:

```toml
[world]
"wasi:cli/command" = "src/cli.wado"
"wasi:http/service" = "src/server.wado"
```

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

A `#[cm(...)]` resource may declare what may be done with its handle: `linearity = "affine"` or `linearity = "unrestricted"`. Omitting the field reads as `"affine"`.

An affine resource is move-only and carries a drop obligation, per [Resource Ownership](./wep-2026-05-21-resource-ownership.md). An unrestricted one owns nothing, so it is an ordinary copyable value. Assigning or passing one leaves the original usable, and nothing is dropped at the end of a scope.

The representation follows from the linearity. An affine resource crosses the Component Model boundary as an `own` / `borrow` handle, an unrestricted one as a plain `f64` the host interprets. `as` converts an unrestricted handle to or from `f64`, keeping every bit, or upcasts it to a resource it extends. No other cast accepts one.

### Resource Inheritance

`resource Child extends Parent` declares that a child handle is usable wherever the parent is. Both resources must declare `linearity = "unrestricted"`, because an upcast copies the handle and an affine one may not be copied. Single inheritance only, and a cycle is an error.

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

Rules:

- The upcast is implicit wherever a value, a `return`, or a `&T` referent is expected, and where branches of an `if` or `match` meet. `&mut T`, container elements (`List<T>`, `Option<T>`, …) and function types are invariant.
- Narrowing back to a child is never implicit. It is written as a type pattern (below), which tests the class the host tagged the handle with.
- `classes = "lo..=hi"` numbers those classes: a resource's own is `lo`, and the resources extending it hold the rest. A child's range lies inside its parent's, above the parent's own class. Sibling ranges do not overlap, and an `extends` tree declares `classes` on every resource or on none. A type pattern narrows only to a resource that declares them.
- `==` and `!=` compare two handles when one type extends the other. The host hands out one handle per object, so equal handles name one object. Handles compare by bits, so a NaN handle equals itself and `-0.0` differs from `0.0`. An unrestricted resource is `Eq`, so a type holding one derives `Eq` too. There is no ordering.
- A child may not redeclare a method it inherits. A name reachable through both the chain and a trait impl is ambiguous: write `Declaring::method(&value)` or `Trait::method(&value)` to pick one.
- Static methods (no `&self`) are not inherited, and `Self` in an inherited method names the resource that declares it.
- A generic resource takes no part in `extends`.

See [Resource Inheritance and Narrowing](./wep-2026-04-28-resource-inheritance.md) for the design and its known gaps.

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

This is not [`match type`](./wep-2026-09-05-total-reflection.md), which narrows a type parameter at compile time, is exhaustive, and takes no `_`.
