//! Plan Wado's value-copy semantics: [`analyze::collect_seed_types`] names the
//! types the fold wraps in `$value_copy$T(…)` and
//! [`synthesize::synthesize_helpers`] generates a helper apiece. A wrap is
//! emitted only where the ownership analyses cannot prove a move, share or fresh
//! value, so every copy is necessary and nothing elides them later.

pub mod analyze;
pub mod callgraph;
pub mod confine;
pub mod funcset;
pub mod last_use;
pub mod modref;
pub mod ownership;
pub mod place;
pub mod stores;
pub mod synthesize;

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TypeId, TypeTable};
use funcset::{FuncKeyMap, FuncKeySet};

/// The element type `T` of a `builtin::array_clone::<T>` /
/// `array_clone_prefix::<T>` call, or `None` for any other expression.
fn array_clone_element_type_arg(expr: &TirExpr) -> Option<TypeId> {
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return None;
    };
    if !func.module_source.is_core_builtin() {
        return None;
    }
    let is_clone = ["array_clone", "array_clone_prefix"]
        .into_iter()
        .any(|name| crate::tir::matches_builtin(&func.name, func.monomorph_info.as_ref(), name));
    if !is_clone {
        return None;
    }
    func.monomorph_info
        .as_ref()
        .and_then(|mi| mi.impl_type_args.first().copied())
}

/// Which helper copies a type, keyed by the structural mangle that is the
/// helper's identity — and the one place a type becomes that key. The table
/// interns one logical type under several `TypeId`s, so an id key answers for
/// one of them only.
#[derive(Debug)]
pub struct ValueCopyHelpers<T> {
    by_key: IndexMap<String, T>,
    /// Memo over the types synthesis walked. A miss is unobservable — `get`
    /// derives the key instead.
    key_of: IndexMap<TypeId, String>,
}

impl<T> Default for ValueCopyHelpers<T> {
    fn default() -> Self {
        Self {
            by_key: IndexMap::default(),
            key_of: IndexMap::default(),
        }
    }
}

impl<T> ValueCopyHelpers<T> {
    pub fn insert(&mut self, type_id: TypeId, key: String, helper: T) {
        self.by_key.entry(key.clone()).or_insert(helper);
        self.key_of.insert(type_id, key);
    }

    #[must_use]
    pub fn get(&self, type_id: TypeId, type_table: &TypeTable) -> Option<&T> {
        if let Some(key) = self.key_of.get(&type_id) {
            return self.by_key.get(key);
        }
        self.by_key
            .get(&type_table.mangle_type_arg_for_generic(type_id))
    }

    pub fn map<U>(&self, mut f: impl FnMut(&T) -> U) -> ValueCopyHelpers<U> {
        ValueCopyHelpers {
            by_key: self.by_key.iter().map(|(k, v)| (k.clone(), f(v))).collect(),
            key_of: self.key_of.clone(),
        }
    }
}

/// Which `$value_copy$` helper copies each type, plus the return conventions
/// telling the fold whether a call result is owned (a move) or borrowed.
pub struct ValueCopyPlan {
    pub helpers: ValueCopyHelpers<(ModuleSource, String)>,
    /// What each call writes into its receiver, as fields of the receiver's
    /// type: a read of one field is undisturbed by a call that writes another.
    pub mod_ref: modref::ModRef,
    /// The projection each accessor returns out of its receiver, so a name
    /// taken from one resolves to the storage it stands for.
    pub return_paths: place::ReturnPaths,
    pub returns_owned: FuncKeySet,
    /// Functions whose every returned value is owned *or* a projection of the
    /// receiver / first parameter (`build(&self) -> List { return *self }`). A
    /// call to one is fresh when its receiver is, so a `[1, 2, 3]` builder
    /// finalized by `.build()` is not defensively copied. Superset of
    /// `returns_owned`.
    pub returns_self_projection: FuncKeySet,
    /// Per-callee, per-position reference-storage: which parameter positions a
    /// callee may persist a reference to. A local whose `&`/`&mut` is passed at
    /// a *stored* position is borrow-escaped and cannot be moved; passed at a
    /// non-stored position it is a transient borrow. Interprocedurally inferred
    /// (a least fixpoint over the call graph), so it is a sound superset of the
    /// declared `stores[...]` clauses and catches undeclared stores too. Position
    /// 0 is the receiver, so `receiver_storing_methods` is subsumed by this.
    pub stored_params: stores::StoredParams,
    /// Functions whose first parameter is `&mut self` — the only methods that
    /// can mutate the caller's receiver storage. The last-use move analysis
    /// treats such a call's receiver as a sibling mutation, so a by-value
    /// argument aliasing it keeps its copy.
    pub mut_receiver_methods: FuncKeySet,
    /// Per-parameter confinement: a still-live value into a confined parameter
    /// needs no defensive copy (caller-side replacement for `param_escapes`).
    pub confined_params: confine::ConfinedParams,
    /// Per-callee, which parameters are `&mut` — the only siblings that can
    /// mutate a shared by-value argument during the call. A confined by-value
    /// argument keeps its copy only when it aliases such a sibling.
    pub mut_ref_params: FuncKeyMap<Vec<bool>>,
    /// Methods whose receiver is a reference (`&self` / `&mut self`) — a
    /// borrowing, not consuming, receiver. Used by the read-only-share analysis.
    pub ref_receiver_methods: FuncKeySet,
    /// Functions whose result aliases their receiver's storage (a borrowed
    /// projection / element read). Because it admits borrowed projections it
    /// must not feed the move / owned decision.
    pub returns_receiver_alias: FuncKeySet,
    /// Return types for which every closure `__call` — the complete set of
    /// indirect-call targets after closure lowering — returns owned, so an
    /// indirect call yielding that type is fresh.
    pub indirect_owned_returns: IndexSet<TypeId>,
}

pub fn plan(
    flat: &mut FlatPackage,
    confined_params: confine::ConfinedParams,
    ref_receiver_methods: FuncKeySet,
) -> ValueCopyPlan {
    register_variant_cases(flat);
    let seed = analyze::collect_seed_types(flat);
    let helpers = synthesize::synthesize_helpers(flat, seed);
    // Built after synthesis so the value-copy helpers (always owned) are present
    // in `flat.functions`, and shared: every summary below is a monotone
    // backward fixpoint over the same edges.
    let call_graph = callgraph::CallGraph::build(flat);
    // Three passes, because the paths gate the conventions
    // ([`hands_out_payload`]) and the conventions place the paths: a seed pass
    // without the gate breaks the knot.
    let seed_conventions =
        ownership::compute_return_conventions(flat, &call_graph, &place::ReturnPaths::default());
    let return_paths = place::compute_return_paths(
        flat,
        &call_graph,
        &flat.type_table.borrow(),
        &seed_conventions.returns_owned,
    );
    let conventions = ownership::compute_return_conventions(flat, &call_graph, &return_paths);
    let stored_params = stores::compute_stored_params(flat, &call_graph);
    let mut mut_receiver_methods = FuncKeySet::default();
    let mut mut_ref_params = FuncKeyMap::default();
    for f in &flat.functions {
        let f = f.borrow();
        if f.params.first().is_some_and(|p| p.is_mut_ref) {
            mut_receiver_methods.insert(f.module_source.clone(), f.name.clone());
        }
        mut_ref_params.insert(
            f.module_source.clone(),
            f.name.clone(),
            f.params.iter().map(|p| p.is_mut_ref).collect(),
        );
    }
    let returns_receiver_alias =
        ownership::compute_receiver_alias(flat, &call_graph, &return_paths);
    let indirect_owned_returns =
        ownership::compute_indirect_owned_returns(flat, &conventions.returns_owned);
    ValueCopyPlan {
        helpers,
        mod_ref: modref::compute_mod_ref(flat, &return_paths, &conventions.returns_owned),
        return_paths,
        returns_owned: conventions.returns_owned,
        returns_self_projection: conventions.returns_self_projection,
        stored_params,
        mut_receiver_methods,
        confined_params,
        mut_ref_params,
        ref_receiver_methods,
        returns_receiver_alias,
        indirect_owned_returns,
    }
}

/// Publish every variant declaration's case templates to the type table,
/// so [`needs_value_copy`] can classify variants from a `&TypeTable`.
fn register_variant_cases(flat: &FlatPackage) {
    let mut type_table = flat.type_table.borrow_mut();
    for variant in &flat.variants {
        let cases: Vec<(String, u32, TypeId)> = variant
            .cases
            .iter()
            .map(|c| (c.name.clone(), c.index, c.payload))
            .collect();
        type_table.register_variant_cases(variant.def, cases);
    }
}

/// Whether `func` hands a returned variant's payload out uncopied instead of
/// defending it — see [`analyze::returned_value`].
///
/// Only where a caller can name what it got, because the callee's one copy is
/// shared by every call site and moving it out multiplies it. Naming it means a
/// [`place::ReturnPath`] that stays inside the receiver's own storage — one
/// leaving through a `&` field lands somewhere the caller cannot place.
#[must_use]
pub fn hands_out_payload(
    func: &crate::tir::TirFunction,
    return_paths: &place::ReturnPaths,
) -> bool {
    return_paths
        .get(&func.module_source, &func.name)
        .is_some_and(|path| !path.through_borrow)
}

/// Whether `type_id` is spelled `&T` / `&mut T`. Narrower than
/// [`TypeTable::is_reference_shaped`], which also answers for a boxed borrow.
pub fn is_reference_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    )
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
/// `TypeParam`s in. Each arg is in the parent frame's terms; frames borrow
/// per descent, so classification never interns substituted types.
struct EnvFrame<'a> {
    args: &'a [TypeId],
    parent: Option<&'a EnvFrame<'a>>,
}

/// Recursion is bounded by variant-payload chains only; past this depth
/// `true` (copy) is the safe answer.
const NEEDS_COPY_DEPTH_LIMIT: usize = 32;

/// Resolve `param::assoc_name` to a concrete type without interning.
/// `None` when the param stays abstract or the resolution is unrecorded.
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
        ResolvedType::Struct { def, type_args } => {
            type_table.struct_head_name(*def) != box_name || type_args.is_empty()
        }
        ResolvedType::GenericInstance { def, type_args } => {
            let name = type_table.def_name(*def);
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
            if let Some(cases) = type_table.variant_template_cases(*def) {
                let frame = EnvFrame {
                    args: type_args,
                    parent: env,
                };
                return cases.iter().any(|(_, _, payload)| {
                    needs_copy_in_env(*payload, Some(&frame), type_table, depth + 1)
                });
            }
            // Every other nominal instantiation is a struct, copied field by
            // field. Answering from the declaration's fields would need a
            // registry the type table does not keep, and `true` is the safe
            // direction: a spurious copy costs, a missed one aliases.
            true
        }
        // A variant is reference-shaped at the WIR level, so copying it
        // shares the payload storage: it needs a deep copy exactly when
        // one of its payloads does.
        ResolvedType::Variant { def } => {
            type_table
                .variant_template_cases(*def)
                .is_some_and(|cases| {
                    cases.iter().any(|(_, _, payload)| {
                        needs_copy_in_env(*payload, None, type_table, depth + 1)
                    })
                })
        }
        // An unresolvable projection gets `true`: a spurious copy is
        // safe, a missed one aliases.
        ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            ..
        } => match resolve_projection_in_env(*param_id, assoc_name, env, type_table) {
            Some(payload) => needs_copy_in_env(payload, None, type_table, depth + 1),
            None => true,
        },
        // A bare param with no frame is an unmonomorphized template
        // body — no copy decision applies.
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
        // rather than reference-semantic (WEP-2026-06-02).
        ResolvedType::BuiltinArray(_) => true,
        // `&T` / `&mut T` are reference types: assignment copies the
        // pointer, not the pointee. This is intentional — a struct
        // field of type `&mut T` is meant to share the referenced
        // value with the original, not duplicate it. Stdlib types
        // that need deep-copy semantics use `List<T>` / `String`
        // (which deep-copy via `array_clone` on their internal
        // `Array<T>`), not `&mut T`.
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => false,
        ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. } => false,
        _ => false,
    }
}
