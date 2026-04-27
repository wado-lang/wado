//! Effect handler dispatch synthesis (Phase 3 of WEP 2026-04-11).
//!
//! Runs after effect-check / stores-check, before link/monomorphize.
//!
//! MVP scope (cross-function-boundary dispatch is deferred to Phase 4):
//! 1. Rewrite `Resume { value }` to `Return { value }` in every method
//!    inside an `impl <Effect> for <Type>` block.
//! 2. Replace each `WithHandler { bindings, body, .. }` node with a plain
//!    `Block(body)` whose body has every direct `<E>::<op>(args)` call
//!    statically devirtualised to a `MethodCall { receiver: handler, .. }`
//!    on the bound handler. Recognises both shapes the resolver and CM
//!    binding pass produce: the WASI rewriting `__cm_binding__<E>_<op>`
//!    and the user-effect namespaced shape `<op>` in
//!    `Local{path: "<E>"}` from `CalleeRef::local_namespace`.
//!
//! See WEP 2026-04-11 for the full design (per-effect global +
//! `__Dispatch_<E>` struct + dispatch wrapper functions). Static
//! devirtualisation only handles direct calls inside the do-block body;
//! calls reached through helper functions invoked from the body keep
//! routing through the existing CM binding (for WASI) or fail to resolve
//! (for user effects). The proper dispatch wrapper path is reserved for
//! the cross-function-boundary follow-up.

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::package::Package;
use crate::synthesis::common::synth_span;
use crate::tir::{
    EffectRef, TirBlock, TirEffectOp, TirExpr, TirExprKind, TirField, TirStmt, TirStmtKind,
    TirStruct, TirTemplatePart, TypeId, TypeTable,
};

/// Canonical identity of an effect: `(defining_module, name)`.
///
/// Two effects in distinct modules that happen to share a name (rare in
/// practice but possible) compare unequal. Mirrors the resolver's
/// canonicalisation of `EffectRef::Concrete { name, module_source }`.
#[allow(dead_code)]
type EffectKey = (ModuleSource, String);

/// Metadata for a single effect, indexed by `EffectKey`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct EffectMeta {
    /// The operation declarations (in source order) — copied so later
    /// passes don't have to walk the module tree again.
    operations: Vec<TirEffectOp>,
}

/// Walk every TIR module and build an `EffectKey -> EffectMeta` index.
#[allow(dead_code)]
fn build_effect_index(project: &Package) -> IndexMap<EffectKey, EffectMeta> {
    let mut out: IndexMap<EffectKey, EffectMeta> = IndexMap::default();
    for (module_source, module) in &project.tir_modules {
        for effect in &module.effects {
            out.insert(
                (module_source.clone(), effect.name.clone()),
                EffectMeta {
                    operations: effect.operations.clone(),
                },
            );
        }
    }
    out
}

/// Identify which effects need dispatch infrastructure.
///
/// An effect is "active" iff at least one user-written `impl <Effect> for
/// <Type>` block exists for it in the package. Effects without any impl
/// don't need a dispatch struct / global / wrapper triple — they always
/// route through the WASI CM binding (or, for user-defined effects with
/// no impl, were already rejected by effect-check).
///
/// `impl_index` is keyed by bare effect name (resolver enforces no name
/// collisions across modules). When the effect index has multiple
/// entries with the same name (in distinct modules), all of them are
/// flagged active so the dispatch infrastructure attaches to whichever
/// `EffectKey` the impl-walking pass canonicalises against.
#[allow(dead_code)]
fn identify_active_effects(
    effect_index: &IndexMap<EffectKey, EffectMeta>,
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
) -> IndexSet<EffectKey> {
    let mut out: IndexSet<EffectKey> = IndexSet::default();
    for (_, effect_name) in impl_index.keys() {
        for (key, _) in effect_index {
            if &key.1 == effect_name {
                out.insert(key.clone());
            }
        }
    }
    out
}

/// All the bookkeeping needed to emit / refer to the dispatch
/// infrastructure for a single effect.
///
/// A `DispatchPlan` is built once per active effect by
/// `synthesize_dispatch_infrastructure` and consumed by the lowering
/// (`lower_with_handler`) and call-site rewriting
/// (`rewrite_call_sites_to_wrappers`) passes.
#[allow(dead_code)]
#[derive(Debug)]
struct DispatchPlan {
    /// `TypeId` of the synthesised `__Dispatch_<E>` struct.
    struct_type_id: TypeId,
    /// `TypeId` of `Option<&__Dispatch_<E>>` — the global's runtime type
    /// and the type of `outer` / dispatch wrapper-saved values.
    nullable_ref_type_id: TypeId,
    /// `TypeId` of `&__Dispatch_<E>` — handed to `Option::Some` when
    /// installing a fresh dispatch record.
    inner_ref_type_id: TypeId,
    /// Name of the synthesised `__effect_<E>` mutable global.
    global_name: String,
    /// Operation name → dispatch wrapper function name
    /// (`__effect_dispatch__<E>__<op>`).
    wrapper_names: IndexMap<String, String>,
    /// Operation name → dispatch struct field name (`op_<op>`).
    field_names: IndexMap<String, String>,
    /// Operation name → field type (`fn(<op_params>) -> <op_ret>`).
    field_types: IndexMap<String, TypeId>,
    /// Operation name → 0-based field index in `__Dispatch_<E>`. The
    /// `outer` field always sits at index 0; ops start at 1.
    field_indices: IndexMap<String, u32>,
    /// Cached operation declarations (cloned from `EffectMeta`) so the
    /// wrapper / closure synth doesn't have to walk back to the index.
    operations: Vec<TirEffectOp>,
}

/// Synthesise the `__Dispatch_<E>` Wasm GC struct for one effect.
///
/// Two-phase construction handles the recursive
/// `outer: Option<&__Dispatch_<E>>` field:
///
/// 1. `make_struct` interns an empty / forward-declared struct type id
///    in the shared `TypeTable`.
/// 2. Field types — including the `Option<&Self>` outer field, which
///    references that very type id — are computed in the same borrow
///    scope.
///
/// The matching `TirStruct` decl is then appended to the entry module's
/// `structs` list. Layout:
///
/// ```text
/// struct __Dispatch_<E> {
///     outer: Option<&__Dispatch_<E>>,
///     op_<n>: fn(<op_n_params>) -> <op_n_ret>,
///     ...
/// }
/// ```
///
/// All synthesised dispatch infrastructure lives in the entry module,
/// mirroring `cm_binding`'s placement of WASI binding adapters.
#[allow(dead_code)]
fn synthesize_dispatch_struct(
    project: &mut Package,
    entry_source: &ModuleSource,
    key: &EffectKey,
    meta: &EffectMeta,
) -> DispatchPlan {
    let (_, effect_name) = key;
    let struct_name = format!("__Dispatch_{effect_name}");
    let global_name = format!("__effect_{effect_name}");

    let entry_module = project
        .tir_modules
        .get_mut(entry_source)
        .expect("entry module must exist");
    let tt_rc = entry_module.type_table.clone();

    // Build all the type IDs in a single borrow scope. The recursive
    // `outer` field type is computed off the freshly-interned struct
    // type id.
    let struct_type_id;
    let inner_ref_type_id;
    let nullable_ref_type_id;
    let mut op_field_types: Vec<TypeId> = Vec::with_capacity(meta.operations.len());
    {
        let mut tt = tt_rc.borrow_mut();
        struct_type_id = tt.make_struct(struct_name.clone(), entry_source.clone());
        inner_ref_type_id = tt.make_ref(struct_type_id);
        nullable_ref_type_id = tt.make_option(inner_ref_type_id);
        for op in &meta.operations {
            let param_types: Vec<TypeId> = op.params.iter().map(|p| p.type_id).collect();
            let op_func_type = tt.make_function(param_types, op.return_type, vec![], vec![]);
            op_field_types.push(op_func_type);
        }
    }

    // Build the field decls: outer at index 0, then ops in source order.
    let outer_field_type = nullable_ref_type_id;
    let mut fields: Vec<TirField> = Vec::with_capacity(meta.operations.len() + 1);
    fields.push(TirField {
        name: "outer".to_string(),
        is_pub: false,
        type_id: outer_field_type,
        index: 0,
        span: synth_span(),
        is_hidden: false,
        serde_rename: None,
        serde_default: false,
        default_expr: None,
    });

    let mut wrapper_names: IndexMap<String, String> = IndexMap::default();
    let mut field_names: IndexMap<String, String> = IndexMap::default();
    let mut field_types: IndexMap<String, TypeId> = IndexMap::default();
    let mut field_indices: IndexMap<String, u32> = IndexMap::default();

    for (i, op) in meta.operations.iter().enumerate() {
        let field_name = format!("op_{}", op.name);
        let field_type = op_field_types[i];
        let field_index = (i + 1) as u32;
        fields.push(TirField {
            name: field_name.clone(),
            is_pub: false,
            type_id: field_type,
            index: field_index,
            span: synth_span(),
            is_hidden: false,
            serde_rename: None,
            serde_default: false,
            default_expr: None,
        });
        wrapper_names.insert(
            op.name.clone(),
            format!("__effect_dispatch__{}__{}", effect_name, op.name),
        );
        field_names.insert(op.name.clone(), field_name);
        field_types.insert(op.name.clone(), field_type);
        field_indices.insert(op.name.clone(), field_index);
    }

    entry_module.add_struct(TirStruct {
        name: struct_name,
        module_source: entry_source.clone(),
        is_pub: false,
        type_params: vec![],
        monomorph_info: None,
        fields,
        span: synth_span(),
        serde_rename_all: None,
    });

    DispatchPlan {
        struct_type_id,
        nullable_ref_type_id,
        inner_ref_type_id,
        global_name,
        wrapper_names,
        field_names,
        field_types,
        field_indices,
        operations: meta.operations.clone(),
    }
}

/// Run the effect dispatch synthesis pass on the package.
///
/// Returns a `Result` so a future implementation that rejects malformed
/// input has a place to report errors. The MVP currently never fails.
pub fn synthesize(mut project: Package) -> Result<Package, String> {
    lower_resume_in_handler_methods(&mut project);
    let impl_index = build_handler_impl_index(&project);
    lower_with_handler_in_modules(&mut project, &impl_index);
    Ok(project)
}

/// Index of `impl Effect for Type` blocks: maps
/// `(handler_type_name, effect_name)` to the impl block's owning module
/// and the bare names of methods declared inside it. The dispatch
/// lowering uses this to resolve `<Effect>::<op>(args)` calls inside a
/// `with E = h do` body to a direct method call on `h`.
type HandlerImplKey = (String, String); // (handler_type_name, effect_name)

#[derive(Debug)]
struct HandlerImplInfo {
    /// Module that owns the impl block — used to build the
    /// `FunctionRef::module_source` of the generated `MethodCall`.
    impl_module: ModuleSource,
    /// Per-method return type from the impl. Lets the lowering patch up
    /// the result type of a synthesised `MethodCall` even when the
    /// original `Counter::next()` `Call` came in with `Unit` (the
    /// resolver doesn't know the operation's return type for
    /// user-defined effects without a CM binding).
    methods: IndexMap<String, TypeId>,
    /// Handler struct's type name (the `T` in `impl Effect for T`).
    /// Used to build the mangled method name in generated `MethodCall`s.
    handler_type_name: String,
}

fn build_handler_impl_index(project: &Package) -> IndexMap<HandlerImplKey, HandlerImplInfo> {
    // Impl-block methods are flattened into `TirModule.functions` by the
    // resolver and tagged via `method_info: LocalMethodName { struct_name,
    // trait_name, method_name }`. The companion `TirImpl` list is empty
    // for trait impls in the current pipeline, so we recover the
    // (struct, trait) → methods map directly from the function tags.
    let mut out: IndexMap<HandlerImplKey, HandlerImplInfo> = IndexMap::default();
    for (module_source, module) in &project.tir_modules {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            let Some(method_info) = &func.method_info else {
                continue;
            };
            let Some(trait_name) = &method_info.trait_name else {
                continue;
            };
            let key = (method_info.struct_name.clone(), trait_name.clone());
            let entry = out.entry(key).or_insert_with(|| HandlerImplInfo {
                impl_module: module_source.clone(),
                methods: IndexMap::default(),
                handler_type_name: method_info.struct_name.clone(),
            });
            entry
                .methods
                .insert(method_info.method_name.clone(), func.return_type);
        }
    }
    out
}

/// Walk every TIR function body in every module and replace each
/// `TirExprKind::WithHandler` with a desugared `Block` whose body has
/// any direct `<Effect>::<op>(args)` calls rewritten to `MethodCall`s
/// on the bound handler.
///
/// The MVP statically devirtualises calls that appear directly inside
/// the do-block body. Cross-function-boundary dispatch (calls reached
/// through helper functions invoked from the body) is deferred to the
/// per-effect global + dispatch wrapper follow-up tracked in the WEP.
fn lower_with_handler_in_modules(
    project: &mut Package,
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.clone();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                lower_with_in_block(body, impl_index, &type_table);
            }
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    lower_with_in_block(body, impl_index, &type_table);
                }
            }
        }
    }
}

fn lower_with_in_block(
    block: &mut TirBlock,
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    for stmt in &mut block.stmts {
        lower_with_in_stmt(stmt, impl_index, type_table);
    }
}

fn lower_with_in_stmt(
    stmt: &mut TirStmt,
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => lower_with_in_expr(value, impl_index, type_table),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                lower_with_in_expr(v, impl_index, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            lower_with_in_block(body, impl_index, type_table);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            lower_with_in_expr(condition, impl_index, type_table);
            lower_with_in_block(then_block, impl_index, type_table);
            if let Some(eb) = else_block {
                lower_with_in_block(eb, impl_index, type_table);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            lower_with_in_expr(scrutinee, impl_index, type_table);
            lower_with_in_block(then_block, impl_index, type_table);
            if let Some(eb) = else_block {
                lower_with_in_block(eb, impl_index, type_table);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            lower_with_in_expr(value, impl_index, type_table);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            lower_with_in_expr(iterable, impl_index, type_table);
            lower_with_in_block(body, impl_index, type_table);
        }
    }
}

fn lower_with_in_expr(
    expr: &mut TirExpr,
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    // Recurse first so nested `with` blocks are lowered inner-out, which
    // matches the runtime install order (innermost handler is the most
    // specific match for an effect operation in the body).
    walk_children(expr, impl_index, type_table);

    if let TirExprKind::WithHandler { bindings, body, .. } = &mut expr.kind {
        // For each binding, statically devirtualise direct calls inside
        // the body. The handler expression itself was already recursed
        // through in `walk_children`.
        for binding in bindings {
            let Some(EffectRef::Concrete {
                name: effect_name, ..
            }) = &binding.effect
            else {
                continue;
            };
            let handler_type_name = type_table
                .borrow()
                .type_name(deref_type(&type_table.borrow(), binding.handler.type_id));
            let key = (handler_type_name, effect_name.clone());
            let Some(info) = impl_index.get(&key) else {
                continue;
            };
            rewrite_effect_calls_in_block(body, effect_name, &binding.handler, info);
        }
        // Replace the WithHandler expression with the rewritten body so
        // the rest of the pipeline sees a plain block.
        let span = expr.span;
        let body_block = std::mem::replace(
            body,
            TirBlock {
                stmts: Vec::new(),
                span,
            },
        );
        let result_type = expr.type_id;
        *expr = TirExpr::new(TirExprKind::Block(body_block), result_type, span);
    }
}

fn walk_children(
    expr: &mut TirExpr,
    impl_index: &IndexMap<HandlerImplKey, HandlerImplInfo>,
    type_table: &std::cell::RefCell<TypeTable>,
) {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            lower_with_in_block(block, impl_index, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            lower_with_in_expr(condition, impl_index, type_table);
            lower_with_in_block(then_branch, impl_index, type_table);
            if let Some(eb) = else_branch {
                lower_with_in_block(eb, impl_index, type_table);
            }
        }
        TirExprKind::Match { expr, arms } => {
            lower_with_in_expr(expr, impl_index, type_table);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    lower_with_in_expr(g, impl_index, type_table);
                }
                lower_with_in_expr(&mut arm.body, impl_index, type_table);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            lower_with_in_expr(scrutinee, impl_index, type_table);
            for arm in arms {
                lower_with_in_block(arm, impl_index, type_table);
            }
            lower_with_in_block(default, impl_index, type_table);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                lower_with_in_expr(&mut arg.expr, impl_index, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            lower_with_in_expr(callee, impl_index, type_table);
            for arg in args {
                lower_with_in_expr(arg, impl_index, type_table);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                lower_with_in_expr(arg, impl_index, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            lower_with_in_expr(receiver, impl_index, type_table);
            for arg in args {
                lower_with_in_expr(&mut arg.expr, impl_index, type_table);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            lower_with_in_expr(left, impl_index, type_table);
            lower_with_in_expr(right, impl_index, type_table);
        }
        TirExprKind::Unary { expr, .. }
        | TirExprKind::Cast { expr, .. }
        | TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. }
        | TirExprKind::ClosureToCanonical { functor: expr, .. } => {
            lower_with_in_expr(expr, impl_index, type_table);
        }
        TirExprKind::Assign { target, value } => {
            lower_with_in_expr(target, impl_index, type_table);
            lower_with_in_expr(value, impl_index, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            lower_with_in_expr(value, impl_index, type_table);
        }
        TirExprKind::Index { expr, index } => {
            lower_with_in_expr(expr, impl_index, type_table);
            lower_with_in_expr(index, impl_index, type_table);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                lower_with_in_expr(&mut field.value, impl_index, type_table);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                lower_with_in_expr(elem, impl_index, type_table);
            }
        }
        TirExprKind::Closure { body, .. } => lower_with_in_expr(body, impl_index, type_table),
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                lower_with_in_expr(p, impl_index, type_table);
            }
        }
        TirExprKind::Resume { value } => {
            lower_with_in_expr(value, impl_index, type_table);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                lower_with_in_expr(&mut binding.handler, impl_index, type_table);
            }
            lower_with_in_block(body, impl_index, type_table);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    lower_with_in_expr(expr, impl_index, type_table);
                }
            }
        }
        // Leaf nodes: nothing to recurse into.
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Strip a single leading `&` / `&mut` layer to find the underlying
/// struct type that an `impl Effect for T` block targets.
fn deref_type(tt: &TypeTable, type_id: TypeId) -> TypeId {
    use crate::tir::ResolvedType;
    match tt.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
        _ => type_id,
    }
}

/// Walk every direct `<Effect>::<op>(args)` call inside the do-block
/// body and rewrite it to a `MethodCall { receiver: handler, ... }` on
/// the bound handler. Static devirtualisation only — calls reached
/// through helper functions invoked from `body` are not rewritten.
fn rewrite_effect_calls_in_block(
    block: &mut TirBlock,
    effect_name: &str,
    handler_expr: &TirExpr,
    info: &HandlerImplInfo,
) {
    for stmt in &mut block.stmts {
        rewrite_effect_calls_in_stmt(stmt, effect_name, handler_expr, info);
    }
}

fn rewrite_effect_calls_in_stmt(
    stmt: &mut TirStmt,
    effect_name: &str,
    handler_expr: &TirExpr,
    info: &HandlerImplInfo,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => {
            rewrite_effect_calls_in_expr(value, effect_name, handler_expr, info);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_effect_calls_in_expr(v, effect_name, handler_expr, info);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_effect_calls_in_block(body, effect_name, handler_expr, info);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_effect_calls_in_expr(condition, effect_name, handler_expr, info);
            rewrite_effect_calls_in_block(then_block, effect_name, handler_expr, info);
            if let Some(eb) = else_block {
                rewrite_effect_calls_in_block(eb, effect_name, handler_expr, info);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_effect_calls_in_expr(scrutinee, effect_name, handler_expr, info);
            rewrite_effect_calls_in_block(then_block, effect_name, handler_expr, info);
            if let Some(eb) = else_block {
                rewrite_effect_calls_in_block(eb, effect_name, handler_expr, info);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_effect_calls_in_expr(value, effect_name, handler_expr, info);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            rewrite_effect_calls_in_expr(iterable, effect_name, handler_expr, info);
            rewrite_effect_calls_in_block(body, effect_name, handler_expr, info);
        }
    }
}

fn rewrite_effect_calls_in_expr(
    expr: &mut TirExpr,
    effect_name: &str,
    handler_expr: &TirExpr,
    info: &HandlerImplInfo,
) {
    // Recurse first so children get rewritten before we examine the
    // current node — children inside Call args might themselves contain
    // effect-op calls.
    rewrite_children_for_effect_calls(expr, effect_name, handler_expr, info);

    if let TirExprKind::Call {
        func,
        args,
        type_args,
    } = &expr.kind
        && let Some(op_name) = match_effect_op_call(&func.name, &func.module_source, effect_name)
        && let Some(&method_return_type) = info.methods.get(&op_name)
    {
        // The original `Counter::next()` `Call` arrives here
        // with `expr.type_id == Unit` for user-defined effects
        // because the resolver does not look up the operation's
        // declared return type when it routes the call through
        // `CalleeRef::local_namespace`. Patch up to the impl
        // method's actual return type so the synthesised
        // `MethodCall` carries the right shape into WIR build.
        let receiver = handler_expr.clone();
        let mangled = crate::name::MethodName::format_local(
            &info.handler_type_name,
            Some(effect_name),
            &op_name,
        );
        let new_func = crate::tir::FunctionRef {
            module_source: info.impl_module.clone(),
            name: mangled,
            monomorph_info: None,
            method_info: Some(crate::name::LocalMethodName::new(
                info.handler_type_name.clone(),
                Some(effect_name.to_string()),
                op_name.clone(),
            )),
        };
        let new_kind = TirExprKind::method_call(
            Box::new(receiver),
            new_func,
            type_args.clone(),
            args.clone(),
        );
        let new_span = expr.span;
        *expr = TirExpr::new(new_kind, method_return_type, new_span);
    }
}

/// Return the operation name when `(module, name)` looks like an
/// effect-operation call for `effect_name`. Recognises both shapes the
/// resolver and CM-binding pass produce:
/// - WASI rewriting: `name == "__cm_binding__<E>_<op>"` (any module)
/// - User-defined effect: `module == ModuleSource::Local { path: "<E>" }`
fn match_effect_op_call(name: &str, module: &ModuleSource, effect_name: &str) -> Option<String> {
    let cm_prefix = format!("__cm_binding__{effect_name}_");
    if let Some(op) = name.strip_prefix(&cm_prefix) {
        return Some(op.to_string());
    }
    if let ModuleSource::Local { path } = module
        && path == effect_name
    {
        return Some(name.to_string());
    }
    None
}

fn rewrite_children_for_effect_calls(
    expr: &mut TirExpr,
    effect_name: &str,
    handler_expr: &TirExpr,
    info: &HandlerImplInfo,
) {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_effect_calls_in_block(block, effect_name, handler_expr, info);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_effect_calls_in_expr(condition, effect_name, handler_expr, info);
            rewrite_effect_calls_in_block(then_branch, effect_name, handler_expr, info);
            if let Some(eb) = else_branch {
                rewrite_effect_calls_in_block(eb, effect_name, handler_expr, info);
            }
        }
        TirExprKind::Match { expr, arms } => {
            rewrite_effect_calls_in_expr(expr, effect_name, handler_expr, info);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite_effect_calls_in_expr(g, effect_name, handler_expr, info);
                }
                rewrite_effect_calls_in_expr(&mut arm.body, effect_name, handler_expr, info);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_effect_calls_in_expr(scrutinee, effect_name, handler_expr, info);
            for arm in arms {
                rewrite_effect_calls_in_block(arm, effect_name, handler_expr, info);
            }
            rewrite_effect_calls_in_block(default, effect_name, handler_expr, info);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                rewrite_effect_calls_in_expr(&mut arg.expr, effect_name, handler_expr, info);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_effect_calls_in_expr(callee, effect_name, handler_expr, info);
            for arg in args {
                rewrite_effect_calls_in_expr(arg, effect_name, handler_expr, info);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                rewrite_effect_calls_in_expr(arg, effect_name, handler_expr, info);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_effect_calls_in_expr(receiver, effect_name, handler_expr, info);
            for arg in args {
                rewrite_effect_calls_in_expr(&mut arg.expr, effect_name, handler_expr, info);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_effect_calls_in_expr(left, effect_name, handler_expr, info);
            rewrite_effect_calls_in_expr(right, effect_name, handler_expr, info);
        }
        TirExprKind::Unary { expr, .. }
        | TirExprKind::Cast { expr, .. }
        | TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. }
        | TirExprKind::ClosureToCanonical { functor: expr, .. } => {
            rewrite_effect_calls_in_expr(expr, effect_name, handler_expr, info);
        }
        TirExprKind::Assign { target, value } => {
            rewrite_effect_calls_in_expr(target, effect_name, handler_expr, info);
            rewrite_effect_calls_in_expr(value, effect_name, handler_expr, info);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_effect_calls_in_expr(value, effect_name, handler_expr, info);
        }
        TirExprKind::Index { expr, index } => {
            rewrite_effect_calls_in_expr(expr, effect_name, handler_expr, info);
            rewrite_effect_calls_in_expr(index, effect_name, handler_expr, info);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                rewrite_effect_calls_in_expr(&mut field.value, effect_name, handler_expr, info);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                rewrite_effect_calls_in_expr(elem, effect_name, handler_expr, info);
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_effect_calls_in_expr(body, effect_name, handler_expr, info);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_effect_calls_in_expr(p, effect_name, handler_expr, info);
            }
        }
        TirExprKind::Resume { value } => {
            rewrite_effect_calls_in_expr(value, effect_name, handler_expr, info);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                rewrite_effect_calls_in_expr(&mut binding.handler, effect_name, handler_expr, info);
            }
            rewrite_effect_calls_in_block(body, effect_name, handler_expr, info);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    rewrite_effect_calls_in_expr(expr, effect_name, handler_expr, info);
                }
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Walk every method in every `impl Effect for T` block (where `Effect` is
/// an actual effect declaration) and rewrite `Resume { value }` to
/// `Return { value }`. Per the WEP MVP, `resume` has no post-resume
/// continuation, so it is semantically identical to `return`.
///
/// The resolver flattens impl-block methods into `TirModule.functions`
/// and tags each with `method_info: { struct_name, trait_name, ... }`.
/// We use the `trait_name` to recognise handler methods.
fn lower_resume_in_handler_methods(project: &mut Package) {
    // Collect bare names of every effect declaration so we can recognise
    // candidate impl blocks. A name collision between an effect and a
    // regular trait would already be a resolver error.
    let mut effect_names: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for effect in &module.effects {
            effect_names.insert(effect.name.clone());
        }
    }

    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            let Some(method_info) = &func.method_info else {
                continue;
            };
            let Some(trait_name) = &method_info.trait_name else {
                continue;
            };
            if !effect_names.contains(trait_name) {
                continue;
            }
            if let Some(body) = &mut func.body {
                rewrite_resume_in_block(body);
            }
        }
    }
}

/// Rewrite every `Resume { value }` in the block to a `Return { value }`
/// statement. The resume expression itself yields `Unit` at the source
/// level — when it sits at statement position it becomes a real return;
/// when it appears as a sub-expression we leave it as-is and rely on
/// `Return` short-circuiting the enclosing computation. The MVP fixtures
/// only place `resume` at statement position, so the simple statement-
/// level rewrite is sufficient.
fn rewrite_resume_in_block(block: &mut TirBlock) {
    for stmt in &mut block.stmts {
        rewrite_resume_in_stmt(stmt);
    }
}

fn rewrite_resume_in_stmt(stmt: &mut TirStmt) {
    // Statement-position `resume value;` is parsed as
    // `TirStmtKind::Expr(TirExpr { kind: Resume { value }, .. })`.
    // Replace it with `TirStmtKind::Return { value }`.
    // `resume value;` at statement position becomes `return value;`.
    if let TirStmtKind::Expr(expr) = &mut stmt.kind
        && let TirExprKind::Resume { value } = &mut expr.kind
    {
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span);
        let value = std::mem::replace(value.as_mut(), placeholder);
        stmt.kind = TirStmtKind::Return { value: Some(value) };
        return;
    }
    // `return resume value;` (which the resolver synthesises when a
    // method body's tail expression is `resume`, because the
    // missing-return rewriter sees `Resume { value }` in expression
    // position and wraps it in `Return { value: Some(Resume { ... }) }`)
    // collapses to `return value;`.
    if let TirStmtKind::Return { value: Some(value) } = &mut stmt.kind
        && let TirExprKind::Resume { value: inner } = &mut value.kind
    {
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, value.span);
        let inner = std::mem::replace(inner.as_mut(), placeholder);
        stmt.kind = TirStmtKind::Return { value: Some(inner) };
        return;
    }
    // Recurse into sub-statements; expression-position resumes inside
    // composite expressions are still walked but currently left as-is
    // (the e2e MVP has no such uses; if they appear, the `unreachable!`
    // stub in the lower phase will catch them).
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => rewrite_resume_in_expr(value),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_resume_in_expr(v);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_resume_in_block(body);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_resume_in_expr(condition);
            rewrite_resume_in_block(then_block);
            if let Some(eb) = else_block {
                rewrite_resume_in_block(eb);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_resume_in_expr(scrutinee);
            rewrite_resume_in_block(then_block);
            if let Some(eb) = else_block {
                rewrite_resume_in_block(eb);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => rewrite_resume_in_expr(value),
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            rewrite_resume_in_expr(iterable);
            rewrite_resume_in_block(body);
        }
    }
}

fn rewrite_resume_in_expr(expr: &mut TirExpr) {
    // Walk into all sub-expressions so nested handler methods (closures,
    // labelled blocks, etc.) get their statement-level `resume` rewritten
    // too. Expression-level rewriting is not needed for the MVP.
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_resume_in_block(block);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_resume_in_expr(condition);
            rewrite_resume_in_block(then_branch);
            if let Some(eb) = else_branch {
                rewrite_resume_in_block(eb);
            }
        }
        TirExprKind::Match { expr, arms } => {
            rewrite_resume_in_expr(expr);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite_resume_in_expr(g);
                }
                rewrite_resume_in_expr(&mut arm.body);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_resume_in_expr(scrutinee);
            for arm in arms {
                rewrite_resume_in_block(arm);
            }
            rewrite_resume_in_block(default);
        }
        TirExprKind::Closure { body, .. } => rewrite_resume_in_expr(body),
        _ => {}
    }
}
