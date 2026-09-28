# Worlds and Entry Points

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

A world imports whole interfaces, as a WIT world does, and exports interfaces or functions:

<!-- {"fixture":"spec_components_world_declaration.wado"} -->

```wado
#[cm("example:app/plugin@0.1.0")]
pub world Plugin {
    import Stdout;
    import Environment;

    export Run;                                              // an interface
    export fn transform(input: String) -> String;            // a function
    export async fn fetch(url: String) -> Result<String, String>;
}

test {
    assert true;   // a world is a declaration: accepting it is the check
}
```

- `import Iface;` and `export Iface;` name a `pub interface`. The interface's own `#[cm(...)]` gives its Component Model name and version, and an export takes its signatures from the interface.
- `export [async] fn name(...) -> T;` exports a freestanding function. `async` marks an export that maps to a WIT `async func`.
- `#[cm("namespace:package/world@version")]` on the world gives its Component Model name.

### WASI CLI World Example

The standard WASI CLI `command` world, as `wasi:cli` declares it:

<!-- {"fixture":"spec_components_command_world.wado"} -->

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

test {
    assert true;   // a world is a declaration: accepting it is the check
}
```

`Run` declares `async fn run() -> AsyncCall<Result<(), ()>>`. A program implements it with an `export fn run()`:

<!-- {"fixture":"spec_components_run.wado"} -->

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let greeting = "Hello, WASI world!";
    assert greeting.len() == 18;
    println(greeting);
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

A dependency is imported through its library world's entry module, the file its `[package].lib` names. A dependency without `[package].lib` cannot be imported. A `path` dependency that names a single `.wado` file has that file as its entry module.

Rationale: [WEP: Package Manifest](./wep-2026-02-14-package-manifest.md).

## Entry Points

Each hosted world defines its entry point:

| World                 | Entry Point                                                               | Driver       |
| --------------------- | ------------------------------------------------------------------------- | ------------ |
| `wasi:cli/command`    | `export fn run()`                                                         | `wado run`   |
| `wasi:http/service`   | `export async fn handle(request: Request) -> Result<Response, ErrorCode>` | `wado serve` |
| `core:kiln/generator` | `export fn generate(...)`                                                 | Kiln         |
| `test`                | the entry module's `test` blocks                                          | `wado test`  |

`test` is a synthetic world: it exports the entry module's `test` blocks and nothing else. See [Selecting a World](#selecting-a-world) for how a program's world is chosen.

## `task return` Statement

`task return expr;` is a statement that calls the Component Model `task.return` instruction, delivering the function's result without terminating the Wasm function. Execution continues after `task return`, allowing the function to fulfill outstanding futures (e.g. trailers) or perform cleanup.

### Motivation

HTTP handlers return a `Response` that contains a `Future`-based trailers channel. With a regular `return`, the Wasm function exits immediately, making it impossible to write to that channel. `task return` separates result delivery from function termination:

<!-- {"fixture":"spec_components_task_return.wado"} -->

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_future, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Headers::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_future);
    assert response.get_status_code() == 200;  // the default status

    task return Result::<Response, ErrorCode>::Ok(response); // deliver result; function continues
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null)); // fulfill trailers
}
```

### Rules

- `task return` is only valid inside `export async fn` bodies.
- An `export async fn` body must carry a `task return`, because a body without one could never deliver its result. A body whose every path provably exits first (`panic`, an endless loop) has no result to deliver and is exempt.
- Whether a `task return` under a branch is reached is not checked. A path that misses it traps at the boundary, the same as a declared result the body never binds.
- Regular `return` is forbidden in `async fn` bodies. It would exit the Wasm function without notifying the CM runtime.
- The `task return` expression is type-checked against the declared return type of the enclosing `export async fn`.
- `task return` delivers the result to the function's caller. A call through the component boundary delivers it to the Component Model runtime, and a Wado caller receives it as an ordinary return value.
- The `async` of an `export async fn` asks nothing of a Wado call site, which calls it as any other function. It selects the CM async calling convention at the component boundary.

## WASI P3 CLI Interfaces

Wado targets WASI Preview 3 (0.3.0), whose `stream<T>` and `future<T>` types
map directly to Wado's `Stream<T>` and `Future<T>`. Wado effects map to WASI P3
interfaces:

| Wado Effect   | WASI Interface         | Key Functions                                                           |
| ------------- | ---------------------- | ----------------------------------------------------------------------- |
| `Stdout`      | `wasi:cli/stdout`      | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stderr`      | `wasi:cli/stderr`      | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stdin`       | `wasi:cli/stdin`       | `read-via-stream() -> tuple<stream<u8>, future<result<_, error-code>>>` |
| `Environment` | `wasi:cli/environment` | `get-arguments()`, `get-environment()`                                  |
| `Exit`        | `wasi:cli/exit`        | `exit(result)`, `exit-with-code(u8)`                                    |
