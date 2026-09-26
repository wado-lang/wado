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

### Half Precision (`f16`, `bf16`)

`f16` is IEEE 754 binary16. `bf16` is bfloat16: `f32`'s exponent range with an
8-bit significand. Both are storage types. They hold a value and do no
arithmetic:

- No arithmetic or bitwise operator applies to either, unary `-` included.
- `as` does not convert them in either direction (see
  [Type Cast](./spec-lexical.md#type-cast-as)). The error names the method to
  write instead.
- A numeric literal coerces to either type and is rounded once (see
  [Floating-Point Literals](./spec-literals.md#floating-point-literals)).
- A comparison widens both operands to `f32` and compares those, so `<` is
  IEEE and `Ord` is the total order (see
  [Ord](./spec-traits.md#ord---ordering)).
- Neither type crosses a component boundary (see
  [Type Mapping](./spec-components.md#type-mapping-at-component-boundaries)).

Values convert through methods and the conversion traits:

```wado
let one = f16::from_bits(0x3C00);   // 1.0; `one.to_bits()` is 0x3C00 again
let wide = f32::from(one);          // exact; `f64::from` too
let near = f16::from_f32(0.1);      // rounds to the nearest f16
let same = f16::try_from(wide);     // Ok: 1.0 survives the round trip
```

- `to_bits` and `from_bits` read and write the 16 bits as a `u16`.
- `From<f16>` and `From<bf16>` are implemented for `f32` and `f64`. Widening is
  exact.
- `from_f32` and `from_f64` narrow. They round once, to nearest with ties to
  even. A value that rounds past the largest finite one becomes an infinity.
- `TryFrom<f32>` and `TryFrom<f64>` answer `Ok` only where the value survives the
  round trip, a NaN included. Otherwise they answer `Err(ConvertError)`.
- `from_str` rounds the decimal text once, as a literal is rounded. Text that
  rounds past the largest finite value parses to an infinity.
  `from_str_lenient` takes the spellings `LenientFromStr` accepts.
- There is no conversion between `f16` and `bf16`, because each keeps something
  the other drops: `f16` has the shorter exponent range, `bf16` the shorter
  significand. Widen to `f32` and narrow again.

Both types carry `f32`'s associated constants under the same names (`MAX`,
`MIN`, `MIN_POSITIVE`, `EPSILON`, `INFINITY`, `NAN`, `MANTISSA_DIGITS`, …) and
`is_nan`.

A template renders either type through its `f32` value, as
[Display Output](./spec-literals.md#display-output) and
[Inspect Output](./spec-literals.md#inspect-output) state.
[Serialization](./spec-serialization.md) states how either type is written and
read.

Rationale: [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

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
let b = a as f64;             // wide int → float, correctly rounded (ties to even)
let c = a as i64;             // wide int → int, truncates to the low bits
let d = (-1 as i128) as u128; // i128 ↔ u128 reinterprets the bits (u128::MAX)
```

Checked conversions are available through `TryFrom` (e.g. `i64::try_from(a)`, `u128::try_from(n)`), returning `Err` when the value is out of range for the target type.

## SIMD Types (v128)

Wado exposes WebAssembly SIMD via the `core:simd` module. A single primitive type `v128` represents a 128-bit vector, and 10 newtypes over it give it type-safe lane interpretations:

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

`v128` itself takes no arithmetic, bitwise or shift operator. The lane types
carry those, and each works lane by lane. A shift takes one `u32` count and
shifts every lane by it.

The comparison methods (`eq`, `ne`, `lt`, `le`, `gt`, `ge`) answer a mask of
the same lane type: each lane is all ones where the comparison holds and all
zeros where it does not. `bitselect` takes such a mask.

`==` and `!=` are not lane-wise. On `v128` and on every lane type, they compare
all 128 bits and answer one `bool`, because every lane type is a newtype over
the same `v128`. So a NaN lane equals itself, and a `0.0` lane differs from a
`-0.0` lane. Use `eq` for the IEEE comparison.

```wado
let nan = f32x4::splat(f32::NAN);
assert nan == nan;                  // the bits are equal
let mask = nan.eq(&nan);            // every lane zero: IEEE says NaN != NaN
```

Beyond basic arithmetic and comparison, types provide specialized operations: saturating arithmetic (`add_sat_s/u` and `sub_sat_s/u` on signed types, `add_sat` and `sub_sat` on unsigned ones), lane narrowing/extension, extended multiplication, pairwise addition, type conversion between integer and float, and bit selection. Where an input is out of range, the strict operations are defined: `trunc_sat_*` saturates, and `swizzle` gives a zero lane for an index past the last lane. See [`core:simd`](./stdlib-core-simd.md) for the full API.

### Relaxed SIMD

Relaxed SIMD operations trade strict determinism for performance. Edge-case behavior (NaN, out-of-range values) is implementation-defined but consistent within a single runtime. Methods use the `relaxed_` prefix on existing newtypes:

- Fused multiply-add: `f32x4/f64x2.relaxed_madd(b, c)`, `relaxed_nmadd(b, c)`
- Min/Max: `f32x4/f64x2.relaxed_min/max` (faster than strict `min`/`max`)
- Truncation: `i32x4::relaxed_trunc_f32x4_s/u`, `relaxed_trunc_f64x2_s/u_zero`
- Lane select: `relaxed_laneselect` on `i8x16`, `i16x8`, `i32x4`, `i64x2`
- Swizzle: `i8x16.relaxed_swizzle`
- Dot product: `i16x8.relaxed_dot_i8x16_i7x16_s`, `i32x4::relaxed_dot_i8x16_i7x16_add_s(a, b, &c)`
- Q15 multiply: `i16x8.relaxed_q15mulr_s`

A relaxed method is an ordinary method. Calling one declares no effect.

Rationale: [WEP: SIMD v128 Types](./wep-2026-01-31-simd-v128.md).

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

### Iterating Bytes and Characters

`bytes()` and `chars()` return iterator objects (`StrUtf8ByteIter` and `StrCharIter`) that implement both `Iterator` and `IntoIterator`, so they work with `for-of` directly:

```wado
for let c of "hello".chars() {
    println(`${c}`);  // h, e, l, l, o
}

for let b of "hello".bytes() {
    println(`${b}`);  // 104, 101, 108, 108, 111
}
```

### String Building

`push_str` appends to a `String` in place:

```wado
let mut builder = String::with_capacity(20);
builder.push_str("Hello");
builder.push_str(", ");
builder.push_str("World!");
// builder is now "Hello, World!"

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

Rationale: [WEP: String Type Design](./wep-2026-01-15-string-type-design.md).

## Newtype

`type T = U` creates a newtype: a distinct type with the same values as its base type.

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
in. So does a type that must hide its base type's methods:

```wado
struct Miles { value: i32 }
```

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

Every occurrence of the base type in the signature is substituted: the
receiver, each parameter, and the return type, including inside a generic
argument. A method returning `Option<Point>` or `List<Point>` returns
`Option<Location>` or `List<Location>` on a `Location`. An argument of the base
type where the substituted signature expects the newtype is a type mismatch:
`loc1.distance(&Point { x: 0, y: 0 })` is an error.

### Inherited Associated Functions and Traits

A newtype inherits its base type's associated functions too. A function whose
return type is the base type returns the newtype, and the base's type arguments
settle the call, so it needs no turbofish:

```wado
type ByteList = List<u8>;

let mut b = ByteList::with_capacity(16);   // b: ByteList
b.push(0xff);
```

A newtype implements every trait its base implements. It satisfies a bound
the base satisfies (`fn max<T: Ord>(a: T, b: T)` takes two `Meters`). `for-of`
iterates it as it iterates the base, by value and through `&`, and `collect()`
builds it wherever it builds the base (`let b: ByteList = s.bytes().collect()`).

### Newtype-Specific Methods

```wado
impl Location {
    fn name(&self) -> String { ... }  // only on Location, not Point
}
```

A trait impl written for the newtype wins over the one it inherits from the
base. See [The Order](./spec-traits.md#the-order).

### Casts

`as` converts between a newtype and its base in both directions, between two
newtypes over the same base, and through a chain of newtypes in one step. A
reference casts the same way: `&Meters as &f64`. A generic newtype casts to and
from its base instantiation.

```wado
type A = i32;
type B = A;
type C = B;

let c: C = 1;
let a = c as A;    // OK: direct cast through chain
let i = c as i32;  // OK: direct cast to ultimate base

type Wrapper<T> = List<T>;
let w = [1] as Wrapper<i32>;
let l = w as List<i32>;    // OK
```

`as` does not reach a newtype nested inside another type: `List<Meters>` does
not cast to `List<f64>`, and `Option<Meters>` does not cast to `Option<f64>`.

A function type is the exception, since a function value is reused unchanged.
`as` converts one function type to another when both take the same number of
parameters and each parameter and the return type differ only by newtype steps,
at the top, under a reference, or inside a function type that is otherwise
identical. The cast may also widen `fn` to `fn mut` and add effects, never the
reverse. A function type casts to no other kind of type.

```wado
type Meters = f64;
fn double(x: f64) -> f64 { return x * 2.0; }

let h = double as fn(Meters) -> Meters;       // OK
let r = double as fn mut(f64) -> f64;         // OK: `fn` widens to `fn mut`
let bad = double as fn(i32) -> f64;           // Error: `i32` is not `f64`
```

Rationale: [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md).

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

A struct field takes the visibility modifiers other declarations take, and
reaches as far as [Visibility](./spec-modules.md#visibility) states.

```wado
pub struct Config {
    pub name: String,   // visible to other packages
    internal tag: i32,  // visible to other files in this package
    secret: i32,        // private to this file
}
```

Reading, setting, or binding a field beyond its reach is a compile error. This
holds for field access (`c.secret`), a struct literal (`Config { secret: ... }`),
and a destructuring pattern (`let Config { secret, .. } = c`, `match`).

A literal in another module may still omit a field it cannot reach when the
field has a default expression (`f: T = expr`). The default resolves in the
defining module (see [Struct Field Defaults](#struct-field-defaults)), so the
field is never read or set across the boundary. A field out of reach with no
default cannot be filled from another module, so only a function within reach
can construct such a struct.

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
a module boundary.

`base` must have the literal's own type, which an implicit literal takes from
its expected type (`let u: User = { ..user, age: 31 }`). Its type arguments take
part in inference like an explicit field. An override cannot change one, so with
`b: Box<i32>`, `Box { ..b, value: "s" }` is a type mismatch. The members are
evaluated once each, in source order, so `base` comes first. An override may
repeat the value `base` already holds.

Rationale: [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

### Anonymous Structs

A `{ … }` literal where no struct or map type is expected builds an anonymous
struct. It names no declaration: its type is its shape, the field names and
each field's type. Two literals of one shape have one type, so
`if c { { x: 1, y: 2 } } else { { x: 3, y: 4 } }` type-checks. Its fields are
read, destructured and nested as a named struct's are.

```wado
let p = { x: 1, y: 2 };
let line = { start: p, end: { x: 10, y: 20 } };
let { start: { x, y }, end } = line;
```

No `impl` can name an anonymous struct, so it takes derived traits
([Derivation Policy](./spec-traits.md#derivation-policy)) and blanket impls
([Candidates](./spec-traits.md#candidates)) only.

#### Composition

An anonymous literal may compose spread bases: `{ ..a, ..b, field: v }`
builds an anonymous struct whose fields are the union of the bases' and explicit
fields. Members apply in source order, and the last contributor of a name wins,
its type included. Every member is evaluated once, in source order. Each base is
a struct value, and a spread of anything else is an error.

Unlike a named struct's leading-single `..base`, composition allows spreads in
any position and more than one. One rule limits them: a member every one of
whose fields is overwritten by a later member is a dead-write error, so
`{ ..a, ..b }` with `a` and `b` of one struct type is rejected. A lone `{ ..a }`
is rejected too, since under value semantics it only copies `a`. A spread reads
only the fields reachable at the use site, as `a.f` would.

```wado
let base = { user_id: 1, ip: "10.0.0.1" };
let event = { ..base, level: "warn" };  // { user_id, ip, level }
```

Composition applies where no nominal type is expected. An expected struct type
makes the literal a named one, with the named struct's rule
([Struct Construction](#struct-construction)), and an expected map
type makes it a key-value literal ([`..base` Spread](./spec-literals.md#base-spread)).
A key-value spread with no map type expected is an error.

Rationale: [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

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

A struct derives `Eq` field by field, and `Ord` lexicographically in field
declaration order. A variant derives `Eq` only, not `Ord`: two values are equal
when they are the same case and their payloads, if any, are equal.

When a derived impl exists, which instantiations of a generic type it covers,
and how a written impl overrides it are stated in
[Derivation Policy](./spec-traits.md#derivation-policy).

### Struct Field Defaults

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

A default expression is evaluated each time a literal omits its field. It must be effect-free and cannot reference other fields. Field shorthand (`{ host }`) and destructuring are unaffected: destructuring sees every field regardless of defaults. A literal may omit every field that has a default, down to `ServerConfig {}` where all of them do.

A default resolves its names in the module that declares the struct, not where
the literal is written. It may name that module's private items, its import
aliases, and a type or variant case the constructing module never imports.
Nothing the constructing module declares, imports, or binds changes what an
omitted field evaluates to.

A default may name the struct's own type parameters. Each literal settles them
first, from a turbofish, its annotation, or the fields it lists, and the default
is evaluated at those types:

```wado
struct Holder<T: Default> { a: T, b: T = T::default() }

let h: Holder<String> = { a: "x" };   // b is ""
let s = Holder { a: 5 };              // T = i32 from `a`, so b is 0
```

A field with a default is optional in deserialization too (see
[Missing, Repeated, and Unknown Fields](./spec-serialization.md#missing-repeated-and-unknown-fields)).

Whether a struct derives `Default` from its field defaults is stated in
[Auto-Derivation](./spec-traits.md#auto-derivation).

Rationale: [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

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
