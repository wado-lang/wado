# WEP: Anonymous Struct Literal to Map Coercion

## Context

Anonymous struct literals `{ key: value, ... }` should coerce to associative containers (e.g., `TreeMap<String, V>`) when a target type is available, using the Builder pattern from [Literal-to-Collection Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md).

The tuple → Array coercion is already hardcoded in the compiler. This WEP implements the analogous object literal → map coercion path using the same Builder approach.

## Decision

### Traits

Define in `core:prelude`:

```wado
pub trait ObjectBuilder {
    type Value;
    type Output;
    fn with_capacity(n: i32) -> Self;
    fn insert(&mut self, key: String, value: Self::Value);
    fn build(self) -> Self::Output;
}

pub trait FromObject {
    type Value;
    type Builder: ObjectBuilder<Value = Self::Value, Output = Self>;
}
```

### Expansion

```wado
let config: TreeMap<String, i32> = { width: 1920, height: 1080 };

// → desugars to:
{
    let mut __builder = TreeMap::<String, i32>::with_capacity(2);
    __builder.insert("width", 1920);
    __builder.insert("height", 1080);
    __builder.build()
}
```

Identifier keys are stringified: `width` → `"width"`. Quoted keys (`"width"`) are also accepted.

### Priority Rule

Named struct literal takes priority. If the target type is a named struct with matching fields, it is a struct literal, not a `FromObject` coercion:

```wado
struct Config { width: i32, height: i32 }
let c: Config = { width: 1920, height: 1080 };  // struct literal, NOT coercion
```

`FromObject` coercion applies only when the target type implements `FromObject` and is not a struct with matching fields.

### TreeMap Implementation

TreeMap serves as its own builder (no separate builder type needed):

```wado
impl ObjectBuilder for TreeMap<String, V> {
    type Value = V;
    type Output = TreeMap<String, V>;

    fn with_capacity(n: i32) -> Self {
        return TreeMap::<String, V>::with_capacity(n);
    }

    fn insert(&mut self, key: String, value: V) {
        self.insert(key, value);
    }

    fn build(self) -> TreeMap<String, V> {
        return self;
    }
}

impl FromObject for TreeMap<String, V> {
    type Value = V;
    type Builder = TreeMap<String, V>;
}
```

### Coercion Contexts

Same as tuple → Array coercion:

1. Variable initialization: `let m: TreeMap<String, i32> = { x: 1 };`
2. Function argument: `process({ x: 1 })`
3. Explicit cast: `{ x: 1 } as TreeMap<String, i32>`
4. Return statement: `return { x: 1 };`
5. Conditional branches

### Heterogeneous Values

With the `Into<T>` trait (from the parent WEP), heterogeneous object literals coerce when all values implement `Into<V>`:

```wado
// Requires Into<JSONValue> for i32, String, bool, etc.
let data: JSONValue = {
    "name": "Alice",
    "age": 30,
    "active": true,
};
```

## Consequences

- Consistent with tuple → Array coercion (same Builder pattern)
- Extensible to any user-defined associative container
- Keys are always `String` (matching Wado's anonymous struct semantics)
- No runtime overhead (compile-time expansion)
- Requires `TreeMap::with_capacity` method (add if missing)
