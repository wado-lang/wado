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

Handlers satisfy effects by providing implementations for effect operations. An effect handler is any value whose type implements the effect via `impl Effect for Type`, analogous to trait implementations. The `with Effect = value do { ... }` block installs a handler for the scope of the `do` block.

Only the effects actually needed are required on the calling function:

- The handled effect itself: not required (handler satisfies it)
- Effects used by handler methods: required on the caller

#### Effect as Trait

An effect declaration defines an interface (like a trait). Any struct that implements the effect's operations can serve as a handler:

```wado
effect Stdin {
    fn read_line() -> String;
}

struct MockStdin {
    responses: Array<String>,
    mut index: i32,
}

impl Stdin for MockStdin {
    fn read_line(&mut self) -> String {
        let result = self.responses[self.index];
        self.index += 1;
        resume result
    }
}
```

Handler implementations add `&self` or `&mut self` to access the handler's state. The effect declaration itself has no `self` parameter (effect operations are free functions from the caller's perspective).

#### Using Handlers

```wado
fn test_input() {
    let mut mock = MockStdin { responses: ["hello", "world"], index: 0 };
    with Stdin = &mut mock do {
        let a = Stdin::read_line();  // "hello"
        let b = Stdin::read_line();  // "world"
    }
    assert mock.index == 2;
}
```

Multiple handlers:

```wado
with Stdin = &mut mock_stdin, Stdout = &mut mock_stdout do {
    // ...
}
```

#### Handler Methods with Effects

Handler methods can have their own effect requirements:

```wado
struct LoggingStdin {
    response: String,
}

impl Stdin for LoggingStdin {
    fn read_line(&self) -> String with Stdout {
        println("reading...");
        resume self.response
    }
}

// Caller must have Stdout (handler method's effect), but not Stdin (handled)
fn test_logging() with Stdout {
    let mock = LoggingStdin { response: "mocked" };
    with Stdin = &mock do {
        let line = Stdin::read_line();
    }
}
```

#### Handling Granularity and Wildcard

By default, `impl Effect for Type` must implement all operations of the effect (like a complete trait impl). Use `..` (rest pattern) to opt in to trapping on unimplemented operations:

```wado
struct MinimalTcp;

impl TcpSocket for MinimalTcp {
    fn create(&self, family: IpAddressFamily) -> Result<TcpSocket, ErrorCode> {
        resume Result::Ok(mock_socket())
    }
    fn connect(&self, self_: &TcpSocket, addr: IpSocketAddress) -> Result<(), ErrorCode> {
        resume Result::Ok(())
    }
    ..  // bind, listen, send, receive, etc. — trap if called
}
```

`..` is consistent with struct rest patterns (`let { name, .. } = person`).

#### CM Streams and Futures Are Unbuffered

CM streams and futures are semantically unidirectional **unbuffered** channels (see [CM Concurrency spec](../vendor/component-model/design/mvp/Concurrency.md)). `stream.write` blocks until a concurrent reader consumes the data; `future.write` blocks until a concurrent reader reads the value. The CM runtime does not buffer data between the readable and writable ends.

This means synchronous effect handlers cannot directly use CM streams for data transfer. Consider `println`:

```wado
pub fn println(message: String) with Stdout {
    let [rx, tx] = Stream::<u8>::new();           // 1. stream pair
    let handle = Stdout::write_via_stream(rx);    // 2. handler intercepts here
    write_to_stream(tx, message, true);           // 3. tx.write() — blocks if no reader on rx
    drop_cli_write_future(handle);                // 4. future.drop() — blocks if no writer
}
```

If the handler at step 2 simply stores `rx` and resumes, the caller's `tx.write()` at step 3 blocks waiting for a reader on `rx`. In a synchronous handler, there is no concurrent reader — deadlock.

The real WASI runtime avoids this because `write_via_stream` starts an async task that reads `rx` concurrently. Synchronous mock handlers need a different approach: replace CM's unbuffered streams and futures with buffered in-memory implementations.

#### Buffered CM Handlers

- [ ] Not yet implemented.

`core:test` will provide `MockCM` — a handler that implements `Stream<u8>`, `StreamWritable<u8>`, `Future<T>`, and `FutureWritable<T>` with buffered in-memory semantics. Writes append to a buffer without blocking; reads return buffered data immediately.

```wado
struct StreamBuffer {
    mut data: Array<u8>,
    mut read_pos: i32,
    mut write_closed: bool,
}

struct MockCM {
    mut stream_buffers: Array<StreamBuffer>,
    mut future_count: i32,
}

impl Stream<u8> for MockCM {
    fn new(&mut self) -> [Stream<u8>, StreamWritable<u8>] {
        let id = self.stream_buffers.len();
        self.stream_buffers.push(StreamBuffer { data: [], read_pos: 0, write_closed: false });
        resume [id as Stream<u8>, id as StreamWritable<u8>]
    }

    fn read(&mut self, stream: &Stream<u8>, max: i32) -> Array<u8> {
        let id = *stream as i32;
        let buf = &mut self.stream_buffers[id];
        let available = buf.data.len() - buf.read_pos;
        if available == 0 { resume [] }
        let count = i32::min(max, available);
        let mut result: Array<u8> = [];
        for let mut i = 0; i < count; i += 1 {
            result.push(buf.data[buf.read_pos + i]);
        }
        buf.read_pos += count;
        resume result
    }

    fn drop(&self, stream: &Stream<u8>) { resume () }
    fn cancel_read(&self, stream: &Stream<u8>) { resume () }
}

impl StreamWritable<u8> for MockCM {
    fn write(&mut self, writable: &StreamWritable<u8>, data: Array<u8>) {
        let id = *writable as i32;
        self.stream_buffers[id].data.extend(data);
        resume ()  // buffered — never blocks
    }

    fn write_raw(&mut self, writable: &StreamWritable<u8>, data: builtin::array<u8>, len: i32) {
        let id = *writable as i32;
        let buf = &mut self.stream_buffers[id];
        for let mut i = 0; i < len; i += 1 {
            buf.data.push(builtin::array_get_u8(data, i));
        }
        resume ()
    }

    fn drop(&mut self, writable: &StreamWritable<u8>) {
        let id = *writable as i32;
        self.stream_buffers[id].write_closed = true;
        resume ()
    }

    fn cancel_write(&self, writable: &StreamWritable<u8>) { resume () }
}
```

Future and FutureWritable use type-erased storage to handle generic `Future<T>`:

```wado
impl<T> Future<T> for MockCM {
    fn new(&mut self) -> [Future<T>, FutureWritable<T>] {
        let id = self.future_count;
        self.future_count += 1;
        resume [id as Future<T>, id as FutureWritable<T>]
    }

    fn read(&self, f: &Future<T>) -> Option<T> {
        // type-erased lookup; returns stored value if written
        ..
    }

    fn drop(&self, f: &Future<T>) { resume () }
    fn cancel_read(&self, f: &Future<T>) { resume () }
}

impl<T> FutureWritable<T> for MockCM {
    fn write(&mut self, fw: &FutureWritable<T>, value: T) {
        // store value (type-erased) for later read
        resume ()
    }

    fn drop(&self, fw: &FutureWritable<T>) { resume () }
    fn cancel_write(&self, fw: &FutureWritable<T>) { resume () }
}
```

#### Handler Bundling

- [ ] Not yet implemented.

When a type implements multiple effects, listing each one in `with` is verbose. If the effect name is omitted, the `with` block handles all effects the type implements:

```wado
// Explicit: list each effect separately
with Stream<u8> = &mut cm, StreamWritable<u8> = &mut cm,
     Future<T> = &mut cm, FutureWritable<T> = &mut cm do { ... }

// Bundled: handle all effects MockCM implements
with &mut cm do { ... }
```

Multiple handlers compose naturally:

```wado
with &mut cm, Stdout = &mut stdout, Client = &mut client do {
    run();
}
```

This follows wasmtime's pattern where a single `WasiState` struct implements multiple `*View` traits and is registered with one `add_to_linker` call.

#### Handlers for Testing

Effect handlers enable testing code that uses WASI effects without a real WASI runtime. Test functions implicitly have all effects, so handlers can provide any effect. `core:test::MockCM` provides buffered CM canonical handlers as a foundation.

##### Stdout Handler Example

MockStdout stores stream handles from each `write_via_stream` call. Because streams go through `MockCM` (buffered), the caller's `tx.write()` succeeds without blocking. After the `do` block, `drain()` reads buffered data from the stored stream handles:

```wado
struct MockStdout {
    mut streams: Array<Stream<u8>>,
}

impl Stdout for MockStdout {
    fn write_via_stream(&mut self, data: Stream<u8>) -> Future<Result<(), ErrorCode>> {
        self.streams.push(data);
        let [f, ftx] = Future::<Result<(), ErrorCode>>::new();
        ftx.write(Result::<(), ErrorCode>::Ok(()));
        ftx.drop();
        resume f  // no post-resume → compiles to return
    }
}

impl MockStdout {
    fn drain(&mut self) -> String {
        let mut result = String::with_capacity(256);
        for let stream of self.streams {
            loop {
                let chunk = stream.read(4096);
                if chunk.is_empty() { break; }
                result.push_str(String::from_utf8(chunk));
            }
            stream.drop();
        }
        self.streams = [];
        return result;
    }
}

test "println captures output" {
    let mut cm = MockCM::new();
    let mut stdout = MockStdout { streams: [] };
    with &mut cm, Stdout = &mut stdout do {
        println("hello");
        println("world");
        // drain() must be called inside MockCM scope (fake handles are only valid here)
        let output = stdout.drain();
        assert output == "hello\nworld\n";
    }
}
```

Execution flow:

```
println("hello"):
  Stream::<u8>::new()            → MockCM: creates buffer #0, returns fake handles
  Stdout::write_via_stream(rx)  → MockStdout: stores rx, creates fake Future, resumes
  tx.write_raw(bytes, len)      → MockCM: appends to buffer #0 (no block)
  tx.drop()                     → MockCM: marks buffer #0 as write-closed
  future.drop()                 → MockCM: no-op

stdout.drain():
  stream.read(4096)             → MockCM: reads from buffer #0 (immediate)
  stream.drop()                 → MockCM: no-op
```

##### HTTP Client Handler Example

Testing code that calls `Client::send` (e.g., `example/http-get.wado`). The mock constructs a Response with body data pre-written to a buffered stream — this is safe because `MockCM` streams are buffered, so `body_tx.write()` succeeds immediately without a concurrent reader:

```wado
struct MockClient {
    mut requests: Array<String>,
    response_body: String,
    status: StatusCode,
}

impl Client for MockClient {
    fn send(&mut self, request: Request) -> Result<Response, ErrorCode> {
        if let Some(path) = request.get_path_with_query() {
            self.requests.push(path);
        }

        let headers = Fields::new();  // forwards to outer scope
        let [trailers_rx, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
        let [body_rx, body_tx] = Stream::<u8>::new();  // → MockCM (buffered)

        body_tx.write(self.response_body.bytes().collect());  // buffered — no block
        body_tx.drop();
        trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
        trailers_tx.drop();

        let [resp, _] = Response::new(headers, Option::Some(body_rx), trailers_rx);
        resp.set_status_code(self.status);

        resume Result::<Response, ErrorCode>::Ok(resp)  // no post-resume
    }
    ..
}

test "http-get fetches and prints" {
    let mut cm = MockCM::new();
    let mut stdout = MockStdout { streams: [] };
    let mut client = MockClient {
        requests: [],
        response_body: `{"origin": "127.0.0.1"}`,
        status: 200,
    };
    with &mut cm, Stdout = &mut stdout, Client = &mut client do {
        run();  // example/http-get.wado's export fn run()
        assert client.requests[0] == "/get";
        let output = stdout.drain();
        assert output contains "Status: 200";
    }
}
```

Note: `Fields::new()`, `Response::new()` etc. are HTTP resource operations that forward to the outer scope. This test requires a world that imports `wasi:http` types (e.g., `wasi:http/service`), or additional handlers for those resources.

##### HTTP Server Middleware Example (Post-Resume)

A timing middleware uses post-resume to measure request processing time. The handler delegates to the outer `Handler` implementation via effect forwarding, resumes the response to the caller, then records metrics:

```wado
struct TimingMiddleware {
    mut log: Array<[String, u64]>,
}

impl Handler for TimingMiddleware {
    fn handle(&mut self, request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or("?");
        let start = MonotonicClock::now();
        let resp = Handler::handle(request);  // delegates to outer scope
        resume resp;
        // Post-resume (Stack Switching): runs after do block completes
        let elapsed = MonotonicClock::now() - start;
        self.log.push([path, elapsed]);
    }
    ..
}
```

Testing with MockHandler as the downstream:

```wado
struct MockHandler {
    status: StatusCode,
    body: String,
}

impl Handler for MockHandler {
    fn handle(&self, request: Request) -> Result<Response, ErrorCode> {
        let headers = Fields::new();
        let [trailers_rx, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
        let [body_rx, body_tx] = Stream::<u8>::new();
        body_tx.write(self.body.bytes().collect());
        body_tx.drop();
        trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
        trailers_tx.drop();
        let [resp, _] = Response::new(headers, Option::Some(body_rx), trailers_rx);
        resp.set_status_code(self.status);
        resume Result::<Response, ErrorCode>::Ok(resp)
    }
    ..
}

test "timing middleware records elapsed time" {
    let mut cm = MockCM::new();
    let downstream = MockHandler { status: 200, body: "ok" };
    let mut timing = TimingMiddleware { log: [] };
    with &mut cm do {
        with Handler = &downstream do {
            with Handler = &mut timing do {
                let req = create_test_request("/api");
                let resp = Handler::handle(req);
                assert resp matches { Ok(_) };
            }
        }
    }
    assert timing.log.len() == 1;
    assert timing.log[0].0 == "/api";
}
```

Handler nesting: inner `TimingMiddleware` intercepts `Handler::handle`, delegates to the outer `MockHandler` via effect forwarding, and records timing in post-resume.

### Effect Forwarding

Handlers only handle the effects they declare. All other effects forward to the outer scope. This follows the universal pattern in algebraic effect systems (Koka, Eff, OCaml 5, Effekt).

```wado
let mock = MockClient;
with Client = &mock do {
    let headers = Fields::new();    // Fields is not handled → forwards to outer scope
    let req = Request::new(...);    // Request is not handled → forwards to outer scope
    let resp = Client::send(req);   // Client IS handled → goes to MockClient
}
```

Handler method bodies execute in the outer effect scope. This means:

- A handler for effect E can call E's operations in its body to delegate to the outer implementation (no infinite recursion).
- A handler for effect E can use other effects that are available in the outer scope.

```wado
struct CachingClient {
    cache: &mut TreeMap<String, Response>,
}

impl Client for CachingClient {
    fn send(&mut self, request: Request) -> Result<Response, ErrorCode> {
        let key = request.get_path_with_query();
        if let Some(cached) = self.cache.get(key) {
            resume Result::Ok(cached)
        }
        let resp = Client::send(request);  // Client — outer scope (real impl)
        self.cache[key] = resp;
        resume resp
    }
    ..
}

export fn run() with Stdout, Client {
    let mut cache = TreeMap::<String, Response>::new();
    with Client = &mut CachingClient { cache: &mut cache } do {
        app();
    }
}
```

### Handler Nesting

Handlers nest naturally. Inner handlers override specific effects; unhandled effects forward through the chain to the outermost scope:

```wado
let mut mock_stdout = MockStdout { captured: [] };
let mock_client = MockClient;
with Stdout = &mut mock_stdout do {
    with Client = &mock_client do {
        println("sending...");   // Stdout → MockStdout (outer handler)
        Client::send(req);       // Client → MockClient (inner handler)
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

For the test world, the runtime provides a minimal set (Stdout, Stderr, CM builtins). Effects not imported by the test world (e.g., Client, Fields, Request from `wasi:http`) must be provided by user handlers.

### Resume Keyword

`resume` is a control flow expression similar to `return`. It passes a value to the computation and transfers control. The expression `resume` itself evaluates to `()`.

```wado
impl Stdin for MockStdin {
    fn read_line(&self) -> String {
        resume "value"
    }
}
```

For post-processing (one-shot continuations):

```wado
impl FileSystem for ManagedFs {
    fn open_file(&self, path: String) -> Handle {
        let handle = real_open(path);
        resume handle;
        real_close(handle);  // runs after do block completes
    }
}
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
- Effects are traits: any type implementing `impl Effect for Type` can serve as a handler
- No `handler` keyword needed; handlers are ordinary values with effect implementations
- Mutable state in handlers is natural: struct fields accessed via `&self` / `&mut self`
- Handlers satisfy effects locally; unhandled effects forward to the outer scope
- Handler bodies execute in the outer effect scope, enabling delegation to real implementations
- World imports are the outermost handler scope; user handlers nest inside
- `..` wildcard enables partial handling with runtime trap for unimplemented operations
- `resume` without post-processing compiles to `return`; post-processing requires Stack Switching
- One-shot semantics ensure resource safety
- Generic effects (`<effect E>`) support higher-order functions without effect polymorphism complexity
- No existing language has signature-based effect propagation; this is a novel design
- CM streams and futures are unbuffered — synchronous handlers need `MockCM` (buffered CM handlers) for data transfer
- Handler bundling (`with &mut value do`) reduces boilerplate when a type implements multiple effects
- `core:test::MockCM` provides standard buffered Stream/Future handlers as a foundation for all test mocks
