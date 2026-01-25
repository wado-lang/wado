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

#### Null Literal

```wado
let n = null;  // Type: Null
```

`null` has the special type `Null`. By default, `Null` coerces to `Option::None`. Custom coercion can be provided via `impl Into<T> for Null`.

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
    fn build(self) -> Self::Output;
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
    fn build(self) -> Self::Output;
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
2. **Recursively coerce nested literals**: If `ei` is itself a literal (tuple or object), coerce it to `E` first
3. **Expand at compile time**:

```wado
// Source
let container: C = [e0, e1, e2];

// Compiler expansion
let container: C = {
    let mut __builder = C::Builder::with_capacity(3);
    __builder.append(Into::<E>::into(e0));
    __builder.append(Into::<E>::into(e1));
    __builder.append(Into::<E>::into(e2));
    __builder.build()
};
```

**Important**: Coercion only applies to **literals**, not variables:

```wado
let t = [1, 2, 3];           // t is a tuple [i32, i32, i32]
let arr: Array<i32> = t;     // ERROR: t is not a literal, no coercion
let arr: Array<i32> = [1, 2, 3];  // OK: literal coercion
```

### 8. Object Literal Coercion Rules

When the compiler encounters an object literal `{ k0: v0, k1: v1, ... }` with target type `C` where `C: FromObject<Value = V>`:

1. **Check each value**: Verify `Vi: Into<V>` for each value type `Vi`
2. **Recursively coerce nested literals**: If `vi` is itself a literal (tuple or object), coerce it to `V` first
3. **Expand at compile time**:

```wado
// Source
let container: C = { "name": "Alice", "age": 30 };

// Compiler expansion
let container: C = {
    let mut __builder = C::Builder::with_capacity(2);
    __builder.insert("name", Into::<V>::into("Alice"));
    __builder.insert("age", Into::<V>::into(30));
    __builder.build()
};
```

**Struct literal priority**: If the target type is a struct with matching fields, it is interpreted as a struct literal, not an object literal coercion:

```wado
struct Config { width: i32, height: i32 }

let c: Config = { width: 1920, height: 1080 };  // Struct literal, NOT FromObject coercion
```

### 9. Coercion Contexts

Coercion applies in the following contexts where the target type is known:

1. **Variable initialization**: `let arr: Array<i32> = [1, 2, 3];`
2. **Function argument**: `process([1, 2, 3]);`
3. **Explicit cast**: `[1, 2, 3] as Array<i32>`
4. **Return statement**: `fn f() -> Array<i32> { return [1, 2, 3]; }`
5. **Conditional branches**: `let x: Array<i32> = if cond { [1, 2] } else { [3, 4, 5] };`

**Not a coercion context** (requires explicit cast):

```wado
[1, 2, 3].iter()  // ERROR: tuple has no iter() method
([1, 2, 3] as Array<i32>).iter()  // OK: explicit cast
```

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

    fn build(self) -> Array<T> {
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
    __builder.build()
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

    fn build(self) -> Dict<String, V> {
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
    __builder.build()
}
```

### 12. JSONValue Example (Both FromTuple and FromObject)

JSONValue can accept both tuple and object literals by implementing both traits:

```wado
pub variant JSONValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Array<JSONValue>),
    Object(Dict<String, JSONValue>),
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

// Null type coerces to JSONValue::Null
impl Into<JSONValue> for Null {
    fn into(self) -> JSONValue {
        return JSONValue::Null;
    }
}
```

### 13. JSONValue FromTuple Implementation

```wado
// Builder for tuple literals → JSONValue::Array
pub struct JSONValueArrayBuilder {
    items: Array<JSONValue>,
}

impl TupleBuilder for JSONValueArrayBuilder {
    type Element = JSONValue;
    type Output = JSONValue;

    fn with_capacity(n: i32) -> Self {
        return JSONValueArrayBuilder { items: Array::<JSONValue>::with_capacity(n) };
    }

    fn append(&mut self, element: JSONValue) {
        self.items.append(element);
    }

    fn build(self) -> JSONValue {
        return JSONValue::Array(self.items);
    }
}

impl FromTuple for JSONValue {
    type Element = JSONValue;
    type Builder = JSONValueArrayBuilder;
}
```

### 14. JSONValue FromObject Implementation

```wado
// Builder for object literals → JSONValue::Object
pub struct JSONValueObjectBuilder {
    entries: Dict<String, JSONValue>,
}

impl ObjectBuilder for JSONValueObjectBuilder {
    type Value = JSONValue;
    type Output = JSONValue;

    fn with_capacity(n: i32) -> Self {
        return JSONValueObjectBuilder { entries: Dict::<String, JSONValue>::with_capacity(n) };
    }

    fn insert(&mut self, key: String, value: JSONValue) {
        self.entries.insert(key, value);
    }

    fn build(self) -> JSONValue {
        return JSONValue::Object(self.entries);
    }
}

impl FromObject for JSONValue {
    type Value = JSONValue;
    type Builder = JSONValueObjectBuilder;
}
```

### 15. Nested JSON Literals

With both traits implemented, JSONValue supports nested structures:

```wado
let data: JSONValue = {
    "name": "Alice",
    "age": 30,
    "tags": [1, 2, 3],           // Nested tuple → JSONValue::Array
    "metadata": {                 // Nested object → JSONValue::Object
        "created": "2024-01-01",
        "active": true,
    },
    "nullable": null,            // Null → JSONValue::Null
};

// Compiler recursively coerces each nested literal:
// 1. Outer { ... } → FromObject → JSONValue::Object
// 2. Inner [1, 2, 3] → FromTuple → JSONValue::Array
// 3. Inner { ... } → FromObject → JSONValue::Object
// 4. null → Into<JSONValue> → JSONValue::Null
```

### 16. Immutable Collection Example

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

    fn build(self) -> ImmutableList<T> {
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

### 17. Empty Literals

Empty literals coerce to empty collections:

```wado
let empty_arr: Array<i32> = [];
let empty_dict: Dict<String, i32> = {};

// Expand to:
// C::Builder::with_capacity(0).build()
```

### 18. Relationship with Iterator Traits

Builder-based coercion and Iterator traits serve different purposes:

| Mechanism         | Purpose                     | Hetero Support |
| ----------------- | --------------------------- | -------------- |
| **TupleBuilder**  | Tuple literal → Collection  | ✅ Yes         |
| **ObjectBuilder** | Object literal → Collection | ✅ Yes         |
| **FromIterator**  | Iterator → Collection       | ❌ Homo only   |
| **IntoIterator**  | Collection → Iterator       | N/A            |

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

### 19. Type Error Messages

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

### 20. Implementation Strategy

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
    stmts.push(call_method("__builder", "build", []));

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

| Aspect         | Tuple Literal     | Object Literal                    |
| -------------- | ----------------- | --------------------------------- |
| Default type   | Tuple `[T, U, V]` | Anonymous struct `{ a: T, b: U }` |
| Coercion trait | `FromTuple`       | `FromObject`                      |
| Builder trait  | `TupleBuilder`    | `ObjectBuilder`                   |
| Key type       | N/A               | `String` (fixed)                  |
| Hetero support | ✅ via `Into<E>`  | ✅ via `Into<V>`                  |

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

    fn build(self) -> SortedSet<T> {
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
