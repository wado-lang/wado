# Effect System

## Design Philosophy

The effect system does three jobs:

- It tracks access to external resources and global variables.
- It injects dependencies: a handler a caller installs reaches every callee without being passed.
- It corresponds directly to WASI capabilities.

Rationale: [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md).

## Effect Definition

An effect is declared as an `interface`, whose operations are free functions:

```wado
// WASI CLI effects (see wasi:cli for the real definitions)
interface Stdout {
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}

interface Stderr {
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}

interface Environment {
    fn get_environment() -> List<[String, String]>;
    fn get_arguments() -> List<String>;
    fn get_initial_cwd() -> Option<String>;
}

// A custom effect interface
interface Http {
    fn get(url: String) -> String;
    fn post(url: String, body: String) -> String;
}
```

### Default Implementations

An `interface` dispatches differently from a trait, but writes its members exactly as a trait does. An operation is a signature ending in `;`, or a signature followed by a block. That block is the operation's default implementation. What answers an operation dispatched with no handler installed is in [With No Handler Installed](#with-no-handler-installed).

```wado
interface Log {
    fn emit(message: String) {
        log_stderr(message);          // no handler installed: degrade, don't trap
    }

    fn level() -> i32;                // no default: unhandled dispatch traps
}
```

A default also answers an operation that a handler leaves out, directly or through `..forward` from the outermost handler (see [Operations a Handler Leaves Out](#operations-a-handler-leaves-out)). So a layer that decorates only one operation can be installed on its own.

A default is a handler body, so it runs in the outer scope like every other handler method (see [Where a Handler Method Runs](#where-a-handler-method-runs)). An `Effect::op(...)` inside a default reaches the next handler out, not the handler the default is filling, so a forward never recurses into itself. This is the one place the analogy with a trait's default method stops. A trait default calling `self.other()` reaches the impl's override, and a filled operation's call does not.

A parameter may declare a default, and a call that omits the argument gets it filled in at the call site, as a function call does. The handler receives the argument already in place.

Beyond a name, parameters and a return type, an operation declares nothing else. Each of these is a compile error, for the reason given:

- A body on an operation a Component Model import backs (one carrying `#[cm(...)]`, and every `resource` method). With no handler installed, the host answers it, so the body could never run.
- A body on an `async` operation. A call to it evaluates to an `AsyncCall`, which a plain body does not produce.
- A `self` receiver. An operation is called as `Effect::op(args)`, with no receiver to bind it to.
- A `with` clause. An operation's effects are not required at its call sites, so one would let a default perform a capability its caller never declared. A default has to be performable wherever it is dispatched, which means pure or `#[ambient]` code.
- A `#[retain(...)]` attribute. An operation dispatches to a handler, whose own body states what it keeps, so one here would constrain call sites on a promise the handler never makes.
- Type parameters. An operation dispatches to one handler method, not to one per instantiation.

### Async Operations

An operation that maps to a WIT `async func` is declared `async fn`, and its return type must be `AsyncCall<T>`. How a caller waits on the result is in [Async Imports](./spec-components.md#async-imports).

```wado
// From wasi:http
#[cm("wasi:http/client@0.3.0")]
pub interface Client {
    #[cm("wasi:http/client@0.3.0#send")]
    async fn send(request: Request) -> AsyncCall<Result<Response, ErrorCode>>;
}
```

A function that calls an async operation is not itself async. `async` marks only the operation, and the `export async fn` of a world export (see [`task return`](./spec-components.md#task-return-statement)).

## Effect Declaration in Functions

```wado
// Declare required effects with `with`
fn greet(name: String) with Stdout {
    println(`Hello, ${name}!`);
}

// Multiple effects
fn show_env() with (Stdout, Environment) {
    let args = Environment::get_arguments();
    println(`Arguments: ${args:?}`);
}

// A method declares its effects the same way
impl Logger {
    fn log(&self, message: String) with Stderr {
        eprintln(`${self.prefix}${message}`);
    }
}

// No effects = pure function
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}
```

The effects a `with` clause lists form its row. A row of one is written bare,
and a row of more than one in parentheses, wherever the row appears. So a comma
after a bare effect always belongs to the enclosing list, never to the row:

```wado
fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E { ... }
fn both(f: fn() with (Stdout, Stderr), x: i32) { ... }
```

Every member of a row is an effect.

## Importing Effect Operations

To avoid the verbosity of `Effect::operation()` calls, you can explicitly import effect operations:

```wado
// Import effect operations
use {Stdout::{write_via_stream}} from "wasi:cli";
use {Environment::{get_environment, get_arguments}} from "wasi:cli";

pub fn println(message: String) with Stdout {
    // Create stream, start consumer, write data, close stream
    // (simplified - see core:cli for full implementation)
    write_via_stream(...);
}

pub fn env(name: String) -> Option<String> with Environment {
    let vars = get_environment();  // No need for Environment:: prefix
    for let [key, value] of vars {
        if key == name {
            return Some(value);
        }
    }
    return None;
}
```

### Import Rules

- Effect operations use `::` syntax: `use {Effect::{op1, op2}} from "..."`
- Multiple operations can be imported: `Effect::{op1, op2, op3}`
- Renaming is supported: `use {Effect::{op as renamed}} from "..."`
- Wildcards are prohibited: `use {Effect::{*}}` is not allowed
- An imported operation demands what `Effect::op()` demands (see [Effect Propagation](#effect-propagation))

### Name Resolution

- Imported effect operations can be called directly without the `Effect::` prefix
- If an operation name is ambiguous, use the fully qualified `Effect::operation()` syntax
- Non-imported effect operations must always use the `Effect::operation()` syntax

```wado
// Example with name collision handling
use {Stdout::{write_via_stream}} from "wasi:cli";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

pub fn log(message: String) with (Stdout, Stderr) {
    write_via_stream(...);  // Calls Stdout::write_via_stream
    stderr_write(...);      // Calls Stderr::write_via_stream (renamed)
}
```

## Effect Propagation

Every function declares its effects, whatever its visibility. Nothing is inferred from the body. A call demands of its caller:

- for a function, the effects in its `with` clause;
- for an operation of a host-backed interface (one carrying `#[cm(...)]`), that interface;
- for an operation of a resource, that resource (see [Resources as Effects](#resources-as-effects));
- for an operation of a user-defined interface, nothing. An installed handler answers it, and it traps where none is installed (see [Handlers](#handlers)).

```wado
fn helper() {
    println("x");      // ERROR: missing effect 'Stdout' required by 'println'
}

fn next_id() -> i32 {
    return Counter::next();   // OK: `Counter` is a user-defined interface
}

pub fn report() with (Stdout, Preopens) {   // `pub` changes nothing
    // ...
}
```

A test block declares no effects and may perform any (see [Syntax Rules](./spec-testing.md#syntax-rules)).

### Resources as Effects

Every `resource` is an effect. Its constructors, static methods and instance methods are host calls, so calling any of them demands the resource. `Stream<T>` and `Future<T>` are resources like any other.

```wado
use { TcpSocket, IpAddressFamily } from "wasi:sockets";

fn connect() with TcpSocket {
    let socket = TcpSocket::create(IpAddressFamily::Ipv4);   // OK
}

fn bad() {
    let _ = TcpSocket::create(IpAddressFamily::Ipv4);        // ERROR: missing resource 'TcpSocket'
}
```

### Implied Resources

A held effect also holds every resource its operations mention in a parameter or return type. The rule applies again to each resource reached, so the grant is transitive:

```text
with Stdout
  → Stream, Future        write_via_stream(Stream<u8>) -> Future<…>
    → StreamWritable      Stream::new() returns one
    → FutureWritable      Future::new() returns one
```

So `println` needs only `with Stdout`, though its body creates a stream and drops a future. An effect whose signatures mention no resource implies nothing: `with Environment` holds `Environment` alone.

Only resources are implied. They are found inside references, containers, tuples, newtypes, struct fields and variant payloads, but a struct, enum, variant or primitive is never granted in its own right. Each effect implies its own resources and no others, so a function without `with` cannot call `Stream::<u8>::new()` just because some other effect would have implied `Stream`.

### Resources in a Signature

A function holds every resource its own signature mentions, without naming it in `with`. They are found in it as in an operation's signature (above), and also inside function types. The return type counts too, including the declared result of an `export async fn`, which the body delivers with `task return`. Resources implied by those are held as well.

```wado
fn consume(s: Stream<u8>) {                     // holds Stream, and StreamWritable
    let [rx, tx] = Stream::<u8>::new();
    tx.drop();
    rx.drop();
    s.drop();
}

fn make_pair() -> [Stream<u8>, StreamWritable<u8>] {
    return Stream::<u8>::new();
}

// `Headers` is a newtype of the `Fields` resource: no `with Fields` needed.
fn headers_to_map(headers: &Headers) -> TreeMap<String, String> { ... }
```

A [type pattern](./spec-components.md#type-patterns) that narrows a handle to resource `R` holds `R` from then on, as an operation returning `R` would:

```wado
fn describe(n: Node) -> String {                // holds Node
    return match n {
        input: HtmlInputElement => input.value(), // holds HtmlInputElement too
        _ => "",
    };
}
```

Apart from a narrowing, nothing a body does adds to what its function holds.

### Ambient Functions

`#[ambient]` on a function exempts its body from effect checking. The body may perform any effect without declaring it, and a call demands only what the function's own `with` clause declares. It is for best-effort output that must work from any function: `log_stdout` and `log_stderr` are ambient, and so is the `core:log` facade.

```wado
use { log_stderr } from "core:cli";

fn compute(x: i32) -> i32 {
    log_stderr(`computing ${x}`);   // no `with` needed
    return x * 2;
}
```

[`#[benign(E)]`](./spec-attributes.md#benigne-) is the narrower form: only the named effects go undeclared, and the rest of the body is checked.

### Non-Effects

`panic` and `unreachable` are not effects. Both return `!`, so a pure function may call them.

## Generic Effects (Effect Polymorphism)

Use `<effect E>` to declare a generic effect parameter. `E` can represent zero or more concrete effects, inferred from function-typed arguments at each call site.

```wado
fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn map<T, U, effect E>(arr: List<T>, f: fn(T) -> U with E) -> List<U> with E {
    // ...
}
```

Effect parameters:

- Are declared with the `effect` keyword in generic parameter lists
- Are inferred from the effects of function-typed arguments at each call site; when multiple function-typed arguments reference the same effect parameter, `E` resolves to the union of all their effects
- Can coexist with type parameters: `<T, effect E>`
- Can be declared in any number, each resolved on its own from the arguments that name it, so a function can keep the effects of two closures apart

The caller of a generic function must hold what its parameter resolves to:

```wado
fn bad() {
    wrapper(|| { println("x"); });   // ERROR: E = Stdout, and `bad` does not hold it
}
```

A closure argument's effects are those inferred from its body (see [Closures](./spec-functions.md#closures)).

### `with _`

`with _` introduces a fresh effect parameter and forwards it, so it is sugar for `<effect E> with E`. Every `_` in one signature is the same parameter, distinct from any `<effect E>` the signature declares:

```wado
fn wrapper(f: fn() with _) with _ { f(); }      // == fn wrapper<effect E>(f: fn() with E) with E

fn combine(f: fn() with _, g: fn() with _) with _ {
    f();
    g();
}
```

### An Effect Parameter Is Only Forwarded

A body uses its effect parameter only by forwarding it: calling a function or closure whose effects are `E`, or passing a `fn() with E` on. `E` stands for an unknown set of effects, so the body cannot:

- call an operation through it (`E::op()`), since `E` has no operations;
- install a handler for it: `with E => h do { ... }` is a compile error;
- ask or branch on what `E` resolves to.

A function that installs a handler takes a concrete effect instead:

```wado
fn with_mock_counter(f: fn() with Counter) {
    let mut c = MockCounter { value: 0 };
    with Counter => &mut c do { f(); }
}
```

## Effects on Trait Methods

A trait method's `with` clause is the contract every impl of it writes to. An impl method may not declare an effect the trait method leaves out, and a call to the method requires what the trait declares, whichever impl runs.

```wado
trait Source {
    fn next(&mut self) -> i32 with Stdout;
}

impl Source for Loud {
    fn next(&mut self) -> i32 with Stdout { ... }   // matching the declaration
}

fn draw<S: Source>(s: &mut S) -> i32 with Stdout {  // required: `s.next()` needs it
    return s.next();
}
```

A call reaches a method through a type parameter's bound in three shapes: a method call on a receiver whose type is the parameter, a static call written `T::make()`, and a `for-of` over an iterable whose type is the parameter. None of them knows which impl runs, so each demands what the trait method declares.

An `interface` is exempt. Its operations declare no effects, and a handler method answers an operation rather than implementing a trait contract.

### The Trait Head

A `with` clause on the trait itself says what every impl of it may do. A method's own clause overrides it.

| Head                          | Every impl of it       |
| ----------------------------- | ---------------------- |
| `trait Foo { … }`             | as `with _`, diagnosed |
| `trait Foo with () { … }`     | is pure                |
| `trait Foo with Stdout { … }` | gets exactly `Stdout`  |
| `trait Foo with _ { … }`      | brings its own effects |

An open head is a fresh effect parameter, as [`with _`](#with-_) is in a signature. It hands each impl its own effects and names none of them:

```wado
trait Source with _ {
    fn next(&mut self) -> i32;
}

impl Source for Loud {
    fn next(&mut self) -> i32 with Stdout { ... }   // E = Stdout
}

fn draw<S: Source>(s: &mut S) -> i32 with _ {       // as effectful as `S`
    return s.next();
}

export fn run() with Stdout {
    println(`${draw(&mut loud)}`);                  // Stdout, from `Loud`'s impl
}
```

A fixed head demands the same effects of every caller. An open one is resolved from the type each call names, so `draw(&mut quiet)` demands nothing when `Quiet`'s impl declares nothing. Inside `draw`, `s.next()` names only the bound `S: Source`, which has no impl to read. Its effects stay unresolved, and `draw` forwards them with `with _`. A generic caller of `draw` that forwards them again writes its own `with _`.

A head that writes nothing reads as `with _`, so a bare trait is open rather than pure. Publishing an undecided contract is reported: a `pub` trait warns, a file-private or `internal` one remarks, and `#[allow(undecided_effects)]` on the declaration or `#![allow(undecided_effects)]` on the module waives it while the decision is pending.

Every trait in the standard library says `with ()`. An impl of one that performs I/O is a design error, for comparison, conversion and iteration alike.

A type implements a trait once, so two impls that differ only in their effects are rejected. The effects are what the impl brings, not a choice a call makes.

## Handlers

A handler answers an effect's operations in place of the host or the operation's default implementation. It is an ordinary value whose type implements the effect, and a `with ... do` block installs it while the block runs. This is how a program injects a dependency, mocks one in a test, or layers behavior over one.

### Handler Implementations

`impl E for T` makes values of `T` handlers for `E`, where `E` is an effect interface or a resource. A plain trait is neither, even one spelled like an effect, and implementing it grants nothing.

A handler method implements one operation. It takes the operation's parameters and return type, after a receiver the operation does not have: `&self` or `&mut self` to reach the handler's state, or none when the method needs no state. A resource's instance method receives the handle as an explicit parameter after that receiver.

```wado
interface Stdin {
    fn read_line() -> String;
}

struct MockStdin {
    responses: List<String>,
    index: i32,
}

impl Stdin for MockStdin {
    fn read_line(&mut self) -> String {
        let line = self.responses[self.index];
        self.index += 1;
        resume line
    }
}

impl Fields for CountingFields {
    fn new(&self) -> Fields { ... }                                 // answers `Fields::new()`
    fn has(&self, this: &Fields, name: FieldName) -> bool { ... }   // answers `f.has(name)`
    ..trap
}
```

A generic impl such as `impl<T> Greeter for Holder<T>` makes every instance of `Holder` a handler. A generic effect is implemented per instance: `impl Stream<u8> for MockStream` handles `Stream<u8>` and no other `Stream`.

A handler method may declare effects in a `with` clause, like any function.

### `resume`

`resume value` hands `value` to the operation's caller and ends the handler method, as `return` would. The value is checked against the operation's return type. A method for an operation that returns `()` may end without one.

`resume` is valid only in a handler method's body. Anywhere else it is a compile error, including in a closure written inside a handler method. A caller is resumed at most once: continuations are one-shot.

Which names `resume` leaves free is in [Contextual Keywords](./spec-lexical.md#contextual-keywords).

### Installing a Handler

`with E => h do { body }` installs `h` as the handler for `E` while `body` runs. The `=>` reads as "calls to `E` go to `h`": it binds a dispatch, and it is not an assignment. One `with` installs several, separated by commas:

```wado
with Stdin => &mut mock do { ... }
with Stdin => &mut s, Stdout => &mut o do { ... }
```

- `E` names an effect as a `with` clause does, with its type arguments when it has them (`Stream<u8>`). A name that reaches no effect is a compile error.
- `h` must implement `E` at those arguments. A handler whose type is a type parameter is rejected, even when its bound names `E`, because which impl would answer is not known.
- `h` is a unary expression: a name, a reference (`&h`, `&mut h`), a call, a method call, or a field or index access. Anything else, a cast for instance, goes in parentheses: `with E => (h as &mut MockE) do { ... }`.
- `E` is a concrete effect, never an effect parameter (see [An Effect Parameter Is Only Forwarded](#an-effect-parameter-is-only-forwarded)).

The handler expressions are evaluated before the body, outside the handlers they install.

`with ... do` is an expression. Its value is the body's, as a block's is:

```wado
let id = with Random => &mut rng do { Uuid::v4() };
```

The body holds every effect its `with` installs. It may perform them and call functions that declare them, without the enclosing function declaring them:

```wado
fn use_both() -> i32 with (A, B) { ... }

fn mocked() -> i32 {                        // declares neither A nor B
    return with A => &mut a, B => &mut b do { use_both() };
}
```

A `with ... do` as a closure's body needs parentheses or a block (see [Closures](./spec-functions.md#closures)).

### Handler Scope

A handler is installed for the dynamic extent of its body. Every operation performed from the body reaches it, at any call depth. It stays installed until control leaves the body, by reaching the end or by a `return`, `break` or `continue` that jumps out, and then the handler that was there before answers again. A value that a `return` or `break` carries out is computed while the handler is still installed. A `break` to a label inside the body does not leave it.

An operation reaches the innermost handler installed for its own effect. An inner `with` for the same effect takes over until its body ends:

```wado
with Counter => &outer do {
    with Counter => &inner do {
        Counter::next();     // inner
    }
    Counter::next();         // outer
}
```

Bindings on one `with` line install in source order, so a later binding for an effect sits inside an earlier one for the same effect. The later one answers, and the earlier one is the next handler out.

A `with` in a global initializer covers that initializer and nothing after it.

### Bundled Handlers

A binding without `E =>` installs its handler for every effect its type implements:

```wado
with &mut cm do { ... }                          // every effect `cm`'s type implements
with &mut cm, Stdout => &mut stdout do { ... }   // mixed with explicit bindings
```

A type implementing several instances of one generic effect is installed for each of them. The handler expression is evaluated once, and every effect it is installed for shares that one value, so state that one operation changes is seen by the others.

A bundled binding whose type implements no effect is a compile error. So is one whose type has no declaration to find impls on: a type parameter, an associated type projection, a function type.

### Handler State

A handler is a value like any other. `with E => &mut h do` hands the handler a reference to `h`, so what its `&mut self` methods change is in `h` once the block ends. A handler written by value is a copy, and its changes end with the block.

```wado
let mut c = CounterState { value: 0 };
with Counter => &mut c do {
    Counter::next();
    Counter::next();
}
assert c.value == 2;
```

### Where a Handler Method Runs

A handler method for `E` runs with its own installation set aside, so `E`'s operations inside it reach the next handler out. That is how a handler delegates, to an outer handler or to the host, without recursing into itself. The method holds `E` for this without declaring it.

```wado
use { Random } from "wasi:random";

struct Counting {
    calls: i32,
}

impl Random for Counting {
    fn get_random_bytes(&mut self, max_len: u64) -> List<u8> {
        self.calls += 1;
        resume Random::get_random_bytes(max_len)   // the next handler out: the host
    }
    ..forward
}
```

Any other effect the method performs is answered by whatever handles that effect at the operation's call.

### Operations a Handler Leaves Out

An `impl E for T` need not implement every operation. An operation it leaves out is decided by the block's rest clause:

- `..forward` sends it to the next handler out. It behaves as `fn op(args) { resume E::op(args) }` would.
- `..trap` traps if it is dispatched, even where the interface gives it a default. A mock that says an operation must not be called means it.
- With no rest clause, the interface's [default implementation](#default-implementations) answers it, and it traps where there is none.

The rest clause is the block's last item, and a block may hold nothing else. A bare `..` is a syntax error. Forwarding where trapping was meant, or the reverse, fails silently, so the choice is written out. `forward` and `trap` are [contextual keywords](./spec-lexical.md#contextual-keywords).

```wado
struct Filter { min: i32 }

impl Log for Filter {                      // a layer: decorate one operation
    fn enabled(&self, level: i32) -> bool {
        resume level >= self.min && Log::enabled(level)
    }
    ..forward                              // everything else → the outer `Log`
}

impl TcpSocket for MinimalTcp {            // a mock: anything unexpected traps
    fn create(&self, family: IpAddressFamily) -> Result<TcpSocket, ErrorCode> {
        resume Result::Ok(mock_socket())
    }
    ..trap
}

impl Log for Passthrough {                 // implements nothing, forwards all
    ..forward
}
```

### With No Handler Installed

An operation with no handler installed for it is answered by:

- the host, for an operation of a host-backed interface or a resource;
- the operation's default implementation, where the interface declares one;
- otherwise nothing, and the operation traps.

The world's imports thus act as the outermost handler. A `..forward` from the outermost handler reaches the same place.

How a handler answers an async operation is in [Handling an Async Operation](./spec-components.md#handling-an-async-operation).

Rationale: [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md).
