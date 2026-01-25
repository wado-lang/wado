# WEP: Tuple-to-Collection Coercion

## Context

Wado allows tuple literals to be coerced to collection types in certain contexts:

```wado
let a: Array<i32> = [1, 2, 3];  // Tuple literal → Array<i32>
fn takes(arr: Array<i32>) {}
takes([1, 2, 3]);                // Implicit coercion
let b = [1, 2, 3] as Array<i32>; // Explicit cast
```

This coercion is currently **hardcoded** in the compiler. When the compiler encounters a tuple literal with a target type of `Array<T>`, it performs special conversion logic.

### Requirements

The coercion mechanism must support:

1. **Homogeneous tuples**: `[1, 2, 3]` → `Array<i32>`
2. **Heterogeneous tuples**: `[1, "hello", true]` → `JSONArray` (when each element converts to `JSONValue`)
3. **Immutable containers**: Target container may not expose mutation methods
4. **User-defined collections**: Any collection implementing the right traits

### Approaches Considered

#### Approach A: Iterator-Based (FromIterator)

```wado
let a: Array<i32> = [1, 2, 3];
// Desugars to: Array::from_iter([1, 2, 3].into_iter())
```

Limitations:
- **Cannot handle heterogeneous tuples**: `IntoIterator::Item` must be a single type
- Requires `IntoIterator` impl for each tuple arity

#### Approach B: Builder Pattern

```wado
let a: Array<i32> = [1, 2, 3];
// Desugars to:
// ArrayBuilder::new().add(1).add(2).add(3).build()
```

Advantages:
- **Handles heterogeneous tuples**: Each `add()` call can use `Into<E>` conversion
- **Works with immutable containers**: Only Builder exposes mutation
- **Compile-time expansion**: Known tuple size allows direct code generation

### Decision: Builder Pattern

We adopt the Builder pattern as the primary mechanism for tuple-to-collection coercion.

## Decision

### 1. Builder Trait

```wado
/// Trait for building collections element by element
pub trait Builder {
    type Element;
    type Output;

    /// Create a new empty builder
    fn new() -> Self;

    /// Add an element and return the builder
    fn add(self, element: Self::Element) -> Self;

    /// Finalize and return the built collection
    fn build(self) -> Self::Output;
}
```

The `add` method takes `self` by value (consuming) to enable fluent chaining. Implementations may internally use mutation.

### 2. Collectable Trait

```wado
/// Trait for collection types that can be built from elements
pub trait Collectable {
    type Element;
    type Builder: Builder<Element = Self::Element, Output = Self>;
}
```

This associates a collection type with its builder.

### 3. Into Trait for Element Conversion

```wado
/// Trait for type conversions
pub trait Into<T> {
    fn into(self) -> T;
}

// Identity implementation
impl Into<T> for T {
    fn into(self) -> T {
        return self;
    }
}
```

### 4. Coercion Rules

When the compiler encounters a tuple literal `[e0, e1, ..., en]` with target type `C` where `C: Collectable<Element = E>`:

1. **Check each element**: Verify `Ti: Into<E>` for each element type `Ti`
2. **Expand at compile time**: Generate builder chain

```wado
// Source
let container: C = [e0, e1, e2];

// Compiler expansion
let container: C = {
    C::Builder::new()
        .add(Into::<E>::into(e0))
        .add(Into::<E>::into(e1))
        .add(Into::<E>::into(e2))
        .build()
};
```

### 5. Coercion Contexts

Coercion applies in three contexts:

#### Context 1: Variable Initialization with Type Annotation

```wado
let arr: Array<i32> = [1, 2, 3];
```

#### Context 2: Function Argument Passing

```wado
fn process(items: Array<i32>) { ... }
process([1, 2, 3]);
```

#### Context 3: Explicit Cast with `as`

```wado
let arr = [1, 2, 3] as Array<i32>;
```

### 6. Array Implementation

```wado
pub struct ArrayBuilder<T> {
    items: Array<T>,
}

impl Builder for ArrayBuilder<T> {
    type Element = T;
    type Output = Array<T>;

    fn new() -> Self {
        return ArrayBuilder { items: [] };
    }

    fn add(self, element: T) -> Self {
        self.items.append(element);
        return self;
    }

    fn build(self) -> Array<T> {
        return self.items;
    }
}

impl Collectable for Array<T> {
    type Element = T;
    type Builder = ArrayBuilder<T>;
}
```

### 7. JSONArray Example (Heterogeneous)

```wado
pub variant JSONValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(JSONArray),
    Object(JSONObject),
}

pub struct JSONArray {
    items: Array<JSONValue>,
}

// Into implementations for JSONValue
impl Into<JSONValue> for i32 {
    fn into(self) -> JSONValue {
        return JSONValue::Number(self as f64);
    }
}

impl Into<JSONValue> for String {
    fn into(self) -> JSONValue {
        return JSONValue::String(self);
    }
}

impl Into<JSONValue> for bool {
    fn into(self) -> JSONValue {
        return JSONValue::Bool(self);
    }
}

// Builder for JSONArray
pub struct JSONArrayBuilder {
    items: Array<JSONValue>,
}

impl Builder for JSONArrayBuilder {
    type Element = JSONValue;
    type Output = JSONArray;

    fn new() -> Self {
        return JSONArrayBuilder { items: [] };
    }

    fn add(self, element: JSONValue) -> Self {
        self.items.append(element);
        return self;
    }

    fn build(self) -> JSONArray {
        return JSONArray { items: self.items };
    }
}

impl Collectable for JSONArray {
    type Element = JSONValue;
    type Builder = JSONArrayBuilder;
}
```

Usage:

```wado
// Heterogeneous tuple → JSONArray
let json: JSONArray = [1, "hello", true, null];

// Expands to:
let json: JSONArray = {
    JSONArrayBuilder::new()
        .add(Into::<JSONValue>::into(1))        // i32 → JSONValue::Number
        .add(Into::<JSONValue>::into("hello"))  // String → JSONValue::String
        .add(Into::<JSONValue>::into(true))     // bool → JSONValue::Bool
        .add(Into::<JSONValue>::into(null))     // null → JSONValue::Null
        .build()
};
```

### 8. Immutable Collection Example

```wado
// Completely immutable - no mutation methods exposed
pub struct ImmutableList<T> {
    // Internal representation (e.g., cons list, rope, etc.)
    #[hidden]
    repr: InternalRepr<T>,
}

impl ImmutableList<T> {
    fn len(&self) -> i32 { ... }
    fn get(&self, index: i32) -> Option<T> { ... }
    // No append, push, or any mutation methods!
}

// Builder handles construction internally
pub struct ImmutableListBuilder<T> {
    buffer: Array<T>,
}

impl Builder for ImmutableListBuilder<T> {
    type Element = T;
    type Output = ImmutableList<T>;

    fn new() -> Self {
        return ImmutableListBuilder { buffer: [] };
    }

    fn add(self, element: T) -> Self {
        self.buffer.append(element);
        return self;
    }

    fn build(self) -> ImmutableList<T> {
        // Convert buffer to immutable internal representation
        return ImmutableList::from_array(self.buffer);
    }
}

impl Collectable for ImmutableList<T> {
    type Element = T;
    type Builder = ImmutableListBuilder<T>;
}

// Now this works!
let list: ImmutableList<i32> = [1, 2, 3, 4, 5];
// list has no mutation methods, but was constructed via Builder
```

### 9. Empty Tuple

The empty tuple `[]` coerces to empty collections:

```wado
let empty_arr: Array<i32> = [];
let empty_json: JSONArray = [];
let empty_list: ImmutableList<String> = [];

// All expand to: C::Builder::new().build()
```

### 10. Relationship with Iterator Traits

Builder-based coercion and Iterator traits serve different purposes:

| Mechanism | Purpose | Tuple Support |
|-----------|---------|---------------|
| **Builder** | Literal → Collection coercion | Homo + Hetero |
| **FromIterator** | Iterator → Collection | Homo only |
| **IntoIterator** | Collection → Iterator | N/A |

They can coexist:

```wado
// Builder-based (compile-time, supports hetero)
let json: JSONArray = [1, "hello", true];

// Iterator-based (runtime, homo only)
let arr: Array<i32> = some_iterator.collect();

// FromIterator can use Builder internally
impl FromIterator<T> for Array<T> {
    fn from_iter(iter: impl Iterator<Item = T>) -> Array<T> {
        let mut builder = ArrayBuilder::new();
        loop {
            if let Some(item) = iter.next() {
                builder = builder.add(item);
            } else {
                break;
            }
        }
        return builder.build();
    }
}
```

### 11. Type Error Messages

Clear error messages guide users:

```wado
// ERROR: No Into<E> implementation
let arr: Array<i32> = [1, "hello", 3];
// error: cannot coerce String to i32
//   --> example.wado:1:27
//    |
// 1  | let arr: Array<i32> = [1, "hello", 3];
//    |                           ^^^^^^^ String does not implement Into<i32>
```

```wado
// ERROR: Collection not Collectable
struct MyType { ... }
let m: MyType = [1, 2, 3];
// error: MyType does not implement Collectable
//   --> example.wado:2:5
//    |
// 2  | let m: MyType = [1, 2, 3];
//    |     ^ cannot coerce tuple literal to MyType
//    |
//    = help: implement Collectable for MyType to enable tuple coercion
```

### 12. Implementation Strategy

#### Phase 1: Core Traits

Define `Builder`, `Collectable`, and `Into` traits in prelude.

#### Phase 2: Array Implementation

Implement `ArrayBuilder` and `Collectable for Array<T>`.

#### Phase 3: Compiler Coercion Logic

Replace hardcoded tuple-to-array coercion with trait-based expansion:

```rust
// In resolver.rs
fn try_tuple_coercion(tuple_expr: &Expr, target_type: &Type) -> Option<Expr> {
    // 1. Check target implements Collectable
    let element_type = get_collectable_element(target_type)?;
    let builder_type = get_collectable_builder(target_type)?;

    // 2. Check each tuple element has Into<Element>
    for elem in tuple_elements {
        check_into_impl(elem.type, element_type)?;
    }

    // 3. Generate builder chain
    let mut expr = call_static(builder_type, "new", []);
    for elem in tuple_elements {
        let converted = call_into(elem, element_type);
        expr = call_method(expr, "add", [converted]);
    }
    expr = call_method(expr, "build", []);

    return Some(expr);
}
```

#### Phase 4: Standard Library

Implement `Collectable` for standard collections (Set, Dict, etc.).

## Consequences

### Positive

1. **Heterogeneous tuple support**: `[1, "hello", true]` → `JSONArray` works
2. **Immutable containers**: Collections need not expose mutation
3. **User extensibility**: Any type can implement `Collectable`
4. **Compile-time expansion**: No runtime iterator overhead for literals
5. **Type safety**: Each element conversion is checked at compile time
6. **Clear semantics**: Builder pattern is well-understood

### Negative

1. **Requires Builder per collection**: Each collection needs a Builder type
   - **Mitigation**: Can be derived or generated
2. **More traits to implement**: `Builder`, `Collectable`, `Into`
   - **Mitigation**: `Into` is useful beyond coercion; Builder is optional
3. **Trait bounds complexity**: `Builder<Element = E, Output = C>`
   - **Mitigation**: Users rarely write these directly

### Trade-offs

| Aspect | Iterator-Based | Builder-Based |
|--------|---------------|---------------|
| Heterogeneous tuples | ❌ Not supported | ✅ Supported |
| Immutable containers | ⚠️ Needs internal mutation | ✅ Fully supported |
| Runtime overhead | ⚠️ Iterator creation | ✅ None (compile-time) |
| Implementation effort | Low (one trait) | Medium (two traits + Builder type) |
| Generality | High | High |

## Examples

### Basic Array

```wado
fn run() with Stdout {
    // Homogeneous tuple → Array
    let numbers: Array<i32> = [1, 2, 3, 4, 5];

    for let n of numbers {
        println(`{n}`);
    }
}
```

### JSON Construction

```wado
fn run() with Stdout {
    // Heterogeneous tuple → JSONArray
    let data: JSONArray = [
        42,
        "hello",
        true,
        null,
        [1, 2, 3],  // Nested array
    ];

    println(data.stringify());
    // [42, "hello", true, null, [1, 2, 3]]
}
```

### Custom Collection

```wado
// User-defined sorted set
struct SortedSet<T: Ord> {
    items: Array<T>,
}

struct SortedSetBuilder<T: Ord> {
    items: Array<T>,
}

impl Builder for SortedSetBuilder<T: Ord> {
    type Element = T;
    type Output = SortedSet<T>;

    fn new() -> Self {
        return SortedSetBuilder { items: [] };
    }

    fn add(self, element: T) -> Self {
        // Insert in sorted order, skip duplicates
        // ... implementation ...
        return self;
    }

    fn build(self) -> SortedSet<T> {
        return SortedSet { items: self.items };
    }
}

impl Collectable for SortedSet<T: Ord> {
    type Element = T;
    type Builder = SortedSetBuilder<T>;
}

fn run() {
    // Automatically sorted and deduplicated!
    let set: SortedSet<i32> = [3, 1, 4, 1, 5, 9, 2, 6];
    // set.items = [1, 2, 3, 4, 5, 6, 9]
}
```

### Dict from Pairs

```wado
impl Collectable for Dict<K, V> {
    type Element = [K, V];  // Key-value tuple
    type Builder = DictBuilder<K, V>;
}

fn run() {
    // Tuple of pairs → Dict
    let config: Dict<String, i32> = [
        ["width", 1920],
        ["height", 1080],
        ["fps", 60],
    ];
}
```

## Related WEPs

- [Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md) - Defines tuple literal syntax
- [Iterator Traits Design](./wep-2026-01-24-iterator-traits.md) - Iterator traits (orthogonal to Builder)
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) - Foundation for traits

## References

- [Rust std::iter::FromIterator](https://doc.rust-lang.org/std/iter/trait.FromIterator.html)
- [Swift ExpressibleByArrayLiteral](https://developer.apple.com/documentation/swift/expressiblebyarrayliteral)
- [C# Collection Initializers](https://docs.microsoft.com/en-us/dotnet/csharp/programming-guide/classes-and-structs/object-and-collection-initializers)
- [Kotlin buildList](https://kotlinlang.org/api/latest/jvm/stdlib/kotlin.collections/build-list.html)
