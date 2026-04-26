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
    // Discovery and infrastructure generation are wired in but currently
    // unused by the lowering, which static-devirtualises direct
    // `<Effect>::<op>` calls inside `with E = h do { ... }` to method
    // calls on `h`. The infrastructure (per-effect dispatch struct +
    // mut global) is reserved for the cross-function-boundary dispatch
    // follow-up; until that lands we skip emission to avoid registering
    // unused recursive struct types that break Wasm validation.
    let _usage = discover(&project);
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
            let entry = out
                .entry(key)
                .or_insert_with(|| HandlerImplInfo {
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
/// through helper functions invoked from the body) is not yet handled
/// — it is reserved for the per-effect global / dispatch wrapper
/// follow-up that the registered `__Dispatch_<E>` struct + global
/// already prepare for.
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
            lower_with_in_expr(value, impl_index, type_table)
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
            let handler_type_name = type_table.borrow().type_name(deref_type(
                &type_table.borrow(),
                binding.handler.type_id,
            ));
            let key = (handler_type_name, effect_name.clone());
            let Some(info) = impl_index.get(&key) else {
                continue;
            };
            rewrite_effect_calls_in_block(
                body,
                effect_name,
                &binding.handler,
                info,
            );
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
            lower_with_in_expr(value, impl_index, type_table)
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

    if let TirExprKind::Call { func, args, type_args } = &expr.kind {
        if let Some(op_name) = match_effect_op_call(&func.name, &func.module_source, effect_name) {
            if let Some(&method_return_type) = info.methods.get(&op_name) {
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
    if let ModuleSource::Local { path } = module {
        if path == effect_name {
            return Some(name.to_string());
        }
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

/// Register a `__Dispatch_<E>` struct and `__effect_<E>` mut global for
/// every effect that needs dispatch infrastructure. The struct is added to
/// the entry module so it is always present (the effect's defining module
/// may live in read-only stdlib).
#[allow(dead_code)] // wired up by the cross-function dispatch follow-up
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
#[allow(dead_code)] // wired up by the cross-function dispatch follow-up
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

#[allow(dead_code)]
const OUTER_FIELD: &str = "outer";

#[allow(dead_code)]
fn dispatch_struct_name(effect_name: &str) -> String {
    format!("__Dispatch_{effect_name}")
}

#[allow(dead_code)]
fn effect_global_name(effect_name: &str) -> String {
    format!("__effect_{effect_name}")
}

#[allow(dead_code)]
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
#[allow(dead_code)] // wired up by the cross-function dispatch follow-up
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

