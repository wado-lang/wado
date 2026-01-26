# Type Registration Redesign

## Problem

Current approach registers types in multiple phases with complex ordering:
- Non-mono structs → arrays → closures → mono structs
- Topological sorting within each phase
- Special handling for deferred types

This breaks when dependencies cross phase boundaries (e.g., non-mono struct with mono struct field).

## Solution: Single Rec Group for User Types

WebAssembly GC allows mutual forward references within a `rec` group.
Put ALL user-defined types in one rec group.

### What goes in the rec group:
1. All structs (mono and non-mono, from all modules)
2. All variants (mono and non-mono)
3. Array<T> where T is a user type (needs to reference T)
4. Raw GC array types for user type elements

### What stays outside:
1. Primitive types (i32, f64, etc.)
2. Array<primitive> types
3. Box types for primitives
4. Function types

## Implementation

```rust
fn register_all_user_types(
    &mut self,
    all_tir_modules: &[(ModuleSource, TirModule)],
    entry_tir: &TirModule,
    type_table: &TypeTable,
    builder: &mut CoreModuleBuilder,
) {
    // Phase 1: Collect all user types
    let mut rec_group_types: Vec<(String, RecTypeKind)> = vec![];

    // Collect structs from all modules
    for (_, tir_mod) in all_tir_modules {
        for tir_struct in &tir_mod.structs {
            if should_include_struct(tir_struct) {
                let fields = self.build_struct_fields(tir_struct, type_table);
                rec_group_types.push((tir_struct.name.clone(), RecTypeKind::Struct(fields)));
            }
        }
    }

    // Collect variants
    for variant in &entry_tir.variants {
        if should_include_variant(variant) {
            let fields = self.build_variant_fields(variant, type_table);
            rec_group_types.push((variant.name.clone(), RecTypeKind::Struct(fields)));
        }
    }

    // Collect Array<UserType> types
    for type_id in type_table.all_types() {
        if let ResolvedType::BuiltinArray(inner) = type_table.get(type_id) {
            if is_user_type(type_table.get(inner)) {
                let array_name = format!("Array<{}>", type_name(inner));
                // Add raw array type + Array struct
                rec_group_types.push(...);
            }
        }
    }

    // Phase 2: Define all in one rec group
    let indices = builder.define_rec_group(&rec_group_types);

    // Phase 3: Update registries with allocated indices
    for (i, (name, _)) in rec_group_types.iter().enumerate() {
        self.struct_types.insert(name, StructTypeInfo { type_idx: indices[i], ... });
    }
}
```

## Benefits

1. **No topological sorting needed** - rec group handles any dependency order
2. **No phase ordering issues** - all types defined simultaneously
3. **Simpler code** - one function instead of multiple phases
4. **Always works** - any dependency pattern is supported

## Trade-offs

1. **Larger rec groups** - all types in one group even if no circular deps
2. **Must build all field types upfront** - need to resolve TypeId→ValType before defining

## Key Challenge: type_id_to_valtype

Current `type_id_to_valtype` requires referenced types to be registered.
For rec group approach, we need a modified version that:
1. Returns the pre-allocated type index (from Phase 1)
2. Works before types are fully defined

Solution: Pre-allocate indices, store in a temporary map, use that map during field resolution.

```rust
// During Phase 1: pre-allocate indices
let base_idx = builder.peek_next_type_idx();
let mut pending_indices: HashMap<String, u32> = HashMap::new();
for (i, (name, _)) in rec_group_types.iter().enumerate() {
    pending_indices.insert(name.clone(), base_idx + i as u32);
}

// During field resolution: use pending_indices if not in main registry
fn type_id_to_valtype_with_pending(&self, type_id, pending: &HashMap<String, u32>) -> ValType {
    // ... existing logic ...
    // For user types, check pending first
    if let Some(idx) = pending.get(&type_name) {
        return ValType::Ref(RefType { heap_type: HeapType::Concrete(*idx), ... });
    }
    // ... fallback to existing registry ...
}
```
