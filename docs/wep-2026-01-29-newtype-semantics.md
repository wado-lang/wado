# WEP: Newtype Semantics

## Decision

`type T = U` uses **newtype semantics**:

1. `T` is a distinct type from `U`
2. `T` inherits all methods, operators, and traits from `U`
3. `T` and `U` share the same runtime representation (zero-cost abstraction)
4. Explicit cast (`as`) is required to convert between `T` and `U`
5. Literal coercion to `T` is allowed when type context expects `T`

### Basic Usage

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;       // OK: literal coercion
let km: Kilometers = 1.0;     // OK: literal coercion

let sum = m + m;              // OK: Meters + Meters -> Meters
let bad = m + km;             // ERROR: cannot mix Meters and Kilometers

let raw: f64 = m as f64;      // OK: explicit cast
let converted = (m as f64) / 1000.0 as Kilometers;  // OK: explicit conversion
```

### Method Signature Substitution

When calling a method on a newtype, the method signature is substituted to use the newtype instead of the base type. This applies to:

- `self` parameter type
- Other parameters of the base type
- Return type

```wado
type Duration = u64;

// u64 has: fn saturating_add(&self, other: u64) -> u64
// When called on Duration, becomes: fn saturating_add(&self, other: Duration) -> Duration

let d1: Duration = 1000;
let d2: Duration = 2000;
let d3 = d1.saturating_add(d2);  // d3: Duration (not u64)

let raw: u64 = 500;
let bad = d1.saturating_add(raw);  // ERROR: expected Duration, got u64
```

This is critical for user-defined types like `i128`:

```wado
// i128 might be defined as a tuple internally
type i128 = [i64, i64];

impl i128 {
    fn add(&self, other: &i128) -> i128 { ... }
}

let a: i128 = ...;
let b: i128 = ...;
let c = a + b;  // c: i128 (not [i64, i64])
```

### Newtype-Specific Implementations

You can add methods specific to a newtype via `impl`:

```wado
type Radians = f64;
type Degrees = f64;

impl Radians {
    fn to_degrees(&self) -> Degrees {
        return (*self * 180.0 / 3.14159) as Degrees;
    }
}

impl Degrees {
    fn to_radians(&self) -> Radians {
        return (*self * 3.14159 / 180.0) as Radians;
    }
}

let r: Radians = 3.14159;
let d = r.to_degrees();  // d: Degrees

let x: f64 = 1.0;
x.to_degrees();  // ERROR: f64 has no method 'to_degrees'
```

### Base Type Implementations Are Inherited

When you add methods to the base type, all newtypes derived from it can use those methods:

```wado
type Meters = f64;
type Seconds = f64;

impl f64 {
    fn is_positive(&self) -> bool {
        return *self > 0.0;
    }
}

let m: Meters = 100.0;
let s: Seconds = -5.0;

m.is_positive();  // OK: true
s.is_positive();  // OK: false
```

### Trait Inheritance

Newtypes inherit all trait implementations from the base type:

```wado
type Duration = u64;

// u64 implements Eq and Ord
// Therefore Duration also implements Eq and Ord

let d1: Duration = 1000;
let d2: Duration = 2000;

d1 == d2;         // OK: false
d1 < d2;          // OK: true

fn compare<T: Ord>(a: T, b: T) -> bool { ... }
compare(d1, d2);  // OK: Duration satisfies Ord bound
```

### Cast Rules

| From          | To                      | Allowed       |
| ------------- | ----------------------- | ------------- |
| Newtype `T`   | Base type `U`           | Yes, via `as` |
| Base type `U` | Newtype `T`             | Yes, via `as` |
| Newtype `T`   | Newtype `S` (same base) | Yes, via `as` |
| `List<T>`     | `List<U>`               | No            |
| `&T`          | `&U`                    | Yes, via `as` |
| `Fn(T)`       | `Fn(U)`                 | No            |

### Chained Newtypes

```wado
type A = i32;
type B = A;
type C = B;

let c: C = 1;
let b = c as B;    // OK
let a = c as A;    // OK: direct cast through chain
let i = c as i32;  // OK: direct cast to base
```

### Generic Newtypes

```wado
type MyArray<T> = List<T>;

let arr: MyArray<i32> = [1, 2, 3];
arr.len();                        // OK: inherits List methods
arr.push(4);                      // OK

let plain: List<i32> = arr as List<i32>;  // ERROR: generic cast not allowed
```

### What Newtypes Do NOT Provide

For complete type isolation where you want to:

- Hide the base type's methods
- Control exactly which operations are allowed
- Have different runtime behavior

Use a struct wrapper instead:

```wado
struct UserId {
    value: i32,
}

impl UserId {
    fn new(value: i32) -> UserId {
        return UserId { value };
    }

    fn get(&self) -> i32 {
        return self.value;
    }
}
```

## Consequences

### Positive

1. **Type safety**: Prevents mixing incompatible types that share representation
2. **Zero cost**: No runtime overhead, same Wasm output
3. **Ergonomic**: Methods and operators work naturally
4. **Extensible**: Can add newtype-specific methods via `impl`
5. **Consistent with WASI**: Types like `Instant` and `Duration` become truly distinct

### Negative

1. **More verbose**: Need explicit casts where implicit conversion worked before
2. **Method return types**: Substitution may be surprising (returns newtype, not base)

## Comparison with Other Languages

| Language   | `type T = U` Semantics                    |
| ---------- | ----------------------------------------- |
| Rust       | Alias (use `struct T(U)` for newtype)     |
| Haskell    | `type` = alias, `newtype` = newtype       |
| Go         | Distinct type (like this proposal)        |
| TypeScript | Alias (use branded types for distinction) |
| **Wado**   | **Newtype (this proposal)**               |

## TODO

- [ ] Generic newtypes (`type MyArray<T> = List<T>`)
- [ ] Return type substitution for generic containers (`Option<Base>` → `Option<Newtype>` when calling inherited method on newtype)
- [x] Trait bounds with newtypes (`fn compare<T: Ord>(a: T, b: T)` with newtype) — implemented
- [x] Methods on primitive newtypes (`impl UserId { ... }` where `type UserId = i32`) — implemented
- [x] `List<Newtype>.sort()` via Ord inheritance — implemented

## See Also

- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) - struct wrapper alternative
- [Literal Type Conversion Rules](./wep-2026-01-12-literal-type-conversion.md) - literal coercion
