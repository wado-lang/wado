# Wado Cheatsheet

Quick reference for Wado syntax.

## Concepts

## Comments

```wado
// Line comment
/* Block comment */
```

## Shebang

```wado
#!/usr/bin/env wado
// Shebang is only valid on the first line and is ignored by the compiler.
// Note: #![ is an inner attribute, not a shebang.
```

## Literals

```wado
// Numbers
42              // integer literal (defaults to i32 without type context)
42 as i64       // i64 via cast
3.14            // float literal (defaults to f64 without type context)
3.14 as f32     // f32 via cast
1_000_000       // underscores for readability
0xFF            // hex
0b1010          // binary
0o755           // octal

// Numeric literal coercion: integer/float literals have no fixed type until
// the type context is known (annotation, function argument, etc.)
let x: i64 = 42;               // integer literal → i64
let y: u8 = 255;               // integer literal → u8
let z: u128 = 1_000_000_000;   // integer literal → u128
let f: f32 = 3.14;             // float literal → f32
fn foo(n: i64) { ... }
foo(100);                       // integer literal coerced to i64

// Strings
"hello"         // String
`Hello, {name}` // template string

// Characters
'A'
'\n'
'\u0041'

// Booleans
true
false

// Null
null            // same as Option::None

// Unit
()
```

## Variables

```wado
let x = 42;             // immutable
let mut y = 0;          // mutable
let z: i64 = 100;       // with type annotation
```

## Global Variables

```wado
// Module-level globals
global PI: f64 = 3.14159;           // immutable
global mut counter: i32 = 0;        // mutable

// With visibility
pub global VERSION: i32 = 1;        // accessible from other modules

// Arithmetic expressions
global DOUBLED: i32 = 21 * 2;       // evaluated at initialization

// Object type globals
global mut MESSAGE: String = "Hello, World!";
global mut ITEMS: Array<i32> = [1, 2, 3];

struct Point { x: i32, y: i32 }
global mut ORIGIN: Point = Point { x: 0, y: 0 };

// Usage
fn example() {
    println(`{PI}`);                // read global
    counter = counter + 1;          // write mutable global
}
```

Global variables map directly to WebAssembly globals. Constant expressions are evaluated at instantiation; non-constant expressions use lazy initialization.

## Types

```wado
// Primitives
i8, i16, i32, i64         // signed integers
u8, u16, u32, u64         // unsigned integers
f32, f64                  // floats
bool, char

// 128-bit integers (prelude types, work like primitives)
i128, u128

// Composite
String                  // UTF-8 string
Array<T>                // dynamic array
[T, U, V]               // tuple type
Option<T>               // optional value
Result<T, E>            // result type

// Reference
&T                      // immutable reference
&mut T                  // mutable reference

// Unit type
()
```

## Newtype

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;       // literal coercion
let km: Kilometers = 1.0;

let sum = m + m;              // OK: Meters + Meters -> Meters
// let bad = m + km;          // ERROR: cannot mix Meters and Kilometers

let raw: f64 = m as f64;      // explicit cast required
let conv = (m as f64) / 1000.0 as Kilometers;
```

Newtypes are distinct types that:

- Inherit methods, operators, and traits from the base type
- Require explicit `as` cast to convert to/from base type
- Have zero runtime cost (same representation)

```wado
// Newtype of struct - inherits methods
type Location = Point;

impl Point {
    fn distance(&self, other: &Point) -> f64 { ... }
}

let loc1: Location = Point { x: 0, y: 0 } as Location;
let loc2: Location = Point { x: 3, y: 4 } as Location;
let d = loc1.distance(&loc2);  // returns f64, params expect &Location

// Newtype-specific methods
impl Location {
    fn name(&self) -> String { ... }  // only on Location, not Point
}
```

## Tuples and Arrays

```wado
// Tuples
let t = [1, "hello", true];   // [i32, String, bool]
let x = t.0;                  // dot notation
let y = t[1];                 // bracket notation (constant index only)

// Arrays (requires explicit type context)
let a: Array<i32> = [1, 2, 3];           // type annotation
let b = [1, 2, 3] as Array<i32>;         // explicit cast
fn takes(arr: Array<i32>) {}
takes([1, 2, 3]);                        // implicit coercion

// Array constructors
let arr = Array::<i32>::with_capacity(10);  // empty array with pre-allocated capacity
let bools = Array::<bool>::filled(100, true);  // array of 100 elements, all true

// Array methods
let mut arr: Array<i32> = [];
arr.append(1);                           // add element to end
arr.append(2);
let n = arr.len();                       // get length (2)
let first = arr[0];                      // index access (read)
arr[0] = 100;                            // index assignment (write, requires mut)

// Sorting (stable, O(n log n) worst case)
let mut nums: Array<i32> = [5, 3, 8, 1];
nums.sort();                             // in-place ascending sort (uses < operator)
nums.sort_by(|a: &i32, b: &i32| {       // in-place sort with Ordering comparator
    if *a > *b { return Ordering::Less; }
    if *a < *b { return Ordering::Greater; }
    return Ordering::Equal;
});                                      // now sorted descending

let orig: Array<i32> = [5, 3, 8, 1];
let asc = orig.sorted();                // returns new sorted array (original unchanged)
let desc = orig.sorted_by(|a: &i32, b: &i32| {
    if *a > *b { return Ordering::Less; }
    if *a < *b { return Ordering::Greater; }
    return Ordering::Equal;
});
```

## Strings

```wado
// String literals
let s = "hello";                         // String literal (UTF-8)

// Multiline strings (newlines preserved)
let poem = "Line 1
Line 2
Line 3";

// Template strings (interpolation)
let name = "Alice";
let greeting = `Hello, {name}!`;         // "Hello, Alice!"

// Multiline template strings
let message = `Dear {name},

Welcome to Wado!`;

// String constructors
let s = String::with_capacity(100);      // empty string with pre-allocated capacity

// String methods
let s = "hello";
let n = s.len();                         // byte length (5)
let char_count = s.chars().count();      // character count (5 for ASCII)
let empty = s.is_empty();                // check if empty (false)

// For UTF-8 strings, byte length != character count
let jp = "日本";
jp.len();                                // 6 (bytes)
jp.chars().count();                      // 2 (characters)

// Low-level byte access (prefer iterators)
let byte = s.get_byte(0);                // get byte at index
s.set_byte(0, 72);                       // set byte at index (requires mut)

// String building (O(1) amortized append)
let mut builder = String::with_capacity(20);
builder.append("Hello");
builder.append(", ");
builder.append("World!");
// builder is now "Hello, World!"

// String concatenation (static method)
let combined = String::concat("Hello, ", "World!");  // "Hello, World!"

// Iterating over characters (for-of with chars())
for let c of "hello".chars() {
    println(`{c}`);              // h, e, l, l, o
}

// Iterating over bytes (for-of with bytes())
for let b of "hello".bytes() {
    println(`{b}`);              // 104, 101, 108, 108, 111
}
```

## Structs

```wado
struct Point {
    x: i32,
    y: i32,
}

// Generic struct
struct Box<T> {
    value: T,
}

// Construction
let p = Point { x: 10, y: 20 };
let b = Box { value: 42 };  // T is inferred as i32

// Shorthand (variable name matches field)
let x = 10;
let y = 20;
let p: Point = { x, y };

// Field access
let sum = p.x + p.y;
let v = b.value;
```

## Enums

Enums are discriminated values without payloads (i32 discriminant):

```wado
enum Color {
    Red,
    Green,
    Blue,
}

let c = Color::Red;

// Pattern matching with match
let name = match c {
    Red => "red",
    Green => "green",
    Blue => "blue",
};

// if let pattern matching
if let Red = c {
    println("it's red");
}

// matches operator
if c matches { Green } {
    println("it's green");
}

// match with guards
let desc = match c {
    Red => "warm",
    Blue => "cool",
    _ => "neutral",
};
```

Enums auto-derive `Display`, `Eq`, and `Ord`:

```wado
// Display: case name as string
println(`{c}`);              // "Red"

// Eq: compare by discriminant
let a = Color::Red;
let b = Color::Red;
if a == b { println("same"); }

// Ord: ordered by declaration order (Red=0 < Green=1 < Blue=2)
if Color::Red < Color::Blue {
    println("red comes before blue");
}
```

Enums can have methods via `impl` blocks:

```wado
impl Color {
    fn is_primary(&self) -> bool {
        return match *self {
            Red => true,
            Green => false,
            Blue => true,
        };
    }
}
```

## Flags

Flags are bitmask types where each member represents a single bit. They are used for WASI permission flags and similar bitfield values.

```wado
pub flags Perms {
    Read,     // bit 0 → value 1
    Write,    // bit 1 → value 2
    Execute,  // bit 2 → value 4
}

// Access individual members
let r = Perms::Read;    // 1
let w = Perms::Write;   // 2

// Bitwise combination
let rw = r | w;         // 3
let rwx = rw | Perms::Execute;  // 7

// Bitwise AND (masking)
let masked = rwx & Perms::Read;   // 1

// Bitwise XOR (toggle)
let toggled = rw ^ Perms::Read;   // 2

// Special static methods
let none = Perms::none();  // 0 (no bits set)
let all  = Perms::all();   // 7 (all bits set)

// Cast to/from u32
assert rw as u32 == 3;

// Arithmetic operators (+, -, *, /, %) are NOT allowed on flags types
// Use bitwise operators (|, &, ^) instead
```

Flags are newtypes over `u32`. Bitwise operators (`|`, `&`, `^`) work naturally.
Attributes can annotate members for WIT name mapping:

```wado
pub flags PathFlags {
    #[wasi("symlink-follow")]
    SymlinkFollow,
}
```

## Variants

Variants are sum types with payloads (unlike enums which have no payloads):

```wado
// Custom variant with unit and payload cases
variant Shape {
    Circle(f64),           // radius
    Rectangle(f64, f64),   // width, height
    Point,                 // no payload
}

// Generic variant
variant Maybe<T> {
    Just(T),
    Nothing,
}

// Construction
let c = Shape::Circle(5.0);
let p = Shape::Point;

// Option and Result are defined as variants in core:prelude
// pub variant Option<T> { Some(T), None }
// pub variant Result<T, E> { Ok(T), Err(E) }

// Option construction
let some_val: Option<i32> = Option::<i32>::Some(42);
let none_val: Option<i32> = null;  // null is equivalent to Option::None

// Pattern matching with if let
if let Some(x) = some_val {
    println(`Got value: {x}`);
} else {
    println("No value");
}

if let None = none_val {
    println("It's none");
}

// Custom variant pattern matching (non-generic)
variant ParseResult {
    Fail,
    Number(i32),
}
let r = ParseResult::Number(42);
if let Number(n) = r {
    // pattern uses case name only, not Type::CaseName
    println(`Got: {n}`);
}
```

Note: Generic variants (custom `Maybe<T>`) and `Result<T, E>` pattern matching are not yet implemented. Non-generic custom variants work with `if let`.

## Functions

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
```

A function must have `return` if it returns a value. This is applied to methods and closures as well.

## Visibility

Wado has three levels of visibility:

| Keyword  | Term             | Scope                                 |
| -------- | ---------------- | ------------------------------------- |
| (none)   | private          | Within the module                     |
| `pub`    | module public    | Other modules within the same project |
| `export` | component export | CM boundary (package's public API)    |

```wado
fn private_fn() { }           // module-private (default)
pub fn public_fn() { }        // project-internal (other modules in same project)
export fn run() { }           // component export (visible to consumers)
pub export fn both() { }      // both project-internal and component export
```

All entity definitions can have `pub` visibility.

`export` defines what is visible when the package is consumed as a dependency or published as a `.wasm` component. `pub` items are accessible within the project but hidden from external consumers.

## Methods

```wado
impl Point {
    // Instance method with &self (borrows immutably)
    fn sum(&self) -> i32 {
        return self.x + self.y;
    }

    // Instance method with &mut self (borrows mutably)
    fn reset(&mut self) {
        self.x = 0;
        self.y = 0;
    }

    // Instance method with self by value (copies value)
    fn to_tuple(self) -> [i32, i32] {
        return [self.x, self.y];
    }

    // Static method (no self parameter)
    fn origin() -> Point {
        return Point { x: 0, y: 0 };
    }
}

// Instance method call
let mut p = Point { x: 1, y: 2 };
let s = p.sum();
p.reset();

// Static method call
let origin = Point::origin();

// Static method on generic type (turbofish syntax)
let arr = Array::<i32>::with_capacity(10);
```

## Associated Constants

```wado
// Associated constants are compile-time constants defined in impl blocks
// They are inlined at every use site and cannot be mutated
impl f64 {
    pub const PI: f64 = 3.14159265358979323846;
}

// Access with Type::CONST syntax (no parentheses)
let pi = f64::PI;
let max = i32::MAX;
```

Primitive types provide built-in associated constants:

| Type                     | Constants                                                                                                                           |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------- |
| `f64`                    | `PI`, `TAU`, `E`, `LN2`, `LN10`, `LOG2_E`, `LOG10_E`, `SQRT2`, `FRAC_1_SQRT2`, `FRAC_PI_2`, `FRAC_PI_4`, `INFINITY`, `NEG_INFINITY` |
| `f32`                    | `PI`, `TAU`, `E`, `INFINITY`, `NEG_INFINITY`                                                                                        |
| `i8`..`i64`, `u8`..`u64` | `MAX`, `MIN`                                                                                                                        |

## Traits

```wado
// Trait declaration
trait Greet {
    fn greet(&self) -> String;
}

// Trait implementation
impl Greet for Person {
    fn greet(&self) -> String {
        return `Hello, {self.name}!`;
    }
}

// Multiple traits on the same struct
trait Named {
    fn name(&self) -> String;
}

trait Aged {
    fn age(&self) -> i32;
}

impl Named for Person {
    fn name(&self) -> String { return self.name; }
}

impl Aged for Person {
    fn age(&self) -> i32 { return self.age; }
}

// Trait method call (resolved at compile time)
let p = Person { name: "Alice", age: 30 };
println(p.greet());  // Calls Person's Greet::greet
println(p.name());   // Calls Person's Named::name

// Trait with default method implementation
trait Summary {
    fn title(&self) -> String;

    // Default method - implementors can override or use as-is
    fn summary(&self) -> String {
        return `Title: {self.title()}`;
    }
}

impl Summary for Article {
    fn title(&self) -> String { return self.headline; }
    // summary() uses the default implementation
}

impl Summary for DetailedArticle {
    fn title(&self) -> String { return self.headline; }
    // Override the default
    fn summary(&self) -> String { return `{self.headline}: {self.body}`; }
}

// Trait with associated type
trait Container {
    type Item;

    fn get(&self) -> Self::Item;
}

impl Container for IntBox {
    type Item = i32;

    fn get(&self) -> Self::Item {
        return self.value;
    }
}
```

Traits use static dispatch - method calls are resolved at compile time to the concrete implementation.

Associated types allow traits to declare placeholder types that are specified by implementors. Use `Self::TypeName` to refer to an associated type within trait methods.

### Builtin Traits

The prelude defines several builtin traits for common operations:

```wado
// Ordering - result of a three-way comparison
enum Ordering {
    Less,    // first value is less than second
    Equal,   // values are equal
    Greater, // first value is greater than second
}

// Eq - equality comparisons (== and !=)
trait Eq {
    fn eq(&self, other: &Self) -> bool;
}

// Ord - ordering comparisons (<, <=, >, >=)
// Returns Ordering for three-way comparison (like C++20 <=> or Rust's cmp)
trait Ord {
    fn cmp(&self, other: &Self) -> Ordering;
}

// IndexValue - value-based index access (arr[i] returns a copy)
trait IndexValue<IndexType> {
    type Output;
    fn index_value(&self, index: IndexType) -> Self::Output;
}

// IndexAssign - index assignment (arr[i] = value)
trait IndexAssign<IndexType> {
    type Input;
    fn index_assign(&mut self, index: IndexType, value: Self::Input);
}

// Index - reference-based index access (for reference-type elements)
trait Index<IndexType> {
    type Output;
    fn index(&self, index: IndexType) -> &Self::Output;
}
```

`String` and `Array<T>` implement `Eq` and `Ord` in the prelude:

```wado
// String comparison (lexicographic)
let a = "apple";
let b = "banana";
if a < b {
    println("apple comes before banana");
}

// Custom Eq implementation
struct Point { x: i32, y: i32 }

impl Eq for Point {
    fn eq(&self, other: &Self) -> bool {
        return self.x == other.x && self.y == other.y;
    }
}

let p1 = Point { x: 1, y: 2 };
let p2 = Point { x: 1, y: 2 };
if p1 == p2 {
    println("Points are equal");
}

// Custom Ord implementation (compare by distance from origin)
impl Ord for Point {
    fn cmp(&self, other: &Self) -> Ordering {
        let dist_self = self.x * self.x + self.y * self.y;
        let dist_other = other.x * other.x + other.y * other.y;
        if dist_self < dist_other {
            return Ordering::Less;
        }
        if dist_self > dist_other {
            return Ordering::Greater;
        }
        return Ordering::Equal;
    }
}

let origin = Point { x: 0, y: 0 };
let far = Point { x: 10, y: 10 };
if origin < far {
    println("origin is closer");
}
```

Note: `IndexValue` returns elements by value (copy) because Wasm GC cannot return references to array elements. `Index` is for containers of reference-type elements.

### Trait Bounds

Type parameters can have trait bounds that constrain what types can be used:

```wado
// Struct with trait bound - T must implement Ord
struct SortedPair<T: Ord> {
    first: T,
    second: T,
}

// Multiple bounds with + syntax
struct PrintableOrd<T: Ord + Printable> {
    value: T,
}

// Works: i32 implements Ord (built-in for primitives)
let pair = SortedPair { first: 1, second: 2 };

// Compile error: MyStruct doesn't implement Ord
// let bad = SortedPair { first: MyStruct {}, second: MyStruct {} };
```

#### Function Trait Bounds

```wado
// Bounds on function type parameters - enforced at call sites
fn max<T: Ord>(a: T, b: T) -> T {
    if a > b { return a; }
    return b;
}

max::<i32>(1, 2);        // OK: i32 implements Ord
// max::<MyStruct>(...);  // Compile error: MyStruct doesn't implement Ord
```

#### Bounded impl Blocks

```wado
// Methods only available when T: Ord
impl Array<T: Ord> {
    pub fn sort(&mut self) { ... }
    pub fn sorted(&self) -> Array<T> { ... }
}

let mut nums: Array<i32> = [3, 1, 2];
nums.sort();  // OK: i32 implements Ord

struct Foo {}
let mut foos: Array<Foo> = [];
// foos.sort();  // Compile error: Foo doesn't implement Ord
```

#### Bounded Trait Implementations

```wado
// Pair<T> implements Eq only when T: Eq
impl<T: Eq> Eq for Pair<T> {
    fn eq(&self, other: &Self) -> bool {
        return self.first == other.first && self.second == other.second;
    }
}
```

Built-in trait implementations:

- All primitive types (`i32`, `f64`, `bool`, etc.) implement `Eq` and `Ord`
- Custom types must explicitly implement traits

## Control Flow

```wado
// If statement
if x > 0 {
    println("positive");
} else if x < 0 {
    println("negative");
} else {
    println("zero");
}

// If expression (produces a value)
let abs = if x < 0 { -x } else { x };

// If expression with else-if
let grade = if score >= 90 {
    "A"
} else if score >= 80 {
    "B"
} else {
    "C"
};

// Trailing semicolon is optional in blocks (like trailing commas)
let y = if cond { 42 } else { 0 };     // no trailing semicolon
let z = if cond { 42; } else { 0; };   // trailing semicolon (same result)

// If with init (Go-style)
if let x = get_value(); x > 0 {
    println(`positive: {x}`);
} else {
    println(`non-positive: {x}`);
}
// x is not in scope here

// If let pattern matching (Rust-style)
let opt: Option<i32> = Option::<i32>::Some(42);
if let Some(x) = opt {
    println(`Got: {x}`);
} else {
    println("None");
}

// While
while i < 10 {
    i += 1;
}

// While let pattern matching
let mut iter = items.iter();
while let Some(x) = iter.next() {
    println(`{x}`);
}

// For (C-style)
for let mut i = 0; i < 10; i += 1 {
    println(`{i}`);
}

// For with pattern condition
let mut iter = items.iter();
for ; let Some(x) = iter.next(); {
    println(`{x}`);
}

// For with pattern and update expression
let mut iter = items.iter();
let mut count = 0;
for ; let Some(x) = iter.next(); count += 1 {
    println(`item {count}: {x}`);
}

// For-of (any IntoIterator type)
for let item of items {
    println(`{item}`);
}
// Works with Array<T> and any type implementing IntoIterator

// Infinite loop
loop {
    if done {
        break;
    }
    continue;
}

// Labeled block (creates new scope)
let x = 10;
scope: {
    let x = 20;  // shadows outer x
    println(`x = {x}`);  // 20
}
println(`x = {x}`);  // 10 (outer x unchanged)

// Nested labeled blocks
outer: {
    let a = 1;
    inner: {
        let b = 2;
        println(`{a + b}`);  // 3
    }
    // b is not visible here
}

// Match expression
let opt: Option<i32> = Option::<i32>::Some(42);
let result = match opt {
    Some(x) => x * 2,
    None => 0,
};

// Match with custom variants
variant Shape {
    Circle(f64),
    Point,
}
let shape = Shape::Circle(5.0);
let area = match shape {
    Circle(r) => 3.14159 * r * r,
    Point => 0.0,
};

// Match with wildcard (catch-all)
let name = match color {
    Red => "red",
    Green => "green",
    _ => "other",
};

// Match with block bodies
let description = match value {
    Some(n) => {
        let doubled = n * 2;
        return `value is {doubled}`;
    },
    None => "no value",
};

// Match with guard (condition after &&)
let label = match value {
    Some(x) && x > 100 => "large",
    Some(x) && x > 10 => "medium",
    Some(_) => "small",
    None => "none",
};

// Matches operator - returns bool for pattern testing
let is_some = opt matches { Some(_) };
let is_large = opt matches { Some(x) && x > 10 };  // with guard

// Use matches in conditions
if shape matches { Circle(_) } {
    println("it's a circle");
}

// Pattern bindings are scoped to the guard only
// This is a compile error - x is not in scope outside matches:
// if opt matches { Some(x) } && x > 0 { }  // ERROR: 'x' is not in scope

// For value extraction, use if let instead:
if let Some(x) = opt {
    if x > 0 {
        println("positive value");
    }
}
```

## Operators

```wado
// Arithmetic
+ - * / %

// Comparison (can be chained with restrictions)
== != < <= > >=
a < b < c       // same as: a < b && b < c
a == b == c     // same as: a == b && b == c

// Chaining restrictions:
// - `!=` CANNOT be chained: `a != b != c` is an error
// - Cannot mix `==` with inequalities: `a == b < c` is an error
// - Same-direction only: `a < b < c` OK, `a < b > c` is an error

// Logical
&& || !

// Bitwise
& | ^ ~ << >>

// Assignment
= += -= *= /= %= &= |= ^= <<= >>=

// Type cast
42 as f64
'A' as i32              // char -> i32: 65
'A' as u32              // char -> u32: 65
// 65 as char            // compile error: use char::from_i32()

// Pattern testing (returns bool)
opt matches { Some(_) }
opt matches { Some(x) && x > 0 }  // with guard

// Reference and Dereference
&x              // create reference
*ref            // dereference
```

## References

```wado
let x = 42;
let r = &x;           // immutable reference
let v = *r;           // dereference

let mut y = 0;
let mr = &mut y;      // mutable reference
*mr = 10;             // assign through reference

// Reference to reference (GC-managed)
let rr = &r;          // &&i32
let val = **rr;       // double dereference

// &mut to & coercion (automatic)
fn read(r: &i32) { ... }
read(&mut y);         // OK: &mut i32 coerced to &i32

// Key differences from Rust (GC-based memory model):
// - No borrow checker: multiple mutable references allowed
// - Can return references to local variables (GC keeps them alive)
// - No lifetime annotations needed
```

## Value Semantics

```wado
// Value types copy on assignment
let s1 = "hello";
let s2 = s1;          // s1 is copied, both are usable

let arr1: Array<i32> = [1, 2, 3];
let arr2 = arr1;      // arr1 is copied (deep copy)

// Reference types share the value (no copy)
let x = 42;
let r1 = &x;
let r2 = r1;          // r2 shares the same reference, no copy
```

Value types (primitives, String, Array, Tuple, Struct) have **value semantics**: assignment creates a copy. Reference types (`&T`, `&mut T`) share the underlying value.

## Assert

```wado
assert x > 0;
assert x > 0, "x must be positive";
```

## Tests

```wado
// Named test
test "addition works" {
    assert 1 + 1 == 2;
}

// Unnamed test (identified by file:line)
test {
    let result = fib(10);
    assert result == 55;
}

// Expect-trap test: passes when the body traps (panics/unreachable/failed assert)
#[expect_trap]
test "panics on invalid input" {
    panic("bad input");
}

#[expect_trap]
test {
    unreachable();
}

// TODO test: like #[expect_trap] but with a distinct runner message when it passes
#[TODO]
test "not yet implemented" {
    panic("TODO: implement this");
}
```

Run tests with the CLI:

```sh
wado test                       # discover and run *_test.wado files
wado test file.wado             # run specific file
wado test --filter pattern      # filter tests by name
```

## Imports

```wado
// Named imports
use { println, eprintln } from "core:cli";

// Effect with operations
use { Stdout, Stdout::{write_via_stream} } from "wasi:cli";

// Namespace import
use utils from "./utils.wado";

// Rename
use { foo as bar } from "./mod.wado";

// Re-export (pub use)
pub use { foo, bar } from "./internal.wado";   // re-export for other modules
pub use { foo as baz } from "./internal.wado"; // re-export with rename
```

## Effects

```wado
// Declare effect requirement
fn write_file(path: String, data: String) with FileSystem {
    // ...
}

// Multiple effects
fn main() with Stdout, FileSystem {
    // ...
}

// No effects = pure function
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

// Effect in function type position (higher-order functions)
fn for_each(items: Array<i32>, f: fn(i32) with Stdout) with Stdout {
    for let item of items {
        f(item);
    }
}
```

## Reference Storage (`stores`)

```wado
// Declare that function stores a reference parameter
fn register(data: &Data) -> Handle with stores[data] {
    registry.push(data);  // stores the reference
    return new_handle();
}

// Combined with effects
fn store_and_log(data: &Data) with Stdout, stores[data] {
    println("Storing...");
    save(data);
}

// Functor type that stores its parameter
fn take_storing(f: Fn(&Data) with stores[0]) { ... }
```

Note: `stores` is for function parameters. Closures use "capture" terminology (`|| x + 1` captures `x`).

## Entrypoint

The entrypoint is defined in a World, which requires `export` keyword.

`run()` is the entry point for wasi:cli Command world.

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello!");
}
```

`handle(request)` is the entry point for the wasi:http/service world. It must be declared `async` because HTTP handlers use the Component Model async calling convention, which allows the function to remain alive after delivering the response headers (e.g. to fulfill trailers futures).

```wado
use { Request, Response, ErrorCode, Fields, Trailers } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_future, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Fields::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_future);

    // task return delivers the result to the CM runtime without ending the function.
    // The function continues executing after this point to fulfill trailers.
    task return Result::<Response, ErrorCode>::Ok(response);
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

### `task return` Statement

`task return expr;` is a statement valid only inside `export async fn` bodies. It delivers the function's result to the Component Model runtime without terminating the Wasm function, allowing cleanup and future fulfillment to continue afterward.

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    // ... build response ...

    task return Result::<Response, ErrorCode>::Ok(response);  // deliver result

    // Function is still alive here — fulfill outstanding futures
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

The expression is type-checked against the declared return type of the enclosing `export async fn`. Regular `return` is forbidden in `async fn` bodies.

## Standard Library

```wado
// core:prelude (auto-imported)
panic("error message");   // trap with message
unreachable();            // trap

// core:cli - Output
use { println, eprintln, print, eprint, Stdout, Stderr } from "core:cli";
println("with newline");
eprintln("error line");

// core:clocks
use { now, MonotonicClock } from "core:clocks";
let t = now();            // current time in nanoseconds

// core:collections - TreeMap (insertion-order preserved)
use { TreeMap } from "core:collections";
let mut map = TreeMap::<String, i32>::new();
map["key"] = 42;          // index assignment
map["key2"] = 100;
if let Some(v) = map["key"] { ... }  // index access returns Option<V>
let keys = map.keys();    // keys in insertion order
map.remove("key");
```

### char Conversion

```wado
// char -> integer (extracts code point, truncated for smaller types)
let code = 'A' as i32;              // 65
let ucode = 'A' as u32;            // 65
let byte = 'A' as u8;              // 65

// u8 -> char is always valid (all u8 values are valid Unicode)
let c = (65 as u8) as char;         // 'A'

// integer -> char: use checked conversion (as char is a compile error)
let c = char::from_u32(65 as u32);  // Option<char>: Some('A')
let c = char::from_i32(65);         // Option<char>: Some('A')

// Invalid values return null
char::from_u32(0xD800 as u32);      // null (surrogate)
char::from_u32(0x110000 as u32);    // null (out of range)
char::from_i32(-1);                 // null (negative)

// Unchecked conversion (caller must ensure validity)
let c = char::from_u32_unchecked(65 as u32);  // 'A'
```

### Math Functions

Math functions are provided as static methods on `f64` and `f32`:

```wado
// Constants
let pi = f64::PI;
let e = f64::E;

// Wasm instruction math (single-instruction, fast)
f64::abs(x)        f64::ceil(x)       f64::floor(x)
f64::trunc(x)      f64::round(x)      f64::sqrt(x)
f64::min(x, y)     f64::max(x, y)     f64::copysign(x, y)

// Transcendental math (bundled deterministic libm)
f64::sin(x)        f64::cos(x)        f64::tan(x)
f64::asin(x)       f64::acos(x)       f64::atan(x)
f64::atan2(y, x)   f64::sinh(x)       f64::cosh(x)
f64::tanh(x)       f64::asinh(x)      f64::acosh(x)
f64::atanh(x)      f64::exp(x)        f64::exp2(x)
f64::expm1(x)      f64::ln(x)         f64::log2(x)
f64::log10(x)      f64::ln1p(x)       f64::pow(x, y)
f64::cbrt(x)       f64::hypot(x, y)   f64::fmod(x, y)

// f32 has the same set of functions
f32::sin(x)        f32::sqrt(x)       f32::PI

// Integer min/max
i32::min(a, b)     i32::max(a, b)
i64::min(a, b)     i64::max(a, b)
// Also available for i8, u8, i16, u16, u32, u64
```

## Generic Functions and Methods

```wado
// Generic function
fn identity<T>(x: T) -> T {
    return x;
}

// Generic method
impl Container {
    fn transform<T, U>(&self, a: T, b: U) -> T {
        return a;
    }
}

// Calling with turbofish syntax (explicit type arguments)
let x = identity::<i32>(42);
let y = container.transform::<i32, i64>(10, 20 as i64);
```

## Closures

```wado
// Pure closure (no captures) - expression body
let add_one = |x: i32| x + 1;
let result = add_one(5);  // 6

// Closure with multiple parameters
let add = |a: i32, b: i32| a + b;
let sum = add(3, 4);  // 7

// Closure returning different types
let is_even = |x: i32| x % 2 == 0;
let check = is_even(4);  // true

// Closure with block body (requires explicit return)
let compute = |x: i32| {
    let doubled = x * 2;
    let tripled = x * 3;
    return doubled + tripled;
};
let result = compute(4);  // 20

// Closure returning struct literal
let make_point = |x: i32, y: i32| Point { x, y };

// Capturing outer variables (value semantics - copy)
let multiplier = 10;
let scale = |x: i32| x * multiplier;  // Captures `multiplier` by value
let result = scale(5);  // 50

// Mutable capture: use &mut || to mutate captured variables
let mut count = 0;
let inc = &mut || { count += 1; };
inc();
inc();
println(`{count}`);  // 2

// Multiple closures sharing the same mutable variable
let mut count = 0;
let inc = &mut || { count += 1; };
let get = || count;
inc();
inc();
println(`{get()}`);  // 2
```

## Iterators

Wado provides iterator traits for generic iteration over collections.

### Iterator Traits

```wado
// Iterator - the core trait for yielding values
trait Iterator {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
}

// IntoIterator - convert a collection into an iterator
trait IntoIterator {
    type Item;
    type Iter;
    fn into_iter(&self) -> Self::Iter;
}

// FromIterator - create a collection from an iterator
trait FromIterator<T> {
    type Iter;
    fn from_iter(iter: Self::Iter) -> Self;
}
```

### ArrayIter and Array Iteration

```wado
// Array<T> implements IntoIterator
let arr: Array<i32> = [1, 2, 3, 4, 5];

// for-of uses IntoIterator automatically
for let x of arr {
    println(`{x}`);
}

// Get iterator explicitly
let mut iter = arr.iter();

// Manual iteration
loop {
    if let Some(x) = iter.next() {
        println(`{x}`);
    } else {
        break;
    }
}

// Collect remaining elements into a new array
let mut iter2 = arr.iter();
iter2.next();  // skip first element
let rest = iter2.collect();  // Array<i32> with [2, 3, 4, 5]
```

### Iterator vs IntoIterator

| Trait            | Question                               | Examples                                         |
| ---------------- | -------------------------------------- | ------------------------------------------------ |
| **Iterator**     | "Can I call `next()` on this?"         | `ArrayIter<T>`, `StrCharIter`, `StrUtf8ByteIter` |
| **IntoIterator** | "Can I convert this into an iterator?" | `Array<T>`, `StrCharIter`, `StrUtf8ByteIter`     |

Collections like `Array<T>` implement `IntoIterator` to produce a separate iterator object.
String iterators (`StrCharIter`, `StrUtf8ByteIter`) implement both `Iterator` and `IntoIterator`,
so they work directly with `for-of`:

```wado
let arr: Array<i32> = [1, 2, 3];
// arr.next() would NOT work - Array has no next() method

let mut iter: ArrayIter<i32> = arr.into_iter();
// ArrayIter implements Iterator
iter.next();  // Some(1)
iter.next();  // Some(2)

// String iterators (StrCharIter, StrUtf8ByteIter) implement both traits,
// so they work directly with for-of (see Strings section for examples)
```

### Custom Iterables

Implement `IntoIterator` to make custom types work with `for-of`:

```wado
struct Stack<T> {
    items: Array<T>,
}

struct StackIter<T> {
    items: Array<T>,
    index: i32,
}

impl Iterator for StackIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < 0 {
            return null;
        }
        let item = self.items.get(self.index);
        self.index -= 1;
        return Option::<T>::Some(item);
    }
}

impl IntoIterator for Stack<T> {
    type Item = T;
    type Iter = StackIter<T>;

    fn into_iter(&self) -> StackIter<T> {
        return StackIter {
            items: self.items,
            index: self.items.len() - 1,
        };
    }
}

// Now for-of works with Stack
for let x of stack {
    println(`{x}`);  // Iterates in LIFO order
}
```

### Iterator Combinators

```wado
let arr: Array<i32> = [1, 2, 3, 4, 5];

// map - transform each element
let doubled = arr.iter().map(|x: i32| x * 2).collect();
// [2, 4, 6, 8, 10]

// filter - keep elements matching predicate
let evens = arr.iter().filter(|x: i32| x % 2 == 0).collect();
// [2, 4]

// fold - reduce to single value
let sum = arr.iter().fold(0, |acc: i32, x: i32| acc + x);
// 15

// Chaining combinators
let result = arr.iter()
    .filter(|x: i32| x > 2)
    .map(|x: i32| x * 10)
    .collect();
// [30, 40, 50]
```

## Compile-Time Location Literals

```wado
// Get current source file
let file = #file;           // "<entry>" or "./module.wado"

// Get current line number (1-indexed)
let line = #line;           // i32

// Get current function name
let func = #function;       // "run" or "Point::distance"

// Example: debug logging
fn log_debug(message: String) with Stdout {
    println(`[{#file}:{#line}] {message}`);
}
```

## Attributes

```wado
// Disable auto-import of core:prelude
#![no_prelude]

struct Foo {
    #[hidden]
    secret: String, // won't be shown in debug stringify
}

// Test attributes
#[expect_trap]
test "should panic" {
    panic("intentional");  // test passes because it traps
}

#[TODO]
test "not yet implemented" {
    panic("TODO");  // passes while trapping; runner warns when it stops trapping
}
```

## Macros

Wado intentionally does not support macros.

## Not Yet Implemented

- `resource` (Wasm CM resource handles)
- Trait bounds: using bounds for method resolution on type params (e.g., calling `T.method()` where `T: Trait`)
- Effect handlers
- `reactive` values and `observe()`
- `stores[...]` syntax for reference storage
- postfix `?` operator (error propagation)
- JSX
- Generic function/method type inference
- Generic variant pattern matching (custom `Maybe<T>`)
- `Result<T, E>` pattern matching

## See Also

- [wado-compiler/tests/fixtures/\*.wado](wado-compiler/tests/fixtures) - E2E test fixtures
