# Control Flow

## Conditional Statements

```wado
if condition {
    // then block
} else {
    // else block
}

// else-if chains
if x < 0 {
    println("negative");
} else if x == 0 {
    println("zero");
} else {
    println("positive");
}
```

### If Expression

```wado
let abs = if x < 0 { -x } else { x };

let grade = if score >= 90 { "A" } else if score >= 80 { "B" } else { "C" };
```

### If Let Pattern Matching

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
if let Some(x) = opt {
    println(`Got: ${x}`);
} else {
    println("None");
}
```

### Match Ergonomics

When the scrutinee of `if let`, `match`, or `matches` is a reference type (`&T` or `&mut T`), patterns match against the underlying type. Payload bindings become references — e.g. matching `&Option<T>` with `Some(x)` gives `x: &T`, not `x: T` (Rust-compatible, RFC 2005).

A destructuring `let` or `for` binding follows the same rule, and so does a reference met below the top of a pattern: `for let [a, b] of &pairs` gives `a: &A`, and `[n, { x, .. }]` against `[i32, &Point]` gives `x: &i32`.

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
let ro = &opt;
if let Some(x) = ro {       // ro: &Option<i32>, x: &i32
    println(`Got: ${*x}`);   // dereference to use the value
}
```

### Let Else

`let PATTERN = EXPR else { ... };` binds a refutable pattern whose bindings
escape into the enclosing scope. On a match they cover the rest of the block;
otherwise the `else` block runs and must diverge (`return`, `break`,
`continue`, `panic`, …). The `else` block does not see the bindings.

```wado
fn parse_port(s: String) -> i32 {
    let Ok(port) = i32::from_str(&s) else {
        return -1;
    };
    return port;                // `port` is in scope here
}
```

An irrefutable pattern is rejected — its `else` could never run; use a plain
`let`.

## While Loop

```wado
let mut i = 0;
while i < 10 {
    println(`i = ${i}`);
    i = i + 1;
}
```

### While Let Pattern Matching

`while let` allows iterating while a pattern matches:

```wado
let items: List<i32> = [1, 2, 3];
let mut iter = items.into_iter();

while let Some(x) = iter.next() {
    println(`${x}`);
}
```

The loop continues as long as the pattern matches. When the pattern fails to match (e.g., `iter.next()` returns `None`), the loop exits.

## For Loop

C-style for loop with initialization, condition, and update. Parentheses are optional:

```wado
for let mut i = 0; i < 10; i = i + 1 {
    println(`i = ${i}`);
}

// With parentheses (also valid)
for (let mut i = 0; i < 10; i = i + 1) {
    println(`i = ${i}`);
}

// All parts are optional
for ;; {
    // infinite loop
}
```

### Note

`continue` in a for loop executes the update expression before the next iteration, matching C semantics.

### For with Pattern Condition

The condition part of a C-style for loop can use `let` pattern matching:

```wado
let items: List<i32> = [10, 20, 30];
let mut iter = items.into_iter();

for ; let Some(x) = iter.next(); {
    println(`${x}`);
}

// With update expression
let mut count = 0;
for ; let Some(x) = iter.next(); count += 1 {
    println(`item ${count}: ${x}`);
}
```

The loop continues as long as the pattern matches. This is useful for iterating with additional state (like a counter) alongside pattern matching.

## For-Of Loop

For iterating over any type that implements `IntoIterator`:

```wado
let numbers: List<i32> = [1, 2, 3, 4, 5];
for let n of numbers {
    println(`${n}`);
}

// With mutable binding
for let mut item of items {
    item = item * 2;  // Can modify the local binding
    println(`${item}`);
}

// Custom types that implement IntoIterator also work
for let x of my_collection {
    println(`${x}`);
}
```

### Semantics

`for let item of collection { body }` calls `collection.into_iter()` once,
before the first iteration. Each iteration then calls `next()` on the result,
binds the element to `item` and runs the body. The loop ends when `next()`
returns `None`.

The binding is a copy of each element (value semantics), so modifying it does
not affect the original collection.

The binding must match every element, as a `let` pattern must (see
[Patterns That Cannot Fail](#patterns-that-cannot-fail)). A pattern that can
fail (`for let Some(x) of xs`, a narrowing type pattern) is a compile error.
Match on the element in the body instead.

### Tuple for-of (compile-time expansion)

When the iterable is a tuple, the loop body is expanded once per element at compile time. Each expansion independently types the binding, enabling heterogeneous iteration with per-element trait dispatch.

```wado
let t = [42, "hello", true];
for let v of t {
    println(`${v}`);  // expanded to three blocks, each with the correct type
}
```

`break` and `continue` are not allowed inside a tuple for-of body because the loop is unrolled at compile time into sequential blocks and these have no natural target. `.enumerate()` is supported and provides a compile-time index.

### Tuple comprehension

Wrapping the same walk in `[...]` collects one result element per source element. The braces hold a single expression — the element's value — since every position of the result tuple has one.

```wado
impl<..T: Doubled> Doubled for [..T] {
    fn doubled(&self) -> [..T] {
        return [for let v of *self { v.doubled() }];
    }
}
```

The `.enumerate()` form binds the index alongside the value (`[for let [i, v] of t.enumerate() { ... }]`). The index is a compile-time constant, so it is also the one non-literal a tuple accepts as a subscript (`t[i]`), for reads and writes alike.

The source must be a variadic tuple (`[..T]`); a concrete tuple is not walkable this way.

## Infinite Loop

```wado
loop {
    // runs forever until break
    if should_exit() {
        break;
    }
}
```

## Break and Continue

`break` exits the innermost enclosing loop. `continue` skips to the next iteration.

```wado
// break example
let mut i = 0;
while i < 100 {
    if i == 10 {
        break;  // exit the loop
    }
    i = i + 1;
}

// continue example
for let mut i = 0; i < 10; i = i + 1 {
    if i == 5 {
        continue;  // skip printing 5
    }
    println(`${i}`);
}
```

Both `break` and `continue` work with `while`, `for`, and `loop`, and a loop
carries no label. An unlabeled `break` or `continue` acts on the innermost loop,
and outside a loop both are errors. `continue` never takes a label.
`break LABEL` is a different statement: it leaves an enclosing labeled block,
needs no loop of its own, and is how to leave more than the innermost loop. A
closure body starts its own loop scope, so a loop around a closure binds nothing
written inside it.

## Labeled Blocks

A labeled block is a named block that `break LABEL` leaves from anywhere inside
it. It is Wado's only non-local jump, and it replaces loop labels, labeled
`continue`, and `goto`. It is also the block form that carries a value.

### Syntax

`LABEL: { ... }`, in statement or expression position.

- The label is an identifier followed by a colon
- `break LABEL;` leaves the block; `break LABEL: expr;` leaves it with a value
- `()` is a value like any other, so `break LABEL: ()` says what `break LABEL`
  says, and `break ()` says what `break` says
- Nested blocks may reuse a label name; a `break` targets the innermost match
- The block opens a new scope, and a name declared inside may shadow an outer
  one

The label is mandatory to tell a block apart from a struct literal, since
`{ field: value }` on its own could be either. An unlabeled `{ ... }` block is
therefore a parse error.

### Escaping and Early Exit

`break` alone leaves the innermost loop. A labeled block around a nest leaves
all of it at once, and its tail is the path no `break` took. Inside a block,
`break LABEL` skips the rest, so a chain of guards stays flat instead of nesting
one inside the next. A `break` may also leave an effect handler's `do` block,
which restores the outer handler either way.

```wado
search: {
    for let r of 0..<grid.len() {
        for let c of 0..<grid[r].len() {
            if grid[r][c] == needle {
                hit = [r, c];
                break search;   // leaves both loops
            }
        }
    }
    hit = [-1, -1];             // reached only when no break was taken
}
```

### As an Expression

A labeled block used as a value yields two ways: through `break LABEL: expr`,
and through its trailing statement on the path that reaches the end. Both are
branches, so every path must agree on one type. They unify against the type
expected where the block sits, so a literal coerces to that rather than to its
own default. Any expression position takes one, not only the right-hand side of
a `let`.

```wado
let found = search: {
    for let item of items {
        if item.key == key {
            break search: item.value;
        }
    }
    -1  // the value when no break is taken
};
```

A trailing statement that is not a value yields `()`. A block whose branches all
yield `()` has the type `()`, and one mixing `()` with a value is the same type
error as any other disagreement.

A path that cannot reach the end is no branch at all. When the trailing
statement is a loop that only `break LABEL` leaves, nothing reaches the tail, so
the breaks alone type the block. This is how a loop computes a value:

```wado
let found = scan: {
    loop {
        let line = next();
        if line.is_match() { break scan: line; }
    }
};
```

## Match Expression

Match expression provides exhaustive pattern matching on variants and other types.

```wado
// Match expression (produces a value)
let result = match opt {
    Some(x) => x * 2,
    None => 0,
};

// Match with custom variants
let area = match shape {
    Circle(r) => 3.14159 * r * r,
    Rectangle([w, h]) => w * h,
    Point => 0.0,
};

// Match statement (no value produced)
match command {
    Start => engine.start(),
    Stop => engine.stop(),
}
```

### Pattern Syntax

| Pattern       | Example                      | Description                                  |
| ------------- | ---------------------------- | -------------------------------------------- |
| Wildcard      | `_`                          | Matches anything                             |
| Variable      | `x`                          | Binds matched value                          |
| Mut variable  | `mut x`, `Some(mut x)`       | Binds as mutable                             |
| Literal       | `0`, `"hello"`, `true`       | Matches exact value                          |
| Constant      | `MAX_LEN`, `i32::MAX`        | Matches an immutable global / const by value |
| Variant       | `Some(x)`, `None`            | Matches variant case                         |
| Tuple         | `[a, b, c]`                  | Destructures tuple                           |
| Nested tuple  | `[10, Some(x)]`              | Literal/variant sub-patterns in tuple        |
| Struct        | `{ x, y }`, `Point { x, y }` | Destructures struct                          |
| Nested struct | `{ x: 0, y }`                | Literal/variant sub-patterns in struct       |
| Or            | `Red \| Blue`                | Matches either pattern                       |
| Guard         | `Some(x) && x > 0`           | Pattern with condition                       |

A tuple pattern names every element of the tuple, or ends in `..` after at most
that many. Over a tuple carrying a variadic pack, only the elements ahead of the
pack have a fixed position, so the pattern names at most those and ends in `..`.

A string-literal pattern tests the scrutinee with `==` against a `String`, so
any type implementing `Eq<String>` matches one: a `String`, a `StrSlice`, or a
newtype over either. A type parameter bounded by `AsStrSlice` matches as well,
since `AsStrSlice` requires `Eq<String>`.

```wado
fn kind<S: AsStrSlice>(s: S) -> i32 {
    return match s { "cntrl" => 1, "digit" => 2, _ => 0 };
}
kind("digit".as_str_slice());   // 2
```

### Patterns That Cannot Fail

A `let` or `for` binding takes a pattern that matches every value of its type.
A case pattern does so when no other case of the type holds a value, as defined
under Exhaustiveness below. A literal, a range, a constant, an or-pattern, and a
narrowing type pattern never do.

```wado
variant W { A(i32) }
let A(x) = w;                     // OK: W has no other case
let Ok(v) = r;                    // OK where r: Result<i32, !>
let Some(y) = opt;                // Error: `Some` may not match
```

A bare name in such a pattern, at its root or below it, is a case pattern when
it names a case of the type it matches, and a binding otherwise. It never names
a global: a constant pattern can always fail, so reading one here could only be
rejected. So `let [None, n] = pair` tests its first element and is an error,
since `None` may not match, while `let limit = 1` binds even where a
`global limit` is in scope. The `shadowed_name` lint reports that global.

A refutable pattern reads a bare name that names an immutable global as a
constant pattern instead. The refutable positions are a `match` arm, `if let`,
`while let`, and `let ... else`, so `let limit = v else { … }` runs the `else`
block unless `v == limit`.

An uninitialized `let x: T;` declares a single name, or `_`.

### Exhaustiveness

Match must cover all possible cases. Use `_` wildcard for catch-all:

```wado
match color {
    Red => "red",
    Green => "green",
    _ => "other",  // Required for exhaustiveness
}
```

The guardless arms must cover every value together. A guarded arm covers
nothing, and a case is covered only as far as its payload patterns reach:

```wado
match opt {
    Some(1) => "one",
    None => "none",
}   // Error: non-exhaustive match: missing case `Some(-2147483648..=0)`
```

An arm no value can reach is an error too: the guardless arms before it already
take every value it matches.

```wado
match opt {
    _ => 0,
    Some(x) => x,   // Error: unreachable arm
}
```

A case whose payload type has no value needs no arm, since nothing constructs
it. An arm naming one is unreachable, and is the same error. A type has no value
when it is `!`, when a tuple element or struct field of it has none, or when
every case of a variant has a payload with none:

```wado
let r: Result<i32, !> = Ok(1);
match r {
    Ok(v) => v,     // exhaustive: no `Result<i32, !>` is an `Err`
}
```

### Guard Expressions

Guards use `&&` to reflect left-to-right evaluation (pattern first, then guard):

```wado
match customer {
    Premium(years) && years > 5 => 0.3,
    Premium(_) => 0.2,
    _ => 0.1,
}
```

### Qualified Patterns

A case may be written under the type that declares it: `Color::Green`. Two kinds
of qualifier reach the same cases.

The first is a name that resolves to the scrutinee's type. An import alias
(`M::Nothing` under `use { Maybe as M }`), `Self` inside an `impl`, and a
namespace-qualified type (`dep::Maybe::Nothing`) all qualify. So does any name on
the scrutinee's newtype chain, a newtype's cases being its base's: with
`type C = Color`, both `C::Green` and `Color::Green` qualify. A second newtype
over the same base does not, being a distinct type.

The second is a namespace prefix the scrutinee's type is reachable through, such
as `h::Green` under `use h from "./hue.wado"`. That prefix names a module rather
than a type.

Either way the name must be one the file can see. Only the prelude is in scope
without a `use`, so a qualifier naming an unimported type is an error even where
the bare case would match.

A qualifier may restate the scrutinee's type arguments. It must then write as
many as the scrutinee carries, so `Maybe<i32>::Just` qualifies a `Maybe<i32>`
while `Color<i32>::Red` is an error, because `Color` declares no type parameters.

Only a bare identifier can bind. A qualified path names a case, an associated
constant, or an immutable `global`; anything else is an error, never a variable
of that name.

### Constant Patterns

A pattern identifier that resolves to an immutable `global` or an associated constant matches by value, instead of binding a new variable:

```wado
global TK_FOO: i32 = 1;
global TK_BAR: i32 = 2;

let kind = match token {
    TK_FOO | TK_BAR => "keyword",
    i32::MAX        => "max",
    _               => "other",
};
```

The pattern matches where `scrutinee == CONSTANT` holds, so a constant of any
type with an `Eq` compares as `==` would: a `String`, a struct, a tuple or an
`Option` constant matches at the top of an arm or nested in another pattern.

### Or Patterns

Or patterns match if any alternative matches. All alternatives must bind the same names with the same types:

```wado
// Enum or-patterns
match color {
    Red | Blue => "cool",
    Green => "warm",
}

// Variant or-patterns with bindings
match expr {
    Num(n) | Neg(n) => use(n),
    Zero => 0,
}

// Literal or-patterns
match n {
    1 | 2 | 3 => "low",
    _ => "high",
}

// Or patterns in matches operator
if shape matches { Circle(_) | Square(_) } { ... }
```

### Nested Sub-Patterns in Tuple/Struct Destructuring

Tuple and struct patterns support literal, variant, enum, or-pattern, and range sub-patterns:

```wado
// Literal sub-patterns in tuples
match [x, y] {
    [0, 0] => "origin",
    [0, _] => "y-axis",
    [_, 0] => "x-axis",
    _ => "other",
}

// Variant sub-patterns in tuples
match [a, b] {
    [Some(x), Some(y)] => x + y,
    [Some(x), None] => x,
    [None, _] => 0,
}

// Literal sub-patterns in structs
if let { x: 0, y: 0 } = point { println("origin"); }

// Enum sub-patterns in tuples
match [color, size] {
    [Red, Large] => "big red",
    [Blue, _] => "blue",
    _ => "other",
}

// Or-pattern and range sub-patterns
match point {
    { x: 0 | 1, y } => y,
    { x: 10..=20, y } => -y,
    _ => 0,
}
```

### Mutable Bindings in Patterns

The `mut` keyword before a binding name makes it mutable inside the pattern body:

```wado
if let Some(mut x) = opt {
    x += 10;  // x is mutable
}

match result {
    Ok(mut value) => {
        value *= 2;
        value
    },
    Err(_) => 0,
}
```

## Matches Operator

The `matches` infix operator tests if a value matches a pattern, returning `bool`.

```wado
// Basic usage
let is_some = opt matches { Some(_) };
let is_circle = shape matches { Circle(_) };

// With guard
let is_large = shape matches { Circle(r) && r > 10.0 };

// In conditions
if opt matches { Some(_) } {
    println("has value");
}
```

### Scope

Pattern bindings are scoped to the guard only and do not escape:

```wado
// Bindings don't escape
if opt matches { Some(x) } && x > 0 { }  // ERROR: x not in scope

// Use guard inside the pattern instead
if opt matches { Some(x) && x > 0 } { }  // OK
```

## Branch Hints

`builtin::cold_path()` marks the code path that contains it as cold (rarely
executed). It is a statement with no runtime effect. It is a performance hint:
the compiler and the Wasm engine treat the other side of the branch that
contains it as the likely one.

Because it is a plain statement rather than a condition wrapper, `cold_path()`
works anywhere a branch body does — including an `if let` or `match` arm, where
no boolean condition is available:

```wado
// Error/abort guard: the taken branch is cold.
fn get(&self, i: i32) -> i32 {
    if i >= self.len {
        builtin::cold_path();
        panic("index out of bounds");
    }
    return self.data[i];
}

// `match` arm with no boolean condition.
match command {
    Command::Run => execute(),
    Command::Crash => {
        builtin::cold_path();
        panic("crashed");
    }
}
```

Placed on the fall-through after a guard whose taken branch diverges, it hints
the guard as likely-taken — the guard-clause idiom:

```wado
fn lookup(&self, key: String) -> i32 {
    if let Some(v) = self.fast_path(key) {
        return v;
    }
    builtin::cold_path(); // the slow path below is rarely reached
    return self.slow_path(key);
}
```

## Optimization Barrier

`builtin::black_box(value)` returns `value` unchanged but never as a
compile-time constant, so the compiler does not fold away a computation reading
it. It emits no instruction. Use it to keep a test or benchmark measuring the
work it names:

```wado
test "sign extension folds the redundant mask" {
    assert to_i8(builtin::black_box(300)) == 44;
}
```

Without it, `to_i8(300)` folds to `44` and the test no longer reaches `to_i8`.

The barrier binds the Wado compiler only. The Wasm engine that runs the program
is still free to fold the value.

## Embedded Data

`List::<T>::from_le_bytes(bytes)` reads `bytes` as little-endian `T`s, back to
back, for any `T: FromLeBytes`: the fixed-width integers, `f16`, `bf16`, `f32`
and `f64`. It panics when the byte count is not a whole number of `T`s.

When the argument is a byte string literal or `#include_bytes` and `T` is
concrete, the compiler folds the call into a constant, so no decode loop runs at
startup. A long list is created from a data segment with one `array.new_data`,
and the Wasm grows by the byte count alone. A short one is built inline, where
that encodes smaller. Either way the result is an ordinary `List<T>`. A ragged
literal is not folded, and panics as it would at run time. Nor is a call whose
`T` is a newtype with its own `FromLeBytes`, which decides what the bytes mean.

`builtin::array_new_data::<T>(bytes)` is the same fold returning an `Array<T>`,
for a caller building its own container. It has no run-time form, so its
argument must be a literal, `T` must be a numeric primitive, and the byte count
must be a whole number of `T`s. Each is a compile error otherwise.

```wado
let weights = List::<f32>::from_le_bytes(#include_bytes("./weights.bin"));
let bias = List::<bf16>::from_le_bytes(b"\x80\x3f\x00\x40");   // [1.0, 2.0]
```

## Error Handling

### Unrecoverable Errors (Traps)

```wado
panic("Fatal error");      // logs the message to stderr, then traps
unreachable();             // traps
assert condition;          // panics when the condition is false
```

A trap cannot be caught in Wado. It ends the program.

### Recoverable Errors (Result Type)

```wado
fn parse_int(s: String) -> Result<i32, ParseError> {
    // ...
}

fn read_config(path: String) -> Result<Config, ConfigError> with Preopens {
    let content = fs::read_to_string(path)
        .map_err(|e| ConfigError::Io(e))?;
    let config = parse_config(content)?;
    return Ok(config);
}

// Handle with pattern matching
match result {
    Ok(value) => process(value),
    Err(e) => handle_error(e),
}
```
