//! Effect handler dispatch synthesis (Phase 3 of WEP 2026-04-11).
//!
//! Runs after effect-check / stores-check, before link/monomorphize.
//!
//! Responsibilities:
//! 1. For every effect `E` that appears as a `with E = h do` binding or as a
//!    direct `<E>::<op>(...)` call site, synthesize:
//!    - `__Dispatch_<E>` GC struct (TIR struct with one field per operation
//!      plus an `outer` chain pointer)
//!    - `__effect_<E>` mut global of type `Option<&__Dispatch_<E>>`,
//!      initialised to `None` so the null-fast-path lands on the existing
//!      CM binding (for WASI effects) or traps (for user-defined effects)
//!    - One dispatch wrapper TIR function per operation, e.g.
//!      `__effect_dispatch__<E>__<op>(args) -> ret`
//! 2. Replace `TirExprKind::WithHandler` with the desugared block:
//!    `let __save = __effect_<E>; __effect_<E> = build_dispatch(); body;
//!    __effect_<E> = __save;`
//! 3. Replace `TirExprKind::Resume { value }` with `TirStmtKind::Return`.
//! 4. Rewrite call sites of `<E>::<op>` (and the matching CM binding for
//!    WASI effects) to call `__effect_dispatch__<E>__<op>` instead.

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::package::Package;
use crate::tir::{
    EffectRef, TirBlock, TirEffect, TirExpr, TirExprKind, TirField, TirModule, TirStmt,
    TirStmtKind, TirStruct, TirTemplatePart, TypeId, TypeTable,
};
use crate::token::Span;

/// A unique identifier for an effect. Two `with` clauses that mention the
/// same effect (even via different import aliases) share the same key
/// because the resolver canonicalises `EffectRef::Concrete` to the
/// effect's defining module.
type EffectKey = (ModuleSource, String);

/// Per-effect discovery output: which operations of `E` are reached from
/// any TIR call site or `WithHandler` body in the program.
#[derive(Debug, Default)]
struct EffectUsage {
    /// True if at least one `WithHandler` binding installs a handler for
    /// this effect. Without any binding, no dispatch infrastructure is
    /// needed (effect operations would either go to the existing CM
    /// binding for WASI or fail effect-check for user effects).
    handler_installed: bool,
    /// Names of operations actually called somewhere in the program.
    /// Operations not in this set still get a wrapper (because effect-check
    /// only sees `<E>::<op>` symbolically and handler bodies might call
    /// any operation), but the discovery output is currently unused —
    /// reserved for the wrapper-emission step.
    #[allow(dead_code)]
    operations_called: IndexSet<String>,
}

/// Per-effect dispatch infrastructure registered with the entry module.
/// Fields are read by the lowering passes that follow Phase 3c, so they
/// are kept under `#[allow(dead_code)]` until those passes land.
#[derive(Debug)]
#[allow(dead_code)]
struct EffectDispatchInfo {
    /// `__Dispatch_<E>` struct type id.
    dispatch_struct_type_id: TypeId,
    /// `__Dispatch_<E>` struct name (matches the registered TIR struct).
    dispatch_struct_name: String,
    /// Module that owns the synthesized dispatch struct (always the entry
    /// module; the effect's defining module may be read-only stdlib).
    owner_module: ModuleSource,
    /// `__effect_<E>` global name.
    global_name: String,
    /// Type of the global: `Option<&__Dispatch_<E>>`.
    global_type_id: TypeId,
    /// Operation field names in declaration order, used by the dispatch
    /// wrapper and `with` lowering to look up funcref slots.
    op_field_names: Vec<String>,
}

/// Run the effect dispatch synthesis pass on the package.
///
/// Returns a `Result` so a future implementation that rejects malformed
/// input has a place to report errors. The MVP currently never fails.
pub fn synthesize(mut project: Package) -> Result<Package, String> {
    let usage = discover(&project);
    let effects = lookup_effect_decls(&project, &usage);
    let _infra = generate_infrastructure(&mut project, &effects);
    // TODO(phase-3): generate dispatch wrapper functions, lower WithHandler /
    // Resume, and rewrite call sites. Infrastructure is now in place; the
    // remaining commits use it.
    Ok(project)
}

/// Register a `__Dispatch_<E>` struct and `__effect_<E>` mut global for
/// every effect that needs dispatch infrastructure. The struct is added to
/// the entry module so it is always present (the effect's defining module
/// may live in read-only stdlib).
fn generate_infrastructure(
    project: &mut Package,
    effects: &IndexMap<EffectKey, TirEffect>,
) -> IndexMap<EffectKey, EffectDispatchInfo> {
    let mut out: IndexMap<EffectKey, EffectDispatchInfo> = IndexMap::default();
    if effects.is_empty() {
        return out;
    }

    let entry_source = project.entry_module_source.clone();
    let entry_module = match project.tir_modules.get_mut(&entry_source) {
        Some(m) => m,
        None => return out,
    };
    let type_table = entry_module.type_table.clone();

    for (key, effect) in effects {
        let info = register_effect_dispatch(entry_module, &entry_source, &type_table, effect);
        out.insert(key.clone(), info);
    }

    out
}

/// Register the `__Dispatch_<E>` struct type, build its field list, and
/// add a corresponding `__effect_<E>` global initialised to `null`.
fn register_effect_dispatch(
    module: &mut TirModule,
    owner_module: &ModuleSource,
    type_table: &std::cell::RefCell<TypeTable>,
    effect: &TirEffect,
) -> EffectDispatchInfo {
    // 1. Reserve the struct type id so the `outer` field can refer back
    //    to the struct being defined (recursive type).
    let struct_name = dispatch_struct_name(&effect.name);
    let dispatch_struct_type_id = type_table
        .borrow_mut()
        .make_struct(struct_name.clone(), owner_module.clone());

    // 2. Build the field list. `outer` is `Option<&Self>`; per-op fields
    //    are `Option<fn(args...) -> ret>` so an unimplemented operation
    //    can be represented by `None` (the dispatch wrapper traps on
    //    None). All field types are interned in the entry module's
    //    shared type table.
    let mut fields = Vec::with_capacity(effect.operations.len() + 1);
    let outer_type = {
        let mut tt = type_table.borrow_mut();
        let self_ref = tt.make_ref(dispatch_struct_type_id);
        tt.make_option(self_ref)
    };
    fields.push(TirField {
        name: OUTER_FIELD.to_string(),
        is_pub: false,
        type_id: outer_type,
        index: 0,
        span: effect.span,
        is_hidden: false,
        serde_rename: None,
        serde_default: false,
        default_expr: None,
    });

    let mut op_field_names = Vec::with_capacity(effect.operations.len());
    for (i, op) in effect.operations.iter().enumerate() {
        let field_name = op_field_name(&op.name);
        op_field_names.push(field_name.clone());
        let fn_type = {
            let mut tt = type_table.borrow_mut();
            let params: Vec<TypeId> = op.params.iter().map(|p| p.type_id).collect();
            tt.make_function(params, op.return_type, Vec::new(), Vec::new())
        };
        let field_type = type_table.borrow_mut().make_option(fn_type);
        fields.push(TirField {
            name: field_name,
            is_pub: false,
            type_id: field_type,
            index: (i + 1) as u32,
            span: op.span,
            is_hidden: false,
            serde_rename: None,
            serde_default: false,
            default_expr: None,
        });
    }

    // 3. Register the struct definition.
    module.structs.push(TirStruct {
        name: struct_name.clone(),
        module_source: owner_module.clone(),
        is_pub: false,
        type_params: Vec::new(),
        monomorph_info: None,
        fields,
        span: effect.span,
        serde_rename_all: None,
    });

    // 4. Build the global type `Option<&__Dispatch_<E>>` and the
    //    initial null value.
    let global_type_id = {
        let mut tt = type_table.borrow_mut();
        let self_ref = tt.make_ref(dispatch_struct_type_id);
        tt.make_option(self_ref)
    };
    let global_name = effect_global_name(&effect.name);
    let init = TirExpr::new(TirExprKind::Null, global_type_id, effect.span);
    module.globals.push(crate::tir::TirGlobal {
        name: global_name.clone(),
        ty: global_type_id,
        initializer: init,
        mutable: true,
        wado_mutable: true,
        is_pub: false,
        module_source: owner_module.clone(),
        span: effect.span,
        is_nullable: true,
        local_types: Vec::new(),
    });

    EffectDispatchInfo {
        dispatch_struct_type_id,
        dispatch_struct_name: struct_name,
        owner_module: owner_module.clone(),
        global_name,
        global_type_id,
        op_field_names,
    }
}

const OUTER_FIELD: &str = "outer";

fn dispatch_struct_name(effect_name: &str) -> String {
    format!("__Dispatch_{effect_name}")
}

fn effect_global_name(effect_name: &str) -> String {
    format!("__effect_{effect_name}")
}

fn op_field_name(op_name: &str) -> String {
    format!("op_{op_name}")
}

#[allow(dead_code)]
fn dispatch_wrapper_func_name(effect_name: &str, op_name: &str) -> String {
    format!("__effect_dispatch__{effect_name}__{op_name}")
}

/// Synthetic span used for nodes the dispatch synthesis introduces.
/// They have no source location in the user's program.
#[allow(dead_code)]
fn synth_span(effect_span: Span) -> Span {
    effect_span
}

/// Look up the `TirEffect` declaration for every effect that needs
/// dispatch infrastructure. Effects that have no `WithHandler` binding
/// in the program are skipped — the rest of the pipeline already knows
/// how to lower their operation calls (CM bindings for WASI, error for
/// user-defined).
///
/// Returns owned `TirEffect` clones so the caller can take a mutable
/// borrow on `project` afterwards (cloning is cheap relative to the
/// rest of synthesis and effect declarations are tiny).
fn lookup_effect_decls(
    project: &Package,
    usage: &IndexMap<EffectKey, EffectUsage>,
) -> IndexMap<EffectKey, TirEffect> {
    let mut out: IndexMap<EffectKey, TirEffect> = IndexMap::default();
    for (key, info) in usage {
        if !info.handler_installed {
            continue;
        }
        let (module_source, effect_name) = key;
        let Some(module) = project.tir_modules.get(module_source) else {
            continue;
        };
        if let Some(effect) = module.effects.iter().find(|e| &e.name == effect_name) {
            out.insert(key.clone(), effect.clone());
        }
    }
    out
}

/// Walk every TIR function body and collect the effects that need
/// dispatch infrastructure.
fn discover(project: &Package) -> IndexMap<EffectKey, EffectUsage> {
    let mut usage: IndexMap<EffectKey, EffectUsage> = IndexMap::default();
    for module in project.tir_modules.values() {
        scan_module(module, &mut usage);
    }
    usage
}

fn scan_module(module: &TirModule, usage: &mut IndexMap<EffectKey, EffectUsage>) {
    for func_rc in &module.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            scan_block(body, usage);
        }
    }
    for impl_block in &module.impls {
        for method in &impl_block.methods {
            if let Some(body) = &method.body {
                scan_block(body, usage);
            }
        }
    }
}

fn scan_block(block: &TirBlock, usage: &mut IndexMap<EffectKey, EffectUsage>) {
    for stmt in &block.stmts {
        scan_stmt(stmt, usage);
    }
}

fn scan_stmt(stmt: &TirStmt, usage: &mut IndexMap<EffectKey, EffectUsage>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => scan_expr(value, usage),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                scan_expr(v, usage);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            scan_block(body, usage);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
            ..
        } => {
            scan_expr(condition, usage);
            scan_block(then_block, usage);
            if let Some(eb) = else_block {
                scan_block(eb, usage);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            scan_expr(scrutinee, usage);
            scan_block(then_block, usage);
            if let Some(eb) = else_block {
                scan_block(eb, usage);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => scan_expr(value, usage),
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            scan_expr(iterable, usage);
            scan_block(body, usage);
        }
    }
}

fn scan_expr(expr: &TirExpr, usage: &mut IndexMap<EffectKey, EffectUsage>) {
    match &expr.kind {
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                if let Some(EffectRef::Concrete {
                    name,
                    module_source,
                }) = &binding.effect
                {
                    let entry = usage
                        .entry((module_source.clone(), name.clone()))
                        .or_default();
                    entry.handler_installed = true;
                }
                scan_expr(&binding.handler, usage);
            }
            scan_block(body, usage);
        }
        TirExprKind::Resume { value } => scan_expr(value, usage),
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            scan_block(block, usage);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            scan_expr(condition, usage);
            scan_block(then_branch, usage);
            if let Some(eb) = else_branch {
                scan_block(eb, usage);
            }
        }
        TirExprKind::Match { expr, arms } => {
            scan_expr(expr, usage);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    scan_expr(g, usage);
                }
                scan_expr(&arm.body, usage);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            scan_expr(scrutinee, usage);
            for arm in arms {
                scan_block(arm, usage);
            }
            scan_block(default, usage);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                scan_expr(&arg.expr, usage);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            scan_expr(callee, usage);
            for arg in args {
                scan_expr(arg, usage);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                scan_expr(arg, usage);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            scan_expr(receiver, usage);
            for arg in args {
                scan_expr(&arg.expr, usage);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            scan_expr(left, usage);
            scan_expr(right, usage);
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
        | TirExprKind::ClosureToCanonical { functor: expr, .. } => scan_expr(expr, usage),
        TirExprKind::Assign { target, value } => {
            scan_expr(target, usage);
            scan_expr(value, usage);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            scan_expr(value, usage);
        }
        TirExprKind::Index { expr, index } => {
            scan_expr(expr, usage);
            scan_expr(index, usage);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                scan_expr(&field.value, usage);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                scan_expr(elem, usage);
            }
        }
        TirExprKind::Closure { body, .. } => scan_expr(body, usage),
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                scan_expr(p, usage);
            }
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    scan_expr(expr, usage);
                }
            }
        }
        // Leaf nodes: nothing to scan.
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

