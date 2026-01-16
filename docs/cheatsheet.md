# Wado Cheatsheet

Quick reference for Wado syntax.

## Concepts

## Comments

```wado
// Line comment
/* Block comment */
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

## Types

```wado
// Primitives
i8, i16, i32, i64       // signed integers
u8, u16, u32, u64       // unsigned integers
f32, f64                // floats
bool, char

// Composite
String                  // UTF-8 string
Array<T>                // dynamic array
[T, U, V]               // tuple type
Option<T>               // optional value
Result<T, E>            // result type (not yet implemented)

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

## Enums (parsing only, codegen not yet implemented)

```wado
enum Color {
    Red,
    Green,
    Blue,
}

let c = Color::Red;  // not yet implemented
```

## Functions

```wado
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

// With effects
fn greet(name: String) with Stdout {
    println(`Hello, {name}!`);
}

// Public
pub fn api_function() -> i32 {
    return 42;
}
```

## Methods

```wado
impl Point {
    fn sum(&self) -> i32 {
        return self.x + self.y;
    }

    fn reset(&mut self) {
        self.x = 0;
        self.y = 0;
    }
}

let mut p = Point { x: 1, y: 2 };
let s = p.sum();
p.reset();
```

## Control Flow

```wado
// If
if x > 0 {
    println("positive");
} else if x < 0 {
    println("negative");
} else {
    println("zero");
}

// While
while i < 10 {
    i += 1;
}

// For (C-style)
for let mut i = 0; i < 10; i += 1 {
    println(`{i}`);
}

// For-of (iterables)
for let item of items {
    println(`{item}`);
}

// Infinite loop
loop {
    if done {
        break;
    }
    continue;
}
```

## Operators

```wado
// Arithmetic
+ - * / %

// Comparison (can be chained)
== != < <= > >=
a < b < c       // same as: a < b && b < c

// Logical
&& || !

// Bitwise
& | ^ ~ << >>

// Assignment
= += -= *= /= %= &= |= ^= <<= >>=

// Type cast
42 as f64

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
```

## Assert

```wado
assert x > 0;
assert x > 0, "x must be positive";
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

## Not Yet Implemented

- `enum` construction (parsed but no codegen)
- `match` statements/expressions
- `variant` (sum types with payloads)
- `flags` (bit flags)
- `trait` declarations
- Effect handlers
- `reactive` values and `observe()`
- Closures (parsed but no codegen)
- `Dict<K, V>`
- postfix `?` operator (error propagation)
- JSX
- Generic function/method type inference

## See Also

- [wado-compiler/tests/fixtures/\*.wado](wado-compiler/tests/fixtures) - E2E test fixtures
