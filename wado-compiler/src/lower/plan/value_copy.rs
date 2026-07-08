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
pub mod last_use;
pub mod ownership;
pub mod synthesize;

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::FunctionId;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// `TypeId` → `(ModuleSource, $value_copy$T<id>)` for every helper
/// `synthesize_helpers` registered in [`FlatPackage::functions`], plus the
/// interprocedural return-convention set the fold consults to decide whether a
/// call result is owned (a move) or borrowed (a copy).
pub struct ValueCopyPlan {
    pub name_for_type: IndexMap<TypeId, (ModuleSource, String)>,
    pub returns_owned: IndexSet<FunctionId>,
    /// Functions whose every returned value is owned *or* a projection of the
    /// receiver / first parameter (`build(&self) -> List { return *self }`). A
    /// call to one is fresh when its receiver is, so a `[1, 2, 3]` builder
    /// finalized by `.build()` is not defensively copied. Superset of
    /// `returns_owned`.
    pub returns_self_projection: IndexSet<FunctionId>,
    /// Functions with a non-empty `stores[...]` clause — a callee that may
    /// persist a reference passed to it. A local whose `&`/`&mut` is passed to
    /// one is borrow-escaped and cannot be moved (the TIR move analysis runs
    /// before inlining, so `stores_aliased_locals` is not yet populated).
    pub functions_with_stores: IndexSet<FunctionId>,
}

pub fn plan(flat: &mut FlatPackage) -> ValueCopyPlan {
    let seed = analyze::collect_seed_types(flat);
    let name_for_type = synthesize::synthesize_helpers(flat, seed);
    // Computed after synthesis so the value-copy helpers (always owned) are
    // present in `flat.functions` and seed the fixpoint.
    let conventions = ownership::compute_return_conventions(flat);
    let functions_with_stores = flat
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            (!f.stores.is_empty()).then(|| ownership::func_key(&f.module_source, &f.name))
        })
        .collect();
    ValueCopyPlan {
        name_for_type,
        returns_owned: conventions.returns_owned,
        returns_self_projection: conventions.returns_self_projection,
        functions_with_stores,
    }
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
    let list_name = items.struct_name(crate::compiler_item::CompilerItem::List);
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
            if TypeTable::is_tuple_type(name) {
                // Empty tuples are unit-shaped; non-empty tuples need
                // element-wise deep copy.
                return !type_args.is_empty();
            }
            if name == list_name {
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
        // The raw GC array is a value type: assignment / parameter
        // passing / return deep-copies it, like every other value.
        // Its synthesized helper is `array_clone::<T>(&v)` (see
        // `synthesize::build_copy_body`), the same intrinsic that
        // deep-copies the `repr` field of `List<T>` / `String`. This
        // is the only thing that makes `Array<T>` value-semantic
        // rather than reference-semantic (WEP-2026-06-02 Phase 2).
        ResolvedType::BuiltinArray(_) => true,
        // `&T` / `&mut T` are reference types: assignment copies the
        // pointer, not the pointee. This is intentional — a struct
        // field of type `&mut T` is meant to share the referenced
        // value with the original, not duplicate it. Stdlib types
        // that need deep-copy semantics use `List<T>` / `String`
        // (which deep-copy via `array_clone` on their internal
        // `Array<T>`), not `&mut T`.
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => false,
        // Variants and resources are reference-shaped at WIR level;
        // their copy body is identity (`return v;`).
        ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. } => false,
        _ => false,
    }
}
