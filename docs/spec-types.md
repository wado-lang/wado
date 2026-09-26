# Types

## The Prelude

The prelude (`core:prelude`) is automatically imported into every module, providing access to fundamental types without requiring explicit imports:

### Automatically Available

- `String` - UTF-8 string type
- `List<T>` - Dynamic array type
- `Option<T>` and its cases `Some(x)` and `None` (`null` also denotes `None`)
- `Result<T, E>` and its cases `Ok(x)` and `Err(e)`
- `Stream<T>` - Component Model async stream
- `Future<T>` - Component Model async future
- `i128`, `u128` - 128-bit integer types

A case is written bare (`Some(x)`) only where an expected type says which type
it belongs to. Elsewhere it is qualified: `Option::Some(x)`.

### Disabling the Prelude

```wado
#![no_prelude]  // At the top of a module

// Now you must explicitly import everything
use {String, List, Option, Result, Stream, Future} from "core:prelude";
```

## Primitive Types

Primitive types are built into the language (no import required):

```wado
// Numeric
i8, i16, i32, i64
u8, u16, u32, u64
f32, f64
f16, bf16   // half precision: storage only, no arithmetic

// Basic
bool
char
```

`f16` and `bf16` hold a value but do no arithmetic, and `as` does not cast them.
`to_bits` / `from_bits` reach the bits, and `From` / `TryFrom` / `from_f32`
convert the value. A comparison widens both operands to `f32` and compares
those. See [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

## Associated Constants

Associated constants are compile-time constants defined in `impl` blocks using the `const` keyword. They cannot be mutated.

```wado
impl f64 {
    pub const PI: f64 = 3.14159265358979323846;
}

let pi = f64::PI;
```

Primitive types provide built-in associated constants and static methods. See [`core:prelude`](./stdlib-core-prelude.md) for the full list.

## 128-bit Integer Types (i128/u128)

Unlike primitive types, `i128` and `u128` are implemented as structs in the prelude. They can be used like primitives thanks to operator overloading:

```wado
let a: u128 = 42;                      // literal coercion
let b = u128::from_u64(1_000_000);     // explicit construction
let sum = a + b;                       // via Add trait
let cmp = a < b;                       // via Ord trait

// Access low/high 64-bit parts
let low = a.low();
let high = a.high();
```

Available operations:

| Category   | Operations                                                     |
| ---------- | -------------------------------------------------------------- |
| Arithmetic | `+`, `-`, `*`, `/`, `%`, unary `-` (i128)                      |
| Comparison | `==`, `!=`, `<`, `<=`, `>`, `>=`                               |
| Bitwise    | `&`, `\|`, `^`, `~`, `<<`, `>>`                                |
| Conversion | `from_u64()`, `from_i64()`, `low()`, `high()`, `as`, `TryFrom` |

Literal and range patterns work on them in every pattern position, nested ones
included: `match [x, y] { [1..=5, _] => … }`. Each pattern matches exactly when
the equivalent `==` or range comparison holds.

`as` casts follow Rust semantics in both directions:

```wado
let a = 42 as u128;           // numeric → wide int
let e = 1.0e40 as u128;       // float → wide int, saturates (u128::MAX)
let b = a as f64;             // wide int → float, correctly rounded (ties to even)
let c = a as i64;             // wide int → int, truncates to the low bits
let d = (-1 as i128) as u128; // i128 ↔ u128 reinterprets the bits (u128::MAX)
```

Checked conversions are available through `TryFrom` (e.g. `i64::try_from(a)`, `u128::try_from(n)`), returning `Err` when the value is out of range for the target type.

## SIMD Types (v128)

See [WEP: SIMD v128](./wep-2026-01-31-simd-v128.md) for full design and rationale.

Wado exposes WebAssembly SIMD via the `core:simd` module. A single primitive type `v128` represents a 128-bit vector, with 10 newtype aliases providing type-safe interpretations:

| Category | Types                              |
| -------- | ---------------------------------- |
| Signed   | `i8x16`, `i16x8`, `i32x4`, `i64x2` |
| Unsigned | `u8x16`, `u16x8`, `u32x4`, `u64x2` |
| Float    | `f32x4`, `f64x2`                   |

All SIMD newtypes share the `v128` base and can be reinterpreted via `as` cast (zero-cost). Each type provides `splat()` construction, lane access, per-lane comparison methods such as `eq` and `lt`, and the operators its lanes support. The sets differ by type, following the Wasm SIMD instruction set:

- `/` exists only on `f32x4` and `f64x2`, and the bitwise operators only on the integer types.
- `i8x16` and `u8x16` have no `*`.
- `u64x2` compares only with `eq` and `ne`.
- `i8x16` and `i16x8` read a lane with `extract_lane_s` or `extract_lane_u`; the other types use `extract_lane`.

Sequence literal coercion is supported via `impl From<Array<i32>> for i32x4`:

```wado
use { i32x4, f64x2 } from "core:simd";

let v: i32x4 = [1, 2, 3, 4];     // tuple literal coercion
let w = i32x4::splat(10);         // broadcast
let sum = v + w;                   // [11, 12, 13, 14]
let mask = v.lt(&w);              // per-lane comparison mask
```

Beyond basic arithmetic and comparison, types provide specialized operations: saturating arithmetic (`add_sat_s/u` and `sub_sat_s/u` on signed types, `add_sat` and `sub_sat` on unsigned ones), lane narrowing/extension, extended multiplication, pairwise addition, type conversion between integer and float, and bit selection. See the `core:simd` module documentation for the full API.

### Relaxed SIMD

Relaxed SIMD operations trade strict determinism for performance. Edge-case behavior (NaN, out-of-range values) is implementation-defined but consistent within a single runtime. Methods use the `relaxed_` prefix on existing newtypes:

- Fused multiply-add: `f32x4/f64x2.relaxed_madd(b, c)`, `relaxed_nmadd(b, c)`
- Min/Max: `f32x4/f64x2.relaxed_min/max` (faster than strict `min`/`max`)
- Truncation: `i32x4::relaxed_trunc_f32x4_s/u`, `relaxed_trunc_f64x2_s/u_zero`
- Lane select: `relaxed_laneselect` on `i8x16`, `i16x8`, `i32x4`, `i64x2`
- Swizzle: `i8x16.relaxed_swizzle`
- Dot product: `i16x8.relaxed_dot_i8x16_i7x16_s`, `i32x4::relaxed_dot_i8x16_i7x16_add_s(a, b, &c)`
- Q15 multiply: `i16x8.relaxed_q15mulr_s`

Relaxed SIMD is not modeled as an effect because: (1) results are deterministic within an environment, (2) hardware behavior cannot be intercepted, and (3) standard floats already have similar NaN non-determinism.

## String Type

`String` is a built-in type representing UTF-8 encoded text with value semantics and GC management.

### Design Principles

- Value semantics: deep-copied on assignment, parameter passing, and return — passing a `String` to a function gives the callee its own buffer
- Mutable through the local binding: `push_str` modifies the receiver in place and `+=` reassigns the binding, but neither reaches the caller's value
- GC-managed: Memory is automatically managed by Wasm GC
- UTF-8 encoding: Direct mapping to Component Model `string`

### Semantics and Encoding

- Semantically, a `String` is a sequence of Unicode scalar values
- Invalid UTF-8 byte sequences are not allowed; all String values must be valid UTF-8
- This ensures interoperability with Component Model `string` type and safe string operations

### Index Access (Prohibited)

Direct index access is prohibited to avoid ambiguity between byte and character indexing:

```wado
let s = "Hello世界";

// Prohibited
s[0]      // Compile error
s[0..<5]  // Compile error
```

Use explicit methods instead:

```wado
// Byte-level access
let bytes: List<u8> = s.bytes().collect();
let first_byte = bytes[0];

// Character-level access
let chars: List<char> = s.chars().collect();
let first_char = chars[0];

// Other methods
s.len() -> i32             // Length in bytes
s.is_empty() -> bool       // Check if empty
```

#### Note

`bytes()` and `chars()` return iterator objects (`StrUtf8ByteIter` and `StrCharIter`) that implement both `Iterator` and `IntoIterator`, so they work with `for-of` directly:

```wado
for let c of "hello".chars() {
    println(`${c}`);  // h, e, l, l, o
}

for let b of "hello".bytes() {
    println(`${b}`);  // 104, 101, 108, 108, 111
}
```

#### String Building

`push_str` appends to a `String` in place:

```wado
let mut builder = String::with_capacity(20);
builder.push_str("Hello");
builder.push_str(", ");
builder.push_str("World!");
// builder is now "Hello, World!"

// `+` operator for two-string concatenation
let combined = "Hello, " + "World!";  // "Hello, World!"

// `join` concatenates a list with a separator
let parts: List<String> = ["a", "b", "c"];
let joined = parts.join(",");         // "a,b,c"
```

### Concatenation

#### New String (`+` operator)

```wado
let s1 = "hello";
let s2 = " world";
let s3 = s1 + s2;  // Creates new String
```

#### Reassignment (`+=` operator)

`a += b` is `a = a + b` through `Add`. `String` follows the same rule as every other type:

```wado
let mut s = "hello";
s += " world";     // s = s + " world"
s += "!";
```

See `docs/wep-2026-01-15-string-type-design.md` for design rationale.

## Newtype

`type T = U` creates a newtype: a distinct type with the same values as its base type. See [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md).

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;       // literal coercion
let km: Kilometers = 1.0;

let sum = m + m;              // OK: Meters + Meters -> Meters
// let bad = m + km;          // ERROR: cannot mix Meters and Kilometers

let raw: f64 = m as f64;      // explicit cast required
```

### Properties

- `T` is a distinct type from `U` (no implicit conversion)
- `T` inherits all methods, operators, and traits from `U`
- Explicit `as` cast required to convert between `T` and `U`
- Zero runtime cost
- Literal coercion to `T` when type context expects `T`

A newtype does not carry an invariant of its own. `as` converts in both
directions at no cost, so `T` admits exactly what `U` admits. An invariant the
base type does not enforce, such as UTF-8 in a byte view, belongs in a `struct`
with a private field and a checked constructor, where the check is the only way
in.

### Method Signature Substitution

When calling inherited methods on a newtype, parameters and return types are substituted:

```wado
type Location = Point;

impl Point {
    fn distance(&self, other: &Point) -> f64 { ... }
}

let loc1: Location = Point { x: 0, y: 0 } as Location;
let loc2: Location = Point { x: 3, y: 4 } as Location;
loc1.distance(&loc2);  // params expect &Location, returns f64
```

### Newtype-Specific Methods

```wado
impl Location {
    fn name(&self) -> String { ... }  // only on Location, not Point
}
```

### Chained Newtypes

```wado
type A = i32;
type B = A;
type C = B;

let c: C = 1;
let a = c as A;    // OK: direct cast through chain
let i = c as i32;  // OK: direct cast to ultimate base
```

For complete type isolation where you want to hide base type methods, use a struct wrapper:

```wado
struct Miles { value: i32 }
```

## Structs

Wado uses `struct` for structured data types. A struct crosses a component boundary as a Component Model `record`.

```wado
// Struct definition
struct User {
    name: String,
    age: i32,
    active: bool,
}

// Recursive struct
struct Node {
    value: i32,
    next: Option<Node>,
}
```

### Field Visibility

Struct fields follow the same visibility rules as other declarations (see [Visibility](./spec-modules.md#visibility)). A field without a modifier is private to the defining file; `internal` widens it to the package; `pub` exposes it to other Wado packages.

```wado
pub struct Config {
    pub name: String,   // visible to other packages
    internal tag: i32,  // visible to other files in this package
    secret: i32,        // private to this file
}
```

Within the defining module, all fields (including private ones) are accessible for construction, reading, and mutation. From another file in the same package, `internal` (and `pub`) fields are accessible; from another package, only `pub` fields are. Reading, setting, or binding a field beyond its reach is a compile error, whether through field access (`c.secret`), a struct literal (`Config { secret: ... }`), or a destructuring pattern (`let Config { secret, .. } = c`, `match`). A non-reachable field may still be _omitted_ from a struct literal in another module when it has a default expression (`f: T = expr`): the default is evaluated in the defining module, so the field is never read or set across the boundary and encapsulation is preserved. A non-reachable field without a default cannot be satisfied from another module, so such a struct can only be constructed by a function within reach.

### Struct Construction

```wado
let user = User { name: "Alice", age: 30, active: true };

// Shorthand (variable name matches field)
let name = "Bob";
let age = 25;
let bob: User = { name, age, active: false };

// Implicit struct literal (requires type annotation)
let user: User = { name: "Alice", age: 30, active: true };
```

Functional update (`..base`): a leading `..base` fills every field the literal
does not list explicitly from the struct value `base` (same type). The listed
fields override; `base` is evaluated once and left unchanged (value semantics).

```wado
let u2 = User { ..user, age: 31 };  // every field from `user`, age replaced
```

The spread is leading and single: a field written before it would be overwritten
and unused, so `User { age: 31, ..user }`, a second spread, and a bare
`User { ..user }` (a plain copy) are all errors. A `..base` cannot read a field
that is not reachable at the use site, so it never exposes a private field across
a module boundary. See [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

### Struct Destructuring

```wado
let p = Point { x: 10, y: 20 };

// Unnamed destructuring (type inferred from RHS)
let { x, y } = p;

// Named destructuring (explicit type)
let Point { x, y } = p;

// Renaming fields
let { x: horizontal, y: vertical } = p;

// Ignore remaining fields with ..
struct Person { name: String, age: i32, email: String }
let { name, .. } = person;

// Mutable destructuring
let mut { x, y } = p;

// Nested destructuring
struct Line { start: Point, end: Point }
let { start: { x: x1, y: y1 }, end: { x: x2, y: y2 } } = line;

// In for-of
for let { x, y } of points {
    println(`${x}, ${y}`);
}
```

### Auto-derived Traits

Structs derive `Eq` (field-wise equality) and `Ord` (lexicographic comparison by field declaration order) when all fields implement those traits. They are derived where a use or bound needs them, not for every struct. See [Bound-Driven Eq / Ord](./spec-traits.md#bound-driven-eq--ord). A user-provided `impl Eq` or `impl Ord` takes precedence.

For generic structs, the auto-derived impls have trait bounds on the type parameters: `impl<T: Eq> Eq for Foo<T>`, `impl<T: Ord> Ord for Foo<T>`.

Variants derive `Eq` only (not `Ord`) the same on-demand way, when all payload types implement `Eq`: both values must be the same case, and payloads (if any) are compared. A user-provided `impl Eq` takes precedence. For generic variants, the auto-derived impls have trait bounds on the type parameters: `impl<T: Eq> Eq for Maybe<T>`.

### Struct Field Defaults

See [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

Struct fields may declare a default expression with `= expr`. Fields with defaults may be omitted at construction sites; fields without defaults are required:

```wado
struct ServerConfig {
    host: String,            // required
    port: i32 = 8080,        // optional
    timeout: i32 = 30,       // optional
    debug: bool = false,     // optional
}

let c = ServerConfig { host: "localhost" };
// Desugars to: ServerConfig { host: "localhost", port: 8080, timeout: 30, debug: false }

let c = ServerConfig { host: "localhost", port: 3000 };
// Desugars to: ServerConfig { host: "localhost", port: 3000, timeout: 30, debug: false }

ServerConfig { port: 3000 };  // compile error: missing required field 'host'
```

Default expressions are evaluated at the construction site. They must be effect-free and cannot reference other fields. Field shorthand (`{ host }`) and destructuring are unaffected: destructuring sees every field regardless of defaults.

A non-generic struct whose every field has a default auto-derives `Default`. A fieldless struct has no field to default, so it qualifies. See [Default Trait](./spec-traits.md#default-trait).

## Generic Type Inference

Wado infers type arguments for struct literals, variant constructors, and generic function and method calls. It uses two complementary mechanisms.

Forward inference derives type parameters from the values provided (fields, payloads, or arguments):

```wado
struct Box<T> { value: T }
let b = Box { value: 42 };             // Box<i32> — T=i32 from field value

let opt = Option::Some("hello");        // Option<String> — T=String from payload
let opt2 = Option::Some(42);            // Option<i32> — T=i32 from payload
```

Backward inference derives type parameters from an expected type context (variable annotation, function parameter type, or return type):

```wado
let none: Option<i32> = Option::None;   // T=i32 from annotation
let ok: Result<i32, String> = Result::Ok(42);
// T=i32 from payload (forward), E=String from annotation (backward)
```

When both mechanisms apply, they must agree. An untyped literal takes its type from the expected type, so `let x: Option<i64> = Option::Some(42)` is an `Option<i64>`. A value whose type is already fixed must match it: with `y: i32`, `let b: Box<i64> = Box { value: y }` is a type mismatch. Backward inference fills in any parameter the values do not mention.

A turbofish on the type name pins the arguments outright. It reaches a parameter
no field mentions, and it overrides one a field would otherwise settle. It says
what the matching annotation says, so the two must agree:

```wado
struct Tagged<T> { tag: i32 }
let b = Box::<i64> { value: 1 };        // T=i64, not the i32 the literal infers
let t = Tagged::<String> { tag: 7 };    // T names no field
let n: Box<i32> = Box::<i64> { … };     // error: the annotation disagrees
```

On a variant, the turbofish may follow the case name instead, as in Rust. It
means the same thing and names every parameter. Writing one on both the type
and the case is an error.

```wado
let a = Option::Some::<i64>(1);         // Option::<i64>::Some(1)
let e = Result::Err::<i32, String>("x");
let n: Option<i32> = None::<i32>;       // a bare case, where its type is known
```

A newtype over a variant names its base's cases, and the value takes the
newtype. A generic newtype's arguments are inferred as the variant's would be:

```wado
type Opt<T> = Option<T>;
let a = Opt::Some(1);                   // Opt<i32>
let b: Opt<i64> = Opt::None;            // the annotation settles T
let c = Opt::<i64>::Some(1);
```

### Scope of inference

| Site                   | Forward (from values) | Backward (from expected type) |
| ---------------------- | --------------------- | ----------------------------- |
| Struct literals        | yes                   | yes                           |
| Variant constructors   | yes                   | yes                           |
| Generic function calls | yes                   | yes                           |
| Generic method calls   | yes                   | yes                           |

```wado
fn identity<T>(x: T) -> T { return x; }
fn none_of<T>() -> Option<T> { return null; }

let x = identity(42);                  // T=i32 from the argument
let s = identity("hi");                // T=String from the argument
let n: Option<i64> = none_of();        // T=i64 from the annotation
```

A `_` inside a turbofish leaves that type-argument slot for inference while the
others stay explicit, reusing the same inference an omitted turbofish uses. The
explicit (non-`_`) arguments always win. An uninferable `_` is the same error as
an omitted turbofish on an uninferable parameter. A `_` works only in a
turbofish: in a plain type annotation (`let xs: List<_>`) it is an error.

A turbofish may also stop short of the declared parameters. The ones it does not
name are inferred, as a `_` in their place would be.

```wado
let r = Result::<_, MyErr>::Ok(42);    // infers Ok payload type, pins the error type
let a = pick::<_, bool>(1, true);      // infers the first type argument
let b = pick::<i32>(1, true);          // stops short: infers the second
```

## Enums, Variants, and Flags

Wado follows Component Model's distinction between enums and variants (unlike Rust):

Enums (no payloads - Component Model `enum`):

```wado
// Simple enumeration - all cases have no data
enum Color {
    Red,
    Green,
    Blue,
}

// Construction
let c = Color::Red;
let d: Color = Red; // bare only where the expected type supplies it (an
                    // annotation, a parameter, a return type, a payload);
                    // `let e = Red;` is an error

// Pattern matching: match, if let, matches
let name = match c {
    Red => "red",
    Green => "green",
    Blue => "blue",
};

if let Red = c { /* ... */ }

if c matches { Green } { /* ... */ }

// Match with wildcards and guards
match c {
    Red => "warm",
    _ => "other",
}
```

Enums auto-derive `Display` as the bare case name (`Red`), distinct from `Inspect`'s `Color::Red`. `Eq` (discriminant equality) and `Ord` (declaration order) derive the same on-demand way as for structs. See [Auto-derived Traits](#auto-derived-traits) above.

Enums can have `impl` blocks:

```wado
impl Color {
    fn is_warm(&self) -> bool {
        return match *self {
            Red => true,
            _ => false,
        };
    }
}
```

Variants (with payloads - Component Model `variant`):

Wado variants have exactly one payload type per case. Unit cases have no payload, and multiple values require explicit tuple syntax `[T, U]`:

```wado
// Sum type where variants can carry data
variant Shape {
    Circle(f64),           // single payload (radius)
    Rectangle([f64, f64]), // explicit tuple payload (width, height)
    Point,                 // no payload (unit)
}

// Generic variant
variant Maybe<T> {
    Just(T),
    Nothing,
}

// Construction
let s = Shape::Circle(5.0);
let r = Shape::Rectangle([10.0, 20.0]);
let p = Shape::Point;
let c: Shape = Circle(5.0); // bare where the expected type supplies it

// Option construction — type inferred from payload (forward inference)
let opt = Option::Some(42);              // Option<i32> inferred
let opt_str = Option::Some("hello");     // Option<String> inferred

// Option construction — type inferred from annotation (backward inference)
let none: Option<i32> = Option::None;    // T=i32 from annotation

// Result construction — combined forward and backward inference
let ok: Result<i32, String> = Result::Ok(42);      // T from payload, E from annotation
let err: Result<i32, String> = Result::Err("fail"); // E from payload, T from annotation

// Explicit turbofish syntax (always available), on the type or on the case
let opt2 = Option::<i32>::Some(42);
let opt3 = Option::Some::<i32>(42);

// Inside an impl, `Self::Case` names a case of the impl's own type. `Self`
// already carries its type arguments, so the case takes no turbofish.
impl<T> Maybe<T> {
    fn wrap(v: T) -> Maybe<T> { return Self::Just(v); }
}

if let Some(x) = opt {
    println(`Got: ${x}`);
}

// Custom variant pattern matching with tuple destructuring.
// A pattern names the case bare or qualified (`ParseResult::Fail`).
variant ParseResult {
    Fail,
    Number([i32, i32]),  // start, end positions
}
let result = ParseResult::Number([0, 10]);
if let Number([start, end]) = result {
    println(`Got number from ${start} to ${end}`);
}
if let Fail = result {
    println("Failed");
}

match s {
    Circle(r) => calculate_circle_area(r),
    Rectangle([w, h]) => w * h,
    Point => 0.0,
}
```

`Option<T>` and `Result<T, E>` are declared as variants in `core:prelude`.

Flags (bit flags - Component Model `flags`):

```wado
// Bit flags - each member is a power-of-two bitmask
pub flags Perms {
    Read,     // bit 0 → value 1
    Write,    // bit 1 → value 2
    Execute,  // bit 2 → value 4
}

// Access members
let r = Perms::Read;   // 1
let w = Perms::Write;  // 2

// Bitwise combination with |
let rw = r | w;        // 3

// Bitwise AND for masking
let masked = rw & Perms::Read;   // 1 (Read bit is set)

// Bitwise XOR for toggling
let toggled = rw ^ Perms::Read;  // 2 (Read bit cleared)

// Special static methods
let none = Perms::none();  // 0 (no bits set)
let all  = Perms::all();   // 7 (all bits set)

// Cast to u32 for numeric comparison
assert rw as u32 == 3;

// Arithmetic operators (+, -, *, /, %) are NOT allowed on flags types
// They produce a compile error; use bitwise operators (|, &, ^) instead
```

Flags auto-derive `Eq` and `Ord` over their raw bits, the same on-demand way enums derive theirs over the discriminant. See [Auto-derived Traits](#auto-derived-traits).

A flags type is a newtype over `u32`: an integer literal coerces to it, `as` converts to and from `u32`, and it inherits `u32`'s methods. Member names can carry `#[cm("...")]` attributes for Component Model name mapping:

```wado
pub flags PathFlags {
    #[cm("symlink-follow")]
    SymlinkFollow,
}
```

Note: Wado's `enum` maps to Component Model's `enum` (simple enumeration), and `variant` maps to Component Model's `variant` (tagged union with payloads). This differs from Rust where `enum` can have payloads.

---
