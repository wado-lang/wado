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
42              // i32 (default)
42 as i64       // i64 via cast
3.14            // f64 (default)
3.14 as f32     // f32 via cast
1_000_000       // underscores for readability
0xFF            // hex
0b1010          // binary
0o755           // octal

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
// Module-level globals (compile to Wasm globals)
global PI: f64 = 3.14159;           // immutable
global mut counter: i32 = 0;        // mutable

// With visibility
pub global VERSION: i32 = 1;        // accessible from other modules

// Usage
fn example() {
    println(`{PI}`);                // read global
    counter = counter + 1;          // write mutable global
}
```

Global variables map directly to WebAssembly globals. Only literal initializers are currently supported.

## Types

```wado
// Primitives
i8, i16, i32, i64, i128  // signed integers
u8, u16, u32, u64, u128  // unsigned integers
f32, f64                 // floats
bool, char

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

## Type Alias

```wado
type Kilometers = i32;    // alias - same type as i32
type UserID = String;

let km: Kilometers = 100;
let m: i32 = km;          // OK - interchangeable

// For distinct types, use struct wrapper (newtype pattern)
struct Miles { value: i32 }
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
```

## Strings

```wado
// String literals
let s = "hello";                         // String literal (UTF-8)

// String constructors
let s = String::with_capacity(100);      // empty string with pre-allocated capacity

// String methods
let s = "hello";
let n = s.len();                         // get length in bytes (5)
let empty = s.is_empty();                // check if empty (false)
let byte = s.get(0);                     // get byte at index
s.set(0, 72);                            // set byte at index (requires mut)

// String building (O(1) amortized append)
let mut builder = String::with_capacity(20);
builder.append("Hello");
builder.append(", ");
builder.append("World!");
// builder is now "Hello, World!"

// String concatenation (static method)
let combined = String::concat("Hello, ", "World!");  // "Hello, World!"
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

Enums are discriminated values without payloads:

```wado
enum Color {
    Red,
    Green,
    Blue,
}

let c = Color::Red;  // enum value (i32 discriminant)
```

Note: Enum pattern matching is not yet implemented.

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

// World export (exposed at CM boundary)
export fn run() { ... }
```

A function must have `return` if it returns a value. This is applied to methods and closures as well.

## Visibility

Wado has two kinds of "public":

| Keyword  | Term          | Scope                    |
| -------- | ------------- | ------------------------ |
| `pub`    | module public | Other Wado modules       |
| `export` | world export  | Component Model boundary |

```wado
fn private_fn() { }           // module-private (default)
pub fn public_fn() { }        // module public
export fn entry() { }         // world export
pub export fn both() { }      // both
```

All entity definitions can have `pub` visibility.

## Methods

```wado
impl Point {
    // Instance method (has self parameter)
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

// Instance method call
let mut p = Point { x: 1, y: 2 };
let s = p.sum();
p.reset();

// Static method call
let origin = Point::origin();

// Static method on generic type (turbofish syntax)
let arr = Array::<i32>::with_capacity(10);
```

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
// Eq - equality comparisons (== and !=)
trait Eq {
    fn eq(&self, other: &Self) -> bool;
}

// Ord - ordering comparisons (<, <=, >, >=)
trait Ord {
    fn lt(&self, other: &Self) -> bool;
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

Built-in trait implementations:

- All primitive types (`i32`, `f64`, `bool`, etc.) implement `Eq` and `Ord`
- Custom types must explicitly implement traits

Note: Function type parameter bounds (`fn foo<T: Trait>(x: T)`) are parsed but not yet enforced.

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
use {println, eprintln} from "core:cli";

// Effect with operations
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

// Namespace import
use utils from "./utils.wado";

// Rename
use {foo as bar} from "./mod.wado";

// Re-export (pub use)
pub use {foo, bar} from "./internal.wado";  // re-export for other modules
pub use {foo as baz} from "./internal.wado"; // re-export with rename
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

```wado
use {println, Stdout} from "core:cli";

// run() is the entry point for wasi:cli Command world
fn run() with Stdout {
    println("Hello!");
}
```

## Standard Library

```wado
// core:prelude (auto-imported)
panic("error message");   // trap with message
unreachable();            // trap

// core:cli - Output
use {println, eprintln, print, eprint, Stdout, Stderr} from "core:cli";
println("with newline");
eprintln("error line");

// core:clocks
use {now, MonotonicClock} from "core:clocks";
let t = now();            // current time in nanoseconds

// core:collections - TreeMap (sorted map)
use {TreeMap} from "core:collections";
let mut map = TreeMap::<String, i32>::new();
map["key"] = 42;          // index assignment
map["key2"] = 100;
if let Some(v) = map["key"] { ... }  // index access returns Option<V>
let keys = map.keys();    // keys in sorted order
map.remove("key");
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

| Trait            | Question                               | Examples             |
| ---------------- | -------------------------------------- | -------------------- |
| **Iterator**     | "Can I call `next()` on this?"         | `ArrayIter<T>`       |
| **IntoIterator** | "Can I convert this into an iterator?" | `Array<T>`, `String` |

Collections like `Array<T>` implement `IntoIterator` to produce a separate iterator object:

```wado
let arr: Array<i32> = [1, 2, 3];
// arr.next() would NOT work - Array has no next() method

let mut iter: ArrayIter<i32> = arr.into_iter();
// ArrayIter implements Iterator
iter.next();  // Some(1)
iter.next();  // Some(2)
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
```

User-facing attributes are not yet supported.

## Macros

Wado intentionally does not support macros.

## Not Yet Implemented

- `enum` pattern matching
- `flags` (parsed but no codegen)
- `resource` (Wasm CM resource handles)
- Trait bounds (`T: Display`)
- Default trait method implementations
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
