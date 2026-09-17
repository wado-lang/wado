# WEP: Effect System Design

Status: Draft

## Context

Wado tracks side effects through an effect system. This WEP defines the syntax and semantics for effect declarations, effect checking, effect handlers, and the relationship between resource types and effects.

## Decision

### Effect Declaration

Effects must be explicitly declared on functions. No inference.

```wado
fn greet(name: String) with Stdout {
    println(`Hello, ${name}!`);
}

fn pure_add(a: i32, b: i32) -> i32 {
    return a + b;  // no effects
}
```

Multiple effects use comma separation:

```wado
fn process() with (Stdout, Stderr, FileSystem) {
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

`log_stdout` and `log_stderr` from `core:rt` are effect-less by compiler magic. They can be called from any function without effect declaration. This is _ambient authority_: no world import and no `with` annotation are required, justified by the effects being best-effort and unobservable as a dependency.

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

#### `with _`

- [x] Implemented.

`with _` introduces a fresh effect parameter and forwards it, so it is sugar for `<effect E> with E`. A function that only passes its callees' effects through writes it and names nothing:

```wado
fn wrapper(f: fn() with _) with _ { f(); }   // == fn wrapper<effect E>(f: fn() with E) with E
```

The same spelling on a trait head is what opens the trait to its impls, below.

### Closure Types

Closures require explicit effect annotation:

```wado
let f: fn(i32) -> i32 with Stdout = |x| {
    println(`${x}`);
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

fn reset_and_print() with (counter, Stdout) {
    counter = 0;
    println(`Counter reset`);
}
```

| Declaration             | Read     | Write           | Effect |
| ----------------------- | -------- | --------------- | ------ |
| `global X: T = ...`     | OK       | N/A (immutable) | None   |
| `global mut X: T = ...` | `with X` | `with X`        | Yes    |

This design follows Koka's approach where state effects are tracked, but uses simpler syntax. The `with counter` declaration is sufficient; no separate get/set functions are generated internally.

See also: [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md) for how effects relate to WIT interfaces, and [Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md) for deriving a caller's effects from an imported component's host-leaf imports rather than from its exported interface.

### Resource Types as Effects

- [x] Implemented.

Resource types (`resource`) are capabilities. Every operation on a resource (constructors, methods, statics) is a host call that requires the host to provide the implementation. Therefore, resource types are effects: using any operation on a resource type requires that the resource is available in the current effect scope.

The "every resource operation is a host call" premise is generalized by [Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md): it holds for host-provided resources (all of today's), which stay effects, but a guest-implemented resource imported from a fused component is guest-to-guest and reconstructs to the exporter's own host-leaf imports.

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

### Traits and Effects

A trait head says what every impl of it may do. It has four states, and a bare head is the one nobody has decided yet:

| Head                          | Every impl of it                            |
| ----------------------------- | ------------------------------------------- |
| `trait Foo { … }`             | undecided: reads as `with _`, and is warned |
| `trait Foo with () { … }`     | is pure                                     |
| `trait Foo with Stdout { … }` | gets exactly `Stdout`                       |
| `trait Foo with _ { … }`      | brings its own effects                      |

A method's own `with` clause overrides the head.

The point of the table is that "should be pure" is a contract worth writing down, and that writing nothing is not the same statement. Every trait the standard library declares says `with ()`: an impl of one performing I/O is a design error, comparison, conversion and iteration alike. A trait that means to admit an effect says so, and its author does not have to guess on the first day.

#### A head that names no hole

- [x] Implemented.

An impl method may not declare an effect the trait method leaves out:

```wado
trait Source with () {
    fn next(&mut self) -> i32;
}

impl Source for Loud {
    // error: effect 'Stdout' is not declared by trait method 'Source::next'
    fn next(&mut self) -> i32 with Stdout { ... }
}
```

`stores` is exempt: it says which reference parameters the body keeps, so each impl declares its own.

An `interface` is exempt as a whole. Its operations declare no effects, and a handler method answers an operation rather than implementing a trait contract.

#### Dispatch through a bound

- [x] Implemented.

A call reaches a method through a type parameter's bound in three shapes: a method call on a receiver whose type is the parameter, a static call written `T::make()`, and a `for-of` over an iterable whose type is the parameter. Each demands the effects the trait method declares.

```wado
trait Source with Stdout {
    fn next(&mut self) -> i32;
}

fn draw<S: Source>(s: &mut S) -> i32 with Stdout {  // `with Stdout` is required here
    return s.next();
}
```

The impl is not known at such a call, so the declaration is the only thing it can demand. The head is what makes that sound: it bounds every impl.

Resolving the selected impl's effects at each instantiation would admit more programs, since an impl could then add an effect and still be caught where it is used. It would also break the rule that a signature is the whole contract. A call added inside `draw` could change what every caller of `draw` must declare, with nothing in `draw`'s signature to show it.

#### `with _`: the impl decides

- [x] Implemented.

`with _` on a trait head is the same sugar as on a function, so `trait Source with _` is `trait Source<effect E> with E`. The impl's own method signatures supply the argument, and nothing new has to be named:

```wado
trait Source with _ {
    type Item;
    fn read(&mut self) -> Option<Self::Item>;
}

impl Source for LineReader {
    type Item = String;
    fn read(&mut self) -> Option<String> with FileSystem { ... }   // E = FileSystem
}
```

A bound leaves the argument free, or passes one to constrain it. A caller that only forwards writes `with _` and names nothing:

```wado
fn count<S: Source>(s: S) -> i32 with _ { ... }            // as effectful as `s`
fn sum<S: Source with ()>(s: S) -> i32 { ... }             // only a pure source
```

A type implements a trait once, so two impls differing only in the effect argument are rejected. That is what keeps the argument an output of the impl rather than a choice made at the call.

The parameter is resolved where the call names the type: `count(lines)` demands what `impl Source for LineReader` declares, and `count(nums)` demands nothing. That is the one place an instantiation decides a requirement, and the signature is what admits it — `with _` says "as effectful as `S`", where a fixed head's `with Stdout` says `Stdout` and nothing else. A caller that forwards rather than resolves writes `with _` of its own.

A rigid dispatch is the case that does not resolve: in `fn count<S: Source>(s: S)` the body's `s.read()` has no impl to read, since `S` is a type parameter, so the hole survives and `count` forwards it. That is why every trait in the standard library and in the test corpus declares its head — a bare one would push a `with _` onto each of its callers.

#### An undecided head

- [x] Reads as `with _`, and is diagnosed.

A bare head is not neutral. It reads as `with _`, so it publishes an open contract, and deciding it later is a breaking change: a downstream `with _` that forwarded the trait's effects has nothing left to forward once the head says `with ()`.

So the compiler says the head is undecided, at the severity the trait's visibility calls for. A file-private or `internal` trait gets a remark, which is the state a trait is in while it is being written. A `pub` or `export` trait gets a warning, because publishing an undecided contract is the defect. Both are waived per declaration or per module with `allow`, the way `shadowed_name` is, and only user-authored modules are diagnosed.

The effect of this is that nobody has to predict a trait's effects on the day they write it, and nobody can publish one without saying.

### Handlers

See [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md) for the full handler design including syntax, resume semantics, MockCM, handler bundling, and testing patterns.

### Relation to `stores`

The `stores` annotation shares syntax with effects:

```wado
fn register(data: &Data) -> Handle with (Stdout, stores[data]) {
    // ...
}
```

## Roadmap

The trait head is in. What is left is recorded as gaps below.

## Known gaps

### An effect argument on a bound

`fn sum<S: Source with ()>(s: S)` — constraining an open trait's effect argument at the bound — does not parse yet. Until it does, a caller that wants only a pure impl has no way to say so, and writes `with _` to accept any.

### How deep an open head resolves

A hole is filled from the impl the call names: a free call reads its type arguments, a method dispatch reads its receiver, and a receiver that is itself a wrapper is followed through its own type arguments to a bounded depth. Past that depth, and for a receiver naming no impl this phase indexed, the parameter survives and the caller forwards it with `with _` — sound, but more than the impl would have demanded.

### One effect parameter per function

A function may declare at most one `<effect E>`. More than one is rejected today (`effect_polymorphism_multi_param_error.wado`), which blocks effect subtraction: a combinator that handles one abstract effect and forwards another.

```wado
// Rejected today:
fn handle_one<effect E1, effect E2>(f: fn() with E1, E2, h: impl E1) with E2 {
    with E1 => h do { f(); }
}
```

Nothing in the design turns on the restriction. Inference has to move from unioning every callable effect into one variable to solving over several, which interacts with signature-resource inference, and no code in scope needs it yet. Existing single-`E` code is unaffected whenever it is lifted.

Note that `with _` mints a parameter, so a function cannot write `with _` and declare its own `<effect E>` until this is lifted.

## Consequences

- All function effects are explicit and checked at compile time
- Effect violations produce clear compile errors
- Resource types are effects: every resource operation requires the resource to be in scope
- Effect propagation eliminates verbosity: `with Stdout` automatically grants `Stream`, `Future`, etc.
- Signature-resource inference removes the need to repeat resources that already appear in parameter or return types (including `async fn` task return types and newtypes of resources)
- Generic effects (`<effect E>`) support higher-order functions without effect polymorphism complexity
- No existing language has signature-based effect propagation; this is a novel design
- See [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md) for handler-specific consequences
