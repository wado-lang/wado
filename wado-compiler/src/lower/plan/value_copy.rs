//! Plan Wado's value-copy semantics.
//!
//! Runs as part of [`super::plan`], i.e. after the TIR-mutating sub-passes
//! and before the TIR → NIR translator.
//!
//! - [`analyze::collect_seed_types`] walks every function body read-only,
//!   mirroring the fold's wrap-site predicates, and returns the set of
//!   `TypeId`s the translator will wrap in `$value_copy$T(...)` calls. The
//!   set also includes element types of `array_clone::<T>(...)` calls,
//!   which codegen routes through the same helper.
//! - [`synthesize::synthesize_helpers`] consumes the seed, iteratively
//!   generates per-type `$value_copy$T<id>` helper functions until the
//!   transitive closure of nested value-typed fields is covered, and
//!   pushes the helpers into [`FlatPackage::functions`]. Returns the
//!   `TypeId → (ModuleSource, helper-name)` map.
//!
//! The translator consumes [`ValueCopyPlan::name_for_type`] to emit a
//! direct `$value_copy$T(value)` call at every wrap site in user
//! function bodies (see [`analyze::should_wrap`] for the predicate the
//! fold and the seed walker share). User-program TIR carries no
//! `builtin::copy_value::<T>(x)` markers after Phase A of WEP
//! 2026-05-11 Step 5.
//!
//! Synthesized helper bodies still contain
//! `builtin::copy_value::<NestedT>(x)` markers for nested value-typed
//! fields; the translator's existing `convert_call` arm rewrites those
//! into the same `$value_copy$NestedT` calls.
//!
//! Wrapper elision happens later in `optimize::value_copy_elide`, which
//! runs as a regular pass inside the optimizer fixed-point loop so that
//! newly-exposed `$value_copy$T(...)` patterns from inlining or SROA get
//! collapsed in the same iteration that exposes them.

pub mod analyze;
pub mod synthesize;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Result of value-copy planning.
///
/// `name_for_type` is consumed by the TIR → NIR translator at every
/// wrap site to emit a direct call to the per-type `$value_copy$T<id>`
/// helper function registered in [`FlatPackage::functions`]. The same
/// map drives the marker rewrite in `convert_call` that handles
/// `builtin::copy_value::<NestedT>(...)` markers inside synthesized
/// helper bodies (the only place markers still appear after Phase A
/// of WEP 2026-05-11 Step 5).
pub struct ValueCopyPlan {
    pub name_for_type: IndexMap<TypeId, (ModuleSource, String)>,
}

/// Run the value-copy planner.
///
/// 1. Walks every function body read-only and collects the seed set of
///    `TypeId`s the translator will wrap.
/// 2. Iteratively generates `$value_copy$T<id>` helpers for the seed and
///    every type reached transitively through synthesized helper bodies,
///    and registers each helper in [`FlatPackage::functions`]. Returns the
///    map the translator uses to emit wrap calls and to rewrite the
///    `copy_value::<NestedT>` markers that still appear inside helper
///    bodies.
pub fn plan(flat: &mut FlatPackage) -> ValueCopyPlan {
    let seed = analyze::collect_seed_types(flat);
    let name_for_type = synthesize::synthesize_helpers(flat, seed);
    ValueCopyPlan { name_for_type }
}

/// True when a value of `type_id` must be deep-copied on assignment or
/// parameter passing.
///
/// Shared between [`analyze::should_wrap`] (which the fold and the
/// seed walker consult to decide whether a wrap site needs a
/// `$value_copy$T(...)` call) and `synthesize::make_field_copy` (which
/// decides whether a synthesized helper's per-field projection also
/// needs to recurse into a nested value-typed struct). The two callers
/// must agree: when this returns `true`,
/// `synthesize::generate_copy_function` emits a `$value_copy$T<id>`
/// helper, and any caller — user code or another synthesized helper —
/// must route through it. If we wrapped fields whose helper would be
/// identity (`return v;`), the helper would still be typed at WIR
/// level as `(T) -> T` non-null, and any nullable source (e.g. a
/// `None`-initialised `Option<&T>` global) would hit a
/// `(ref X) / nullref` mismatch — an invalid Wasm module at compile
/// time.
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
