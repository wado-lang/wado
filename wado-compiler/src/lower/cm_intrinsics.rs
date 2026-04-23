//! Expand CM async intrinsics (`builtin::cm_lift_async_result`) into inline
//! TIR that lifts the async result from the caller-provided outptr buffer.
//!
//! The `AsyncCall<T>::wait` method in `core:prelude/types.wado` materialises
//! its result value by calling `builtin::cm_lift_async_result::<T>(outptr)`.
//! Monomorphisation substitutes `T` with the concrete async-import result
//! type, but the call itself remains a generic builtin — it cannot be
//! lowered to Wasm directly. This module walks the monomorphised TIR,
//! finds the calls, and replaces each with the equivalent inline lift
//! code produced by [`synthesize_lift_with_context`], parameterised by
//! the concrete return type of that call site.
//!
//! The pass runs as part of `lower` (after monomorphise, before boxing
//! and codegen) so that every call has its final concrete type.
//!
//! If a concrete `T` cannot be recovered as a Wado AST type, the call is
//! left alone — later Wasm validation will surface the remaining
//! intrinsic as a malformed call, signalling a bug. In practice the
//! only types that reach this pass are WASI CM return types for which
//! [`type_id_to_ast_type`] has a conversion path.

use std::cell::RefCell;

use crate::ast::{self, Type};
use crate::component_model::WasiRegistry;
use crate::name::ModuleSource;
use crate::synthesis::cm_binding::{LiftContext, synthesize_lift_with_context};
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirModule, TirStmt, TirStmtKind,
    TypeId, TypeTable,
};
use crate::tir_visitor::TirMutVisitor;
use crate::token::Span;

/// Expand all occurrences of `builtin::cm_lift_async_result::<T>(outptr)`
/// in `module` into inline lift code.
pub fn expand_cm_intrinsics(module: &mut TirModule) {
    // Acquire a static reference to the WASI registry — lift of WASI
    // variants / records needs it for layout information. Reusing the
    // shared registry avoids the cost of rebuilding from stdlib.
    let (wasi_registry, _world_registry) = WasiRegistry::build_from_stdlib();

    let type_table = module.type_table.clone();

    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        if func.body.is_none() {
            continue;
        }
        let mut next_local = func.local_count;
        let mut local_types = std::mem::take(&mut func.local_types);
        let mut body = func.body.take().expect("body checked above");
        let fn_name = func.name.clone();
        {
            let mut rewriter = Rewriter {
                next_local: &mut next_local,
                local_types: &mut local_types,
                type_table: &type_table,
                wasi_registry,
                rewrite_count: 0,
                function_name: &fn_name,
            };
            rewriter.visit_block(&mut body);
            let _ = rewriter.rewrite_count;
        }
        func.body = Some(body);
        func.local_count = next_local;
        func.local_types = local_types;
    }
}

struct Rewriter<'a> {
    next_local: &'a mut u32,
    local_types: &'a mut Vec<TypeId>,
    type_table: &'a RefCell<TypeTable>,
    wasi_registry: &'a WasiRegistry,
    rewrite_count: u32,
    #[allow(dead_code)]
    function_name: &'a str,
}

impl TirMutVisitor for Rewriter<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // Recurse first so inner calls get expanded before the outer
        // layer is examined.
        self.walk_expr(expr);

        let matches = match &expr.kind {
            TirExprKind::Call { func, args, .. } => is_cm_lift_call(func) && args.len() == 1,
            _ => false,
        };
        if !matches {
            return;
        }
        self.rewrite_count += 1;
        let addr_expr = match &expr.kind {
            TirExprKind::Call { args, .. } => args[0].expr.clone(),
            _ => unreachable!(),
        };
        let (ast_type, wasi_pkg) = {
            let tt = self.type_table.borrow();
            match type_id_to_ast_type(expr.type_id, &tt, self.wasi_registry) {
                Some(t) => {
                    let pkg = infer_wasi_package(expr.type_id, &tt).unwrap_or_default();
                    (t, pkg)
                }
                None => return,
            }
        };
        let mut stmts: Vec<TirStmt> = Vec::new();
        let lifted = synthesize_lift_with_context(
            &ast_type,
            addr_expr,
            self.next_local,
            &mut stmts,
            self.local_types,
            &LiftContext {
                wasi_registry: self.wasi_registry,
                type_table: self.type_table,
                wasi_package: &wasi_pkg,
            },
        );

        // Replace the Call expression with a Block whose last statement is
        // an `Expr` holding the lifted value — making the block's value
        // the lifted T. Any users of `expr` see the lifted value's type.
        let lifted_type = lifted.type_id;
        let mut block_stmts = stmts;
        block_stmts.push(TirStmt::new(TirStmtKind::Expr(lifted), expr.span));
        let mut block = TirBlock {
            stmts: block_stmts,
            span: expr.span,
        };
        // Walk into the newly-synthesised block so nested cm_lift_async_result
        // calls (should synthesize_lift ever emit one) are expanded too.
        self.visit_block(&mut block);

        expr.kind = TirExprKind::Block(block);
        expr.type_id = lifted_type;
    }
}

fn is_cm_lift_call(func: &FunctionRef) -> bool {
    func.module_source == ModuleSource::builtin() && func.name == "cm_lift_async_result"
}

/// Reconstruct a WASI-style AST [`Type`] from a `TypeId`. Covers the
/// type shapes that can appear as the result of an `async func` CM
/// import: primitives, WASI records/variants/enums (referenced by
/// name), and the generic `Array<T>`, `Option<T>`, `Result<T, E>` /
/// `Tuple` wrappers.
///
/// Returns `None` for types that cannot be directly expressed as a
/// `Type` without losing information (e.g. references). The caller
/// falls back to leaving the lift intrinsic in place, which surfaces as
/// a Wasm validation error and is a signal to extend this converter.
pub fn type_id_to_ast_type(
    type_id: TypeId,
    type_table: &TypeTable,
    wasi_registry: &crate::component_model::WasiRegistry,
) -> Option<Type> {
    let resolved = type_table.get(type_id).clone();
    type_id_to_ast_type_resolved(&resolved, type_table, wasi_registry)
}

fn type_id_to_ast_type_resolved(
    resolved: &ResolvedType,
    type_table: &TypeTable,
    wasi_registry: &crate::component_model::WasiRegistry,
) -> Option<Type> {
    match resolved {
        ResolvedType::Primitive(prim) => Some(named(prim.as_str())),
        ResolvedType::Struct {
            name, module_source, ..
        }
        | ResolvedType::Variant {
            name, module_source, ..
        }
        | ResolvedType::Enum {
            name, module_source, ..
        }
        | ResolvedType::Flags {
            name, module_source, ..
        }
        | ResolvedType::Newtype {
            name, module_source, ..
        }
        | ResolvedType::Resource {
            name, module_source, ..
        } => Some(named_from_module_source(name, module_source, wasi_registry)),
        ResolvedType::GenericInstance {
            name, type_args, ..
        } => {
            let args: Option<Vec<Type>> = type_args
                .iter()
                .map(|&arg| type_id_to_ast_type(arg, type_table, wasi_registry))
                .collect();
            Some(Type::Generic(ast::GenericType {
                id: ast::AstId::fresh(),
                name: name.clone(),
                args: args?,
                span: zero_span(),
            }))
        }
        ResolvedType::GenericResource {
            name, type_args, ..
        } => {
            let args: Option<Vec<Type>> = type_args
                .iter()
                .map(|&arg| type_id_to_ast_type(arg, type_table, wasi_registry))
                .collect();
            Some(Type::Generic(ast::GenericType {
                id: ast::AstId::fresh(),
                name: name.clone(),
                args: args?,
                span: zero_span(),
            }))
        }
        ResolvedType::BuiltinArray(elem) => {
            let elem_ty = type_id_to_ast_type(*elem, type_table, wasi_registry)?;
            Some(Type::Generic(ast::GenericType {
                id: ast::AstId::fresh(),
                name: "Array".to_string(),
                args: vec![elem_ty],
                span: zero_span(),
            }))
        }
        ResolvedType::Unit => Some(Type::Tuple(Vec::new())),
        ResolvedType::Ref(_)
        | ResolvedType::MutRef(_)
        | ResolvedType::Function { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::Reactive(_)
        | ResolvedType::Never
        | ResolvedType::TypePack { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::Unknown
        | ResolvedType::Error => None,
    }
}

/// Infer the WASI package (e.g. `"http"`) associated with a type by
/// walking into its structure and returning the first `ModuleSource::Wasi`
/// interface's package part.
///
/// Used to disambiguate WASI types that share a name across packages —
/// for example `wasi:cli/ErrorCode` vs `wasi:http/ErrorCode`. Without
/// a package hint, `synthesize_lift_with_context` falls back to
/// unscoped lookup and picks an arbitrary match.
fn infer_wasi_package(type_id: TypeId, type_table: &TypeTable) -> Option<String> {
    let resolved = type_table.get(type_id).clone();
    match resolved {
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::Variant { module_source, .. }
        | ResolvedType::Enum { module_source, .. }
        | ResolvedType::Flags { module_source, .. }
        | ResolvedType::Newtype { module_source, .. }
        | ResolvedType::Resource { module_source, .. } => {
            wasi_package_from_module_source(&module_source)
        }
        ResolvedType::GenericInstance { type_args, .. }
        | ResolvedType::GenericResource { type_args, .. } => type_args
            .iter()
            .find_map(|&arg| infer_wasi_package(arg, type_table)),
        ResolvedType::BuiltinArray(inner)
        | ResolvedType::Ref(inner)
        | ResolvedType::MutRef(inner)
        | ResolvedType::Reactive(inner) => infer_wasi_package(inner, type_table),
        _ => None,
    }
}

fn wasi_package_from_module_source(module_source: &ModuleSource) -> Option<String> {
    // `Wasi { interface: "http/types.wado" }` → `"http"`.
    // `Wasi { interface: "cli.wado" }` (the umbrella file) is not scoped
    // to a specific package but the interface file `cli/types.wado`
    // encodes the `cli` package — take the leading path component.
    match module_source {
        ModuleSource::Wasi { interface } => {
            let head = interface.split('/').next()?;
            let head = head.strip_suffix(".wado").unwrap_or(head);
            if head.is_empty() {
                None
            } else {
                Some(head.to_string())
            }
        }
        _ => None,
    }
}

fn named(name: &str) -> Type {
    Type::Named(ast::NamedType {
        id: ast::AstId::fresh(),
        name: name.to_string(),
        span: zero_span(),
        source_interface: None,
    })
}

/// Construct a `Type::Named` whose `source_interface` is derived from the
/// TIR `ModuleSource` of the resolved type. For a `ModuleSource::Wasi`,
/// this produces the full `"wasi:{pkg}/{iface}@{version}"` form by matching
/// the TIR-side snake_case interface ("filesystem/types.wado") against the
/// stdlib-registered kebab-case interface ("wasi:filesystem/types@..."),
/// disambiguating by the type's Wado name. For non-wasi module_sources we
/// leave `source_interface` unresolved.
fn named_from_module_source(
    name: &str,
    module_source: &crate::name::ModuleSource,
    wasi_registry: &crate::component_model::WasiRegistry,
) -> Type {
    let source_interface = match module_source {
        crate::name::ModuleSource::Wasi { interface } => {
            let stripped = interface
                .strip_suffix(".wado")
                .unwrap_or(interface.as_str());
            let kebab = stripped.replace('_', "-");
            let prefix = format!("wasi:{kebab}@");
            wasi_registry
                .find_wasi_source_under_prefix(&prefix, name)
                .map(str::to_string)
        }
        _ => None,
    };
    Type::Named(ast::NamedType {
        id: ast::AstId::fresh(),
        name: name.to_string(),
        span: zero_span(),
        source_interface,
    })
}

fn zero_span() -> Span {
    Span::new(0, 0, 1, 1)
}
