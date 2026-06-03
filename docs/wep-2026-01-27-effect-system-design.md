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

`log_stdout` and `log_stderr` from `core:internal` are effect-less by compiler magic. They can be called from any function without effect declaration. This is _ambient authority_: no world import and no `with` annotation are required, justified by the effects being best-effort and unobservable as a dependency.

### Benign Effects

`#[benign(E)]` marks a function that performs effect `E` but whose effect is observationally pure — unobservable through the function's interface — so `E` is not propagated to callers. Unlike an ambient effect, a benign effect still requires the world import; only the `with E` propagation is elided. The canonical use is `HashMap::new` consuming `InsecureSeed` for Hash DoS resistance without leaking it (the seed is unobservable because the map iterates in insertion order). See `wep-2026-01-20-effect-system-randomness.md`.

### Generic Effects

Use `<effect E>` to declare a generic effect parameter. `E` represents an unknown set of effects, inferred at each call site as the **union** of effects from all function-typed arguments.

```wado
fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn map<T, U, effect E>(arr: List<T>, f: fn(T) -> U with E) -> List<U> with E {
    // ...
}

fn run_both<effect E>(f: fn() with E, g: fn() with E) with E {
    f();
    g();
}

run_both(
    || { println("stdout"); },     // Stdout
    || { eprintln("stderr"); },    // Stderr
);  // E is inferred as Stdout ∪ Stderr; caller needs both
```

Effects are types. No bounds needed.

#### Generic Effect Parameters Are Propagation-Only

A function body can use `E` only by **forwarding** it: calling functions that declare `with E`, or passing `fn() with E` values to other generic functions. The body cannot:

- Install a handler for `E` (`with E => h do { ... }` requires a concrete effect declaration).
- Call any operation through `E` (`E::<op>(...)` — `E` has no known operations).
- Inspect or branch on what `E` resolves to.

The reason is that effect-check runs before monomorphisation, so at the type-check site there is no operation list to dispatch against and no way to verify that a handler value implements `E`. Without effect bounds (`<effect E: SomeEffect>`), effect rows (`<E | SomeEffect>`), or full effect inference, abstract `E` is **opaque inside the function body**.

```wado
fn ok<effect E>(f: fn() with E) with E {
    f();                              // OK: forwarding
}

// fn bad<effect E>(f: fn() with E, h: ???) {
//     with E => h do { f(); }       // ERROR: E is not a concrete effect
// }
```

For handler installation, write a function that takes a concrete effect:

```wado
fn run_with_mock_counter(f: fn() with Counter) {
    let mut c = MockCounter { value: 0 };
    with Counter => &mut c do { f(); }
}
```

#### Single Effect Parameter Per Function

A function may declare **at most one** `<effect E>` parameter. Multiple effect parameters (`<effect E1, effect E2>`) are rejected at compile time (`effect_polymorphism_multi_param_error.wado`).

The single-parameter form covers higher-order combinators (`map`, `fold`, `for_each`, `wrapper`, `run_both`) without effect-set subtyping or row polymorphism, because callbacks with different effect sets are unioned into one inferred `E`.

The pattern that single-`E` cannot express is **effect subtraction** — a generic combinator that handles one abstract effect and propagates another:

```wado
// Hypothetical multi-param form (currently rejected):
fn handle_one<effect E1, effect E2>(
    f: fn() with E1, E2,
    h: impl E1,
) with E2 {
    with E1 => h do { f(); }
}
```

Wado defers multi-effect parameters because:

1. The built-in `with E => h do { ... }` syntax handles the concrete-effect case inline at the use site, removing the main motivation for generic handler combinators in user code.
2. Inference shifts from "union all callable effects into one variable" to constraint solving over multiple variables, which interacts non-trivially with signature-resource inference and effect propagation.
3. No production code in scope today (`core:*`, `wasi:*`, examples) needs effect subtraction.

This is a forward-compatible restriction: existing single-`E` code continues to work unchanged if multi-effect parameters are introduced later (e.g. for `core:test` runners or dynamic middleware composition).

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

- [x] Implemented.

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

- [x] Implemented. Depends on resource-as-effect.

When an effect operation's signature contains resource types, those resource effects are automatically available to the caller. This propagation is transitive.

The rule: if a resource type `R` appears in any parameter type or return type of an effect's operations, then `with Effect` implicitly grants `with R`. Recursively, if `R`'s operations mention another resource type `S`, then `S` is also granted.

No existing language has this mechanism. The closest precedents are Koka's effect aliases (manual grouping) and Rust's supertraits (`trait Ord: Eq`). Effect propagation is an automatic, signature-derived form of supertrait.

Example: `Stdout` has a single operation:

```wado
pub interface Stdout {
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
with Environment  → (nothing)   // get_environment() -> List<[String, String]>
with Random       → (nothing)   // get_random_bytes(u64) -> List<u8>
with Exit         → (nothing)   // exit(Result<(), ()>)
```

Only resource types (`resource` keyword) trigger propagation. Structs, enums, variants, and primitives do not.

### Signature-Resource Inference

- [x] Implemented.

Resources that appear in a function's own parameter types or return type do not need to be repeated in `with`. They are inferred. This mirrors effect propagation but applies to the function's own signature rather than to an effect's operations.

The rule: if a resource type `R` appears anywhere in a function's parameter types, return type (including the declared return type of an `async fn` that erases to unit through `task return`), or reachable via newtypes, containers (`Option`, `Result`, tuples, `List<T>`, `&T`, `&mut T`), struct fields, variant case payloads, or function types, then `R` is unioned into the function's declared `with` set before effect checking. Propagation (above) then runs over the union, so transitive resources (`Stream` → `StreamWritable`, etc.) also become available.

```wado
// `s: Stream<u8>` puts Stream (and transitively StreamWritable) in scope.
// No `with Stream` / `with StreamWritable` needed.
fn consume(s: Stream<u8>) {
    let [rx, tx] = Stream::<u8>::new();
    tx.drop();
    rx.drop();
    s.drop();
}

// Return type counts too. `make_pair` sees Stream / StreamWritable
// through the tuple payload of the return type.
fn make_pair() -> [Stream<u8>, StreamWritable<u8>] {
    return Stream::<u8>::new();
}

// `&Headers` is a newtype of `Fields` (a resource). Signature inference
// unwraps the newtype, so `with Fields` is not needed.
fn headers_to_map(headers: &Headers) -> TreeMap<String, String> { ... }

// Async handlers: the declared return type `Result<Response, ErrorCode>`
// is erased to unit at the Wasm boundary (the result travels via
// `task return`), but the effect checker still walks it, so
// `with Response` is not needed.
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    // Request (param), Response + ErrorCode (task return) all in scope.
    ...
}
```

This is the Wado analogue of Scala 3 Caprese's capture inference: a capability named in the signature does not need to be repeated in the capture set. Unlike Caprese, Wado has no subtyping on effect sets — inference only unions, never narrows.

Limitations — these require separate work and are pinned by `#![TODO]` fixtures today:

- Closure body effects (`effect_propagation_indirect.wado`): a closure body that uses `Stream::new()` assigned to a declared `fn() with Stdout` cannot be rescued, because the closure's signature doesn't name `Stream`. Requires effect-set propagation-closure equivalence at the closure-typing site.
- Generic body effects (`effect_propagation_generic_body.wado`): a `<effect E>` function body that uses a concrete resource cannot be rescued by signature inference either. Requires body-effect inference + generic monomorphization.

### Handlers

See [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md) for the full handler design including syntax, resume semantics, MockCM, handler bundling, and testing patterns.

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
- Signature-resource inference removes the need to repeat resources that already appear in parameter or return types (including `async fn` task return types and newtypes of resources)
- Generic effects (`<effect E>`) support higher-order functions without effect polymorphism complexity
- No existing language has signature-based effect propagation; this is a novel design
- See [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md) for handler-specific consequences
