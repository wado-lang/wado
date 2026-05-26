# Compile-Time Tuple Enumeration

## Context

Tuples in Wado are heterogeneous: each element can have a different type (`[i32, String, f64]`). This means tuples cannot be iterated at runtime using the standard `Iterator` trait, because `next()` must return a single, fixed type.

However, several use cases require processing each element of a tuple individually:

1. **Tagged template literals**: A tag function receives interpolated values as a tuple and must process each value according to its type (e.g., `sql` tag converting each value to a SQL parameter)
2. **Serialization**: Converting a tuple of heterogeneous values into a serialized form
3. **Structured logging**: Formatting each value with type-appropriate formatting

The key insight is that for tuples, the element count and element types are always known at compile time. The compiler can **unroll** a loop over a tuple into a sequence of statements, each typed independently.

### Alternatives Considered

#### Builder Protocol

Instead of iterating over a tuple, define a trait with `begin`/`add`/`finish` methods:

```wado
trait TemplateTag {
    type Output;
    fn begin(strings: CookedStrings) -> Self;
    fn add<T>(&mut self, value: T);
    fn finish(self) -> Self::Output;
}
```

The compiler generates sequential `add()` calls for each interpolated value.

- Does not require a new language feature
- More boilerplate for tag authors (struct + trait impl + 3 methods vs 1 function)
- Less general (only useful for builder-pattern scenarios)

#### Type-Erased Array

Pass all values as `Array<String>` (pre-stringified):

- Simple, no new features needed
- Loses original type information (tag function cannot distinguish `{42}` from `{"42"}`)
- Not general enough for type-safe DSLs like SQL query builders

## Decision

Adopt compile-time tuple enumeration as a general language feature. When a `for-of` loop targets a tuple with a concrete type, the compiler unrolls the loop body at compile time, binding each element with its specific type.

### Syntax

Two forms, consistent with array iteration:

```wado
// Value-only iteration
for let v of tuple_expr {
    // v has type T_k in the k-th unrolled iteration
}

// Indexed iteration
for let [i, v] of tuple_expr.enumerate() {
    // i: i32 (compile-time constant, 0, 1, 2, ...)
    // v has type T_k in the k-th unrolled iteration
}
```

For arrays, `for let v of array` uses `IntoIterator` at runtime. For tuples, the same syntax triggers compile-time unrolling. The distinction is determined by the type of the iterable expression.

### `.enumerate()` Pseudo-Method

`.enumerate()` on a tuple type is a compiler-recognized pattern, not a real method. It produces `[index, element]` pairs for compile-time enumeration. This follows Rust's naming convention for indexed iteration (`iter().enumerate()`), but applied directly to the tuple since tuples cannot produce a runtime iterator.

| Receiver type | `.enumerate()` behavior         | Returns                     |
| ------------- | ------------------------------- | --------------------------- |
| Iterator      | Runtime combinator (Rust-style) | `EnumerateIter<T>`          |
| Tuple         | Compile-time pseudo-method      | Unrolled `[i32, T_k]` pairs |

### Expansion Rules

Given a concrete tuple type `[T_0, T_1, ..., T_{n-1}]`:

```wado
for let [i, v] of values.enumerate() {
    body(i, v);
}
```

Expands to:

```wado
__unroll_0: {
    let i: i32 = 0;
    let v: T_0 = values.0;
    body(i, v);
}
__unroll_1: {
    let i: i32 = 1;
    let v: T_1 = values.1;
    body(i, v);
}
// ...
__unroll_{n-1}: {
    let i: i32 = {n-1};
    let v: T_{n-1} = values.{n-1};
    body(i, v);
}
```

Each unrolled block is a labeled block with a compiler-generated label. Each block is type-checked independently, because `v` has a different type in each block.

Value-only `for let v of values` works the same way without the `i` binding.

### Where Expansion Happens

Expansion occurs in the **elaborator during monomorphization**, not in the desugar phase. This is because the concrete tuple type is only known after type parameter substitution.

Flow:

1. A generic function `fn tag<Values>(strings: CookedStrings, values: Values)` is defined
2. At a call site, `Values` is instantiated to a concrete tuple type (e.g., `[i32, String]`)
3. The elaborator monomorphizes the function body with `Values = [i32, String]`
4. During monomorphization, the elaborator encounters `for let [i, v] of values.enumerate()`
5. `values` has concrete type `[i32, String]` -> the elaborator unrolls the loop
6. Each unrolled body is resolved independently with the appropriate element type

### Type Checking Model

Wado uses the **C++ template model** for tuple enumeration: the loop body is type-checked only after expansion, when the concrete element type is known.

```wado
fn process<Values>(values: Values) {
    for let v of values {
        v.some_method();  // Not checked until Values is concrete
    }
}
```

At definition time, the compiler does not verify that each element type has `some_method()`. This check happens at monomorphization time, when the concrete tuple type is known. If an element type lacks the required method, the compiler reports an error pointing to both the call site (where the concrete type is determined) and the method call (where the error occurs).

Future work: trait bounds on tuple element types (e.g., `Values: TupleOf<ToSqlParam>`) could enable definition-time checking, but this is deferred as YAGNI.

### Concrete Example

```wado
trait ToSqlParam {
    fn to_sql_param(&self) -> SqlParam;
}

impl ToSqlParam for i32 {
    fn to_sql_param(&self) -> SqlParam { return SqlParam::Int(*self); }
}

impl ToSqlParam for String {
    fn to_sql_param(&self) -> SqlParam { return SqlParam::Text(*self); }
}

fn sql<Values>(strings: CookedStrings, values: Values) -> SqlQuery {
    let mut query = strings[0];
    let mut params: Array<SqlParam> = [];
    for let [i, v] of values.enumerate() {
        params.push(v.to_sql_param());
        query.push_str("?");
        query.push_str(strings[i + 1]);
    }
    return SqlQuery { query, params };
}
```

When called as `sql::<[i32, String]>(strings, [42, "Alice"])`, the loop body expands to:

```wado
__unroll_0: {
    let i: i32 = 0;
    let v: i32 = values.0;
    params.push(v.to_sql_param());        // resolves to i32::to_sql_param
    query.push_str("?");
    query.push_str(strings[1]);
}
__unroll_1: {
    let i: i32 = 1;
    let v: String = values.1;
    params.push(v.to_sql_param());        // resolves to String::to_sql_param
    query.push_str("?");
    query.push_str(strings[2]);
}
```

### Constraints

- **Concrete type required**: Tuple enumeration is only valid when the tuple type is fully concrete. Attempting to enumerate a generic `T` that has not been instantiated is a compile error
- **`break` and `continue` are not allowed**: Since the loop is unrolled into sequential blocks, `break`/`continue` have no natural target. They are compile errors inside tuple enumeration
- **Index is a compile-time constant**: The `i` binding from `.enumerate()` is a compile-time `i32` constant, usable in tuple/array indexing expressions
- **Empty tuple**: `for let v of []` produces zero unrolled blocks (no-op)
- **Nested enumeration**: Enumerating a tuple that contains tuples works; inner tuples are elements, not automatically flattened

### Error Reporting

Since type checking happens at monomorphization time, error messages must include:

1. The call site where the concrete tuple type was determined
2. The specific element index and type that caused the error
3. The location in the loop body where the error occurred

Example error:

```
error: method `to_sql_param` not found on type `bool`
  --> app.wado:10:5
   |
10 |     for let [i, v] of values.enumerate() {
   |                       ^^^^^^^^^^^^^^^^
   |                       element 2 of tuple [i32, String, bool]
   |
11 |         params.push(v.to_sql_param());
   |                       ^^^^^^^^^^^^^^^^ `bool` does not implement `ToSqlParam`
   |
note: tuple type determined here
  --> app.wado:20:9
   |
20 |     sql`SELECT * WHERE id = {id} AND name = {name} AND active = {is_active}`
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
```

## Implementation Plan

- [ ] Detect `for let v of <tuple>` and `for let [i, v] of <tuple>.enumerate()` patterns in the elaborator
- [ ] Implement loop unrolling during monomorphization
- [ ] Type-check each unrolled block independently
- [ ] Emit compile errors for `break`/`continue` inside tuple enumeration
- [ ] Implement error messages that show call site, element index, and body location
- [ ] Test with tagged template use cases

## Consequences

### Positive

- General-purpose feature useful beyond templates (serialization, logging, etc.)
- Tag functions are simple generic functions, no special protocol needed
- Type-safe: each element processed according to its type
- Zero runtime overhead: unrolled at compile time
- No dedicated syntax needed: reuses existing `for-of` syntax
- Orthogonal to other language features

### Negative

- Adopts C++ template model for type checking (monomorphization-time errors)
- Adds complexity to the elaborator's monomorphization logic
- `.enumerate()` as a pseudo-method is a novel concept that may surprise users
- Error messages require careful engineering to be useful

### Risks

- May create pressure for more compile-time metaprogramming features (tuple length, conditional compilation, etc.)
- Interaction with future trait bounds on tuple elements needs careful design
- Large tuples could produce significant code bloat from unrolling

## Related WEPs

- [String Template Desugaring](./wep-2026-01-20-string-template-desugaring.md): Primary motivation; tagged templates use tuple enumeration to process interpolated values
- [Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md): Defines tuple literal syntax and semantics

## References

- [Zig comptime](https://ziglang.org/documentation/master/#comptime): Zig's compile-time execution model, which includes tuple/struct field iteration
- [C++ fold expressions](https://en.cppreference.com/w/cpp/language/fold): C++17 mechanism for expanding parameter packs
- [Rust `macro_rules!` for tuples](https://doc.rust-lang.org/std/macro.impl_for_tuples.html): Rust uses macros to generate per-arity implementations (the problem Wado avoids)
