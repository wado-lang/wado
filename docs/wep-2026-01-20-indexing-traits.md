# Indexing Traits Design

This WEP defines the trait system for indexing operations (`[]` operator) in Wado.

## Context

Wado needs traits to support indexing operations on collections like `Array<T>`, `Dict<K, V>`, and user-defined types. The design must account for:

1. **Three distinct operations**:
   - Read: `let x = arr[i]`
   - Mutable access: `arr[i].method()` where method takes `&mut self`
   - Assignment: `arr[i] = value`

2. **Wasm GC constraints**: In Wasm GC, `array.get` returns a value (not a reference), and `array.set` takes a value. You cannot get a mutable reference to a primitive array element.

3. **Flexibility**: Different collection types may support different subsets of operations.

## Decision

Split indexing into three independent traits:

```wado
/// Read-only indexing: container[index] -> &Output
pub trait Index<IndexType> {
    type Output;
    fn index(&self, index: IndexType) -> &Self::Output;
}

/// Mutable access: container[index].mutating_method()
pub trait IndexMut<IndexType>: Index<IndexType> {
    fn index_mut(&mut self, index: IndexType) -> &mut Self::Output;
}

/// Assignment: container[index] = value
pub trait IndexAssign<IndexType> {
    type Input;
    fn index_assign(&mut self, index: IndexType, value: Self::Input);
}
```

### Design Rationale

#### Why Three Traits Instead of One?

A single `Indexable` trait would force all operations to be implemented together:

```wado
// NOT chosen: Forces all three operations
trait Indexable<Idx> {
    type Element;
    fn get(&self, idx: Idx) -> Self::Element;
    fn get_mut(&mut self, idx: Idx) -> &mut Self::Element;
    fn set(&mut self, idx: Idx, value: Self::Element);
}
```

This fails for collections that only support a subset of operations.

#### Why IndexMut Extends Index?

`IndexMut` requires `Index` as a supertrait because:

- Mutable access logically implies readable access
- They share the same `Output` type
- Rust uses this pattern successfully

#### Why IndexAssign is Independent?

`IndexAssign` is separate from `Index`/`IndexMut` because:

- Assignment doesn't require returning a reference
- `Input` type may differ from `Output` (e.g., accepting owned values vs returning references)
- Some types support read + assign but not mutable references (Wasm GC primitives)

### Use Cases

#### Index Only (Read-Only)

| Type               | Description                                          |
| ------------------ | ---------------------------------------------------- |
| Immutable strings  | `str[i]` returns char, but strings are immutable     |
| Range objects      | `range[i]` computes i-th value, no storage to mutate |
| Computed sequences | Fibonacci where `fib[n]` computes on demand          |
| Frozen collections | Immutable maps, frozen sets                          |
| Read-only views    | Slices without modification rights                   |
| Constant tables    | Configuration tables that should never be modified   |

#### Index + IndexAssign (No Mutable Access)

| Type               | Description                                            |
| ------------------ | ------------------------------------------------------ |
| Primitive arrays   | Wasm GC: can read/write `i32` but can't get `&mut i32` |
| Remote collections | Can GET/PUT but no live mutable references             |
| Database-backed    | Read/write records but no mutable references           |
| Copy-on-write      | Replace is cheap, mutable access requires copy         |

#### Index + IndexMut (No Assignment)

| Type                 | Description                                    |
| -------------------- | ---------------------------------------------- |
| Fixed object pools   | Mutate existing objects but can't replace them |
| Interned collections | Mutate properties but not object identity      |

#### All Three (Full Access)

| Type                                     | Description                   |
| ---------------------------------------- | ----------------------------- |
| `Array<T>` where T is a reference type   | Full read/mutate/write access |
| `Dict<K, V>` where V is a reference type | Full access to values         |

### Compiler Desugaring

The compiler desugars `[]` syntax based on context:

```wado
// Read context
let x = arr[i];
// Desugars to:
let x = *arr.index(i);

// Mutable method call
arr[i].push(value);
// Desugars to:
arr.index_mut(i).push(value);

// Assignment
arr[i] = value;
// Desugars to:
arr.index_assign(i, value);
```

### Implementation for Array

```wado
// For reference element types (T where T is not a primitive)
impl Index<i32> for Array<T> {
    type Output = T;
    fn index(&self, index: i32) -> &Self::Output {
        return builtin::array_get_ref(self.repr, index);
    }
}

impl IndexMut<i32> for Array<T> {
    fn index_mut(&mut self, index: i32) -> &mut Self::Output {
        return builtin::array_get_mut_ref(self.repr, index);
    }
}

impl IndexAssign<i32> for Array<T> {
    type Input = T;
    fn index_assign(&mut self, index: i32, value: Self::Input) {
        builtin::array_set(self.repr, index, value);
    }
}

// For primitive types (i32, f64, etc.) - only Index and IndexAssign
// IndexMut is NOT implemented because Wasm GC cannot provide &mut to primitives
```

## Consequences

### Advantages

1. **Flexibility**: Collections implement only the operations they support
2. **Type safety**: Compile-time errors for unsupported operations
3. **Wasm GC compatible**: Primitive arrays work without fake mutable references
4. **Clear semantics**: Each trait has one responsibility
5. **Familiar**: Similar to Rust's `Index`/`IndexMut` with Wasm-specific `IndexAssign`

### Trade-offs

1. **More traits to implement**: Full access requires three impl blocks
2. **Learning curve**: Users must understand when each trait applies
3. **Potential confusion**: `IndexMut` vs `IndexAssign` naming may need explanation

### Migration Path

Existing code using explicit method calls (`arr.get(i)`, `arr.set(i, v)`) continues to work. The `[]` syntax is purely ergonomic sugar.

## Implementation Status

- [x] Trait definitions in `core:prelude` (Index, IndexMut, IndexAssign)
- [x] `IndexAssign<i32>` implementation for `Array<T>`
- [x] Compiler: `[]` read desugaring to `Index::index()` for custom types
- [x] Compiler: `[]` assignment desugaring to `IndexAssign::index_assign()` for custom types
- [x] Direct codegen for `Array<T>` indexing (optimized path)
- [x] `IndexMut` desugaring for mutable method calls (`arr[i].method()`)
- [ ] `Index` and `IndexMut` for `Array<T>` with reference element types
- [ ] Supertrait syntax (`trait IndexMut<Idx>: Index<Idx>`)

## Related

- [Associated Types](./wep-2026-01-20-associated-types.md) - Required for `type Output` in traits
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md) - General operator trait design
