# Functions

This chapter covers functions and what calls them: declarations and calls,
methods, generic functions, closures and function values, and default
arguments. It ends with variadic type packs, which let one function take tuples
of any arity.

## Function Declarations

A function declaration is `fn`, a name, a parameter list in parentheses, an
optional return type after `->`, and a block body. Each parameter is written
`name: Type`.

<!-- {"fixture":"spec_functions_declarations.wado"} -->

```wado
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

test "a declaration and a call" {
    assert add(2, 3) == 5;
}
```

Every parameter declares its type. Nothing infers it from the calls:

<!-- {"fixture":"spec_functions_param_type_required.wado"} -->

```wado
fn double(n) -> i32 {
    return n * 2;
}
```

Two parameters of one function may not share a name:

<!-- {"fixture":"spec_functions_duplicate_param.wado"} -->

```wado
fn area(side: i32, side: i32) -> i32 {
    return side * side;
}
```

A parameter is a local binding of the body. It holds the callee's own copy of
the argument ([Value Semantics](./spec-memory.md#value-semantics)). It is
immutable unless declared `mut` ([`mut` Parameters](./spec-memory.md#mut-parameters)).

### Return Values

A function returns a value only through `return`
([Statements](./spec-expressions.md#statements)). Every path through a
value-returning body must end in a `return`, or in a call that never returns,
such as `panic`:

<!-- {"fixture":"spec_functions_declarations.wado"} -->

```wado
fn sign(n: i32) -> i32 {
    if n < 0 {
        return -1;
    } else if n > 0 {
        return 1;
    } else {
        return 0;
    }
}

fn check_positive(n: i32) -> i32 {
    if n > 0 {
        return n;
    }
    panic("not positive");
}

test "every path returns" {
    assert sign(-5) == -1 && sign(0) == 0 && sign(9) == 1;
    assert check_positive(3) == 3;
}
```

A path that reaches the end of the body without a `return` is an error. The
error points at the function:

<!-- {"fixture":"spec_functions_missing_return.wado"} -->

```wado
fn sign(n: i32) -> i32 {
    if n < 0 {
        return -1;
    }
}
```

A function without `-> T` returns `()`, and `-> ()` says the same. Such a
function may leave early with `return;`:

<!-- {"fixture":"spec_functions_declarations.wado"} -->

```wado
fn record(log: &mut List<String>, line: String) {
    if line.is_empty() {
        return;              // an early exit from a unit function
    }
    log.push(line);
}

fn clear(log: &mut List<String>) -> () {
    log.truncate(0);
}

test "a unit function" {
    let mut log: List<String> = [];
    record(&mut log, "");
    record(&mut log, "a");
    assert log == ["a"];
    assert record(&mut log, "b") == ();
    clear(&mut log);
    assert log.is_empty();
}
```

A bare `return;` returns `()`. Every `return` is checked against the declared
return type, so `return;` is an error in a function that returns a value:

<!-- {"fixture":"error_return_missing_value.wado"} -->

```wado
fn f() -> i32 {
    return;
}
```

A value is an error in a function that returns `()`:

<!-- {"fixture":"error_return_value_from_unit_fn.wado"} -->

```wado
fn f() {
    return 1;
}
```

A function that never returns declares `-> !`
([The Never Type](./spec-types.md#the-never-type-)).

### Calls

A call `f(args)` passes one argument per parameter, in order. A trailing
parameter with a default may be left out ([Default Arguments](#default-arguments)).
Any other count is an error:

<!-- {"fixture":"spec_functions_arg_count.wado"} -->

```wado
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

test {
    assert add(1, 2, 3) == 6;
}
```

The arguments are evaluated left to right, and then the body runs:

<!-- {"fixture":"spec_functions_declarations.wado"} -->

```wado
fn next(log: &mut List<i32>, v: i32) -> i32 {
    log.push(v);
    return v;
}

fn digits(a: i32, b: i32, c: i32) -> i32 {
    return a * 100 + b * 10 + c;
}

test "arguments are evaluated left to right" {
    let mut order: List<i32> = [];
    let n = digits(next(&mut order, 1), next(&mut order, 2), next(&mut order, 3));
    assert n == 123 && order == [1, 2, 3];
}
```

### Scope

A module-level function is in scope in the whole module, before its declaration
as well as after it. Functions may call each other recursively:

<!-- {"fixture":"spec_functions_declarations.wado"} -->

```wado
test "a function declared later" {
    assert is_even(10) && !is_even(7);
}

fn is_even(n: i32) -> bool {
    if n == 0 { return true; }
    return is_odd(n - 1);
}

fn is_odd(n: i32) -> bool {
    if n == 0 { return false; }
    return is_even(n - 1);
}
```

A module declares each function name once. A second `fn` of the same name is an
error, not an overload:

<!-- {"fixture":"spec_functions_duplicate_fn.wado"} -->

```wado
fn scale(x: i32) -> i32 { return x * 2; }
fn scale(x: f64) -> f64 { return x * 2.0; }
```

### Visibility and Effects

A function without a modifier is private to its file. `internal`, `pub` and
`export` widen its reach, as [Visibility](./spec-modules.md#visibility)
specifies.

A `with` clause after the return type lists the effects the function performs,
as [Effect Declaration in Functions](./spec-effects.md#effect-declaration-in-functions)
specifies.

## Methods

An `impl Type { … }` block declares methods of `Type`.
[Inherent Impls](./spec-traits.md#inherent-impls) says where such a block may be
written, and
[Impl Type Parameters Are Declared](./spec-traits.md#impl-type-parameters-are-declared)
covers `impl<T>`.

A method whose first parameter is `&self` or `&mut self` is an instance method.
A method without that receiver is a static method.
[Method Receiver: `self` by Value](./spec-memory.md#method-receiver-self-by-value)
says when the receiver may be a bare `self`.

An instance method is called on a value with `.`. A static method is called on
the type with `::`:

<!-- {"fixture":"spec_functions_methods.wado"} -->

```wado
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn new(x: i32, y: i32) -> Point {
        return Point { x, y };
    }

    fn sum(&self) -> i32 {
        return self.x + self.y;
    }

    fn scale(&mut self, k: i32) {
        self.x *= k;
        self.y *= k;
    }
}

test "instance and static methods" {
    let mut p = Point::new(1, 2);
    assert p.sum() == 3;
    p.scale(10);
    assert p.x == 10 && p.y == 20;
}
```

A static method has no receiver, so calling one on a value is an error:

<!-- {"fixture":"spec_functions_static_with_dot.wado"} -->

```wado
let p = Point { x: 1, y: 2 };
assert p.origin().x == 0;
```

A method written with `.` may also be called as `Type::method(recv, args…)`
([Qualified Calls](./spec-traits.md#qualified-calls)).

### The Receiver

Inside a method, `self` is the receiver. Its fields and the type's other methods
are reached through it. A bare field name is not in scope:

<!-- {"fixture":"spec_functions_method_bare_field.wado"} -->

```wado
impl Point {
    fn sum(&self) -> i32 {
        return x + y;
    }
}
```

A `&self` or `&mut self` method takes a reference to its receiver. So the
receiver may be a value, a `&T` or a `&mut T`:

<!-- {"fixture":"spec_functions_methods.wado"} -->

```wado
test "a call through a reference" {
    let mut p = Point::new(1, 2);
    let r = &p;
    assert r.sum() == 3;           // `&self` through `&Point`
    let m = &mut p;
    m.scale(2);                    // `&mut self` through `&mut Point`
    assert p.sum() == 6;
}
```

A `&mut self` method writes to its receiver, so the receiver must be mutable. An
immutable binding is an error:

<!-- {"fixture":"spec_functions_method_mut_immutable.wado"} -->

```wado
let c = Counter { n: 0 };
c.bump();
assert c.n == 1;
```

A `&T` does not reach a `&mut self` method either, even when the binding behind
it is mutable:

<!-- {"fixture":"spec_functions_method_mut_through_ref.wado"} -->

```wado
let mut c = Counter { n: 0 };
let r = &c;
r.bump();
assert c.n == 1;
```

### `Self`

Inside an `impl`, `Self` names the impl's type. It stands for that type in a
signature or a body, and it prefixes the type's static methods and, on a
variant, its cases ([Variants](./spec-types.md#variants)). A call may follow
another on the value the first one returns:

<!-- {"fixture":"spec_functions_methods.wado"} -->

```wado
impl Point {
    fn origin() -> Self {
        return Self::new(0, 0);
    }

    fn moved(&self, dx: i32, dy: i32) -> Self {
        return Self::new(self.x + dx, self.y + dy);
    }

    fn doubled_sum(&self) -> i32 {
        return self.sum() * 2;     // another method, through `self`
    }
}

test "Self and chained calls" {
    assert Point::origin().sum() == 0;
    assert Point::origin().moved(1, 2).moved(3, 4).doubled_sum() == 20;
}
```

### Several Impl Blocks

A type may have several inherent `impl` blocks, as `Point` above does. Their
methods form one set, so a name is declared once across all of them:

<!-- {"fixture":"spec_functions_method_duplicate.wado"} -->

```wado
impl Point {
    fn sum(&self) -> i32 { return self.x + self.y; }
}

impl Point {
    fn sum(&self) -> i32 { return self.x; }
}
```

Fields and methods are looked up apart. A field and a method may share a name:
`c.count` reads the field, and `c.count()` calls the method.

<!-- {"fixture":"spec_functions_methods.wado"} -->

```wado
struct Counter {
    count: i32,
}

impl Counter {
    fn count(&self) -> i32 {
        return self.count * 10;
    }
}

test "a field and a method may share a name" {
    let c = Counter { count: 4 };
    assert c.count == 4 && c.count() == 40;
}
```

When a trait declares a method of the same name,
[Method Resolution](./spec-traits.md#method-resolution) says which one a call
reaches.

### Associated Constants

An `impl` block may declare a `const` with a type and a value. It is named
through the type, as a static method is, and it cannot be assigned:

<!-- {"fixture":"spec_types_assoc_const.wado"} -->

```wado
struct Board {
    cells: List<i32>,
}

impl Board {
    pub const SIZE: i32 = 8;
}

test {
    assert Board::SIZE * Board::SIZE == 64;
    assert f64::PI > 3.14;
}
```

## Generic Functions

A function declares type parameters in `<…>` after its name. The parameter
types, the return type and the body may all name them:

<!-- {"fixture":"spec_functions_generics.wado"} -->

```wado
fn first<T>(items: List<T>) -> Option<T> {
    if items.is_empty() {
        return null;
    }
    return Some(items[0]);
}

fn pair<A, B>(a: A, b: B) -> [A, B] {
    return [a, b];
}

test "type parameters" {
    assert first([7, 8]) == Some(7);
    assert first(["x"]) == Some("x");
    let p = pair(1, "one");
    assert p.0 == 1 && p.1 == "one";
}
```

A type parameter may declare a default
([Type Parameter Defaults](#type-parameter-defaults)). A function may also take
a pack of them ([Variadic Type Packs](#variadic-type-packs)).

### Type Arguments

A call settles each type parameter from its arguments and from the type expected
of the call. [Generic Type Inference](./spec-types.md#generic-type-inference)
gives the rules. A turbofish, `f::<T>(…)`, writes the type arguments at the
call instead:

<!-- {"fixture":"spec_functions_generics.wado"} -->

```wado
fn zero<T: Default>() -> T {
    return T::default();
}

test "a turbofish" {
    assert zero::<i32>() == 0;
    let s: String = zero();          // or the expected type settles `T`
    assert s == "";
    let n = first::<i64>([5]);       // the literal becomes an `i64`
    let wide: i64 = n.unwrap();
    assert wide == 5;
}
```

A type parameter that nothing settles is an error:

<!-- {"fixture":"spec_functions_generic_uninferable.wado"} -->

```wado
fn zero<T: Default>() -> T {
    return T::default();
}

test {
    zero();
}
```

A turbofish may not name more type arguments than the function declares:

<!-- {"fixture":"spec_functions_turbofish_count.wado"} -->

```wado
fn identity<T>(x: T) -> T {
    return x;
}

test {
    assert identity::<i32, i32>(1) == 1;
}
```

### Bounds

A type parameter may carry bounds, such as `T: Ord` or `T: Ord + Display`.
[Trait Bounds](./spec-traits.md#trait-bounds) specifies them. A bound lets the
body use what its traits provide: `>` from `Ord`, `${a}` from `Display`, and
`T::default()` from `Default`:

<!-- {"fixture":"spec_functions_generics.wado"} -->

```wado
fn largest<T: Ord>(items: List<T>) -> T {
    let mut best = items[0];
    for let x of items {
        if x > best {
            best = x;
        }
    }
    return best;
}

fn describe<T: Ord + Display>(a: T, b: T) -> String {
    return if a < b { `${a} < ${b}` } else { `${a} >= ${b}` };
}

test "bounds" {
    assert largest([3, 9, 2]) == 9;
    assert largest(["b", "c", "a"]) == "c";
    assert describe(1, 2) == "1 < 2";
}
```

The body may use nothing else. A method that no bound declares is an error,
whatever type a call would pass:

<!-- {"fixture":"spec_functions_generic_no_bound_method.wado"} -->

```wado
fn size<T>(x: T) -> i32 {
    return x.len();
}
```

An operator follows the same rule. A comparison needs `T: Ord`, and `==` needs
`T: Eq`:

<!-- {"fixture":"error_generic_compare_unbounded.wado"} -->

```wado
fn less<T>(a: T, b: T) -> bool {
    return a < b;
}
```

A call checks its type arguments against the bounds:

<!-- {"fixture":"spec_functions_generic_bound_unmet.wado"} -->

```wado
trait Named {
    fn name(&self) -> String;
}

fn greet<T: Named>(x: &T) -> String {
    return `Hello, ${x.name()}`;
}

test {
    assert greet(&1) == "Hello, 1";
}
```

### Generic Methods

A method may declare type parameters of its own, after its name. They sit beside
the ones its `impl` declares. A turbofish on a method call follows the method
name:

<!-- {"fixture":"spec_functions_generics.wado"} -->

```wado
struct Stack<T> {
    items: List<T>,
}

impl<T> Stack<T> {
    fn map<U>(&self, f: fn(T) -> U) -> Stack<U> {
        let mut out: List<U> = [];
        for let x of self.items {
            out.push(f(x));
        }
        return Stack { items: out };
    }
}

test "a generic method" {
    let s = Stack { items: [1, 2] };
    assert s.map(|x| `${x}`).items == ["1", "2"];
    assert s.map::<i64>(|x| x as i64 * 10).items == [10, 20];
}
```

## Closures

A closure is an anonymous function expression, written `|params| body`. An
expression body returns its value implicitly:

<!-- {"fixture":"spec_functions_closures.wado"} -->

```wado
let add_one = |x: i32| x + 1;
let make_point = |x: i32, y: i32| Point { x, y };
assert add_one(41) == 42;
assert make_point(1, 2).y == 2;
```

A block body requires explicit `return`:

<!-- {"fixture":"spec_functions_closures.wado"} -->

```wado
let compute = |x: i32| {
    let doubled = x * 2;
    return doubled + x * 3;
};
assert compute(2) == 10;
```

An optional `-> Type` declares the return type. A `?` in the body needs a known
return type: this one, or the `R` of an expected `fn(..) -> R`.

<!-- {"fixture":"spec_functions_closures.wado"} -->

```wado
let parse = |s: String| -> Result<i32, String> {
    let n = to_int(s)?;
    return Result::Ok(n + 1);
};
assert parse("41").unwrap() == 42;
assert parse("x").is_err();
```

A parameter type is inferred from the expected `fn(..)` type, matched by
position. Any context that supplies such a type counts: a typed binding, a
function or method parameter, a struct field, a newtype over a `fn(..)`.
Annotate only where nothing supplies one; an annotation always wins:

<!-- {"fixture":"spec_functions_closures.wado"} -->

```wado
let arr: List<i32> = [1, 2, 3];
assert arr.into_iter().map(|x| x * 2).collect() == [2, 4, 6];  // `x: i32`, from `Iterator::Item`
assert arr.into_iter().fold(0, |acc, x| acc + x) == 6;         // `acc: i32`, from the body
let add_one = |x: i32| x + 1;                                    // no expected type: annotate
assert add_one(1) == 2;
```

The expected type may be one of the callee's own type parameters. A sibling
argument then supplies it, whichever side of the closure it is written on. A
numeric-literal sibling does not: `fold(0, |acc, x| acc + x)` over a `List<i64>`
takes `i64` from the body, not `i32` from the `0`. A parameter nothing supplies
is reported at the call, not inside the closure.

A closure cannot declare effects. Its effects are inferred from its body, and a
`with` after the parameter list or after `-> Type` is a compile error. So a
closure whose body is a `with … do` handler writes it in parentheses or a block:

<!-- {"fixture":"spec_functions_closure_handler.wado"} -->

```wado
let mut f = || (with Log => &mut sink do { Log::emit(`hi`); });
f();
assert sink.lines == ["hi"];
```

### Closure Types

A closure's type is `fn(P...) -> R` when every capture is read-only, and
`fn mut(P...) -> R` when any capture is written (see [Capture](#capture)).
`fn` is a subtype of `fn mut`: a `fn` closure is accepted where a `fn mut` is
expected, and a `fn mut` closure where a `fn` is expected is a compile error.

<!-- {"fixture":"spec_functions_closure_types.wado"} -->

```wado
fn apply(f: fn(i32) -> i32, x: i32) -> i32 { return f(x); }
fn apply_twice(mut f: fn mut(i32) -> i32, x: i32) -> i32 { return f(f(x)); }

test {
    let n = 1;
    assert apply_twice(|x| x + n, 5) == 7;      // OK: a `fn` where `fn mut` is expected
    assert apply(|x| x * 2, 5) == 10;
}
```

The other direction is rejected:

<!-- {"fixture":"spec_functions_fn_mut_where_fn.wado"} -->

```wado
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

<!-- {"fixture":"spec_functions_closures.wado"} -->

```wado
let f: fn(i32) -> i32 with Stdout = |x| { println(`${x}`); return x; };
assert f(7) == 7;
```

A type parameter may be bounded by a closure type. The bound names the type
once, so the parameter can be reused across positions:

<!-- {"fixture":"spec_functions_closure_bound.wado"} -->

```wado
fn apply<F: fn(i32) -> i32>(f: F, x: i32) -> i32 { return f(x); }
fn invoke<F: fn mut(i32)>(mut f: F) { f(1); f(2); }
fn dup<F: fn(i32) -> i32>(f: F) -> [F, F] { return [f, f]; }

test {
    assert apply(|x| x + 1, 1) == 2;
    let mut sum = 0;
    invoke(|x| sum += x);
    assert sum == 3;
    let [f, g] = dup(|x| x * 10);
    assert f(1) + g(2) == 30;
}
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

<!-- {"fixture":"spec_functions_call_fn_mut.wado"} -->

```wado
fn run(mut f: fn mut(i32)) { f(1); f(2); }   // `mut f` is required

test {
    let mut count = 0;
    let mut c = || count += 1;                   // `mut c` is required
    c();
    run(|x| count += x);
    assert count == 4;
}
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

<!-- {"fixture":"spec_functions_capture.wado"} -->

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

<!-- {"fixture":"spec_functions_capture.wado"} -->

```wado
let mut count = 0;
let mut outer = || {
    let mut inner = || count += 1;   // writes the function's `count`
    inner();
};
outer();
assert count == 1;
```

To capture a snapshot, copy the value into a local first and let the closure
capture that:

<!-- {"fixture":"spec_functions_capture.wado"} -->

```wado
let snapshot = original;     // a copy, independent of `original`
let f = || snapshot * 2;
original = 5;
assert f() == 2;
```

A closure capturing an outer binding is a separate matter from what a function
does with its reference _parameters_; see
[Reference Retention](./spec-memory.md#reference-retention).

### Closures Are Values

A closure is copied on assignment, on passing and on return, like any other
value. Its captures are references, so every copy still refers to the same
bindings:

<!-- {"fixture":"spec_functions_capture.wado"} -->

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

<!-- {"fixture":"spec_functions_fn_refs.wado"} -->

```wado
fn double(n: i32) -> i32 { return n * 2; }

test {
    let f = double;            // type: fn(i32) -> i32
    assert f(21) == 42;

    assert apply(double, 21) == 42;   // pass directly; no `&` needed
    let g: fn(i32) -> i32 = double;
    assert g(1) == 2;
}
```

A function value carries no observable identity. It holds no state to observe,
and two `fn` values cannot be compared.

`&` and `&mut` apply to a function value as to any other value
([Reference Types](./spec-memory.md#reference-types)). `&f` has type
`&fn(...)`, and `&mut f` on a mutable binding has type `&mut fn(...)`.
`&fn(...)` is not a synonym for `fn(...)`, so passing one where the other is
expected is a type error. Through a `&mut fn(...)` parameter the callee may
store another function (`*p = other_fn`), and the caller sees it.

A `&fn(...)` or `&mut fn(...)` is callable directly: the call dereferences it,
so `let r = &double; r(21)` needs no `*r`.

A generic function taken as a value needs its type arguments pinned, in one of
two ways:

- A turbofish on the name: `let f = identity::<i32>;` is a `fn(i32) -> i32`,
  and so is `identity::<i32>` passed as `apply(identity::<i32>, 7)`.
- An expected `fn(...)` type at the use site: `let f: fn(i32) -> i32 = identity;`,
  or `apply(identity, 7)` where `apply`'s parameter is `fn(i32) -> i32`. The
  expected signature settles the type arguments by position.

Without either, it is a compile error, which suggests a turbofish or a closure
wrapper (`|x| identity(x)`).

A function type crosses the Component Model boundary only as a
[callback](./spec-components.md#callbacks).

## Default Arguments

Trailing function parameters may declare default values with `= expr`. A call
that omits a defaulted argument gets the default expression filled in at the
call site:

<!-- {"fixture":"spec_functions_default_args.wado"} -->

```wado
fn connect(host: String, port: i32 = 8080, timeout: i32 = 30) -> String { return `${host}:${port}/${timeout}`; }

test "trailing defaults" {
    assert connect("localhost") == "localhost:8080/30";           // → connect("localhost", 8080, 30)
    assert connect("localhost", 3000) == "localhost:3000/30";     // → connect("localhost", 3000, 30)
    assert connect("localhost", 3000, 60) == "localhost:3000/60";
}
```

A parameter without a default cannot follow one with a default, and `self`
cannot have a default:

<!-- {"fixture":"spec_functions_default_order.wado"} -->

```wado
fn foo(a: i32 = 0, b: i32) { }   // ERROR: parameters without defaults cannot follow
```

### Default Expressions

A default is any expression that performs no effect. It is evaluated at each
call that omits the argument, not once at the declaration. A default that
performs an effect is a compile error:

<!-- {"fixture":"spec_functions_default_args.wado"} -->

```wado
fn foo(
    x: i32 = 0,                   // literal
    y: f64 = f64::PI,             // associated constant
    z: String = `default`,        // template string
    w: Option<Config> = null,     // Option::None
    v: Color = Color::Red,        // enum case
    u: i32 = i32::max(1, 2),      // pure function call
) -> String { return `${x} ${y:.2} ${z} ${w matches { None }} ${v} ${u}`; }

test "default expressions" {
    assert foo() == "0 3.14 default true Red 2";
    assert foo(1, 0.5, "s", Option::Some(Config { name: "c" }), Color::Green, 7) == "1 0.50 s false Green 7";
}
```

<!-- {"fixture":"spec_functions_default_effect.wado"} -->

```wado
fn noisy() -> i32 with Stdout { println("noisy"); return 1; }
fn bar(v: i32 = noisy()) { }  // ERROR: the default performs `Stdout`
```

A default may name an earlier parameter, and then reads the value the call
supplied for it. Each argument is still evaluated once, in its place among the
others, with the receiver first:

<!-- {"fixture":"spec_functions_default_args.wado"} -->

```wado
fn make_rect(width: f64, height: f64 = width) -> Rect { return Rect { width, height }; }

fn twice(a: i32, b: i32 = a + a) -> i32 { return b; }

test "a default names an earlier parameter" {
    assert make_rect(10.0).height == 10.0;   // → make_rect(10.0, 10.0)

    assert twice(next()) == 2;               // `next()` runs once
    assert CALLS == 1;
}
```

Where the named parameter is a `&mut`, the default borrows the same place again,
and the callee's writes still reach it:

<!-- {"fixture":"spec_functions_default_args.wado"} -->

```wado
fn put(o: &mut Option<i32>, v: i32 = peek(o) + 1) { *o = Option::Some(v); }

test "a default borrows a `&mut` parameter's place again" {
    let mut s = S { o: Option::Some(1) };
    put(&mut s.o);                   // → put(&mut s.o, peek(&mut s.o) + 1)
    assert s.o == Option::Some(2);
}
```

A default may name a type parameter of the declaration that wrote it. It stands
for the type argument the call site settled on, whether a turbofish spelled it,
an argument beside it pinned it, or the parameter's own default supplied it:

<!-- {"fixture":"spec_functions_default_args.wado"} -->

```wado
fn info<T: Default>(msg: String, fields: T = T::default()) -> String { return `${msg} ${fields:?}`; }

test "a default names a type parameter" {
    assert info::<i32>("count") == "count 0";  // → info::<i32>("count", 0)
}
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

<!-- {"fixture":"spec_functions_default_scope.wado"} -->

```wado
// sub/spec_functions_default_scope_lib.wado:
//     global DEFAULT_PORT: i32 = 8080;             // private to that module
//     pub fn connect(host: String, port: i32 = DEFAULT_PORT) -> i32 { return port; }
use { connect } from "./sub/spec_functions_default_scope_lib.wado";

test {
    let DEFAULT_PORT = 1;
    assert connect("localhost") == 8080;            // port is the library's 8080
}
```

`#file`, `#line` and `#function` are the exception: they evaluate at the call
site (see
[Call-site evaluation in default arguments](./spec-literals.md#call-site-evaluation-in-default-arguments)).

### Restrictions

- A function type carries no defaults. A function with defaults assigned to a `fn(...)` type loses them, and every call through that value supplies every argument.
- A closure cannot declare defaults, since its arity must match its `fn(...)` type. `= expr` on a closure parameter is a compile error.
- An `export fn` cannot declare defaults, since the component's WIT signature requires every parameter. A private helper can declare them behind a thin `export fn` wrapper.
- An `interface` operation or a `#[cm]` resource method may declare defaults. The call fills them in, so the handler or the host receives every argument.
- A trait method declares its defaults in the trait only, including in a default body there. An implementation receives every parameter, and one that writes a default is a compile error. Every spelling of a call fills the trait's defaults: `x.m()`, `Type::m(..)`, and `T::m(..)` through a bound. A method of an inherent `impl Type { ... }` may declare defaults freely.

### Type Parameter Defaults

A type parameter may declare a default with `= Type`, on a free function, an inherent method or a trait method. An omitted turbofish takes the default; a spelled one wins. `core:log` uses it:

<!-- {"fixture":"spec_functions_type_param_defaults.wado"} -->

```wado
pub fn info<T: Serialize = NoFields>(message: String, fields: T = NoFields {}) -> String { return `${message} ${to_string(&fields).unwrap()}`; }

test "an omitted turbofish takes the default" {
    let f = Fields { user: 1 };
    assert info("started") == "started {}";                         // → info::<NoFields>("started", NoFields {})
    assert info::<Fields>("started", f) == "started {\"user\":1}";
}
```

Inference runs first and the default fills only what it left unbound, so an argument or an expected type always decides the slot it pins.

A default resolves in the declaring module's scope, as a [value default does](#where-a-default-resolves). It may therefore name a type the call site does not import: a caller of `core:log`'s `info` names no `NoFields`. By the same rule a parameter the use site declares does not answer for it, however the two are spelled:

<!-- {"fixture":"spec_functions_type_param_defaults.wado"} -->

```wado
struct Zero {}
struct Marked<M: Mark = Zero> { value: i32 }

fn f<Zero: Mark>(probe: Zero) -> i32 {
    let m: Marked = Marked { value: 1 };  // the module's `Zero`, not `f`'s
    return m.total();
}

test {
    assert f(One {}) == 1;                // not 101: `f`'s `Zero` is `One` here
}
```

A default may name a parameter to its left, and stands for that parameter's argument. One naming a parameter at or after its own slot is rejected, since no argument has settled it yet:

<!-- {"fixture":"spec_functions_type_param_defaults.wado"} -->

```wado
struct Both<A, B = A> { v: A }        // OK: `B` takes `A`'s argument

test {
    let b: Both<i32> = Both { v: 1 };
    assert b.v == 1;
}
```

<!-- {"fixture":"spec_functions_type_param_default_forward.wado"} -->

```wado
struct Fwd<A = B, B = i32> { v: B }   // ERROR: `A`'s default names `B`
struct Own<A = A> { v: i32 }          // ERROR: the same, one slot nearer
```

Expanding a default must reach a fixpoint. One that leads back to the declaration it belongs to is rejected, whether it names that declaration directly, under an argument, or through another declaration's defaults:

<!-- {"fixture":"spec_functions_type_param_default_cycle.wado"} -->

```wado
struct Rec<T = Rec> { v: i32 }           // ERROR
struct Pair<A, B = Pair<A>> { v: i32 }   // ERROR
struct Ping<X, Y = Pong<X>> { v: i32 }   // ERROR, paired with
struct Pong<X, Y = Ping<X>> { v: i32 }   // this one
```

A trait method's type parameter default belongs to the trait, as its value defaults do. The implementation restates the same parameters in the same order and omits the defaults. Every spelling of the call fills them from the trait's declaration:

<!-- {"fixture":"spec_functions_trait_type_param_default.wado"} -->

```wado
pub trait Boxed {
    fn boxed<T: Named = Tag>(&self) -> String;   // the caller need not import `Tag`
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

test {
    let m = M {};
    assert m.boxed() == "Tag";              // → m.boxed::<Tag>()
    assert m.boxed::<Local>() == "Local";   // spelled, so `Local`
    assert M::made() == "Tag";              // the static spelling reads the same declaration
    assert M::boxed(&m) == "Tag";           // and so does the receiver-taking one
}
```

Wado accepts a type parameter default wherever a parameter list is written. Rust accepts one only on a type or trait declaration (rust-lang/rust#36887).

The same `= expr` syntax applies to struct fields; see [Struct Field Defaults](./spec-types.md#struct-field-defaults).

Rationale: [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

## Variadic Type Packs

`<..T>` declares a type pack: a parameter that stands for zero or more types. A
function over a pack takes tuples of any arity.

<!-- {"fixture":"spec_functions_type_packs.wado"} -->

```wado
fn identity<..T>(x: [..T]) -> [..T] {
    return x;
}

fn prepend<A, ..T>(a: A, rest: [..T]) -> [A, ..T] {
    return [a, ..rest];
}

test {
    assert identity([1, "hello", true]) == [1, "hello", true];
    assert prepend(0, [1, "x"]) == [0, 1, "x"];
}
```

A pack is declared with the `..` prefix in a generic parameter list: `<..T>`,
`<A, ..T>`. `...` (three dots) is a parse error, which suggests `..`. A pack
cannot be an effect parameter, so `<effect ..T>` is invalid. A list may declare
more than one pack ([Multiple Type Packs](#multiple-type-packs)).

A pack is used inside a tuple type as `[..T]`, alone or beside fixed elements:
`[A, ..T]`, `[..T, B]`. A call infers the pack from the tuple it passes. A
[value spread](./spec-literals.md#value-spread) builds a value of such a type,
as `[a, ..rest]` does.

### Multiple Type Packs

A parameter list may declare more than one pack. Each is settled from the
argument that carries it alone, so nothing has to find a boundary that was
never written:

<!-- {"fixture":"spec_functions_type_packs.wado"} -->

```wado
fn concat<..A, ..B>(a: [..A], b: [..B]) -> [..A, ..B] {
    return [..a, ..b];
}

test {
    assert concat([1, "x"], [true]) == [1, "x", true];   // A = [i32, String], B = [bool]
}
```

A tuple holding two packs (`[..A, ..B]`) settles neither pack, because matching
it against a concrete tuple admits every split. Such a tuple is legal anywhere.
It just cannot be the only thing naming a pack. Something else must settle each
one: another parameter, a turbofish, or an annotation. The tuple is then checked
against the arity they fix.

<!-- {"fixture":"spec_functions_type_packs.wado"} -->

```wado
fn wrap<X, ..A, ..B, Y>(x: X, a: [..A], b: [..B], y: Y) -> [X, ..A, ..B, Y] {
    return [x, ..a, ..b, y];   // the parameters settle both packs
}

fn joined<..A, ..B>(split: [[..A], [..B]], both: [..A, ..B]) -> i32 { return 2; }

fn middle<..Pre, K, ..Post>(t: [..Pre, K, ..Post]) -> i32 { return 3; }

test {
    assert wrap(0, [1], ["a"], true) == [0, 1, "a", true];
    assert joined([[1], [true]], [1, true]) == 2;          // `split` settles both
    assert middle::<[i32], String, [bool]>([1, "mid", true]) == 3;   // the turbofish settles them
}
```

Without them, the same functions are rejected:

<!-- {"fixture":"spec_functions_type_packs_unsettled.wado"} -->

```wado
joined([[1], [true]], [1, true, "x"]);    // ERROR: expected `[i32, bool]`
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

<!-- {"fixture":"spec_functions_type_packs.wado"} -->

```wado
fn reorder<..A, ..B>(a: [..A], b: [..B]) -> [..A, ..B] {
    let ab: [..A, ..B] = [..a, ..b];      // OK
    return ab;
}

test {
    assert reorder([1], ["x", true]) == [1, "x", true];
}
```

<!-- {"fixture":"spec_functions_type_packs_layout.wado"} -->

```wado
let ba: [..B, ..A] = [..a, ..b];   // ERROR: the order is part of the type
let shifted: [i32, ..A] = [..a];   // ERROR: so is a fixed element
```

A turbofish spells each pack as its own tuple:

<!-- {"fixture":"spec_functions_type_packs.wado"} -->

```wado
assert concat::<[i32], [bool, String]>([1], [true, "x"]) == [1, true, "x"];
```

A flat list of types carries no boundary, so it is refused, even with one type
per pack. `<i32, String>` could mean `<[i32], [String]>` or
`<[i32, String], []>`, and nothing says which:

<!-- {"fixture":"spec_functions_type_packs_flat_turbofish.wado"} -->

```wado
concat::<i32, String>([1], ["x"]);  // ERROR: spell each type pack as a tuple
```

A function with a single pack still takes the flat form, since that pack
takes every type the scalar parameters leave over
(`make_defaults::<i32, String>()`).

`zip` transposes its operands position by position, so its rows must be equally
long. Two distinct packs are never known to be, so `[[..a], [..b]].zip()` is
rejected where it is written.
