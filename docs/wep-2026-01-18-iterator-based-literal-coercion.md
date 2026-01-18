# WEP: Iterator-Based Literal Coercion

## Context

Wado allows tuple literals to be coerced to `Array<T>` in certain contexts:

```wado
let a: Array<i32> = [1, 2, 3];  // Tuple literal → Array<i32>
fn takes(arr: Array<i32>) {}
takes([1, 2, 3]);                // Implicit coercion
let b = [1, 2, 3] as Array<i32>; // Explicit cast
```

This coercion is currently **hardcoded** in the compiler (`resolver.rs` lines 1252-1278). When the compiler encounters a tuple literal with a target type of `Array<T>`, it performs special conversion logic.

### Current Implementation

The resolver has three specific coercion points:

1. **Variable initialization with type annotation**
   ```rust
   // In resolver.rs
   if let ResolvedType::Array { element_type } = target_type {
       // Special case: convert TupleLiteral to ArrayLiteral
       return coerce_tuple_to_array(...);
   }
   ```

2. **Function argument passing**
   ```rust
   // Check if parameter type is Array and argument is tuple
   if is_array_type(param_type) && is_tuple_literal(arg) {
       coerce_tuple_to_array(...);
   }
   ```

3. **Explicit `as` cast**
   ```rust
   // Handle tuple-to-array cast specially
   if cast_to_array && expr_is_tuple {
       convert_to_array_literal(...);
   }
   ```

### Problems with Current Approach

1. **Compiler complexity**: Each new collection type (Set, Dict, Vec, etc.) requires adding more special cases to the compiler
2. **Limited extensibility**: Users cannot define their own collection types with literal coercion
3. **Inconsistency**: Array gets special treatment, but other collection types don't
4. **Maintenance burden**: Coercion logic is scattered across multiple compiler phases

### The Opportunity

Wado already has iterator traits defined in the spec (WEP: Struct and Trait System):

```wado
trait Iterator {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
}

trait IntoIterator {
    type Item;
    type Iter: Iterator<Item = Self::Item>;
    fn into_iter(self) -> Self::Iter;
}

trait FromIterator<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Self;
}
```

These traits provide a **general mechanism** for converting between iterable types and collections. If we leverage them for literal coercion, we can:

- Remove hardcoded Array coercion from the compiler
- Enable coercion for any type implementing `FromIterator<T>`
- Allow users to define custom collection types with literal syntax
- Align with Rust's `.collect()` pattern

### Language Survey

#### Rust

Rust uses explicit `.collect()` with type inference:

```rust
let v: Vec<i32> = vec![1, 2, 3].into_iter().collect();
let set: HashSet<i32> = vec![1, 2, 2, 3].into_iter().collect();
```

Wado's proposal is similar but allows implicit coercion in type contexts:

```wado
let a: Array<i32> = [1, 2, 3];  // Implicit
let s: Set<i32> = [1, 2, 2, 3]; // Implicit
```

This is more concise while maintaining type safety.

#### TypeScript

TypeScript has structural typing and doesn't have explicit iterator-based coercion. Arrays and tuples are distinguished by type annotations only.

#### Python

Python has distinct syntax for lists `[1, 2, 3]` and tuples `(1, 2, 3)`, and uses comprehensions or explicit constructors:

```python
my_set = set([1, 2, 2, 3])  # Explicit constructor
```

Wado's approach is more elegant with implicit coercion based on type context.

## Decision

### 1. Generalize Literal Coercion via Iterator Traits

When the compiler encounters an expression `expr` that doesn't match the target type `T`, it applies the following coercion rule:

**Automatic Iterator Coercion**:
- If `expr`'s type `E` implements `IntoIterator`
- And target type `T` implements `FromIterator<E::Item>`
- Then desugar to: `T::from_iter(expr.into_iter())`

This replaces all hardcoded tuple-to-array coercion logic.

### 2. Tuple Types Implement IntoIterator

All tuple types implement `IntoIterator`:

```wado
// For homogeneous tuples (same element type)
impl<T> IntoIterator for [T, T] {
    type Item = T;
    type Iter = TupleIter<T>;

    fn into_iter(self) -> TupleIter<T> {
        return TupleIter::new_2(self.0, self.1);
    }
}

impl<T> IntoIterator for [T, T, T] {
    type Item = T;
    type Iter = TupleIter<T>;

    fn into_iter(self) -> TupleIter<T> {
        return TupleIter::new_3(self.0, self.1, self.2);
    }
}

// ... and so on for various tuple sizes
```

**Note**: Only homogeneous tuples (all elements same type) can implement `IntoIterator`. Heterogeneous tuples like `[i32, String, bool]` cannot be iterated uniformly.

### 3. Collection Types Implement FromIterator

Standard collection types implement `FromIterator<T>`:

```wado
// Array
impl<T> FromIterator<T> for Array<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Array<T> {
        let mut arr: Array<T> = [];
        for let item of iter {
            arr.append(item);
        }
        return arr;
    }
}

// Set (hypothetical)
impl<T: Eq + Hash> FromIterator<T> for Set<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Set<T> {
        let mut set = Set::new();
        for let item of iter {
            set.insert(item);
        }
        return set;
    }
}

// Dict (from key-value pair tuples)
impl<K: Eq + Hash, V> FromIterator<[K, V]> for Dict<K, V> {
    fn from_iter<I: Iterator<Item = [K, V]>>(iter: I) -> Dict<K, V> {
        let mut dict = Dict::new();
        for let [k, v] of iter {
            dict.insert(k, v);
        }
        return dict;
    }
}
```

### 4. Compiler Desugaring Rules

The compiler applies iterator coercion in three contexts (same as current hardcoded coercion):

#### Context 1: Variable Initialization with Type Annotation

```wado
// Source
let a: Array<i32> = [1, 2, 3];

// Desugared to
let a: Array<i32> = Array::from_iter([1, 2, 3].into_iter());
```

#### Context 2: Function Argument Passing

```wado
// Source
fn process(items: Array<i32>) { ... }
process([1, 2, 3]);

// Desugared to
fn process(items: Array<i32>) { ... }
process(Array::from_iter([1, 2, 3].into_iter()));
```

#### Context 3: Explicit Cast with `as`

```wado
// Source
let a = [1, 2, 3] as Array<i32>;

// Desugared to
let a = Array::from_iter([1, 2, 3].into_iter()) as Array<i32>;
```

### 5. Type Checking Requirements

For coercion to succeed, the compiler verifies:

1. **Source implements IntoIterator**: `E: IntoIterator`
2. **Target implements FromIterator**: `T: FromIterator<E::Item>`
3. **Element type matches**: The `Item` type from `IntoIterator` matches the type parameter of `FromIterator`

If any condition fails, coercion is not applied and a type error is reported.

### 6. Heterogeneous Tuple Handling

Heterogeneous tuples (mixed element types) cannot be coerced to collections:

```wado
let mixed = [1, "hello", true];  // Type: [i32, String, bool]

// ERROR: Cannot coerce to Array<?>
let a: Array<i32> = mixed;  // ❌ Heterogeneous tuple doesn't implement IntoIterator
```

This is a compile-time error with a clear message:
```
error: cannot coerce heterogeneous tuple to Array
  --> example.wado:2:21
   |
2  | let a: Array<i32> = mixed;
   |                     ^^^^^ type [i32, String, bool] does not implement IntoIterator
   |
   = note: only homogeneous tuples (all elements same type) can be iterated
```

### 7. Empty Tuple Special Case

The empty tuple `[]` has type `[]` (0-tuple). It implements `IntoIterator`:

```wado
impl IntoIterator for [] {
    type Item = !;  // Never type (no elements)
    type Iter = EmptyIter;

    fn into_iter(self) -> EmptyIter {
        return EmptyIter::new();
    }
}
```

This allows:

```wado
let empty: Array<i32> = [];  // OK: creates empty array
let empty_set: Set<String> = [];  // OK: creates empty set
```

**Type parameter inference**: When coercing `[]`, the element type is inferred from the target collection's type parameter.

### 8. User-Defined Collection Types

Users can define their own collection types with literal coercion by implementing `FromIterator`:

```wado
// User-defined immutable vector
struct Vec<T> {
    items: Array<T>,
}

impl<T> Vec<T> {
    fn new() -> Vec<T> {
        return Vec { items: [] };
    }
}

impl<T> FromIterator<T> for Vec<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Vec<T> {
        let mut vec = Vec::new();
        for let item of iter {
            vec.items.append(item);
        }
        return vec;
    }
}

// Now tuple literals work automatically!
let v: Vec<i32> = [1, 2, 3];  // ✅ Coercion via FromIterator
```

No compiler changes needed - the trait implementation is sufficient.

### 9. Chaining with Iterator Methods

Iterator coercion composes with iterator methods:

```wado
// Explicit iterator chain
let even_doubles: Array<i32> = [1, 2, 3, 4, 5]
    .into_iter()
    .filter(|x| x % 2 == 0)
    .map(|x| x * 2)
    .collect();

// With type inference
let result: Set<String> = [1, 2, 2, 3]
    .into_iter()
    .map(|n| `num_{n}`)
    .collect();
```

**Note**: `.collect()` is a method on `Iterator` that calls `FromIterator::from_iter(self)`. It provides the same functionality as implicit coercion but can be used mid-chain.

### 10. Remove Hardcoded Array Coercion

The compiler's special-case logic for tuple-to-array coercion (in `resolver.rs`) is **removed** and replaced with the general iterator coercion mechanism.

**Before** (hardcoded):
```rust
// resolver.rs - REMOVED
if let ResolvedType::Array { element_type } = target_type {
    if let TirExprKind::TupleLiteral { elements } = &expr.kind {
        // Special case: convert to ArrayLiteral
        return coerce_tuple_to_array(elements, element_type);
    }
}
```

**After** (general):
```rust
// resolver.rs - NEW
if let Some(coercion) = try_iterator_coercion(expr_type, target_type) {
    // General case: E: IntoIterator, T: FromIterator<E::Item>
    return desugar_to_from_iter_call(expr, target_type, coercion);
}
```

### 11. Dict Literal Syntax (Future Extension)

While not part of this WEP, the iterator-based coercion enables future Dict literal syntax:

```wado
// Tuple of 2-tuples coerces to Dict
let config: Dict<String, i32> = [
    ["width", 800],
    ["height", 600],
    ["fps", 60],
];

// Desugars to
let config: Dict<String, i32> = Dict::from_iter(
    [["width", 800], ["height", 600], ["fps", 60]].into_iter()
);
```

This requires `Dict` to implement `FromIterator<[K, V]>`, which is natural.

## Consequences

### Positive

1. **Simplicity**: Removes ~200 lines of special-case coercion logic from the compiler
2. **Generality**: Any type implementing `FromIterator<T>` gets literal coercion for free
3. **Extensibility**: Users can define custom collection types with literal syntax
4. **Consistency**: All collections are treated uniformly, not just `Array<T>`
5. **Composability**: Works seamlessly with iterator methods (`.filter()`, `.map()`, etc.)
6. **Rust alignment**: Matches Rust's `.collect()` pattern, reducing learning curve
7. **Type safety**: Compiler verifies trait bounds at compile time
8. **Clear errors**: Type errors point to missing trait implementations, not mysterious coercion failures
9. **Future-proof**: New collection types (Set, Dict, Vec, etc.) work automatically

### Negative

1. **Requires trait system**: This feature depends on full trait implementation (not yet complete)
   - **Mitigation**: Trait system is high priority and already designed (WEP: Struct and Trait System)
2. **Potential performance**: Iterator creation and `from_iter` calls may have overhead
   - **Mitigation**: Compiler can inline and optimize away iterator abstractions in most cases
   - **Mitigation**: For literals, the compiler can directly generate optimized code (constant folding)
3. **Heterogeneous tuples don't coerce**: Mixed-type tuples like `[1, "hello"]` cannot be iterated
   - **Mitigation**: This is expected behavior - heterogeneous collections don't make sense
   - **Mitigation**: Clear error messages guide users
4. **Breaking change**: If any code relies on current coercion behavior, it may break
   - **Mitigation**: Current behavior is preserved - this is a drop-in replacement
   - **Mitigation**: Implementation strategy can maintain compatibility during transition

### Trade-offs

| Aspect | Hardcoded Coercion | Iterator-Based Coercion |
|--------|-------------------|------------------------|
| **Compiler complexity** | High (special cases) | Low (general rule) |
| **Extensibility** | ❌ Compiler changes needed | ✅ Trait implementation only |
| **User-defined types** | ❌ Not supported | ✅ Fully supported |
| **Performance** | ✅ Direct codegen | ⚠️ Requires optimization |
| **Consistency** | ❌ Array is special | ✅ All collections equal |
| **Error messages** | ⚠️ "Type mismatch" | ✅ "Missing trait impl" |
| **Maintenance** | ❌ High | ✅ Low |

## Implementation Strategy

### Phase 1: Trait System Foundation (Prerequisite)

This WEP depends on the trait system from WEP: Struct and Trait System. Required features:

- [ ] Trait definitions and implementations
- [ ] Trait bounds in function signatures
- [ ] Associated types (`type Item`)
- [ ] Trait method calls
- [ ] Generic trait implementations (`impl<T>`)

### Phase 2: Core Iterator Traits

Implement the three core iterator traits in the standard library:

```wado
// core:iter module
trait Iterator {
    type Item;

    fn next(&mut self) -> Option<Self::Item>;

    // Default implementations for common methods
    fn collect<C: FromIterator<Self::Item>>(self) -> C {
        return C::from_iter(self);
    }

    fn map<U, F: Fn(Self::Item) -> U>(self, f: F) -> Map<Self, F> { ... }
    fn filter<F: Fn(&Self::Item) -> bool>(self, f: F) -> Filter<Self, F> { ... }
    fn fold<U, F: Fn(U, Self::Item) -> U>(self, init: U, f: F) -> U { ... }
}

trait IntoIterator {
    type Item;
    type Iter: Iterator<Item = Self::Item>;

    fn into_iter(self) -> Self::Iter;
}

trait FromIterator<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Self;
}
```

### Phase 3: Tuple Iterator Implementations

Implement `IntoIterator` for homogeneous tuples:

```wado
// Compiler-generated implementations for tuples
impl<T> IntoIterator for [T] { ... }
impl<T> IntoIterator for [T, T] { ... }
impl<T> IntoIterator for [T, T, T] { ... }
// ... up to reasonable tuple size (e.g., 32 elements)
```

### Phase 4: Array FromIterator Implementation

Implement `FromIterator` for `Array<T>`:

```wado
// core:prelude
impl<T> FromIterator<T> for Array<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Array<T> {
        let mut arr: Array<T> = [];
        for let item of iter {
            arr.append(item);
        }
        return arr;
    }
}
```

### Phase 5: Compiler Iterator Coercion

Add iterator-based coercion to the resolver:

1. **Detection**: When types don't match, check for `IntoIterator` + `FromIterator`
2. **Desugaring**: Rewrite expression to `T::from_iter(expr.into_iter())`
3. **Type checking**: Verify trait bounds and element type compatibility
4. **Error reporting**: Clear messages for missing trait implementations

### Phase 6: Optimization Pass

Add compiler optimizations to avoid iterator overhead:

1. **Constant folding**: Literal tuples → direct array construction
   ```wado
   let a: Array<i32> = [1, 2, 3];
   // Optimized to: ArrayLiteral { elements: [1, 2, 3], used: 3 }
   ```

2. **Inline `from_iter`**: Inline `FromIterator::from_iter` calls when possible
3. **Iterator fusion**: Combine multiple iterator operations into single loop

### Phase 7: Remove Hardcoded Coercion

Once iterator coercion is working:

1. Remove special-case tuple-to-array coercion from `resolver.rs`
2. Run full test suite to verify compatibility
3. Update compiler documentation

### Phase 8: Standard Library Extensions

Add `FromIterator` implementations for other collection types:

- `Set<T>`: From any `Iterator<Item = T>`
- `Dict<K, V>`: From `Iterator<Item = [K, V]>`
- Any future collection types

## Examples

### Basic Array Coercion

```wado
use {println} from "core:cli";

fn run() with Stdout {
    // Tuple literal coerces to Array
    let numbers: Array<i32> = [1, 2, 3, 4, 5];

    // Works in function arguments
    fn sum(items: Array<i32>) -> i32 {
        let mut total = 0;
        for let n of items {
            total += n;
        }
        return total;
    }

    let result = sum([10, 20, 30]);  // Implicit coercion
    println(`Sum: {result}`);  // Sum: 60

    // Explicit cast
    let explicit = [1, 2, 3] as Array<i32>;
}
```

### User-Defined Collection

```wado
// User-defined immutable vector
struct ImmutableVec<T> {
    items: Array<T>,
}

impl<T> ImmutableVec<T> {
    fn len(&self) -> i32 {
        return self.items.len();
    }

    fn get(&self, idx: i32) -> &T {
        return &self.items[idx];
    }
}

// Implement FromIterator to enable literal coercion
impl<T> FromIterator<T> for ImmutableVec<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> ImmutableVec<T> {
        let items = Array::from_iter(iter);
        return ImmutableVec { items };
    }
}

fn run() {
    // Now tuple literals work automatically!
    let vec: ImmutableVec<i32> = [1, 2, 3, 4, 5];

    assert vec.len() == 5;
    assert *vec.get(0) == 1;
}
```

### Iterator Method Chaining

```wado
fn run() {
    // Filter and map with automatic coercion
    let squares: Array<i32> = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        .into_iter()
        .filter(|x| x % 2 == 0)
        .map(|x| x * x)
        .collect();

    // squares = [4, 16, 36, 64, 100]
    assert squares.len() == 5;
}
```

### Set with Deduplication

```wado
// Hypothetical Set implementation
struct Set<T> {
    items: Array<T>,
}

impl<T: Eq + Hash> FromIterator<T> for Set<T> {
    fn from_iter<I: Iterator<Item = T>>(iter: I) -> Set<T> {
        let mut set = Set { items: [] };
        for let item of iter {
            if !set.contains(&item) {
                set.items.append(item);
            }
        }
        return set;
    }
}

fn run() {
    // Duplicates automatically removed
    let unique: Set<i32> = [1, 2, 2, 3, 3, 3, 4];
    assert unique.len() == 4;  // [1, 2, 3, 4]
}
```

### Dict from Tuple Pairs

```wado
// Hypothetical Dict implementation
impl<K: Eq + Hash, V> FromIterator<[K, V]> for Dict<K, V> {
    fn from_iter<I: Iterator<Item = [K, V]>>(iter: I) -> Dict<K, V> {
        let mut dict = Dict::new();
        for let [k, v] of iter {
            dict.insert(k, v);
        }
        return dict;
    }
}

fn run() {
    // Config dict from tuple of tuples
    let config: Dict<String, i32> = [
        ["width", 1920],
        ["height", 1080],
        ["fps", 60],
    ];

    assert config.get("width") == Some(1920);
}
```

### Empty Collection

```wado
fn run() {
    // Empty tuple coerces to empty collections
    let empty_array: Array<i32> = [];
    let empty_set: Set<String> = [];
    let empty_dict: Dict<i32, String> = [];

    assert empty_array.len() == 0;
    assert empty_set.len() == 0;
    assert empty_dict.len() == 0;
}
```

### Error Cases

```wado
fn run() {
    // ERROR: Heterogeneous tuple cannot be coerced
    let mixed = [1, "hello", true];  // [i32, String, bool]
    let arr: Array<i32> = mixed;
    // error: type [i32, String, bool] does not implement IntoIterator

    // ERROR: Element type mismatch
    let numbers = [1, 2, 3];  // [i32, i32, i32]
    let strings: Array<String> = numbers;
    // error: cannot coerce [i32, i32, i32] to Array<String>
    //        element types do not match (i32 vs String)
}
```

## Comparison with Other Languages

| Language | Literal Syntax | Default Type | Coercion Mechanism |
|----------|----------------|--------------|-------------------|
| **Wado** | `[1, 2, 3]` | Tuple `[i32, i32, i32]` | `IntoIterator` + `FromIterator` |
| Rust | `vec![1, 2, 3]` | `Vec<i32>` (macro) | Explicit `.collect()` |
| TypeScript | `[1, 2, 3]` | `number[]` | Type annotation for tuples |
| Python | `[1, 2, 3]` | `list` | Constructor: `set([1, 2, 3])` |
| Swift | `[1, 2, 3]` | `[Int]` | No coercion to Set/Dict |

Wado's approach is unique in providing **implicit coercion via traits** while maintaining **type safety** and **extensibility**.

## Related WEPs

- [WEP: Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md) - Defines tuple literal syntax
- [WEP: Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) - Foundation for trait-based coercion
- [WEP: Literal Type Conversion Rules](./wep-2026-01-12-literal-type-conversion.md) - General type conversion philosophy

## References

- [Rust Iterator trait](https://doc.rust-lang.org/std/iter/trait.Iterator.html)
- [Rust IntoIterator trait](https://doc.rust-lang.org/std/iter/trait.IntoIterator.html)
- [Rust FromIterator trait](https://doc.rust-lang.org/std/iter/trait.FromIterator.html)
- [Rust collect() method](https://doc.rust-lang.org/std/iter/trait.Iterator.html#method.collect)
- [TypeScript Tuple Types](https://www.typescriptlang.org/docs/handbook/2/objects.html#tuple-types)
