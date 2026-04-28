# Effect Handler

Status: Stable

## Implementation Status

The full dispatch protocol — front-end, resolver/effect-check, synthesis pass,
codegen — is shipping. Detailed development history is in the git log; this
section records only the current state.

- [x] Front-end: `with E => h do { ... }`, `resume value`, and `..` rest in
      `impl Effect for Type`.
- [x] Resolver / effect-check: `TirExprKind::WithHandler` and
      `TirExprKind::Resume` carry the structure through TIR. The resolver
      validates effect-decl reference, handler-impls-effect relationship,
      and `resume`-only-in-handler-method. Effect-check skips handled
      effects from caller requirements while walking the body.
- [x] Dispatch synthesis (`synthesis/effect_dispatch.rs`) emits, per effect:
      a `__Dispatch_<E>` Wasm GC struct (recursive `outer: Option<&Self>` +
      one `fn(args) -> ret` closure field per operation), an
      `__effect_<E>: Option<&__Dispatch_<E>>` mut global initialised to
      `null`, and an `__effect_dispatch__<E>__<op>` wrapper per operation.
      `WithHandler` lowers to a desugared block that saves the global,
      builds closures capturing the handler, populates the dispatch
      struct, installs the global, runs the body, and restores on exit.
      Every `<E>::<op>` call site is rewritten to call the wrapper, so
      installed handlers are observed at any call-stack depth.
- [x] Cross-function-boundary dispatch. Calls to effect operations from
      helper functions invoked inside `with` reach the installed handler
      via the global. The wrapper restores `outer` before invoking the
      handler closure, so a handler method calling its own effect's
      operation reaches the outer handler chain rather than recursing
      through itself. Unimplemented operations (`..` rest) get a trapping
      stub closure.
      Fixtures: `effect_handler_cross_function.wado`,
      `effect_handler_nested_same.wado`, `effect_handler_self_delegation.wado`.
- [x] Early-exit restore. `return v` / `break L: v` / `continue` /
      label-less `break` that cross out of a `with` body splice the
      restore sequence in front of the jump (`RestoreInjector` TIR
      walker). Value-carrying jumps evaluate the value under the
      still-installed handler before restoring. Jumps to labels declared
      inside the do-block body are skipped; closures are not descended
      into. Fixtures: `effect_handler_early_return.wado`,
      `effect_handler_break_label_cross.wado`,
      `effect_handler_continue_cross.wado`,
      `effect_handler_inner_break.wado` (negative).
- [x] Bundled handlers (`with &mut h do`, `with h do`). The resolver
      expands a bundled binding into one `TirHandlerBinding` per effect
      that the handler's type implements. Bindings install in source
      order; a later binding wins when the same effect appears twice on
      a `with` line, with the earlier binding reachable via the outer
      chain. All bindings from one bundled clause share a synthesised
      `__h_<bundle>` local (`TirHandlerBinding.bundle_group`), so the
      handler expression evaluates exactly once and mutations through
      any installed effect are observed by the rest — the contract that
      makes value-form `with h do` work. Diagnostics:
      `BundledHandlerImplementsNoEffect`,
      `BundledHandlerUnsupportedHandlerType` (e.g. type parameters,
      associated-type projections; user is directed to `with E => h do`).
      Fixtures: `effect_handler_bundled.wado`,
      `effect_handler_bundled_value.wado`,
      `effect_handler_bundled_mixed.wado`,
      `effect_handler_bundled_no_effect.wado` (negative),
      `effect_handler_bundled_type_param_rejected.wado` (negative).
- [x] Resource handler dispatch. Resources participate in the dispatch
      protocol identically to effects: `with R => h do` installs a
      handler whose `impl R for T` methods are one-shot handler bodies
      (`resume` is valid), `R::<op>(args)` and `r.<op>(args)` call sites
      route through the dispatch wrapper, and the handler body sees the
      outer scope so `R::<op>` from inside the impl delegates to the
      wasmtime-provided implementation.
      `synthesis::effect_dispatch::build_effect_index` walks both
      `module.effects` and `module.resources` into one
      `EffectKey -> EffectMeta` index; `EffectMeta.is_resource` keeps
      the wrapper's `effects: vec![]` empty for resources (matching the
      cm_binding adapter — resources are not effects).
      Fixture: `effect_handler_resource_fields.wado` (counting handler
      for `wasi:http`'s `Fields` with self-delegation to real WASI).

- [x] Generic-resource handler dispatch (`Stream<T>`,
      `StreamWritable<T>`, `Future<T>`, `FutureWritable<T>`).
      The dispatch synthesis runs in two halves around the cm_binding
      pass: `synthesize_pre_cm_binding` (wrappers, globals, structs,
      call-site rewriting) before `cm_binding`, and
      `synthesize_post_check` (`WithHandler` desugaring) after
      `effect_check`. Per `(module, base, type_args)` instantiation
      key, the synthesis substitutes the resource's operation template
      and emits a distinct `__Dispatch_Stream<u8>` struct +
      `__effect_Stream<u8>` global + `__effect_dispatch__Stream<u8>__op`
      wrapper triple. `&self` / `&mut self` shorthand on impl methods is
      synthesised as a `TirEffectOp` receiver at index 0
      (`Self = GenericResource { name, module, type_args }`), and call
      sites — both static (`Stream::<u8>::new()`) and instance
      (`tx.write(payload)`) — are rewritten through the matching
      wrapper. The wrapper's no-handler else-branch emits the same
      `MethodCall`/`Call { cm_name }` shape that user code emits, with
      `MonomorphInfo { impl_type_args }` for static methods, so
      cm_binding rewrites both paths uniformly.
      Fixtures: `effect_handler_resource_stream.wado` (`MockStream`
      implementing both ends, round-tripping bytes through a buffer),
      `effect_handler_resource_future.wado` (`MockFuture` round-tripping
      a value through a stored slot).

- [ ] `core:test::MockCM` and handler-bundling helpers (e.g.
      `MockStdout::drain()`). With generic-resource dispatch in place,
      packaging the buffered Stream/Future handlers from the
      [Buffered CM Handlers (MockCM)](#buffered-cm-handlers-mockcm)
      section into a reusable `core:test` fixture is the remaining work.

## Context

[WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md) defines how Wado tracks side effects. This WEP defines how effect handlers provide implementations for effects, enabling dependency injection, testing, and middleware patterns.

Effects are capabilities required by functions. The runtime (wasmtime) provides real implementations for world-imported effects. Effect handlers let user code substitute custom implementations, analogous to how wasmtime's `add_to_linker` registers host implementations for WIT imports.

### Design Goals

- Effects are traits: handlers are ordinary values with `impl Effect for Type`
- No `handler` keyword; handlers are struct instances
- Mutable state is natural via `&self` / `&mut self`
- `resume` is control flow (like `return`), not a special built-in
- CM stream/future unbuffered semantics are addressed by `MockCM`
- Handler bundling reduces boilerplate for types implementing multiple effects

## Decision

### Effect as Trait

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

### Using Handlers

The `with Effect => value do { ... }` block installs a handler for the scope of the `do` block. The `=>` arrow reads as a dispatch binding ("calls to `Effect` go to `value`"); it mirrors the match-arm arrow and is deliberately not `=`, since the operation pushes a handler onto a per-effect stack rather than performing assignment.

```wado
fn test_input() {
    let mut mock = MockStdin { responses: ["hello", "world"], index: 0 };
    with Stdin => &mut mock do {
        let a = Stdin::read_line();  // "hello"
        let b = Stdin::read_line();  // "world"
    }
    assert mock.index == 2;
}
```

Multiple handlers:

```wado
with Stdin => &mut mock_stdin, Stdout => &mut mock_stdout do {
    // ...
}
```

Only the effects actually needed are required on the calling function:

- The handled effect itself: not required (handler satisfies it)
- Effects used by handler methods: required on the caller

### Handler Methods with Effects

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
    with Stdin => &mock do {
        let line = Stdin::read_line();
    }
}
```

### Handling Granularity and Wildcard

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

### Effect Forwarding

Handlers only handle the effects they declare. All other effects forward to the outer scope. This follows the universal pattern in algebraic effect systems (Koka, Eff, OCaml 5, Effekt).

```wado
let mock = MockClient;
with Client => &mock do {
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
    with Client => &mut CachingClient { cache: &mut cache } do {
        app();
    }
}
```

### Handler Nesting

Handlers nest naturally. Inner handlers override specific effects; unhandled effects forward through the chain to the outermost scope:

```wado
let mut mock_stdout = MockStdout { captured: [] };
let mock_client = MockClient;
with Stdout => &mut mock_stdout do {
    with Client => &mock_client do {
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

### CM Streams and Futures Are Unbuffered

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

### Buffered CM Handlers (MockCM)

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

Future and FutureWritable use `&T` references for type-erased storage (GC keeps values alive):

```wado
struct MockCMFutureSlot {
    mut value: Option<&()>,  // type-erased: &T cast to &()
}

// MockCM also has:
//   mut future_slots: Array<MockCMFutureSlot>,

impl<T> Future<T> for MockCM {
    fn new(&mut self) -> [Future<T>, FutureWritable<T>] {
        let id = self.future_slots.len();
        self.future_slots.push(MockCMFutureSlot { value: null });
        resume [id as Future<T>, id as FutureWritable<T>]
    }

    fn read(&self, f: &Future<T>) -> Option<T> {
        let id = *f as i32;
        if let Some(erased) = self.future_slots[id].value {
            let ref = erased as &T;  // cast back to original type
            resume Option::Some(*ref)
        }
        resume null
    }

    fn drop(&self, f: &Future<T>) { resume () }
    fn cancel_read(&self, f: &Future<T>) { resume () }
}

impl<T> FutureWritable<T> for MockCM {
    fn write(&mut self, fw: &FutureWritable<T>, value: T) {
        let id = *fw as i32;
        self.future_slots[id].value = Option::Some(&value as &());  // type-erase via &()
        resume ()
    }

    fn drop(&self, fw: &FutureWritable<T>) { resume () }
    fn cancel_write(&self, fw: &FutureWritable<T>) { resume () }
}
```

### Handler Bundling

When a type implements multiple effects, listing each one in `with` is verbose. If the effect name is omitted, the `with` block handles all effects the type implements:

```wado
// Explicit: list each effect separately
with Stream<u8> => &mut cm, StreamWritable<u8> => &mut cm,
     Future<T> => &mut cm, FutureWritable<T> => &mut cm do { ... }

// Bundled: handle all effects MockCM implements
with &mut cm do { ... }
```

Multiple handlers compose naturally:

```wado
with &mut cm, Stdout => &mut stdout, Client => &mut client do {
    run();
}
```

This follows wasmtime's pattern where a single `WasiState` struct implements multiple `*View` traits and is registered with one `add_to_linker` call.

### Handlers for Testing

Effect handlers enable testing code that uses WASI effects without a real WASI runtime. Test functions implicitly have all effects, so handlers can provide any effect. `core:test::MockCM` provides buffered CM canonical handlers as a foundation.

#### Stdout Handler Example

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
    with &mut cm, Stdout => &mut stdout do {
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

#### HTTP Client Handler Example

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
    with &mut cm, Stdout => &mut stdout, Client => &mut client do {
        run();  // example/http-get.wado's export fn run()
        assert client.requests[0] == "/get";
        let output = stdout.drain();
        assert output contains "Status: 200";
    }
}
```

Note: `Fields::new()`, `Response::new()` etc. are HTTP resource operations that forward to the outer scope. This test requires a world that imports `wasi:http` types (e.g., `wasi:http/service`), or additional handlers for those resources.

#### HTTP Server Middleware Example (Post-Resume)

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
        with Handler => &downstream do {
            with Handler => &mut timing do {
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

## Implementation Notes

### Front-End Grammar Notes

#### `do` and `resume` are contextual keywords

The lexer never emits dedicated `Do` or `Resume` token kinds; both
words are returned as ordinary identifiers and the parser only treats
them as keywords in unambiguous positions:

- `do` is recognised in the trailing position of a `with ... do { ... }`
  clause, immediately after the handler binding list.
- `resume` is recognised only in expression position. In statement /
  pattern positions (e.g. `let resume = ...;`) it remains an ordinary
  identifier.

This keeps both words available as variable names and avoids breaking
generated Wado source (e.g. ANTLR4 driver output that uses `let do = …`
for a TypeScript token of that name).

#### Handler expressions are restricted to unary expressions

Inside `with E1 => handler do { ... }`, the `handler` slot is parsed
with `parse_unary_expr`, which covers references (`&h`, `&mut h`),
prefix-`*` deref, `!`/`~`/`-`, identifiers, calls, method calls, and
field/index access. It deliberately stops short of:

- `as` casts
- `if` / `match` / `matches` / `do` expressions
- assignment / compound assignment

This keeps the grammar unambiguous: stopping at unary level prevents the
handler expression from greedily eating the trailing `,` or `do` token
that closes the binding list. Cases that need the excluded forms must
wrap the handler in parentheses, e.g.
`with E => (h as &mut MockE) do { ... }`.

### Dispatch Mechanism: funcref vtable + Wasm Global

Each effect gets a Wasm global holding a nullable reference to a dispatch record. The dispatch record is a Wasm GC struct containing a funcref per operation, a reference to the handler instance, and a reference to the outer (previous) dispatch record.

```wat
;; One dispatch record type per effect
(type $Dispatch_Stdout (struct
  (field $outer   (ref null $Dispatch_Stdout))   ;; previous handler
  (field $handler (ref any))                      ;; handler instance
  (field $op_write_via_stream (ref $sig_wvs))     ;; funcref per operation
))

;; One global per effect
(global $__effect_Stdout (mut (ref null $Dispatch_Stdout)) (ref.null $Dispatch_Stdout))
```

When no handler is installed (the common production path), the global is null and effect operations call the CM adapter directly. The null check is branch-predictor-friendly and adds negligible overhead relative to the CM boundary crossing.

### Dispatch Function

One dispatch function is generated per effect operation. It checks the global, and either calls the CM adapter (default) or calls through the dispatch record's funcref:

```wat
;; __dispatch_Stdout_write_via_stream
(func $dispatch_stdout_wvs (param ...) (result ...)
  (local $dispatch (ref null $Dispatch_Stdout))
  (local.set $dispatch (global.get $__effect_Stdout))
  (if (ref.is_null (local.get $dispatch))
    (then
      ;; Default: call existing CM adapter
      (return (call $__cm_adapter_stdout_wvs ...)))
    (else
      ;; Handler: restore outer scope, call handler, re-install
      (global.set $__effect_Stdout
        (struct.get $Dispatch_Stdout $outer (local.get $dispatch)))
      (local.set $result
        (call_ref $sig_wvs
          (struct.get $Dispatch_Stdout $handler (local.get $dispatch))
          ...
          (struct.get $Dispatch_Stdout $op_write_via_stream (local.get $dispatch))))
      (global.set $__effect_Stdout (local.get $dispatch))
      (return (local.get $result)))))
```

The outer-scope restoration before `call_ref` ensures that handler method bodies execute in the outer effect scope. This makes effect forwarding and self-delegation (e.g., `CachingClient` calling `Client::send` to reach the real implementation) work correctly without infinite recursion.

### Compilation of `with ... do`

```wado
with Stdout => &mut mock do { body }
```

Compiles to:

1. Construct a dispatch record: `struct.new $Dispatch_Stdout (global.get $__effect_Stdout, mock_ref, funcref_for_each_op)`
2. `global.set $__effect_Stdout` with the new dispatch record
3. Execute body — every control-flow exit from the body (`return`, `break L`/`continue` to a target outside the body) gets the restore step (4) spliced in front of it; value-carrying jumps bind the value to a temp local first so it evaluates under the still-installed handler
4. `global.set $__effect_Stdout` with the dispatch record's `outer` field (restore)

Nesting composes naturally — each `with` block links to the previous dispatch record via `outer`.

### Compilation of `resume`

`resume value` compiles to `return value`. The handler method is a normal function; the dispatch function receives and propagates the return value. Post-resume code (e.g., cleanup after the `do` block) requires Wasm Stack Switching and is deferred.

### Wildcard `..`

Unimplemented operations get a trap stub funcref in the dispatch record: `(func $trap (...) (unreachable))`.

### Binary Size

| Element          | Cost                               |
| ---------------- | ---------------------------------- |
| Per effect       | 1 global + 1 GC struct type        |
| Per operation    | 1 dispatch function (10-20 instr)  |
| Per `with` block | 1 struct.new + global save/restore |
| Per handler impl | 1 wrapper per implemented op       |

Growth is O(operations), independent of the number of call sites or handler types. Function signatures are unchanged — no hidden parameters.

### Design Alternatives Considered

- Hidden parameter threading: passes dispatch record as an extra function parameter. Enables static devirtualization at `with` sites, but changes every function signature in the effect chain and increases binary size proportional to call-chain depth times number of effects. Could be added later as an optimization pass on top of the global-based mechanism.
- Flat globals (one funcref global per operation, no struct): avoids GC allocation but requires O(operations) save/restore per nesting level and cannot represent the outer chain cleanly.
- Switch/br_table with integer discriminant: enables direct calls but duplicates dispatch code at every call site, growing O(call_sites × handler_types).

## Consequences

- Effects are traits: any type implementing `impl Effect for Type` can serve as a handler
- No `handler` keyword needed; handlers are ordinary values with effect implementations
- Mutable state in handlers is natural: struct fields accessed via `&self` / `&mut self`
- Handlers satisfy effects locally; unhandled effects forward to the outer scope
- Handler bodies execute in the outer effect scope, enabling delegation to real implementations
- World imports are the outermost handler scope; user handlers nest inside
- `..` wildcard enables partial handling with runtime trap for unimplemented operations
- `resume` without post-processing compiles to `return`; post-processing requires Stack Switching
- One-shot semantics ensure resource safety
- CM streams and futures are unbuffered — synchronous handlers need `MockCM` (buffered CM handlers) for data transfer
- Handler bundling (`with &mut value do`) reduces boilerplate when a type implements multiple effects
- `core:test::MockCM` provides standard buffered Stream/Future handlers as a foundation for all test mocks
