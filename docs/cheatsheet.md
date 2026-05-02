# Wado Cheatsheet

Quick reference for Wado syntax.

## Shebang

```wado
#!/usr/bin/env wado run
```

## Comments

```wado
// Line comment
/* Block comment */

//! Module doc comment
/// Doc comment
```

## Imports

```wado
use { println, eprintln } from "core:cli";
use { Stdout, Stdout::{write_via_stream} } from "wasi:cli";
use utils from "./utils.wado";                // namespace import
use { foo as bar } from "./mod.wado";         // rename
pub use { foo, bar } from "./internal.wado";  // re-export
```

Namespace imports make all pub symbols from the source module directly available:

```wado
use geo from "./geo.wado";
let p = geo::Point::new(1, 2);  // access via namespace
```

### Schema Imports

Non-`.wado` files (`.g4`, `.proto`, ...) are imported via a generator declared in `[build-dependencies]` of `wado.toml`. See [WEP: Kiln](./wep-2026-04-12-kiln.md) for the mechanism, [WEP: Gale](./wep-2026-03-02-gale.md) for the real-world usage.

```wado
use { Parser } from "./Calc.g4" with { // Gale parses ANTLR4 grammar files
    generator: {
        module: "wado:gale@0.1",
        options: { highlight: false },
    },
};
```

## Value Semantics

See [WEP: Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md).

Wado uses Wasm GC for memory management. There is no borrow checker or lifetime annotations. Primitives and composite types have value semantics: assignment creates a copy. Reference types (`&T`, `&mut T`) share the underlying value.

## Literals

```wado
// Numbers
42              // integer literal (defaults to i32 without type context)
3.14            // float literal (defaults to f64 without type context)
1_000_000       // underscores for readability
0xFF            // hex
0b1010          // binary
0o755           // octal

// Numeric literal coercion
let x: i64 = 42;               // integer literal → i64
let y: u8 = 255;               // integer literal → u8
let z: u128 = 1_000_000_000;   // integer literal → u128
let f: f32 = 3.14;             // float literal → f32
fn foo(n: i64) { ... }
foo(100);                      // integer literal coerced to i64

// Strings
"Hello"         // String
`Hello, {name}` // Template string
`\{"key"\}`     // Escaped braces in template string → {"key"}
"Hello,
world!"         // Multi-line string

// Characters
'A'
'\n'
'\u0041'
'\u{1F600}'

// Booleans
true
false

// Null
null // coerce to Option::None

// Unit
()
```

## Variables

```wado
let x = 42;             // immutable
let mut y = 0;          // mutable
let z: i64 = 100;       // with type annotation

// Same-scope shadowing (when RHS references the old value)
let x = x + 1;          // OK: derives from old x
let x = transform(x);   // OK: derives from old x
// let x = 2;           // Error: does not reference old x
```

## Global Variables

See [WEP: Global Variables](./wep-2026-01-27-global-variables.md).

```wado
global PI: f64 = 3.14159;           // immutable
global mut counter: i32 = 0;        // mutable
pub global VERSION: i32 = 1;        // accessible from other modules
global DOUBLED: i32 = 21 * 2;       // expressions allowed

fn example() {
    println(`{PI}`);                // read global
    counter = counter + 1;          // write mutable global
}
```

## Types

```wado
// Primitives
i8, i16, i32, i64         // signed integers
u8, u16, u32, u64         // unsigned integers
f32, f64                  // floats
char                      // a valid unicode scalar
bool

// 128-bit integers (prelude types, work like primitives)
i128, u128

// Composite (provided by prelude)
String                  // UTF-8 string
Array<T>                // dynamic array
[T, U, V]               // tuple
Option<T>               // optional value
Result<T, E>            // result type

// Reference
&T                      // immutable reference
&mut T                  // mutable reference

// Unit type
()
```

### Newtype

Newtypes are distinct types that inherit methods/operators/traits from the base type, require explicit `as` cast, and have zero runtime cost.

See [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md).

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;       // literal coercion
let sum = m + m;              // OK: Meters + Meters -> Meters
// let bad = m + km;          // ERROR: cannot mix Meters and Kilometers
let raw: f64 = m as f64;      // explicit cast required

type Location = Point;
let loc: Location = Point { x: 0, y: 0 } as Location;
loc.distance(&loc2);  // inherits Point methods, params expect &Location

impl Location {
    fn name(&self) -> String { ... }  // newtype-specific method
}
```

### Tuples and Arrays

See [WEP: Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md).

```wado
// Tuples
let t = [1, "hello", true];   // [i32, String, bool]
let x = t.0;                  // dot notation
let y = t[1];                 // bracket notation (constant index only)

// Arrays (requires explicit type context)
let a: Array<i32> = [1, 2, 3];           // type annotation
let b = [1, 2, 3] as Array<i32>;         // explicit cast
fn takes(arr: Array<i32>) {}
takes([1, 2, 3]);                        // coercion to Array

// Array methods
let mut arr: Array<i32> = [];
arr.push(1);                             // add element to end
let n = arr.len();                       // get length
let empty = arr.is_empty();              // check if empty
let first = arr[0];                      // index access (read)
arr[0] = 100;                            // index assignment (write, requires mut)
// Note: there is no iter_mut(); mutate elements via index access

// Sorting
let mut nums: Array<i32> = [5, 3, 8, 1];
nums.sort();                             // in-place ascending sort (requires T: Ord)
let asc = nums.sorted();                 // returns new sorted array
nums.sort_by(|a: &i32, b: &i32| { ... });  // sort with custom Ordering comparator
```

### Strings

`String` is a prelude type with a literal syntax.

```wado
// Template strings (interpolation)
let name = "Alice";
let greeting = `Hello, {name}!`;         // "Hello, Alice!"

// Float-to-string: shortest roundtrip representation (no trailing .0)
let s = `{5.0}`;                         // "5"
let s = `{3.14}`;                        // "3.14"

// Format specifiers via Display (see docs/wep-2026-01-17-template-format-specifiers.md)
let formatted = `{3.14159:0.2f}`;        // "3.14"
let hex = `{255:x}`;                     // "ff"
let hex_alt = `{255:#x}`;                // "0xff" (via alternate flag)

// Inspect — auto-derived debug outputs (see docs/wep-2026-02-21-inspect-debug-output.md)
println(`{point:?}`);                    // "Point { x: 10, y: 20 }"
println(`{point:#?}`);                   // pretty-print with indentation (see below)
println(`{point}`);                      // falls back to inspect when no Display impl

// Pretty-print (:#?) — multi-line indented output for composite types
let arr: Array<i32> = [1, 2, 3];
println(`{arr:#?}`);
// [
//   1,
//   2,
//   3,
// ]

// String methods (mostly Rust compatible)
let n = s.len();                         // UTF8 byte length
let chars = s.chars().count();           // character count based on Unicode scalars

// String building
let mut builder = String::with_capacity(20);
builder.push_str("Hello");
builder.push_str(", World!");

// Iterating over characters
for let c of "hello".chars() {
    println(`{c}`);
}
```

### Structs

```wado
// Simple struct
struct Point {
    x: i32,
    y: i32,
}

// Generics
struct Pair<F, S> {
    first: F,
    second: S,
}

// Field visibility
pub struct Config {
    pub name: String,   // accessible from other modules
    secret: i32,        // private to this module
}

// Field defaults (omittable at construction) — see WEP: Default Arguments
struct ServerConfig { host: String, port: i32 = 8080, debug: bool = false }
let c = ServerConfig { host: "localhost" };  // port=8080, debug=false

// Construction
let p = Point { x: 10, y: 20 };
let b = Pair { first: 0, second: 1 };  // F and S are inferred as i32

// Shorthand
let x = 10;
let y = 20;
let p: Point = { x, y };

// Field access
let sum = p.x + p.y;

// Destructuring
let { x, y } = p;                        // unnamed
let Point { x, y } = p;                  // named
let { x: horizontal, y: vertical } = p;  // renaming
let { name, .. } = person;               // ignore remaining fields
let mut { x, y } = p;                    // mutable

// Recursive types
struct Node {
    value: i32,
    next: Option<Node>,
}

// Nested destructuring
let { start: { x: x1, y: y1 }, end: { x: x2, y: y2 } } = line;

// Destructuring in for-of
for let { x, y } of points {
    println(`{x}, {y}`);
}
```

### Enums

Wado has three distinct type kinds for Component Model alignment: enums (no payload), variants (with payload), and flags (bitmask).

Enums are discriminated values without payloads:

```wado
enum Color {
    Red,
    Green,
    Blue,
}

let c = Color::Red;

// Pattern matching
let name = match c {
    Red => "red",
    Green => "green",
    Blue => "blue",
};
```

### Variants

Variants are sum types with payloads (unlike enums which have no payloads). See [WEP: Variant Payload Design](./wep-2026-01-25-variant-payload-design.md).

```wado
variant Shape {
    Circle(f64),           // radius
    Rectangle([f64, f64]), // width, height (tuple payload)
    Point,                 // no payload
}

// Generics
variant Maybe<T> {
    Just(T),
    Nothing,
}

// Option and Result are defined as variants in core:prelude
// pub variant Option<T> { Some(T), None }
// pub variant Result<T, E> { Ok(T), Err(E) }

// Construction
let some_val = Option::Some(42);                         // type inferred
let none_val: Option<i32> = null;                        // Option::None
let ok_val: Result<i32, String> = Result::Ok(42);
let err_val: Result<i32, String> = Result::Err("fail");

// Explicit turbofish (required when inference is insufficient)
let opt = Option::<i32>::Some(42);
let res = Result::<i32, String>::Ok(42);
```

See Control Flow for pattern matching with `match`, `if let`, and `matches`.

### Flags

Flags are bitmask types where each member represents a single bit:

```wado
pub flags Perms {
    Read,     // bit 0 → value 1
    Write,    // bit 1 → value 2
    Execute,  // bit 2 → value 4
}

let rw = Perms::Read | Perms::Write;   // bitwise combination
let masked = rw & Perms::Read;         // bitwise AND
let toggled = rw ^ Perms::Read;        // bitwise XOR

let none = Perms::none();  // 0 (no bits set)
let all  = Perms::all();   // 7 (all bits set)

assert rw as u32 == 3;     // cast to/from u32
```

## References

```wado
let x = 42;
let r = &x;           // immutable reference
let v = *r;           // dereference

let mut y = 0;
let mr = &mut y;      // mutable reference
*mr = 10;             // assign through reference

let rr = &r;          // &&i32
let val = **rr;       // double dereference

// &mut to & coercion (automatic)
fn read(r: &i32) { ... }
read(&mut y);         // OK: &mut i32 coerced to &i32
```

Key differences from Rust:

- No borrow checker: multiple mutable references allowed
- Can return references to local variables
- No lifetime annotations needed

## Operators

See [WEP: Operator Precedence and Associativity](./wep-2026-01-11-operator-precedence.md) and [WEP: Operator Overloading](./wep-2026-01-18-operator-overloading.md).

```wado
// Arithmetic
+ - * / %

// Comparison (can be chained: a < b < c → a < b && b < c)
== != < <= > >=

// Logical
&& || !

// Bitwise
& | ^ ~ << >>

// Assignment
= += -= *= /= %= &= |= ^= <<= >>=

// Type cast
42 as f64
'A' as i32              // char -> i32: 65
// 65 as char           // compile error: use char::from_u32()

// Range: exclusive and inclusive
..<  ..=

// Pattern testing (returns bool)
opt matches { Some(_) }
```

## Control Flow

```wado
// If / else if / else
if x > 0 {
    println("positive");
} else if x < 0 {
    println("negative");
} else {
    println("zero");
}

// If expression
let abs = if x < 0 { -x } else { x };

// If let pattern matching
if let Some(x) = opt {
    println(`Got: {x}`);
}

// Mutable pattern bindings
if let Some(mut x) = opt {
    x += 10;
}

// Match ergonomics: &T scrutinees match against inner type
let ro = &opt;                // &Option<i32>
if let Some(x) = ro {         // x: &i32
    println(`Got: {*x}`);
}

// While
while i < 10 { i += 1; }

// While let
while let Some(x) = iter.next() { println(`{x}`); }

// C-style for
for let mut i = 0; i < 10; i += 1 {
    println(`{i}`);
}

// For-of (any IntoIterator type)
for let item of items {
    println(`{item}`);
}

// Range for-of
for let i of 0..<10 { println(`{i}`); }    // 0 to 9
for let i of 1..=10 { println(`{i}`); }    // 1 to 10
for let c of 'a'..='z' { print(`{c}`); }   // abcdefghijklmnopqrstuvwxyz

// Tuple for-of (compile-time expansion)
for let v of [42, "hello", true] {
    println(`{v}`);
}

// Infinite loop
loop {
    if done { break; }
    continue;
}

// Labeled block — all blocks require a label
scope: {
    let x = 20;  // new scope
}

// Labeled break — exit a named block early
outer: {
    if condition {
        break outer;  // jump past the block
    }
    // skipped if break taken
}

// Match expression
let result = match opt {
    Some(x) => x * 2,
    None => 0,
};

// Match statement with "or" patterns
match color {
    Red | Blue => "cool",
    Green => "warm",
}

// Or patterns with bindings (all alternatives must bind the same names)
match expr {
    Num(n) | Neg(n) => use(n),
    Zero => 0,
}

// Or patterns in matches operator
if shape matches { Circle(_) | Square(_) } { ... }

// Note: matches bindings don't escape — use guard instead
// if opt matches { Some(x) } && x > 0 { ... }  // Error: x not in scope
if opt matches { Some(x) && x > 0 } { ... }     // OK: guard inside braces

// Match with guard
let label = match value {
    Some(x) && x > 100 => "large",
    Some(x) && x > 10 => "medium",
    Some(_) => "small",
    None => "none",
};

// Match with block body
let desc = match value {
    Some(n) => {
        let doubled = n * 2;
        `value is {doubled}`
    },
    None => "no value",
};

// Range patterns
let grade = match score {
    0..<60   => "F",
    60..<70  => "D",
    70..<80  => "C",
    80..<90  => "B",
    90..=100 => "A",
    _ => "invalid",
};
```

Semicolons do not have particular semantics; they are just separators to statements. Convention in `wado format`: single-line block does not use semicolon.

```wado
let a = if cond() { 1 } else { 2 };   // either 1 or 2
let b = if cond() { 1; } else { 2; }; // ditto
```

## Assert

`assert` behaves like power assert.

```wado
assert x > 0;
assert x > 0, "x must be positive";
```

## Functions, Methods, and Closures

### Functions

```wado
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

// With effects
fn greet(name: String) with Stdout {
    println(`Hello, {name}!`);
}

// Module public (accessible from other Wado modules)
pub fn api_function() -> i32 {
    return 42;
}

// Component export (public API at CM boundary)
export fn run() { ... }

// Default args, trailing only — see WEP: Default Arguments
fn connect(host: String, port: i32 = 8080) { ... }
connect("localhost");           // → connect("localhost", 8080)
```

A function must have `return` if it returns a value. Default expressions must be effect-free; `export fn` and closures cannot have defaults.

### Methods

```wado
impl Point {
    fn sum(&self) -> i32 {
        return self.x + self.y;
    }

    fn reset(&mut self) {
        self.x = 0;
        self.y = 0;
    }

    // Static method (no self parameter)
    fn origin() -> Point {
        return Point { x: 0, y: 0 };
    }
}

let mut p = Point { x: 1, y: 2 };
let s = p.sum();
p.reset();
let origin = Point::origin();
```

Note: bare `self` (by value) is not allowed. Use `&self` or `&mut self`.

### Closures

See [WEP: Closure Implementation](./wep-2026-01-16-closure-implementation.md).

```wado
// Expression body
let add_one = |x: i32| x + 1;

// Block body
let compute = |x: i32| {
    let doubled = x * 2;
    return doubled + x * 3;
};

// Struct literal return
let make_point = |x: i32, y: i32| Point { x, y };

// Capturing outer variables (value semantics - copy)
let multiplier = 10;
let scale = |x: i32| x * multiplier;

// Mutable capture: use &mut || to mutate captured variables
let mut count = 0;
let inc = &mut || { count += 1; };
inc();
inc();
println(`{count}`);  // 2
```

### Mut Parameters

```wado
// mut allows reassigning the parameter inside the function
fn increment(mut n: i32) -> i32 {
    n += 1;
    return n;
}

// Caller's variable is unchanged for primitives (value type)
let x = 5;
let y = increment(x);
// x == 5, y == 6

// Closures also support mut parameters
let double = |mut n: i32| { n *= 2; return n; };

// Without mut, assignment is a compile error
// fn bad(n: i32) { n = 0; }  // Error: cannot assign to immutable variable 'n'
```

### Generics

```wado
fn identity<T>(x: T) -> T {
    return x;
}

impl Container {
    fn transform<T, U>(&self, a: T, b: U) -> T {
        return a;
    }
}

// Turbofish syntax (explicit type arguments)
let x = identity::<i32>(42);
let y = container.transform::<i32, i64>(10, 20 as i64);
let arr = Array::<i32>::with_capacity(10);  // turbofish for generic statics

// Variadic type packs: operate on tuples of any arity
fn variadic_identity<..T>(x: [..T]) -> [..T] {
    return x;
}
let t = variadic_identity([1, "hello", true]); // t: [i32, String, bool]

// Mixed scalar + pack parameters
fn prepend<A, ..T>(a: A, rest: [..T]) -> [A, ..T] {
    return [a, ..rest];  // value spread: splice rest into tuple
}

// Value spread (works with any tuple, not just packs)
let a = [1, "hello"];
let b = [..a, true];   // [i32, String, bool]

// Type pack expansion: call a static method on each type in the pack
fn make_defaults<..T: Default>() -> [..T] {
    return [..T::default()];   // expands to [T_0::default(), T_1::default(), ...]
}
```

### Reference Storage

See [WEP: Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md).

Functions that store reference parameters must declare `stores[...]`:

```wado
struct Container {
    data: &Data,
}

// Function that stores a reference parameter — must declare stores
fn store_data(data: &Data) -> Container with stores[data] {
    return Container { data };
}

// Function that uses but does NOT store a reference — no stores needed
fn use_data(data: &Data) -> i32 {
    return data.value;
}

// Combined with effects
fn store_and_log(data: &Data) -> Container with Stdout, stores[data] {
    println(`Storing: {data.value}`);
    return Container { data };
}
```

Rules:

- `stores[param]` declares that the function may store the reference parameter
- Only reference parameters (`&T` or `&mut T`) can appear in `stores[...]`
- Without `stores[param]`, a function cannot return, store in struct fields, or assign to globals the reference parameter
- In function type position, use positional indices: `fn(&Data) with stores[0]`

## Visibility

Wado has three levels of visibility:

| Keyword  | Term             | Scope                                 |
| -------- | ---------------- | ------------------------------------- |
| (none)   | private          | Within the module                     |
| `pub`    | module public    | Other modules within the same project |
| `export` | component export | CM boundary (package's public API)    |

```wado
fn private_fn() { }           // module-private (default)
pub fn public_fn() { }        // project-internal
export fn run() { }           // component export
pub export fn both() { }      // both
```

All entity definitions can have `pub` visibility, including struct fields.

## Traits

```wado
trait Greet {
    fn greet(&self) -> String;
}

impl Greet for Person {
    fn greet(&self) -> String {
        return `Hello, {self.name}!`;
    }
}

// Default methods
trait Summary {
    fn title(&self) -> String;

    fn summary(&self) -> String {
        return `Title: {self.title()}`;
    }
}

// Associated type
trait Container {
    type Item;

    fn get(&self) -> Self::Item;
}

impl Container for IntBox {
    type Item = i32;
    fn get(&self) -> Self::Item { return self.value; }
}
```

Traits use static dispatch. Use `Self::TypeName` to refer to associated types.

### Prelude Traits

See [WEP: Default Trait](./wep-2026-03-04-default-trait.md) for `Default`.

```wado
// For == and != operators
trait Eq { fn eq(&self, other: &Self) -> bool; }

// For <, <=, >, >= operators
trait Ord { fn cmp(&self, other: &Self) -> Ordering; }

// For default value (auto-implemented for primitives, String, Array<T>,
// Option<T>, TreeMap<K, V>; not Result)
trait Default { fn default() -> Self; }

// For [] operators
trait IndexValue<I> { type Output; fn index_value(&self, index: I) -> Self::Output; }
trait IndexAssign<I> { type Input; fn index_assign(&mut self, index: I, value: Self::Input); }
trait Index<I> { type Output; fn index(&self, index: I) -> &Self::Output; }

// For string template interpolation
pub trait Display { fn fmt(&self, f: &mut Formatter); }         // stringify with specifiers
pub trait DisplayAlt { fn fmt_alt(&self, f: &mut Formatter); }  // for # (alt) flag
```

### Trait Bounds

See [WEP: Trait Bounds Enforcement](./wep-2026-02-07-trait-bounds.md).

```wado
struct SortedPair<T: Ord> { first: T, second: T }
struct PrintableOrd<T: Ord + Printable> { value: T }

fn max<T: Ord>(a: T, b: T) -> T {
    if a > b { return a; }
    return b;
}

// Bounded impl blocks — methods only available when bound is satisfied
impl<T: Ord> Array<T> {
    pub fn sort(&mut self) { ... }
}

// Bounded trait impl — Pair<T> implements Eq only when T: Eq
impl<T: Eq> Eq for Pair<T> {
    fn eq(&self, other: &Self) -> bool {
        return self.first == other.first && self.second == other.second;
    }
}
```

### Auto-Derived Traits

All primitives implement `Eq` and `Ord`. Structs auto-derive `Eq` and `Ord` when all fields implement the trait.

Variants auto-derive `Eq` and `Ord` as well. `Option<T: Eq>`, `Result<T: Eq, E: Eq>`, `Array<T: Eq>` implement `Eq`. `Array<T: Ord>` implements `Ord`.

`Inspect` and `InspectAlt` are also auto-derived. The compiler synthesizes `Display` and `DisplayAlt` fallbacks that delegate to `Inspect` and `InspectAlt` respectively.

`Default` is auto-derived for a non-generic struct when every field has a declared default expression (`f: T = expr`).

Auto-derived traits are overridden by user-written `impl Trait for T`.

## Associated Constants

```wado
impl f64 {
    pub const PI: f64 = 3.14159265358979323846;
}

let pi = f64::PI;
let max = i32::MAX;
```

Primitives provide built-in constants: `f64::PI`, `f64::INFINITY`, `f64::NAN`, `i32::MAX`, `i32::MIN`, etc. See [`core:prelude`](./stdlib-core-prelude.md).

## Primitive Type Methods

See [`core:prelude`](./stdlib-core-prelude.md) for the full API.

```wado
f64::sin(x)    f64::cos(x)    f64::sqrt(x)
f64::abs(x)    f64::ceil(x)   f64::floor(x)
f64::pow(x, y) f64::ln(x)     f64::exp(x)

x.is_nan()     x.is_finite()    // where x is f64 or f32

f64::from_str("3.14")           // Result<f64, ParseFloatError>
i32::from_str("42")             // Result<i32, ParseIntError>
i32::from_str_hex("ff")         // Result<i32, ParseIntError> (radix 16)
i32::from_str_radix("1010", 2)  // Result<i32, ParseIntError> (radix 2..=36)

i32::min(a, b)  i32::max(a, b)

// char classification and conversion
let code = 'A' as i32;                // 65
let c = char::from_u32(65);           // Option::<char>::Some('A')
let d = char::from_u32_unchecked(65); // if you have already validated the u32 value
'A'.is_ascii_uppercase()              // true
'a'.is_ascii_lowercase()              // true
'A'.to_ascii_lowercase()              // 'a'
'a'.to_ascii_uppercase()              // 'A'

'a'.is_hexdigit()                     // true
'a'.hex_digit_value()                 // 10 (panic if the char is not a hex digit)
```

## Iterators

See [WEP: Iterator Traits Design](./wep-2026-01-24-iterator-traits.md).

`Iterator` provides `next()`. `IntoIterator` converts a collection into an iterator. Every `Iterator` automatically implements `IntoIterator` via a blanket impl, so all iterators work with `for-of`.

```wado
// Array iteration
let arr: Array<i32> = [1, 2, 3, 4, 5];
for let x of arr { println(`{x}`); }

// Explicit iterator
let mut iter = arr.into_iter();
iter.next();                              // Option<i32>
let rest = iter.collect();                // Array<i32>

// Combinators
let doubled = arr.into_iter().map(|x: i32| x * 2).collect();       // [2, 4, 6, 8, 10]
let evens = arr.into_iter().filter(|x: i32| x % 2 == 0).collect(); // [2, 4]
let sum = arr.into_iter().fold(0, |acc: i32, x: i32| acc + x);     // 15

// Chaining
let result = arr.into_iter()
    .filter(|x: i32| x > 2)
    .map(|x: i32| x * 10)
    .collect();  // [30, 40, 50]
```

### Custom Iterables

Implement `IntoIterator` to make custom types work with `for-of`. See [`core:prelude`](./stdlib-core-prelude.md) for trait definitions.

## Ranges

See [WEP: Range Object](./wep-2026-03-03-range-object.md).

Two range types: `RangeExclusive<T>` and `RangeInclusive<T>`. Both are generic structs in `core:prelude`.

```wado
// Range expressions
0..<10             // RangeExclusive<i32>: [0, 10)
1..=10             // RangeInclusive<i32>: [1, 10]
'a'..='z'          // RangeInclusive<char>

// Iteration (integers and char via Step trait)
for let i of 0..<5 { println(`{i}`); }    // 0, 1, 2, 3, 4
for let c of 'a'..='e' { print(`{c}`); }  // abcde
```

## Effects

See [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md).

```wado
fn write_file(path: String, data: String) with FileSystem { ... }
fn main() with Stdout, FileSystem { ... }
fn add(a: i32, b: i32) -> i32 { return a + b; }  // no effects = pure

// Effect in function type position
fn for_each(items: Array<i32>, f: fn(i32) with Stdout) with Stdout {
    for let item of items { f(item); }
}

// Generic effects — polymorphic over effects (one effect param per function)
fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E {
    return f(x);
}

// E is inferred from the closure's effects at each call site
wrapper(|| { println("hello"); });     // E = Stdout
let x = apply(|n: i32| n + 1, 41);     // E = (none)
let y = apply(|n: i32| {               // E = Stdout
    println(`{n}`);
    return n * 2;
}, 21);

// E resolves to the union of effects from all function-typed arguments
fn run_both<effect E>(f: fn() with E, g: fn() with E) with E {
    f();
    g();
}
run_both(
    || { println("stdout"); },    // Stdout
    || { eprintln("stderr"); },   // Stderr
);  // E = Stdout + Stderr
```

### Effect Handlers

See [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md).

An effect handler is an `impl Effect for Type` where the methods may call `resume value` to continue the suspended computation. The `with` block installs handlers for the duration of its `do` body. The `=>` arrow reads as "calls to E dispatch to h".

```wado
interface Counter {
    fn next() -> i32;
}

struct MyCounter { value: i32 }

impl Counter for MyCounter {
    fn next(&mut self) -> i32 {
        self.value += 1;
        resume self.value
    }
}

fn main() {
    let mut m = MyCounter { value: 0 };
    with Counter => &mut m do {
        let a = Counter::next();   // 1
        let b = Counter::next();   // 2
    }
    // Multiple handlers — comma-separated
    // with Stdin => &mut s, Stdout => &mut o do { ... }
}
```

`resume value` (only valid inside a handler method) hands `value` back to the caller of the operation. Without post-resume code it lowers to `return value`.

## Entrypoints

The entrypoint is defined in a world, which requires `export` keyword.

`run()` is the entry point for `wasi:cli/command`:

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello!");
}
```

`handle(request)` is the entry point for `wasi:http/service`. It must be `async` because HTTP handlers use the Component Model async calling convention:

```wado
use { Request, Response, ErrorCode, Fields, Trailers } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_rx, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Fields::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_rx);

    // task return: delivers result without ending the function
    task return Result::<Response, ErrorCode>::Ok(response);

    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

`task return expr;` delivers the function's result to the CM runtime without terminating the function. Valid only inside `export async fn`. Regular `return` is forbidden in `async fn` bodies.

## Test Blocks

Test blocks compile to the `test` world. Files with test blocks are discovered and executed by `wado test`:

```sh
wado test                            # walk the project for every *.wado file
wado test file.wado                  # run a specific file
wado test --filter '*pattern*'       # keep files whose path matches the wildcard
wado compile --world test file.wado  # compile a single file with the test world
```

Discovery walks the project root for every `*.wado` file, honouring
`.gitignore`, `.gitmodules`, dot-prefixed entries, and nested `wado.toml`
boundaries (each sub-package is run in its own context). Add
`[test].exclude = ["..."]` to `wado.toml` to skip extra paths. Files without
`test` blocks are still parsed and compiled (compile-only validation); only
files with `test` blocks register and run tests.

`--filter <pattern>` is a path-based shell wildcard (`*`, `?`, `[...]`); it
is _not_ a regex. To match anywhere in a path, wrap the term in `*`s, e.g.
`'*foo*'`. The runner exits non-zero on any compile failure, test failure,
or `#[TODO]` test that resolved unexpectedly.

```wado
test {
    assert fib(10) == 55;
}

test "addition works" {
    assert 1 + 1 == 2;
}

// Expect-trap test: passes when the body traps
#[expect_trap]
test "panics on invalid input" {
    panic("bad input");
}

// TODO test: reported on a separate axis from pass/fail.
// Pending (traps) = expected. Resolved (passes) = must remove #[TODO].
#[TODO]
test "not yet implemented" {
    panic("TODO: implement this");
}
```

## Standard Library

For full API reference, see:

- Core library:
  - [`core:prelude`](./stdlib-core-prelude.md) - auto-imported types, traits, primitive APIs
  - [`core:cli`](./stdlib-core-cli.md) - stdout/stderr printing
  - [`core:collections`](./stdlib-core-collections.md) - `TreeMap<K,V>` and `TreeSet<T>`
  - [`core:serde`](./stdlib-core-serde.md) - `Serialize` and `Deserialize` framework
  - [`core:json`](./stdlib-core-json.md) - JSON (self-describing)
  - [`core:json_nsd`](./stdlib-core-json_nsd.md) - JSON (non-self-describing)
  - [`core:json_value`](./stdlib-core-json_value.md) - dynamic JSON value
  - [`core:base64`](./stdlib-core-base64.md) - base64 encoding
  - [`core:zlib`](./stdlib-core-zlib.md) - zlib (de)compression
  - [`core:simd`](./stdlib-core-simd.md) - Wasm 128-bit SIMD
  - [`core:url`](./stdlib-core-url.md) - WHATWG URL parsing
  - [`core:kiln`](./stdlib-core-kiln.md) - Kiln IDL host bindings
- [WASI Standard Library Reference](./stdlib-wasi.md) - `wasi:cli`, `wasi:filesystem`, `wasi:http`, `wasi:clocks`, `wasi:random`, `wasi:sockets`

```wado
// core:prelude (auto-imported)
panic("error message");   // trap with message
unreachable();            // trap

// core:cli
use { println, eprintln, print, eprint, Stdout, Stderr } from "core:cli";
use { log_stdout, log_stderr } from "core:cli";  // no effect required

// core:collections
use { TreeMap, TreeSet } from "core:collections";
let mut map = TreeMap::<String, i32>::new();
map["key"] = 42;          // index assignment
let v = map["key"];       // index access (panics if not found)
let opt = map.get("key"); // fallible access returns Option<V>

let set = ["foo", "bar", "baz"] as TreeSet<String>;
assert set.contains("foo");
```

## Compile-Time Literals

```wado
let file = #file;           // current source file path (String)
let line = #line;           // current line number (i32)
let func = #function;       // current function name (String)
let data = #data;           // __DATA__ section content (String)

let src = #include_str("./runtime.wado");  // include file as String
let icon = #include_bytes("./icon.png");   // include file as Array<u8>
```

Paths in `#include_str` and `#include_bytes` are resolved relative to the source file. See [WEP: Compile-Time File Inclusion](./wep-2026-03-02-include-str.md).

## Attributes

```wado
#![no_prelude]             // disable auto-import of core:prelude
#![TODO]                   // all tests must fail or not compile
#![generated]              // marks machine-generated code
#![generated(by = "wado-from-idl", sources = ["deps/random.wit"])]  // with metadata

struct Foo {
    #[hidden]
    secret: String,        // won't be shown in Display / Inspect
}

#[inline]                  // hint: prefer inlining
#[inline(always)]          // always inline
#[inline(never)]           // never inline
```

## Serialization and Deserialization

Wado supports automatic serialization/deserialization via `core:serde`. See [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md).

## SIMD

Wado supports 128-bit SIMD operations including Relaxed SIMD. See [WEP: SIMD v128](./wep-2026-01-31-simd-v128.md).

## See Also

- [Language Specification](./spec.md) - Full language specification
