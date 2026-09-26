# Wado Cheatsheet

Quick reference for Wado syntax.

## Shebang

```wado
#!/usr/bin/env -S wado run
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
let p = geo::Point::new(1, 2);      // access via namespace
impl geo::Show for Local { ... }    // and on either side of an impl head
```

### Generated Imports

Any file that is neither `.wado` nor a Wasm asset (`.wasm` / `.wat`) is imported via a generator: `.g4`, `.proto`, a Wado dialect, and so on. Name the generator as a `[build-dependencies]` entry or as a relative path. See [WEP: Kiln](./wep-2026-04-12-kiln.md) for the mechanism, [WEP: Gale](./wep-2026-03-02-gale.md) for the real-world usage.

```wado
use { Parser } from "./Calc.g4" with { // Gale parses ANTLR4 grammar files
    generator: {
        module: "wado-lang:gale",
    },
};
```

Code a generator writes may call a runtime library. Grog's does, so a package
lists `wado-lang:grog` under `[dependencies]` as well. See
[WEP: Grog](./wep-2026-09-22-grog.md).

```wado
use grog from "lib:grog";
use { Account } from "./account.proto" with { generator: { module: "lib:grog" } };

let bytes = grog::encode(&Account { id: 150 });       // [0x08, 0x96, 0x01]
let back = grog::decode::<Account, ByteList>(bytes)?;
```

### Wasm Imports

A `.wasm` / `.wat` asset is imported with `with { type: "wasm" | "wat" }`. The
compiler detects from the binary whether the file is a core module or a
Component Model component — both use `type: "wasm"`. See [WEP: Wasm Module Import](./wep-2026-01-10-wasm-import.md) and [WEP: Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md).

```wado
// Core wasm / wat: each export becomes a free function.
use { sin, cos } from "./libm.wat" with { type: "wat" };
use { helper }   from "./mod.wasm" with { type: "wasm" };

// CM component: each exported interface becomes a Wado `interface`,
// called like a WASI method.
use { Compress, Decompress } from "./brotli.wasm" with { type: "wasm" };

let out = Compress::compress(bytes);  // requires `with Compress`
```

Wado↔CM type correspondence at the boundary is in [the spec](./spec-components.md#type-mapping-at-component-boundaries).

## Value Semantics

See [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

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
`Hello, ${name}` // Template string
`{"key": ${v}}` // Braces are literal → {"key": <v>}
"Hello,
world!"         // Multi-line string

// Byte strings (b-prefix) → ByteList, ASCII + \xNN escapes (no \u)
b"\x89PNG\r\n"                 // ByteList [137, 80, 78, 71, 13, 10]
let raw: List<u8> = b"abc";    // newtype literal coercion to the base type

// Byte literal (b-prefix on a char) → u8, ASCII + \xNN escapes (no \u)
b'A'                           // 65 (coerces like an integer literal)
b'\xff'                        // 255

// Characters
'A'
'\n'
'\u0041'
'\u{1F600}'

// Booleans
true
false

// Null
null // the None of the Option its context expects; Option<!> with none

// Unit
()
```

## Semicolons

```wado
fn f() -> i32 {
    let x = 1;
    return x + 1        // the last statement may drop its `;`
}                       // (a value-returning fn still needs `return`)

let x = 1 let y = 2;    // Error: a newline does not separate (no ASI)
let z = 1;;             // OK: an empty statement, removed by `wado format`

let a = if c { 1; } else { 2; };  // 1 or 2 — a trailing `;` is not Rust's `()`
let u = if c { g(); () } else { () };  // write `()` to mean `()`
let b = { 1 };          // Error: a brace in value position is a struct literal
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

// Any other binder taking a name that already reaches a known symbol warns
// (`shadowed_name`). Waive it per binder or per module with `allow`.
let println = 1;                                  // warns: shadows the function
#[allow(shadowed_name)] let eprintln = 1;         // deliberate, no warning
if let Some(x) = x { }                            // exempt: derives from old x
if let Some(x) = y { }                            // warns: derives from y
match x { Some(x) => ... }                        // warns: x means two things here
```

## Global Variables

See [WEP: Global Variables](./wep-2026-01-27-global-variables.md).

```wado
global PI: f64 = 3.14159;           // immutable
global mut counter: i32 = 0;        // mutable
pub global VERSION: i32 = 1;        // accessible from other modules
global DOUBLED: i32 = 21 * 2;       // expressions allowed
global GREETING: String = `v${VERSION}`;  // template strings too

fn example() {
    println(`${PI}`);                // read global
    counter = counter + 1;          // write mutable global
}
```

An initializer must be pure: calling a function that declares an effect is a
compile error. A user-defined effect's operation is another matter — it may be
dispatched, and traps at module init where no `with … do` installs a handler,
just as it would in a function body. An initializer may install its own.

## Types

```wado
// Primitives
i8, i16, i32, i64         // signed integers
u8, u16, u32, u64         // unsigned integers
f32, f64                  // floats
char                      // a valid unicode scalar
bool

// wide integers (GC types, work like primitives)
i128, u128

// half precision: storage only, no arithmetic and no `as` cast.
// Bits via `to_bits` / `from_bits`, values via `From` / `TryFrom` / `from_f32` /
// `from_f64`, text via `from_str` (each rounded once, as a literal is).
// Every comparison hands the widened value to f32's, so `==` and `<` are IEEE
// and `Ord` is the total order — the same split f32 has.
f16, bf16
let w: List<bf16> = [0.5, -1.25];   // a float literal rounds once, ties to even

// Composites
String                  // UTF-8 string
List<T>                 // dynamic array
[T, U, V]               // tuple
Option<T>               // optional value
Result<T, E>            // result type

// References
&T                      // immutable reference
&mut T                  // mutable reference

// Unit type
()

// Never type
!
```

### Newtype

Newtypes are distinct types that inherit methods/operators/traits from the base type, require explicit `as` cast, and have zero runtime cost.

A newtype does not carry an invariant of its own: `as` converts both ways for
free, so it admits exactly what its base admits. An invariant the base lacks
(UTF-8 bytes, non-empty, normalized) needs a `struct` with a private field and
a checked constructor.

See [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md).

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;       // literal coercion
let sum = m + m;              // OK: Meters + Meters -> Meters
// let bad = m + km;          // ERROR: cannot mix Meters and Kilometers
let raw: f64 = m as f64;      // explicit cast required

type Location = Point;
let loc: Location = { x: 0, y: 0 };  // literal coercion; or `Point { … } as Location`
loc.distance(&loc2);  // inherits Point methods, params expect &Location

impl Location {
    fn name(&self) -> String { ... }  // newtype-specific method
}
```

### Tuples and Arrays

See [WEP: Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md).

```wado
// Tuples
let t = [1, "hello", true];   // [i32, String, bool]
let x = t.0;                  // dot notation
let y = t[1];                 // bracket notation (constant index only)

// Lists (requires explicit type context)
let a: List<i32> = [1, 2, 3];           // type annotation
let b = [1, 2, 3] as List<i32>;         // explicit cast
fn takes(arr: List<i32>) {}
takes([1, 2, 3]);                        // coercion to List

// List methods
let mut arr: List<i32> = [];
arr.push(1);                             // add element to end
let n = arr.len();                       // get length
let empty = arr.is_empty();              // check if empty
let first = arr[0];                      // index access (read)
arr[0] = 100;                            // index assignment (write, requires mut)

let names: List<String> = ["a", "b"];
names.contains_str(view);                // membership by text, copying nothing

// In-place element mutation: iterate `&mut` to get `&mut T`
let mut ps: List<Point> = [Point { x: 1 }, Point { x: 2 }];
for let p of &mut ps { p.x += 10; }      // struct/List/String elements mutate in place
// For primitive/enum/variant elements (replace-on-assign), use index access:
for let mut i = 0; i < arr.len(); i += 1 { arr[i] = arr[i] * 2; }

// Sorting
let mut nums: List<i32> = [5, 3, 8, 1];
nums.sort();                               // in-place ascending sort (requires T: Ord)
let asc = nums.sorted();                   // returns new sorted array
nums.sort_by(|a, b| { ... });              // sort with custom Ordering comparator
```

### Strings

`String` is a prelude type with a literal syntax.

```wado
// Template string literals (interpolation)
let name = "Alice";
let greeting = `Hello, ${name}!`;         // "Hello, Alice!"

// Float-to-string: shortest roundtrip representation (no trailing .0)
let s = `${5.0}`;                         // "5"
let s = `${3.14}`;                        // "3.14"

// Format specifiers (see docs/wep-2026-01-17-template-format-specifiers.md)
// ${expr:[[fill]align][sign][#][0][width][.precision]type}
let formatted = `${3.14159:.2}`;          // "3.14"   precision = decimal places
let hex = `${255:x}`;                     // "ff"     b / o / x / X on integers
let hex_alt = `${255:#x}`;                // "0xff"   # adds the radix prefix
let sci = `${1200:e}`;                    // "1.2e3"  e / E on integers and floats
let padded = `${42:*>5}`;                 // "***42"  width counts characters
let signed = `${42:+}`;                   // "+42"
let zeroed = `${-42:08}`;                 // "-0000042" zeros go after the sign
let capped = `${"hello world":.5}`;       // "hello"  precision = max characters

// Inspect (:?) — auto-derived debug outputs (see docs/wep-2026-02-21-inspect-debug-output.md)
println(`${point:?}`);                    // "Point { x: 10, y: 20 }"
println(`${point:#?}`);                   // pretty-print with indentation (Inspect, alternate)
// `${point}` (Display) needs an `impl Display for Point`; use `${point:?}` for debug.

// String methods (mostly Rust compatible)
let n = s.len();                         // UTF8 byte length
let chars = s.chars().count();           // character count based on Unicode scalars

// Byte indices must sit on a character boundary, or substr_bytes/truncate panic.
// Round a byte budget down to one first; past len() it clamps to len().
let head = s.substr_bytes(0, s.floor_char_boundary(200));

// String building
let mut builder = String::with_capacity(20);
let part: String = "Hello";
builder.push_str(&part);
builder.push_str(", World!");

// Iterating over characters
for let c of "hello".chars() {
    println(`${c}`);
}
```

A `StrSlice` views part of a string without copying it. Its ends are always on
character boundaries, which is why it is a `struct` and not a newtype over
`ByteSlice`. `AsStrSlice` lets one signature take an owned `String`, a
reference to one, or a view of one — Wado's answer to Rust's `AsRef<str>`. See
[WEP: String Views](./wep-2026-09-13-string-slice.md).

```wado
let v = "banana".as_str_slice();
let part = v.slice(1, 4);        // "ana"; panics off a character boundary
part.len();                      // 3, in bytes
part.to_string();                // copies out, here and only here
for let c of part.chars() { ... }

part == "ana";                   // `==` and a match pattern both reach a literal
match part { "ana" => 1, _ => 0 };

fn byte_len<S: AsStrSlice>(s: S) -> i32 {
    return s.len();
}
byte_len("banana");              // a String, a &String, or a StrSlice
```

Tagged templates (see [WEP: Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md)): a path written directly before the backtick calls that function on the template's holes, each in its own type, with the literal text around them as constants.

```wado
fn sql<T: ReflectTemplate<Holes = [..V]>, ..V: ToParam>(t: T) -> Query {
    let mut text = "";
    let mut params: List<Param> = [];
    for let h of ReflectTemplate::<T>::members() {   // one unrolled step per hole
        text.push_str(h.lit());                      // literal before the hole (constant)
        text.push_str("?");
        params.push(h.get(&t).to_param());           // the hole's value, typed
    }
    text.push_str(ReflectTemplate::<T>::tail());     // literal after the last hole
    return Query { text, params };
}
let q = sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`;

// A hole handle also answers raw() / source() / has_spec() / index(), and
// fmt(&t, f) renders through the specifier as `${x:spec}` would.
format`x=${x:04}`          // == `x=${x:04}` — the untagged meaning, as a tag
String::raw`a\n${x}`       // escapes kept as written: "a\\n" + x
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

// Turbofish, where the fields do not settle the parameters
let q = Pair::<i64, String> { first: 0, second: "one" };

// Functional update: `..base` (leading, single) fills unlisted fields from a
// same-type value; listed fields override, base is evaluated once, unchanged.
let p2 = Point { ..p, x: 99 };  // y from p, x replaced

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
    println(`${x}, ${y}`);
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
let d: Color = Red; // bare only where the expected type supplies it; `let e = Red;` is an error

// Pattern matching
let name = match c {
    Red => "red",
    Green => "green",
    Blue => "blue",
};

// A case may also be written under a qualifier: the scrutinee's type
// (`Color::Red`), `Self` inside an `impl`, any name on the scrutinee's newtype
// chain, or a namespace the type is reachable through (`hue::Red`). The name
// must be visible at the pattern, and only the prelude is visible without a
// `use`, so a qualifier naming an unimported type is an error.
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
let ok_val: Result<i32, String> = Ok(42);                // bare: the annotation supplies the type
let err_val: Result<i32, String> = Result::Err("fail");

// Explicit turbofish (required when inference is insufficient), on the type or
// on the case as in Rust, but not on both. A payload-less case takes it too.
let opt = Option::<i32>::Some(42);
let res = Result::Ok::<i32, String>(42);
let none = Option::<i32>::None;

// Inside an impl, `Self::Case` names a case of the impl's own type, as in Rust.
impl<T> Maybe<T> {
    fn wrap(v: T) -> Maybe<T> { return Self::Just(v); }
}
```

`Option` and `Result` carry a deliberately small method set. `unwrap` and
`expect` take the value out, and `Result` adds `is_ok` / `is_err` /
`unwrap_err` / `expect_err`. These six transform it instead:

```wado
opt.unwrap_or(0)                  // the value, or the fallback
opt.map(|v: i32| v * 2)           // Option<U>; None passes through
opt.ok_or("missing")              // Result<T, E>; `?` cannot turn an Option into one

res.unwrap_or(0)                  // the Ok value, or the fallback
res.map(|v: i32| v * 2)           // Result<U, E>; an Err passes through
res.map_err(|e: String| e.len())  // Result<T, F>; an Ok passes through
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

let rw = Perms::Read | Perms::Write;   // bitwise OR
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

// == compares the values, as in Rust; ref_eq asks for one place
let a = 42;
&x == &a;             // true
ref_eq(r, &x);        // true: r points to x
ref_eq(&x, &a);       // false or true: two places, which may be stored once
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

// Comparison (can be chained: a < b < c → (a < b) & (b < c); no short-circuit)
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
    println(`Got: ${x}`);
}

// Mutable pattern bindings
if let Some(mut x) = opt {
    x += 10;
}

// Match ergonomics: &T scrutinees match against inner type
let ro = &opt;                // &Option<i32>
if let Some(x) = ro {         // x: &i32
    println(`Got: ${*x}`);
}

// While
while i < 10 { i += 1; }

// While let
while let Some(x) = iter.next() { println(`${x}`); }

// Let else — refutable binding; else must diverge, bindings escape on a match.
let Some(x) = opt else { return; };
println(`${x}`);                            // x in scope here

// C-style for
for let mut i = 0; i < 10; i += 1 {
    println(`${i}`);
}

// For-of (any IntoIterator type)
for let item of items {
    println(`${item}`);
}

// Range for-of
for let i of 0..<10 { println(`${i}`); }    // 0 to 9
for let i of 1..=10 { println(`${i}`); }    // 1 to 10
for let c of 'a'..='z' { print(`${c}`); }   // abcdefghijklmnopqrstuvwxyz

// Tuple for-of (compile-time expansion)
for let v of [42, "hello", true] {
    println(`${v}`);
}

// Infinite loop
loop {
    if done { break; }
    continue;
}

// A loop carries no label: `continue` takes none, and `break LABEL` names a
// labeled block. To leave a loop nest, see Labeled Blocks.

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
        `value is ${doubled}`
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

// Type patterns: `p: T` in any pattern position; a `let` annotation is one.
// Narrowing a resource to one that extends it asks the host, so it is
// refutable, and a match over type patterns ends in `_`.
let value = match node {
    input: HtmlInputElement => input.value(),
    _ => "",
};
let input: HtmlInputElement = el else { return; };

// Constant patterns: an immutable global or associated const matches by
// value, not a binding. TK_FOO/TK_BAR are `global`s, and a namespace prefix
// reaches one the same way (`tok::TK_FOO`).
let kind = match token {
    TK_FOO | TK_BAR => "keyword",
    i32::MAX        => "max",
    _               => "other",
};
```

Semicolons do not have particular semantics; they are just separators to statements. Convention in `wado format`: single-line block does not use semicolon.

```wado
let a = if cond() { 1 } else { 2 };   // either 1 or 2
let b = if cond() { 1; } else { 2; }; // ditto
```

### Labeled Blocks

A named block that `break LABEL` leaves from any depth inside it. Wado's only
non-local jump: it replaces loop labels, labeled `continue`, and `goto`, and it
is the block form that carries a value. See
[the spec](./spec-control-flow.md#labeled-blocks).

The label is required, since an unlabeled `{ ... }` is a struct literal. A
nested block may reuse a label, and `break` takes the innermost.
`break LABEL: ()` says what `break LABEL` says.

```wado
// Leave a whole loop nest with one break; the tail is the path no break took.
search: {
    for let r of 0..<grid.len() {
        for let c of 0..<grid[r].len() {
            if grid[r][c] == needle { hit = [r, c]; break search; }
        }
    }
    hit = [-1, -1];
}

// Guard chain: `break LABEL` skips the rest, so the guards stay flat.
attempt: {
    if !consume(&toks, &mut pos, TK_A) { break attempt; }
    if !consume(&toks, &mut pos, TK_B) { break attempt; }
    matched = pos;
}

// As an expression: `break LABEL: expr` and the trailing statement are both
// branches, must agree on one type, and coerce to the type expected at the
// use site. A tail that is not a value yields `()`.
let first_even = find: {
    for let x of xs {
        if x % 2 == 0 { break find: x; }
    }
    -1
};
```

### Branch Hints

`builtin::cold_path()` marks the path containing it as rarely executed, and
emits no code. The engine predicts the other side of the branch, and the inliner
leaves the cold path out of its cost estimate. It is a statement rather than a
condition wrapper, so it also works in a `match` or `if let` arm, where no
boolean is available. See [the spec](./spec-control-flow.md#branch-hints).

```wado
if i >= len {
    builtin::cold_path();
    panic("index out of bounds");
}

// On the fall-through after a diverging guard, it hints the guard as likely.
if let Some(v) = fast_path(key) {
    return v;
}
builtin::cold_path();
return slow_path(key);
```

## Assert

`assert` behaves like power-assert.

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
    println(`Hello, ${name}!`);
}

// Module public (accessible from other Wado modules)
pub fn api_function() -> i32 {
    return 42;
}

// Component export (public API at CM boundary)
export fn run() { ... }

// Default args, trailing only
fn connect(host: String, port: i32 = 8080) { ... }
connect("localhost");           // → connect("localhost", 8080)

// Type parameters take defaults too; Rust allows these on types only
fn info<T: Serialize = NoFields>(message: String, fields: T = NoFields {}) { ... }
info("started");                // → info::<NoFields>("started", NoFields {})
```

A function must have `return` if it returns a value. Default expressions must be effect-free; `export fn` and closures cannot have defaults. On a trait method both kinds of default belong to the trait: the `impl` restates the parameters without them, and the call site fills them from the declaration.

### Declared Absence

A name Wado deliberately does not offer is declared, not simply missing, so a
call to it reports why instead of "no method named". See
[WEP: Declared Absence](./wep-2026-09-13-declared-absence.md).

```wado
impl File {
    #[unavailable("write `open_with(Options::default())` instead")]
    pub fn open(&self);                     // a name that never existed

    #[unavailable("removed in 0.5.0; use `open_with`")]
    pub fn open_timeout(&self);             // a name that was taken out
}
```

The reason is required, and it is the attribute's only argument: a removal
writes its own version into the sentence. The declaration reserves a name rather
than a signature, so the parameters it lists are never checked against a call.
Write `self` to reserve the instance name and leave it out to reserve the static
one. It goes on a module function, an `impl` method, or a trait method.

### Local Items

`struct` and `type` (newtype) may be declared inside a function body, scoped to
the declaring block. See [WEP: Local Item Definitions](./wep-2026-07-09-local-item-definitions.md).

```wado
fn area(width: i32, height: i32) -> i32 {
    let s = Size { width, height };            // in scope before its
    struct Size { width: i32, height: i32 }    // declaration, unlike `let`
    return s.width * s.height;
}
```

Always private, and shadows a same-named module-level item.
Either may be generic. `enum`/`variant`/`flags` and a local `impl`/`trait` are
not yet supported.

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

// Capturing outer variables: auto-by-reference
let multiplier = 10;
let scale = |x: i32| x * multiplier;        // captures &multiplier; type fn(i32) -> i32

// Mutating capture: closure type is fn mut, binding must be mut
let mut count = 0;
let mut inc = || count += 1;                // captures &mut count; type fn mut() -> ()
let get = || count;                          // captures &count; type fn() -> i32
inc();
inc();
assert get() == 2;

// A parameter type comes from the expected fn type; annotate only where
// nothing supplies one (the `let`s above).
let f: fn(i32) -> i32 = |x| x + 1;
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
let arr = List::<i32>::with_capacity(10);  // turbofish for generic statics

// Variadic type packs: operate on tuples of any arity
fn variadic_identity<..T>(x: [..T]) -> [..T] {
    return x;
}
let t = variadic_identity([1, "hello", true]); // t: [i32, String, bool]

// Mixed scalar + pack parameters
fn prepend<A, ..T>(a: A, rest: [..T]) -> [A, ..T] {
    return [a, ..rest];  // value spread: splice rest into tuple
}

// More than one pack: each is settled by the argument carrying it alone, so a
// turbofish spells each as its own tuple. `[..A, ..B]` settles neither, so
// something else must — a sibling parameter, a turbofish, or an annotation.
fn concat<..A, ..B>(a: [..A], b: [..B]) -> [..A, ..B] {
    return [..a, ..b];
}
concat([1, "x"], [true]);                 // A = [i32, String], B = [bool]
concat::<[i32], [bool, String]>([1], [true, "x"]);

// A pack on either side of a scalar: nothing settles the ends, so spell them.
fn middle<..Pre, K, ..Post>(t: [..Pre, K, ..Post]) -> i32 { ... }
middle::<[i32], String, [bool]>([1, "mid", true]);   // 3
// middle([1, "mid", true]);              // ERROR: cannot infer `Pre`, `K`, `Post`

// Value spread (works with any tuple, not just packs)
let a = [1, "hello"];
let b = [..a, true];   // [i32, String, bool]

// Type pack expansion: call a static method on each type in the pack
fn make_defaults<..T: Default>() -> [..T] {
    return [..T::default()];   // expands to [T_0::default(), T_1::default(), ...]
}

// Tuple comprehension: one result element per source element; the braces hold
// one expression. `.enumerate()` binds the index, which doubles as a subscript.
impl<..T: Doubled> Doubled for [..T] {
    fn doubled(&self) -> [..T] {
        return [for let v of *self { v.doubled() }];
    }
}

// A pack is walked, never indexed by a literal: its arity and per-position
// types are only known once it expands. Only a scalar ahead of the pack has a
// fixed position.
fn head<A, ..T>(t: &[A, ..T]) -> A {
    return t.0;          // OK: ahead of the pack
    // return t.1;       // Error: lands on the pack
}

// That limit is on reading a value. In type position a pack takes a scalar on
// either side, and the match splits the tuple type across them. `Tag` here is
// any type carrying the list as a parameter.
fn drop_last<..Rest, Last>(t: &Tag<[..Rest, Last]>) -> Tag<[..Rest]> { ... }
fn drop_ends<First, ..Mid, Last>(t: &Tag<[First, ..Mid, Last]>) -> Tag<[..Mid]> { ... }

// This shortens a type, never a value. Over a bare tuple the same signature is
// declarable but not implementable: returning the argument is a type error, a
// comprehension keeps the arity it walked, and nothing else builds the shorter
// tuple.
// fn drop_last<..Rest, Last>(t: [..Rest, Last]) -> [..Rest]

// A pack bound through another parameter's associated type is projected from
// it, so the call site names neither.
fn arity<T: Parts<Items = [..P]>, ..P>(t: &T) -> i32 { ... }
arity(&tri);             // ..P comes from `Tri::Items`
```

## Visibility

Visibility has two orthogonal axes: a scope ladder (`internal` / `pub`) and a
CM-surface flag (`export`). `core:*` and `wasi:*` are each their own package.

| Keyword    | Axis    | Reach                                     |
| ---------- | ------- | ----------------------------------------- |
| (none)     | scope   | The defining file (private)               |
| `internal` | scope   | Other files in the same package           |
| `pub`      | scope   | Other Wado packages — the library API     |
| `export`   | CM flag | Also lowered at the CM boundary (`⟹ pub`) |

```wado
fn helper() { }               // file-private (default)
internal fn build_ast() { }   // package-internal
pub fn map() { }              // library API (Wado-native)
export fn run() { }           // library API + CM boundary
```

`internal` and `pub` are mutually exclusive; `export` already implies `pub`, so
`pub export` is just `export`. Importing a symbol that is not visible at the
import site (file-private, or `internal` from another package) is a compile
error. Struct fields take the same modifiers.

A `use` with a visibility modifier re-exports at that reach. It may narrow what
it names but never widen it, so `pub use { x }` requires a `pub` `x`. The facade
pattern is the narrowing one: the entry module publishes the API under its own
names, and consumers never name the files behind it.

```wado
pub fn compute() { }                        // the implementation file's API
pub use { compute } from "./impl.wado";     // published under this module's name
internal use { helper } from "./impl.wado"; // a `pub` helper, kept in the package
```

## Traits

```wado
trait Greet {
    fn greet(&self) -> String;
}

impl Greet for Person {
    fn greet(&self) -> String {
        return `Hello, ${self.name}!`;
    }
}

// Default methods
trait Summary {
    fn title(&self) -> String;

    fn summary(&self) -> String {
        return `Title: ${self.title()}`;
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

```wado
// For arithmetic operators; `a + b` is `T::Output`, so a generic body
// folding back into its own parameter writes `T: Add<Output = T>`
trait Add<Rhs = Self> { type Output; fn add(&self, rhs: &Rhs) -> Self::Output; }

// For == and != operators
trait Eq<Rhs = Self> { fn eq(&self, other: &Rhs) -> bool; }

// A total order: what `sort()`, `TreeMap` and a `T: Ord` bound read. On a
// float it is IEEE 754-2019 `totalOrder`, as C++20's `std::strong_order` is:
// -NaN < -Inf < -0 < +0 < +Inf < +NaN. The comparison operators keep IEEE's
// answers on every float, so `sort()` and `<` disagree about a NaN — see
// WEP: The Operator Order and the Total Order. Any other type reads `cmp`.
trait Ord: Eq { fn cmp(&self, other: &Self) -> Ordering; }

// For default value (implemented for primitives, String, List<T>,
// Option<T>, TreeMap<K, V>; not Result)
trait Default { fn default() -> Self; }

// For [] operators
trait IndexRef<I> { type Output: Ref; fn index_ref(&self, index: I) -> &Self::Output; }
trait IndexRefMut<I> { type Output: RefMut; fn index_ref_mut(&mut self, index: I) -> &mut Self::Output; }
trait IndexValue<I> { type Output; fn index_value(&self, index: I) -> Self::Output; }
trait IndexAssign<I> { type Output; fn index_assign(&mut self, index: I, value: Self::Output); }

// For string template interpolation
pub trait Display { fn fmt(&self, f: &mut Formatter); }         // stringify with specifiers

// What every error type is. It adds nothing to Display, so `E: Error` says
// only that the failure has a readable reason.
pub trait Error: Display { }

// For parsing a value from a string. The parameter takes a `StrSlice` among
// the rest, so parsing a field out of a larger buffer allocates no substring.
// `Err: Error`, so a caller reaching it through the bound can always report
// the reason.
pub trait FromStr {
    type Err: Error;
    fn from_str<S: AsStrSlice>(s: S) -> Result<Self, Self::Err>;
}

// Forgiving sibling of FromStr for human-supplied strings: accepts casing,
// radix prefixes (0x/0o/0b), `_` digit separators, and alternate bool words
// (1/0). Never trims whitespace. See WEP: Lenient String Parsing.
pub trait LenientFromStr {
    type Err: Error;  // built-in impls all use LenientParseError
    fn from_str_lenient<S: AsStrSlice>(s: S) -> Result<Self, Self::Err>;
}

// Value-to-value conversion. `Err: Error` for the same reason FromStr's is.
// The stdlib impls all use `ConvertError`.
pub trait From<T> { fn from(value: T) -> Self; }
pub trait TryFrom<T> {
    type Err: Error;
    fn try_from(value: T) -> Result<Self, Self::Err>;
}
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

// An impl declares its type parameters in `impl<...>`, as Rust does. A name
// the list does not hold is a type the module declares.
impl<T> List<T> { ... }
impl<K: Ord, V> TreeMap<K, V> { ... }
impl Display for List<i32> { ... }   // one instantiation declares none

// Bounded impl blocks — methods only available when bound is satisfied
impl<T: Ord> List<T> {
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

`Eq`, `Ord`, `Default`, `Serialize`, and `Deserialize` are _bound-driven_: they are derived where a use or bound needs them, no marker required. A plain struct is comparable and serializable with no declaration:

```wado
struct Point { x: i32, y: i32 }
Point { x: 1, y: 2 } == Point { x: 1, y: 2 };  // Eq derived here
to_string(&Point { x: 1, y: 2 });              // Serialize derived here
```

An empty marker `impl Trait for T;` asserts conformance: the compiler checks `T` is eligible and errors if not. Optional for these traits, but it documents intent and is the way to attach `#[wire(...)]` customization.

```wado
struct Broken { retries: i32 = 3, name: String }
impl Default for Broken;   // ERROR: `name` has no default expression
```

`From` takes the same marker. Nothing derives it from a use, so the marker is
what asks for one. On a variant, `impl From<T> for V;` wraps the value into the
single case whose payload is `T`.

```wado
variant ServiceError { Network(NetworkError), Timeout(TimeoutError) }
impl From<NetworkError> for ServiceError;   // -> ServiceError::Network(e)
```

`${x:?}` / `${x:#?}` (`Inspect`, plainly or indented) work for every type. `${x}` (`Display`) uses the type's `impl Display`: primitives, `String`, plain enums (bare case name, e.g. `Red`), and newtypes (inherited from the base) have one; other types need a hand-written impl, else `${x}` is a compile error and `${x:?}` gives the debug form. `${x:#}` runs the same `Display` with `Formatter.alternate` set.

A hand-written `impl Trait for T { … }` always wins. See [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

Every standard library error type implements `Display` and `Error`, so
`` `${e}` `` renders the reason. A wider error that carries a narrower one
interpolates it rather than wording the failure again. It declares
`impl From<Narrower> for Wider`, so `?` converts at the call site:

```wado
impl From<Utf8Error> for ParseError;

fn percent_decode(input: String) -> Result<String, ParseError> {
    let bytes = decode_octets(input)?;
    return Result::Ok(String::from_utf8(bytes)?);   // Utf8Error -> ParseError
}
```

## Associated Constants

```wado
impl f64 {
    pub const PI: f64 = 3.14159265358979323846;
}

let pi = f64::PI;
let max = i32::MAX;
```

Primitives provide built-in constants: `f64::PI`, `f64::INFINITY`, `f64::NAN`, `f64::MAX`, `f64::EPSILON`, `i32::MAX`, `i32::MIN`, etc. Every float type, `f16` and `bf16` included, carries Rust's limits (`MAX`, `MIN`, `MIN_POSITIVE`, `EPSILON`, `MANTISSA_DIGITS`, …). See [`core:prelude`](./stdlib-core-prelude.md).

## Primitive Type Methods

See [`core:prelude`](./stdlib-core-prelude.md) for the full API.

```wado
f64::sin(x)    f64::cos(x)    f64::sqrt(x)
f64::abs(x)    f64::ceil(x)   f64::floor(x)
f64::pow(x, y) f64::ln(x)     f64::exp(x)
f64::mul_add(x, y, z)

x.is_nan()     x.is_finite()    // where x is f64 or f32

f64::from_str("3.14")                 // Result<f64, ParseFloatError>
i32::from_str("42")                   // Result<i32, ParseIntError>
i32::from_str_hex("ff")               // Result<i32, ParseIntError> (radix 16)
i32::from_str_radix("1010", 2)        // Result<i32, ParseIntError> (radix 2..=36)
i32::from_str("xyz42abc".as_str_slice().slice(3, 5))  // no substring alloc

i32::min(a, b)  i32::max(a, b)
i32::clamp(v, lo, hi)                 // traps when lo > hi
i32::abs(x)                           // i32::MIN wraps to itself

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
// List iteration
let arr: List<i32> = [1, 2, 3, 4, 5];
for let x of arr { println(`${x}`); }

// Explicit iterator
let mut iter = arr.into_iter();
iter.next();                              // Option<i32>
let rest = iter.collect();                // List<i32> (default target)
let bytes: ByteList = s.bytes().collect(); // any FromIterator target, incl. a newtype over List

// Combinators: the closure's parameter types come from the call
let doubled = arr.into_iter().map(|x| x * 2).collect();            // [2, 4, 6, 8, 10]
let evens = arr.into_iter().filter(|x| x % 2 == 0).collect();      // [2, 4]
let acc = arr.into_iter().fold(0, |acc, x| acc + x);               // 15

// sum/product/min/max and the _by/_by_key variants work on any iterator
let sum = arr.into_iter().sum();                                   // Some(15)
let hi = arr.into_iter().map(|x| x * 2).max();                     // Some(10)

// Chaining
let result = arr.into_iter()
    .filter(|x| x > 2)
    .map(|x| x * 10)
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
for let i of 0..<5 { println(`${i}`); }    // 0, 1, 2, 3, 4
for let c of 'a'..='e' { print(`${c}`); }  // abcde
for let i of (0..<10).step_by(3) { ... }   // 0, 3, 6, 9 (any iterator takes step_by)
```

## Effects

See [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md).

```wado
fn write_file(path: String, data: String) with FileSystem { ... }
fn main() with (Stdout, FileSystem) { ... }        // more than one → parentheses
fn add(a: i32, b: i32) -> i32 { return a + b; }  // no effects = pure

// Same rule in every position, so a comma after a bare effect is the list's.
fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T { ... }   // two parameters
fn both(f: fn() with (Stdout, Stderr), x: i32) { ... }           // two parameters

// A trait method's `with` clause bounds every impl of it. A call requires what
// the trait declares, since through a bound there is no impl to read.
trait Source { fn next(&mut self) -> i32 with Stdout; }
impl Source for Loud {
    fn next(&mut self) -> i32 with Stdout { ... }   // matching; more is an error
}
fn draw<S: Source>(s: &mut S) -> i32 with Stdout { return s.next(); }

// A `with` clause on the trait itself bounds the methods that declare none:
// `with ()` forbids every effect, `with Stdout` hands that one to each impl,
// and `with _` lets each impl bring its own.
trait Tick with _ { fn tick(&mut self) -> i32; }
impl Tick for Loud { fn tick(&mut self) -> i32 with Stdout { ... } }
impl Tick for Quiet { fn tick(&mut self) -> i32 { ... } }
fn run<T: Tick>(t: &mut T) -> i32 with _ { return t.tick(); }
// `run(&mut loud)` requires Stdout at the call; `run(&mut quiet)` requires none

// A head that writes nothing reads as `with _`, and is diagnosed (a `pub`
// trait warns, a private one remarks). Waive it while deciding:
#[allow(undecided_effects)]
trait Undecided { fn tick(&mut self) -> i32; }

// Every standard library trait says `with ()`, Iterator and FromStr included:
// an impl of one that performs I/O is a design error.

// Effect in function type position
fn for_each(items: List<i32>, f: fn(i32) with Stdout) with Stdout {
    for let item of items { f(item); }
}

// Generic effects — polymorphic over effects (one effect param per function)
fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E {
    return f(x);
}

// `with _` is sugar for it: every `_` in one signature is the same parameter,
// so `wrapper` above is `fn wrapper(f: fn() with _) with _`.

// E is inferred from the closure's effects at each call site
wrapper(|| { println("hello"); });     // E = Stdout
let x = apply(|n| n + 1, 41);          // E = (none)
let y = apply(|n| {                    // E = Stdout
    println(`${n}`);
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

`resume value` (only valid inside a handler) hands `value` back to the caller of the operation.

An `interface` is a trait with a different dispatch story, so its members are written as a trait's are — and an operation with a body declares its default implementation: what it does when dispatched with no handler installed, and what fills a handler that leaves the operation out. Without one, an unhandled operation traps. A parameter may take a default, filled in at the call site. Beyond a name, parameters and a return type an operation declares nothing else (no receiver, effects or type parameters); see [the spec](./spec-effects.md#default-implementations).

```wado
interface Log {
    fn emit(message: String) {
        log_stderr(message);   // no handler installed: degrade, don't trap
    }

    fn level() -> i32;         // no default: unhandled dispatch traps
}
```

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
use { Request, Response, ErrorCode, Headers, Trailers } from "wasi:http";

export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_rx, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Headers::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_rx);

    // task return: delivers result without ending the function
    task return Result::<Response, ErrorCode>::Ok(response);

    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
}
```

`task return expr;` delivers the function's result to the CM runtime without terminating the function. Valid only inside `export async fn`, and required there: a body carrying none can never deliver, so the compiler rejects it. One under a branch is fine; a path that misses it traps. Regular `return` is forbidden in `async fn` bodies. A Wado call of such a function gets the delivered value back as an ordinary return value, so a test can assert on it.

## Test Blocks

Test blocks compile to the `test` world. Files with test blocks are discovered and executed by `wado test`:

```sh
wado test                            # walk the project for every *.wado file
wado test file.wado                  # run a specific file
wado test --filter '*pattern*'       # keep files whose path matches the wildcard
wado test --test-name 'addition'     # run only test blocks whose name contains "addition"
wado test file.wado --test-name add  # narrow both: this file, those test names
wado compile --world test file.wado  # compile a single file with the test world
```

Discovery walks the project root for every `*.wado` file, honouring
`.gitignore`, `.gitmodules`, dot-prefixed entries, and nested `wado.toml`
boundaries (each sub-package is run in its own context). Add
`[test].exclude = ["..."]` to `wado.toml` to skip extra paths. Files without
`test` blocks are still parsed and compiled.

`--filter <pattern>` is a path-based shell wildcard (`*`, `?`, `[...]`); it
is _not_ a regex. To match anywhere in a path, wrap the term in `*`s, e.g.
`'*foo*'`. The runner exits non-zero on any compile failure, test failure,
or `#[TODO]` test that resolved unexpectedly.

`--test-name <pattern>` selects individual `test "name"` blocks the way
`cargo test <name>` does: a case-sensitive substring match against the test's
original name (matched against the source name, so multibyte names work).
It is repeatable and combines with OR — a test runs if its name contains any
pattern.

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

// Timeout override. The default is 5000ms, and a test that runs longer is
// interrupted and fails.
#[timeout_ms(30000)]
test "large data processing" {
    process_large_dataset();
}

// Synopsis test: runs like any test; `wado doc` renders its body as the
// module's `## Synopsis` section.
#[synopsis]
test {
    struct Point { x: i32, y: i32 }

    let p = Point { x: 3, y: 4 };
    assert p.length() == 5.0;
}
```

## Compile-Time Literals

```wado
let file = #file;           // current source file path (String)
let line = #line;           // current line number (i32)
let func = #function;       // current function name (String)
let data = #data;           // __DATA__ section content (String)

let src = #include_str("./runtime.wado");  // include file as String
let icon = #include_bytes("./icon.png");   // include file as ByteList
```

A literal read as numbers becomes a constant, with no decode loop at startup.
See [the spec](./spec-control-flow.md#embedded-data).

```wado
let w = List::<f32>::from_le_bytes(#include_bytes("./w.bin"));  // little-endian f32s
```

Paths in `#include_str` and `#include_bytes` are resolved relative to the source file. See [WEP: Compile-Time File Inclusion](./wep-2026-03-02-include-str.md).

## Compile-Time Parameters

`#[param]` on a `global` makes it a build input fed by the `wado` invocation: the type annotation gives the type, the initializer is the fallback, and read sites are ordinary global references.

See [WEP: Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md).

```wado
#[param]
global API_URL: String = "http://localhost";   // -D API_URL=...

#[param(from_env = "PORT")]
global PORT: i32 = 8080;                       // read from an env var

#[param(name = "build.id")]
global BUILD_ID: String = "dev";               // -D build.id=...
```

```sh
wado compile -D API_URL=https://prod.example.com -D PORT=80 app.wado
```

## Standard Library

`core:*` and `wasi:*` modules. Full API in each linked module doc.

### core:prelude

Auto-imported (disable with `#![no_prelude]`). Home of `String`, `List<T>`,
`Option<T>`, `Result<T, E>`, `RangeExclusive`/`RangeInclusive`, the primitive
type methods, and the prelude traits. See [`core:prelude`](./stdlib-core-prelude.md).

```wado
panic("error message");   // log to stderr and trap
unreachable();            // trap on unreachable code

// Comparing a secret (a MAC, a token, an API key): `==` stops at the first
// differing byte, and how long it took says how much of a guess was right.
eq_constant_time(&mac, &expected);   // any AsByteSlice: ByteList, String, …
```

### core:cli

`println` / `eprintln` / `print` / `eprint`, `args`, `program_name`, `env`, `cwd`,
`exit`; `log_stdout` / `log_stderr` print with no effect. See
[`core:cli`](./stdlib-core-cli.md).

```wado
use { println, eprintln, print, eprint, Stdout, Stderr } from "core:cli";
use { args, program_name, env } from "core:cli";

println("hello");
for let arg of args() { println(`arg: ${arg}`); }   // what follows the program name
program_name();   // Some("app.wado") under `wado run app.wado`, never the runner
if let Some(home) = env("HOME") { println(`HOME=${home}`); }
```

### core:fs

Whole-file I/O against the first preopened directory (`wado run` grants the
current one), and the path text that reaches it. `""` and `"."` name that
directory. Every call that reaches the filesystem resolves its path when it
runs, so ask-then-act (`exists` and then `read`) races; act and read the
error. The path functions resolve nothing. See
[`core:fs`](./stdlib-core-fs.md) and
[WEP: core:fs](./wep-2026-09-12-core-fs.md).

```wado
use fs from "core:fs";
use { Preopens } from "core:fs";                 // the effect, re-exported: no wasi import

let text = fs::read_to_string("docs/spec-overview.md")?;  // Result<String, FsError>
let bytes = fs::read("icon.png")?;               // Result<ByteList, FsError>
fs::write("build/out.json", &text)?;             // any AsByteSlice; replaces via rename
                                                 // (a non-regular file is refused)
fs::write_in_place("big.bin", &bytes)?;          // truncates instead of replacing
fs::rename("build/a.txt", "build/b.txt")?;       // replaces what b.txt named
fs::create_dir_all("build/reports")?;            // mkdir -p
fs::create_dir("build/reports/today")?;          // one level; parent must exist
fs::remove_file("build/stale.txt")?;
fs::remove_dir("build/empty")?;                  // the directory must be empty
fs::remove_dir_all("build/site")?;               // rm -rf; a missing path is Ok
                                                 // (a path resolving to the preopen is refused)

if fs::exists("wado.toml") { ... }               // no error to discard
fs::try_exists("wado.toml")?;                    // absence only; anything else is the error
let meta = fs::metadata("icon.png")?;            // Metadata { type, size, modified }

for let entry of fs::read_dir("src")? {          // DirEntry { name, type }
    if entry.type matches { Directory } { continue; }
}
for let found of fs::walk_dir("src")? {          // WalkEntry { path, type }, deep
    println(`${found.path}`);
}
// `e.path` reaches the entry from the walk's root, so a name is compared as one
fs::walk_dir(".", |e| fs::file_name(&e.path).unwrap() != ".git")?;

// Path text, no I/O: `/`-separated and preopen-relative, never URL rules
fs::join("build", "out.json");                   // "build/out.json"
fs::parent("a/b/c");                             // Some("a/b")
fs::file_name("a/b.txt");                        // Some("b.txt"); None on `.` or `..`
fs::file_stem("a/b.txt");                        // Some("b")
fs::extension("a/b.txt");                        // Some("txt")
fs::normalize("a/./b/../c")?;                    // "a/c"; ".." past the preopen fails

if let Err(e) = fs::read_to_string("missing.txt") {
    eprintln(`error: ${e}`);            // "missing.txt: no such file or directory"
}

let dir = fs::root()?;          // the Descriptor, for anything wasi:filesystem does
```

Streaming a file (rather than holding it) stays on `wasi:filesystem`; see
`example/cat.wado`.

### core:collections

`TreeMap<K, V>` and `TreeSet<T>`, iterating in insertion order. See
[`core:collections`](./stdlib-core-collections.md).

`TreeMap<K, V>` is also the Wado spelling of the Component Model `map<K, V>`, so
it crosses a component boundary. There `K` must be `bool`, `char`, `String`, or
an integer. A repeated key on the wire takes the last pair's value.

```wado
use { TreeMap, TreeSet } from "core:collections";

let mut map = TreeMap::<String, i32>::new();
map["key"] = 42;              // index assignment
let v = map["key"];           // index access (panics if absent)
let opt = map.get("key");     // fallible access -> Option<V>
map.get_str(view);            // String-keyed map: look up by a view, no key copy
map.contains_key_str(view);   // likewise -> bool
map.get_ref_str(view);        // likewise -> Option<&V>, when copying V would be waste
map.remove("key");            // -> bool
map.try_insert("k", 1);       // insert if absent -> bool
map.get_or_insert("k", 1);    // the stored value, or the inserted one
for let [k, v] of map.entries() { println(`${k}=${v}`); }

let sizes = { small: 1, large: 3 } as TreeMap<String, i32>;
let set = ["foo", "bar", "baz"] as TreeSet<String>;
set.contains("foo");          // -> bool; set.insert(x) -> bool
```

### core:serde

Format-agnostic `Serialize` / `Deserialize` framework.
A plain struct derives with no marker; `impl Serialize for T;` attaches
`#[wire(...)]` customization. Wire keys default to the field name; override
with `#[wire(name_policy = "...")]` (per type) or `#[wire(name = "...")]`
(per field). See [`core:serde`](./stdlib-core-serde.md) and
[WEP: Serde](./wep-2026-02-28-serde.md).

```wado
struct Point { x: i32, y: i32 }         // serializable, no marker needed

// name_policy: camelCase / snake_case / PascalCase / SCREAMING_SNAKE_CASE /
//             kebab-case / SCREAMING-KEBAB-CASE
#[wire(name_policy = "camelCase")]
struct Event {
    created_at: String,                 // wire key: "createdAt"
    #[wire(name = "type")]
    event_type: String,                 // wire key: "type"
}

struct Config {
    host: String,                       // required — error if missing
    port: i32 = 8080,                   // missing -> 8080
}
impl Deserialize for Config;
```

### core:json

JSON serialization and deserialization. See [`core:json`](./stdlib-core-json.md).

```wado
use { to_string, to_bytes, from_string, from_bytes, to_bytes_pretty, to_bytes_canonical  } from "core:json";

let bytes = to_bytes(&p);                          // UTF-8 bytes
let pretty = to_bytes_pretty(&p);                  // indented
let canon = to_bytes_canonical(&p);                // sorted keys, deterministic

let json = to_string(&Point { x: 1, y: 2 });       // Ok("{\"x\":1,\"y\":2}")
let p = from_string::<Point>("{\"x\":1,\"y\":2}"); // Ok(Point { x: 1, y: 2 })
```

### core:cbor

CBOR (RFC 8949), same serde model as JSON — any JSON-serializable type works
unchanged. See [`core:cbor`](./stdlib-core-cbor.md).

```wado
use { to_bytes, from_bytes, to_bytes_canonical } from "core:cbor";

let enc = to_bytes(&Point { x: 1, y: 2 });   // preferred (shortest) encoding
let dec = from_bytes::<Point>(enc);          // variation-tolerant decode
let sig = to_bytes_canonical(&p);            // deterministic, for COSE/CWT
```

### Other core modules

- [`core:json_nsd`](./stdlib-core-json_nsd.md) — non-self-describing JSON
- [`core:args`](./stdlib-core-args.md) — command-line argument parsing via serde
- [`core:value`](./stdlib-core-value.md) — dynamic, format-agnostic value
- [`core:base64`](./stdlib-core-base64.md) — base64 encoding and decoding
- [`core:digest`](./stdlib-core-digest.md) — cryptographic hashes and HMAC (SHA-256)
- [`core:jwt`](./stdlib-core-jwt.md) — JSON Web Tokens over JWS Compact (`HS256`)
- [`core:zlib`](./stdlib-core-zlib.md) — zlib/gzip compression
- [`core:simd`](./stdlib-core-simd.md) — Wasm 128-bit SIMD, incl. Relaxed SIMD
- [`core:url`](./stdlib-core-url.md) — WHATWG URL parsing
- [`core:prng`](./stdlib-core-prng.md) — seedable, reproducible pseudo-randomness
  for simulation: `Rng`, `VectorRng`, and the keyed `Squares64`
- [`core:secure_random`](./stdlib-core-secure_random.md) — unpredictable
  randomness from `wasi:random`: `bytes`, the `token_*` generators,
  `with_buffered`, and `seed()` for `core:prng`
- [`core:uuid`](./stdlib-core-uuid.md) — UUID v4 / v7
- [`core:temporal`](./stdlib-core-temporal.md) — date/time on the TC39 Temporal
  model (`Instant`, `ZonedDateTime`, `Duration`, `Plain*`)
- [`core:log`](./stdlib-core-log.md) — structured logging and tracing (levels, fields, spans, sinks)
- [`core:router`](./stdlib-core-router.md) — HTTP path router
- [`core:icu`](./stdlib-core-icu.md) — Unicode character properties, as code
  point ranges or per character
- [`core:kiln`](./stdlib-core-kiln.md) — Kiln IDL host bindings
- [`core:benchmark`](./stdlib-core-benchmark.md) — benchmark timing/throughput utilities

### WASI

[WASI Standard Library Reference](./stdlib-wasi.md): `wasi:cli`, `wasi:random`,
`wasi:clocks`, `wasi:http`, `wasi:filesystem`, `wasi:sockets`, `wasi:tls`.

[`wasi:webgpu`](./stdlib-wasi-webgpu.md) has a reference of its own: GPU compute
and offscreen rendering, and a host of its own in `wado run-webgpu`.

## See Also

- [Language Specification](./spec-overview.md) - Full language specification
