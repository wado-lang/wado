# WEP: Trait Bounds Enforcement

## Context

Trait bounds syntax (`T: Trait`, `T: Trait1 + Trait2`) is already parsed and stored in AST/TIR, and struct instantiation bounds are enforced. However, function and method trait bounds are not enforced at call sites. This means any type can be passed to a bounded generic function without error.

More importantly, there is no way to express conditional method availability. For example, `Array<T>::sort()` uses the `<` operator on `T`, but there is no mechanism to restrict this method to types that implement `Ord`. Currently it compiles for any `T` and fails at codegen or produces incorrect code if `T` doesn't support `<`.

## Decision

### 1. Enforce Function/Method Trait Bounds at Call Sites

When a generic function or method with trait bounds is called, the elaborator checks that each concrete type argument satisfies all declared bounds.

```wado
fn max<T: Ord>(a: T, b: T) -> T {
    if a > b { return a; }
    return b;
}

max(1, 2);           // OK: i32 implements Ord
max("a", "b");       // OK: String implements Ord

struct Foo {}
max(Foo {}, Foo {}); // ERROR: type 'Foo' does not implement trait 'Ord'
                     //        required by bound on 'T'
```

### 2. Bounded `impl` Blocks for Conditional Methods

An `impl` block can declare trait bounds on its type parameters. Methods inside such a block are only available when the bounds are satisfied.

```wado
// Available for all Array<T>
impl Array<T> {
    pub fn len(&self) -> i32 { ... }
    pub fn append(&mut self, value: T) { ... }
}

// Only available when T: Ord
impl Array<T: Ord> {
    pub fn sort(&mut self) { ... }
    pub fn sorted(&self) -> Array<T> { ... }
}
```

```wado
let mut nums: Array<i32> = [3, 1, 2];
nums.sort();        // OK: i32 implements Ord

struct Foo {}
let mut foos: Array<Foo> = [];
foos.push(Foo {}); // OK: push has no bounds
foos.sort();         // ERROR: type 'Foo' does not implement trait 'Ord'
                     //        required by bound on 'T'
```

### 3. Bounds on Trait Implementations

Trait `impl` blocks can also have bounds, restricting when a trait is implemented.

```wado
// Array<T> implements Eq only when T implements Eq
impl Eq for Array<T: Eq> {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() { return false; }
        for let mut i = 0; i < self.len(); i += 1 {
            if self[i] != other[i] { return false; }
        }
        return true;
    }
}
```

This makes trait satisfaction propagate: `Array<i32>` implements `Eq` because `i32` implements `Eq`, but `Array<Foo>` does not unless `Foo` implements `Eq`.

### 4. Scope

The following are in scope:

- Enforce bounds on generic function/method calls
- Bounded `impl` blocks (conditional methods and trait impls)
- Bound propagation for `type_implements_trait` checks
- Multiple bounds with `+` syntax

The following are out of scope (future work):

- `where` clauses
- Trait objects (`&dyn Trait`)
- Higher-kinded bounds
- Default trait method implementations

## Implementation Strategy

### Elaborator Changes

1. In `resolve_call` / `resolve_method_call`: after resolving type arguments, check each type argument against its declared bounds using the existing `type_implements_trait`.

2. In `lookup_method_info`: when multiple `impl` blocks exist for a type, filter out methods from bounded `impl` blocks whose bounds are not satisfied by the current type arguments.

3. In `type_implements_trait`: when checking if `Array<Foo>` implements `Eq`, find the `impl Eq for Array<T: Eq>` block, substitute `T = Foo`, then recursively check `Foo: Eq`.

### Stdlib Changes

Move `sort`, `sorted`, `sorted_by` to a bounded `impl` block:

```wado
impl Array<T: Ord> {
    pub fn sort(&mut self) { ... }
    pub fn sorted(&self) -> Array<T> { ... }
    pub fn sorted_by(&self, cmp: fn(&T, &T) -> Ordering) -> Array<T> { ... }
}
```

## Consequences

### Positive

- Type errors caught at compile time instead of codegen or runtime
- Conditional methods express API constraints precisely
- Enables safe generic algorithms (sort, search, comparison)
- Foundation for richer trait-based APIs (e.g., `FromIterator` with bounds)

### Negative

- Adds complexity to method resolution (must check bounds during lookup)
- Error messages need care to be clear about which bound is unsatisfied and why
- Existing code that calls bounded methods on unbounded types will get new compile errors (intentional)
