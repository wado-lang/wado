# Generic Struct Field Monomorphization Issue

## Status: Root Cause Identified

## Problem
Cross-module generic type references fail during monomorphization.

Example:
```wado
// main module
struct Container<T> {
    items: Array<T>,  // Array<T> is from prelude module
}
```

When monomorphizing `Container<i32>`, the `T` in `Array<T>` is NOT substituted to `i32`.

## Root Cause

### Level 1: TypeTable Cloning in Resolver
In `resolver.rs` line 562:
```rust
let mut resolver = Resolver {
    type_table: type_table.clone(),  // ← THIS IS THE PROBLEM!
    ...
};
```

Each module gets its own **cloned** TypeTable, not a shared one.

### Level 2: TypeId Mismatch Across Modules
- `Container<T>` is resolved in main module's TypeTable
- Its field `items: Array<T>` references prelude module's TypeTable
- The TypeParam `T` in `Array<T>` has TypeId 21 in prelude's TypeTable
- When `substitute_type` tries to resolve TypeId 21 using main module's TypeTable, it fails
- TypeId 21 in main module != TypeParam{T} in prelude module

### Evidence
Error message shows:
```
Array struct type not registered for element type 21 (resolved: TypeParam { name: "T", index: 0 })
Available array types: [2]
```

## Solution

### Complete Fix (Required)
Replace `TypeTable` with `Rc<RefCell<TypeTable>>` to enable true sharing:

1. **In `resolver.rs`**:
   ```rust
   pub struct Resolver<'a> {
       type_table: Rc<RefCell<TypeTable>>,  // instead of: TypeTable
       ...
   }
   ```

2. **In `tir.rs`**:
   ```rust
   pub struct TirModule {
       pub type_table: Rc<RefCell<TypeTable>>,  // instead of: TypeTable
       ...
   }
   ```

3. **Update all access**:
   - Change `self.type_table.method()` to `self.type_table.borrow_mut().method()`
   - Change `type_table.get()` to `type_table.borrow().get()`

### Files to Modify
- `wado-compiler/src/resolver.rs` - Resolver struct and resolve_all_modules
- `wado-compiler/src/tir.rs` - TirModule struct
- `wado-compiler/src/lower.rs` - All type_table accesses
- `wado-compiler/src/codegen.rs` - All type_table accesses

### Estimated Effort
- ~200-300 lines of code changes
- Multiple compilation iterations to fix borrow checker issues
- Thorough testing required

## Current Workarounds

### Test Expectations
`generic-struct-field-monomorphization.wado` expects `compile_error` until fixed.

### Codegen Improvements
Added in this PR:
- Skip generic template functions/methods to avoid processing unsubstituted types
- Improved error messages showing TypeParam details

## Next Steps
1. Create separate PR for Rc<RefCell<TypeTable>> refactoring
2. Run full test suite after refactoring
3. Update this test to expect success

## References
- Issue location: `wado-compiler/src/resolver.rs:562`
- Error location: `wado-compiler/src/codegen.rs:4088`
- Test: `wado-compiler/tests/fixtures/generic-struct-field-monomorphization.wado`
