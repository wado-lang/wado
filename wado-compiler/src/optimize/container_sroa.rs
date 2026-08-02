//! Container SROA (P0a) — `AoS` → `SoA` transformation for `List<Tuple<...>>` locals.
//!
//! This pass decomposes local variables of type `List<[T_0, T_1, ..., T_n]>` into
//! N parallel `List<T_k>` locals, eliminating the per-element `struct.new` for
//! tuple payloads. After decomposition, operations on the original array are
//! rewritten as parallel operations on the new per-field arrays:
//!
//! ```text
//! let mut v: List<[i32, i32]> = [];        →   let mut v_0: List<i32> = [];
//! v.push([a, b]);                                let mut v_1: List<i32> = [];
//! let sum = v[i].0 + v[i].1;                    v_0.push(a); v_1.push(b);
//!                                                let sum = v_0[i] + v_1[i];
//! ```
//!
//! P0a scope covers `List<Tuple<...>>` and `List<UserStruct>` locals (both have
//! the same `WasmGC` struct representation). Nested arrays are not yet decomposed.
//! Only method-call usage is handled; direct indexing is expected to have been
//! desugared already into `index_value`/`index_assign` trait calls by lowering.
//!
//! TODO(optimizer): nested-container decomposition (`List<List<T>>`,
//! `List<UserStruct { List<T>, ... }>`). The recursion into nested element
//! types is a clean extension of `decompose_local`; the harder problem is the
//! recursive element-immutability proof, which `value_copy_demote.rs` already
//! solves and could be lifted out for reuse here.
//!
//! TODO(optimizer): replace the hardcoded method-shape whitelist
//! (`ElementWriter` / `IndexReader` / `IndexWriter` / `Constructor`) with a
//! query against `value_copy_demote`'s element-immutability analysis so any
//! element-immutable `&self`/`&mut self` method becomes a SROA-safe use,
//! not just `push` / `index_value` / `index_assign` / `len` / `is_empty`.
//!
//! # List method identification
//!
//! Rather than hardcoding method names (`"push"`, `"len"`, `"index_value"`, …),
//! this pass identifies relevant List methods by **signature shape**:
//!
//! | Kind             | Signature                                         | stdlib method  |
//! |------------------|---------------------------------------------------|----------------|
//! | `ElementWriter`  | `fn(&mut List<T>, T) -> ()`                      | `push`         |
//! | `IndexReader`    | `fn(&List<T>, i32) -> T`                         | `index_value`  |
//! | `IndexWriter`    | `fn(&mut List<T>, i32, T) -> ()`                 | `index_assign` |
//! | `Constructor`    | `fn(i32) -> List<T>` (static)                    | `with_capacity`|
//! | `Query`          | `fn(&List<T>) -> i32 \| bool` (length-invariant) | `len`, `is_empty`, `capacity` |
//!
//! Classification happens once when the `MethodCatalog` is built, and every
//! whitelist and rewrite decision is driven by looking up the call's classified
//! `ListMethodKind`. Unclassified List methods cause the candidate to escape,
//! so adding a new stdlib method that doesn't match any kind is safe by default.
//! Adding a new method that *does* match a kind (e.g., `push_back`) is
//! automatically handled — no optimizer change required.
//!
//! # Pipeline position
//!
//! Runs *first* in each fixed-point iteration, before `inline`. The pass
//! relies on every `List<T>` access being a method call (`push`,
//! `index_value`, `index_assign`, `len`, ...), but `inline` expands those
//! thin wrappers into raw `builtin::array_get`/`array_set` + field-access
//! pairs, after which the method-call shape is gone. Running before inline
//! preserves the call structure that `list_method_kind` classifies.
//!
//! Running inside each loop iteration (rather than only once up front) also
//! lets container SROA pick up new `List<Tuple<...>>` locals exposed by
//! earlier-iteration inlining of helper functions.
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and performs the whole-function rewrite in one shot. The
//! analysis phases (candidate collection, escape / used-kinds) stay read-only
//! walks over `engine.body`; the rewrite routes every mutation through the
//! engine edit API (`set_block_stmts`, `replace_expr_kind`, `become_expr`,
//! `alloc_stmt`, `alloc_expr`, `alloc_local`, `clone_expr`) so the parent map
//! and use index stay coherent.

use std::cell::Cell;

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirFunction, NirStruct, NirUnaryOp};
use crate::nir_arena::{
    ArenaCallArg, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use cranelift_entity::EntityRef;

use super::arena_query::reachable_blocks;
use super::gate::{FunctionGate, GatedPass};

/// Signature key for a monomorphized `List<T>` method: (`trait_name`, `method_name`).
/// Inherent methods (`push/len/is_empty/with_capacity`) use `trait_name = None`;
/// trait methods (`index_value/index_assign`) use `Some("IndexValue<i32>")` etc.
///
/// This key is the *method family* identifier — it is invariant under the element
/// type `T` (i.e., `List<i32>::push` and `List<i64>::push` share the same
/// `SigKey`). The catalog then uses `(TypeId, SigKey)` for per-element-type lookup.
type SigKey = (Option<String>, String);

/// Classification of an `List<T>` method by signature shape. Determines whether
/// the pass can safely rewrite calls on decomposed candidates, and how.
///
/// See the module-level table for the mapping from each kind to stdlib methods.
/// Classification is *signature-driven*: any List method whose signature
/// matches one of these shapes is automatically handled, regardless of name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ListMethodKind {
    /// `fn(&mut List<T>, T) -> ()` — stores one element (e.g., `push`).
    ///
    /// Rewrite: N parallel calls, one per field, with the T argument projected
    /// per field.
    ElementWriter,
    /// `fn(&List<T>, i32) -> T` — reads one element by index (e.g., `index_value`).
    ///
    /// Rewrite: at each use, read each per-field array at the same index and
    /// reconstruct a tuple/struct literal — but only when the surrounding
    /// expression is a `FieldAccess` with a constant field index (so we can
    /// dispatch directly to the relevant per-field read). Bare full-value reads
    /// cause the candidate to escape.
    IndexReader,
    /// `fn(&mut List<T>, i32, T) -> ()` — writes one element by index
    /// (e.g., `index_assign`).
    ///
    /// Rewrite: N parallel calls, sharing the same (duplicable) index and
    /// projecting the T argument per field.
    IndexWriter,
    /// `fn(i32) -> List<T>` (static, no receiver) — constructs a new container
    /// with the given capacity (e.g., `with_capacity`).
    ///
    /// Rewrite: N parallel calls, one per field, each constructing an
    /// `List<T_k>` with the same capacity.
    Constructor,
    /// `fn(&List<T>) -> i32 | bool` — length-invariant query with no element
    /// argument (e.g., `len`, `is_empty`, `capacity`).
    ///
    /// Rewrite: dispatch to field 0's corresponding method. Since push, slot
    /// assign, and constructor-with-capacity all keep per-field arrays in
    /// lockstep, field 0 is representative.
    ///
    /// We restrict the return type to `i32`/`bool` so that only true
    /// length-invariant queries qualify — a hypothetical `hash_code() -> u64`
    /// that depends on element contents would not match.
    Query,
}

/// Classify a `List` method into a [`ListMethodKind`] by signature shape, read
/// against the element type its `monomorph_info` names.
fn classify_array_method_sig(func: &NirFunction, type_table: &TypeTable) -> Option<ListMethodKind> {
    // Must be a method (instance or static) on `List`.
    let info = func.method_info.as_ref()?;
    if info.receiver_decl_name() != "List" {
        return None;
    }
    // Must be a monomorphized instance so we know the concrete element type.
    let mono = func.monomorph_info.as_ref()?;
    if mono.impl_type_args.len() != 1 {
        return None;
    }
    let elem_ty = mono.impl_type_args[0];

    let params: Vec<TypeId> = func.params.iter().map(|p| p.type_id).collect();
    let ret = func.return_type;

    let is_list_of_t = |ty: TypeId| type_table.as_list(ty) == Some(elem_ty);
    let is_ref_list_of_t = |ty: TypeId| {
        matches!(
            type_table.get(ty),
            ResolvedType::Ref(inner) if type_table.as_list(*inner) == Some(elem_ty)
        )
    };
    let is_mut_ref_list_of_t = |ty: TypeId| {
        matches!(
            type_table.get(ty),
            ResolvedType::MutRef(inner) if type_table.as_list(*inner) == Some(elem_ty)
        )
    };
    let is_t = |ty: TypeId| ty == elem_ty;
    let is_i32 = |ty: TypeId| ty == TypeTable::I32;
    let is_unit = |ty: TypeId| ty == TypeTable::UNIT;
    // Length-invariant query return types: i32 (len, capacity) or bool (is_empty).
    let is_query_return = |ty: TypeId| ty == TypeTable::I32 || ty == TypeTable::BOOL;

    match params.as_slice() {
        // fn(&List<T>) -> i32 | bool — Query
        [p0] if is_ref_list_of_t(*p0) && is_query_return(ret) => Some(ListMethodKind::Query),
        // fn(i32) -> List<T> — Constructor (static)
        [p0] if is_i32(*p0) && is_list_of_t(ret) => Some(ListMethodKind::Constructor),
        // fn(&mut List<T>, T) -> () — ElementWriter
        [p0, p1] if is_mut_ref_list_of_t(*p0) && is_t(*p1) && is_unit(ret) => {
            Some(ListMethodKind::ElementWriter)
        }
        // fn(&List<T>, i32) -> T — IndexReader
        [p0, p1] if is_ref_list_of_t(*p0) && is_i32(*p1) && is_t(ret) => {
            Some(ListMethodKind::IndexReader)
        }
        // fn(&mut List<T>, i32, T) -> () — IndexWriter
        [p0, p1, p2] if is_mut_ref_list_of_t(*p0) && is_i32(*p1) && is_t(*p2) && is_unit(ret) => {
            Some(ListMethodKind::IndexWriter)
        }
        _ => None,
    }
}

/// Lookup: method family `SigKey` → `ListMethodKind`. Built once from the
/// function table and used at every call site to classify the operation.
/// Since classification depends only on signature *shape* (not element type),
/// one entry per family suffices — `List<i32>::push` and `List<i64>::push`
/// share `((None, "push"), ElementWriter)`.
type SigKindIndex = IndexMap<SigKey, ListMethodKind>;

/// Per-callee classification: a `List` method's [`FuncId`](crate::nir::FuncId) →
/// its [`ListMethodKind`]. Lets a call site be classified by its stamped
/// `func_id` instead of reading the call node's `FunctionRef`. Built alongside
/// [`SigKindIndex`]; an entry exists for exactly the functions whose `SigKey`
/// classifies, so `id_kinds.get(call.func_id) == sig_kinds.get(sig_key_of(call))`.
type IdKindIndex = IndexMap<crate::nir::FuncId, ListMethodKind>;

/// The method-signature classification, bundled so it threads as one borrow.
struct MethodSig {
    id_kinds: IdKindIndex,
    /// A `List` method's [`FuncId`](crate::nir::FuncId) → its `SigKey`, so the
    /// rewriter recovers the callee's `(trait, method)` by id (for catalog
    /// retargeting) instead of reading the call node's `FunctionRef`.
    id_sigkeys: IndexMap<crate::nir::FuncId, SigKey>,
    /// Element type `T` and [`ListMethodKind`] → the [`SigKey`] of a
    /// monomorphized `List<T>` method of that kind. Direct index so
    /// [`find_sig_key_for_kind`] is a single lookup, not a per-call catalog scan.
    kind_index: IndexMap<(TypeId, ListMethodKind), SigKey>,
}

/// Lookup table: (element type `T_k`, (trait, method)) → `FunctionRef` for
/// `List<T_k>::method`. Built once per pass.
type MethodCatalog = IndexMap<(TypeId, SigKey), (FunctionRef, crate::nir::FuncId)>;

/// A local that is a candidate for container SROA.
struct Candidate {
    /// Original local index
    local_index: u32,
    /// Original local name (for generating new local names)
    local_name: String,
    /// Element types of the container (for tuples: the tuple element types;
    /// for structs: the struct field types in declaration order).
    element_types: Vec<TypeId>,
    /// Whether every field is a scalar (carries no identity, needs no value
    /// copy). Only then may a slot copy `v[i] = $value_copy$T(v[j])` be seen
    /// through: the decomposition becomes per-field scalar copies. With an
    /// identity-carrying field the wrapper is load-bearing and must block SROA.
    all_scalar: bool,
    /// How the element is laid out: tuple or user struct. Determines which
    /// literal shape (`TupleLiteral` vs `StructLiteral`) is accepted as a
    /// decomposable source, and is carried into the rewrite for consistent
    /// treatment.
    layout: ElementLayout,
    /// Span of the original let statement
    span: Span,
    /// Form of the initializer — currently always a `Constructor` call whose
    /// (duplicable) capacity expression is carried forward to build the
    /// per-field `List<T_k>::with_capacity(...)` calls during rewrite.
    init: CandidateInit,
}

/// Layout of the per-element container value.
#[derive(Debug, Clone)]
enum ElementLayout {
    /// `List<Tuple<T_0, T_1, ..., T_n>>` — decomposable sources are
    /// `TupleLiteral` (or another decomposable container's `index_value`).
    Tuple,
    /// `List<UserStruct>` — decomposable sources are `StructLiteral` of that
    /// specific struct (or another decomposable container's `index_value`).
    /// The `type_id` identifies the exact struct type so we reject literals
    /// of a different (even structurally-compatible) struct.
    Struct { type_id: TypeId },
}

/// How the candidate was initialized.
///
/// Any List method classified as `Constructor` with a single duplicable
/// capacity argument qualifies. The capacity expression (an arena `ExprId` in
/// the live body) is deep-cloned once per decomposed field at rewrite time, so
/// it must be side-effect-free.
struct CandidateInit {
    /// Capacity operand passed to each per-field `with_capacity(...)` call —
    /// a skeleton subtree (cloned per field) or a promoted constant
    /// (re-materialised per field).
    capacity: Operand,
}

/// Apply container SROA to all functions in the project.
pub fn scalarize_containers(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    // Build the method catalog + signature-kind index once, using an immutable
    // borrow on functions. Both indexes are derived from the same scan.
    let (catalog, method_sig) = {
        let type_table = project.type_table.borrow();
        build_method_catalog(project, &type_table)
    };
    if catalog.is_empty() {
        return false;
    }

    // Build a struct lookup: (name, module_source) → &NirStruct. Used by
    // `collect_candidates` to expand `List<UserStruct>` element types.
    let struct_index = build_struct_index(&project.structs);

    // Per-function engine session, gate-skipped. Mutations route through the
    // engine API; the rule fires once at the body root (whole-function shape).
    // Retargeting some `List<Tuple>::m` calls to per-field `List<F>::m` callees
    // shifts the function's call edges, which only costs propagation precision,
    // not correctness.
    let type_table_rc = project.type_table.clone();
    let value_copy_ids = project.value_copy_func_ids();
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::ContainerSroa, len, |fid| {
        let func_rc = &project.functions[fid.index()];
        // Skip CM bindings (ABI bridges) and body-less declarations.
        {
            let func = func_rc.borrow();
            if func.is_cm_binding || func.body.is_none() {
                return false;
            }
        }
        let mut func = func_rc.borrow_mut();
        let rule = ContainerSroaRule {
            catalog: &catalog,
            sig: &method_sig,
            struct_index: &struct_index,
            type_table_rc: type_table_rc.clone(),
            value_copy_ids: &value_copy_ids,
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        // A promoted constant capacity is re-materialized during the rewrite;
        // `materialize_operand` falls back to a decimal repr without the type
        // table, which is exact for the non-negative capacity ints. (The session
        // cannot hold a `type_table` borrow — the rule's `make_list` needs
        // `borrow_mut`.)
        engine.run(&[&rule])
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function container SROA at the body root.
pub(super) struct ContainerSroaRule<'a> {
    catalog: &'a MethodCatalog,
    sig: &'a MethodSig,
    struct_index: &'a StructIndex<'a>,
    /// Shared `TypeTable` — `make_list(elem_ty)` interns per-field array types
    /// during the local-allocation step. Borrowed through the `Rc` to avoid
    /// holding a long mutable borrow across the rewrite.
    type_table_rc: std::rc::Rc<std::cell::RefCell<TypeTable>>,
    /// The `$value_copy$T` helper ids. A slot copy `v[i] = $value_copy$T(v[j])`
    /// of an all-scalar element decomposes to per-field scalar copies, so the
    /// wrapper is seen through during decomposition.
    value_copy_ids: &'a IndexSet<crate::nir::FuncId>,
    /// Whole-function rewrite: only run once per session.
    applied: Cell<bool>,
}

impl Rule for ContainerSroaRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        scalarize_at_root(engine, self)
    }
}

/// Lookup index for user-defined structs by (name, module source).
type StructIndex<'a> = IndexMap<(String, ModuleSource), &'a NirStruct>;

fn build_struct_index(structs: &[NirStruct]) -> StructIndex<'_> {
    let mut out: StructIndex = IndexMap::default();
    for s in structs {
        out.insert((s.name.clone(), s.module_source.clone()), s);
    }
    out
}

/// Build a catalog of monomorphized `List<T>::{method}` function references in
/// this project, plus a parallel `SigKindIndex` that classifies each method
/// family into an `ListMethodKind` based on its signature shape.
fn build_method_catalog(
    project: &NirPackage,
    type_table: &TypeTable,
) -> (MethodCatalog, MethodSig) {
    let mut catalog = MethodCatalog::default();
    let mut sig_kinds = SigKindIndex::default();
    let mut id_kinds = IdKindIndex::default();
    let mut id_sigkeys: IndexMap<crate::nir::FuncId, SigKey> = IndexMap::default();
    let mut kind_index: IndexMap<(TypeId, ListMethodKind), SigKey> = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        // Skip dead/extern declarations: a bodyless function's signature types
        // may already be DCE'd, so reading them (element type, sig classify)
        // would dangle. `dce` clears dead bodies in place (Phase 4).
        if func.body.is_none() {
            continue;
        }
        // Must be an instance/static method with `method_info`.
        let Some(method_info) = &func.method_info else {
            continue;
        };
        // Must be a method on List (by base struct name). The kind is still
        // List-specific because the pass itself is List-specific — we only
        // de-hardcode method *names*, not the container type.
        if method_info.receiver_decl_name() != "List" {
            continue;
        }
        // Must be a monomorphized method (we need the concrete impl type arg).
        let Some(mono) = &func.monomorph_info else {
            continue;
        };
        // Expect exactly one impl type arg (the element type T).
        if mono.impl_type_args.len() != 1 {
            continue;
        }
        let element_ty = mono.impl_type_args[0];
        let sig_key: SigKey = (
            method_info.trait_name.clone(),
            method_info.method_name.clone(),
        );
        let func_ref = FunctionRef::from_resolved(&func, func.module_source.clone());
        let func_id = func.id.expect("func_id assigned at lower");
        id_sigkeys.insert(func_id, sig_key.clone());
        // First-writer wins (there should only be one per (T, sig)).
        catalog
            .entry((element_ty, sig_key.clone()))
            .or_insert((func_ref, func_id));

        // Classify this method by signature shape. A method family (same
        // `SigKey`) has the same shape across all element types, so first-
        // writer-wins is fine here too. The family's kind, once known, is also
        // recorded per `func_id` so call sites classify by id.
        let kind = sig_kinds
            .get(&sig_key)
            .copied()
            .or_else(|| classify_array_method_sig(&func, type_table));
        if let Some(kind) = kind {
            kind_index
                .entry((element_ty, kind))
                .or_insert_with(|| sig_key.clone());
            sig_kinds.entry(sig_key).or_insert(kind);
            id_kinds.insert(func_id, kind);
        }
    }
    (
        catalog,
        MethodSig {
            id_kinds,
            id_sigkeys,
            kind_index,
        },
    )
}

/// Whole-function container SROA driven from the engine session root.
fn scalarize_at_root(engine: &mut Engine, rule: &ContainerSroaRule) -> bool {
    // Step 1: collect candidates. Immutable borrow of type_table.
    let candidates = {
        let type_table = rule.type_table_rc.borrow();
        collect_candidates(
            engine.body,
            &type_table,
            rule.struct_index,
            rule.sig,
            rule.value_copy_ids,
        )
    };
    if candidates.is_empty() {
        return false;
    }

    // Step 2: escape analysis. Build safe-set via whitelist + tuple-source fixpoint.
    // Also track which `ListMethodKind`s were observed on each whitelisted use,
    // so step 3 can demand only the monomorphizations that will actually be
    // emitted per field (rather than unconditionally requiring all four kinds).
    let (safe_indices, used_kinds_map) =
        compute_safe_set(engine.body, &candidates, rule.sig, rule.value_copy_ids);
    if safe_indices.is_empty() {
        return false;
    }

    // Step 3: verify that every required (element_ty, sig) is present in the catalog.
    // Required kinds = `Constructor` (always, for the initializer) ∪ observed
    // kinds. If any candidate has missing monomorphizations, drop it.
    let empty_used: IndexSet<ListMethodKind> = IndexSet::default();
    let safe_candidates: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| safe_indices.contains(&c.local_index))
        .filter(|c| {
            let used = used_kinds_map.get(&c.local_index).unwrap_or(&empty_used);
            required_methods_available(c, used, rule.sig)
        })
        .collect();
    if safe_candidates.is_empty() {
        return false;
    }

    // Step 4: allocate parallel `List<T_k>` locals through the engine. The
    // type-table borrow is scoped so it does not overlap the engine's locals
    // mutation (`alloc_local` takes `&mut self`).
    let mut field_map: IndexMap<(u32, u32), FieldList> = IndexMap::default();
    let mut decomposed: IndexSet<u32> = IndexSet::default();
    for c in &safe_candidates {
        for (k, &elem_ty) in c.element_types.iter().enumerate() {
            let list_type = rule.type_table_rc.borrow_mut().make_list(elem_ty);
            let name = format!("__csroa_{}_{}", c.local_name, k);
            let local_index = engine.alloc_local(name.clone(), list_type, /* is_mut */ false);
            field_map.insert(
                (c.local_index, k as u32),
                FieldList {
                    local_index,
                    name,
                    list_type,
                    elem_type: elem_ty,
                },
            );
        }
        decomposed.insert(c.local_index);
    }

    // Build a lookup from local_index → candidate data needed during rewrite.
    let candidate_data: IndexMap<u32, CandidateRewriteInfo> = safe_candidates
        .iter()
        .map(|c| {
            (
                c.local_index,
                CandidateRewriteInfo {
                    element_types: c.element_types.clone(),
                    all_scalar: c.all_scalar,
                    layout: c.layout.clone(),
                    span: c.span,
                    init: CandidateInit {
                        capacity: c.init.capacity,
                    },
                },
            )
        })
        .collect();

    // Step 5: rewrite the body via the engine edit API.
    let ctx = RewriteCtx {
        decomposed: &decomposed,
        field_map: &field_map,
        candidate_data: &candidate_data,
        catalog: rule.catalog,
        sig: rule.sig,
        value_copy_ids: rule.value_copy_ids,
    };
    let root = engine.body.root;
    Rewriter { ctx: &ctx }.rewrite_block(engine, root);

    true
}

/// Data carried from analysis into rewrite for each decomposed candidate.
struct CandidateRewriteInfo {
    element_types: Vec<TypeId>,
    all_scalar: bool,
    layout: ElementLayout,
    span: Span,
    init: CandidateInit,
}

/// The parallel `List<T_k>` local one decomposed field became.
#[derive(Clone)]
struct FieldList {
    local_index: u32,
    name: String,
    /// `List<T_k>` — the new local's own type.
    list_type: TypeId,
    /// `T_k` — the per-field element type, the catalog's lookup key.
    elem_type: TypeId,
}

struct RewriteCtx<'a> {
    decomposed: &'a IndexSet<u32>,
    field_map: &'a IndexMap<(u32, u32), FieldList>,
    candidate_data: &'a IndexMap<u32, CandidateRewriteInfo>,
    catalog: &'a MethodCatalog,
    sig: &'a MethodSig,
    value_copy_ids: &'a IndexSet<crate::nir::FuncId>,
}

/// Whether the catalog holds, for every per-field element type, each
/// [`ListMethodKind`] the rewrite will emit.
///
/// `Constructor` is always needed; the rest only where escape analysis observed
/// a use. `Query` dispatches to field 0, so only field 0 needs it.
fn required_methods_available(
    c: &Candidate,
    used_kinds: &IndexSet<ListMethodKind>,
    sig: &MethodSig,
) -> bool {
    for (fi, &t) in c.element_types.iter().enumerate() {
        // Constructor is always needed for every field's initializer.
        if find_sig_key_for_kind(sig, t, ListMethodKind::Constructor).is_none() {
            return false;
        }
        for &kind in used_kinds {
            // `Query` (len / is_empty / capacity) dispatches to field 0 only, so
            // only field 0 needs its monomorphization. Element writers/readers
            // operate per field and are required for every field.
            if kind == ListMethodKind::Query && fi != 0 {
                continue;
            }
            if find_sig_key_for_kind(sig, t, kind).is_none() {
                return false;
            }
        }
    }
    true
}

/// The `SigKey` of a monomorphized `List<elem_ty>` method classified as `kind`,
/// or `None` if no such method is monomorphized in this project. O(1) via the
/// pre-built `(TypeId, ListMethodKind)` index.
fn find_sig_key_for_kind(sig: &MethodSig, elem_ty: TypeId, kind: ListMethodKind) -> Option<SigKey> {
    sig.kind_index.get(&(elem_ty, kind)).cloned()
}

/// Collect candidate `let` bindings across the whole function body. The escape
/// analysis and rewriter both walk every reachable block, so a `List<Tuple>` /
/// `List<Struct>` local bound inside a nested block (an `if` arm, loop body, or
/// labeled block) is a valid candidate too — locals are function-scoped.
fn collect_candidates(
    body: &Body,
    type_table: &TypeTable,
    struct_index: &StructIndex<'_>,
    sig: &MethodSig,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for block in reachable_blocks(body) {
        for s in &body.blocks[block].stmts {
            let StmtKind::Let {
                name,
                local_index,
                type_id,
                value,
                ..
            } = &body.stmts[*s].kind
            else {
                continue;
            };
            // Type must be List<Tuple<...>> or List<UserStruct>.
            let Some(elem_ty) = type_table.as_list(*type_id) else {
                continue;
            };
            let Some((layout, element_types)) =
                element_layout_of(elem_ty, type_table, struct_index)
            else {
                continue;
            };
            if element_types.is_empty() {
                continue;
            }
            // Initializer must be one of the recognized forms.
            let Some(init) = recognize_init_operand(body, *value, sig, value_copy_ids) else {
                continue;
            };
            let all_scalar = element_types
                .iter()
                .all(|t| !crate::lower::plan::value_copy::needs_value_copy(*t, type_table));
            out.push(Candidate {
                local_index: *local_index,
                local_name: name.clone(),
                element_types,
                all_scalar,
                layout,
                span: body.stmts[*s].span,
                init,
            });
        }
    }
    out
}

/// Determine the decomposable layout and per-field types for a container element
/// type. Returns `Some((layout, types))` if the element is a tuple or a
/// user-defined struct whose fields are indexed 0..N; `None` otherwise.
fn element_layout_of(
    elem_ty: TypeId,
    type_table: &TypeTable,
    struct_index: &StructIndex<'_>,
) -> Option<(ElementLayout, Vec<TypeId>)> {
    // Tuple element (e.g., `List<[i32, i32]>`).
    if let Some(tuple_elems) = type_table.as_tuple(elem_ty) {
        return Some((ElementLayout::Tuple, tuple_elems));
    }
    // User struct element (e.g., `List<Point>`). Generic struct instances
    // appear as `ResolvedType::Struct` after monomorphization.
    if let ResolvedType::Struct {
        decl_name,
        module_source,
        type_args,
    } = type_table.get(elem_ty)
    {
        let key = (
            type_table.struct_rendered_name(decl_name, type_args),
            module_source.clone(),
        );
        let tir_struct = struct_index.get(&key)?;
        if tir_struct.fields.is_empty() {
            return None;
        }
        // Fields indexed 0..N by declaration order. We sort defensively.
        let mut ordered: Vec<&crate::nir::NirField> = tir_struct.fields.iter().collect();
        ordered.sort_by_key(|f| f.index);
        for (i, f) in ordered.iter().enumerate() {
            if f.index != i as u32 {
                return None;
            }
        }
        let field_types: Vec<TypeId> = ordered.iter().map(|f| f.type_id).collect();
        return Some((ElementLayout::Struct { type_id: elem_ty }, field_types));
    }
    None
}

fn recognize_init_operand(
    body: &Body,
    op: Operand,
    sig: &MethodSig,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> Option<CandidateInit> {
    op.as_expr()
        .and_then(|e| recognize_init(body, e, sig, value_copy_ids))
}

/// Strip a single `$value_copy$T(inner)` wrapper, returning its inner
/// expression, or `None` when `e` is not a one-argument value-copy call. Shared
/// by every SROA site that sees through a value copy (including `sroa`'s
/// soft-escape walk).
pub(super) fn strip_one_value_copy(
    body: &Body,
    e: ExprId,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> Option<ExprId> {
    let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
        return None;
    };
    if value_copy_ids.contains(func_id) && args.len() == 1 {
        args[0].expr.as_expr()
    } else {
        None
    }
}

/// Peel `$value_copy$T(inner)` wrappers, returning the innermost expression. A
/// value copy of a fresh value (a constructor result) is a no-op.
fn peel_value_copy(
    body: &Body,
    e: ExprId,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> ExprId {
    let mut cur = e;
    while let Some(inner) = strip_one_value_copy(body, cur, value_copy_ids) {
        cur = inner;
    }
    cur
}

/// Recognize the supported initializer form for container-SROA candidates.
///
/// Two equivalent shapes are accepted, both matched purely *structurally*
/// (no hardcoded label or method names):
///
/// 1. A direct `Call` classified as `Constructor` by signature — e.g.
///    `List::<T>::with_capacity(cap)` written by the user directly. The
///    argument must be side-effect-free so it can be cloned once per field.
/// 2. The `SequenceLiteralBuilder` desugaring for empty array literals
///    (`[]`), which lowers to
///    `{ let __b = <Constructor call>; break label: __b.<build>(); }`.
///    We structurally unwrap the labeled block, look through the `Let`, and
///    fall through to form (1) on the inner constructor call. Neither the
///    label string nor the builder method name is inspected — only the
///    shape of the wrapper.
fn recognize_init(
    body: &Body,
    value: ExprId,
    sig: &MethodSig,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> Option<CandidateInit> {
    // The constructor result is fresh, so a `$value_copy$T` wrapping the whole
    // initializer (inserted for the by-value binding) is a no-op — see through
    // it to reach the `Constructor` call.
    let value = peel_value_copy(body, value, value_copy_ids);
    let inner = unwrap_builder_labeled_block(body, value).unwrap_or(value);
    let inner = peel_value_copy(body, inner, value_copy_ids);
    let ExprKind::Call { func_id, args, .. } = &body.exprs[inner].kind else {
        return None;
    };
    if list_method_kind(*func_id, sig) != Some(ListMethodKind::Constructor) {
        return None;
    }
    if args.len() != 1 {
        return None;
    }
    let cap = args[0].expr;
    // The capacity expression is cloned once per per-field constructor
    // call during rewrite, so it must be side-effect-free. A promoted constant
    // is trivially duplicable.
    if !cap.as_expr().is_none_or(|e| is_duplicable_expr(body, e)) {
        return None;
    }
    Some(CandidateInit { capacity: cap })
}

/// Unwrap a labeled block of shape
/// `{ let __b = <X>; break label: <method on __b> }` and return `<X>`.
///
/// This is the structural shape of the `[]` literal desugaring via
/// `SequenceLiteralBuilder`: the `Let` holds the constructor call, and the
/// `Break` holds the `build()` method call on the freshly constructed local.
/// We don't check the label, the binding name, or the break method's name —
/// only that the `Break` exits this block by calling a zero-argument method
/// whose receiver is the `Let`'s local (directly or via `&__b` / `&mut __b`).
fn unwrap_builder_labeled_block(body: &Body, expr: ExprId) -> Option<ExprId> {
    let ExprKind::LabeledBlock { label, block, .. } = &body.exprs[expr].kind else {
        return None;
    };
    let block = *block;
    if body.blocks[block].stmts.len() != 2 {
        return None;
    }
    let s0 = body.blocks[block].stmts[0];
    let s1 = body.blocks[block].stmts[1];
    let StmtKind::Let {
        local_index: b_local,
        value: inner,
        ..
    } = &body.stmts[s0].kind
    else {
        return None;
    };
    let b_local = *b_local;
    let inner = *inner;
    let StmtKind::Break {
        label: brk_label,
        value: Some(brk_val),
    } = &body.stmts[s1].kind
    else {
        return None;
    };
    if brk_label.as_deref() != Some(label.as_str()) {
        return None;
    }
    let brk_val = *brk_val;
    // Break value must be a zero-argument method call whose receiver is `__b`
    // (possibly wrapped in `&`/`&mut`).
    let ExprKind::MethodCall { receiver, args, .. } = &body.exprs[brk_val.as_expr()?].kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let receiver_local = match receiver.as_expr().map(|re| &body.exprs[re].kind) {
        Some(ExprKind::Local { index, .. }) => *index,
        Some(ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner_ref,
        }) => {
            let Some(ExprKind::Local { index, .. }) =
                inner_ref.as_expr().map(|ir| &body.exprs[ir].kind)
            else {
                return None;
            };
            *index
        }
        _ => return None,
    };
    if receiver_local != b_local {
        return None;
    }
    inner.as_expr()
}

/// Look up the `ListMethodKind` of a call target by signature, via the
/// pre-built `SigKindIndex`. Returns `None` for non-method functions, non-List
/// methods, or List methods whose signature didn't match any kind.
fn list_method_kind(func_id: crate::nir::FuncId, sig: &MethodSig) -> Option<ListMethodKind> {
    sig.id_kinds.get(&func_id).copied()
}

/// The callee's `SigKey` by its stamped `func_id` (the rewriter's catalog
/// retarget key), or `None` for a non-`List`-method callee.
fn sig_key_of_id(sig: &MethodSig, func_id: crate::nir::FuncId) -> Option<SigKey> {
    sig.id_sigkeys.get(&func_id).cloned()
}

/// Compute the set of safe (decomposable) candidate locals via whitelist escape
/// analysis plus a fixpoint for element-source dependency.
fn compute_safe_set(
    body: &Body,
    candidates: &[Candidate],
    sig: &MethodSig,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> (IndexSet<u32>, IndexMap<u32, IndexSet<ListMethodKind>>) {
    let shape_of: IndexMap<u32, CandidateShape> = candidates
        .iter()
        .map(|c| {
            (
                c.local_index,
                CandidateShape {
                    arity: c.element_types.len(),
                    layout: c.layout.clone(),
                    all_scalar: c.all_scalar,
                },
            )
        })
        .collect();

    // Iterate to fixpoint: start with all candidates safe, then remove any that
    // reference an escaped candidate via element-source push. The used-kinds map
    // is rebuilt from scratch each iteration so it reflects the final safe set.
    let mut safe: IndexSet<u32> = candidates.iter().map(|c| c.local_index).collect();
    let used_kinds = loop {
        let mut checker = WhitelistChecker {
            safe: &safe,
            shape_of: &shape_of,
            value_copy_ids,
            sig,
            escaped: IndexSet::default(),
            used_kinds: IndexMap::default(),
        };
        checker.visit(body, NodeRef::Block(body.root));
        if checker.escaped.is_empty() {
            break checker.used_kinds;
        }
        for idx in checker.escaped {
            safe.shift_remove(&idx);
        }
    };
    (safe, used_kinds)
}

/// The per-candidate facts the whitelist walk keys on.
struct CandidateShape {
    /// Number of parallel lists the candidate decomposes to.
    arity: usize,
    layout: ElementLayout,
    /// Whether every field is a scalar; gates the `$value_copy$T` see-through
    /// in [`WhitelistChecker::check_source`].
    all_scalar: bool,
}

struct WhitelistChecker<'a> {
    safe: &'a IndexSet<u32>,
    shape_of: &'a IndexMap<u32, CandidateShape>,
    value_copy_ids: &'a IndexSet<crate::nir::FuncId>,
    sig: &'a MethodSig,
    escaped: IndexSet<u32>,
    /// Per-candidate set of `ListMethodKind`s observed on whitelisted uses.
    used_kinds: IndexMap<u32, IndexSet<ListMethodKind>>,
}

impl WhitelistChecker<'_> {
    /// The shape of a `safe` local. Every safe local was collected as a
    /// candidate, so a miss is a bug rather than a case to default through.
    fn shape(&self, idx: u32) -> &CandidateShape {
        self.shape_of
            .get(&idx)
            .unwrap_or_else(|| panic!("safe local {idx} has no candidate shape"))
    }

    fn mark(&mut self, idx: u32) {
        if self.safe.contains(&idx) {
            self.escaped.insert(idx);
        }
    }

    /// Record that a whitelisted call of `kind` was observed on candidate `idx`.
    fn record_use(&mut self, idx: u32, kind: ListMethodKind) {
        self.used_kinds.entry(idx).or_default().insert(kind);
    }

    /// Default walk: recurse into every id-bearing child. The checker only
    /// overrides expression handling (`visit_expr`); statements, blocks, and
    /// patterns fall to this walk.
    fn visit(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(e) = node {
            self.visit_expr(body, e);
            return;
        }
        self.walk(body, node);
    }

    fn walk(&mut self, body: &Body, node: NodeRef) {
        body.for_each_child(node, |c| self.visit(body, c));
    }

    /// Visit an operand for escape analysis. A promoted constant
    /// (`Operand::Value`) references no local, so there is nothing to visit.
    fn visit_operand(&mut self, body: &Body, op: Operand) {
        if let Some(e) = op.as_expr() {
            self.visit_expr(body, e);
        }
    }

    /// Operand form of [`Self::check_source`]: a promoted constant is a scalar,
    /// never a decomposable tuple/struct source, so it is not a valid element
    /// source for a candidate.
    fn check_source_operand(
        &mut self,
        body: &Body,
        op: Operand,
        expected_arity: usize,
        expected_layout: &ElementLayout,
        src_all_scalar: bool,
    ) -> bool {
        match op.as_expr() {
            Some(e) => self.check_source(body, e, expected_arity, expected_layout, src_all_scalar),
            None => false,
        }
    }

    /// Check an expression used as a value-source for `push`/`index_assign`.
    /// `src_all_scalar` says the destination element is all-scalar, which is the
    /// precondition for seeing through a `$value_copy$T` wrapper.
    fn check_source(
        &mut self,
        body: &Body,
        e: ExprId,
        expected_arity: usize,
        expected_layout: &ElementLayout,
        src_all_scalar: bool,
    ) -> bool {
        // See through a defensive slot copy `v[i] = $value_copy$T(src)` when the
        // element is all-scalar: after decomposition each field is copied by
        // value, so the struct-level clone is redundant. For an identity-carrying
        // field the copy is load-bearing, so leave it (this arm doesn't fire) and
        // SROA conservatively bails on the candidate.
        if src_all_scalar && let Some(inner) = strip_one_value_copy(body, e, self.value_copy_ids) {
            return self.check_source(body, inner, expected_arity, expected_layout, src_all_scalar);
        }
        match &body.exprs[e].kind {
            // Direct tuple literal `[e0, e1, ...]` (heap or multi-value form).
            ExprKind::TupleLiteral { elements } => {
                if !matches!(expected_layout, ElementLayout::Tuple) {
                    return false;
                }
                if elements.len() != expected_arity {
                    return false;
                }
                let elements = elements.clone();
                for el in elements {
                    if let Some(e) = el.as_expr() {
                        self.visit_expr(body, e);
                    }
                }
                true
            }
            // Direct struct literal: StructName { field_0: v0, field_1: v1, ... }
            ExprKind::StructLiteral {
                struct_type,
                fields,
                ..
            } => {
                let ElementLayout::Struct {
                    type_id: expected_ty,
                } = expected_layout
                else {
                    return false;
                };
                if struct_type != expected_ty {
                    return false;
                }
                if fields.len() != expected_arity {
                    return false;
                }
                // Field indices must cover 0..N exactly once so the rewrite can
                // pull per-field values unambiguously.
                let mut seen = vec![false; expected_arity];
                for f in fields {
                    let k = f.field_index as usize;
                    if k >= expected_arity || seen[k] {
                        return false;
                    }
                    seen[k] = true;
                }
                let field_vals: Vec<Operand> = fields.iter().map(|f| f.value).collect();
                for v in field_vals {
                    if let Some(e) = v.as_expr() {
                        self.visit_expr(body, e);
                    }
                }
                true
            }
            // Element from another candidate: other.IndexReader(j)
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } if list_method_kind(*func_id, self.sig) == Some(ListMethodKind::IndexReader) => {
                if args.len() != 1 {
                    return false;
                }
                let receiver = *receiver;
                let arg0 = args[0].expr;
                let Some(other) = receiver_local(body, receiver) else {
                    // Receiver isn't a bare local — recurse normally.
                    self.visit_expr(body, e);
                    return false;
                };
                // The receiver must be one of our candidates with matching arity.
                if !self.safe.contains(&other) {
                    return false;
                }
                let other_shape = self.shape(other);
                if other_shape.arity != expected_arity {
                    return false;
                }
                // Layouts must match: tuple ↔ tuple, and struct ↔ same struct.
                if !layouts_compatible(expected_layout, &other_shape.layout) {
                    return false;
                }
                // The rewrite clones the index expression N times (once per
                // field). A promoted constant index is trivially duplicable.
                if !arg0.as_expr().is_none_or(|e| is_duplicable_expr(body, e)) {
                    // Fall through to a normal visit so `other` gets marked
                    // escaped via the bare `index_value` branch in `visit_expr`.
                    self.visit_expr(body, e);
                    return false;
                }
                // Index expression must be visited as a normal expression.
                if let Some(e) = arg0.as_expr() {
                    self.visit_expr(body, e);
                }
                // Record that `other` is being read via IndexReader so it
                // needs that method monomorphization during rewrite.
                self.record_use(other, ListMethodKind::IndexReader);
                true
            }
            _ => false,
        }
    }

    fn visit_expr(&mut self, body: &Body, e: ExprId) {
        match &body.exprs[e].kind {
            // v.method(...) — inspect receiver for whitelisted patterns.
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } => {
                let receiver = *receiver;
                let Some(recv_e) = receiver.as_expr() else {
                    return;
                };
                // Args are operands: a promoted constant (`Operand::Value`) is a
                // valid index/source — duplicability and decomposability are
                // judged per arg below, not by requiring every arg be a skeleton
                // expression.
                let arg_ops: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                let kind = list_method_kind(*func_id, self.sig);
                if let Some(rec_local) = receiver_local(body, receiver)
                    && self.safe.contains(&rec_local)
                {
                    match (kind, arg_ops.len()) {
                        // v.push-shaped(source)
                        (Some(ListMethodKind::ElementWriter), 1) => {
                            let shape = self.shape(rec_local);
                            let (arity, layout, all_scalar) =
                                (shape.arity, shape.layout.clone(), shape.all_scalar);
                            if self
                                .check_source_operand(body, arg_ops[0], arity, &layout, all_scalar)
                            {
                                self.record_use(rec_local, ListMethodKind::ElementWriter);
                            } else {
                                self.mark(rec_local);
                            }
                            return;
                        }
                        // v.len() / v.is_empty() / v.capacity() — Query, no arg
                        (Some(ListMethodKind::Query), 0) => {
                            self.record_use(rec_local, ListMethodKind::Query);
                            return;
                        }
                        // v.index_assign-shaped(i, source)
                        (Some(ListMethodKind::IndexWriter), 2) => {
                            let shape = self.shape(rec_local);
                            let (arity, layout, all_scalar) =
                                (shape.arity, shape.layout.clone(), shape.all_scalar);
                            // The rewrite clones the destination index N times.
                            if !is_duplicable_operand(body, arg_ops[0]) {
                                self.mark(rec_local);
                                self.visit_operand(body, arg_ops[0]);
                                self.visit_operand(body, arg_ops[1]);
                                return;
                            }
                            // index argument visited normally
                            self.visit_operand(body, arg_ops[0]);
                            if self
                                .check_source_operand(body, arg_ops[1], arity, &layout, all_scalar)
                            {
                                self.record_use(rec_local, ListMethodKind::IndexWriter);
                            } else {
                                self.mark(rec_local);
                            }
                            return;
                        }
                        // Bare v.index_value(i) — safe only when the *enclosing*
                        // expression is a `FieldAccess` (handled below). Reaching
                        // here directly means the whole struct value escapes.
                        (Some(ListMethodKind::IndexReader), 1) => {
                            self.mark(rec_local);
                            self.visit_operand(body, arg_ops[0]);
                            return;
                        }
                        // Any other method call on a candidate → escape.
                        _ => {
                            self.mark(rec_local);
                        }
                    }
                }
                // Fall through: recurse into receiver and args normally.
                self.visit_expr(body, recv_e);
                for a in arg_ops {
                    self.visit_operand(body, a);
                }
            }
            // v.IndexReader(i).K — safe read pattern
            ExprKind::FieldAccess { expr: inner, .. } => {
                let inner = *inner;
                let safe_read = if let Some(inner_e) = inner.as_expr()
                    && let ExprKind::MethodCall {
                        receiver,
                        func_id,
                        args,
                        ..
                    } = &body.exprs[inner_e].kind
                    && list_method_kind(*func_id, self.sig) == Some(ListMethodKind::IndexReader)
                    && args.len() == 1
                    && let Some(rec_local) = receiver_local(body, *receiver)
                    && self.safe.contains(&rec_local)
                {
                    Some((rec_local, args[0].expr))
                } else {
                    None
                };
                if let Some((rec_local, idx_arg)) = safe_read {
                    // Safe — just visit the index expression.
                    self.record_use(rec_local, ListMethodKind::IndexReader);
                    if let Some(e) = idx_arg.as_expr() {
                        self.visit_expr(body, e);
                    }
                    return;
                }
                if let Some(inner_e) = inner.as_expr() {
                    self.visit_expr(body, inner_e);
                }
            }
            // Bare Local reference to a candidate → escape.
            ExprKind::Local { index, .. } => {
                self.mark(*index);
            }
            // Address taken on a candidate → escape.
            ExprKind::Unary { op, expr: inner } => {
                let inner = *inner;
                if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                    && let Some(ie) = inner.as_expr()
                    && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
                {
                    self.mark(*index);
                    return;
                }
                if let Some(ie) = inner.as_expr() {
                    self.visit_expr(body, ie);
                }
            }
            _ => self.walk(body, NodeRef::Expr(e)),
        }
    }
}

/// Both layouts must agree: either both tuple of the same arity, or both the
/// same struct type. Arity is enforced separately by the caller.
fn layouts_compatible(a: &ElementLayout, b: &ElementLayout) -> bool {
    match (a, b) {
        (ElementLayout::Tuple, ElementLayout::Tuple) => true,
        (ElementLayout::Struct { type_id: ta }, ElementLayout::Struct { type_id: tb }) => ta == tb,
        _ => false,
    }
}

/// If the operand is `Local { index }` — or `Unary::{Ref,MutRef}` wrapping a
/// Local — return the index. A promoted-value operand has no place, so `None`.
fn receiver_local(body: &Body, op: Operand) -> Option<u32> {
    let e = op.as_expr()?;
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => match inner.as_expr().map(|ie| &body.exprs[ie].kind) {
            Some(ExprKind::Local { index, .. }) => Some(*index),
            _ => None,
        },
        _ => None,
    }
}

/// Drives the in-place rewrite of decomposed container candidates over a
/// function body. `rewrite_block` performs the statement-level expansion
/// (candidate `Let` → N per-field `Let`s, and `push` / `index_assign`
/// expression-statements → N per-field statements); `rewrite_expr` rewrites the
/// remaining whitelisted reads (`len` / `is_empty`, `index_value(i).K`).
struct Rewriter<'a, 'c> {
    ctx: &'c RewriteCtx<'a>,
}

impl Rewriter<'_, '_> {
    /// Replace candidate let-bindings and expression-statement-level
    /// `push` / `index_assign` calls with their per-field versions; recurse
    /// into everything else.
    fn rewrite_block(&self, engine: &mut Engine, block: BlockId) {
        let old_stmts = engine.body.blocks[block].stmts.clone();
        let mut out: Vec<StmtId> = Vec::with_capacity(old_stmts.len());
        for s in old_stmts {
            self.process_stmt(engine, s, &mut out);
        }
        engine.set_block_stmts(block, out);
    }

    /// Route a statement: either emit its per-field expansion or recurse + push as-is.
    fn process_stmt(&self, engine: &mut Engine, s: StmtId, out: &mut Vec<StmtId>) {
        let ctx = self.ctx;
        // Candidate Let: expand in place.
        if let StmtKind::Let { local_index, .. } = &engine.body.stmts[s].kind
            && ctx.decomposed.contains(local_index)
        {
            let local_index = *local_index;
            self.expand_candidate_let(engine, local_index, out);
            return;
        }

        // Candidate push/index_assign as an ExprStmt at the statement level:
        // expand flat (no wrapping block). A whitelisted writer that fails to
        // expand is analysis/rewrite drift — panic rather than leave the removed
        // candidate binding dangling.
        if let StmtKind::Expr(Operand::Expr(expr)) = &engine.body.stmts[s].kind {
            let expr = *expr;
            if self.is_decomposed_writer_call(engine, expr) {
                let span = engine.body.stmts[s].span;
                let expanded = self
                    .try_expand_call_stmt(engine, expr, span)
                    .expect("decomposed-candidate writer call must expand");
                out.extend(expanded);
                return;
            }
        }

        // Otherwise, recurse into the statement (rewriting any nested
        // expressions/blocks) and push it unchanged.
        self.walk_children(engine, NodeRef::Stmt(s));
        out.push(s);
    }

    /// Emit N per-field Let statements for a decomposed candidate.
    fn expand_candidate_let(&self, engine: &mut Engine, local_index: u32, out: &mut Vec<StmtId>) {
        let ctx = self.ctx;
        let info = ctx
            .candidate_data
            .get(&local_index)
            .expect("candidate data must exist for decomposed local");
        let arity = info.element_types.len();
        let span = info.span;
        let capacity = info.init.capacity;
        for k in 0..arity {
            let field = ctx.field_map[&(local_index, k as u32)].clone();
            let cap = clone_or_dup(engine, capacity);
            let init = build_with_capacity_call(engine, &field, cap, span, ctx);
            let let_stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name: field.name,
                    local_index: field.local_index,
                    // The per-field list local is allocated `is_mut: false` — the
                    // slots are single-assignment (reassignment goes through
                    // `push` / `index_assign` on the same binding), so the `Let`
                    // must agree.
                    is_mut: false,
                    is_reactive: false,
                    type_id: field.list_type,
                    value: init.into(),
                    skip_value_copy: false,
                },
                span,
            );
            out.push(let_stmt);
        }
    }

    /// True when `e` is a `MethodCall` on a decomposed candidate classified as a
    /// writer (`ElementWriter` / `IndexWriter`) — a call the rewrite must expand
    /// (flat at statement level, or into a `Block` at an expression position).
    fn is_decomposed_writer_call(&self, engine: &Engine, e: ExprId) -> bool {
        let ExprKind::MethodCall {
            receiver, func_id, ..
        } = &engine.body.exprs[e].kind
        else {
            return false;
        };
        let Some(rec_local) = receiver_local(engine.body, *receiver) else {
            return false;
        };
        self.ctx.decomposed.contains(&rec_local)
            && matches!(
                list_method_kind(*func_id, self.ctx.sig),
                Some(ListMethodKind::ElementWriter | ListMethodKind::IndexWriter)
            )
    }

    /// Try to expand an expression-statement into multiple per-field statements.
    /// Returns `Some(stmts)` if the expression was a `push`/`index_assign` call on a
    /// decomposed candidate; `None` otherwise.
    fn try_expand_call_stmt(
        &self,
        engine: &mut Engine,
        expr: ExprId,
        span: Span,
    ) -> Option<Vec<StmtId>> {
        let ctx = self.ctx;
        let (receiver, func_id, arg_ids) = match &engine.body.exprs[expr].kind {
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } => (
                *receiver,
                *func_id,
                args.iter().map(|a| a.expr).collect::<Vec<_>>(),
            ),
            _ => return None,
        };
        let rec_local = receiver_local(engine.body, receiver)?;
        if !ctx.decomposed.contains(&rec_local) {
            return None;
        }
        let info = ctx.candidate_data.get(&rec_local)?;
        let arity = info.element_types.len();
        let layout = info.layout.clone();
        let all_scalar = info.all_scalar;

        let kind = list_method_kind(func_id, ctx.sig);
        match (kind, arg_ids.len()) {
            // Case 1: v.ElementWriter(source) — e.g. push
            (Some(ListMethodKind::ElementWriter), 1) => {
                let per_field = self.decompose_source(
                    engine,
                    arg_ids[0].as_expr()?,
                    arity,
                    &layout,
                    all_scalar,
                )?;
                let sig = sig_key_of_id(ctx.sig, func_id)?;
                let mut out = Vec::with_capacity(arity);
                for (k, elem_expr) in per_field.into_iter().enumerate() {
                    let field = ctx.field_map[&(rec_local, k as u32)].clone();
                    let call =
                        build_element_writer_call(engine, &field, elem_expr, &sig, span, ctx);
                    let st = engine.alloc_stmt(StmtKind::Expr(call.into()), span);
                    out.push(st);
                }
                Some(out)
            }
            // Case 2: v.IndexWriter(i, source) — e.g. index_assign
            (Some(ListMethodKind::IndexWriter), 2) => {
                let idx = arg_ids[0];
                let src = arg_ids[1];
                if !is_duplicable_operand(engine.body, idx) {
                    return None;
                }
                let per_field =
                    self.decompose_source(engine, src.as_expr()?, arity, &layout, all_scalar)?;
                let sig = sig_key_of_id(ctx.sig, func_id)?;
                let mut out = Vec::with_capacity(arity);
                for (k, elem_expr) in per_field.into_iter().enumerate() {
                    let field = ctx.field_map[&(rec_local, k as u32)].clone();
                    let idx_clone = clone_or_dup(engine, idx);
                    let call = build_index_writer_call(
                        engine, &field, idx_clone, elem_expr, &sig, span, ctx,
                    );
                    let st = engine.alloc_stmt(StmtKind::Expr(call.into()), span);
                    out.push(st);
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// Decompose a source expression into N per-field value operands.
    /// `src_all_scalar` mirrors the analysis-side gate: only then is a
    /// `$value_copy$T` wrapper seen through (the per-field scalar assignments
    /// realize the value copy).
    fn decompose_source(
        &self,
        engine: &mut Engine,
        expr: ExprId,
        expected_arity: usize,
        expected_layout: &ElementLayout,
        src_all_scalar: bool,
    ) -> Option<Vec<Operand>> {
        let ctx = self.ctx;
        // See through a defensive slot copy `$value_copy$T(src)` — matches
        // `check_source`. The wrapped element read is decomposed instead.
        if src_all_scalar
            && let Some(inner) = strip_one_value_copy(engine.body, expr, ctx.value_copy_ids)
        {
            return self.decompose_source(
                engine,
                inner,
                expected_arity,
                expected_layout,
                src_all_scalar,
            );
        }
        // Classify the source shape from a read-only inspection first.
        enum Source {
            Tuple(Vec<Operand>),
            Struct(Vec<(u32, Operand)>),
            IndexRead {
                other: u32,
                idx: Operand,
                sig: SigKey,
                span: Span,
            },
        }
        let source = match &engine.body.exprs[expr].kind {
            ExprKind::TupleLiteral { elements } => {
                if !matches!(expected_layout, ElementLayout::Tuple) {
                    return None;
                }
                if elements.len() != expected_arity {
                    return None;
                }
                Source::Tuple(elements.clone())
            }
            ExprKind::StructLiteral {
                struct_type,
                fields,
                ..
            } => {
                let ElementLayout::Struct {
                    type_id: expected_ty,
                } = expected_layout
                else {
                    return None;
                };
                if struct_type != expected_ty {
                    return None;
                }
                if fields.len() != expected_arity {
                    return None;
                }
                Source::Struct(fields.iter().map(|f| (f.field_index, f.value)).collect())
            }
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } if list_method_kind(*func_id, ctx.sig) == Some(ListMethodKind::IndexReader)
                && args.len() == 1 =>
            {
                let other = receiver_local(engine.body, *receiver)?;
                if !ctx.decomposed.contains(&other) {
                    return None;
                }
                let other_info = ctx.candidate_data.get(&other)?;
                if other_info.element_types.len() != expected_arity {
                    return None;
                }
                if !layouts_compatible(expected_layout, &other_info.layout) {
                    return None;
                }
                let idx_expr = args[0].expr;
                if !idx_expr
                    .as_expr()
                    .is_none_or(|e| is_duplicable_expr(engine.body, e))
                {
                    return None;
                }
                let sig = sig_key_of_id(ctx.sig, *func_id)?;
                Source::IndexRead {
                    other,
                    idx: idx_expr,
                    sig,
                    span: engine.body.exprs[expr].span,
                }
            }
            _ => return None,
        };

        match source {
            Source::Tuple(elements) => {
                // Each element becomes one per-field value, deep-cloned then
                // rewritten to propagate nested decomposed reads.
                let mut out = Vec::with_capacity(expected_arity);
                for el in elements {
                    let c = clone_or_dup(engine, el);
                    if let Some(e) = c.as_expr() {
                        self.rewrite_expr(engine, e);
                    }
                    out.push(c);
                }
                Some(out)
            }
            Source::Struct(fields) => {
                // Reorder by `field_index` so output position k corresponds to
                // field k. `check_source` verified indices cover 0..N exactly once.
                let mut out: Vec<Option<Operand>> = (0..expected_arity).map(|_| None).collect();
                for (field_index, value) in fields {
                    let k = field_index as usize;
                    if k >= expected_arity {
                        return None;
                    }
                    if out[k].is_some() {
                        return None;
                    }
                    let c = clone_or_dup(engine, value);
                    if let Some(e) = c.as_expr() {
                        self.rewrite_expr(engine, e);
                    }
                    out[k] = Some(c);
                }
                out.into_iter().collect::<Option<Vec<_>>>()
            }
            Source::IndexRead {
                other,
                idx,
                sig,
                span,
            } => {
                let mut out = Vec::with_capacity(expected_arity);
                for k in 0..expected_arity {
                    let field = ctx.field_map[&(other, k as u32)].clone();
                    let idx_clone = clone_or_dup(engine, idx);
                    let call = build_index_reader_call(engine, &field, idx_clone, &sig, span, ctx);
                    out.push(Operand::Expr(call));
                }
                Some(out)
            }
        }
    }

    /// Rewrite an expression in place: `v.len()`/`v.is_empty()` → field-0 call;
    /// `v.index_value(i).K` → `v_K.index_value(i)`; a writer call (`push` /
    /// `index_assign`) at an expression position → a unit-valued `Block` of the
    /// per-field statements. All other expressions recurse.
    fn rewrite_expr(&self, engine: &mut Engine, e: ExprId) {
        let ctx = self.ctx;

        // Writer call (`push` / `index_assign`) reaching an expression position —
        // e.g. a bare `v.push(x)` match-arm / if-branch body (`Operand::Expr`).
        // The whitelist accepts writers at any position; statement-level writers
        // are expanded flat by `process_stmt`, but a nested one must expand here
        // into a unit-valued `Block` of the per-field statements, or the removed
        // candidate binding is left dangling. A whitelisted writer that fails to
        // expand is analysis/rewrite drift — panic rather than miscompile.
        if self.is_decomposed_writer_call(engine, e) {
            let span = engine.body.exprs[e].span;
            let stmts = self
                .try_expand_call_stmt(engine, e, span)
                .expect("decomposed-candidate writer call must expand");
            let block = engine.alloc_block(stmts, span);
            engine.replace_expr_kind(e, ExprKind::Block(block));
            return;
        }

        // Handle FieldAccess on IndexValue first (read pattern).
        let field_read = if let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &engine.body.exprs[e].kind
        {
            let inner = *inner;
            let field_index = *field_index;
            if let Some(inner_e) = inner.as_expr()
                && let ExprKind::MethodCall {
                    receiver,
                    func_id,
                    args,
                    ..
                } = &engine.body.exprs[inner_e].kind
                && list_method_kind(*func_id, ctx.sig) == Some(ListMethodKind::IndexReader)
                && args.len() == 1
                && let Some(rec_local) = receiver_local(engine.body, *receiver)
                && ctx.decomposed.contains(&rec_local)
            {
                sig_key_of_id(ctx.sig, *func_id)
                    .map(|sig| (rec_local, field_index, args[0].expr, sig))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((rec_local, field_index, idx_arg, sig)) = field_read {
            let info = ctx
                .candidate_data
                .get(&rec_local)
                .expect("decomposed must have candidate data");
            let k = field_index as usize;
            if k < info.element_types.len() {
                let field = ctx.field_map[&(rec_local, k as u32)].clone();
                let idx_clone = clone_or_dup(engine, idx_arg);
                if let Some(ie) = idx_clone.as_expr() {
                    self.rewrite_expr(engine, ie);
                }
                let span = engine.body.exprs[e].span;
                let new_call = build_index_reader_call(engine, &field, idx_clone, &sig, span, ctx);
                // Promote `new_call`'s content into `e`, leaving `new_call`
                // a dead `Unit`. Equivalent to the old `body.exprs[e] = node;`
                // but registers the move in the engine's parent map and use
                // index.
                engine.become_expr(e, new_call);
                return;
            }
        }

        // Handle Query calls on decomposed candidates (len/is_empty/capacity).
        let query = if let ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } = &engine.body.exprs[e].kind
        {
            if let Some(rec_local) = receiver_local(engine.body, *receiver)
                && ctx.decomposed.contains(&rec_local)
                && args.is_empty()
                && list_method_kind(*func_id, ctx.sig) == Some(ListMethodKind::Query)
            {
                sig_key_of_id(ctx.sig, *func_id).map(|sig| (rec_local, sig))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((rec_local, sig)) = query {
            // Field 0 is representative: every writer keeps the lists in lockstep.
            let field = ctx.field_map[&(rec_local, 0)].clone();
            let (_, new_func_id) = ctx
                .catalog
                .get(&(field.elem_type, sig))
                .cloned()
                .expect("Query monomorphization must exist for decomposed element type");
            let span = engine.body.exprs[e].span;
            let new_receiver = build_receiver(engine, &field, false, span);
            engine.replace_expr_kind(
                e,
                ExprKind::MethodCall {
                    func_id: new_func_id,
                    receiver: new_receiver.into(),
                    type_args: Vec::new(),
                    args: Vec::new(),
                },
            );
            return;
        }

        // Default: recurse into children.
        self.walk_children(engine, NodeRef::Expr(e));
    }

    /// Default mutating walk: recurse into every id-bearing child, dispatching
    /// blocks back through the statement-restructuring `rewrite_block`.
    fn walk_children(&self, engine: &mut Engine, node: NodeRef) {
        let mut kids = Vec::new();
        engine.body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            match c {
                NodeRef::Block(b) => self.rewrite_block(engine, b),
                NodeRef::Expr(ex) => self.rewrite_expr(engine, ex),
                NodeRef::Stmt(_) | NodeRef::Pat(_) => self.walk_children(engine, c),
            }
        }
    }
}

/// Duplicate an operand for one more per-field use: a skeleton is deep-cloned,
/// a promoted constant is immutable and reused as is.
fn clone_or_dup(engine: &mut Engine, op: Operand) -> Operand {
    match op {
        Operand::Expr(e) => Operand::Expr(engine.clone_expr(e)),
        Operand::Value(_) => op,
    }
}

/// The `&v_field` / `&mut v_field` receiver of a per-field call.
fn build_receiver(engine: &mut Engine, field: &FieldList, mut_ref: bool, span: Span) -> ExprId {
    let local = engine.alloc_expr(
        ExprKind::Local {
            index: field.local_index,
            name: field.name.clone(),
        },
        field.list_type,
        span,
    );
    let op = if mut_ref {
        NirUnaryOp::MutRef
    } else {
        NirUnaryOp::Ref
    };
    engine.alloc_expr(
        ExprKind::Unary {
            op,
            expr: local.into(),
        },
        field.list_type,
        span,
    )
}

/// The `func_id` of `List<field.elem_type>`'s method with signature `sig`.
fn field_method(field: &FieldList, sig: &SigKey, ctx: &RewriteCtx) -> crate::nir::FuncId {
    ctx.catalog
        .get(&(field.elem_type, sig.clone()))
        .expect("method entry checked by required_methods_available")
        .1
}

/// Build a `List<T_k>::Constructor(cap)` NIR call — e.g. `with_capacity(cap)`.
fn build_with_capacity_call(
    engine: &mut Engine,
    field: &FieldList,
    cap: Operand,
    span: Span,
    ctx: &RewriteCtx,
) -> ExprId {
    let sig = find_sig_key_for_kind(ctx.sig, field.elem_type, ListMethodKind::Constructor)
        .expect("Constructor checked by required_methods_available");
    let func_id = field_method(field, &sig, ctx);
    engine.alloc_expr(
        ExprKind::Call {
            func_id,
            type_args: Vec::new(),
            args: vec![ArenaCallArg {
                expr: cap,
                is_mut: false,
            }],
        },
        field.list_type,
        span,
    )
}

/// Build `v_field.ElementWriter(value)` — e.g. `v_field.push(value)`.
fn build_element_writer_call(
    engine: &mut Engine,
    field: &FieldList,
    value: Operand,
    sig: &SigKey,
    span: Span,
    ctx: &RewriteCtx,
) -> ExprId {
    let func_id = field_method(field, sig, ctx);
    let receiver = build_receiver(engine, field, true, span);
    engine.alloc_expr(
        ExprKind::MethodCall {
            func_id,
            receiver: receiver.into(),
            type_args: Vec::new(),
            args: vec![ArenaCallArg {
                expr: value,
                is_mut: false,
            }],
        },
        TypeTable::UNIT,
        span,
    )
}

/// Build `v_field.IndexWriter(index, value)` — e.g. `index_assign(index, value)`.
fn build_index_writer_call(
    engine: &mut Engine,
    field: &FieldList,
    index: Operand,
    value: Operand,
    sig: &SigKey,
    span: Span,
    ctx: &RewriteCtx,
) -> ExprId {
    let func_id = field_method(field, sig, ctx);
    let receiver = build_receiver(engine, field, true, span);
    engine.alloc_expr(
        ExprKind::MethodCall {
            func_id,
            receiver: receiver.into(),
            type_args: Vec::new(),
            args: vec![
                ArenaCallArg {
                    expr: index,
                    is_mut: false,
                },
                ArenaCallArg {
                    expr: value,
                    is_mut: false,
                },
            ],
        },
        TypeTable::UNIT,
        span,
    )
}

/// Build `v_field.IndexReader(index)` — e.g. `index_value(index)`.
fn build_index_reader_call(
    engine: &mut Engine,
    field: &FieldList,
    index: Operand,
    sig: &SigKey,
    span: Span,
    ctx: &RewriteCtx,
) -> ExprId {
    let func_id = field_method(field, sig, ctx);
    let receiver = build_receiver(engine, field, false, span);
    engine.alloc_expr(
        ExprKind::MethodCall {
            func_id,
            receiver: receiver.into(),
            type_args: Vec::new(),
            args: vec![ArenaCallArg {
                expr: index,
                is_mut: false,
            }],
        },
        field.elem_type,
        span,
    )
}

/// Returns true if the expression can be safely duplicated (cloned and
/// re-evaluated N times with no observable side effects).
fn is_duplicable_expr(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } | ExprKind::GlobalVarGet { .. } => true,
        ExprKind::Binary { left, right, .. } => {
            is_duplicable_operand(body, *left) && is_duplicable_operand(body, *right)
        }
        ExprKind::Unary { op, expr: inner } => {
            !matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && is_duplicable_operand(body, *inner)
        }
        ExprKind::Cast { expr: inner, .. } => is_duplicable_operand(body, *inner),
        // FieldAccess is only duplicable if its inner is too (most commonly a Local).
        // We deliberately exclude MethodCall / Call / Index / etc. because they
        // may allocate, trap, or have side effects.
        ExprKind::FieldAccess { expr: inner, .. } => is_duplicable_operand(body, *inner),
        _ => false,
    }
}

/// Operand form of [`is_duplicable_expr`]: a promoted constant (`Operand::Value`)
/// is always duplicable.
fn is_duplicable_operand(body: &Body, op: Operand) -> bool {
    op.as_expr().is_none_or(|e| is_duplicable_expr(body, e))
}
