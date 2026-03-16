# Research: From/Into Conversion Trait Framework

Survey of type conversion trait designs, with Rust's `From`/`Into` as the central case study.
Background for a potential Wado WEP on structured type conversions.

## Motivation

Wado currently supports:

- **`as` casts** — explicit primitive-to-primitive and newtype conversions
- **Literal coercion** — numeric/string literals adapt to the target type context
- **`null` → `Option<T>`** coercion
- **Builder traits** — `SequenceLiteralBuilder`, `KeyValueLiteralBuilder` for collection literals
- **`FromIterator`** — collecting iterators into collections

There is no general-purpose mechanism for converting between arbitrary user-defined types.
A structured conversion framework would enable:

1. Ergonomic APIs that accept related types (e.g., `String` where `&str` suffices)
2. Error type composition (prerequisite for the `?` operator)
3. Newtype interop beyond `as` casts
4. Standardized patterns for library authors

---

## Rust — From / Into / TryFrom / TryInto

### Design

Rust's conversion framework (RFC 529, stabilized in Rust 1.0) consists of four traits in
`std::convert`:

```rust
trait From<T> {
    fn from(value: T) -> Self;
}

trait Into<T> {
    fn into(self) -> T;
}

trait TryFrom<T> {
    type Error;
    fn try_from(value: T) -> Result<Self, Self::Error>;
}

trait TryInto<T> {
    type Error;
    fn try_into(self) -> Result<T, Self::Error>;
}
```

Key relationships:

- **Blanket impl**: `impl<T, U> Into<U> for T where U: From<T>` — implementing `From`
  automatically provides `Into`.
- **Reflexive impl**: `impl<T> From<T> for T` — every type can convert from itself. This
  enables `fn new(s: impl Into<String>)` to accept both `String` and `&str`.
- **Try variants** (stabilized in Rust 1.34): fallible conversions returning `Result`.

In addition, Rust has borrowing conversion traits:

- **`AsRef<T>`**: cheap reference-to-reference conversion (e.g., `String` → `&str`)
- **`AsMut<T>`**: mutable reference-to-reference conversion
- **`Borrow<T>`**: like `AsRef` but with a hash/eq consistency contract

### Usage Patterns

**Pattern 1: Flexible function parameters**

```rust
fn greet(name: impl Into<String>) {
    let name: String = name.into();
    println!("Hello, {}!", name);
}
greet("world");         // &str → String via From
greet(String::from("world")); // String → String via reflexive From
```

**Pattern 2: Error type composition with `?`**

```rust
impl From<io::Error> for AppError { ... }
impl From<ParseIntError> for AppError { ... }

fn process() -> Result<(), AppError> {
    let data = std::fs::read_to_string("file.txt")?;  // io::Error → AppError
    let n: i32 = data.trim().parse()?;                 // ParseIntError → AppError
    Ok(())
}
```

The `?` operator calls `From::from()` to convert the error type, making `From` the backbone
of Rust's error handling ergonomics.

**Pattern 3: Newtype wrapping**

```rust
struct UserId(u64);
impl From<u64> for UserId {
    fn from(id: u64) -> Self { UserId(id) }
}
let id: UserId = 42u64.into();
```

### Pros

1. **Standardization**: all Rust libraries agree on a single conversion protocol. No ad hoc
   `from_foo()` methods proliferating across crates.

2. **Composability with `?`**: `From` impls compose with the `?` operator, enabling clean
   error propagation without manual matching.

3. **Generic API ergonomics**: `impl Into<T>` as a parameter bound lets functions accept
   multiple input types while remaining type-safe.

4. **Zero-cost abstraction**: monomorphized at compile time — no dynamic dispatch overhead.

5. **Discoverability**: a well-known trait that IDE tooling can index. "What can I convert
   this type to/from?" becomes answerable via trait impls.

6. **Separation of fallible/infallible**: `From` guarantees infallible conversion, while
   `TryFrom` makes fallibility explicit in the type system.

### Cons

1. **Type inference failures with `.into()`**

   The target type of `Into` is on the trait (`Into<T>`), not the method. Turbofish cannot
   be used: `.into::<String>()` is a compile error. The caller must provide type context
   through variable annotations or other means.

   ```rust
   // Fails: cannot infer type
   let x = some_value.into();
   // Must write:
   let x: String = some_value.into();
   // Or use From directly:
   let x = String::from(some_value);
   ```

   This asymmetry is a frequent source of confusion. `From::from()` is often preferred for
   readability precisely because the target type is visible at the call site.

2. **Readability and code navigation**

   `.into()` hides the target type, making it harder to understand code without IDE support.
   "Go to definition" on `.into()` leads to the blanket impl, not the actual conversion
   logic. This is a well-known complaint in medium-to-large Rust codebases.

3. **Hidden performance costs**

   The compiler has no knowledge of the performance characteristics of a given `impl From`.
   It might require allocation, syscalls, or I/O. Making conversions syntactically cheap
   (`.into()`) can mask expensive operations.

4. **Orphan rule complications**

   Before Rust 1.41, if neither the source nor target type was local, you couldn't implement
   `From`. This forced some users to implement `Into` directly, which doesn't provide the
   reverse `From` impl — creating an asymmetric situation. The orphan rules still limit
   cross-crate conversion definitions.

5. **Trait family confusion**

   Users must choose between `From`/`Into`, `AsRef`/`AsMut`, `Borrow`, `Deref`, and `ToOwned`.
   The distinctions are subtle:

   | Trait      | Semantics                          | When to use                    |
   |------------|------------------------------------|--------------------------------|
   | `From<T>`  | Owned → owned (consuming)          | Type construction              |
   | `AsRef<T>` | Borrowed → borrowed (cheap)        | Read-only access               |
   | `Borrow<T>`| Like AsRef + hash/eq contract      | HashMap keys                   |
   | `Deref`    | Smart pointer transparency         | Wrapper types                  |
   | `ToOwned`  | Borrowed → owned (cloning)         | `&str` → `String`             |

   This proliferation is a common complaint. New Rust users struggle to know which trait to
   implement for their conversion.

6. **Not dyn-compatible (not object-safe)**

   `From` cannot be used as a trait object (`dyn From<T>`), limiting its use in dynamic
   dispatch scenarios.

7. **Effect system limitations**

   `From`/`Into` cannot be made async or fallible (beyond `TryFrom`). In an effect-generic
   future, you'd need separate `AsyncFrom`, `StreamFrom`, etc. — an exponential explosion
   of trait variants.

8. **Semantic guarantees are conventions only**

   The documentation states conversions should be "infallible," "value-preserving," and
   "obvious" — but the compiler cannot enforce these. Misuse (e.g., lossy conversions via
   `From`) is possible and occurs in practice.

### Community Opinions

The Rust community is broadly positive about `From`/`Into` as a framework, but has specific
recurring complaints:

- **Reddit/forums**: ".into() is write-only code" is a common refrain. Experienced users
  often prefer `Type::from(x)` for clarity.

- **"What is wrong with auto .into?"** (internals.rust-lang.org, 2022): A highly-discussed
  thread proposing automatic `.into()` calls. Arguments against centered on hidden performance
  costs and type inference complications. Even the author of the `auto_into` proc-macro crate
  concluded it was "an anti-pattern."

- **"Implicit into() on return"** (internals.rust-lang.org, 2022): Proposed that return
  statements automatically apply `.into()` when types mismatch. Received pushback on
  explicitness grounds.

- **Effective Rust** (David Drysdale): Recommends preferring `From`/`Into` over `as` casts,
  and `TryFrom`/`TryInto` over potentially lossy `as`. Notes the "inner function" pattern
  to avoid generic code bloat.

---

## Scala — Implicit Conversions → given/using (Scala 3)

### Design

Scala 2 had `implicit def` for automatic type conversions:

```scala
implicit def intToString(x: Int): String = x.toString
val s: String = 42  // compiler inserts intToString(42)
```

Scala 3 replaced this with the `Conversion` type class and `given`/`using` keywords:

```scala
given Conversion[Int, String] = _.toString
```

### What Went Wrong

Scala's implicit conversions are widely considered the language's biggest design mistake:

1. **Invisible behavior**: Conversions happen silently with no syntactic marker. Reading code
   reveals no hint that a conversion is occurring.

2. **Non-total conversions**: Nothing prevents defining a conversion from `String` to `Int`
   that throws `NumberFormatException` at runtime. The type system says it's safe; it isn't.

3. **Import-triggered surprises**: Importing a module can silently change the behavior of
   existing code by bringing new implicit conversions into scope.

4. **Poor error messages**: When implicit resolution fails, the compiler reports generic type
   mismatches rather than explaining which conversion was expected.

5. **IDE masking**: IDEs automatically suppress compiler warnings about implicit conversions,
   undermining the language's own safety mechanisms.

6. **Debugging difficulty**: In large codebases, tracking which implicit conversion is being
   applied at a given call site requires deep understanding of the implicit scope rules.

### Lessons for Wado

- **Implicit conversions without syntactic markers are harmful.** Scala 3 effectively admits
  this by deprecating the old approach.
- **Conversions should be explicitly requested at the call site** — either through `.into()`,
  `as`, or at minimum through a visible trait bound in the function signature.
- **Non-total conversions need a separate type** (like Rust's `TryFrom` returning `Result`).

---

## Swift — ExpressibleBy Protocols

### Design

Swift uses protocol conformance for literal-to-type conversion:

```swift
protocol ExpressibleByStringLiteral {
    init(stringLiteral value: String)
}

struct UserID: ExpressibleByStringLiteral {
    let raw: String
    init(stringLiteral value: String) { self.raw = value }
}

let id: UserID = "abc123"  // compiler calls init(stringLiteral:)
```

For non-literal conversions, Swift relies on explicit initializers:

```swift
let x = Double(42)  // explicit, not implicit
let s = String(describing: someValue)
```

### Strengths

- **Literals are ergonomic**: Custom types feel native when they can be expressed as literals.
- **Non-literal conversions are explicit**: No hidden `.into()` — you call an initializer.
- **Progressive disclosure**: Simple code stays simple; conversion machinery only appears
  when needed.

### Weaknesses

- **Limited scope**: Only works for literals, not arbitrary value-to-value conversion.
- **No general conversion trait**: Swift has no equivalent of `From`/`Into` for non-literal
  contexts.

### Relevance to Wado

Wado's existing `SequenceLiteralBuilder` and `KeyValueLiteralBuilder` are conceptually similar
to Swift's `ExpressibleBy` protocols — they handle literal-to-type conversion. A `From`/`Into`
framework would complement this by handling non-literal conversions.

---

## C++ — Implicit Conversion Operators

### Design

C++ allows implicit conversions through converting constructors and conversion operators:

```cpp
class MyString {
    MyString(const char* s);        // implicit converting constructor
    operator int() const;           // implicit conversion to int
    explicit operator bool() const; // explicit only
};
```

### Problems (Well-documented)

1. **Silent, surprising conversions**: `MyString s = "hello"; int n = s;` compiles silently.
2. **Ambiguity in overload resolution**: Multiple implicit conversion paths create compilation
   errors or, worse, select the wrong overload.
3. **The `explicit` keyword** was added precisely because implicit conversions caused too many
   bugs. Modern C++ style guidelines recommend making all single-argument constructors
   `explicit` by default.
4. **Performance**: Temporary objects created by implicit conversions are a hidden cost.

Google's C++ style guide and many corporate guidelines forbid implicit conversions entirely.

### Lessons for Wado

C++ is the strongest cautionary tale: **implicit conversions without opt-in at the call site
are a proven source of bugs.** The `explicit` keyword was a retroactive fix for a design
mistake.

---

## Go — Explicit Conversions Only

### Design

Go has no implicit conversions whatsoever. Even `int32` → `int64` requires an explicit cast:

```go
var x int32 = 42
var y int64 = int64(x)  // must be explicit
```

### Rationale

Go's designers explicitly chose this to avoid the "complexity and confusion" that implicit
conversions cause in C/C++. The FAQ states: "The convenience of automatic conversion between
numeric types in C is outweighed by the confusion it causes."

### Strengths

- **No surprises**: Code does exactly what it says.
- **Simple mental model**: No conversion rules to memorize.

### Weaknesses

- **Verbose**: Numeric code becomes cluttered with casts.
- **No generic conversion protocol**: No way to express "any type convertible to X" as a
  bound.

---

## Haskell — Type Classes

### Design

Haskell uses type classes for conversion, but there is no single unified `From`/`Into`:

```haskell
class Num a where
    fromInteger :: Integer -> a

class Real a => Integral a where ...
fromIntegral :: (Integral a, Num b) => a -> b
```

Conversions are always explicit function calls, but polymorphic literals (like `42`) are
implicitly converted via `fromInteger`.

### Strengths

- **Polymorphic literals**: `42` works as any `Num` type without explicit casts.
- **Type class coherence**: Global uniqueness of instances prevents ambiguity.

### Weaknesses

- **No unified conversion trait**: `fromIntegral`, `toInteger`, `fromRational` are all
  separate functions with no shared structure.
- **Partial functions**: `fromIntegral` can overflow silently (no `TryFrom` equivalent in
  base).

---

## Python — Dunder Methods

### Design

Python uses special methods for type conversion:

```python
class Celsius:
    def __init__(self, value): self.value = value
    def __float__(self): return self.value
    def __int__(self): return int(self.value)
    def __str__(self): return f"{self.value}°C"
```

Conversions are explicit at the call site: `float(celsius)`, `int(celsius)`, `str(celsius)`.

### Strengths

- **Explicit call sites**: `float(x)` is clear about what's happening.
- **Standardized protocol**: Well-known dunder methods that all Python developers understand.

### Weaknesses

- **No static type checking**: Wrong dunder method signatures are discovered at runtime.
- **Limited to built-in types**: No way to define `__mytype__` for custom-to-custom conversion.

---

## Zig — @as and Explicit Philosophy

### Design

Zig uses `@as` for explicit type coercion and has a very limited set of implicit coercions
(mainly for comptime-known values):

```zig
const x: u32 = @as(u32, some_u16);
```

### Philosophy

Zig follows Go's approach of minimal implicit behavior, preferring explicit conversions. The
language deliberately avoids traits/interfaces for conversion, relying on explicit function
calls.

---

## Kotlin — Extension Functions and Smart Casts

### Design

Kotlin uses explicit conversion methods (`toInt()`, `toString()`, etc.) and extension
functions for custom conversions:

```kotlin
fun String.toUserId(): UserId = UserId(this)
val id = "abc123".toUserId()
```

Smart casts narrow types after `is` checks but don't perform value conversion.

### Strengths

- **Discoverable**: `.to*()` methods are visible in autocomplete.
- **Explicit**: No hidden conversions.
- **Extensible**: Extension functions allow adding conversions without modifying types.

### Weaknesses

- **No generic bound**: Cannot express "any type convertible to X" in a generic context.
- **Naming convention only**: No compiler-enforced trait/interface for conversions.

---

## Cross-Language Summary

| Language  | Mechanism              | Implicit? | Generic bound? | Fallible variant? |
|-----------|------------------------|-----------|----------------|-------------------|
| Rust      | `From`/`Into` traits   | No (explicit `.into()`) | Yes (`impl Into<T>`) | `TryFrom`/`TryInto` |
| Scala 2   | `implicit def`         | Yes       | Via implicits   | No (runtime exn) |
| Scala 3   | `Conversion` typeclass | Semi (requires import) | Yes | No |
| Swift     | `ExpressibleBy` + init | Literals only | No | `init?` (failable) |
| C++       | Converting constructors | Yes (unless `explicit`) | No | No |
| Go        | Explicit cast syntax   | No        | No              | No |
| Haskell   | Per-domain type classes | No        | Yes             | Partial |
| Python    | Dunder methods         | No        | No (dynamic)    | Exceptions |
| Zig       | `@as`                  | No        | No              | No |
| Kotlin    | `.to*()` extensions    | No        | No              | Nullable return |

---

## Key Takeaways for Wado

### What works well in Rust's design

1. **Trait-based conversion is the right abstraction level.** It enables generic bounds,
   compiler verification, and ecosystem-wide standardization.

2. **The `From`→`Into` blanket impl is elegant.** Users implement `From` (natural direction),
   get `Into` for free (ergonomic direction). This duality is genuinely useful.

3. **Separating infallible (`From`) and fallible (`TryFrom`) is valuable.** It encodes
   conversion safety in the type system.

4. **Integration with `?` is the killer feature.** The `From` trait's greatest value is
   enabling automatic error type conversion via `?`. Without this use case, `From`/`Into`
   would be merely convenient; with it, they're essential.

### What to improve or reconsider

1. **The `.into()` inference problem needs addressing.** The inability to turbofish `.into()`
   is a fundamental ergonomic issue. Possible solutions:
   - Make the target type a method parameter: `fn into<T>(self) -> T`
   - Prefer `Type::from(x)` style (which Wado can optimize for)
   - Provide syntax sugar that makes the target type visible

2. **The trait family is too large.** Rust has `From`, `Into`, `TryFrom`, `TryInto`, `AsRef`,
   `AsMut`, `Borrow`, `BorrowMut`, `ToOwned`, `Deref`, `DerefMut` — all related to "getting
   one type from another." Wado should aim for fewer, more orthogonal traits.

3. **Wado's GC simplifies the picture.** Rust needs `AsRef`/`Borrow`/`Deref` because of
   ownership and borrowing semantics. With GC-based memory management, Wado doesn't need
   most of these. The core need is:
   - **Value conversion** (consuming): `From<T>` / `Into<T>`
   - **Fallible conversion**: `TryFrom<T>` / `TryInto<T>`
   - Reference-based conversions (`AsRef`) may be less critical.

4. **Implicit vs. explicit is a spectrum.** The consensus across languages is:
   - Fully implicit (Scala 2, C++) → proven harmful
   - Fully explicit (Go) → safe but verbose
   - Explicit with trait-based generic bounds (Rust) → good middle ground
   - **Call-site-visible but minimal-syntax** → optimal target for Wado

5. **Wado already has literal coercion.** Wado's existing `SequenceLiteralBuilder` and
   numeric literal coercion handle the "literal → type" case (like Swift's `ExpressibleBy`).
   A `From`/`Into` framework would handle the "value → value" case, complementing what exists.

6. **The `?` operator dependency.** If Wado plans to implement the `?` operator (listed as
   "not yet implemented" in the spec), `From` is a prerequisite. The design of `From` and `?`
   should be co-designed.

7. **Effect interaction.** Wado has an effect system. A conversion trait should consider
   whether conversions can have effects (e.g., a conversion that requires I/O). This is
   something Rust cannot express.

---

## Open Questions for Wado

1. **Should Wado have both `From` and `Into`, or just one?** The blanket impl trick works
   but adds complexity. An alternative: a single `Convert<From, To>` trait, or just `From`
   with compiler-provided `.into()` sugar.

2. **Should `.into()` be a method or syntax sugar?** If it's syntax sugar (e.g., `expr as T`
   calling `From::from`), the turbofish problem disappears.

3. **How does this interact with newtypes?** Currently newtypes use `as` for conversion.
   Should `From` be auto-derived for newtypes, or should `as` remain the only mechanism?

4. **Should conversions support effects?** e.g., `fn from(value: T) -> Self with FileSystem`

5. **What about the `?` operator?** If Wado implements `?`, the error conversion mechanism
   needs to be designed together with `From`.

6. **How many conversion traits?** Possible minimal set:
   - `From<T>` — infallible value conversion
   - `TryFrom<T>` — fallible value conversion (returns `Result`)
   - Compiler-provided `.into()` sugar that calls `From::from()`

## References

- [RFC 529: Conversion Traits](https://rust-lang.github.io/rfcs/0529-conversion-traits.html)
- [RFC 401: Coercions](https://github.com/rust-lang/rfcs/blob/master/text/0401-coercions.md)
- [Effective Rust — Item 5: Understand Type Conversions](https://effective-rust.com/casts.html)
- ["What is wrong with auto .into?"](https://internals.rust-lang.org/t/what-is-wrong-with-auto-into/17319) — Rust Internals
- ["Implicit into() on return"](https://internals.rust-lang.org/t/language-tidy-up-feature-2-implicit-into-on-return/17872) — Rust Internals
- ["Traits as Implicit Conversions"](https://slightknack.dev/passerine/traits-as/) — Isaac Clayton
- [Scala 3 Implicit Redesign](https://www.baeldung.com/scala/scala-3-implicit-redesign) — Baeldung
- ["Can We Wean Scala Off Implicit Conversions?"](https://contributors.scala-lang.org/t/can-we-wean-scala-off-implicit-conversions/4388) — Scala Contributors
- [Scala Best Practices: Avoid Implicit Conversions](https://nrinaudo.github.io/scala-best-practices/unsafe/implicit_conversions.html)
- [Swift ExpressibleBy Protocols Internals](https://swiftrocks.com/swift-expressibleby-protocols-how-they-work-internally-in-the-compiler)
- [Rust API Design: AsRef, Into, Cow](https://www.philipdaniels.com/blog/2019/rust-api-design/)
- ["From & Into Confusion — Why Do We Need Both?"](https://users.rust-lang.org/t/from-into-confusion-why-do-we-need-both/80181) — Rust Users Forum
- [Implicit Numeric Widening Proposal](https://internals.rust-lang.org/t/implicit-numeric-widening-coercion-proposal/23660) — Rust Internals
