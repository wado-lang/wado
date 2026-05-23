//! Plan Wado's value-copy semantics.
//!
//! [`analyze::collect_seed_types`] returns the set of `TypeId`s the
//! fold will wrap in `$value_copy$T(...)`;
//! [`synthesize::synthesize_helpers`] generates a per-type helper for
//! the seed plus its transitive closure of nested value-typed fields.
//! The fold (`lower::translate`) emits wrap calls directly using
//! [`ValueCopyPlan::name_for_type`].
//!
//! Wrapper elision runs later in `optimize::value_copy_elide`.

pub mod analyze;
pub mod synthesize;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// `TypeId` → `(ModuleSource, $value_copy$T<id>)` for every helper
/// `synthesize_helpers` registered in [`FlatPackage::functions`].
pub struct ValueCopyPlan {
    pub name_for_type: IndexMap<TypeId, (ModuleSource, String)>,
}

pub fn plan(flat: &mut FlatPackage) -> ValueCopyPlan {
    let seed = analyze::collect_seed_types(flat);
    let name_for_type = synthesize::synthesize_helpers(flat, seed);
    ValueCopyPlan { name_for_type }
}

/// True when a value of `type_id` must be deep-copied on assignment
/// or parameter passing.
///
/// `analyze::should_wrap` and `synthesize::make_field_copy` MUST
/// agree: a `true` here means `synthesize::generate_copy_function`
/// emits a non-identity helper, and every caller routes through it.
/// Wrapping a type whose helper is identity (`return v;`) leaves the
/// helper typed as `(T) -> T` non-null at the WIR level, and a
/// nullable source (e.g. a `None`-initialised `Option<&T>` global,
/// `ref.null` at lowering) trips a `(ref X) / (ref null X)`
/// validation mismatch.
pub fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    let items = type_table.compiler_items();
    let box_name = items.struct_name(crate::compiler_item::CompilerItem::Box);
    let array_name = items.struct_name(crate::compiler_item::CompilerItem::Array);
    match type_table.get(type_id) {
        // Concrete structs need a field-by-field deep copy, except for
        // the `Box<T>` shortcut whose semantics intentionally share
        // the underlying cell.
        ResolvedType::Struct { base_name, .. } => base_name.as_deref() != Some(box_name),
        ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        } => {
            if name == box_name {
                return false;
            }
            if TypeTable::is_tuple_type(name, module_source) {
                // Empty tuples are unit-shaped; non-empty tuples need
                // element-wise deep copy.
                return !type_args.is_empty();
            }
            if name == array_name {
                return true;
            }
            // `Option<T>` / `Result<T, E>` are reference-shaped variants
            // at WIR level. Their synthesized helper is identity
            // (`return v;`); a non-identity match body that
            // deep-copies the payload would have to be typed at WIR
            // level as `(ref X) -> (ref X)` non-null, but call sites
            // routinely pass nullable sources (e.g. a `None`-initialised
            // `Option<&T>` global lowers to `ref.null`), hitting a
            // `(ref X) / (ref null X)` validation mismatch. References
            // (`Option<&mut T>`) don't need deep-copy anyway — a ref
            // copy intentionally shares the pointee. For value-typed
            // payloads (`Option<String>`, `Option<MyStruct>`),
            // shallow-sharing the payload across copies remains an
            // open bug; tracked separately from the nested-struct /
            // array-of-struct fixes that *are* landed here.
            // Other generic instances: only generic-struct templates
            // need deep copy. Generic-variant templates and
            // generic-resource templates fall through to identity in
            // `synthesize::build_copy_body`, so wrapping them is wasted
            // and triggers the nullable-source mismatch described
            // above.
            type_table.find_struct_type(name, module_source).is_some()
        }
        // `&T` / `&mut T` are reference types: assignment copies the
        // pointer, not the pointee. This is intentional — a struct
        // field of type `&mut T` is meant to share the referenced
        // value with the original, not duplicate it. Stdlib types
        // that need deep-copy semantics use `Array<T>` / `String`
        // (which deep-copy via `array_clone` on their internal
        // `builtin::array`), not `&mut T`.
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => false,
        // Variants and resources are reference-shaped at WIR level;
        // their copy body is identity (`return v;`).
        ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. } => false,
        _ => false,
    }
}
