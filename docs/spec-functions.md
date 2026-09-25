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

An optional `-> Type` declares the return type, and is what a `?` in the body
resolves against:

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

A `?` in the body needs the return type known — via `-> Type` or an expected
`fn(..) -> R`.

A closure declares no effects; they are inferred from the body.
`with` after the parameter list, or after `-> Type`, would be that declaration,
and is a compile error. A handler body therefore needs a block or parentheses:

```wado
let f = || (with Log => &mut sink do { Log::emit(`hi`); });
```

Closures auto-capture each free variable by reference; the reference kind is inferred from body usage (`&T` for read-only, `&mut T` for mutating). Pure read-only captures keep the closure type at `fn`; any `&mut` capture promotes it to `fn mut`. Calling a `fn mut` closure requires the _root_ of the callee place to be a mutable binding (mirrors Rust's `FnMut` rule); this applies whether the closure is called directly (`f()`) or reached through field access or indexing (`(h.f)()`, `arr[i]()`). A temporary root — a call result, a literal — has no binding and is always accepted.

Shared mutable state across closures is automatic — multiple closures referring to the same outer binding share the underlying location, with no explicit reference dance needed:

```wado
let mut count = 0;
let mut inc = || count += 1;   // captures &mut count; type fn mut() -> ()
let get = || count;             // captures &count; type fn() -> i32
inc();
inc();
assert get() == 2;
```

See [`docs/wep-2026-01-16-closure-implementation.md`](./wep-2026-01-16-closure-implementation.md) for the full design (`fn` vs `fn mut`, sub-typing, effect generics, iterator API integration).

A closure capturing an outer binding is a separate concept from what a function
does with its reference _parameters_, which nothing in the language states — see
[WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

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
- A function type crosses the Component Model boundary only as a `#[cm]` import's parameter, as a `u32` key the host calls back through. It takes scalars and handles and returns nothing. Anywhere else, an `export fn` included, it is a compile error. See [Tide § Callbacks](./wep-2026-04-01-tide.md#callbacks).

## Default Arguments

See [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

Trailing function parameters may declare default values with `= expr`. Calls that omit defaulted arguments are expanded at the call site, with no runtime cost:

```wado
fn connect(host: String, port: i32 = 8080, timeout: i32 = 30) { ... }

connect("localhost");           // → connect("localhost", 8080, 30)
connect("localhost", 3000);     // → connect("localhost", 3000, 30)
connect("localhost", 3000, 60);
```

### Rules

- All defaulted parameters must come after all non-defaulted parameters.
- Default expressions must be effect-free (validated by the effect system).
- Default expressions may reference earlier parameters in the same function:

```wado
fn make_rect(width: f64, height: f64 = width) -> Rect { ... }
make_rect(10.0);  // → make_rect(10.0, 10.0)
```

- Default expressions may name a type parameter of the declaration that wrote them. It stands for the type argument the call site settled on, whether a turbofish spelled it, an argument beside it pinned it, or the parameter's own default supplied it:

```wado
fn info<T: Default>(msg: String, fields: T = T::default()) -> String { ... }
info::<i32>("count");  // → info::<i32>("count", 0)
```

The same holds for an instance or static method, where the `impl` block's parameters come from the receiver, and for a struct field default, where they come from the type the literal is annotated with.

### Restrictions

- `self` cannot have a default.
- Function types do not carry default information; assigning a function with defaults to a `fn(...)` type erases them, and every call site of that variable must supply every argument.
- Closures cannot declare defaults: a closure value's arity must match its `fn(...)` type, so `= expr` on a closure parameter is a compile error.
- `export fn` cannot declare defaults — exported functions appear in the component's WIT signature where every parameter is required by the CM ABI. Split into a private helper plus a thin `export fn` wrapper if defaults are needed.
- Trait methods may declare defaults only in the trait definition; implementations receive every parameter and cannot add, remove, or change defaults. Direct `impl Type { ... }` methods (not part of any trait) may declare defaults freely.

### Type Parameter Defaults

A type parameter may declare a default with `= Type`, on a free function, an inherent method or a trait method. An omitted turbofish takes the default; a spelled one wins. `core:log` uses it:

```wado
pub fn info<T: Serialize = NoFields>(message: String, fields: T = NoFields {}, ...) { ... }

info("started");                  // → info::<NoFields>("started", NoFields {})
info::<Fields>("started", f);
```

Inference runs first and the default fills only what it left unbound, so an argument or an expected type always decides the slot it pins.

A default resolves in the declaring module's scope, as a value default does. It may therefore name a type the call site cannot: `NoFields` above is private to `core:log`. By the same rule a parameter the use site declares does not answer for it, however the two are spelled:

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

## Tagged Template Literals

A path written directly before a template literal is a tag. The template then
denotes a call of that function on the template's holes, in their own types,
with the literal text around them, instead of a rendered `String`:

```wado
let q = sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`;
let s = String::raw`${dir}\bin\run.exe`;   // backslashes kept
```

The tag is a function name or a static method path, with no whitespace before
the backtick. The literal is lexed exactly as an untagged template, so every
escape must still be one the lexer knows even where the tag preserves it.

A tag is an ordinary function whose one parameter is bound by `ReflectTemplate`,
the reflected kind of a template literal. The compiler synthesizes one anonymous
type per template shape — its segments, specifiers, hole types and hole source
texts — holding one field per hole. So `` tag`${a}` `` and `` tag`${b}` `` are
two types, each instantiating the tag, even where `a` and `b` share a type. The
type is unnameable and reached only through the bound; a diagnostic and
`Reflect::type_name()` show it as its text with each hole spelled by type,
`` `id = ${i32}` ``, cut at 50 characters. The tag walks the holes with tuple
`for-of`:

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
hole. Every answer but `get` and `fmt` is a constant.

`members()` walks a pack, so `Holes` is bound either as one (`[..V]`) or as the
empty tuple (`()`, for a tag that reads only `tail()`). A concrete tuple
(`Holes = [List<i32>]`) is an error at the call.

A hole's type may not mention a type parameter of the enclosing item, since the
shape is minted once rather than per instantiation. A generic body passes its
tag a concrete value from its caller. The untagged template makes no shape, so
`` `${v}` `` over a `v: X` is accepted where `` format`${v}` `` is not.

Holes are evaluated once, left to right, before the tag runs. A tag may carry
effects and return any type. Whether a call folds at compile time is the
optimizer's decision, as for any other call; the meaning does not depend on it.

An untagged template means what the prelude's `format` tag means: each hole
rendered through its specifier into one buffer.

See [WEP: Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md)
for the type, the desugaring and the cost model.

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
