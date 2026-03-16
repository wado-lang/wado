# WEP: Conversion Traits (From, TryFrom, ? operator)

## Context

Wado currently has two conversion mechanisms:

- **`as` casts** — explicit primitive-to-primitive (e.g., `i32` → `f64`, `char` → `i32`) and newtype casts (e.g., `Meters` → `f64`)
- **Literal coercion** — numeric and string literals adapt to the target type context (e.g., `let x: i64 = 42`)

There is no general-purpose mechanism for converting between user-defined types, and the `?` operator (already reserved in the spec) has no implementation. The two key motivations for this WEP are:

1. **Error type composition**: Enable the `?` operator for ergonomic error propagation with automatic error conversion
2. **Structured conversions**: Provide a standardized protocol for user-defined type conversions

### Language Survey

See [Research: From/Into Conversion Trait Framework](./research-from-into-framework.md) for the full survey. Key findings:

| Approach | Languages | Outcome |
|----------|-----------|---------|
| Implicit conversions | C++, Scala 2 | Universally regretted; causes bugs, hides costs |
| Fully explicit | Go, Zig | Safe but verbose; no generic conversion protocol |
| Trait-based, call-site-explicit | Rust | Good balance; `From`/`?` integration is the standout success |
| Literal coercion only | Swift | Elegant for literals, insufficient for value-to-value |

Rust's `From`/`Into` framework is the primary reference design. Its strengths: standardized conversion protocol, `?` operator integration, separation of infallible/fallible. Its weaknesses:

- **`.into()` hides the target type** — turbofish doesn't work, IDE "go to definition" leads to blanket impl
- **`Into<T>` exists mainly for orphan rule workarounds** — fixed in Rust 1.41
- **`impl Into<T>` parameters cause monomorphization bloat** — the `momo` crate exists to mitigate this
- **Too many conversion traits** — `From`, `Into`, `TryFrom`, `TryInto`, `AsRef`, `AsMut`, `Borrow`, `Deref`, `ToOwned` confuse users

### Design Goals

1. Support the `?` operator with `From`-based error conversion
2. Keep target types visible at every call site
3. Minimize the number of conversion traits (Wado's GC eliminates the need for `AsRef`/`Borrow`/`Deref`)
4. Support compiler-generated impls for variants and newtypes
5. No implicit conversions beyond existing literal coercion

## Decision

### 1. `From<T>` Trait

Infallible conversion trait in `core:prelude`:

```wado
#[comp_feature("from")]
pub trait From<T> {
    fn from(value: T) -> Self;
}
```

Usage is always explicit via `Type::from(value)`:

```wado
impl From<i32> for String {
    fn from(value: i32) -> String {
        return `{value}`;
    }
}

let s = String::from(42);  // "42"
```

The `#[comp_feature("from")]` attribute registers `From` in the compiler's `TypeTable`, enabling the `?` operator to look up `From` impls for error conversion.

**Reflexive impl**: The compiler provides `impl From<T> for T` for all types (identity conversion). This is needed for `?` to work when the error type already matches.

### 2. `TryFrom<T>` Trait

Fallible conversion trait returning `Result`:

```wado
pub trait TryFrom<T> {
    type Error;
    fn try_from(value: T) -> Result<Self, Self::Error>;
}
```

```wado
impl TryFrom<i64> for u8 {
    type Error = String;
    fn try_from(value: i64) -> Result<u8, String> {
        if value < 0 || value > 255 {
            return Result::<u8, String>::Err(`{value} out of range for u8`);
        }
        return Result::<u8, String>::Ok(value as u8);
    }
}

let n = u8::try_from(42 as i64)?;  // Ok(42)
let n = u8::try_from(999 as i64)?; // Err("999 out of range for u8")
```

### 3. `?` Operator

The `?` postfix operator (already reserved in the spec's operator table) provides early return for `Result` and `Option`.

**On `Result<T, E>`**: Unwrap `Ok(v)` → `v`, or convert the error and return early:

```wado
// Desugars to:
// match expr {
//     Ok(v) => v,
//     Err(e) => { return Result::Err(From::from(e)); },
// }

fn process() -> Result<i32, AppError> {
    let data = read_file("input.txt")?;   // IoError → AppError via From
    let n = parse_int(data)?;              // ParseError → AppError via From
    return Result::<i32, AppError>::Ok(n);
}
```

The error conversion `From::from(e)` is the key integration point: `?` calls `From` to convert the inner error type to the enclosing function's error type.

**On `Option<T>`**: Unwrap `Some(v)` → `v`, or return `null` (which is `Option::None`):

```wado
// Desugars to:
// match expr {
//     Some(v) => v,
//     None => { return null; },
// }

fn find_user_name(id: i32) -> Option<String> {
    let user = find_user(id)?;        // None → return null
    let profile = user.profile()?;    // None → return null
    return Option::<String>::Some(profile.name);
}
```

**Requirements**:
- The enclosing function must return `Result<T, E>` (for `Result` operand) or `Option<T>` (for `Option` operand)
- Using `?` on `Result` in a function returning `Option`, or vice versa, is a compile error
- `?` cannot be used inside closures that don't return `Result`/`Option` themselves

**Effect interaction**: The `?` operator performs early return but does not introduce effects. The function's `with` clause is unaffected.

### 4. No `Into<T>` Trait

Wado does not provide an `Into<T>` trait. The reasons:

1. **`.into()` hides the target type.** Code like `let x = value.into()` requires type context to understand. `String::from(value)` is always clear.

2. **Orphan rules make `Into` unnecessary.** Rust needed `Into` because pre-1.41 orphan rules prevented `impl From<ForeignType> for LocalType` in some cases. Wado uses Rust 1.41+ style orphan rules where having a local type in *any* trait parameter position suffices. With `From<LocalType> for ForeignType` allowed, there's no need for `Into`.

3. **`impl Into<T>` parameters cause monomorphization bloat.** Each unique caller type generates a copy of the function body. The Rust ecosystem created the `momo` crate specifically to work around this problem.

Users write `Type::from(value)` at call sites. This is slightly more verbose than `.into()` but always makes the target type visible.

### 5. No `as` Extension for `From`

The `as` operator retains its current semantics:

- **Primitive casts**: `42 as f64`, `'A' as i32`
- **Newtype casts**: `meters as f64`, `value as Meters`

`as` does **not** call `From::from()`. The semantic distinction:

| Mechanism | Semantics | Cost |
|-----------|-----------|------|
| `as` | Bit reinterpretation / cast | Zero-cost, always succeeds |
| `From::from()` | Semantic conversion | May allocate, always succeeds |
| `TryFrom::try_from()` | Fallible conversion | May allocate, may fail |

This keeps `as` simple and predictable. Developers know `x as T` is always a trivial operation.

### 6. Variant `From` Auto-Derive

Variant types can opt into compiler-generated `From` impls using the existing auto-derive syntax:

```wado
variant AppError {
    Io(IoError),
    Parse(ParseError),
    Validation(String),
}

impl From<IoError> for AppError;      // generates: AppError::Io(value)
impl From<ParseError> for AppError;   // generates: AppError::Parse(value)
impl From<String> for AppError;       // generates: AppError::Validation(value)
```

Each `impl From<T> for Variant;` (with no body) instructs the compiler to generate:

```wado
impl From<IoError> for AppError {
    fn from(value: IoError) -> AppError {
        return AppError::Io(value);
    }
}
```

This follows the existing pattern used by `impl Serialize for Type;` and `impl Deserialize for Type;`.

**Ambiguity rule**: If multiple variant cases have the same payload type, the compiler rejects the auto-derive with a clear error message. The user must write the `From` impl manually.

This pattern is especially powerful with `?`:

```wado
fn process() -> Result<String, AppError> {
    let data = read_file("input.txt")?;   // IoError → AppError::Io via From
    let parsed = parse(data)?;            // ParseError → AppError::Parse via From
    return Result::<String, AppError>::Ok(parsed);
}
```

### 7. Newtype `From` Auto-Generation

The compiler automatically generates bidirectional `From` impls for newtypes:

```wado
type UserId = u64;
// compiler auto-generates:
// impl From<u64> for UserId { fn from(value: u64) -> UserId { return value as UserId; } }
// impl From<UserId> for u64 { fn from(value: UserId) -> u64 { return value as u64; } }
```

This means both `as` and `From::from()` work for newtypes:

```wado
let id: UserId = 42;                    // literal coercion (existing)
let id2 = 42 as UserId;                 // as cast (existing)
let id3 = UserId::from(42 as u64);      // From (new, auto-generated)
let raw = u64::from(id);                // From (new, auto-generated)
```

The auto-generated `From` impls enable newtypes to participate in generic code:

```wado
fn print_id<T: From<u64>>(raw: u64) {
    let id = T::from(raw);
    println(`{id}`);
}
print_id::<UserId>(42);
```

### 8. Standard Library `From` Impls

The standard library provides `From` impls for common lossless conversions:

**Numeric widenings** (infallible, lossless):

| From | To |
|------|----|
| `i8` | `i16`, `i32`, `i64` |
| `i16` | `i32`, `i64` |
| `i32` | `i64` |
| `u8` | `u16`, `u32`, `u64`, `i16`, `i32`, `i64` |
| `u16` | `u32`, `u64`, `i32`, `i64` |
| `u32` | `u64`, `i64` |
| `f32` | `f64` |

**Other conversions**:

- `impl From<char> for String` — single character to string
- `impl From<bool> for i32` — `false` → `0`, `true` → `1`

**`TryFrom` impls** for narrowing conversions:

- `impl TryFrom<i64> for i32` — checked narrowing
- `impl TryFrom<i32> for u32` — checked sign conversion
- (and similar for other narrowing/sign combinations)

### 9. Interaction with Existing Features

| Feature | Status | Interaction |
|---------|--------|-------------|
| `as` casts | Unchanged | `as` = bit cast; `From` = semantic conversion |
| Literal coercion | Unchanged | Literals still coerce to target type without `From` |
| `null` → `Option::None` | Unchanged | Syntactic sugar, not a `From` call |
| Template strings | Unchanged | `\`{x}\`` uses `Display`, not `From` |
| Newtype `as` | Unchanged | `as` still works; `From` is additionally auto-generated |

## Consequences

### Benefits

- **Ergonomic error handling**: `?` with automatic `From` conversion eliminates manual `match` on `Result` for error propagation
- **Explicit target types**: No `.into()` means the target type is always visible at the call site
- **Minimal trait surface**: Two traits (`From`, `TryFrom`) instead of Rust's ten+ conversion traits
- **Variant error composition**: `impl From<E> for AppError;` auto-derive + `?` makes error handling concise
- **GC-simplified design**: No need for `AsRef`, `Borrow`, `Deref`, `ToOwned` — Wado's GC handles reference semantics

### Trade-offs

- **More verbose than `.into()`**: `String::from(value)` is longer than `value.into()`. This is intentional — explicitness over brevity.
- **No generic "accepts any convertible type" parameter**: Without `impl Into<T>`, functions cannot accept multiple types via a single parameter. Users call `Type::from(value)` at the call site instead. This avoids monomorphization bloat but requires the caller to be explicit.

## Future Considerations

### `into T` Operator

If the community finds `Type::from(value)` too verbose in practice, an `into T` postfix operator could be added:

```wado
let s = 42 into String;          // equivalent to String::from(42)
let id = raw_id into UserId;     // equivalent to UserId::from(raw_id)
```

This preserves target type visibility (unlike `.into()`) while being more concise than `Type::from(value)`. It is explicitly out of scope for the initial release — we start with `From::from()` and evaluate community feedback.

### `from T` Parameter Modifier

A parameter modifier that performs conversion at the call site without monomorphization:

```wado
fn greet(name: String from &str) with Stdout {
    println(`Hello, {name}!`);
}
greet("world");  // compiler inserts String::from("world") at call site
```

This is reserved as a future possibility but not included in this proposal.

## References

- [Research: From/Into Conversion Trait Framework](./research-from-into-framework.md) — background research for this WEP
- [RFC 529: Conversion Traits](https://rust-lang.github.io/rfcs/0529-conversion-traits.html)
- [RFC 1542: TryFrom](https://rust-lang.github.io/rfcs/1542-try-from.html)
- [WEP: Literal Type Conversion Rules](./wep-2026-01-12-literal-type-conversion.md)
- [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md)
- [WEP: Trait Bounds Enforcement](./wep-2026-02-07-trait-bounds.md)
- [WEP: Variant Payload Design](./wep-2026-01-25-variant-payload-design.md)
- [WEP: Operator Overloading](./wep-2026-01-18-operator-overloading.md)
- [WEP: Default Trait](./wep-2026-03-04-default-trait.md)
