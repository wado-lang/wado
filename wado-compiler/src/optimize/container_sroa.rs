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

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{
    CallArg, FunctionRef, NirBlock, NirExpr, NirExprKind, NirFunction, NirLocal, NirStmt,
    NirStmtKind, NirStruct,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirMutVisitor, NirRefVisitor};
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

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

/// Classify an List method's signature into an `ListMethodKind`, if it matches
/// any of the recognized shapes. Returns the element type `T` on success so
/// callers can sanity-check consistency with the call-site receiver type.
fn classify_array_method_sig(func: &NirFunction, type_table: &TypeTable) -> Option<ListMethodKind> {
    // Must be a method (instance or static) on `List`.
    let info = func.method_info.as_ref()?;
    if info.base_struct_name != "List" {
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

/// Extract the `SigKey` (trait name, method name) from a `FunctionRef` that
/// refers to an List method. Returns `None` for non-method functions or methods
/// on other types.
fn sig_key_of(func: &FunctionRef) -> Option<SigKey> {
    let info = func.method_info.as_ref()?;
    if info.base_struct_name != "List" {
        return None;
    }
    Some((info.trait_name.clone(), info.method_name.clone()))
}

/// Lookup: method family `SigKey` → `ListMethodKind`. Built once from the
/// function table and used at every call site to classify the operation.
/// Since classification depends only on signature *shape* (not element type),
/// one entry per family suffices — `List<i32>::push` and `List<i64>::push`
/// share `((None, "push"), ElementWriter)`.
type SigKindIndex = IndexMap<SigKey, ListMethodKind>;

/// Lookup table: (element type `T_k`, (trait, method)) → `FunctionRef` for
/// `List<T_k>::method`. Built once per pass.
type MethodCatalog = IndexMap<(TypeId, SigKey), FunctionRef>;

/// A local that is a candidate for container SROA.
struct Candidate {
    /// Original local index
    local_index: u32,
    /// Original local name (for generating new local names)
    local_name: String,
    /// Whether the original let was `mut`
    is_mut: bool,
    /// Element types of the container (for tuples: the tuple element types;
    /// for structs: the struct field types in declaration order).
    element_types: Vec<TypeId>,
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
/// capacity argument qualifies. The capacity expression is cloned once per
/// decomposed field at rewrite time, so it must be side-effect-free.
struct CandidateInit {
    /// Capacity expression to pass to each per-field `with_capacity(...)` call.
    capacity: NirExpr,
}

/// Apply container SROA to all functions in the project.
pub fn scalarize_containers(project: &mut NirPackage) -> bool {
    // Build the method catalog + signature-kind index once, using an immutable
    // borrow on functions. Both indexes are derived from the same scan.
    let (catalog, sig_kinds) = {
        let type_table = project.type_table.borrow();
        build_method_catalog(project, &type_table)
    };
    if catalog.is_empty() {
        return false;
    }

    // Build a struct lookup: (name, module_source) → &NirStruct. Used by
    // `collect_candidates` to expand `List<UserStruct>` element types.
    let struct_index = build_struct_index(&project.structs);

    let mut changed = false;
    for func_rc in &project.functions {
        // Skip CM bindings (they are ABI bridges, do not optimize their bodies)
        {
            let func = func_rc.borrow();
            if func.is_cm_binding {
                continue;
            }
            if func.body.is_none() {
                continue;
            }
        }
        // Scoped mutable borrow for analysis + rewrite.
        let type_table_rc = project.type_table.clone();
        let mut func = func_rc.borrow_mut();
        let func_changed = scalarize_in_function(
            &mut func,
            &type_table_rc,
            &catalog,
            &sig_kinds,
            &struct_index,
        );
        changed |= func_changed;
    }
    changed
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
) -> (MethodCatalog, SigKindIndex) {
    let mut catalog = MethodCatalog::default();
    let mut sig_kinds = SigKindIndex::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        // Must be an instance/static method with `method_info`.
        let Some(method_info) = &func.method_info else {
            continue;
        };
        // Must be a method on List (by base struct name). The kind is still
        // List-specific because the pass itself is List-specific — we only
        // de-hardcode method *names*, not the container type.
        if method_info.base_struct_name != "List" {
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
        // First-writer wins (there should only be one per (T, sig)).
        catalog
            .entry((element_ty, sig_key.clone()))
            .or_insert(func_ref);

        // Classify this method by signature shape. A method family (same
        // `SigKey`) has the same shape across all element types, so first-
        // writer-wins is fine here too.
        if !sig_kinds.contains_key(&sig_key)
            && let Some(kind) = classify_array_method_sig(&func, type_table)
        {
            sig_kinds.insert(sig_key, kind);
        }
    }
    (catalog, sig_kinds)
}

/// Per-function driver: detect candidates, analyze uses, allocate parallel locals, rewrite.
fn scalarize_in_function(
    func: &mut NirFunction,
    type_table_rc: &std::rc::Rc<std::cell::RefCell<TypeTable>>,
    catalog: &MethodCatalog,
    sig_kinds: &SigKindIndex,
    struct_index: &StructIndex<'_>,
) -> bool {
    // Step 1: collect candidates. Immutable borrow of type_table.
    let candidates = {
        let type_table = type_table_rc.borrow();
        let body = &func.body_block().expect("checked in caller");
        collect_candidates(body, &type_table, struct_index, sig_kinds)
    };
    if candidates.is_empty() {
        return false;
    }

    // Step 2: escape analysis. Build safe-set via whitelist + tuple-source fixpoint.
    // Also track which `ListMethodKind`s were observed on each whitelisted use,
    // so step 3 can demand only the monomorphizations that will actually be
    // emitted per field (rather than unconditionally requiring all four kinds).
    let body_ref = &func.body_block().expect("checked in caller");
    let (safe_indices, used_kinds_map) = compute_safe_set(body_ref, &candidates, sig_kinds);
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
            required_methods_available(c, used, catalog, sig_kinds)
        })
        .collect();
    if safe_candidates.is_empty() {
        return false;
    }

    // Step 4: allocate parallel `List<T_k>` locals. Mutable borrow of type_table.
    // Map: (original local, field k) → new local index
    let mut field_local_map: IndexMap<(u32, u32), u32> = IndexMap::default();
    // Map: (original local, field k) → (new name, array type id)
    let mut field_info_map: IndexMap<(u32, u32), (String, TypeId)> = IndexMap::default();
    // The set of original locals we decided to decompose (final).
    let mut decomposed: IndexSet<u32> = IndexSet::default();
    {
        let mut type_table = type_table_rc.borrow_mut();
        for c in &safe_candidates {
            let base = func.local_count;
            for (k, &elem_ty) in c.element_types.iter().enumerate() {
                let arr_ty = type_table.make_list(elem_ty);
                let new_index = base + k as u32;
                let new_name = format!("__csroa_{}_{}", c.local_name, k);
                field_local_map.insert((c.local_index, k as u32), new_index);
                field_info_map.insert((c.local_index, k as u32), (new_name.clone(), arr_ty));
                func.locals.push(NirLocal {
                    name: new_name,
                    type_id: arr_ty,
                    is_mut: false,
                });
            }
            func.local_count += c.element_types.len() as u32;
            decomposed.insert(c.local_index);
        }
    }

    // Build a lookup from local_index → candidate data needed during rewrite.
    let candidate_data: IndexMap<u32, CandidateRewriteInfo> = safe_candidates
        .iter()
        .map(|c| {
            (
                c.local_index,
                CandidateRewriteInfo {
                    element_types: c.element_types.clone(),
                    layout: c.layout.clone(),
                    is_mut: c.is_mut,
                    span: c.span,
                    init: CandidateInit {
                        capacity: c.init.capacity.clone(),
                    },
                },
            )
        })
        .collect();

    // Step 5: rewrite the body.
    let mut owned = func.body_block().expect("checked in caller");
    let body = &mut owned;
    let ctx = RewriteCtx {
        decomposed: &decomposed,
        field_local_map: &field_local_map,
        field_info_map: &field_info_map,
        candidate_data: &candidate_data,
        catalog,
        sig_kinds,
    };
    Rewriter { ctx: &ctx }.visit_block(body);
    func.set_body_block(owned);

    true
}

/// Data carried from analysis into rewrite for each decomposed candidate.
struct CandidateRewriteInfo {
    element_types: Vec<TypeId>,
    layout: ElementLayout,
    is_mut: bool,
    span: Span,
    init: CandidateInit,
}

struct RewriteCtx<'a> {
    decomposed: &'a IndexSet<u32>,
    field_local_map: &'a IndexMap<(u32, u32), u32>,
    field_info_map: &'a IndexMap<(u32, u32), (String, TypeId)>,
    candidate_data: &'a IndexMap<u32, CandidateRewriteInfo>,
    catalog: &'a MethodCatalog,
    sig_kinds: &'a SigKindIndex,
}

/// Returns true if every `ListMethodKind` the rewrite might use is available
/// for every element type `T_k` of the candidate.
///
/// The rewrite always needs:
/// - `Constructor` (for both `Empty` and `WithCapacity` init forms — both are
///   emitted as `List<T_k>::with_capacity(n)`)
/// - `ElementWriter` (push — any original `push` is expanded to per-field pushes)
/// - `IndexReader` (`index_value` — any read `v[i].K` or cross-candidate source)
/// - `IndexWriter` (`index_assign` — any slot copy `v[i] = src`)
///
/// `Query` (len / `is_empty` / capacity) is rewritten by dispatching to field 0,
/// so we don't require it to be present for every `T_k` — only for field 0.
/// In practice all `T_k` get the same stdlib methods monomorphized together,
/// so checking field 0 alone is enough.
///
/// Check that every `ListMethodKind` the rewrite will need for this candidate
/// has a corresponding monomorphization in the catalog for every per-field
/// element type.
///
/// `Constructor` is always required (the initializer itself uses it). The other
/// kinds are only required when escape analysis observed at least one use of
/// that kind on the candidate — the rewrite only needs to emit per-field
/// versions of methods that are actually called.
fn required_methods_available(
    c: &Candidate,
    used_kinds: &IndexSet<ListMethodKind>,
    catalog: &MethodCatalog,
    sig_kinds: &SigKindIndex,
) -> bool {
    let mut required: IndexSet<ListMethodKind> = IndexSet::default();
    required.insert(ListMethodKind::Constructor);
    for &k in used_kinds {
        required.insert(k);
    }
    for &t in &c.element_types {
        for &kind in &required {
            if find_sig_key_for_kind(catalog, sig_kinds, t, kind).is_none() {
                return false;
            }
        }
    }
    true
}

/// Locate the `SigKey` of a monomorphized `List<elem_ty>` method whose signature
/// classifies as `kind`. Returns the first match, or `None` if no such method is
/// monomorphized in this project.
///
/// The catalog is keyed by `(TypeId, SigKey)` and the `sig_kinds` index maps
/// `SigKey → ListMethodKind`. We combine them to answer queries of the form
/// "which stdlib List method of kind K is available for element type T?".
fn find_sig_key_for_kind(
    catalog: &MethodCatalog,
    sig_kinds: &SigKindIndex,
    elem_ty: TypeId,
    kind: ListMethodKind,
) -> Option<SigKey> {
    for ((t, sig), _) in catalog {
        if *t == elem_ty && sig_kinds.get(sig).copied() == Some(kind) {
            return Some(sig.clone());
        }
    }
    None
}

/// Collect candidate `let` bindings in the function body. Only looks at top-level
/// statements (not nested blocks/loops) because P0a restricts scope to simple
/// function-body locals — enough for the zlib insertion-sort pattern.
fn collect_candidates(
    body: &NirBlock,
    type_table: &TypeTable,
    struct_index: &StructIndex<'_>,
    sig_kinds: &SigKindIndex,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for stmt in &body.stmts {
        let NirStmtKind::Let {
            name,
            local_index,
            is_mut,
            type_id,
            value,
            ..
        } = &stmt.kind
        else {
            continue;
        };
        // Type must be List<Tuple<...>> or List<UserStruct>.
        let Some(elem_ty) = type_table.as_list(*type_id) else {
            continue;
        };
        let Some((layout, element_types)) = element_layout_of(elem_ty, type_table, struct_index)
        else {
            continue;
        };
        if element_types.is_empty() {
            continue;
        }
        // Initializer must be one of the recognized forms.
        let Some(init) = recognize_init(value, sig_kinds) else {
            continue;
        };
        out.push(Candidate {
            local_index: *local_index,
            local_name: name.clone(),
            is_mut: *is_mut,
            element_types,
            layout,
            span: stmt.span,
            init,
        });
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
        name,
        module_source,
        ..
    } = type_table.get(elem_ty)
    {
        let tir_struct = struct_index.get(&(name.clone(), module_source.clone()))?;
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
fn recognize_init(value: &NirExpr, sig_kinds: &SigKindIndex) -> Option<CandidateInit> {
    let inner = unwrap_builder_labeled_block(value).unwrap_or(value);
    let NirExprKind::Call { func, args, .. } = &inner.kind else {
        return None;
    };
    if list_method_kind(func, sig_kinds) != Some(ListMethodKind::Constructor) {
        return None;
    }
    if args.len() != 1 {
        return None;
    }
    // The capacity expression is cloned once per per-field constructor
    // call during rewrite, so it must be side-effect-free.
    if !is_duplicable_expr(&args[0].expr) {
        return None;
    }
    Some(CandidateInit {
        capacity: args[0].expr.clone(),
    })
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
fn unwrap_builder_labeled_block(expr: &NirExpr) -> Option<&NirExpr> {
    let NirExprKind::LabeledBlock { label, block, .. } = &expr.kind else {
        return None;
    };
    if block.stmts.len() != 2 {
        return None;
    }
    let NirStmtKind::Let {
        local_index: b_local,
        value: inner,
        ..
    } = &block.stmts[0].kind
    else {
        return None;
    };
    let NirStmtKind::Break {
        label: brk_label,
        value: Some(brk_val),
    } = &block.stmts[1].kind
    else {
        return None;
    };
    if brk_label.as_deref() != Some(label.as_str()) {
        return None;
    }
    // Break value must be a zero-argument method call whose receiver is `__b`
    // (possibly wrapped in `&`/`&mut`). We don't care what the method does
    // semantically — the `SequenceLiteralBuilder::build()` contract is
    // enforced by the trait, and any user-level code matching this shape is
    // vanishingly unlikely to mean anything other than a builder commit.
    let NirExprKind::MethodCall { receiver, args, .. } = &brk_val.kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let receiver_local = match &receiver.kind {
        NirExprKind::Local { index, .. } => *index,
        NirExprKind::Unary {
            op: crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef,
            expr: inner_ref,
        } => {
            let NirExprKind::Local { index, .. } = &inner_ref.kind else {
                return None;
            };
            *index
        }
        _ => return None,
    };
    if receiver_local != *b_local {
        return None;
    }
    Some(inner)
}

/// Look up the `ListMethodKind` of a call target by signature, via the
/// pre-built `SigKindIndex`. Returns `None` for non-method functions, non-List
/// methods, or List methods whose signature didn't match any kind.
fn list_method_kind(func: &FunctionRef, sig_kinds: &SigKindIndex) -> Option<ListMethodKind> {
    let sig = sig_key_of(func)?;
    sig_kinds.get(&sig).copied()
}

/// Compute the set of safe (decomposable) candidate locals via whitelist escape
/// analysis plus a fixpoint for element-source dependency:
///
/// - A candidate `v` is whitelisted if every use is one of:
///   * `v.ElementWriter(source)` — where `source` is element-decomposable
///     (i.e., `push` or a push-shaped method family)
///   * `v.IndexWriter(i, source)` (slot assign)
///   * `v.Query()` (length-invariant query like `len`/`is_empty`/`capacity`)
///   * `v.IndexReader(i).K` — field access on an index-read result (K constant)
/// - "element source" is either a direct `TupleLiteral`/`StructLiteral` of N
///   elements, or `other.IndexReader(j)` where `other` is also safe (fixpoint).
///
/// Anything else — bare `Local { v }`, `&v`, `&mut v`, an unclassified method
/// call, a full-value `index_value(i)` without `FieldAccess` — marks `v` as
/// escaped. Classification is signature-driven via `sig_kinds`.
fn compute_safe_set(
    body: &NirBlock,
    candidates: &[Candidate],
    sig_kinds: &SigKindIndex,
) -> (IndexSet<u32>, IndexMap<u32, IndexSet<ListMethodKind>>) {
    // Map candidate local → element arity (for push/index_assign/index_value arity checks).
    let mut arity_of: IndexMap<u32, usize> = IndexMap::default();
    // Map candidate local → element layout (for verifying literal shape matches).
    let mut layout_of: IndexMap<u32, ElementLayout> = IndexMap::default();
    for c in candidates {
        arity_of.insert(c.local_index, c.element_types.len());
        layout_of.insert(c.local_index, c.layout.clone());
    }

    // Iterate to fixpoint: start with all candidates safe, then remove any that
    // reference an escaped candidate via element-source push. The used-kinds map
    // is rebuilt from scratch each iteration so it reflects the final safe set.
    let mut safe: IndexSet<u32> = candidates.iter().map(|c| c.local_index).collect();
    let used_kinds = loop {
        let mut checker = WhitelistChecker {
            safe: &safe,
            arity_of: &arity_of,
            layout_of: &layout_of,
            sig_kinds,
            escaped: IndexSet::default(),
            used_kinds: IndexMap::default(),
        };
        checker.visit_block(body);
        if checker.escaped.is_empty() {
            break checker.used_kinds;
        }
        for idx in checker.escaped {
            safe.shift_remove(&idx);
        }
    };
    (safe, used_kinds)
}

struct WhitelistChecker<'a> {
    safe: &'a IndexSet<u32>,
    arity_of: &'a IndexMap<u32, usize>,
    layout_of: &'a IndexMap<u32, ElementLayout>,
    sig_kinds: &'a SigKindIndex,
    escaped: IndexSet<u32>,
    /// Per-candidate set of `ListMethodKind`s observed on whitelisted uses.
    /// Only kinds the rewrite actually needs to emit are recorded; bare
    /// `IndexReader` uses (which escape) are not. Drives the
    /// `required_methods_available` check so we only demand stdlib
    /// monomorphizations that will actually be called.
    used_kinds: IndexMap<u32, IndexSet<ListMethodKind>>,
}

impl WhitelistChecker<'_> {
    fn mark(&mut self, idx: u32) {
        if self.safe.contains(&idx) {
            self.escaped.insert(idx);
        }
    }

    /// Record that a whitelisted call of `kind` was observed on candidate `idx`.
    fn record_use(&mut self, idx: u32, kind: ListMethodKind) {
        self.used_kinds.entry(idx).or_default().insert(kind);
    }

    /// Check an expression used as a value-source for `push`/`index_assign`.
    /// Returns true if the expression is a decomposable element source matching
    /// `expected_layout` and `expected_arity`. If the source references another
    /// (escaped) candidate as a non-decomposable use, that candidate is marked
    /// escaped.
    fn check_source(
        &mut self,
        expr: &NirExpr,
        expected_arity: usize,
        expected_layout: &ElementLayout,
    ) -> bool {
        match &expr.kind {
            // Direct tuple literal `[e0, e1, ...]` (heap or multi-value form).
            NirExprKind::TupleLiteral { elements } => {
                if !matches!(expected_layout, ElementLayout::Tuple) {
                    return false;
                }
                if elements.len() != expected_arity {
                    return false;
                }
                // Recurse into each element with a "normal" (non-tuple-source) visit.
                for e in elements {
                    self.visit_expr(e);
                }
                true
            }
            // Direct struct literal: StructName { field_0: v0, field_1: v1, ... }
            NirExprKind::StructLiteral {
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
                for f in fields {
                    self.visit_expr(&f.value);
                }
                true
            }
            // Element from another candidate: other.IndexReader(j)
            NirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } if list_method_kind(func, self.sig_kinds) == Some(ListMethodKind::IndexReader) => {
                if args.len() != 1 {
                    return false;
                }
                let Some(other) = receiver_local(receiver) else {
                    // Receiver isn't a bare local — recurse normally.
                    self.visit_expr(expr);
                    return false;
                };
                // The receiver must be one of our candidates with matching arity.
                if !self.safe.contains(&other) {
                    return false;
                }
                if self.arity_of.get(&other).copied() != Some(expected_arity) {
                    return false;
                }
                // Layouts must match: tuple ↔ tuple, and struct ↔ same struct.
                // Cross-layout pushes are impossible under a sound type system,
                // but we check defensively.
                let Some(other_layout) = self.layout_of.get(&other) else {
                    return false;
                };
                if !layouts_compatible(expected_layout, other_layout) {
                    return false;
                }
                // The rewrite clones the index expression N times (once per field).
                // If the index is not duplicable, `decompose_source` bails —
                // so we must also bail here, otherwise escape analysis would
                // whitelist this use and the rewrite would leave a dangling
                // `v.push(w[i])` referencing decomposed locals.
                if !is_duplicable_expr(&args[0].expr) {
                    // Fall through to a normal visit so `other` gets marked
                    // escaped via the bare `index_value` branch in `visit_expr`.
                    self.visit_expr(expr);
                    return false;
                }
                // Index expression must be visited as a normal expression.
                self.visit_expr(&args[0].expr);
                // Record that `other` is being read via IndexReader so it
                // needs that method monomorphization during rewrite.
                self.record_use(other, ListMethodKind::IndexReader);
                true
            }
            _ => false,
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

impl NirRefVisitor for WhitelistChecker<'_> {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            // v.method(...) — inspect receiver for whitelisted patterns.
            NirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                if let Some(rec_local) = receiver_local(receiver)
                    && self.safe.contains(&rec_local)
                {
                    let kind = list_method_kind(func, self.sig_kinds);
                    match (kind, args.len()) {
                        // v.push-shaped(source)
                        (Some(ListMethodKind::ElementWriter), 1) => {
                            let arity = self.arity_of.get(&rec_local).copied().unwrap_or(0);
                            let layout = self
                                .layout_of
                                .get(&rec_local)
                                .cloned()
                                .unwrap_or(ElementLayout::Tuple);
                            if self.check_source(&args[0].expr, arity, &layout) {
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
                            let arity = self.arity_of.get(&rec_local).copied().unwrap_or(0);
                            let layout = self
                                .layout_of
                                .get(&rec_local)
                                .cloned()
                                .unwrap_or(ElementLayout::Tuple);
                            // The rewrite clones the destination index N times
                            // (once per field). Non-duplicable indices force a
                            // bail in `try_expand_call_stmt`, so we must also
                            // bail here — otherwise the candidate would be
                            // decomposed but the original `v.index_assign(i,
                            // ...)` statement would remain, referencing the
                            // removed local.
                            if !is_duplicable_expr(&args[0].expr) {
                                self.mark(rec_local);
                                self.visit_expr(&args[0].expr);
                                self.visit_expr(&args[1].expr);
                                return;
                            }
                            // index argument visited normally
                            self.visit_expr(&args[0].expr);
                            if self.check_source(&args[1].expr, arity, &layout) {
                                self.record_use(rec_local, ListMethodKind::IndexWriter);
                            } else {
                                self.mark(rec_local);
                            }
                            return;
                        }
                        // Bare v.index_value(i) — safe only when the *enclosing*
                        // expression is a `FieldAccess` with a constant field
                        // index. That case is short-circuited by the
                        // `FieldAccess` branch below; if we arrive here directly
                        // the entire struct value is escaping.
                        (Some(ListMethodKind::IndexReader), 1) => {
                            self.mark(rec_local);
                            self.visit_expr(&args[0].expr);
                            return;
                        }
                        // Any other method call on a candidate → escape.
                        _ => {
                            self.mark(rec_local);
                        }
                    }
                }
                // Fall through: recurse into receiver and args normally.
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
            }
            // v.IndexReader(i).K — safe read pattern
            NirExprKind::FieldAccess { expr: inner, .. } => {
                if let NirExprKind::MethodCall {
                    receiver,
                    func,
                    args,
                    ..
                } = &inner.kind
                    && list_method_kind(func, self.sig_kinds) == Some(ListMethodKind::IndexReader)
                    && args.len() == 1
                    && let Some(rec_local) = receiver_local(receiver)
                    && self.safe.contains(&rec_local)
                {
                    // Safe — just visit the index expression.
                    self.record_use(rec_local, ListMethodKind::IndexReader);
                    self.visit_expr(&args[0].expr);
                    return;
                }
                self.visit_expr(inner);
            }
            // Bare Local reference to a candidate → escape.
            NirExprKind::Local { index, .. } => {
                self.mark(*index);
            }
            // Address taken on a candidate → escape.
            NirExprKind::Unary { op, expr: inner } => {
                if matches!(
                    op,
                    crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef
                ) && let NirExprKind::Local { index, .. } = &inner.kind
                {
                    self.mark(*index);
                    return;
                }
                self.visit_expr(inner);
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// If the expression is `Local { index }` — or `Unary::{Ref,MutRef}` wrapping
/// a Local — return the index. Method call receivers in lowered NIR are
/// typically `Unary::MutRef(Local)` for `&mut self` methods and
/// `Unary::Ref(Local)` for `&self` methods. We treat both as receivers of
/// the underlying local; when they appear elsewhere (as a function argument,
/// for example), the `Unary` branch in `visit_expr` marks the local escaped.
fn receiver_local(expr: &NirExpr) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::Unary {
            op: crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let NirExprKind::Local { index, .. } = &inner.kind {
                Some(*index)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Drives the in-place rewrite of decomposed container candidates over a
/// function body. `visit_block` performs the statement-level expansion
/// (candidate `Let` → N per-field `Let`s, and `push` / `index_assign`
/// expression-statements → N per-field statements); `visit_expr` rewrites the
/// remaining whitelisted reads (`len` / `is_empty`, `index_value(i).K`).
struct Rewriter<'a, 'c> {
    ctx: &'c RewriteCtx<'a>,
}

impl Rewriter<'_, '_> {
    /// Route a statement: either emit its per-field expansion or recurse + push as-is.
    fn process_stmt(&mut self, mut stmt: NirStmt, out: &mut Vec<NirStmt>) {
        let ctx = self.ctx;
        // Candidate Let: expand in place.
        if let NirStmtKind::Let { local_index, .. } = &stmt.kind
            && ctx.decomposed.contains(local_index)
        {
            expand_candidate_let(&stmt, out, ctx);
            return;
        }

        // Candidate push/index_assign as an ExprStmt at the statement level.
        if let NirStmtKind::Expr(expr) = &stmt.kind
            && let Some(expanded) = self.try_expand_call_stmt(expr, stmt.span)
        {
            out.extend(expanded);
            return;
        }

        // Otherwise, recurse into the statement (rewriting any nested
        // expressions/blocks) and push it unchanged.
        self.visit_stmt(&mut stmt);
        out.push(stmt);
    }
}

/// Emit N per-field Let statements for a decomposed candidate.
/// The original Let's initializer is replaced by N `List<T_k>::with_capacity(cap)`
/// calls, one per field. The same (duplicable) capacity expression from the
/// original initializer is cloned once per field.
fn expand_candidate_let(stmt: &NirStmt, out: &mut Vec<NirStmt>, ctx: &RewriteCtx) {
    let NirStmtKind::Let { local_index, .. } = &stmt.kind else {
        return;
    };
    let info = ctx
        .candidate_data
        .get(local_index)
        .expect("candidate data must exist for decomposed local");
    for (k, &elem_ty) in info.element_types.iter().enumerate() {
        let new_local_index = ctx.field_local_map[&(*local_index, k as u32)];
        let (new_name, arr_ty) = ctx.field_info_map[&(*local_index, k as u32)].clone();
        let init =
            build_with_capacity_call(elem_ty, arr_ty, info.init.capacity.clone(), info.span, ctx);
        let let_stmt = NirStmt::new(
            NirStmtKind::Let {
                name: new_name,
                local_index: new_local_index,
                is_mut: info.is_mut,
                is_reactive: false,
                type_id: arr_ty,
                value: init,
                skip_value_copy: false,
            },
            info.span,
        );
        out.push(let_stmt);
    }
}

/// Build a `List<T_k>::Constructor(cap)` NIR call — e.g. `with_capacity(cap)`.
/// The specific method is resolved via `find_sig_key_for_kind(Constructor)`,
/// so there's no hardcoded name match. `cap` is the (duplicable) capacity
/// expression recognized from the original initializer, already cloned once
/// for this field.
fn build_with_capacity_call(
    elem_ty: TypeId,
    arr_ty: TypeId,
    cap: NirExpr,
    span: Span,
    ctx: &RewriteCtx,
) -> NirExpr {
    let sig = find_sig_key_for_kind(
        ctx.catalog,
        ctx.sig_kinds,
        elem_ty,
        ListMethodKind::Constructor,
    )
    .expect("Constructor checked by required_methods_available");
    let func = ctx
        .catalog
        .get(&(elem_ty, sig))
        .expect("Constructor entry checked by required_methods_available")
        .clone();
    let call = NirExprKind::Call {
        func,
        type_args: Vec::new(),
        args: vec![CallArg::new(cap, false)],
    };
    NirExpr::new(call, arr_ty, span)
}

impl Rewriter<'_, '_> {
    /// Try to expand an expression-statement into multiple per-field statements.
    /// Returns `Some(stmts)` if the expression was a `push`/`index_assign` call on a
    /// decomposed candidate; `None` otherwise.
    fn try_expand_call_stmt(&mut self, expr: &NirExpr, span: Span) -> Option<Vec<NirStmt>> {
        let ctx = self.ctx;
        let NirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        let rec_local = receiver_local(receiver)?;
        if !ctx.decomposed.contains(&rec_local) {
            return None;
        }
        let info = ctx.candidate_data.get(&rec_local)?;
        let arity = info.element_types.len();
        let layout = info.layout.clone();

        let kind = list_method_kind(func, ctx.sig_kinds);
        match (kind, args.len()) {
            // Case 1: v.ElementWriter(source) — e.g. push
            (Some(ListMethodKind::ElementWriter), 1) => {
                let src = &args[0].expr;
                let per_field = self.decompose_source(src, arity, &layout)?;
                let sig = sig_key_of(func)?;
                let mut out = Vec::with_capacity(arity);
                for (k, elem_expr) in per_field.into_iter().enumerate() {
                    let field_local = ctx.field_local_map[&(rec_local, k as u32)];
                    let (field_name, arr_ty) = ctx.field_info_map[&(rec_local, k as u32)].clone();
                    let elem_ty = info.element_types[k];
                    let call = build_element_writer_call(
                        elem_ty,
                        arr_ty,
                        field_local,
                        field_name,
                        elem_expr,
                        &sig,
                        span,
                        ctx,
                    );
                    out.push(NirStmt::new(NirStmtKind::Expr(call), span));
                }
                Some(out)
            }
            // Case 2: v.IndexWriter(i, source) — e.g. index_assign
            (Some(ListMethodKind::IndexWriter), 2) => {
                let idx = &args[0].expr;
                let src = &args[1].expr;
                if !is_duplicable_expr(idx) {
                    return None;
                }
                let per_field = self.decompose_source(src, arity, &layout)?;
                let sig = sig_key_of(func)?;
                let mut out = Vec::with_capacity(arity);
                for (k, elem_expr) in per_field.into_iter().enumerate() {
                    let field_local = ctx.field_local_map[&(rec_local, k as u32)];
                    let (field_name, arr_ty) = ctx.field_info_map[&(rec_local, k as u32)].clone();
                    let elem_ty = info.element_types[k];
                    let call = build_index_writer_call(
                        elem_ty,
                        arr_ty,
                        field_local,
                        field_name,
                        idx.clone(),
                        elem_expr,
                        &sig,
                        span,
                        ctx,
                    );
                    out.push(NirStmt::new(NirStmtKind::Expr(call), span));
                }
                Some(out)
            }
            _ => None,
        }
    }
}

impl Rewriter<'_, '_> {
    /// Decompose a source expression into N per-field expressions.
    /// Supports three forms:
    /// - `TupleLiteral([e_0, e_1, ...])` (for tuple layout)
    /// - `StructLiteral { .. }` (for struct layout; fields reordered by `field_index`)
    /// - `other.index_value(j)` where `other` is decomposed → `[other_k.index_value(j)]` for each k
    ///
    /// Returns `None` if the expression doesn't match a decomposable form or if
    /// preconditions fail (wrong arity, non-duplicable index, layout mismatch, etc.).
    fn decompose_source(
        &mut self,
        expr: &NirExpr,
        expected_arity: usize,
        expected_layout: &ElementLayout,
    ) -> Option<Vec<NirExpr>> {
        let ctx = self.ctx;
        match &expr.kind {
            NirExprKind::TupleLiteral { elements } => {
                if !matches!(expected_layout, ElementLayout::Tuple) {
                    return None;
                }
                if elements.len() != expected_arity {
                    return None;
                }
                // Each element becomes one per-field value, rewritten to propagate
                // nested decomposed reads (e.g., FieldAccess on another candidate's index_value).
                let mut out = Vec::with_capacity(expected_arity);
                for e in elements {
                    let mut e = e.clone();
                    self.visit_expr(&mut e);
                    out.push(e);
                }
                Some(out)
            }
            NirExprKind::StructLiteral {
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
                // Reorder by `field_index` so output position k corresponds to
                // field k (matching the per-field local layout). Check-source
                // already verified that indices cover 0..N exactly once.
                let mut out: Vec<Option<NirExpr>> = (0..expected_arity).map(|_| None).collect();
                for f in fields {
                    let k = f.field_index as usize;
                    if k >= expected_arity {
                        return None;
                    }
                    if out[k].is_some() {
                        return None;
                    }
                    let mut e = f.value.clone();
                    self.visit_expr(&mut e);
                    out[k] = Some(e);
                }
                out.into_iter().collect::<Option<Vec<_>>>()
            }
            NirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } if list_method_kind(func, ctx.sig_kinds) == Some(ListMethodKind::IndexReader)
                && args.len() == 1 =>
            {
                let other = receiver_local(receiver)?;
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
                let idx_expr = &args[0].expr;
                if !is_duplicable_expr(idx_expr) {
                    return None;
                }
                let sig = sig_key_of(func)?;
                let mut out = Vec::with_capacity(expected_arity);
                for k in 0..expected_arity {
                    let other_field_local = ctx.field_local_map[&(other, k as u32)];
                    let (other_field_name, other_arr_ty) =
                        ctx.field_info_map[&(other, k as u32)].clone();
                    let other_elem_ty = other_info.element_types[k];
                    let call = build_index_reader_call(
                        other_elem_ty,
                        other_arr_ty,
                        other_field_local,
                        other_field_name,
                        idx_expr.clone(),
                        &sig,
                        expr.span,
                        ctx,
                    );
                    out.push(call);
                }
                Some(out)
            }
            _ => None,
        }
    }
}

/// Build a `Unary::{Ref|MutRef}(Local{field_local})` receiver expression matching
/// the convention used by the lowering phase for method call receivers.
fn build_receiver(
    field_local: u32,
    field_name: String,
    arr_ty: TypeId,
    mut_ref: bool,
    span: Span,
) -> NirExpr {
    let local = NirExpr::new(
        NirExprKind::Local {
            index: field_local,
            name: field_name,
        },
        arr_ty,
        span,
    );
    let op = if mut_ref {
        crate::nir::NirUnaryOp::MutRef
    } else {
        crate::nir::NirUnaryOp::Ref
    };
    NirExpr::new(
        NirExprKind::Unary {
            op,
            expr: Box::new(local),
        },
        arr_ty,
        span,
    )
}

/// Build `v_field.ElementWriter(value)` — e.g. `v_field.push(value)` — using the
/// supplied method family `sig`. The `sig` is taken from the original call that
/// triggered the rewrite, so any stdlib method matching `ElementWriter` flows
/// through unchanged.
fn build_element_writer_call(
    elem_ty: TypeId,
    arr_ty: TypeId,
    field_local: u32,
    field_name: String,
    value: NirExpr,
    sig: &SigKey,
    span: Span,
    ctx: &RewriteCtx,
) -> NirExpr {
    let func = ctx
        .catalog
        .get(&(elem_ty, sig.clone()))
        .expect("ElementWriter entry checked by required_methods_available")
        .clone();
    let receiver = build_receiver(
        field_local,
        field_name,
        arr_ty,
        /*mut_ref=*/ true,
        span,
    );
    let kind = NirExprKind::method_call(
        Box::new(receiver),
        func,
        Vec::new(),
        vec![CallArg::new(value, false)],
    );
    NirExpr::new(kind, TypeTable::UNIT, span)
}

/// Build `v_field.IndexWriter(index, value)` — e.g. `index_assign(index, value)`.
/// The `sig` is taken from the original call that triggered the rewrite.
fn build_index_writer_call(
    elem_ty: TypeId,
    arr_ty: TypeId,
    field_local: u32,
    field_name: String,
    index: NirExpr,
    value: NirExpr,
    sig: &SigKey,
    span: Span,
    ctx: &RewriteCtx,
) -> NirExpr {
    let func = ctx
        .catalog
        .get(&(elem_ty, sig.clone()))
        .expect("IndexWriter entry checked by required_methods_available")
        .clone();
    let receiver = build_receiver(
        field_local,
        field_name,
        arr_ty,
        /*mut_ref=*/ true,
        span,
    );
    let kind = NirExprKind::method_call(
        Box::new(receiver),
        func,
        Vec::new(),
        vec![CallArg::new(index, false), CallArg::new(value, false)],
    );
    NirExpr::new(kind, TypeTable::UNIT, span)
}

/// Build `v_field.IndexReader(index)` — e.g. `index_value(index)`. Returns a
/// `NirExpr` of type `elem_ty`. The `sig` is taken from the original call that
/// triggered the rewrite.
fn build_index_reader_call(
    elem_ty: TypeId,
    arr_ty: TypeId,
    field_local: u32,
    field_name: String,
    index: NirExpr,
    sig: &SigKey,
    span: Span,
    ctx: &RewriteCtx,
) -> NirExpr {
    let func = ctx
        .catalog
        .get(&(elem_ty, sig.clone()))
        .expect("IndexReader entry checked by required_methods_available")
        .clone();
    let receiver = build_receiver(
        field_local,
        field_name,
        arr_ty,
        /*mut_ref=*/ false,
        span,
    );
    let kind = NirExprKind::method_call(
        Box::new(receiver),
        func,
        Vec::new(),
        vec![CallArg::new(index, false)],
    );
    NirExpr::new(kind, elem_ty, span)
}

/// Returns true if the expression can be safely duplicated (cloned and
/// re-evaluated N times with no observable side effects).
fn is_duplicable_expr(e: &NirExpr) -> bool {
    match &e.kind {
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. } => true,
        NirExprKind::Binary { left, right, .. } => {
            is_duplicable_expr(left) && is_duplicable_expr(right)
        }
        NirExprKind::Unary { op, expr: inner } => {
            !matches!(
                op,
                crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef
            ) && is_duplicable_expr(inner)
        }
        NirExprKind::Cast { expr: inner, .. } => is_duplicable_expr(inner),
        // FieldAccess is only duplicable if its inner is too (most commonly a Local).
        // We deliberately exclude MethodCall / Call / Index / etc. because they
        // may allocate, trap, or have side effects.
        NirExprKind::FieldAccess { expr: inner, .. } => is_duplicable_expr(inner),
        _ => false,
    }
}

impl NirMutVisitor for Rewriter<'_, '_> {
    fn visit_block(&mut self, block: &mut NirBlock) {
        // Replace candidate let-bindings and expression-statement-level
        // `push` / `index_assign` calls with their per-field versions; recurse
        // into everything else.
        let old_stmts = std::mem::take(&mut block.stmts);
        let mut out: Vec<NirStmt> = Vec::with_capacity(old_stmts.len());
        for stmt in old_stmts {
            self.process_stmt(stmt, &mut out);
        }
        block.stmts = out;
    }

    /// Rewrite an expression in place. This handles the "leaf" uses of a
    /// decomposed candidate that were whitelisted by escape analysis:
    /// - `v.len()` / `v.is_empty()` → same call on field 0 (len is invariant across fields)
    /// - `v.index_value(i).K` → `v_K.index_value(i)`
    ///
    /// All other expressions recurse into children via `walk_expr`. A bare
    /// `Local { v }` reference should not occur here (escape analysis would have
    /// marked it escaped).
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        let ctx = self.ctx;

        // Handle FieldAccess on IndexValue first (read pattern).
        if let NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &expr.kind
            && let NirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } = &inner.kind
            && list_method_kind(func, ctx.sig_kinds) == Some(ListMethodKind::IndexReader)
            && args.len() == 1
            && let Some(rec_local) = receiver_local(receiver)
            && ctx.decomposed.contains(&rec_local)
        {
            let info = ctx
                .candidate_data
                .get(&rec_local)
                .expect("decomposed must have candidate data");
            let k = *field_index as usize;
            if k < info.element_types.len() {
                let elem_ty = info.element_types[k];
                let field_local = ctx.field_local_map[&(rec_local, k as u32)];
                let (field_name, arr_ty) = ctx.field_info_map[&(rec_local, k as u32)].clone();
                let mut idx_expr = args[0].expr.clone();
                let sig = sig_key_of(func).expect("IndexReader call must have SigKey");
                self.visit_expr(&mut idx_expr);
                let new_call = build_index_reader_call(
                    elem_ty,
                    arr_ty,
                    field_local,
                    field_name,
                    idx_expr,
                    &sig,
                    expr.span,
                    ctx,
                );
                *expr = new_call;
                return;
            }
        }

        // Handle Query calls on decomposed candidates (len/is_empty/capacity —
        // anything classified as length-invariant). Dispatch to field 0: since
        // all per-field arrays are kept in lockstep by push/index_assign/
        // with_capacity, field 0 is representative.
        if let NirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } = &expr.kind
            && let Some(rec_local) = receiver_local(receiver)
            && ctx.decomposed.contains(&rec_local)
            && args.is_empty()
            && list_method_kind(func, ctx.sig_kinds) == Some(ListMethodKind::Query)
        {
            let info = ctx
                .candidate_data
                .get(&rec_local)
                .expect("decomposed must have candidate data");
            let elem_ty = info.element_types[0];
            let field_local = ctx.field_local_map[&(rec_local, 0)];
            let (field_name, arr_ty) = ctx.field_info_map[&(rec_local, 0)].clone();
            // Look up the same query method on List<T_0>. A missing catalog entry
            // here is a compiler bug: escape analysis records every observed Query
            // use via `record_use`, and `required_methods_available` verifies the
            // entry exists for every per-field element type before the candidate
            // is admitted to the decomposed set. Reaching this branch with a miss
            // would mean those invariants broke.
            let sig = sig_key_of(func).expect("Query call must have SigKey");
            let new_func = ctx
                .catalog
                .get(&(elem_ty, sig))
                .cloned()
                .expect("Query monomorphization must exist for decomposed element type");
            let new_receiver = build_receiver(
                field_local,
                field_name,
                arr_ty,
                /*mut_ref=*/ false,
                expr.span,
            );
            let kind =
                NirExprKind::method_call(Box::new(new_receiver), new_func, Vec::new(), Vec::new());
            expr.kind = kind;
            return;
        }

        // Default: recurse into children.
        self.walk_expr(expr);
    }
}
