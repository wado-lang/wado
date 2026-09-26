# Literals

## Primitive Literals

### Boolean Literals

```wado
let active = true;
let disabled = false;
```

### Null Literal

The `null` keyword is equivalent to `None` and represents the absence of a value:

```wado
let missing: Option<i32> = null;            // Same as None
let also_missing = Option::<i32>::None;

// Both are equivalent
assert missing == also_missing;
```

Note: `null` is a language keyword, while `None` is a case of the prelude's `Option`. Bare `None` needs an expected type to say which `Option` it belongs to.

A `null` takes the `Option` its context expects. With no context it has the type `Option<!>`, the `Option` with no `Some`. A `null` is never any type but an `Option`, so `let x: i32 = null` is a type error.

`Option<!>` is a value of every `Option<T>`. Like an integer literal, a `null` answers a type parameter last, so a sibling argument decides which `Option` it is. A `let` settles its type at once, as it does an integer literal's:

```wado
fn pair<T>(a: T, b: T) -> T { return a; }

let p = pair(null, Option::Some(1));    // Option<i32>
let x = null;                           // Option<!>
// x = Option::Some(1);                 // Error: expected Option<!>
// if let Some(v) = x { }               // Error: unreachable, no Option<!> is a Some
```

`Option<!>` is also what a type converts from to accept `null` where an `Option` is not expected, which is how `core:value::Value` takes JSON's `null` in a literal:

```wado
impl From<Option<!>> for Value {
    fn from(value: Option<!>) -> Value {
        return Value::Null;
    }
}

let doc: Value = { name: "Alice", nickname: null };
```

### Character Literals

Character literals use single quotes and represent a Unicode scalar value. `char` is a distinct type with Unicode semantics, not an integer, just as `String` is not `List<u8>`:

```wado
let letter = 'A';
let digit = '9';
let unicode = '\u0041';  // Unicode escape (same as 'A')
let emoji = '😀';        // Direct Unicode character
let newline = '\n';
```

See [Escape Sequences](#escape-sequences) for the supported escapes.

```wado
let a = '\u0041';         // 'A' (BMP)
let smiley = '\u{1F600}'; // '😀' (non-BMP)
```

#### char Casting and Conversion

`char` can be cast to any integer type to extract the Unicode scalar value (possibly truncated for smaller types):

```wado
let c = 'A';
let code = c as i32;    // 65
let ucode = c as u32;   // 65
let byte = c as u8;     // 65 (truncated to low byte)
```

`u8 as char` is allowed because all `u8` values (0..255) are valid Unicode scalar values:

```wado
let byte: u8 = 65;
let c = byte as char;  // 'A'
```

All other integer-to-char casts are prohibited because not all values are valid Unicode scalar values (surrogates `0xD800..0xDFFF` and values `> 0x10FFFF` are invalid):

```wado
let x: i32 = 65;
let c = x as char;  // compile error

let y: i8 = 65;
let c = y as char;  // compile error (i8 can be negative)
```

Use checked conversion functions instead:

```wado
let c = char::from_u32(65 as u32);  // Option<char>: Some('A')
let c = char::from_i32(65);         // Option<char>: Some('A')
```

See [`core:prelude`](./stdlib-core-prelude.md) for the full `char` API.

Casting `char` to non-integer types is a compile error:

```wado
let c = 'A';
let f = c as f64;     // compile error: char can only be cast to integer types
let s = c as String;  // compile error: the two types share no representation
```

### Integer Literals

```wado
let decimal = 42;
let negative = -17;
let with_separator = 1_000_000;    // Underscores for readability
let binary = 0b1010_1100;          // Binary
let octal = 0o755;                 // Octal
let hex = 0xFF_AA_BB;              // Hexadecimal
```

#### Type coercion

When the target type is known from context (type annotation or function argument), integer literals coerce to any compatible integer type, including `i128`/`u128`:

```wado
let byte: i8 = 127;
let long: i64 = 9_223_372_036_854_775_807;
let unsigned: u32 = 4_294_967_295;
let big: u128 = 1_000_000_000_000;
fn foo(n: i64) { ... }
foo(100);  // literal coerced to i64
```

#### Compile-time range checking

The compiler rejects literal coercions whose value falls outside the target type's range. All literal bases (decimal, hex `0x`, octal `0o`, binary `0b`) use strict numeric range: the value must lie within `[MIN, MAX]` for signed types or `[0, MAX]` for unsigned types.

To reinterpret a bit pattern as a signed integer, use an explicit `as` cast.

```wado
let a: i8 = 127;                  // OK: max i8
let b: i8 = 128;                  // compile error: literal out of range for `i8`: 128
let c: i8 = -128;                 // OK: min i8
let d: u32 = -1;                  // compile error: literal out of range for `u32`: -1

let e: i8 = 0xFF;                 // compile error: literal out of range for `i8`: 0xFF
let f: i8 = 0xFF as i8;           // OK: explicit bit-pattern reinterpretation (value: -1)
let g: i32 = 0xFFFF_FFFF;         // compile error: literal out of range for `i32`: 0xFFFF_FFFF
let h: i32 = 0xFFFF_FFFF as i32;  // OK: explicit bit-pattern reinterpretation (value: -1)
let i: u32 = 0x1_0000_0000;       // compile error: literal out of range for `u32`: 0x1_0000_0000
```

A float has no bit pattern for `as` to reinterpret. An integer literal cast to
`f32` or `f64` converts by value, as the annotated form does, and the same
range check applies.

```wado
let m = 340282350000000000000000000000000000000 as f32;  // OK: f32::MAX
let n = 400000000000000000000000000000000000000 as f32;  // compile error: literal out of range for `f32`
```

A literal that nothing coerces falls back to `i32`, and the same range check applies there. `-NUM` is checked as one literal, so it reaches the signed minimum.

```wado
let j = 2147483647;               // OK: max i32
let k = 4294967296;               // compile error: literal out of range for `i32`: 4294967296
let l = -2147483648;              // OK: min i32
let m = -2147483649;              // compile error: literal out of range for `i32`: -2147483649
```

Type conversion (via `as`):

```wado
let byte: i8 = 127 as i8;
let long: i64 = 9_223_372_036_854_775_807 as i64;
let unsigned: u32 = 4_294_967_295 as u32;
```

### Floating-Point Literals

```wado
let pi = 3.14159;
let with_separator = 1_000_000.5;
let scientific = 6.022e23;         // 6.022 × 10²³
let negative_exp = 1.6e-19;        // 1.6 × 10⁻¹⁹
let explicit_positive = 2.5e+10;
```

#### Type coercion

Floating-point literals coerce to `f32`, `f64`, `f16` or `bf16` when the target type is known:

```wado
let single: f32 = 3.14;
let double: f64 = 3.14159265358979;
let half: f16 = 0.5;
let weights: List<bf16> = [0.5, -1.25, 3.0];
```

A literal is rounded once, from its decimal text to the nearest value of its
type, ties to even. One that rounds past the type's largest finite value is a
compile error, as an integer literal past its type's range is:

```wado
let x: f16 = 65520.0;             // compile error: literal out of range for `f16`: 65520.0
let y: f32 = 1e39;                // compile error: literal out of range for `f32`: 1e39
```

Type conversion (via `as`):

```wado
let single: f32 = 3.14 as f32;
let double: f64 = 3.14159265358979 as f64;
```

### String Literals

String literals create `String` values.

Regular strings use double quotes:

```wado
let name = "Alice";           // Type: String
let path = "path/to/file.txt";
let escaped = "Line 1\nLine 2\tTabbed";
```

Byte strings use a `b` prefix and create a constant `ByteList` (the
first-class byte-buffer newtype over `List<u8>`):

```wado
let magic = b"\x89PNG\r\n";             // Type: ByteList, value [137, 80, 78, 71, 13, 10]
let raw: List<u8> = b"\x89PNG\r\n";     // Also OK: newtype literal coercion to the base
```

The content must be ASCII; each `\xNN` escape (two hex digits) or source
character contributes one byte, and the standard escapes (`\n`, `\t`, `\\`,
`\"`, `\0`, `\r`, `\'`) are also accepted. Unicode escapes (`\u{...}` /
`\uHHHH`) are rejected — a Unicode escape denotes a scalar, not a byte; use
`\xNN` for a raw byte. (`#include_bytes("path")` produces the same `ByteList` from a file.)
The default type is `ByteList`, but newtype literal coercion lets it flow into a
`List<u8>` context (or any type whose base is `List<u8>`) with no cast.

Byte literals are the single-byte analog: `b'x'` is one `u8`.

```wado
let a = b'A';              // u8, 65
let hi = b'\xff';          // u8, 255
let n: i32 = b'A';         // 65 — coerces like an integer literal
```

A byte literal is an integer literal defaulting to `u8`, so it coerces like any
integer literal (its value is always `0..=255`). Its content follows the same
one-byte rule as a byte string (`\xNN` for `0x80..=0xFF`; no `\u`); use a char
literal `'…'` for a Unicode scalar.

### Escape Sequences

Escape sequences are shared between character, string and template literals,
except where the table names one:

| Escape   | Character                   |
| -------- | --------------------------- |
| `\'`     | Single quote                |
| `\"`     | Double quote                |
| `` \` `` | Backtick (template only)    |
| `\\`     | Backslash                   |
| `\/`     | Forward slash               |
| `\b`     | Backspace                   |
| `\f`     | Form feed                   |
| `\n`     | Newline                     |
| `\r`     | Carriage return             |
| `\t`     | Tab                         |
| `\0`     | Null                        |
| `\$`     | Dollar sign (template only) |
| `\{`     | Left brace (template only)  |
| `\}`     | Right brace (template only) |
| `\uHHHH` | Unicode BMP (4 hex digits)  |
| `\u{H+}` | Unicode full range          |

In a template string only `${` opens an interpolation, so `{` and `}` are
literal and need no escaping, though `\{` and `\}` are accepted. Use `\$` to
write a literal `$` before a `{` (e.g. `` `\${x}` `` renders the text `${x}`).

For characters outside BMP (U+10000 and above), use either:

```wado
"\uD83D\uDE00"   // Surrogate pair
"\u{1F600}"      // Variable-length escape
"😀"             // Direct Unicode character
```

Template strings (interpolation) use backticks. Interpolation is introduced
with `${expr}` (ES/TypeScript-style); a bare `{` or `}` is literal text, so
JSON-like content needs no escaping:

```wado
let name = "Alice";
let greeting = `Hello, ${name}!`;  // "Hello, Alice!"

let count = 42;
let message = `Count: ${count}`;   // "Count: 42"

// Format specifiers
let pi = 3.14159;
let formatted = `Pi: ${pi:.2}`;   // "Pi: 3.14"
let hex = `${255:x}`;             // "ff"
let sci = `${1200:e}`;            // "1.2e3" (integers as well as floats)
let padded = `${"あい":>6}`;       // "    あい" (width counts characters)

// Inspect (debug) format — works for any type
let p = Point { x: 10, y: 20 };
let debug = `${p:?}`;            // "Point { x: 10, y: 20 }"
let pretty = `${p:#?}`;          // pretty-print: "Point {\n  x: 10,\n  y: 20,\n}"
// `${p}` (Display) needs an `impl Display` for `Point`; use `${p:?}` for debug output.

// Braces are literal — JSON embeds cleanly without escaping
let json = `{"key": "${name}"}`;  // {"key": "Alice"}
```

See [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md) for the full specifier table, [WEP: Format Traits](./wep-2026-02-01-format-traits.md) for the trait/Formatter infrastructure, and [WEP: Inspect](./wep-2026-02-21-inspect-debug-output.md) for the `:?` debug output format.

Multiline strings are supported in both regular and template strings. Literal newlines are preserved:

```wado
// Regular multiline string
let poem = "Roses are red,
Violets are blue,
Wado is great,
And so are you!";

// Multiline template string
let name = "Alice";
let message = `Dear ${name},

Welcome to Wado!

Best regards`;
```

### Tuple Literals

Bracket syntax `[...]` creates tuple values by default. This aligns with TypeScript conventions and JSON interoperability.

```wado
let pair = [1, "hello"];              // Type: [i32, String]
let triple = [42, "answer", true];    // Type: [i32, String, bool]
let single = [42];                    // Type: [i32] (1-tuple)
let empty_tuple: [] = [];             // Empty tuple (distinct from unit ())
let trailing = [1, 2, 3,];            // Trailing comma allowed
```

#### Tuple Types

Tuple types use bracket syntax `[T1, T2, ...]`.

```wado
let point: [i32, i32] = [10, 20];
let record: [String, i32, bool] = ["Alice", 30, true];
```

#### Tuple Element Access

Tuple elements are accessed by constant index using dot notation or bracket notation:

```wado
let t = [10, "hello", true];
let x = t.0;      // 10 - dot notation
let y = t[1];     // "hello" - bracket notation
let z = t.2;      // true

// Variable index is not allowed (compile error)
let i = 1;
let w = t[i];     // Error: tuple index must be a constant integer
```

#### Unit vs Empty Tuple

The unit type `()` and empty tuple `[]` are distinct:

```wado
let unit: () = ();    // Unit type/value
let empty: [] = [];   // Empty tuple (rarely used)
```

They take separate `impl`s, so a method defined on one is not found on the other.

`[]` cannot cross a component boundary. It has no Component Model representation:
a `tuple` carries at least one type, and `()` is the type that carries none. An
export naming one is rejected at compile time.

#### The `never` type (`!`) — bottom type

`never` is the bottom type: it is a subtype of every type. An expression of type `never` never returns — it always diverges (traps). `panic()` and `unreachable()` both return `!`.

Because `never` is assignable to any type, a `never`-typed expression may appear in any value position without a type mismatch:

```wado
// In a match arm — the None branch panics, so the match has type i32
let opt: Option<i32> = Option::<i32>::Some(5);
let x = match opt {
    Some(v) => v,
    None => panic("unexpected none"),
};

// In a let binding with explicit type annotation
let y: i32 = panic("unreachable");

// In a binary expression — execution diverges before the addition
let z: i32 = panic("boom") + 1;
```

The `!` type can be written explicitly as a return type:

```wado
fn fail(msg: String) -> ! {
    panic(msg);
}
```

### List Literals

A bracket literal becomes a `List` through an explicit `as`, or through implicit coercion where the target type is known.

```wado
// Explicit conversion with `as`
let numbers = [1, 2, 3, 4, 5] as List<i32>;

// Implicit coercion (target type known)
fn takes_list(a: List<i32>) { ... }
takes_list([1, 2, 3]);  // OK - compiler knows List<i32> is expected

// Type annotation
let explicit: List<i32> = [1, 2, 3];  // Coerced to List
```

#### Coercion Rules

- Where the target type is known (a function parameter, a type annotation), the literal coerces implicitly
- Elsewhere it is a tuple, and `as List<T>` converts it

```wado
let t = [1, 2, 3];               // Tuple [i32, i32, i32] - no context
let a = [1, 2, 3] as List<i32>; // List - explicit conversion

fn process(data: List<i32>) { ... }
process([1, 2, 3]);              // OK - implicit coercion
```

#### Design Rationale

This design aligns with TypeScript (primary target audience) and enables intuitive JSON interoperability. JSON arrays are heterogeneous and map naturally to tuples:

```json
{ "point": [10, 20], "mixed": [1, "hello", true] }
```

```wado
// A JSON array maps naturally to a tuple:
let point: [i32, i32] = [10, 20];
let mixed: [i32, String, bool] = [1, "hello", true];
```

See `docs/wep-2026-01-15-tuple-and-array-literals.md` for detailed rationale.

#### List Constructors

```wado
let arr = List::<i32>::with_capacity(10);     // empty list with room for 10 elements
let bools = List::<bool>::filled(100, true);  // list of 100 elements, all true
```

#### List Operations

```wado
let mut arr: List<i32> = [1, 2, 3];

// Index access (read)
let first = arr[0];  // 1

// Index assignment (write)
arr[0] = 100;        // Requires a `let mut` binding
arr[1] = 200;

// List methods
arr.push(4);         // Add element to end
let len = arr.len(); // Get length
```

#### Index Assignment Rules

- Requires the list binding to be declared with `let mut`
- Index must be within bounds (runtime check, traps if out of bounds)
- Works with lists of any element type

Sorting (stable, O(n log n) worst case):

| Method        | Mutates? | Comparator                          |
| ------------- | -------- | ----------------------------------- |
| `sort()`      | Yes      | `Ord::cmp` (requires `T: Ord`)      |
| `sort_by()`   | Yes      | Custom `fn mut(&T, &T) -> Ordering` |
| `sorted()`    | No       | `Ord::cmp` (requires `T: Ord`)      |
| `sorted_by()` | No       | Custom `fn mut(&T, &T) -> Ordering` |

On a float, `Ord` is the IEEE 754 total order rather than the order `<` gives,
so a NaN still has a place in a sorted list. See
[WEP: The Operator Order and the Total Order](./wep-2026-09-23-comparison-traits.md).

```wado
let mut nums: List<i32> = [5, 3, 8, 1];
nums.sort();                             // in-place ascending

let orig: List<i32> = [5, 3, 8, 1];
let asc = orig.sorted();                // returns a new sorted list
```

### Collection Literal Coercion

Sequence literals `[e0, e1, ...]` and key-value literals `{ k: v, ... }` can be
coerced to any collection type through `From`. Coercing one materializes an
`Array`, which the target's `From<Array<…>>` impl builds from:

| Literal         | Materializes    | Impl the target writes                            |
| --------------- | --------------- | ------------------------------------------------- |
| `[e0, e1, ...]` | `Array<E>`      | `From<Array<T>> for List<T>`                      |
| `{ k: v, ... }` | `Array<[K, V]>` | `From<Array<[String, V]>> for TreeMap<String, V>` |

This applies only where a coercion runs. The tuple and struct readings keep
their priority: with no target type `[1, 2, 3]` is still the tuple
`[i32, i32, i32]` (see [List Literals](#list-literals)), and `{ … }` against a
nominal struct with matching fields is still a struct literal.

A key-value literal is an array of pairs, so `[["a", 1]]` builds the same map
`{ a: 1 }` does. `Array<T>` itself needs no impl — the array the coercion
materializes is already the result.

#### Usage

```wado
let arr: List<i32> = [1, 2, 3];

use { TreeMap } from "core:collections";
let map: TreeMap<String, i32> = { width: 1920, height: 1080 };
```

Making a user type literal-constructible is one ordinary impl:

```wado
impl<T> From<Array<T>> for MyVec<T> {
    fn from(elements: Array<T>) -> MyVec<T> { ... }
}
```

Where a type accepts both literal forms, `{ … }` takes the impl whose element
is a two-element tuple and `[ … ]` prefers the one whose element is not;
several candidates for one form are an ambiguity error the site reports, and
`T::from(…)` written out resolves it.

#### Implicit conversion

A literal is implicitly converted to its target type through `From`. No other
expression is implicitly converted — a literal position is the whole of it.

```wado
let v: List<Value> = [1, "x"];   // OK — every element is a literal
let v: List<Value> = [a, b];     // ERROR — write [Value::from(a), Value::from(b)]
```

Coercion is literal-only — it does not apply to bound variables. If the target
type is a struct with matching fields, it is interpreted as a struct literal and
coercion is not attempted.

#### `..base` spread

`..base` inside a literal merges through `LiteralSpread`, last write wins:

```wado
pub trait LiteralSpread with () {
    fn spread_literal(&mut self, base: Self);
}
```

A type without the impl rejects `..base` where it is written, and a sequence
literal cannot carry one at all — `[..xs, 4]` is a tuple spread.

See [`docs/wep-2026-08-24-literal-from-array.md`](./wep-2026-08-24-literal-from-array.md)
for the impl-selection rule and newtype targets.

## Compile-Time Location Literals

Compile-time location literals provide source location information at compile time. They use the `#` prefix to clearly signal compile-time evaluation.

| Literal                  | Type       | Value                                              |
| ------------------------ | ---------- | -------------------------------------------------- |
| `#file`                  | `String`   | Current source file path                           |
| `#line`                  | `i32`      | Current line number (1-indexed)                    |
| `#function`              | `String`   | Name of the enclosing function                     |
| `#data`                  | `String`   | `__DATA__` section content (compile error if none) |
| `#include_str("path")`   | `String`   | External file content as string                    |
| `#include_bytes("path")` | `ByteList` | External file content as bytes                     |

```wado
fn example() {
    println(`Error at ${#file}:${#line}`);
    println(`In function: ${#function}`);
}
```

### `#data`

Returns the raw text content of the `__DATA__` section as a `String`. This is useful for programs that need to access embedded metadata at runtime (e.g., configuration, test fixtures, embedded documents). Using `#data` in a file that has no `__DATA__` section is a compile error.

```wado
export fn run() with Stdout {
    let config = #data;  // contains the __DATA__ section text
    println(config);
}

__DATA__
{"key": "value"}
```

### `#include_str` and `#include_bytes`

`#include_str("path")` reads an external file at compile time and returns its content as a `String`. The file must be valid UTF-8; otherwise, a compile error is raised. `#include_bytes("path")` returns the raw bytes as `ByteList` without UTF-8 validation.

The path argument must be a string literal. Paths are resolved relative to the source file containing the expression. See [WEP: Compile-Time File Inclusion](./wep-2026-03-02-include-str.md).

```wado
let template = #include_str("./templates/header.html");
let icon: ByteList = #include_bytes("./assets/logo.png");
```

### `#function` Format

Returns the name without type arguments or signature:

| Context                 | `#function` value            |
| ----------------------- | ---------------------------- |
| Free function           | `my_function`                |
| Method                  | `Point::distance`            |
| Method of `Box<String>` | `Box::name`                  |
| Closure                 | `parent_function::{closure}` |

### Call-site evaluation in default arguments

As a [default argument](./spec-functions.md#default-arguments), `#file` / `#line` / `#function` evaluate at the call site, so a defaulted location parameter reports the caller (cf. Swift's `#file`/`#line` defaults, C++'s `std::source_location::current()`):

```wado
pub fn log(msg: String, file: String = #file, line: i32 = #line) { /* ... */ }

log("started"); // file/line report this call, not where `log` is defined
```

Name resolution in a default otherwise uses the callee's scope; only these three literals are redirected. `#data` / `#include_str` / `#include_bytes` and struct field defaults always report their own defining file. For a nested defaulted call (`fn outer(x = loc())`), every literal reports the outermost call site (`outer(...)`).

## Object Literals

Object literal syntax supports unquoted keys and shorthand properties.

For struct initialization syntax, see the [Structs](./spec-types.md#structs) section.

### TreeMap (Insertion-Order Map)

For associative arrays, use `TreeMap` from `core:collections`:

```wado
use { TreeMap } from "core:collections";

let mut map = TreeMap::<String, i32>::new();
map["x"] = 10;                   // insert or overwrite
map["y"] = 20;

let v = map["x"];                // panics if key not found
let opt = map.get("x");          // returns Option<V>
map.try_insert("x", 99);         // inserts only if absent; reports whether it did

// Keys preserve insertion order
let keys = map.keys();  // an iterator over the keys, in insertion order

// Functional-update spread: seed from a base map, then override/add keys
let m2: TreeMap<String, i32> = { ..map, "x": 99, "w": 40 };
```

A `{ ..base, "k": v }` key-value literal seeds the builder with every entry of
`base` (same map type) and then applies the explicit keys, so explicit keys
override the base. Like the struct form, the spread is leading and single. See
[WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

### Access Methods

```wado
// Struct: dot notation
user.name

// TreeMap: bracket notation or methods
map["key"]        // panics if key not found
map.get("key")    // returns Option<V>
```
