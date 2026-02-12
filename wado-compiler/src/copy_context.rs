//! Copy context for managing value copy scratch locals.
//!
//! This module centralizes the management of temporary local variables needed
//! for value copy operations during code generation. It handles:
//! - Recursive expansion of nested types (e.g., Option<Variant>)
//! - Pre-allocation of all required scratch locals
//! - Type-safe lookup for copy operations
//!
//! # Design
//!
//! The `CopyContext` is created per-function and solves the problem of ensuring
//! all scratch locals are declared before any instructions are generated.
//! WebAssembly requires all locals to be declared upfront in the function signature.
//!
//! Previously, copy locals were pre-allocated in `preallocate_value_copy_locals`
//! and looked up in `generate_*_copy` functions, but the logic was fragmented
//! and didn't handle nested types like `Option<Variant>` correctly.

use indexmap::{IndexMap, IndexSet};

use wasm_encoder::{HeapType, RefType, ValType};

use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Locals needed for array copy operations.
#[derive(Debug, Clone, Copy)]
pub struct ArrayCopyLocals {
    /// Local for the Array struct wrapper source
    pub struct_source: u32,
    /// Local for the raw array source
    pub source: u32,
    /// Local for the raw array destination
    pub dest: u32,
    /// Local for the loop counter
    pub counter: u32,
    /// Local for the array length
    pub len: u32,
}

/// Context for managing value copy scratch locals.
///
/// Created per function, handles pre-allocation and lookup of all
/// scratch locals needed for copying struct, tuple, variant, array,
/// and option types.
#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)]
pub struct CopyContext {
    /// Map from Wasm struct type index to its copy source local index.
    /// Used for struct, tuple, and variant types.
    struct_source_locals: IndexMap<u32, u32>,

    /// Map from Wasm array type index to its copy locals.
    array_copy_locals: IndexMap<u32, ArrayCopyLocals>,

    /// Map from Option's inner Wasm heap type to its copy source local index.
    /// Keyed by inner type to handle multiple Option types in the same function.
    option_source_locals: IndexMap<u32, u32>,
}

impl CopyContext {
    /// Create a new empty `CopyContext`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Recursively expand types to include all nested types that need copy locals.
    ///
    /// For example, `Option<Variant>` expands to include both the Option type
    /// and the Variant type, since copying an Option requires copying its inner value.
    pub fn expand_copy_types(types: &IndexSet<TypeId>, type_table: &TypeTable) -> IndexSet<TypeId> {
        let mut expanded = IndexSet::new();
        for &type_id in types {
            Self::expand_type_recursive(type_id, type_table, &mut expanded);
        }
        expanded
    }

    fn expand_type_recursive(
        type_id: TypeId,
        type_table: &TypeTable,
        expanded: &mut IndexSet<TypeId>,
    ) {
        if expanded.contains(&type_id) {
            return;
        }

        match type_table.get(type_id) {
            ResolvedType::Option(inner) => {
                // Option itself needs a copy local
                expanded.insert(type_id);
                // If inner type needs copying, expand it too
                if Self::needs_value_copy(*inner, type_table) {
                    Self::expand_type_recursive(*inner, type_table, expanded);
                }
            }
            ResolvedType::Struct { .. } | ResolvedType::Tuple(_) | ResolvedType::Variant { .. } => {
                expanded.insert(type_id);
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Array" => {
                expanded.insert(type_id);
                // Array elements might need copying too
                if let Some(&elem_type) = type_args.first()
                    && Self::needs_value_copy(elem_type, type_table)
                {
                    Self::expand_type_recursive(elem_type, type_table, expanded);
                }
            }
            _ => {
                // Other types might still need to be in the set
                // (they were added to needed_copy_types for a reason)
                expanded.insert(type_id);
            }
        }
    }

    /// Check if a type needs value copy (deep copy) semantics.
    fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
        match type_table.get(type_id) {
            ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Variant { .. } => true,
            ResolvedType::Tuple(elements) => !elements.is_empty(),
            ResolvedType::Option(inner) => Self::needs_value_copy(*inner, type_table),
            _ => false,
        }
    }

    /// Register a struct/tuple/variant copy source local.
    ///
    /// Called during pre-allocation phase.
    pub fn register_struct_copy_local(&mut self, wasm_type_idx: u32, local_idx: u32) {
        self.struct_source_locals.insert(wasm_type_idx, local_idx);
    }

    /// Register array copy locals.
    ///
    /// Called during pre-allocation phase.
    pub fn register_array_copy_locals(&mut self, array_type_idx: u32, locals: ArrayCopyLocals) {
        self.array_copy_locals.insert(array_type_idx, locals);
    }

    /// Register an option copy source local.
    ///
    /// Called during pre-allocation phase. Keyed by the inner type's Wasm type index
    /// to handle multiple Option types in the same function.
    pub fn register_option_copy_local(&mut self, inner_heap_type_idx: u32, local_idx: u32) {
        self.option_source_locals
            .insert(inner_heap_type_idx, local_idx);
    }

    /// Get the copy source local for a struct/tuple/variant type.
    ///
    /// Returns None if the local was not pre-allocated (which indicates a bug).
    pub fn get_struct_copy_local(&self, wasm_type_idx: u32) -> Option<u32> {
        self.struct_source_locals.get(&wasm_type_idx).copied()
    }

    /// Get the copy locals for an array type.
    ///
    /// Returns None if the locals were not pre-allocated (which indicates a bug).
    pub fn get_array_copy_locals(&self, array_type_idx: u32) -> Option<ArrayCopyLocals> {
        self.array_copy_locals.get(&array_type_idx).copied()
    }

    /// Get the copy source local for an option type.
    ///
    /// Keyed by the inner type's Wasm heap type index.
    /// Returns None if the local was not pre-allocated (which indicates a bug).
    pub fn get_option_copy_local(&self, inner_heap_type_idx: u32) -> Option<u32> {
        self.option_source_locals.get(&inner_heap_type_idx).copied()
    }

    /// Get the heap type index from a heap type, if it's a concrete type.
    pub fn heap_type_to_idx(heap_type: HeapType) -> Option<u32> {
        match heap_type {
            HeapType::Concrete(idx) => Some(idx),
            _ => None,
        }
    }

    /// Create a nullable reference `ValType` for a concrete type index.
    pub fn nullable_ref(type_idx: u32) -> ValType {
        ValType::Ref(RefType {
            nullable: true,
            heap_type: HeapType::Concrete(type_idx),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_copy_context_struct_local() {
        let mut ctx = CopyContext::new();
        ctx.register_struct_copy_local(42, 5);
        assert_eq!(ctx.get_struct_copy_local(42), Some(5));
        assert_eq!(ctx.get_struct_copy_local(99), None);
    }

    #[test]
    fn test_copy_context_array_locals() {
        let mut ctx = CopyContext::new();
        let locals = ArrayCopyLocals {
            struct_source: 1,
            source: 2,
            dest: 3,
            counter: 4,
            len: 5,
        };
        ctx.register_array_copy_locals(10, locals);
        let retrieved = ctx.get_array_copy_locals(10).unwrap();
        assert_eq!(retrieved.source, 2);
        assert_eq!(retrieved.len, 5);
    }

    #[test]
    fn test_copy_context_option_local() {
        let mut ctx = CopyContext::new();
        ctx.register_option_copy_local(7, 3);
        assert_eq!(ctx.get_option_copy_local(7), Some(3));
    }
}
