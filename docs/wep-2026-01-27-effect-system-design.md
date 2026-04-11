# Effect System Design

Status: Draft

## Context

Wado tracks side effects through an effect system. This WEP defines the syntax and semantics for effect declarations, effect checking, effect handlers, and the relationship between resource types and effects.

## Decision

### Effect Declaration

Effects must be explicitly declared on functions. No inference.

```wado
fn greet(name: String) with Stdout {
    println(`Hello, {name}!`);
}

fn pure_add(a: i32, b: i32) -> i32 {
    return a + b;  // no effects
}
```

Multiple effects use comma separation:

```wado
fn process() with Stdout, Stderr, FileSystem {
    // ...
}
```

### Effect Checking

Calling a function requires its effects. Violations are compile errors.

```wado
fn caller() with Stdout {
    greet("Alice");  // OK: caller has Stdout
}

fn bad() {
    greet("Bob");  // ERROR: missing Stdout effect
}
```

### Ambient Effects

`log_stdout` and `log_stderr` from `core:internal` are effect-less by compiler magic. They can be called from any function without effect declaration.

### Generic Effects

Use `<effect E>` to declare a generic effect parameter. `E` can represent multiple effects.

```wado
fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn map<T, U, effect E>(arr: Array<T>, f: fn(T) -> U with E) -> Array<U> with E {
    // ...
}
```

Effects are types. No bounds needed.

### Closure Types

Closures require explicit effect annotation:

```wado
let f: fn(i32) -> i32 with Stdout = |x| {
    println(`{x}`);
    return x;
};
```

### Test Functions

Test functions implicitly have generic effects:

```wado
// Equivalent to: test<effect E> "name" with E { ... }
test "can use any effect" {
    println("stdout");
    eprintln("stderr");
}
```

### Non-Effects

`panic` and `unreachable` are not effects. They have return type `!` (never).

```wado
fn safe_div(a: i32, b: i32) -> i32 {
    if b == 0 { panic("division by zero"); }
    return a / b;
}
```

### Global State Effects

Mutable global variables (`global mut`) implicitly generate an effect. Accessing them requires declaring the effect with `with`.

```wado
global PI: f64 = 3.14159;        // immutable, no effect
global mut counter: i32 = 0;     // mutable, generates effect

// Pure function - no effect needed
fn circle_area(r: f64) -> f64 {
    return PI * r * r;  // OK: immutable global is a constant
}

// Requires effect declaration
fn increment() with counter {
    counter += 1;
}

fn get_count() with counter -> i32 {
    return counter;  // reading mutable global also requires effect
}

fn reset_and_print() with counter, Stdout {
    counter = 0;
    println(`Counter reset`);
}
```

| Declaration             | Read     | Write           | Effect |
| ----------------------- | -------- | --------------- | ------ |
| `global X: T = ...`     | OK       | N/A (immutable) | None   |
| `global mut X: T = ...` | `with X` | `with X`        | Yes    |

This design follows Koka's approach where state effects are tracked, but uses simpler syntax. The `with counter` declaration is sufficient; no separate get/set functions are generated internally.

See also: [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md) for how effects relate to WIT interfaces.

### Resource Types as Effects

- [ ] Not yet implemented.

Resource types (`resource`) are capabilities. Every operation on a resource (constructors, methods, statics) is a host call that requires the host to provide the implementation. Therefore, resource types are effects: using any operation on a resource type requires that the resource is available in the current effect scope.

```wado
// TcpSocket is a resource — using it requires the TcpSocket effect
fn connect(addr: IpSocketAddress) with TcpSocket {
    let socket = TcpSocket::create(IpAddressFamily::Ipv4);  // TcpSocket effect
    socket.bind(addr);    // TcpSocket effect
    socket.connect(addr); // TcpSocket effect
}
```

This applies uniformly to all resource types:

| Resource     | Origin            | Example operation                    |
| ------------ | ----------------- | ------------------------------------ |
| `TcpSocket`  | `wasi:sockets`    | `TcpSocket::create(family)`          |
| `UdpSocket`  | `wasi:sockets`    | `UdpSocket::create(family)`          |
| `Descriptor` | `wasi:filesystem` | `descriptor.read_via_stream(offset)` |
| `Fields`     | `wasi:http`       | `Fields::new()`                      |
| `Request`    | `wasi:http`       | `Request::new(headers, ...)`         |
| `Response`   | `wasi:http`       | `Response::new(headers, ...)`        |
| `Stream<T>`  | `core:prelude`    | `Stream::<u8>::new()`                |
| `Future<T>`  | `core:prelude`    | `Future::<T>::new()`                 |

Note: `Stream` and `Future` are CM canonical builtins, but their operations (`stream.new`, `stream.read`, etc.) are still host syscalls. There is no special-casing — all resources follow the same rule.

### Effect Propagation

- [ ] Not yet implemented. Depends on resource-as-effect.

When an effect operation's signature contains resource types, those resource effects are automatically available to the caller. This propagation is transitive.

The rule: if a resource type `R` appears in any parameter type or return type of an effect's operations, then `with Effect` implicitly grants `with R`. Recursively, if `R`'s operations mention another resource type `S`, then `S` is also granted.

No existing language has this mechanism. The closest precedents are Koka's effect aliases (manual grouping) and Rust's supertraits (`trait Ord: Eq`). Effect propagation is an automatic, signature-derived form of supertrait.

Example: `Stdout` has a single operation:

```wado
pub effect Stdout {
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}
```

`Stream` and `Future` appear in the signature. Their operations mention `StreamWritable` and `FutureWritable` respectively. So:

```
with Stdout
  → Stream, Future           (direct: appear in write_via_stream signature)
    → StreamWritable          (transitive: Stream::new() returns StreamWritable)
    → FutureWritable          (transitive: Future::new() returns FutureWritable)
```

This means `println` only needs `with Stdout`:

```wado
pub fn println(message: String) with Stdout {
    let [rx, tx] = Stream::<u8>::new();      // Stream, StreamWritable — propagated
    let handle = Stdout::write_via_stream(rx); // Stdout operation, returns Future — propagated
    write_to_stream(tx, message, true);
    drop_cli_write_future(handle);             // FutureWritable — propagated
}
```

More propagation chains:

```
with Client                    (wasi:http)
  → Request, Response          (direct: send(Request) -> Result<Response, ...>)
    → Fields, RequestOptions   (transitive: Request::new(Headers, ..., RequestOptions))
    → Stream, Future           (transitive: Request::new(..., Stream<u8>, Future<...>))
      → StreamWritable         (transitive²)
      → FutureWritable         (transitive²)

with TcpSocket                 (wasi:sockets)
  → Stream, Future             (direct: send(Stream<u8>) -> Future<...>)
    → StreamWritable           (transitive)
    → FutureWritable           (transitive)
```

Effects without resource types in their signatures propagate nothing:

```
with Environment  → (nothing)   // get_environment() -> Array<[String, String]>
with Random       → (nothing)   // get_random_bytes(u64) -> Array<u8>
with Exit         → (nothing)   // exit(Result<(), ()>)
```

Only resource types (`resource` keyword) trigger propagation. Structs, enums, variants, and primitives do not.

### Handlers

- [ ] Not yet implemented.

Handlers satisfy effects. Inside a `with ... do` block, the handled effect is provided by the handler, not required from the caller.

Only the effects actually needed are required on the calling function:

- The handled effect itself: not required (handler satisfies it)
- Effects used by handler methods: required on the caller

#### Named Handler

```wado
handler MockStdin for Stdin {
    fn read_line() -> String {
        resume "mocked input"
    }
}

fn test_input() {
    with Stdin = MockStdin do {
        let line = Stdin::read_line();
        assert line == "mocked input";
    }
}
```

Multiple handlers:

```wado
with Stdin = MockStdin, Stdout = MockStdout do {
    // ...
}
```

Handler methods can have their own effect requirements:

```wado
handler LoggingStdin for Stdin {
    fn read_line() -> String with Stdout {
        println("reading...");
        resume "mocked"
    }
}

// Caller must have Stdout (handler method's effect), but not Stdin (handled)
fn test_logging() with Stdout {
    with Stdin = LoggingStdin do {
        let line = Stdin::read_line();
    }
}
```

#### Inline Handler

Uses the same method definition style as named handlers:

```wado
with Stdin as {
    fn read_line() -> String {
        resume "simple mock"
    }
} do {
    let line = Stdin::read_line();
}
```

With arguments and complex logic:

```wado
with FileSystem as {
    fn read_file(path: String) -> String {
        resume `contents of {path}`
    }
    fn write_file(path: String, data: String) {
        log.push([path, data]);
        resume;
    }
} do {
    let content = FileSystem::read_file("test.txt");
}
```

#### Mixed

Named and inline handlers can be combined:

```wado
with Stdin = MockStdin, Stdout as {
    fn write(s: String) {
        captured.push(s);
        resume;
    }
} do {
    // ...
}
```

Inline handler methods can also have effect requirements:

```wado
fn test_with_logging() with Stderr {
    with Stdin as {
        fn read_line() -> String with Stderr {
            eprintln("debug: reading");
            resume "mocked"
        }
    } do {
        let line = Stdin::read_line();
    }
}
```

#### Handling Granularity and Wildcard

By default, a handler must implement all operations of the handled effect. Use `..` (rest pattern) to opt in to trapping on unimplemented operations:

```wado
// All operations required — compile error if any is missing
with TcpSocket as {
    fn create(family: IpAddressFamily) -> Result<TcpSocket, ErrorCode> { ... }
    fn bind(self: &TcpSocket, addr: IpSocketAddress) -> Result<(), ErrorCode> { ... }
    fn connect(self: &TcpSocket, addr: IpSocketAddress) -> Result<(), ErrorCode> { ... }
    // ... all 20+ methods
} do { ... }

// With wildcard — only implement what you need, rest traps at runtime
with TcpSocket as {
    fn create(family: IpAddressFamily) -> Result<TcpSocket, ErrorCode> {
        resume Result::Ok(mock_socket())
    }
    fn connect(self: &TcpSocket, addr: IpSocketAddress) -> Result<(), ErrorCode> {
        resume Result::Ok(())
    }
    ..  // unimplemented operations trap if called
} do { ... }
```

`..` is consistent with struct rest patterns (`let { name, .. } = person`).

#### Mutable State in Handlers

Inline handlers capture outer variables with the same semantics as closures:

```wado
test "capture output" {
    let mut captured: Array<String> = [];
    with Stdout as {
        fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>> {
            let bytes = data.read(65536);
            captured.push(String::from_utf8(bytes));  // captures &mut captured
            let [f, tx] = Future::<Result<(), ErrorCode>>::new();
            tx.write(Result::<(), ErrorCode>::Ok(()));
            resume f
        }
        ..
    } do {
        println("hello");
    }
    assert captured[0] == "hello\n";
}
```

Named handler mutable state semantics are deferred to a future revision.

### Effect Forwarding

Handlers only handle the effects they declare. All other effects forward to the outer scope. This follows the universal pattern in algebraic effect systems (Koka, Eff, OCaml 5, Effekt).

```wado
with Client as {
    fn send(request: Request) -> Result<Response, ErrorCode> {
        resume Result::Ok(mock_response())
    }
    ..
} do {
    let headers = Fields::new();    // Fields is not handled → forwards to outer scope
    let req = Request::new(...);    // Request is not handled → forwards to outer scope
    let resp = Client::send(req);   // Client IS handled → goes to inline handler
}
```

Handler bodies execute in the outer effect scope. This means:

- A handler for effect E can call E's operations in its body to delegate to the outer implementation (no infinite recursion).
- A handler for effect E can use other effects that are available in the outer scope.

```wado
export fn run() with Stdout, Client {
    with Client as {
        fn send(request: Request) -> Result<Response, ErrorCode> {
            println("intercepted!");         // Stdout — outer scope
            let resp = Client::send(request); // Client — outer scope (real impl)
            resume resp
        }
        ..
    } do {
        app();  // Client::send() here goes to the handler above
    }
}
```

### Handler Nesting

Handlers nest naturally. Inner handlers override specific effects; unhandled effects forward through the chain to the outermost scope:

```wado
with Stdout = MockStdout do {
    with Client as {
        fn send(request: Request) -> Result<Response, ErrorCode> {
            println("sending...");  // Stdout → MockStdout (outer handler)
            resume mock_response()
        }
        ..
    } do {
        println("before");      // Stdout → MockStdout
        Client::send(req);      // Client → inline handler
    }
}
```

### World Imports as the Outermost Handler

A world's imports define the outermost handler scope. The runtime (wasmtime) provides the real implementations for all imported effects. A `with ... do` block creates a nested handler that overrides specific effects within its scope.

Conceptually, compiling for `wasi:cli/command`:

```
wasmtime (outermost handler)
  ├─ Stdout     = WASI stdout implementation
  ├─ Stderr     = WASI stderr implementation
  ├─ TcpSocket  = WASI socket implementation
  ├─ Stream     = CM canonical runtime
  ├─ Future     = CM canonical runtime
  └─ ...all world imports...

  do {
      run()   ← user's export fn
  }
```

For the test world, the runtime provides a minimal set (Stdout, Stderr, CM builtins). Effects not imported by the test world (e.g., Client, Fields, Request from `wasi:http`) must be provided by user handlers:

```wado
test "http handler" {
    // Client, Fields, Request, Response are not in test world imports
    // → handlers required for each
    with Client as {
        fn send(request: Request) -> Result<Response, ErrorCode> {
            resume Result::Ok(mock_response())
        }
        ..
    },
    Fields as { fn new() { resume mock_fields() }; .. },
    Request as { fn new(...) { resume mock_request() }; .. },
    Response as { fn new(...) { resume mock_response() }; .. }
    do {
        // Stream, Future → forward to test world runtime (CM builtins)
        // Client → handled by inline handler
        // Fields, Request, Response → handled by inline handlers
        let resp = Client::send(make_request());
    }
}
```

Standard library test handlers (e.g., `core:test`) will be provided to reduce this boilerplate.

### Resume Keyword

`resume` is a control flow expression similar to `return`. It passes a value to the computation and transfers control. The expression `resume` itself evaluates to `()`.

```wado
with Stdin as {
    fn read_line() -> String {
        resume "value"
    }
} do { ... }
```

For post-processing (one-shot continuations):

```wado
with FileSystem as {
    fn open_file(path: String) -> Handle {
        let handle = real_open(path);
        resume handle;
        real_close(handle);  // runs after do block completes
    }
} do { ... }
```

### Continuation Semantics and Execution Model

One-shot only. Each `resume` executes at most once. Multi-shot continuations are a future consideration pending Wasm Stack Switching support.

Execution model depends on whether post-resume code exists:

| Pattern        | Example                                | Implementation                |
| -------------- | -------------------------------------- | ----------------------------- |
| No post-resume | `fn op() { resume value }`             | `resume` compiles to `return` |
| Post-resume    | `fn op() { resume value; cleanup(); }` | Wasm Stack Switching          |

Most handlers (test mocks, DI) have no post-resume code and use the `return` optimization. Post-resume handlers (resource cleanup, generators) require Wasm Stack Switching, which is available on amd64 in wasmtime.

### Relation to `stores`

The `stores` annotation shares syntax with effects:

```wado
fn register(data: &Data) -> Handle with Stdout, stores[data] {
    // ...
}
```

## Consequences

- All function effects are explicit and checked at compile time
- Effect violations produce clear compile errors
- Resource types are effects: every resource operation requires the resource to be in scope
- Effect propagation eliminates verbosity: `with Stdout` automatically grants `Stream`, `Future`, etc.
- Handlers satisfy effects locally; unhandled effects forward to the outer scope
- Handler bodies execute in the outer effect scope, enabling delegation to real implementations
- World imports are the outermost handler scope; user handlers nest inside
- `..` wildcard enables partial handling with runtime trap for unimplemented operations
- Inline handlers capture mutable state with closure semantics
- `resume` without post-processing compiles to `return`; post-processing requires Stack Switching
- One-shot semantics ensure resource safety
- Generic effects (`<effect E>`) support higher-order functions without effect polymorphism complexity
- No existing language has signature-based effect propagation; this is a novel design
