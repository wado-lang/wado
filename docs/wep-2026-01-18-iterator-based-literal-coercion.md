# WEP: Literal-to-Collection Coercion

## Context

Wado allows object and sequence literals to be coerced to collection types in certain
contexts, via a trait-based mechanism that is extensible to user-defined collections:

```wado
let d: TreeMap<String, i32> = { width: 1920, height: 1080 };  // KeyValueLiteral
let a: Array<i32>            = [1, 2, 3];                      // SequenceLiteral
```

The design must:

1. Be extensible to any user-defined key-value store or sequence type
2. Support immutable output types (builder and output are distinct types)
3. Allow most types to use themselves as their own builder with minimal boilerplate
4. Be type-safe: concrete type positions in impl blocks are correctly validated

## Decision

### 1. Literal Syntax

| Literal         | Default type        | Coercion target   |
| --------------- | ------------------- | ----------------- |
| `{ k: v, ... }` | Anonymous struct    | `KeyValueLiteral` |
| `[e0, e1, ...]` | Tuple `[T, U, ...]` | `SequenceLiteral` |

Coercion is **literal-only** — it does not apply to bound variables:

```wado
let t = [1, 2, 3];           // t: [i32, i32, i32] (tuple, no coercion)
let arr: Array<i32> = t;     // ERROR: not a literal
let arr: Array<i32> = [1, 2, 3];  // OK
```

Struct literal priority: if the target type is a struct with matching fields, it is
interpreted as a struct literal and `KeyValueLiteral` coercion is **not** attempted.

### 2. KeyValueLiteralBuilder Trait

```wado
/// Accumulates key-value pairs and finalizes into the output type.
/// Implement this to make a type constructible from `{ key: value, ... }` literals.
pub trait KeyValueLiteralBuilder {
    type Value;
    type Output;

    /// Create an empty builder, pre-allocated for `capacity` entries.
    fn new_literal(capacity: i32) -> Self;

    /// Record one field from the literal.
    fn insert_literal(&mut self, key: String, value: Self::Value);

    /// Consume the builder and return the finished value.
    fn build(self) -> Self::Output;
}
```

### 3. KeyValueLiteral Trait

```wado
/// A type constructible from a key-value literal.
/// Declares which builder produces it.
pub trait KeyValueLiteral {
    type Value;
    type Builder: KeyValueLiteralBuilder<Value = Self::Value, Output = Self>;
}

// Blanket: any type that builds itself satisfies KeyValueLiteral automatically.
// (Requires associated type projection on type params — T::Value — in the elaborator.)
impl<T: KeyValueLiteralBuilder<Output = T>> KeyValueLiteral for T {
    type Value = T::Value;
    type Builder = T;
}
```

### 4. SequenceLiteralBuilder Trait

```wado
/// Accumulates positional elements and finalizes into the output type.
/// Implement this to make a type constructible from `[e0, e1, ...]` literals.
pub trait SequenceLiteralBuilder {
    type Element;
    type Output;

    /// Create an empty builder, pre-allocated for `capacity` elements.
    fn new_literal(capacity: i32) -> Self;

    /// Append one element from the literal.
    fn push_literal(&mut self, value: Self::Element);

    /// Consume the builder and return the finished value.
    fn build(self) -> Self::Output;
}
```

### 5. SequenceLiteral Trait

```wado
/// A type constructible from a sequence literal.
/// Declares which builder produces it.
pub trait SequenceLiteral {
    type Element;
    type Builder: SequenceLiteralBuilder<Element = Self::Element, Output = Self>;
}

// Blanket: any type that builds itself satisfies SequenceLiteral automatically.
impl<T: SequenceLiteralBuilder<Output = T>> SequenceLiteral for T {
    type Element = T::Element;
    type Builder = T;
}
```

### 6. Compiler Desugaring

```wado
// Source
let m: T = { a: 1, b: 2 };

// Desugared
let m: T = {
    let mut __b: T::Builder = T::Builder::new_literal(2);
    __b.insert_literal("a", 1);
    __b.insert_literal("b", 2);
    break __kv_lit: __b.build();
};
```

```wado
// Source
let s: T = [e0, e1, e2];

// Desugared
let s: T = {
    let mut __b: T::Builder = T::Builder::new_literal(3);
    __b.push_literal(e0);
    __b.push_literal(e1);
    __b.push_literal(e2);
    break __seq_lit: __b.build();
};
```

The `capacity` argument equals the number of literal fields/elements, known at
compile time. When `T::Builder = T` (self-as-builder), `build()` is the identity
and the compiler inlines it away.

### 7. Coercion Rules

#### Object literals

When the compiler sees `{ k0: v0, ... }` targeting type `T: KeyValueLiteral<Value = V>`:

1. Verify all value types match `V`
2. Detect and report duplicate keys as a compile error
3. Desugar via `T::Builder`

#### Sequence literals

When the compiler sees `[e0, e1, ...]` targeting type `T: SequenceLiteral<Element = E>`:

1. Verify all element types match `E`
2. Desugar via `T::Builder`

#### Concrete type positions in impl

A known type (struct, enum, variant, flags, newtype, primitive) in a generic
position of an impl is treated as a concrete constraint, not a free type parameter.
This includes nested generic types (e.g. `Array<String>`):

```wado
enum Direction { North, South }
struct DirMap<D, V> { keys: Array<String>, values: Array<V> }

impl KeyValueLiteralBuilder for DirMap<Direction, V> {
    type Value = V;
    type Output = DirMap<Direction, V>;
    fn new_literal(capacity: i32) -> Self { ... }
    fn insert_literal(&mut self, key: String, value: V) { ... }
    fn build(self) -> Self { return self; }
}

let m: DirMap<Direction, i32> = { north: 1 };  // OK
let m: DirMap<String, i32>    = { a: 1 };       // ERROR: String != Direction

// Nested generic type in concrete position
struct NestedMap<K, V> { ... }

impl KeyValueLiteralBuilder for NestedMap<Array<String>, V> {
    type Value = V;
    type Output = NestedMap<Array<String>, V>;
    ...
}

let m: NestedMap<Array<String>, i32> = { a: 1 };   // OK
let m: NestedMap<Array<i32>, i32>    = { a: 1 };   // ERROR: Array<i32> != Array<String>
```

The validation is recursive: `impl_type_matches_concrete` descends into nested
generic args to ensure every concrete position matches exactly.

### 8. Coercion Contexts

Coercion applies wherever the target type is known:

1. Variable initialization: `let m: T = { ... };`
2. Function argument: `f({ a: 1 })`
3. Explicit cast: `{ a: 1 } as T`
4. Return value: `fn f() -> T { return { a: 1 }; }`
5. Conditional branches: `let x: T = if c { { a: 1 } } else { { b: 2 } };`

### 9. Self-as-Builder (Common Case) — One `impl` Block

For mutable types, implement `KeyValueLiteralBuilder` with `Output = Self` and a
trivial `build`. The `KeyValueLiteral` impl is provided for free by the blanket.

```wado
impl KeyValueLiteralBuilder for TreeMap<String, V> {
    type Value = V;
    type Output = TreeMap<String, V>;  // Output = Self → blanket covers KeyValueLiteral

    fn new_literal(capacity: i32) -> Self {
        return TreeMap::<String, V>::new();
    }

    fn insert_literal(&mut self, key: String, value: V) {
        self.insert(key, value);
    }

    fn build(self) -> Self {
        return self;
    }
}
// No explicit impl KeyValueLiteral needed — covered by the blanket.
```

Same pattern for `SequenceLiteralBuilder`:

```wado
impl SequenceLiteralBuilder for Array<T> {
    type Element = T;
    type Output = Array<T>;

    fn new_literal(capacity: i32) -> Self {
        return Array::<T>::with_capacity(capacity);
    }

    fn push_literal(&mut self, value: T) {
        self.push(value);
    }

    fn build(self) -> Self {
        return self;
    }
}
```

### 10. Separate Builder (Immutable Output) — Two `impl` Blocks

For immutable output types, provide a mutable builder struct and point `KeyValueLiteral`
at it:

```wado
struct FrozenMap<V> { ... }  // No insert/mutation methods exposed

struct FrozenMapBuilder<V> {
    inner: TreeMap<String, V>,
}

impl KeyValueLiteralBuilder for FrozenMapBuilder<V> {
    type Value = V;
    type Output = FrozenMap<V>;

    fn new_literal(capacity: i32) -> Self {
        return FrozenMapBuilder { inner: TreeMap::<String, V>::new() };
    }

    fn insert_literal(&mut self, key: String, value: V) {
        self.inner.insert(key, value);
    }

    fn build(self) -> FrozenMap<V> {
        return FrozenMap::from_tree_map(self.inner);
    }
}

impl KeyValueLiteral for FrozenMap<V> {
    type Value = V;
    type Builder = FrozenMapBuilder<V>;
}
```

### 11. JSONValue Example (Both Traits)

```wado
pub variant JSONValue {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Array<JSONValue>),
    Object(TreeMap<String, JSONValue>),
}

impl KeyValueLiteralBuilder for JSONValue {
    type Value = JSONValue;
    type Output = JSONValue;

    fn new_literal(capacity: i32) -> Self {
        return JSONValue::Object(TreeMap::<String, JSONValue>::new());
    }

    fn insert_literal(&mut self, key: String, value: JSONValue) {
        if let Object(map) = self { map.insert(key, value); }
    }

    fn build(self) -> Self { return self; }
}

impl SequenceLiteralBuilder for JSONValue {
    type Element = JSONValue;
    type Output = JSONValue;

    fn new_literal(capacity: i32) -> Self {
        return JSONValue::Array(Array::<JSONValue>::with_capacity(capacity));
    }

    fn push_literal(&mut self, value: JSONValue) {
        if let Array(arr) = self { arr.push(value); }
    }

    fn build(self) -> Self { return self; }
}
```

With both builder traits (and blanket impls), JSONValue supports nested literals:

```wado
let data: JSONValue = {
    "name": "Alice",
    "scores": [10, 20, 30],       // SequenceLiteral → JSONValue::Array
    "meta": { "active": true },   // KeyValueLiteral → JSONValue::Object
};
```

### 12. Relationship with Iterator Traits

| Mechanism                  | Purpose                       | Hetero elements |
| -------------------------- | ----------------------------- | --------------- |
| **KeyValueLiteralBuilder** | Object literal → collection   | Future          |
| **SequenceLiteralBuilder** | Sequence literal → collection | Future          |
| **FromIterator**           | Iterator → collection         | No (homo only)  |
| **IntoIterator**           | Collection → iterator         | N/A             |

Heterogeneous element coercion (e.g., `[1, "hello", true]` → `JSONValue::Array`)
is deferred: it requires an `Into<E>` conversion per element.

## Consequences

### Positive

1. **Self-as-builder is zero extra work**: implementing `KeyValueLiteralBuilder` is
   sufficient — no explicit `impl KeyValueLiteral` block needed. The coercion resolves
   `KeyValueLiteralBuilder` directly, and the blanket impl satisfies any
   `T: KeyValueLiteral` bound automatically.
2. **Blanket impl works**: the `impl<T: KeyValueLiteralBuilder<Output = T>> KeyValueLiteral for T`
   blanket compiles and is active. Associated type projection on type params (`T::Value`)
   is supported by the elaborator.
3. **Immutable output types**: the two-trait split makes them first-class without complicating
   the simple path
4. **Capacity hint**: `new_literal(capacity)` allows pre-allocation; the compiler always
   knows the count at compile time
5. **Extensible**: any type — user-defined or standard — can implement either trait
6. **Compile-time only**: all expansion happens at compile time; no runtime overhead
7. **Key position safety**: concrete types in impl positions are correctly validated,
   including nested generics (e.g. `Array<String>` in `impl Trait for Foo<Array<String>, V>`)

### Negative

1. **Heterogeneous elements deferred**: both traits currently require a uniform
   `Value`/`Element` type.

2. **Non-String keys not supported**: `insert_literal` takes `key: String`, so all
   literal keys must be plain identifiers (written as strings by the compiler). Typed
   or computed keys — e.g., an enum discriminant or an integer — are not supported.
   When this feature is added, the syntax will follow JavaScript's computed-property
   notation:

   ```wado
   let m: Map<Color, i32> = { [Color::Red]: 1, [Color::Blue]: 2 };
   ```

   Until then, use explicit insertion calls instead.

## Related WEPs

- [Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)
- [Iterator Traits Design](./wep-2026-01-24-iterator-traits.md)
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Associated Types in Traits](./wep-2026-01-20-associated-types.md)
