# Fix Plan: Generic Struct Field Monomorphization

## Current Status

TypeTable sharing via `Rc<RefCell<>>` is complete and working. However, the test still fails because of issues in the lowering phase.

## Root Cause Analysis

### Issue 1: StructLiteral Not Updated During Function Instantiation

When `lower.rs::instantiate_function()` creates `Container$i32::new()` from `Container::new()`, it:
- ✅ Rewrites the return type TypeId correctly (from Container<T> to Container$i32)
- ❌ Does NOT update `struct_name` in StructLiteral expressions
- ❌ Does NOT update `struct_type` TypeId in StructLiteral expressions

**Evidence:**
```rust
// In Container$i32::new() body:
StructLiteral {
    struct_type: 35,           // TypeId 35 = generic "Container" (WRONG!)
    struct_name: "Container",  // Should be "Container$i32" (WRONG!)
    fields: [...]
}
```

Should be:
```rust
StructLiteral {
    struct_type: 34,           // TypeId 34 = monomorphized "Container$i32"
    struct_name: "Container$i32",
    fields: [...]
}
```

### Issue 2: Generic Template Structs Reaching Codegen

The generic template `struct Container<T>` is still present in the lowered output and reaches codegen, which tries to register it and fails because field `items: Array<T>` contains TypeParam.

**Evidence:**
- TypeId 35 = `Struct { name: "Container", module_path: [] }` (generic template)
- TypeId 34 = `Struct { name: "Container$i32", module_path: [] }` (monomorphized)
- Codegen tries to register both, fails on the template

Codegen already has logic to skip generic templates for functions, but needs the same for structs.

## Fix Plan

### Phase 1: Fix StructLiteral Rewriting in Lowering ✅ PRIORITY

**File:** `wado-compiler/src/lower.rs`
**Method:** `instantiate_function()`

**Steps:**
1. Add a helper method `rewrite_types_in_expr()` that recursively walks TirExpr
2. When encountering `TirExprKind::StructLiteral`:
   - Check if `struct_type` TypeId needs substitution
   - If the substituted type is a monomorphized struct, update `struct_name` to match
3. Call this helper on the function body after cloning

**Implementation:**
```rust
fn rewrite_types_in_expr(
    &self,
    expr: &mut TirExpr,
    subst_ctx: &SubstitutionContext,
    type_table: &mut TypeTable,
) {
    // Rewrite expr.type_id
    expr.type_id = subst_ctx.substitute(expr.type_id, type_table);

    match &mut expr.kind {
        TirExprKind::StructLiteral { struct_type, struct_name, fields } => {
            // Rewrite struct_type
            *struct_type = subst_ctx.substitute(*struct_type, type_table);

            // Update struct_name if it's now monomorphized
            if let ResolvedType::Struct { name, .. } = type_table.get(*struct_type) {
                *struct_name = name.clone();
            }

            // Recursively rewrite field values
            for field in fields {
                self.rewrite_types_in_expr(&mut field.value, subst_ctx, type_table);
            }
        }
        // ... handle other expr kinds recursively
    }
}
```

### Phase 2: Skip Generic Template Structs in Codegen (Defense in Depth)

**File:** `wado-compiler/src/codegen.rs`
**Location:** Around lines 866-891 (loaded module structs registration)

**Steps:**
1. The code already skips generic templates with `if !tir_struct.type_params.is_empty()`
2. But the generic template `Container` has `type_params: []` in the lowered output
3. Need to check if the struct contains TypeParam in any field types

**Implementation:**
```rust
// In register phase, add additional check:
fn struct_contains_type_params(&self, tir_struct: &TirStruct, type_table: &TypeTable) -> bool {
    for field in &tir_struct.fields {
        if type_table.contains_type_param(field.type_id) {
            return true;
        }
    }
    false
}

// Then in registration loop:
if !tir_struct.type_params.is_empty()
    || self.struct_contains_type_params(tir_struct, &*tir_mod.type_table.borrow())
{
    continue;  // Skip generic templates
}
```

### Phase 3: Verify and Test

1. Run the test: `cargo test -p wado-compiler --test e2e fixture_test_o0::generic_struct_field_monomorphization_wado`
2. Verify output with: `cargo run --bin wado -- run wado-compiler/tests/fixtures/generic-struct-field-monomorphization.wado`
3. Update test expectation to expect success
4. Run full test suite to ensure no regressions

### Phase 4: Additional Test Coverage

Create additional tests for:
- Generic struct with multiple fields using type parameters
- Nested generic structs (e.g., `Container<Box<T>>`)
- Generic struct with methods that create instances
- Multiple monomorphizations of the same generic struct

## Implementation Order

1. **First:** Fix Phase 2 (defense in depth) - Quick win, prevents crashes
2. **Second:** Fix Phase 1 (root cause) - Proper fix in lowering
3. **Third:** Test and verify
4. **Fourth:** Add comprehensive tests

## Expected Outcome

After fixes:
- ✅ `Container$i32::new()` returns `Container$i32 { items: [] as Array<i32> }`
- ✅ Generic template `Container<T>` is skipped by codegen
- ✅ Test passes with output: "Length: 3\nFirst: 10\nSecond: 20\nThird: 30\n"
- ✅ All existing tests still pass

## Alternative Approach (Not Recommended)

Remove generic templates from the module after lowering, so they never reach codegen. This would work but:
- Loses information that might be useful for debugging
- Harder to implement (requires filtering structs from modules)
- Doesn't fix the root cause in StructLiteral rewriting
