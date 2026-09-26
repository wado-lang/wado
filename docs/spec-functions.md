# Functions

## Closures

Closures are anonymous function expressions with `|params| body` syntax.

An expression body returns its value implicitly:

```wado
let add_one = |x: i32| x + 1;
let make_point = |x: i32, y: i32| Point { x, y };
```

A block body requires explicit `return`:

```wado
let compute = |x: i32| {
    let doubled = x * 2;
    return doubled + x * 3;
};
```

An optional `-> Type` declares the return type. A `?` in the body needs a known
return type: this one, or the `R` of an expected `fn(..) -> R`.

```wado
let parse = |s: String| -> Result<i32, String> {
    let n = to_int(s)?;
    return Result::Ok(n + 1);
};
```

A parameter type is inferred from the expected `fn(..)` type, matched by
position. Any context that supplies such a type counts: a typed binding, a
function or method parameter, a struct field, a newtype over a `fn(..)`.
Annotate only where nothing supplies one; an annotation always wins:

```wado
let arr: List<i32> = [1, 2, 3];
arr.into_iter().map(|x| x * 2);             // `x: i32`, from `Iterator::Item`
arr.into_iter().fold(0, |acc, x| acc + x);  // `acc: i32`, from the body
let add_one = |x: i32| x + 1;               // no expected type: annotate
```

The expected type may be one of the callee's own type parameters. A sibling
argument then supplies it, whichever side of the closure it is written on. A
numeric-literal sibling does not: `fold(0, |acc, x| acc + x)` over a `List<i64>`
takes `i64` from the body, not `i32` from the `0`. A parameter nothing supplies
is reported at the call, not inside the closure.

A closure declares no effects; they are inferred from the body.
`with` after the parameter list, or after `-> Type`, would be that declaration,
and is a compile error. A handler body therefore needs a block or parentheses:

```wado
let f = || (with Log => &mut sink do { Log::emit(`hi`); });
```

### Closure Types

A closure's type is `fn(P...) -> R` when every capture is read-only, and
`fn mut(P...) -> R` when any capture is written (see [Capture](#capture)).
`fn` is a subtype of `fn mut`: a `fn` closure is accepted where a `fn mut` is
expected, and a `fn mut` closure where a `fn` is expected is a compile error.

```wado
fn apply(f: fn(i32) -> i32, x: i32) -> i32 { return f(x); }
fn apply_twice(mut f: fn mut(i32) -> i32, x: i32) -> i32 { return f(f(x)); }

let n = 1;
apply_twice(|x| x + n, 5);                  // OK: a `fn` where `fn mut` is expected
let mut count = 0;
apply(|x| { count += 1; return x; }, 5);    // ERROR: a `fn mut` where `fn` is expected
```

The same bare form names a closure type in every position: a parameter, a
return type, a struct field, a local annotation, a container element. There is
no separate function-pointer type and no `impl` / `dyn` distinction. How a call
through the value is dispatched is the compiler's choice and never changes its
meaning.

A closure type carries the effects a call may perform, in the same `with` row a
function declares (see
[Effect Declaration in Functions](./spec-effects.md#effect-declaration-in-functions)):

```wado
let f: fn(i32) -> i32 with Stdout = |x| { println(`${x}`); return x; };
```

A type parameter may be bounded by a closure type. The bound names the type
once, so the parameter can be reused across positions:

```wado
fn apply<F: fn(i32) -> i32>(f: F, x: i32) -> i32 { return f(x); }
fn invoke<F: fn mut(i32)>(mut f: F) { f(1); f(2); }
fn dup<F: fn(i32) -> i32>(f: F) -> [F, F] { return [f, f]; }
```

Only a function value satisfies such a bound. No user-defined type can be made
callable. A function generic over the effects of a closure it takes declares an
effect parameter (see
[Generic Effects](./spec-effects.md#generic-effects-effect-polymorphism)).

### Calling a `fn mut`

Calling a `fn mut` closure requires the _root_ of the callee place to be a
mutable binding. This applies whether the closure is called directly (`f()`) or
reached through field access or indexing (`(h.f)()`, `arr[i]()`), and to a
parameter as to a local:

```wado
fn run(mut f: fn mut(i32)) { f(1); f(2); }   // `mut f` is required

let mut count = 0;
let mut c = || count += 1;                   // `mut c` is required
c();
```

A binding whose type is `&mut T` is a mutable place, so `(self.f)()` inside a
`&mut self` method is accepted. A temporary root, such as a call result, has no
binding and is always accepted. A closure calling a captured `fn mut` still
requires the outer binding to be `mut`.

A `fn` closure needs no `mut` binding.

### Capture

A closure captures each free variable by reference. The reference kind is
inferred from the body: `&T` where the body only reads the binding, `&mut T`
where it writes it. A write assigns the binding or a place in it, calls a
`&mut self` method on it, or takes `&mut` of it, so `|v| seen.push(v)` is a
`fn mut`.

Because a capture is a reference, every closure naming a binding shares its
location, and a closure reads the value the binding holds when the closure
runs, not when it was made:

```wado
let mut count = 0;
let mut inc = || count += 1;   // captures &mut count; type fn mut() -> ()
let get = || count;             // captures &count; type fn() -> i32
inc();
inc();
assert get() == 2;
count = 10;
assert get() == 10;
```

A closure reaches a binding of any enclosing frame, however many closures sit in
between, and reads and writes the binding the source names. What a closure binds
itself shadows, as anywhere: `|mut count| count += 1` writes its own parameter
and captures nothing.

```wado
let mut count = 0;
let mut outer = || {
    let mut inner = || count += 1;   // writes the function's `count`
    inner();
};
outer();
assert count == 1;
```

There is no `move` keyword. To capture a snapshot, copy the value into a local
first and let the closure capture that:

```wado
let snapshot = original;     // a copy, independent of `original`
let f = || snapshot * 2;
```

A closure capturing an outer binding is a separate matter from what a function
does with its reference _parameters_; see
[Reference Retention](./spec-memory.md#reference-retention).

### Closures Are Values

A closure is copied on assignment, on passing and on return, like any other
value. Its captures are references, so every copy still refers to the same
bindings:

```wado
let mut count = 0;
let mut c1 = || count += 1;
let mut c2 = c1;             // a copy holding the same `&mut count`
c1();
c2();
assert count == 2;
```

A call never consumes a closure, so any closure can be called any number of
times. There is no single-use closure type like Rust's `FnOnce`.

Rationale: [WEP: Closure Implementation](./wep-2026-01-16-closure-implementation.md).

## Function References

A bare function name is an expression of function type. It evaluates to a value of type `fn(P...) -> R [with E...]` matching the function's signature.

```wado
fn double(n: i32) -> i32 { return n * 2; }

let f = double;            // type: fn(i32) -> i32
assert f(21) == 42;

apply(double, 21);         // pass directly; no `&` needed
let g: fn(i32) -> i32 = double;
```

Key points:

- Function values carry no observable identity. There is no state to observe, and no way to compare two `fn` values.
- `&` and `&mut` apply to `fn`-typed values like to any other value, with no special-casing:
  - `&f` has type `&fn(...)`; `&mut f` (on a mutable binding) has type `&mut fn(...)`.
  - These references behave per the [Reference Types](./spec-memory.md#reference-types) rules. `&fn(...)` is _not_ a synonym for `fn(...)`; passing one where the other is expected is a type error.
  - `&mut fn(...)` parameters are useful as out-parameters: the callee can reassign the referenced slot via `*p = other_fn`, and the caller observes the new value through the same binding.
- A `&fn(...)` or `&mut fn(...)` value is directly callable; the call expression auto-derefs to invoke the underlying `fn(...)`. `let r = &double; r(21)` works without an explicit `*r`.
- Generic functions taken as values need their type arguments pinned. Two principled forms are supported:
  - Turbofish on the name itself: `let f = identity::<i32>;` evaluates to a `fn(i32) -> i32` value, and a non-call use like `apply(identity::<i32>, 7)` works the same way.
  - An expected `fn(...)` type at the use site: `let f: fn(i32) -> i32 = identity;` and `apply(identity, 7)` (where `apply`'s parameter is `fn(i32) -> i32`) both pin the type arguments through positional inference against the expected signature.
  - When neither form applies, it is a compile error, and the diagnostic suggests turbofish or a closure wrapper (`|x| identity(x)`).
- A function type crosses the Component Model boundary only as a callback; see [Component Model Callbacks](#component-model-callbacks).

## Component Model Callbacks

A function type crosses the Component Model boundary in one place: as a
parameter of a `#[cm]` import, where it is a callback. Anywhere else it is a
compile error: in an `export fn`'s parameters or result, in an import's result,
and inside a type that crosses, as a field or a payload (`Option<fn(..)>`
included).

```wado
#[cm("example:demo/target", linearity = "unrestricted")]
resource Target {
    #[cm("example:demo/target#listen")]
    #[cm_params("self", "listener")]
    fn listen(&self, listener: fn mut(i32));
}

target.listen(|v| seen.push(v));
```

- A callback's parameters are scalars and handles, and it returns nothing. Any
  other function type on a `#[cm]` declaration is a compile error.
- The closure stays in the guest. The call registers it and passes a `u32` key
  in its place. One closure value has one key.
- The host calls the closure back through the `wado:callback/callback`
  interface, which the component exports. It holds one function per argument
  shape, named `call` followed by one word per argument: the argument's
  primitive type, or `handle` for a handle (`call`, `call-i32`, `call-handle`).
  Each takes the key first, is lifted synchronously, and returns nothing.
  `wado wit` lists the interface among the world's exports.
- The host may call a callback during an import call, which reenters the
  component, or while `run` is suspended.
- A callback runs as a task of its own. It may perform an effect a world import
  backs.
- Where a Wado handler answers the `#[cm]` interface instead of the host, the
  handler receives the closure itself and may call it.

Rationale: [WEP: The Web Interface for Wado](./wep-2026-04-01-web.md#callbacks).

## Default Arguments

Trailing function parameters may declare default values with `= expr`. A call
that omits a defaulted argument gets the default expression filled in at the
call site, so it costs what the call with every argument written costs:

```wado
fn connect(host: String, port: i32 = 8080, timeout: i32 = 30) { ... }

connect("localhost");           // → connect("localhost", 8080, 30)
connect("localhost", 3000);     // → connect("localhost", 3000, 30)
connect("localhost", 3000, 60);
```

A parameter without a default cannot follow one with a default, and `self`
cannot have a default:

```wado
fn foo(a: i32 = 0, b: i32) { ... }   // ERROR: parameters without defaults cannot follow
```

### Default Expressions

A default is any expression that performs no effect. It is evaluated at each
call that omits the argument, not once at the declaration. A default that
performs an effect is a compile error:

```wado
fn foo(
    x: i32 = 0,                   // literal
    y: f64 = f64::PI,             // associated constant
    z: String = `default`,        // template string
    w: Option<Config> = null,     // Option::None
    v: Color = Color::Red,        // enum case
    u: i32 = i32::max(1, 2),      // pure function call
) { ... }

fn noisy() -> i32 with Stdout { ... }
fn bar(v: i32 = noisy()) { ... }  // ERROR: the default performs `Stdout`
```

A default may name an earlier parameter, and then reads the value the call
supplied for it. Each argument is still evaluated once, in its place among the
others, with the receiver first:

```wado
fn make_rect(width: f64, height: f64 = width) -> Rect { ... }
make_rect(10.0);                 // → make_rect(10.0, 10.0)

fn twice(a: i32, b: i32 = a + a) -> i32 { ... }
twice(next());                   // `next()` runs once
```

Where the named parameter is a `&mut`, the default borrows the same place again,
and the callee's writes still reach it:

```wado
fn put(o: &mut Option<i32>, v: i32 = peek(o) + 1) { ... }
put(&mut s.o);                   // → put(&mut s.o, peek(&mut s.o) + 1)
```

A default may name a type parameter of the declaration that wrote it. It stands
for the type argument the call site settled on, whether a turbofish spelled it,
an argument beside it pinned it, or the parameter's own default supplied it:

```wado
fn info<T: Default>(msg: String, fields: T = T::default()) -> String { ... }
info::<i32>("count");  // → info::<i32>("count", 0)
```

The same holds for an instance or static method, where the `impl` block's
parameters come from the receiver, and for a struct field default, where they
come from the type the literal is annotated with. A type parameter pack is named
the same way (`t: [..T] = [..T::default()]`). A call that leaves no type
argument for the pack settles it to the empty pack.

### Where a Default Resolves

A default resolves its names in the scope of the declaration that wrote it, not
at the call site. So a default may name the declaring module's private items and
private types, and its imports and import aliases, which the caller need not be
able to name. Nothing visible only at the call site changes what a default
means: a caller's import or local spelled like a name in the default shadows
nothing.

```wado
// lib.wado
global DEFAULT_PORT: i32 = 8080;                    // private to lib.wado
pub fn connect(host: String, port: i32 = DEFAULT_PORT) { ... }

// main.wado
use { connect } from "./lib.wado";

fn start() {
    let DEFAULT_PORT = 1;
    connect("localhost");                           // port is lib.wado's 8080
}
```

`#file`, `#line` and `#function` are the exception: they evaluate at the call
site (see
[Call-site evaluation in default arguments](./spec-literals.md#call-site-evaluation-in-default-arguments)).

### Restrictions

- Function types do not carry default information; assigning a function with defaults to a `fn(...)` type erases them, and every call site of that variable must supply every argument.
- Closures cannot declare defaults: a closure value's arity must match its `fn(...)` type, so `= expr` on a closure parameter is a compile error.
- `export fn` cannot declare defaults, since the component's WIT signature requires every parameter. A private helper can declare them behind a thin `export fn` wrapper.
- An import may declare defaults: a `#[cm]` resource method, or an `interface` operation (see [Default Implementations](./spec-effects.md#default-implementations)). The call fills the default in, and the import receives every argument.
- Trait methods may declare defaults only in the trait definition; implementations receive every parameter and cannot add, remove, or change defaults. An implementation that writes a default is a compile error. Every spelling of a call fills the trait's defaults: `x.m()`, `Type::m(..)`, and `T::m(..)` through a bound. A default-bodied method in the trait is the declaration, so it may write defaults. Direct `impl Type { ... }` methods (not part of any trait) may declare defaults freely.

### Type Parameter Defaults

A type parameter may declare a default with `= Type`, on a free function, an inherent method or a trait method. An omitted turbofish takes the default; a spelled one wins. `core:log` uses it:

```wado
pub fn info<T: Serialize = NoFields>(message: String, fields: T = NoFields {}, ...) { ... }

info("started");                  // → info::<NoFields>("started", NoFields {})
info::<Fields>("started", f);
```

Inference runs first and the default fills only what it left unbound, so an argument or an expected type always decides the slot it pins.

A default resolves in the declaring module's scope, as a [value default does](#where-a-default-resolves). It may therefore name a type the call site cannot: `NoFields` above is private to `core:log`. By the same rule a parameter the use site declares does not answer for it, however the two are spelled:

```wado
struct Zero {}
struct Marked<M: Mark = Zero> { value: i32 }

fn f<Zero: Mark>(probe: Zero) -> i32 {
    let m: Marked = Marked { value: 1 };  // the module's `Zero`, not `f`'s
    return m.total();
}
```

A default may name a parameter to its left, and stands for that parameter's argument. One naming a parameter at or after its own slot is rejected, since no argument has settled it yet:

```wado
struct Both<A, B = A> { v: A }        // OK: `B` takes `A`'s argument
struct Fwd<A = B, B = i32> { v: B }   // ERROR: `A`'s default names `B`
struct Own<A = A> { v: i32 }          // ERROR: the same, one slot nearer
```

Expanding a default must reach a fixpoint. One that leads back to the declaration it belongs to is rejected, whether it names that declaration directly, under an argument, or through another declaration's defaults:

```wado
struct Rec<T = Rec> { v: i32 }           // ERROR
struct Pair<A, B = Pair<A>> { v: i32 }   // ERROR
struct Ping<X, Y = Pong<X>> { v: i32 }   // ERROR, paired with
struct Pong<X, Y = Ping<X>> { v: i32 }   // this one
```

A trait method's type parameter default belongs to the trait, exactly as its value defaults do. The implementation restates the list — the same parameters in the same order, with the defaults omitted — and every spelling of the call fills them from the trait's declaration:

```wado
pub trait Boxed {
    fn boxed<T: Named = Tag>(&self) -> String;   // `Tag` is private to this module
    fn made<T: Named = Tag>() -> String;
}

impl Boxed for M {
    fn boxed<T: Named>(&self) -> String {        // no default here
        return T::name();
    }

    fn made<T: Named>() -> String {
        return T::name();
    }
}

m.boxed();            // → m.boxed::<Tag>()
m.boxed::<Local>();   // spelled, so `Local`
M::made();            // the static spelling reads the same declaration
M::boxed(&m);         // and so does the receiver-taking one
```

Rust rejects a type parameter default on every function, method and `impl` (rust-lang#36887), allowing them only on type and trait declarations. Wado accepts them wherever a parameter list is written.

The same `= expr` syntax applies to struct fields; see [Struct Field Defaults](./spec-types.md#struct-field-defaults).

Rationale: [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

## Tagged Template Literals

A path written directly before a template literal is a tag. The template then
denotes a call of that function on the template's holes, in their own types,
with the literal text around them, instead of a rendered `String`:

```wado
let q = sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`;
let s = String::raw`${dir}\bin\run.exe`;   // backslashes kept
```

The tag is a function name or a static method path, with no whitespace before
the backtick. The backtick is a postfix on the path and binds as a call does.
Any other expression before a backtick, such as a call result or a
parenthesized expression, is a syntax error. A path naming a variant case or a
closure-typed binding is rejected as a tag. The literal is lexed exactly as an
untagged template, so every escape must still be one the lexer knows even where
the tag preserves it.

A tag is an ordinary function whose first parameter takes the template by value
and is bound by `ReflectTemplate`, the reflected kind of a template literal. The
template is the call's one written argument. Trailing parameters with defaults
are filled as in any call (see [Default Arguments](#default-arguments)). A first
parameter of any other type, `&T` included, is reported as a tag error. Nothing
on the declaration marks a function as a tag.

Each template shape has an anonymous type of its own, holding one field per
hole. The shape is the template's segments, specifiers, hole types and hole
source texts. So
`` tag`${a}` `` and `` tag`${b}` `` are two types, each instantiating the tag,
even where `a` and `b` share a type. The type is unnameable and reached only
through the bound; a diagnostic and `Reflect::type_name()` show it as its text
with each hole spelled by its type and specifier, `` `id = ${i32:04}` ``, cut
at 50 characters with `...`.

`ReflectTemplate` is sealed: only the compiler implements it, and an `impl` of
it is a compile error. Its associated type `Holes` is the tuple of hole types,
and `Members` the tuple of hole handles `members()` returns. The tag walks the
holes with tuple `for-of`:

```wado
fn sql<T: ReflectTemplate<Holes = [..V]>, ..V: ToSqlParam>(t: T) -> SqlQuery {
    let mut query = "";
    let mut params: List<SqlParam> = [];
    for let h of ReflectTemplate::<T>::members() {
        query.push_str(h.lit());                // literal text before this hole
        query.push_str("?");
        params.push(h.get(&t).to_sql_param());  // the value, storage shared
    }
    query.push_str(ReflectTemplate::<T>::tail());
    return SqlQuery { query, params };
}
```

A hole handle (`TemplateHole<T, V>`) answers `index()` (its position, from 0),
`lit()` / `raw()` (the preceding segment, escapes processed or preserved),
`get(&t)` (the value, `V`), `source()` (the expression text), `has_spec()`, and
`fmt(&t, f)` (rendering as the untagged template would).
`ReflectTemplate::<T>::tail()` and `raw_tail()` give the segment after the last
hole. Every answer but `get` and `fmt` is a constant. A hole handle is minted
only by `members()`.

`members()` walks a pack, so `Holes` is bound either as one (`[..V]`) or as the
empty tuple (`[]`, for a tag that reads only `tail()`). A concrete tuple
(`Holes = [List<i32>]`) is an error at the call. A bound on the pack
(`..V: ToSqlParam`) makes a hole whose type lacks it an error at the call,
naming that type.

A hole's type may not mention a type parameter of the enclosing item, since the
shape is minted once rather than per instantiation. A generic body passes its
tag a concrete value from its caller. The untagged template makes no shape, so
`` `${v}` `` over a `v: X` is accepted where `` format`${v}` `` is not.

Holes are evaluated once, left to right, before the tag runs. Each hole's value
is the one it had at its own position, so a later hole that writes to its
storage changes nothing the tag sees: `` format`${a} ${bump(&mut a)}` ``
renders what `` `${a} ${bump(&mut a)}` `` renders. A tag may carry effects,
which the caller declares as for any call, and return any type. Whether a call
folds at compile time is the optimizer's decision, as for any other call; the
meaning does not depend on it.

An untagged template means what the prelude's `format` tag means: each hole
rendered through its specifier into one buffer.

Rationale: [WEP: Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md).

## Variadic Type Packs

Use `<..T>` to declare a type pack parameter that represents zero or more types. Type packs enable writing functions that operate on tuples of any arity.

```wado
fn identity<..T>(x: [..T]) -> [..T] {
    return x;
}

fn prepend<A, ..T>(a: A, rest: [..T]) -> [A, ..T] {
    return [a, ..rest];
}
```

Type pack parameters:

- Are declared with `..` prefix in generic parameter lists: `<..T>`, `<A, ..T>`
- May appear more than once per list, each settled on its own (see
  [Multiple Type Packs](#multiple-type-packs))
- Cannot be combined with `effect`: `<effect ..T>` is invalid
- Appear inside tuple types as `[..T]` (type pack spread)
- Can be mixed with fixed type elements: `[A, ..T]`, `[..T, B]`
- Type arguments are inferred from tuple argument types at call sites

### Multiple Type Packs

A parameter list may declare more than one pack. Each is settled from the
argument that carries it alone, so nothing has to find a boundary that was
never written:

```wado
fn concat<..A, ..B>(a: [..A], b: [..B]) -> [..A, ..B] {
    return [..a, ..b];
}

concat([1, "x"], [true]);   // A = [i32, String], B = [bool]
```

A tuple holding two packs (`[..A, ..B]`) settles neither pack, because matching
it against a concrete tuple admits every split. Such a tuple is legal anywhere.
It just cannot be the only thing naming a pack. Something else must settle each
one: another parameter, a turbofish, or an annotation. The tuple is then checked
against the arity they fix.

```wado
fn wrap<X, ..A, ..B, Y>(x: X, a: [..A], b: [..B], y: Y) -> [X, ..A, ..B, Y] {
    return [x, ..a, ..b, y];   // the parameters settle both packs
}

fn joined<..A, ..B>(split: [[..A], [..B]], both: [..A, ..B]) -> i32 { … }
joined([[1], [true]], [1, true]);         // `split` settles both
joined([[1], [true]], [1, true, "x"]);    // ERROR: expected `[i32, bool]`

fn middle<..Pre, K, ..Post>(t: [..Pre, K, ..Post]) -> i32 { … }
middle::<[i32], String, [bool]>([1, "mid", true]);   // the turbofish settles them
middle([1, "mid", true]);                 // ERROR: cannot infer `Pre`, `K`, `Post`
```

A pack nothing settles is reported at the use site as an uninferred type
parameter, as a scalar parameter no argument reaches is. To settle both packs
from one value, give each a tuple of its own (`[[..A], [..B]]`).

Where such a tuple is produced, its ends still place elements. The elements ahead
of the first pack and behind the last keep their positions, so `[X, ..A, ..B, Y]`
settles `X` and `Y` and nothing else. An element between two packs is settled by
no argument, so name it in a turbofish.

Inside the body that declares them, packs are rigid, as a scalar parameter is. A
value matches such a tuple by layout: the same fixed positions and the same packs
in the same order. Naming the same packs is not enough.

```wado
fn reorder<..A, ..B>(a: [..A], b: [..B]) {
    let ab: [..A, ..B] = [..a, ..b];      // OK
    // let ba: [..B, ..A] = [..a, ..b];   // ERROR: the order is part of the type
    // let shifted: [i32, ..A] = [..a];   // ERROR: so is a fixed element
}
```

A turbofish spells each pack as its own tuple. A flat list carries no boundary
either:

```wado
concat::<[i32], [bool, String]>([1], [true, "x"]);
// concat::<i32, String>(…)  // ERROR: spell each type pack as a tuple
```

Writing one argument per pack is refused as well: `<i32, String>` and
`<[i32, String], []>` split the same list, and nothing says which was meant. The
flat form stays available where a single pack absorbs the surplus on its own
(`make_defaults::<i32, String>()`).

`zip` transposes its operands position by position, so its rows must be equally
long. Two distinct packs are never known to be, so `[[..a], [..b]].zip()` is
rejected where it is written.

### Lexical note

`..` is one token. Writing `...` (three dots) is a parse error with the diagnostic _"unexpected `...`; did you mean `..`?"_.

### Value Spread

Value spread `[..expr]` splices a tuple's elements into an enclosing tuple literal:

```wado
let a = [1, "hello"];
let b = [..a, true];   // b: [i32, String, bool]
let c = [42, ..a];     // c: [i32, i32, String]
```

The spread expression is evaluated exactly once:

```wado
// make_pair() is called once, not twice
let t = [..make_pair(), 30];
```

## Known gaps

### Closures

- A closure whose body performs an effect is accepted where the expected type
  declares none: `let f: fn() = || println("x");` compiles, and a call through
  `f` performs `Stdout` where no `with` admits it.
- Two closure arguments of one call are typed in source order, and the first is
  not revisited. A closure whose parameter types only a later closure in the
  same call could settle is reported as uninferred.

### Component Model Callbacks

- Each mention of a named function makes a new closure value. Passing one
  function to two calls registers two keys, so a host that pairs the calls by
  key, as `removeEventListener` pairs with `addEventListener`, sees two
  functions.
- A registered closure is kept for the life of the component instance. Nothing
  releases it.
- Every callback shape a declared `#[cm]` import takes gets its export, whether
  the program passes such a closure or not.

### Default Arguments

- A call that pins none of the type parameters a default names reports the
  uninferred parameter together with an `unknown function` error for the
  default's own call, such as `T::default`. The second error is noise.
- A call is refused where it passes a borrow of a field holding a `variant` to
  a `&mut` parameter that a closure in a later default names. Writing the same
  closure at the call is accepted.

### Tagged Template Literals

- A tag with a turbofish (`` f::<T>`…` ``) is a syntax error.
- A tag reached through a type parameter (`` T::tag`…` ``) is not checked
  against the template: a mismatch with the method's parameter goes
  unreported, as it does for any `T::m(..)` call.
