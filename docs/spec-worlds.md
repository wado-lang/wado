# Worlds and Entry Points

A Wado program targets one world. The world is the contract between the program
and the host that runs it. Its imports are what the host provides, and a program
reaches them as effects (see [Effect System](./spec-effects.md)). Its exports
are what the program must provide, and for a hosted world that is the entry
point the host calls.

| World                 | Run by       | Entry point                                                               |
| --------------------- | ------------ | ------------------------------------------------------------------------- |
| `wasi:cli/command`    | `wado run`   | `export fn run()`                                                         |
| `wasi:http/service`   | `wado serve` | `export async fn handle(request: Request) -> Result<Response, ErrorCode>` |
| `test`                | `wado test`  | the entry module's `test` blocks                                          |
| `core:kiln/generator` | Kiln         | `export fn generate(...)`                                                 |

## World System

### What is a World?

A world in Wado is the Component Model's `world`. It declares two things:

1. Imports: the interfaces the component needs, provided by the host or by
   other components.
2. Exports: the functions and interfaces the component provides.

Wado sorts worlds into two kinds:

- A hosted world is one a runtime knows how to instantiate and drive. The
  runtime provides every import and calls the exports in a defined order.
  `wasi:cli/command` and `wasi:http/service` are hosted worlds.
- A library world defines a component's public API for composition. No runtime
  runs it directly. Other components import its exports instead.

The Component Model treats all worlds alike. The distinction is Wado's, and it
decides what a world asks of its entry module.

### World Declaration

A world imports whole interfaces, as a WIT world does, and exports interfaces or
functions:

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

- `import Iface;` and `export Iface;` name a `pub interface`. The interface's
  own `#[cm(...)]` gives its Component Model name and version. An export takes
  its signatures from the interface.
- `export [async] fn name(...) -> T;` exports a freestanding function. `async`
  marks an export that maps to a WIT `async func`.
- `#[cm("namespace:package/world@version")]` on the world gives its Component
  Model name.

### Selecting a World

A program does not name its world in source. The package's `wado.toml` maps
each world it targets to an entry module, or the `--world` option of
`wado compile` selects the world for a single file. Without either,
`wado compile` and `wado run` target `wasi:cli/command`, and `wado serve`
targets `wasi:http/service`.

A program's effects declare its imports: the component imports the interface of
every effect the program performs and does not handle itself. An effect the
selected world does not import is no exception. It is imported all the same,
and the host decides whether it can instantiate the component.

The manifest declares worlds in two places:

- The `[world]` table maps a hosted world, keyed by its fully qualified
  Component Model name, to its entry file. The path is relative to
  `wado.toml`.
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

The library world's entry module needs no [entry point](#entry-points).
[The Library World](./spec-packages.md#the-library-world) says what it offers
other packages, and what building it produces.

Rationale: [WEP: Package Manifest](./wep-2026-02-14-package-manifest.md) and
[WEP: World Conformance](./wep-2026-01-16-world-conformance-and-export.md).

## Entry Points

A hosted world's entry module defines the entry point the world exports. The
table at the top of this chapter lists each one. The entry point carries
`export`, since the host calls it across the component boundary.

A program without its world's entry point does not compile:

<!-- {"fixture":"spec_worlds_run_missing.wado"} -->

```wado
fn helper() -> i32 {
    return 1;
}
```

A function of the right name without `export` is not an entry point, and the
program does not compile either:

<!-- {"fixture":"spec_worlds_run_not_exported.wado"} -->

```wado
fn run() with Stdout {
    println("unreachable from the host");
}
```

The entry point's signature must match what the world declares.

## `wasi:cli/command`

`wasi:cli/command` is the world of a command-line program. `wado run` compiles a
program for it and runs it once. The `wasi:cli` package declares the world:

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

`Run` declares `async fn run() -> AsyncCall<Result<(), ()>>`. A program
implements it with an `export fn run()`:

<!-- {"fixture":"spec_components_run.wado"} -->

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let greeting = "Hello, WASI world!";
    assert greeting.len() == 18;
    println(greeting);
}
```

`run` may also return the `Result<(), ()>` that the world declares, or be an
`export async fn` that delivers its result with
[`task return`](#task-return-statement).

### Arguments and Environment

The host gives a command its command line and its environment variables.
[`core:cli`](./stdlib-core-cli.md) reads both, and each read requires
`Environment`:

- `args()` is the arguments after the program name.
- `program_name()` is the name the program was started as. `wado run` passes the
  path it was given, never its own name.
- `env(name)` is the value of one variable, or `null` when it is unset.

<!-- {"fixture":"spec_worlds_command_args.wado"} -->

```wado
use { println, args, program_name, env, Stdout, Environment } from "core:cli";

export fn run() with (Stdout, Environment) {
    let name = program_name().unwrap_or("greet");
    let words = args();                          // the program name is not among them
    assert env("SPEC_WORLDS_UNSET") == null;     // an unset variable reads as `null`
    if words.is_empty() {
        println(`usage: ${name} <name>...`);
        return;
    }
    for let word of words {
        println(`hello, ${word}`);
    }
}
```

`wado run` passes everything after the source file to the program as its
arguments.

### Standard Streams

A command has three byte streams. `Stdin` reads standard input, and `Stdout` and
`Stderr` write standard output and standard error. `core:cli` writes text to the
two outputs: `print` and `println` require `Stdout`, and `eprint` and `eprintln`
require `Stderr`.

<!-- {"fixture":"spec_worlds_command_stdio.wado"} -->

```wado
use { print, eprintln, Stdout, Stderr } from "core:cli";
use { Stdin } from "wasi:cli";

export fn run() with (Stdin, Stdout, Stderr) {
    let [input, _done] = Stdin::read_via_stream();
    let bytes = input.read_to_end();
    input.drop();
    let text = String::from_utf8_lossy(bytes);
    assert text.lines().count() == 2;
    print(text.to_ascii_uppercase());
    eprintln(`read ${bytes.len()} bytes`);
}
```

`log_stdout` and `log_stderr` write the same streams from any function, with no
effect declared (see [Ambient Functions](./spec-effects.md#ambient-functions)).

### Exit Status

A command ends with a status, which is success or failure:

- `run` returns normally, or returns `Ok(())`: success.
- `run` returns `Err(())`: failure.
- A call of `exit` ends the program at once. Nothing after it runs.
- A trap ends the program with failure.

<!-- {"fixture":"spec_worlds_command_result.wado"} -->

```wado
fn check(count: i32) -> Result<(), ()> {
    if count < 0 {
        return Err(());
    }
    return Ok(());
}

export fn run() -> Result<(), ()> with Stdout {
    assert check(-1) == Err(());
    println("all checks passed");
    return check(3);
}
```

`core:cli` exits with the `Exit` effect. `exit(code)` ends the program with that
status code, `exit_success()` with success, and `exit_error()` with failure.
Each returns `!`.

<!-- {"fixture":"spec_worlds_command_exit.wado"} -->

```wado
use { println, eprintln, exit, Stdout, Stderr, Exit } from "core:cli";

export fn run() with (Stdout, Stderr, Exit) {
    let port = i32::from_str("eighty");
    assert port matches { Err(_) };
    println("checking the port");
    eprintln("error: the port is not a number");
    exit(2);
    println("never printed");
}
```

A failed `assert`, `panic` and `unreachable` all trap (see
[Unrecoverable Errors](./spec-control-flow.md#unrecoverable-errors-traps)).
What the program wrote before the trap stays written.

<!-- {"fixture":"spec_worlds_command_trap.wado"} -->

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let items: List<i32> = [];
    println("start");
    assert items.len() > 0, "no items to process";
    println("never printed");
}
```

### Filesystem

A command reaches only the directories its host preopened, through the
`Preopens` effect. `wado run` preopens the current directory. When it is given
`--dir` grants, it preopens exactly those instead.

A path opens relative to a preopened directory. An absolute path, or one that
climbs above the directory with `..`, does not open.
[`core:fs`](./stdlib-core-fs.md) resolves every path against the first
preopened directory:

<!-- {"fixture":"spec_worlds_command_files.wado"} -->

```wado
use { println, Stdout } from "core:cli";
use fs from "core:fs";
use { Preopens } from "core:fs";

export fn run() with (Stdout, Preopens) {
    let text = "remember the milk";
    assert fs::write("notes.txt", &text) matches { Ok(_) };
    assert fs::read_to_string("notes.txt").unwrap() == text;
    assert fs::read_to_string("/etc/hostname") matches { Err(_) };   // absolute
    assert fs::read_to_string("../notes.txt") matches { Err(_) };    // above the preopen
    println("notes saved");
}
```

## `wasi:http/service`

`wasi:http/service` is the world of an HTTP server. `wado serve` compiles a
program for it and calls its entry point once for each request. The
`wasi:http` package declares the world:

<!-- {"source":"wado-compiler/lib/wasi/http/worlds.wado"} -->

```wado
#[cm("wasi:http/service@0.3.0")]
pub world Service {
    import Stdout;
    import Stderr;
    import Stdin;
    import Client;
    import MonotonicClock;
    import SystemClock;
    import Timezone;
    import Random;
    import Insecure;
    import InsecureSeed;

    export Handler;
}
```

The entry point is `handle`, which the `Handler` interface declares. A service
defines it as an `export async fn`:

```text
export async fn handle(request: Request) -> Result<Response, ErrorCode>
```

`Request`, `Response`, `ErrorCode` and the other HTTP types come from
`wasi:http`.

Unlike `wasi:cli/command`, the service world does not import `Environment`,
`Exit` or `Preopens`.

### Building a Response

A response is made of three parts:

- Its headers, a `Headers`.
- Its body: `null` for none, or the readable end of a `Stream<u8>`.
- A `Future` that resolves to its trailers.

`Response::new` takes the three and also returns a future that resolves once the
host has sent the body. A response's status is 200 until `set_status_code`
changes it.

The handler delivers the response with `task return`. The host reads the body
and the trailers only once it has the response, and a stream or future write
waits for its reader (see
[Streams and Futures](./spec-components.md#streams-and-futures)). So the
handler writes the body and resolves the trailers after `task return`:

<!-- {"fixture":"spec_worlds_http_response.wado"} -->

```wado
use { Request, Response, ErrorCode, Headers, Trailers, FieldName } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let headers = Headers::new();
    assert headers.append("content-type" as FieldName, b"text/plain") matches { Ok(_) };

    let [body, body_tx] = Stream::<u8>::new();
    let [trailers, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let [response, _sent] = Response::new(headers, Some(body), trailers);
    assert response.set_status_code(201) matches { Ok(_) };

    task return Result::<Response, ErrorCode>::Ok(response);

    body_tx.write_all("created".bytes().collect());
    body_tx.drop();
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

Dropping the body's writable end ends the body. Writing `Ok(null)` to the
trailers future says the response has no trailers.

### Reading the Request

A request carries its method, its path and query, and its headers.
`Request::consume_body` takes the request and returns its body as a
`Stream<u8>`, with a future for its trailers:

<!-- {"fixture":"spec_worlds_http_request.wado"} -->

```wado
use { Request, Response, ErrorCode, Headers, Trailers, FieldName } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let is_post = request.get_method() matches { Post };
    let path = request.get_path_with_query().unwrap_or("/");
    let headers = request.get_headers();
    let greetings = headers.get("x-greeting" as FieldName);
    assert is_post && path == "/greet" && greetings.len() == 1;

    let [done, _done_tx] = Future::<Result<(), ErrorCode>>::new();
    let [request_body, _request_trailers] = Request::consume_body(request, done);
    let name = String::from_utf8_lossy(request_body.read_to_end());
    request_body.drop();

    let [body, body_tx] = Stream::<u8>::new();
    let [trailers, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let [response, _sent] = Response::new(Headers::new(), Some(body), trailers);
    task return Result::<Response, ErrorCode>::Ok(response);

    let greeting = String::from_utf8_lossy(greetings[0]);
    body_tx.write_all(`${greeting}, ${name}`.bytes().collect());
    body_tx.drop();
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

`consume_body` moves the request, so the handler reads the method, path and
headers before it.

### Failing a Request

A handler that cannot answer delivers an `Err(ErrorCode)` instead of a
response. The host then answers the client with an error response. No body or
trailers follow:

<!-- {"fixture":"spec_worlds_http_error.wado"} -->

```wado
use { Request, Response, ErrorCode } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let path = request.get_path_with_query();
    assert path == Some("/broken");
    task return Result::<Response, ErrorCode>::Err(ErrorCode::InternalError(Some("no backend")));
}
```

A program for this world must define `handle`. A `run` does not stand in for
it:

<!-- {"fixture":"spec_worlds_http_missing_handle.wado"} -->

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("not a service");
}
```

## `task return` Statement

`task return expr;` delivers the result of an `export async fn` without ending
the function. It is the Component Model's `task.return`. Execution continues
after it, so the function can fulfill outstanding futures (such as a response's
trailers) or clean up.

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

- `task return` is only valid inside an `export async fn` body.
- An `export async fn` body must carry a `task return`, because a body without
  one could never deliver its result. A body whose every path provably exits
  first (`panic`, an endless loop) has no result to deliver and is exempt.
- Whether a `task return` under a branch is reached is not checked. A path that
  misses it traps.
- A plain `return` is forbidden in an `async fn` body. It would exit the Wasm
  function without telling the Component Model runtime.
- The `task return` expression is type-checked against the declared return type
  of the enclosing `export async fn`.
- `task return` delivers the result to the function's caller. A call through
  the component boundary delivers it to the Component Model runtime, and a Wado
  caller receives it as an ordinary return value.
- The `async` of an `export async fn` asks nothing of a Wado call site, which
  calls it as any other function. It selects the Component Model async calling
  convention at the component boundary.

A `task return` outside an `export async fn` does not compile:

<!-- {"fixture":"error_task_return_non_async.wado"} -->

```wado
export fn run() {
    task return 42;
}
```

Neither does a plain `return` inside one:

<!-- {"fixture":"spec_worlds_http_return_forbidden.wado"} -->

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    return Result::<Response, ErrorCode>::Err(ErrorCode::InternalError(null));
}
```

Nor an `export async fn` that never delivers:

<!-- {"fixture":"error_async_export_without_task_return.wado"} -->

```wado
export async fn run() with Stdout {
    println("ran");
}
```

## The `test` World

`test` is a synthetic world. `wado test` compiles each file for it and runs the
file's `test` blocks. The world exports those blocks and nothing else, so a
file needs no entry point to be tested. A file that has one keeps it as an
ordinary function. [Testing](./spec-testing.md) holds the rules
for `test` blocks.

<!-- {"fixture":"spec_worlds_test.wado"} -->

```wado
use { println, Stdout } from "core:cli";

fn shout(text: String) -> String {
    return text.to_ascii_uppercase();
}

export fn run() with Stdout {
    println(shout("hello"));
}

test "shout upper-cases its argument" {
    assert shout("hi") == "HI";
}

test "run prints without declaring Stdout" {
    run();
    assert shout("") == "";
}
```

A test may perform any effect ([Syntax Rules](./spec-testing.md#syntax-rules)),
so it may call a command's `run` and a service's `handle`. The call returns what the handler's
`task return` delivered:

<!-- {"fixture":"spec_worlds_test_handler.wado"} -->

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    if request.get_path_with_query() != Some("/") {
        task return Result::<Response, ErrorCode>::Err(ErrorCode::HttpRequestDenied);
    } else {
        let [trailers, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
        let [response, _sent] = Response::new(Headers::new(), null, trailers);
        task return Result::<Response, ErrorCode>::Ok(response);
        trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
    }
}

fn request_for(path: String) -> Request {
    let [trailers, _trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let [request, _sent] = Request::new(Headers::new(), null, trailers, null);
    let _ = request.set_path_with_query(Some(path));
    return request;
}

test "only the root is served" {
    assert handle(request_for("/admin")) matches { Err(HttpRequestDenied) };
}
```

A Wado call runs the handler's body to its end, and nothing reads what the body
writes after `task return`. So a test calls a handler only along a path that
writes no body and no trailers. On any other path the write has no reader, and
the call never completes.

## `core:kiln/generator`

`core:kiln/generator` is the world of a Kiln generator, the program that turns a
non-Wado import into Wado source. Its entry point is `export fn generate(...)`.
[Kiln Generators](./spec-kiln.md#authoring-a-generator) holds its rules.

## WASI Interfaces

Wado targets WASI Preview 3 (0.3.0) and no earlier version. Its `stream<T>` and
`future<T>` types map to Wado's `Stream<T>` and `Future<T>`. The command-line
effects map to these WASI interfaces:

| Wado Effect   | WASI Interface             | Key Functions                                                           |
| ------------- | -------------------------- | ----------------------------------------------------------------------- |
| `Stdout`      | `wasi:cli/stdout`          | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stderr`      | `wasi:cli/stderr`          | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stdin`       | `wasi:cli/stdin`           | `read-via-stream() -> tuple<stream<u8>, future<result<_, error-code>>>` |
| `Environment` | `wasi:cli/environment`     | `get-arguments()`, `get-environment()`                                  |
| `Exit`        | `wasi:cli/exit`            | `exit(result)`, `exit-with-code(u8)`                                    |
| `Preopens`    | `wasi:filesystem/preopens` | `get-directories()`                                                     |

Rationale: [WEP: Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md).
