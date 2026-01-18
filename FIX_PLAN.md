# Fix Plan: Generic Struct Field Monomorphization

## Current Status (Updated 2026-01-18)

✅ **Completed:**
1. TypeTable sharing via `Rc<RefCell<>>` - DONE
2. Defense in codegen to skip generic templates - DONE
3. GenericInstance empty type_args handling - DONE
4. StructLiteral substitution during monomorphization - DONE

🔶 **Current Error:** `unknown method: /Container$i32::add`

❌ **Remaining Issue:** Methods (add, len, get) not being monomorphized for `Container$i32`

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

## Implementation Summary

### What Was Accomplished

#### 1. TypeTable Sharing (Commit e39fb3f)
- Changed `TirModule.type_table` from `TypeTable` to `Rc<RefCell<TypeTable>>`
- Changed `Resolver.type_table` similarly
- Updated all ~200+ call sites across multiple files
- ✅ Result: All modules now share a single TypeTable, enabling cross-module type references

#### 2. Defense in Codegen (Commit 71a482e)
- Added `struct_contains_type_params()` helper method
- Modified struct registration phases to skip structs with TypeParam in fields
- ✅ Result: Generic template structs (e.g., `Container<T>`) no longer crash codegen

####3. GenericInstance Empty Type Args Fix (Commit 71a482e)
- Added check in `collect_instantiation_sites()` to skip `GenericInstance` with empty `type_args`
- Modified `substitute_type()` to handle empty type_args by inferring from substitution context
- ✅ Result: Invalid monomorphizations with empty type_args no longer created

#### 4. StructLiteral Substitution Fix (Commit 71a482e)
- Updated `substitute_types_in_expr()` to correctly substitute StructLiteral struct_type
- Added logic to update struct_name to match monomorphized type
- ✅ Result: `Container { items: [] }` now correctly becomes `Container$i32 { items: [] }`

### Current Test Status

**Test:** `generic-struct-field-monomorphization.wado`

**Error:** `unknown method: /Container$i32::add`

**What Works:**
- ✅ `Container$i32` struct is created and registered
- ✅ `Container$i32::new()` method is monomorphized
- ✅ StructLiteral correctly uses `Container$i32`
- ✅ No crashes from generic templates

**What Doesn't Work:**
- ❌ Methods `Container$i32::add`, `Container$i32::len`, `Container$i32::get` not monomorphized
- ❌ Only the static method `new()` was monomorphized

### Next Steps

#### Investigation Needed: Why Methods Aren't Monomorphized

The function monomorphization logic creates `Container$i32::new()` but not the other methods. Need to investigate:

1. **Check function instantiation collection:**
   - Does `collect_function_instantiation_sites()` find method calls?
   - Are method calls (e.g., `container.add(10)`) being detected?

2. **Check method instantiation keys:**
   - Are InstantiationKeys created for methods?
   - Do they have correct type_args from the receiver type?

3. **Check impl block handling:**
   - Does `impl<T> Container<T>` correctly track type params?
   - Are impl type params being included in instantiation?

#### Possible Root Cause

Looking at the code, `Container::new()` is a **static method** (no self parameter) while `add`, `len`, `get` are **instance methods** (have self parameter). The monomorphization logic might:
- Collect static method calls directly from `StaticCall` expressions
- But miss instance method calls from `MethodCall` expressions
- Or fail to infer type args from the receiver type in MethodCall

#### Suggested Fix

In `collect_func_instantiation_sites_in_expr()`, when handling `TirExprKind::MethodCall`:
1. Extract the receiver type
2. If receiver is a GenericInstance or monomorphized struct, extract its type_args  
3. Create InstantiationKey for the method with those type_args
4. Add to pending queue for monomorphization

This is likely already implemented, so the bug might be in:
- How receiver types are resolved
- How type_args are extracted from receiver
- How method names are looked up in generic_functions map
