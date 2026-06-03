# WEP: Struct and Trait System

## Context

Wado needs a type system for defining custom data types and shared behavior. This WEP defines the syntax and semantics for structs (product types) and traits (behavior abstraction), building on the value semantics established in `wep-2026-01-12-value-semantics-and-stores.md`.

### Design Goals

1. **Value semantics by default**: Align with the value semantics WEP
2. **Explicit ownership transfer**: Use `move` for explicit moves
3. **Compiler optimization**: Allow automatic move when safe
4. **Wasm GC compatibility**: Map naturally to Wasm GC structs
5. **Familiar syntax**: Rust-like syntax for ease of learning

## Decision

### 1. Struct Definition

Structs define product types with named fields:

```wado
struct Point {
    x: i32,
    y: i32,
}

// With visibility modifiers
pub struct User {
    pub name: String,
    pub age: i32,
    active: bool,  // private by default
}

// Generic struct
struct Pair<T, U> {
    first: T,
    second: U,
}

// Recursive struct (enabled by Wasm GC)
struct Node<T> {
    value: T,
    next: Option<Node<T>>,
}
```

### 2. Struct Instantiation

```wado
let p = Point { x: 10, y: 20 };

// Shorthand when variable name matches field
let x = 10;
let y = 20;
let p = Point { x, y };

// Struct update syntax
let p2 = Point { x: 100, ..p };  // copies y from p
```

### 3. Method Blocks (`impl`)

Methods are defined in `impl` blocks:

```wado
impl Point {
    // Associated function (no self) - called via Point::new()
    pub fn new(x: i32, y: i32) -> Point {
        return Point { x, y };
    }

    // Method with immutable self reference
    pub fn magnitude(&self) -> f64 {
        return ((self.x * self.x + self.y * self.y) as f64).sqrt();
    }

    // Method with mutable self reference
    pub fn translate(&mut self, dx: i32, dy: i32) {
        self.x += dx;
        self.y += dy;
    }

    // Method that consumes self (takes ownership)
    pub fn into_tuple(self) -> [i32, i32] {
        return [self.x, self.y];
    }
}

// Usage
let p = Point::new(3, 4);
let mag = p.magnitude();  // 5.0

let mut p2 = Point::new(0, 0);
p2.translate(10, 20);

let tuple = p.into_tuple();  // p is consumed
```

### 4. Methods with Effects

Methods can declare effect requirements:

```wado
impl User {
    pub fn save(&self) with FileSystem {
        FileSystem::write(`users/{self.name}.json`, self.to_json());
    }

    pub fn load(name: String) -> Result<User, IoError> with FileSystem {
        let data = FileSystem::read(`users/{name}.json`)?;
        return User::from_json(data);
    }
}
```

### 5. Trait Definition

Traits define shared behavior:

```wado
trait Display {
    fn display(&self) -> String;
}

trait Default {
    fn default() -> Self;
}

// Trait with default implementation
trait Greet {
    fn name(&self) -> String;

    fn greet(&self) -> String {
        return `Hello, {self.name()}!`;
    }
}

// Trait with associated type
trait Iterator {
    type Item;

    fn next(&mut self) -> Option<Self::Item>;
}

// Trait with effect requirements
trait Loadable {
    fn load(path: String) -> Result<Self, IoError> with FileSystem;
}
```

### 6. Trait Implementation

```wado
impl Display for Point {
    fn display(&self) -> String {
        return `Point({self.x}, {self.y})`;
    }
}

impl Default for Point {
    fn default() -> Point {
        return Point { x: 0, y: 0 };
    }
}

// Generic impl
impl<T: Display> Display for Pair<T, T> {
    fn display(&self) -> String {
        return `({self.first.display()}, {self.second.display()})`;
    }
}
```

### 7. Trait Bounds

```wado
// Single bound
fn print_all<T: Display>(items: List<T>) with Stdout {
    for item in items {
        println(item.display());
    }
}

// Multiple bounds with +
fn process<T: Display + Default>(value: Option<T>) -> String {
    match value {
        Some(v) => v.display(),
        None => T::default().display(),
    }
}

// Where clause for complex bounds
fn merge<T, U>(a: T, b: U) -> String
where
    T: Display + Clone,
    U: Display,
{
    return `{a.display()} and {b.display()}`;
}
```

### 8. Trait Objects (Dynamic Dispatch)

```wado
// Trait object type with &dyn
fn log_item(item: &dyn Display) with Stdout {
    println(item.display());
}

// In collections
let items: List<&dyn Display> = [&point, &user];

// Owned trait object with Box
let boxed: Box<dyn Display> = Box::new(point);
```

**Implementation Note**: `dyn` trait objects map to Wasm GC reference types with a vtable struct.

### 9. Copy and Clone Semantics

Building on the value semantics WEP:

#### Rules

1. **Primitives**: Bitwise copy (implicit, no trait needed)
2. **Structs**:
   - All fields bitwise-copyable → bitwise copy
   - Any field implements `Clone` → automatically call `.clone()` on those fields
3. **`move` operator**: Explicit ownership transfer, no copy/clone
4. **Compiler optimization**: MAY move instead of clone when safe

#### The Clone Trait

```wado
trait Clone {
    fn clone(&self) -> Self;
}
```

#### Examples

```wado
// Primitives - bitwise copy
let a: i32 = 42;
let b = a;  // bitwise copy, both valid

// Simple struct - bitwise copy (all fields are primitives)
struct Point { x: i32, y: i32 }

let p1 = Point { x: 1, y: 2 };
let p2 = p1;  // bitwise copy

// Struct with Clone field - automatic deep copy
struct User {
    name: String,  // String: Clone
    age: i32,      // primitive
}

let u1 = User { name: "Alice", age: 30 };
let u2 = u1;  // Compiler generates: User { name: u1.name.clone(), age: u1.age }

// Explicit move - no clone
let u3 = move u1;  // u1 invalidated
println(u1.name);  // ERROR: u1 has been moved
```

#### Compiler Move Optimization

The compiler MAY optimize clone to move when safe (value not used after):

```wado
// Compiler WILL move (s1 not used after)
let s1 = String::from(bytes);
let s2 = s1;
// s1 not used anymore → compiler moves, no clone
println(s2);

// Compiler MUST clone (s1 used after)
let s1 = String::from(bytes);
let s2 = s1;
println(s1);  // s1 used here → must clone
println(s2);
```

| Pattern                       | Compiler Action |
| ----------------------------- | --------------- |
| Last use before assignment    | Move            |
| Used after assignment         | Clone           |
| Passed to function (last use) | Move            |
| Returned from function        | Move (RVO)      |
| Explicit `move`               | Move (enforced) |

### 10. Built-in Traits

Wado provides several built-in traits in the prelude:

#### Clone

For types that can be deeply copied:

```wado
trait Clone {
    fn clone(&self) -> Self;
}

// Standard library implementations
impl Clone for String { ... }
impl<T: Clone> Clone for List<T> { ... }
impl<T: Clone> Clone for Option<T> { ... }
impl<K: Clone, V: Clone> Clone for TreeMap<K, V> { ... }
```

#### Drop

For types that need cleanup when going out of scope:

```wado
trait Drop {
    fn drop(&mut self);
}

// Example: file handle cleanup
impl Drop for FileHandle {
    fn drop(&mut self) {
        self.close();
    }
}
```

**Note**: Unlike Rust, Wado runs on Wasm GC, so `Drop` is primarily for releasing external resources (file handles, network connections), not memory deallocation.

#### Default

For types with a default value:

```wado
trait Default {
    fn default() -> Self;
}

impl Default for i32 {
    fn default() -> i32 { return 0; }
}

impl Default for String {
    fn default() -> String { return ""; }
}

impl<T> Default for List<T> {
    fn default() -> List<T> { return []; }
}

impl<T> Default for Option<T> {
    fn default() -> Option<T> { return None; }
}
```

#### Display

For human-readable string representation:

```wado
trait Display {
    fn display(&self) -> String;
}

// Used by template strings
let p = Point { x: 1, y: 2 };
let s = `Point is: {p}`;  // calls p.display()
```

#### Debug

For debug/developer-oriented string representation:

```wado
trait Debug {
    fn debug(&self) -> String;
}

// Used by debug printing
debug_println(value);  // calls value.debug()
```

#### Eq

For equality comparison (`==` and `!=`):

```wado
trait Eq {
    fn eq(&self, other: &Self) -> bool;
}
```

All primitive types implement `Eq`. Custom types implement `Eq` to enable `==` and `!=` operators.

#### Ord

For ordering comparison (`<`, `<=`, `>`, `>=`):

```wado
variant Ordering {
    Less,
    Equal,
    Greater,
}

trait Ord {
    fn cmp(&self, other: &Self) -> Ordering;
}
```

All primitive types implement `Ord`. Custom types implement `Ord` to enable comparison operators.

#### Hash

For hashable types:

```wado
trait Hash {
    fn hash(&self) -> u64;
}
```

Note: `TreeMap` (the default associative array in Wado) uses `Ord` for key ordering, not `Hash`.

#### Iterator

For iterable types:

```wado
trait Iterator {
    type Item;

    fn next(&mut self) -> Option<Self::Item>;

    // Default implementations for common operations
    fn map<U, F: Fn(Self::Item) -> U>(self, f: F) -> Map<Self, F> { ... }
    fn filter<F: Fn(&Self::Item) -> bool>(self, f: F) -> Filter<Self, F> { ... }
    fn fold<U, F: Fn(U, Self::Item) -> U>(self, init: U, f: F) -> U { ... }
    fn collect<C: FromIterator<Self::Item>>(self) -> C { ... }
}

trait IntoIterator {
    type Item;
    type Iter: Iterator<Item = Self::Item>;

    fn into_iter(self) -> Self::Iter;
}
```

#### FromIterator

For collecting iterators into containers:

```wado
trait FromIterator<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Self;
}

impl<T> FromIterator<T> for List<T> { ... }
```

### 11. Orphan Rules

Wado enforces orphan rules to prevent conflicting implementations:

- You can implement your trait for any type
- You can implement any trait for your type
- You cannot implement an external trait for an external type

```wado
// OK: your trait for external type
impl MyTrait for String { ... }

// OK: external trait for your type
impl Display for MyStruct { ... }

// ERROR: external trait for external type
impl Display for String { ... }  // Not allowed
```

### 12. Marker Traits

Some traits have no methods and serve as compile-time markers:

```wado
// Sized is implicit for most types
trait Sized {}
```

## Consequences

### Positive

1. **Familiar syntax**: Rust developers will feel at home
2. **Value semantics**: Simple mental model - assignment copies by default
3. **Performance**: Compiler can optimize to move when safe
4. **Explicit moves**: `move` keyword makes ownership transfer clear
5. **Wasm GC fit**: Structs map naturally to Wasm GC struct types
6. **Drop for resources**: Clean up external resources (not memory)
7. **Rich trait ecosystem**: Built-in traits cover common use cases

### Negative

1. **Hidden clones**: Implicit cloning may surprise developers expecting move semantics
   - **Mitigation**: Compiler warnings for expensive implicit clones
2. **Orphan rule limitations**: Cannot extend external types with external traits
   - **Mitigation**: Newtype pattern (`struct MyString(String)`)
3. **No Copy trait**: Unlike Rust, no distinction between bitwise-copy and clone
   - **Mitigation**: Compiler handles this automatically

### Comparison with Rust

| Aspect                | Rust          | Wado                      |
| --------------------- | ------------- | ------------------------- |
| Default on assignment | Move          | Clone (value semantics)   |
| Explicit copy         | `.clone()`    | `move` for avoiding clone |
| Bitwise copy          | `Copy` trait  | Automatic for primitives  |
| Deep copy             | `Clone` trait | `Clone` trait             |
| Memory cleanup        | `Drop` trait  | GC (Drop for resources)   |
| Trait syntax          | Same          | Same                      |
| impl blocks           | Same          | Same                      |

## Examples

### Complete Example

```wado
use {println} from "core:cli";

struct Point {
    x: i32,
    y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Point {
        return Point { x, y };
    }

    pub fn magnitude(&self) -> f64 {
        return ((self.x * self.x + self.y * self.y) as f64).sqrt();
    }

    pub fn translate(&mut self, dx: i32, dy: i32) {
        self.x += dx;
        self.y += dy;
    }
}

impl Clone for Point {
    fn clone(&self) -> Point {
        return Point { x: self.x, y: self.y };
    }
}

impl Default for Point {
    fn default() -> Point {
        return Point { x: 0, y: 0 };
    }
}

impl Display for Point {
    fn display(&self) -> String {
        return `({self.x}, {self.y})`;
    }
}

fn run() with Stdout {
    let p1 = Point::new(3, 4);
    let p2 = p1;  // clone (both valid)

    println(`p1 = {p1}`);  // p1 = (3, 4)
    println(`p2 = {p2}`);  // p2 = (3, 4)
    println(`magnitude = {p1.magnitude()}`);  // magnitude = 5.0

    let mut p3 = Point::default();
    p3.translate(10, 20);
    println(`p3 = {p3}`);  // p3 = (10, 20)

    // Explicit move
    let p4 = move p3;
    // println(p3);  // ERROR: p3 has been moved
}
```

### Generic Container with Traits

```wado
struct Stack<T> {
    items: List<T>,
}

impl<T> Stack<T> {
    pub fn new() -> Stack<T> {
        return Stack { items: [] };
    }

    pub fn push(&mut self, item: T) {
        self.items.push(item);
    }

    pub fn pop(&mut self) -> Option<T> {
        return self.items.pop();
    }

    pub fn is_empty(&self) -> bool {
        return self.items.len() == 0;
    }
}

impl<T: Clone> Clone for Stack<T> {
    fn clone(&self) -> Stack<T> {
        return Stack { items: self.items.clone() };
    }
}

impl<T: Display> Display for Stack<T> {
    fn display(&self) -> String {
        let parts = self.items.iter().map(|x| x.display()).collect::<List<String>>();
        return `Stack[{parts.join(", ")}]`;
    }
}
```

## References

- [Rust Traits](https://doc.rust-lang.org/book/ch10-02-traits.html)
- [Rust Clone vs Copy](https://doc.rust-lang.org/std/clone/trait.Clone.html)
- [Rust Drop](https://doc.rust-lang.org/std/ops/trait.Drop.html)
- [WEP: Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
