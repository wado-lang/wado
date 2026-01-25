# WEP: Literal-to-Collection Coercion

## Context

Wado allows literals to be coerced to collection types in certain contexts:

```wado
// Tuple literal → Array
let a: Array<i32> = [1, 2, 3];

// Object literal → Dict
let d: Dict<String, i32> = { "width": 1920, "height": 1080 };
```

This coercion is currently **hardcoded** in the compiler. We need a trait-based mechanism that:

1. Supports both tuple literals and object literals
2. Handles heterogeneous elements (e.g., `[1, "hello", true]` → `JSONArray`)
3. Works with immutable containers
4. Is extensible to user-defined collections

## Decision

### 1. Literal Types

#### Tuple Literal

```wado
let t = [1, 2, 3];       // Type: [i32, i32, i32]
let mixed = [1, "hi"];   // Type: [i32, String]
```

Default type is a tuple with the inferred element types.

#### Object Literal

```wado
let obj = { name: "Alice", age: 30 };
// Type: { name: String, age: i32 } (anonymous struct)
```

Default type is an anonymous struct. The parser already supports this as "implicit struct".

### 2. TupleBuilder Trait

```wado
/// Trait for building collections from tuple literals
pub trait TupleBuilder {
    type Element;
    type Output;

    /// Create a builder with pre-allocated capacity
    fn with_capacity(n: i32) -> Self;

    /// Append an element to the builder
    fn append(&mut self, element: Self::Element);

    /// Finalize and return the built collection
    fn into(self) -> Self::Output;
}
```

### 3. FromTuple Trait

```wado
/// Trait for collection types that can be built from tuple literals
pub trait FromTuple {
    type Element;
    type Builder: TupleBuilder<Element = Self::Element, Output = Self>;
}
```

### 4. ObjectBuilder Trait

```wado
/// Trait for building collections from object literals
pub trait ObjectBuilder {
    type Value;
    type Output;

    /// Create a builder with pre-allocated capacity
    fn with_capacity(n: i32) -> Self;

    /// Insert a key-value pair
    fn insert(&mut self, key: String, value: Self::Value);

    /// Finalize and return the built collection
    fn into(self) -> Self::Output;
}
```

### 5. FromObject Trait

```wado
/// Trait for collection types that can be built from object literals
pub trait FromObject {
    type Value;
    type Builder: ObjectBuilder<Value = Self::Value, Output = Self>;
}
```

### 6. Into Trait for Element Conversion

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

### 7. Tuple Literal Coercion Rules

When the compiler encounters a tuple literal `[e0, e1, ..., en]` with target type `C` where `C: FromTuple<Element = E>`:

1. **Check each element**: Verify `Ti: Into<E>` for each element type `Ti`
2. **Expand at compile time**:

```wado
// Source
let container: C = [e0, e1, e2];

// Compiler expansion
let container: C = {
    let mut __builder = C::Builder::with_capacity(3);
    __builder.append(Into::<E>::into(e0));
    __builder.append(Into::<E>::into(e1));
    __builder.append(Into::<E>::into(e2));
    __builder.into()
};
```

### 8. Object Literal Coercion Rules

When the compiler encounters an object literal `{ k0: v0, k1: v1, ... }` with target type `C` where `C: FromObject<Value = V>`:

1. **Check each value**: Verify `Vi: Into<V>` for each value type `Vi`
2. **Expand at compile time**:

```wado
// Source
let container: C = { "name": "Alice", "age": 30 };

// Compiler expansion
let container: C = {
    let mut __builder = C::Builder::with_capacity(2);
    __builder.insert("name", Into::<V>::into("Alice"));
    __builder.insert("age", Into::<V>::into(30));
    __builder.into()
};
```

### 9. Coercion Contexts

Coercion applies in three contexts:

1. **Variable initialization**: `let arr: Array<i32> = [1, 2, 3];`
2. **Function argument**: `process([1, 2, 3]);`
3. **Explicit cast**: `[1, 2, 3] as Array<i32>`

### 10. Array Implementation

Array serves as its own builder (no separate builder type needed):

```wado
impl TupleBuilder for Array<T> {
    type Element = T;
    type Output = Array<T>;

    fn with_capacity(n: i32) -> Self {
        return Array::<T>::with_capacity(n);
    }

    fn append(&mut self, element: T) {
        self.append(element);  // Existing method
    }

    fn into(self) -> Array<T> {
        return self;  // Identity
    }
}

impl FromTuple for Array<T> {
    type Element = T;
    type Builder = Array<T>;  // Array is its own builder!
}
```

Expansion for Array:

```wado
let arr: Array<i32> = [1, 2, 3];

// →
{
    let mut __builder = Array::<i32>::with_capacity(3);
    __builder.append(1);
    __builder.append(2);
    __builder.append(3);
    __builder.into()
}
```

### 11. Dict Implementation

Dict also serves as its own builder:

```wado
impl ObjectBuilder for Dict<String, V> {
    type Value = V;
    type Output = Dict<String, V>;

    fn with_capacity(n: i32) -> Self {
        return Dict::<String, V>::with_capacity(n);
    }

    fn insert(&mut self, key: String, value: V) {
        self.insert(key, value);  // Existing method
    }

    fn into(self) -> Dict<String, V> {
        return self;  // Identity
    }
}

impl FromObject for Dict<String, V> {
    type Value = V;
    type Builder = Dict<String, V>;
}
```

Expansion for Dict:

```wado
let config: Dict<String, i32> = { "width": 1920, "height": 1080 };

// →
{
    let mut __builder = Dict::<String, i32>::with_capacity(2);
    __builder.insert("width", 1920);
    __builder.insert("height", 1080);
    __builder.into()
}
```

### 12. JSONArray Example (Heterogeneous Tuple)

```wado
pub variant JSONValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(JSONArray),
    Object(JSONObject),
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

// JSONArray can use Array<JSONValue> as its builder
pub struct JSONArray {
    items: Array<JSONValue>,
}

impl TupleBuilder for JSONArray {
    type Element = JSONValue;
    type Output = JSONArray;

    fn with_capacity(n: i32) -> Self {
        return JSONArray { items: Array::<JSONValue>::with_capacity(n) };
    }

    fn append(&mut self, element: JSONValue) {
        self.items.append(element);
    }

    fn into(self) -> JSONArray {
        return self;
    }
}

impl FromTuple for JSONArray {
    type Element = JSONValue;
    type Builder = JSONArray;
}
```

Usage:

```wado
// Heterogeneous tuple → JSONArray
let json: JSONArray = [1, "hello", true, null];

// Expands to:
{
    let mut __builder = JSONArray::with_capacity(4);
    __builder.append(Into::<JSONValue>::into(1));        // i32 → JSONValue::Number
    __builder.append(Into::<JSONValue>::into("hello"));  // String → JSONValue::String
    __builder.append(Into::<JSONValue>::into(true));     // bool → JSONValue::Bool
    __builder.append(Into::<JSONValue>::into(null));     // null → JSONValue::Null
    __builder.into()
}
```

### 13. JSONObject Example (Heterogeneous Object)

```wado
pub struct JSONObject {
    entries: Dict<String, JSONValue>,
}

impl ObjectBuilder for JSONObject {
    type Value = JSONValue;
    type Output = JSONObject;

    fn with_capacity(n: i32) -> Self {
        return JSONObject { entries: Dict::<String, JSONValue>::with_capacity(n) };
    }

    fn insert(&mut self, key: String, value: JSONValue) {
        self.entries.insert(key, value);
    }

    fn into(self) -> JSONObject {
        return self;
    }
}

impl FromObject for JSONObject {
    type Value = JSONValue;
    type Builder = JSONObject;
}
```

Usage:

```wado
// Heterogeneous object → JSONObject
let obj: JSONObject = {
    "name": "Alice",
    "age": 30,
    "active": true
};

// Expands to:
{
    let mut __builder = JSONObject::with_capacity(3);
    __builder.insert("name", Into::<JSONValue>::into("Alice"));
    __builder.insert("age", Into::<JSONValue>::into(30));
    __builder.insert("active", Into::<JSONValue>::into(true));
    __builder.into()
}
```

### 14. Immutable Collection Example

```wado
// Completely immutable - no mutation methods exposed
pub struct ImmutableList<T> {
    #[hidden]
    repr: InternalRepr<T>,
}

impl ImmutableList<T> {
    fn len(&self) -> i32 { ... }
    fn get(&self, index: i32) -> Option<T> { ... }
    // No append, push, or any mutation methods!
}

// Separate builder for immutable list
pub struct ImmutableListBuilder<T> {
    buffer: Array<T>,
}

impl TupleBuilder for ImmutableListBuilder<T> {
    type Element = T;
    type Output = ImmutableList<T>;

    fn with_capacity(n: i32) -> Self {
        return ImmutableListBuilder { buffer: Array::<T>::with_capacity(n) };
    }

    fn append(&mut self, element: T) {
        self.buffer.append(element);
    }

    fn into(self) -> ImmutableList<T> {
        return ImmutableList::from_array(self.buffer);
    }
}

impl FromTuple for ImmutableList<T> {
    type Element = T;
    type Builder = ImmutableListBuilder<T>;
}

// Now this works!
let list: ImmutableList<i32> = [1, 2, 3, 4, 5];
// list has no mutation methods, but was constructed via Builder
```

### 15. Empty Literals

Empty literals coerce to empty collections:

```wado
let empty_arr: Array<i32> = [];
let empty_dict: Dict<String, i32> = {};

// Expand to:
// C::Builder::with_capacity(0).into()
```

### 16. Relationship with Iterator Traits

Builder-based coercion and Iterator traits serve different purposes:

| Mechanism | Purpose | Hetero Support |
|-----------|---------|----------------|
| **TupleBuilder** | Tuple literal → Collection | ✅ Yes |
| **ObjectBuilder** | Object literal → Collection | ✅ Yes |
| **FromIterator** | Iterator → Collection | ❌ Homo only |
| **IntoIterator** | Collection → Iterator | N/A |

They coexist and can be combined:

```wado
// Literal-based (compile-time, supports hetero)
let json: JSONArray = [1, "hello", true];

// Iterator-based (runtime, homo only)
let arr: Array<i32> = some_iterator.collect();

// FromIterator can use TupleBuilder internally
impl FromIterator<T> for Array<T> {
    fn from_iter(iter: impl Iterator<Item = T>) -> Array<T> {
        let mut arr = Array::<T>::with_capacity(0);
        loop {
            if let Some(item) = iter.next() {
                arr.append(item);
            } else {
                break;
            }
        }
        return arr;
    }
}
```

### 17. Type Error Messages

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
// ERROR: Type not FromTuple
struct MyType { ... }
let m: MyType = [1, 2, 3];
// error: MyType does not implement FromTuple
//   --> example.wado:2:5
//    |
// 2  | let m: MyType = [1, 2, 3];
//    |     ^ cannot coerce tuple literal to MyType
//    |
//    = help: implement FromTuple for MyType to enable tuple coercion
```

### 18. Implementation Strategy

#### Phase 1: Core Traits

Define traits in prelude:
- `Into<T>`
- `TupleBuilder` / `FromTuple`
- `ObjectBuilder` / `FromObject`

#### Phase 2: Standard Implementations

- `impl TupleBuilder for Array<T>`
- `impl FromTuple for Array<T>`
- `impl ObjectBuilder for Dict<String, V>`
- `impl FromObject for Dict<String, V>`

#### Phase 3: Compiler Coercion Logic

Replace hardcoded coercion with trait-based expansion:

```rust
// In resolver.rs
fn try_literal_coercion(expr: &Expr, target_type: &Type) -> Option<Expr> {
    match expr.kind {
        TupleLiteral { elements } => try_tuple_coercion(elements, target_type),
        ObjectLiteral { entries } => try_object_coercion(entries, target_type),
        _ => None,
    }
}

fn try_tuple_coercion(elements: &[Expr], target_type: &Type) -> Option<Expr> {
    // 1. Check target implements FromTuple
    let element_type = get_from_tuple_element(target_type)?;
    let builder_type = get_from_tuple_builder(target_type)?;

    // 2. Check each element has Into<Element>
    for elem in elements {
        check_into_impl(elem.type, element_type)?;
    }

    // 3. Generate builder expansion
    let n = elements.len();
    let mut stmts = vec![
        let_mut("__builder", call_static(builder_type, "with_capacity", [n]))
    ];
    for elem in elements {
        let converted = call_into(elem, element_type);
        stmts.push(call_method_stmt("__builder", "append", [converted]));
    }
    stmts.push(call_method("__builder", "into", []));

    return Some(block_expr(stmts));
}
```

## Consequences

### Positive

1. **Unified design**: Both tuple and object literals use the same pattern
2. **Heterogeneous support**: `[1, "hello", true]` → `JSONArray` works
3. **Immutable containers**: Collections need not expose mutation
4. **User extensibility**: Any type can implement `FromTuple` / `FromObject`
5. **Compile-time expansion**: No runtime overhead for literals
6. **Self-as-builder optimization**: `Array` and `Dict` are their own builders

### Negative

1. **Four traits**: `TupleBuilder`, `FromTuple`, `ObjectBuilder`, `FromObject`
   - **Mitigation**: Clear separation of concerns; users typically implement one pair
2. **Requires `Into` implementations**: For heterogeneous coercion
   - **Mitigation**: `Into` is useful beyond coercion

### Trade-offs

| Aspect | Tuple Literal | Object Literal |
|--------|---------------|----------------|
| Default type | Tuple `[T, U, V]` | Anonymous struct `{ a: T, b: U }` |
| Coercion trait | `FromTuple` | `FromObject` |
| Builder trait | `TupleBuilder` | `ObjectBuilder` |
| Key type | N/A | `String` (fixed) |
| Hetero support | ✅ via `Into<E>` | ✅ via `Into<V>` |

## Examples

### Basic Usage

```wado
fn run() with Stdout {
    // Tuple literal → Array
    let numbers: Array<i32> = [1, 2, 3, 4, 5];

    // Object literal → Dict
    let config: Dict<String, i32> = {
        "width": 1920,
        "height": 1080,
    };

    for let n of numbers {
        println(`{n}`);
    }
}
```

### JSON Construction

```wado
fn run() with Stdout {
    // Nested JSON structure
    let user: JSONObject = {
        "name": "Alice",
        "age": 30,
        "tags": [1, 2, 3],  // Nested JSONArray
        "metadata": {        // Nested JSONObject
            "created": "2024-01-01",
            "active": true,
        },
    };

    println(user.stringify());
}
```

### Custom Collection

```wado
struct SortedSet<T: Ord> {
    items: Array<T>,
}

struct SortedSetBuilder<T: Ord> {
    items: Array<T>,
}

impl TupleBuilder for SortedSetBuilder<T: Ord> {
    type Element = T;
    type Output = SortedSet<T>;

    fn with_capacity(n: i32) -> Self {
        return SortedSetBuilder { items: Array::<T>::with_capacity(n) };
    }

    fn append(&mut self, element: T) {
        // Insert in sorted order, skip duplicates
        // ...
    }

    fn into(self) -> SortedSet<T> {
        return SortedSet { items: self.items };
    }
}

impl FromTuple for SortedSet<T: Ord> {
    type Element = T;
    type Builder = SortedSetBuilder<T>;
}

fn run() {
    let set: SortedSet<i32> = [3, 1, 4, 1, 5, 9, 2, 6];
    // set.items = [1, 2, 3, 4, 5, 6, 9]
}
```

## Related WEPs

- [Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md) - Tuple literal syntax
- [Iterator Traits Design](./wep-2026-01-24-iterator-traits.md) - Iterator traits (orthogonal)
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) - Trait foundation

## References

- [Swift ExpressibleByArrayLiteral](https://developer.apple.com/documentation/swift/expressiblebyarrayliteral)
- [Swift ExpressibleByDictionaryLiteral](https://developer.apple.com/documentation/swift/expressiblebydictionaryliteral)
- [C# Collection Initializers](https://docs.microsoft.com/en-us/dotnet/csharp/programming-guide/classes-and-structs/object-and-collection-initializers)
- [Kotlin buildList / buildMap](https://kotlinlang.org/api/latest/jvm/stdlib/kotlin.collections/build-list.html)
