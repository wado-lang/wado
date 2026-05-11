//! Plan Wado's value-copy semantics.
//!
//! Runs as part of [`super::plan`], i.e. after the TIR-mutating sub-passes
//! and before the TIR → NIR translator.
//!
//! - [`insert::insert_value_copy_calls`] wraps every defensive deep-copy
//!   position in `builtin::copy_value::<T>(x)` (TIR-mutating).
//! - [`synthesize::synthesize_helpers`] generates per-type
//!   `$value_copy$T<id>` helper functions, pushes them into
//!   [`FlatPackage::functions`], and returns the
//!   `TypeId → (ModuleSource, helper-name)` map.
//!
//! The translator consumes [`ValueCopyPlan::name_for_type`] to rewrite the
//! `builtin::copy_value::<T>(x)` markers into direct calls to the helpers
//! during TIR → NIR translation. Marker rewriting therefore does not
//! happen at the planner stage — the translator is the single point that
//! turns markers into resolved calls, for both user code and synthesized
//! helper bodies (which contain `builtin::copy_value::<NestedT>(...)` for
//! nested value-typed fields).
//!
//! Wrapper elision happens later in `optimize::value_copy_elide`, which
//! runs as a regular pass inside the optimizer fixed-point loop so that
//! newly-exposed `$value_copy$T(...)` patterns from inlining or SROA get
//! collapsed in the same iteration that exposes them.

pub mod insert;
pub mod synthesize;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Result of value-copy planning.
///
/// `name_for_type` is consumed by the TIR → NIR translator to rewrite
/// `builtin::copy_value::<T>(x)` markers into direct calls to the
/// `$value_copy$T<id>` helper functions registered in
/// [`FlatPackage::functions`].
pub struct ValueCopyPlan {
    pub name_for_type: IndexMap<TypeId, (ModuleSource, String)>,
}

/// Run the value-copy planner.
///
/// 1. Inserts `builtin::copy_value::<T>(x)` markers at every TIR position
///    that requires a defensive deep-copy (TIR-mutating).
/// 2. Walks the marked TIR, generates one `$value_copy$T<id>` helper for
///    every type reached transitively, and registers each helper in
///    [`FlatPackage::functions`]. Returns the map the translator uses to
///    resolve marker calls.
pub fn plan(flat: &mut FlatPackage) -> ValueCopyPlan {
    insert::insert_value_copy_calls(flat);
    let name_for_type = synthesize::synthesize_helpers(flat);
    ValueCopyPlan { name_for_type }
}

/// True when a value of `type_id` must be deep-copied on assignment or
/// parameter passing.
///
/// Shared between [`insert::insert_value_copy_calls`] (which decides
/// where in user-program TIR to wrap expressions in
/// `builtin::copy_value::<T>(x)`) and [`synthesize::make_field_copy`]
/// (which decides whether a synthesized helper's per-field projection
/// also needs to recurse into a nested value-typed struct). The two
/// callers must agree: when this returns `true`,
/// [`synthesize::generate_copy_function`] emits a `$value_copy$T<id>`
/// helper, and any caller — user code or another synthesized helper —
/// must route through it. If we wrapped fields whose helper would be
/// identity (`return v;`), the helper would still be typed at WIR
/// level as `(T) -> T` non-null, and any nullable source (e.g. a
/// `None`-initialised `Option<&T>` global) would hit a
/// `(ref X) / nullref` mismatch — an invalid Wasm module at compile
/// time.
pub fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        // Concrete structs need a field-by-field deep copy, except for
        // the `Box<T>` shortcut whose semantics intentionally share
        // the underlying cell.
        ResolvedType::Struct { base_name, .. } => base_name.as_deref() != Some("Box"),
        ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        } => {
            if name == "Box" {
                return false;
            }
            if TypeTable::is_tuple_type(name, module_source) {
                // Empty tuples are unit-shaped; non-empty tuples need
                // element-wise deep copy.
                return !type_args.is_empty();
            }
            if name == "Array" {
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
