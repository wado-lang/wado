# Effect System

An effect is a capability a function uses, such as writing to standard output.
This chapter covers declaring an effect, declaring the effects a function
performs and how they propagate to callers, functions generic over effects,
effects on trait methods, and handlers.

The effect system does three jobs:

- It tracks access to external resources and global variables.
- It injects dependencies: a handler a caller installs reaches every callee without being passed.
- It corresponds directly to WASI capabilities.

Rationale: [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md).

## Effect Definition

An effect is declared as an `interface`, whose operations are free functions:

<!-- {"fixture":"spec_effects_definition.wado"} -->

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

struct Echo {}

impl Http for Echo {
    fn get(&self, url: String) -> String { resume `GET ${url}` }
    fn post(&self, url: String, body: String) -> String { resume `POST ${url} ${body}` }
}

test {
    with Http => &Echo {} do {
        assert Http::get("/a") == "GET /a";   // an operation is called as `Effect::op(...)`
        assert Http::post("/b", "x") == "POST /b x";
    }
}
```

### Default Implementations

An `interface` dispatches differently from a trait, but writes its members exactly as a trait does. An operation is a signature ending in `;`, or a signature followed by a block. That block is the operation's default implementation. What answers an operation dispatched with no handler installed is in [With No Handler Installed](#with-no-handler-installed).

<!-- {"fixture":"spec_effects_default_impl.wado"} -->

```wado
interface Log {
    fn emit(message: String) {
        log_stderr(message);          // no handler installed: degrade, don't trap
    }

    fn level() -> i32;                // no default: unhandled dispatch traps
}

test "a default answers with no handler installed" {
    Log::emit("degraded to stderr");
}

#[expect_trap]
test "an operation without one traps" {
    Log::level();
}
```

A default also answers an operation that a handler leaves out, directly or through `..forward` from the outermost handler (see [Operations a Handler Leaves Out](#operations-a-handler-leaves-out)). So a layer that decorates only one operation can be installed on its own.

A default is a handler body, so it runs in the outer scope like every other handler method (see [Where a Handler Method Runs](#where-a-handler-method-runs)). An `Effect::op(...)` inside a default reaches the next handler out, not the handler the default is filling, so a forward never recurses into itself. This is the one place the analogy with a trait's default method stops. A trait default calling `self.other()` reaches the impl's override, and a filled operation's call does not.

A parameter may declare a default, which the call fills in ([Restrictions](./spec-functions.md#restrictions)).

Beyond a name, parameters and a return type, an operation declares nothing else. Each of these is a compile error, for the reason given:

- A body on an operation a Component Model import backs (one carrying `#[cm(...)]`, and every `resource` method). With no handler installed, the host answers it, so the body could never run.
- A body on an `async` operation. A call to it evaluates to an `AsyncCall`, which a plain body does not produce.
- A `self` receiver. An operation is called as `Effect::op(args)`, with no receiver to bind it to.
- A `with` clause. An operation's effects are not required at its call sites, so one would let a default perform a capability its caller never declared. A default has to be performable wherever it is dispatched, which means pure or `#[ambient]` code.
- A `#[retain(...)]` attribute. An operation dispatches to a handler, whose own body states what it keeps, so one here would constrain call sites on a promise the handler never makes.
- Type parameters. An operation dispatches to one handler method, not to one per instantiation.

### Async Operations

An operation that maps to a WIT `async func` is declared `async fn`, and its return type must be `AsyncCall<T>`. How a caller waits on the result is in [Async Imports](./spec-components.md#async-imports).

<!-- {"fixture":"spec_effects_async_operation.wado"} -->

```wado
#[cm("example:demo/fetch")]
pub interface Fetch {
    #[cm("example:demo/fetch#get")]
    async fn get(url: String) -> AsyncCall<Result<String, i32>>;
}

struct Canned {}

impl Fetch for Canned {
    fn get(&self, url: String) -> Result<String, i32> { resume Result::Ok(`body of ${url}`); }
}

test {
    with Fetch => &Canned {} do {
        assert Fetch::get("/a").wait() == Result::Ok("body of /a");
    }
}
```

A function that calls an async operation is not itself async. `async` marks only the operation, and the `export async fn` of a world export (see [`task return`](./spec-worlds.md#task-return-statement)).

## Effect Declaration in Functions

A function or method lists the effects it performs in a `with` clause after its
return type. A function without one is pure:

<!-- {"fixture":"spec_effects_declaration.wado"} -->

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

test {
    greet("Ann");
    show_env();
    Logger { prefix: "> " }.log("hi");
    assert add(1, 2) == 3;
}
```

The effects a `with` clause lists form its row. A row of one is written bare,
and a row of more than one in parentheses, wherever the row appears. So a comma
after a bare effect always belongs to the enclosing list, never to the row:

<!-- {"fixture":"spec_effects_declaration.wado"} -->

```wado
fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E { return f(x); }
fn both(f: fn() with (Stdout, Stderr), x: i32) -> i32 { return x; }

test {
    assert apply(|n| n + 1, 1) == 2;
    assert both(|| { println("out"); eprintln("err"); }, 3) == 3;
}
```

Every member of a row is an effect.

## Importing Effect Operations

An operation is called as `Effect::op()`. An imported operation may also be
called by its bare name:

<!-- {"fixture":"spec_effects_import_ops.wado"} -->

```wado
// Import effect operations
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
use {Environment, Environment::{get_environment, get_arguments}} from "wasi:cli";

pub fn println(message: String) with Stdout {
    let [rx, tx] = Stream::<u8>::new();
    let done = write_via_stream(rx);  // No need for Stdout:: prefix
    tx.write_all(`${message}\n`.bytes().collect());
    tx.drop();
    done.drop();
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

test {
    println(`${get_arguments().len()} arguments`);
    assert env("SPEC_EFFECTS_UNSET_VARIABLE") == null;
}
```

### Import Rules

- `use {Effect::{op1, op2}} from "..."` imports operations by name. A wildcard, `use {Effect::{*}}`, is not allowed.
- An imported operation demands what `Effect::op()` demands (see [Effect Propagation](#effect-propagation)).
- An operation may be renamed as any import may ([Renaming Imports](./spec-modules.md#renaming-imports)). Two effects' operations of the same name are told apart by renaming one, or by calling it as `Effect::op()`:

<!-- {"fixture":"spec_effects_import_ops_rename.wado"} -->

```wado
// Example with name collision handling
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
use {Stderr, Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

pub fn log(message: String) with (Stdout, Stderr) {
    let [out, out_tx] = Stream::<u8>::new();
    let [err, err_tx] = Stream::<u8>::new();
    let out_done = write_via_stream(out);  // Calls Stdout::write_via_stream
    let err_done = stderr_write(err);      // Calls Stderr::write_via_stream (renamed)
    assert out_tx.write_all(`out: ${message}\n`.bytes().collect()) == CopyResult::Completed;
    assert err_tx.write_all(`err: ${message}\n`.bytes().collect()) == CopyResult::Completed;
    out_tx.drop();
    err_tx.drop();
    out_done.drop();
    err_done.drop();
}
```

## Effect Propagation

Every function declares its effects, whatever its visibility. Nothing is inferred from the body. A call demands of its caller:

- for a function, the effects in its `with` clause;
- for an operation of a host-backed interface (one carrying `#[cm(...)]`), that interface;
- for an operation of a resource, that resource (see [Resources as Effects](#resources-as-effects));
- for an operation of a user-defined interface, nothing. An installed handler answers it, and it traps where none is installed (see [Handlers](#handlers)).

<!-- {"fixture":"spec_effects_propagation_missing.wado"} -->

```wado
fn helper() {
    println("x");      // ERROR: missing effect 'Stdout' required by 'println'
}
```

<!-- {"fixture":"spec_effects_propagation.wado"} -->

```wado
fn next_id() -> i32 {
    return Counter::next();   // OK: `Counter` is a user-defined interface
}

pub fn report() with (Stdout, Preopens) {   // `pub` changes nothing
    println("report");
}

test {
    let mut seq = Seq { n: 0 };
    with Counter => &mut seq do {
        assert next_id() == 1;
    }
    report();
}
```

A test block may perform any effect without declaring it (see [Syntax Rules](./spec-testing.md#syntax-rules)).

### Resources as Effects

Every `resource` is an effect. Its constructors, static methods and instance methods are host calls, so calling any of them demands the resource. `Stream<T>` and `Future<T>` are resources like any other.

<!-- {"fixture":"spec_effects_resource.wado"} -->

```wado
use { TcpSocket, IpAddressFamily } from "wasi:sockets";

fn connect() -> bool with TcpSocket {
    let socket = TcpSocket::create(IpAddressFamily::Ipv4);   // OK
    return socket matches { Ok(_) };
}

test {
    assert connect();
}
```

<!-- {"fixture":"spec_effects_resource_missing.wado"} -->

```wado
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

<!-- {"fixture":"spec_effects_signature_resources.wado"} -->

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
fn headers_to_map(headers: &Headers) -> TreeMap<String, String> {
    let mut map = TreeMap::<String, String>::new();
    for let [name, value] of headers.copy_all() {
        map[name as String] = String::from_utf8_lossy(&value);
    }
    return map;
}

test {
    let [rx, tx] = make_pair();
    tx.drop();
    consume(rx);
    let headers = Headers::new();
    let _ = headers.append("x-id" as FieldName, [55]);
    assert headers_to_map(&headers)["x-id"] == "7";
}
```

A [type pattern](./spec-patterns.md#type-patterns) that narrows a handle to resource `R` holds `R` from then on, as an operation returning `R` would:

<!-- {"fixture":"spec_effects_narrowing_holds.wado"} -->

```wado
fn describe(n: Node) -> String {                // holds Node
    return match n {
        input: HtmlInputElement => input.value(), // holds HtmlInputElement too
        _ => "",
    };
}

test {
    with HtmlInputElement => &Typed {} do {
        assert describe(an_input()) == "typed";
        assert describe(a_node()) == "";
    }
}
```

Apart from a narrowing, nothing a body does adds to what its function holds.

### Ambient Functions

`#[ambient]` on a function exempts its body from effect checking. The body may perform any effect without declaring it, and a call demands only what the function's own `with` clause declares. It is for best-effort output that must work from any function: `log_stdout` and `log_stderr` are ambient, and so is the `core:log` facade.

<!-- {"fixture":"spec_effects_ambient.wado"} -->

```wado
use { log_stderr } from "core:cli";

fn compute(x: i32) -> i32 {
    log_stderr(`computing ${x}`);   // no `with` needed
    return x * 2;
}

test {
    assert compute(21) == 42;
}
```

[`#[benign(E)]`](./spec-attributes.md#benigne-) is the narrower form: only the named effects go undeclared, and the rest of the body is checked.

### Non-Effects

`panic` and `unreachable` are not effects. Both return `!`, so a pure function may call them.

## Generic Effects (Effect Polymorphism)

`<effect E>` declares an effect parameter. `E` stands for zero or more concrete effects, inferred at each call site from the function-typed arguments.

<!-- {"fixture":"spec_effects_generic.wado"} -->

```wado
fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn map<T, U, effect E>(arr: List<T>, f: fn(T) -> U with E) -> List<U> with E {
    let mut out: List<U> = [];
    for let x of arr {
        out.push(f(x));
    }
    return out;
}

test {
    wrapper(|| { println("x"); });   // E = Stdout, which a test block holds
    assert map([1, 2], |x: i32| x * 10) == [10, 20];   // E = (none)
}
```

An effect parameter sits beside type parameters in the list (`<T, effect E>`).
Where several function-typed arguments name the same `E`, it resolves to the
union of their effects. A function may declare several effect parameters. Each
resolves on its own from the arguments that name it, so a function can keep the
effects of two closures apart.

The caller of a generic function must hold what its parameter resolves to:

<!-- {"fixture":"spec_effects_generic_missing.wado"} -->

```wado
fn bad() {
    wrapper(|| { println("x"); });   // ERROR: E = Stdout, and `bad` does not hold it
}
```

A closure argument's effects are those inferred from its body (see [Closures](./spec-functions.md#closures)).

### `with _`

`with _` introduces a fresh effect parameter and forwards it, so it is sugar for `<effect E> with E`. Every `_` in one signature is the same parameter, distinct from any `<effect E>` the signature declares:

<!-- {"fixture":"spec_effects_with_underscore.wado"} -->

```wado
fn wrapper(f: fn() with _) with _ { f(); }      // == fn wrapper<effect E>(f: fn() with E) with E

fn combine(f: fn() with _, g: fn() with _) with _ {
    f();
    g();
}

test {
    wrapper(|| { TRACE.push_str("w"); });
    combine(|| { TRACE.push_str("f"); }, || { TRACE.push_str("g"); });
    assert TRACE == "wfg";
}
```

### An Effect Parameter Is Only Forwarded

A body uses its effect parameter only by forwarding it: calling a function or closure whose effects are `E`, or passing a `fn() with E` on. `E` stands for an unknown set of effects, so the body cannot:

- call an operation through it (`E::op()`), since `E` has no operations;
- install a handler for it: `with E => h do { ... }` is a compile error;
- ask or branch on what `E` resolves to.

A function that installs a handler takes a concrete effect instead:

<!-- {"fixture":"spec_effects_install_concrete.wado"} -->

```wado
fn with_mock_counter(f: fn() with Counter) {
    let mut c = MockCounter { value: 0 };
    with Counter => &mut c do { f(); }
}

test {
    with_mock_counter(|| { SEEN = Counter::next() + Counter::next(); });
    assert SEEN == 3;   // the mock answered 1, then 2
}
```

## Effects on Trait Methods

A trait method's `with` clause is the contract every impl of it writes to. An impl method may not declare an effect the trait method leaves out, and a call to the method requires what the trait declares, whichever impl runs.

<!-- {"fixture":"spec_effects_trait_method.wado"} -->

```wado
trait Source {
    fn next(&mut self) -> i32 with Stdout;
}

impl Source for Loud {
    fn next(&mut self) -> i32 with Stdout {         // matching the declaration
        println("tick");
        self.n += 1;
        return self.n;
    }
}

fn draw<S: Source>(s: &mut S) -> i32 with Stdout {  // required: `s.next()` needs it
    return s.next();
}

test {
    let mut loud = Loud { n: 0 };
    assert draw(&mut loud) == 1;
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

<!-- {"fixture":"spec_effects_trait_head_open.wado"} -->

```wado
trait Source with _ {
    fn next(&mut self) -> i32;
}

impl Source for Loud {
    fn next(&mut self) -> i32 with Stdout {         // E = Stdout
        println("tick");
        self.n += 1;
        return self.n;
    }
}

fn draw<S: Source>(s: &mut S) -> i32 with _ {       // as effectful as `S`
    return s.next();
}

export fn run() with Stdout {
    let mut loud = Loud { n: 0 };
    println(`${draw(&mut loud)}`);                  // Stdout, from `Loud`'s impl
    assert pure_draw() == 3;
}

fn pure_draw() -> i32 {                             // `Quiet`'s impl declares nothing
    let mut quiet = Quiet { n: 0 };
    return draw(&mut quiet);
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

<!-- {"fixture":"spec_effects_handler_impl.wado"} -->

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
    fn new(&mut self) -> Fields {                                 // answers `Fields::new()`
        self.calls += 1;
        resume Fields::new()
    }
    fn has(&mut self, this: &Fields, name: FieldName) -> bool {   // answers `f.has(name)`
        self.calls += 1;
        resume this.has(name)
    }
    ..trap
}

test {
    let mut mock = MockStdin { responses: ["a", "b"], index: 0 };
    with Stdin => &mut mock do {
        assert Stdin::read_line() == "a";
        assert Stdin::read_line() == "b";
    }
    let mut counting = CountingFields { calls: 0 };
    with Fields => &mut counting do {
        let f = Fields::new();
        assert !f.has("x-id" as FieldName);
    }
    assert counting.calls == 2;
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

<!-- {"fixture":"spec_effects_install.wado"} -->

```wado
with Stdin => &mut mock do { assert Stdin::read_line() == "a"; }
with Stdin => &mut s, Stdout => &mut o do { Stdout::write(Stdin::read_line()); }
assert o.lines == ["b"];
```

- `E` names an effect as a `with` clause does, with its type arguments when it has them (`Stream<u8>`). A name that reaches no effect is a compile error.
- `h` must implement `E` at those arguments. A handler whose type is a type parameter is rejected, even when its bound names `E`, because which impl would answer is not known.
- `h` is a unary expression: a name, a reference (`&h`, `&mut h`), a call, a method call, or a field or index access. Anything else, a cast for instance, goes in parentheses: `with E => (h as &mut MockE) do { ... }`.
- `E` is a concrete effect, never an effect parameter (see [An Effect Parameter Is Only Forwarded](#an-effect-parameter-is-only-forwarded)).

The handler expressions are evaluated before the body, outside the handlers they install.

`with ... do` is an expression. Its value is the body's, as a block's is:

<!-- {"fixture":"spec_effects_install.wado"} -->

```wado
let id = with Random => &mut rng do { Uuid::v4() };
assert id.to_string() == "00000000-0000-4000-8000-000000000000";
```

The body holds every effect its `with` installs. It may perform them and call functions that declare them, without the enclosing function declaring them:

<!-- {"fixture":"spec_effects_install.wado"} -->

```wado
fn use_both() -> i32 with (A, B) { return A::a() + B::b(); }

fn mocked() -> i32 {                        // declares neither A nor B
    let mut a = Both {};
    let mut b = Both {};
    return with A => &mut a, B => &mut b do { use_both() };
}

test "the body holds what its `with` installs" {
    assert mocked() == 3;
}
```

A `with ... do` as a closure's body needs parentheses or a block (see [Closures](./spec-functions.md#closures)).

### Handler Scope

A handler is installed for the dynamic extent of its body. Every operation performed from the body reaches it, at any call depth. It stays installed until control leaves the body, by reaching the end or by a `return`, `break` or `continue` that jumps out, and then the handler that was there before answers again. A value that a `return` or `break` carries out is computed while the handler is still installed. A `break` to a label inside the body does not leave it.

An operation reaches the innermost handler installed for its own effect. An inner `with` for the same effect takes over until its body ends:

<!-- {"fixture":"spec_effects_handler_scope.wado"} -->

```wado
with Counter => &outer do {
    with Counter => &inner do {
        assert Counter::next() == 2;     // inner
    }
    assert Counter::next() == 1;         // outer
}
```

Bindings on one `with` line install in source order, so a later binding for an effect sits inside an earlier one for the same effect. The later one answers, and the earlier one is the next handler out.

A `with` in a global initializer covers that initializer and nothing after it.

### Bundled Handlers

A binding without `E =>` installs its handler for every effect its type implements:

<!-- {"fixture":"spec_effects_bundled.wado"} -->

```wado
with &mut cm do { assert Counter::tick() == 1; }                               // every effect `cm`'s type implements
with &mut cm, Stdout => &mut stdout do { Stdout::write(Greeting::hello()); }   // mixed with explicit bindings
assert cm.count == 2 && stdout.lines == ["hello"];
```

A type implementing several instances of one generic effect is installed for each of them. The handler expression is evaluated once, and every effect it is installed for shares that one value, so state that one operation changes is seen by the others.

A bundled binding whose type implements no effect is a compile error. So is one whose type has no declaration to find impls on: a type parameter, an associated type projection, a function type.

### Handler State

A handler is a value like any other. `with E => &mut h do` hands the handler a reference to `h`, so what its `&mut self` methods change is in `h` once the block ends. A handler written by value is a copy, and its changes end with the block.

<!-- {"fixture":"spec_effects_handler_scope.wado"} -->

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

<!-- {"fixture":"spec_effects_handler_delegates.wado"} -->

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

test {
    let mut counting = Counting { calls: 0 };
    with Random => &mut counting do {
        assert Random::get_random_bytes(4).len() == 4;
    }
    assert counting.calls == 1;
}
```

Any other effect the method performs is answered by whatever handles that effect at the operation's call.

### Operations a Handler Leaves Out

An `impl E for T` need not implement every operation. An operation it leaves out is decided by the block's rest clause:

- `..forward` sends it to the next handler out. It behaves as `fn op(args) { resume E::op(args) }` would.
- `..trap` traps if it is dispatched, even where the interface gives it a default. A mock that says an operation must not be called means it.
- With no rest clause, the interface's [default implementation](#default-implementations) answers it, and it traps where there is none.

The rest clause is the block's last item, and a block may hold nothing else. A bare `..` is a syntax error. Forwarding where trapping was meant, or the reverse, fails silently, so the choice is written out. `forward` and `trap` are [contextual keywords](./spec-lexical.md#contextual-keywords).

<!-- {"fixture":"spec_effects_rest_clause.wado"} -->

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

test {
    let mut base = Base {};
    with Log => &mut base do {
        with Log => &Filter { min: 2 } do {
            assert !Log::enabled(1) && Log::enabled(2);
            Log::emit("forwarded");
        }
        with Log => &Passthrough {} do {
            assert Log::enabled(1);
        }
    }
    assert base.lines == ["forwarded"];
    with TcpSocket => &MinimalTcp {} do {
        assert TcpSocket::create(IpAddressFamily::Ipv4) matches { Ok(_) };
    }
}

#[expect_trap]
test "`..trap` traps on an operation the mock leaves out" {
    with TcpSocket => &MinimalTcp {} do {
        let socket = TcpSocket::create(IpAddressFamily::Ipv4).unwrap();
        socket.get_is_listening();
    }
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
