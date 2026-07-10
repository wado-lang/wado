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
    register_variant_cases(flat);
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

/// Publish every variant declaration's case templates to the type table, so
/// [`needs_value_copy`] can classify variants by payload from a `&TypeTable`
/// in every later phase (the seed walk here, the fold, `optimize`,
/// `wir_build`).
fn register_variant_cases(flat: &FlatPackage) {
    let mut type_table = flat.type_table.borrow_mut();
    for variant in &flat.variants {
        let cases: Vec<(String, u32, TypeId)> = variant
            .cases
            .iter()
            .map(|c| (c.name.clone(), c.index, c.payload))
            .collect();
        type_table.register_variant_cases(
            variant.name.clone(),
            variant.module_source.clone(),
            cases,
        );
    }
}

/// True when a value of `type_id` must be deep-copied on assignment
/// or parameter passing.
///
/// `analyze::should_wrap` and `synthesize::make_field_copy` MUST
/// agree: a `true` here means `synthesize::generate_copy_function`
/// emits a non-identity helper, and every caller routes through it.
pub fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    needs_copy_in_env(type_id, None, type_table, 0)
}

/// The instantiation environment a template-term payload id resolves its
/// `TypeParam`s in: the instantiation's type args, each of which is itself in
/// the parent frame's terms. Borrowed frames chain per descent instead of
/// cloning environment vectors, so the classification stays allocation-free
/// and can run through a `&TypeTable` without interning substituted types.
struct EnvFrame<'a> {
    args: &'a [TypeId],
    parent: Option<&'a EnvFrame<'a>>,
}

/// Cycles are only possible through variant-payload chains; every other
/// deep-copy carrier (struct, `List`, tuple, raw array) answers without
/// recursing. A chain this deep is a degenerate recursive variant nest, and
/// `true` (copy) is the safe answer for it.
const NEEDS_COPY_DEPTH_LIMIT: usize = 32;

/// Resolve `param::assoc_name` to a concrete type through the frame chain and
/// the table's recorded impl resolutions, without interning. `None` when the
/// param stays abstract or the resolution was never recorded.
fn resolve_projection_in_env(
    param_id: TypeId,
    assoc_name: &str,
    env: Option<&EnvFrame<'_>>,
    type_table: &TypeTable,
) -> Option<TypeId> {
    let concrete = resolve_param_in_env(param_id, env, type_table)?;
    let concrete = type_table.peel_refs(concrete);
    if let Some(resolved) = type_table.resolve_assoc_type(concrete, assoc_name) {
        return Some(resolved);
    }
    // Newtypes inherit associated types from their base type.
    let base = type_table.get_ultimate_base_type(concrete);
    (base != concrete)
        .then(|| type_table.resolve_assoc_type(base, assoc_name))
        .flatten()
}

fn resolve_param_in_env(
    type_id: TypeId,
    env: Option<&EnvFrame<'_>>,
    type_table: &TypeTable,
) -> Option<TypeId> {
    if let ResolvedType::TypeParam { index, .. } = type_table.get(type_id) {
        let frame = env?;
        let arg = *frame.args.get(*index as usize)?;
        return resolve_param_in_env(arg, frame.parent, type_table);
    }
    (!type_table.contains_type_param(type_id)).then_some(type_id)
}

fn needs_copy_in_env(
    type_id: TypeId,
    env: Option<&EnvFrame<'_>>,
    type_table: &TypeTable,
    depth: usize,
) -> bool {
    if depth > NEEDS_COPY_DEPTH_LIMIT {
        return true;
    }
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
            if type_table.find_struct_type(name, module_source).is_some() {
                // Generic-struct templates always need field-by-field copy.
                return true;
            }
            if let Some(cases) = type_table.variant_template_cases(name, module_source) {
                let frame = EnvFrame {
                    args: type_args,
                    parent: env,
                };
                return cases.iter().any(|(_, _, payload)| {
                    needs_copy_in_env(*payload, Some(&frame), type_table, depth + 1)
                });
            }
            // Generic-resource templates and unmaterialised instances.
            false
        }
        // A variant needs a deep copy exactly when one of its payloads
        // does: the variant value itself is reference-shaped at the WIR
        // level, so copying it shares the payload storage unless the
        // synthesized helper re-constructs the matched case with a
        // copied payload (`synthesize::build_variant_copy_body`).
        // Payload-free and reference-payload variants stay share-safe.
        ResolvedType::Variant {
            name,
            module_source,
        } => type_table
            .variant_template_cases(name, module_source)
            .is_some_and(|cases| {
                cases
                    .iter()
                    .any(|(_, _, payload)| needs_copy_in_env(*payload, None, type_table, depth + 1))
            }),
        // A projected payload in template terms (`Data(T::Output)`): resolve
        // the param through the frame, then the projection through the
        // table's recorded impl resolutions. An unresolvable projection gets
        // `true` — a spurious copy is safe, a missed one aliases.
        ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            ..
        } => match resolve_projection_in_env(*param_id, assoc_name, env, type_table) {
            Some(payload) => needs_copy_in_env(payload, None, type_table, depth + 1),
            None => true,
        },
        // A payload id in template terms: resolve the param through the
        // instantiation frame — the arg is in the parent frame's terms. A
        // bare param with no frame is an unmonomorphized template body — no
        // copy decision applies.
        ResolvedType::TypeParam { index, .. } => {
            let i = *index as usize;
            env.is_some_and(|frame| {
                frame
                    .args
                    .get(i)
                    .is_some_and(|arg| needs_copy_in_env(*arg, frame.parent, type_table, depth + 1))
            })
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
        // Resources are opaque handles; copying shares the handle.
        ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. } => false,
        _ => false,
    }
}
