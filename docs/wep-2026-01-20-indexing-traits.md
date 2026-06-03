# Indexing Traits Design

This WEP defines the trait system for indexing operations (`[]` operator) in Wado.

## Context

Wado needs traits to support indexing operations on collections like `List<T>` and user-defined types. The design must account for:

1. **Four distinct operations**:
   - Read (reference): `let x = container[i]` where container returns `&T`
   - Read (value): `let x = container[i]` where container returns `T` by value
   - Mutable access: `container[i].method()` where method takes `&mut self`
   - Assignment: `container[i] = value`

2. **Wasm GC constraints**: In Wasm GC, `array.get` returns a value for primitives but a reference for reference types. You cannot get a mutable reference to a primitive array element.

3. **Flexibility**: Different collection types may support different subsets of operations.

## Decision

Split indexing into four independent traits:

```wado
/// Read-only indexing returning a reference: container[index] -> &Output
pub trait Index<IndexType> {
    type Output;
    fn index(&self, index: IndexType) -> &Self::Output;
}

/// Read-only indexing returning a value: container[index] -> Output
/// Use this for containers of primitives where references cannot be returned.
pub trait IndexValue<IndexType> {
    type Output;
    fn index_value(&self, index: IndexType) -> Self::Output;
}

/// Mutable access: container[index].mutating_method()
pub trait IndexMut<IndexType> {
    type Output;
    fn index_mut(&mut self, index: IndexType) -> &mut Self::Output;
}

/// Assignment: container[index] = value
pub trait IndexAssign<IndexType> {
    type Input;
    fn index_assign(&mut self, index: IndexType, value: Self::Input);
}
```

### Design Rationale

#### Why Four Traits?

The key insight is that `Index` (returning `&Output`) and `IndexValue` (returning `Output`) serve different use cases:

- `Index` returns a reference - works for containers storing reference types
- `IndexValue` returns a copy - necessary for containers storing primitives

This separation is semantically honest about Wasm GC's constraints rather than hiding them behind leaky abstractions like proxy objects.

#### Why Not Proxy Objects?

C++'s `vector<bool>` uses proxy objects that pretend to be references. This is widely considered a design mistake because:

- Proxies don't behave like real references (`auto x = vec[i]` captures proxy, not value)
- Template code breaks unexpectedly
- Mental model mismatch causes bugs

By using `IndexValue`, we're explicit: "you get a copy, not a reference."

#### IndexMut Without Index?

`IndexMut` does NOT require `Index` as a supertrait because:

- A container might support mutable access but not immutable reference return
- For value types, `IndexValue` provides the read capability instead

### Compiler Resolution

The compiler desugars `[]` syntax based on which traits are implemented:

#### For Read (`let x = container[i]`)

1. Check `Index<Idx>` → generate `*container.index(i)`
2. Else check `IndexValue<Idx>` → generate `container.index_value(i)`
3. Else error

#### For Method Call (`container[i].method()`)

1. If method takes `&mut self`:
   - Check `IndexMut<Idx>` → generate `container.index_mut(i).method()`
   - Else error: "cannot mutate indexed value"
2. If method takes `&self`:
   - Check `Index<Idx>` or `IndexMut<Idx>` → use reference
   - Else check `IndexValue<Idx>` → generate `container.index_value(i).method()` (method called on temporary)
3. Else error

#### For Assignment (`container[i] = value`)

1. Check `IndexAssign<Idx>` → generate `container.index_assign(i, value)`
2. Else error

### Use Cases

#### Index Only (Reference Types)

| Type             | Description                                       |
| ---------------- | ------------------------------------------------- |
| `List<Struct>`   | Returns `&Struct` - actual reference to GC object |
| Custom container | User-defined container with reference storage     |
| Tree nodes       | `tree[path]` returns reference to node            |

#### IndexValue Only (Value Types)

| Type               | Description                                   |
| ------------------ | --------------------------------------------- |
| `List<i32>`        | Returns `i32` by value - cannot return `&i32` |
| Computed sequences | Fibonacci where `fib[n]` computes on demand   |
| `RangeExclusive`   | `range[i]` computes i-th value, no storage    |
| Packed bit arrays  | `bits[i]` returns extracted bool              |

#### IndexValue + IndexAssign (Primitive Arrays)

| Type           | Description                               |
| -------------- | ----------------------------------------- |
| `List<i32>`    | Read returns copy, write replaces element |
| `List<f64>`    | Same - Wasm GC constraint                 |
| Remote storage | Can GET/PUT values but no live references |

#### Index + IndexMut + IndexAssign (Full Access)

| Type           | Description                                        |
| -------------- | -------------------------------------------------- |
| `List<Struct>` | Full read/mutate/write for reference element types |
| Custom maps    | Full access to stored reference values             |

### Implementation for List

```wado
// For ALL element types: value-based read and assignment
impl IndexValue<i32> for List<T> {
    type Output = T;
    fn index_value(&self, index: i32) -> Self::Output {
        return builtin::array_get::<T>(self.repr, index);
    }
}

impl IndexAssign<i32> for List<T> {
    type Input = T;
    fn index_assign(&mut self, index: i32, value: Self::Input) {
        builtin::array_set::<T>(self.repr, index, value);
    }
}

// For reference element types only (when trait bounds are available):
// impl Index<i32> for List<T> where T: Reference { ... }
// impl IndexMut<i32> for List<T> where T: Reference { ... }
```

### Optimization: Pattern Recognition

After inlining `IndexValue::index_value` and `IndexAssign::index_assign` to `builtin::array_get`/`builtin::array_set`, the optimizer can recognize patterns:

```wado
// Source
arr[i] = arr[i] + 1;

// After desugaring
arr.index_assign(i, arr.index_value(i) + 1);

// After inlining
builtin::array_set(repr, i, builtin::array_get(repr, i) + 1);

// Optimizer can potentially fuse to read-modify-write
```

This keeps the semantic layer clean while allowing low-level optimization.

## Consequences

### Advantages

1. **Semantically honest**: `IndexValue` clearly means "you get a copy"
2. **Wasm GC compatible**: No fake references for primitives
3. **General**: Works for any `Container<Primitive>`, not just List
4. **No proxy objects**: Avoids C++ `vector<bool>` mistakes
5. **Flexible**: Collections implement only what they support
6. **Optimizable**: Inlining enables pattern recognition

### Trade-offs

1. **Four traits**: More traits to understand than Rust's two
2. **No `arr[i].mutate()` for primitives**: But this is honest - you CAN'T mutate a copy
3. **Learning curve**: Users must understand Index vs IndexValue distinction

### Error Messages

The compiler should provide clear errors:

```
error: cannot mutate indexed value
  --> file.wado:10:5
   |
10 |     arr[i].increment();
   |     ^^^^^^^^^^^^^^^^^^
   |
   = note: `List<i32>` implements `IndexValue`, not `IndexMut`
   = note: primitive array elements cannot be mutated in place
   = help: use `arr[i] = arr[i] + 1` instead
```

## Related

- [Associated Types](./wep-2026-01-20-associated-types.md) - Required for `type Output` in traits
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md) - General operator trait design
