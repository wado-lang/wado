# Effect System

## Design Philosophy

The Effect System is equivalent to:

- Tracking access to external resources / global variables
- Implicitly propagating DI (Dependency Injection)
- Direct correspondence with WASI Capabilities

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

An `interface` is a trait with a different dispatch story, and its members are written exactly as a trait's are: an operation is a signature ending in `;`, or a signature followed by a block. That block is the operation's default implementation: what the operation does when it is dispatched with no handler installed. Without one, dispatching an unhandled operation traps.

```wado
interface Log {
    fn emit(message: String) {
        log_stderr(message);          // no handler installed: degrade, don't trap
    }

    fn level() -> i32;                // no default: unhandled dispatch traps
}
```

A default fills a handler that leaves the operation out, and it is what `..forward` reaches when the outermost handler forwards an operation nobody else handles. So a layer that only decorates one operation is installable on its own. An explicit `..trap` still wins: a mock that says an operation must not be called means it.

A default is a handler body, so it runs in the outer scope like every other one (see [Handlers](#handlers)): an `Effect::op(...)` inside a default reaches the next handler out, not the handler the default is filling. That is what keeps a forward from recursing into itself, and it is the one place the analogy with a trait's default method stops. A trait default calling `self.other()` reaches the impl's override, and a filled operation's call does not.

A parameter may declare a default, and a call that omits the argument gets it filled in at the call site, as a function call does. The handler receives the argument already in place.

Beyond a name, parameters and a return type, an operation declares nothing else. Each of these is a compile error, for the reason given:

- A body on an operation a Component Model import backs (one carrying `#[cm(...)]`, and every `resource` method). Its no-handler case is the CM adapter, so the body could never run.
- A body on an `async` operation. Its call site is typed as an `AsyncCall`, which a plain body does not produce.
- A `self` receiver. An operation is called as `Effect::op(args)`, with no receiver to bind it to.
- A `with` clause. An operation's effects are not required at its call sites, so one would let a default perform a capability its caller never declared. A default has to be performable wherever it is dispatched, which means pure or `#[ambient]` code.
- A `#[retain(...)]` attribute. An operation dispatches to a handler, whose own body states what it keeps, so one here would constrain call sites on a promise the handler never makes.
- Type parameters. Dispatch holds one slot per operation, not one per instantiation.

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

A row of one goes bare; a row of more than one is parenthesized, wherever the
row appears. So a comma after a bare effect always belongs to the enclosing
list, never to the row:

```wado
fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E { ... }
fn both(f: fn() with (Stdout, Stderr), x: i32) { ... }
```

Every row member is an effect.

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
- At most one effect parameter is allowed per function
- Are inferred from the effects of function-typed arguments at each call site; when multiple function-typed arguments reference the same effect parameter, `E` resolves to the union of all their effects
- Can coexist with type parameters: `<T, effect E>`
- Test functions implicitly have all effects

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

An `interface` is exempt: its operations declare no effects, and a handler method answers an operation rather than implementing a trait contract.

### The Trait Head

A `with` clause on the trait itself says what every impl of it may do. A method's own clause overrides it.

| Head                          | Every impl of it       |
| ----------------------------- | ---------------------- |
| `trait Foo { … }`             | as `with _`, diagnosed |
| `trait Foo with () { … }`     | is pure                |
| `trait Foo with Stdout { … }` | gets exactly `Stdout`  |
| `trait Foo with _ { … }`      | brings its own effects |

`with _` is sugar for `<effect E> with E`, so an open head hands each impl its own effects and names none of them:

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

A fixed head demands the same effects of every caller. An open one is resolved from the type each call names, so `draw(&mut quiet)` demands nothing when `Quiet`'s impl declares nothing. The `with _` in `draw`'s signature is what leaves that open. A caller that forwards the effects instead of resolving them writes one of its own.

A head that writes nothing reads as `with _`, so a bare trait is open rather than pure. Publishing an undecided contract is reported: a `pub` trait warns, a file-private or `internal` one remarks, and `#[allow(undecided_effects)]` on the declaration or `#![allow(undecided_effects)]` on the module waives it while the decision is pending.

A body dispatching on a type parameter has no impl to read. In `s.next()`, where `s: S` and `S: Source`, an open head's hole survives, and the enclosing function forwards it with `with _`. Every trait in the standard library says `with ()` instead. An impl of one that performs I/O is a design error, for comparison, conversion and iteration alike.

See [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md).

## Handlers

A handler is an `impl Effect for Type` whose methods may use `resume value` to deliver a value to the suspended caller. The `with E => h do { body }` block installs `h` as the handler for effect `E` for the duration of `body`. The `=>` arrow reads as a dispatch binding ("calls to `E` go to `h`"), not an assignment. An inner `with` for the same effect takes over until its body ends, and the outer handler answers again after it.

```wado
with Stdin => &mut mock do { ... }
with Stdin => &mut s, Stdout => &mut o do { ... }
with &mut bundle do { ... }                       // bundled (omits effect name)
```

See [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md) for resource-as-effect and effect propagation, and [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md) for handler syntax and semantics.
