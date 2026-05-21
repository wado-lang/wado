//! Template string expansion synthesis phase.
//!
//! Expands `TirExprKind::TemplateString` nodes into concrete formatting code:
//! a `__tmpl` labeled block containing `String::with_capacity`, `push_str` calls,
//! `Formatter` construction, and `Display`/`Inspect` trait dispatch.
//!
//! Pipeline position: pre-monomorphize synthesis phase.
//! Template expansion emits trait method calls (`Display::fmt`, `Inspect::inspect`)
//! that the monomorphizer resolves to concrete implementations. This approach
//! eliminates the need for post-mono `has_trait_impl` checks and standalone inspect
//! functions.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::compiler_item::{CompilerItem, CompilerItems};
use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::resolver::trait_env::TraitEnv;
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, TemplateFormatSpec, TirBlock, TirExpr,
    TirExprKind, TirLocal, TirModule, TirStmt, TirStmtKind, TirStructField, TirTemplatePart,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Snapshot of every `core:prelude/format` symbol name + case index the
/// template-expansion synthesiser needs. Resolved once through the
/// [`CompilerItem`] registry per template-string expansion, then threaded
/// through the helpers so stdlib renames flow without touching synthesis
/// sites — same shape as
/// [`super::cm_binding::types::CmStdlibNames`].
///
/// Every format-family trait is single-method, so each `<trait>` field
/// is paired with a `<trait>_method` field carrying the trait's
/// resolved method name (e.g. `"fmt"` for `Display`, `"inspect"` for
/// `Inspect`). Both values come from
/// [`CompilerItems::trait_method_name`].
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(super) struct FormatStdlibNames {
    pub formatter: String,
    pub alignment: String,
    pub left_name: String,
    pub left_index: u32,
    pub center_name: String,
    pub center_index: u32,
    pub right_name: String,
    pub right_index: u32,
    pub display: String,
    pub display_method: String,
    pub display_alt: String,
    pub display_alt_method: String,
    pub inspect: String,
    pub inspect_method: String,
    pub inspect_alt: String,
    pub inspect_alt_method: String,
    pub binary: String,
    pub binary_method: String,
    pub binary_alt: String,
    pub binary_alt_method: String,
    pub octal: String,
    pub octal_method: String,
    pub octal_alt: String,
    pub octal_alt_method: String,
    pub lower_hex: String,
    pub lower_hex_method: String,
    pub lower_hex_alt: String,
    pub lower_hex_alt_method: String,
    pub upper_hex: String,
    pub upper_hex_method: String,
    pub upper_hex_alt: String,
    pub upper_hex_alt_method: String,
    pub lower_exp: String,
    pub lower_exp_method: String,
    pub upper_exp: String,
    pub upper_exp_method: String,
}

impl FormatStdlibNames {
    pub fn from_compiler_items(items: &CompilerItems) -> Self {
        let (_, _, left_name, left_index) = items.require_enum_case(CompilerItem::AlignmentLeft);
        let (_, _, center_name, center_index) =
            items.require_enum_case(CompilerItem::AlignmentCenter);
        let (_, _, right_name, right_index) = items.require_enum_case(CompilerItem::AlignmentRight);
        Self {
            formatter: items.struct_name(CompilerItem::Formatter).to_string(),
            alignment: items.enum_name(CompilerItem::Alignment).to_string(),
            left_name: left_name.to_string(),
            left_index,
            center_name: center_name.to_string(),
            center_index,
            right_name: right_name.to_string(),
            right_index,
            display: items.trait_name(CompilerItem::Display).to_string(),
            display_method: items.trait_method_name(CompilerItem::Display).to_string(),
            display_alt: items.trait_name(CompilerItem::DisplayAlt).to_string(),
            display_alt_method: items
                .trait_method_name(CompilerItem::DisplayAlt)
                .to_string(),
            inspect: items.trait_name(CompilerItem::Inspect).to_string(),
            inspect_method: items.trait_method_name(CompilerItem::Inspect).to_string(),
            inspect_alt: items.trait_name(CompilerItem::InspectAlt).to_string(),
            inspect_alt_method: items
                .trait_method_name(CompilerItem::InspectAlt)
                .to_string(),
            binary: items.trait_name(CompilerItem::Binary).to_string(),
            binary_method: items.trait_method_name(CompilerItem::Binary).to_string(),
            binary_alt: items.trait_name(CompilerItem::BinaryAlt).to_string(),
            binary_alt_method: items.trait_method_name(CompilerItem::BinaryAlt).to_string(),
            octal: items.trait_name(CompilerItem::Octal).to_string(),
            octal_method: items.trait_method_name(CompilerItem::Octal).to_string(),
            octal_alt: items.trait_name(CompilerItem::OctalAlt).to_string(),
            octal_alt_method: items.trait_method_name(CompilerItem::OctalAlt).to_string(),
            lower_hex: items.trait_name(CompilerItem::LowerHex).to_string(),
            lower_hex_method: items.trait_method_name(CompilerItem::LowerHex).to_string(),
            lower_hex_alt: items.trait_name(CompilerItem::LowerHexAlt).to_string(),
            lower_hex_alt_method: items
                .trait_method_name(CompilerItem::LowerHexAlt)
                .to_string(),
            upper_hex: items.trait_name(CompilerItem::UpperHex).to_string(),
            upper_hex_method: items.trait_method_name(CompilerItem::UpperHex).to_string(),
            upper_hex_alt: items.trait_name(CompilerItem::UpperHexAlt).to_string(),
            upper_hex_alt_method: items
                .trait_method_name(CompilerItem::UpperHexAlt)
                .to_string(),
            lower_exp: items.trait_name(CompilerItem::LowerExp).to_string(),
            lower_exp_method: items.trait_method_name(CompilerItem::LowerExp).to_string(),
            upper_exp: items.trait_name(CompilerItem::UpperExp).to_string(),
            upper_exp_method: items.trait_method_name(CompilerItem::UpperExp).to_string(),
        }
    }
}

/// Expand all `TemplateString` nodes in a module.
///
/// Runs as part of the pre-mono synthesis phase. Template expansion emits
/// trait method calls (`Display::fmt`, `Inspect::inspect`) that the monomorphizer
/// subsequently resolves to concrete implementations.
pub fn expand_templates(
    module: &mut TirModule,
    tt: &Rc<RefCell<TypeTable>>,
    trait_env: &Arc<TraitEnv>,
) {
    let names = FormatStdlibNames::from_compiler_items(tt.borrow().compiler_items());
    let ctx = TemplateCtx {
        tt,
        module_src: module.module_source.clone(),
        trait_env,
        names: &names,
    };
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        let local_count = func.local_count;
        if let Some(ref mut body) = func.body {
            let mut alloc = FuncLocalAlloc {
                next_index: local_count,
                new_locals: Vec::new(),
            };
            expand_block(body, &mut alloc, &ctx);
            func.local_count = alloc.next_index;
            func.locals.extend(alloc.new_locals);
        }
    }
    // Walk impl-block methods too. They aren't reachable via
    // `module.functions` (which holds only free functions and
    // synthesised wrappers), so a template string inside e.g.
    // `impl Point { fn show(&self) -> String { return `..` } }` would
    // otherwise survive as a raw `TirExprKind::TemplateString` node and
    // hit later phases that don't know how to handle it.
    for impl_block in &mut module.impls {
        for method in &mut impl_block.methods {
            let local_count = method.local_count;
            if let Some(ref mut body) = method.body {
                let mut alloc = FuncLocalAlloc {
                    next_index: local_count,
                    new_locals: Vec::new(),
                };
                expand_block(body, &mut alloc, &ctx);
                method.local_count = alloc.next_index;
                method.locals.extend(alloc.new_locals);
            }
        }
    }
}

/// Read-only context shared across all template-expansion helpers.
struct TemplateCtx<'a> {
    tt: &'a Rc<RefCell<TypeTable>>,
    module_src: ModuleSource,
    trait_env: &'a Arc<TraitEnv>,
    names: &'a FormatStdlibNames,
}

struct FuncLocalAlloc {
    next_index: u32,
    new_locals: Vec<TirLocal>,
}

impl FuncLocalAlloc {
    fn alloc(&mut self, type_id: TypeId) -> u32 {
        let idx = self.next_index;
        self.next_index += 1;
        self.new_locals.push(TirLocal::synth(idx, type_id, false));
        idx
    }
}

fn expand_block(block: &mut TirBlock, alloc: &mut FuncLocalAlloc, ctx: &TemplateCtx) {
    for stmt in &mut block.stmts {
        expand_stmt(stmt, alloc, ctx);
    }
}

fn expand_stmt(stmt: &mut TirStmt, alloc: &mut FuncLocalAlloc, ctx: &TemplateCtx) {
    match &mut stmt.kind {
        TirStmtKind::Expr(e) => expand_expr(e, alloc, ctx),
        TirStmtKind::Let { value, .. } => {
            expand_expr(value, alloc, ctx);
        }
        TirStmtKind::Return { value: Some(e) } | TirStmtKind::Break { value: Some(e), .. } => {
            expand_expr(e, alloc, ctx);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expand_expr(condition, alloc, ctx);
            expand_block(then_block, alloc, ctx);
            if let Some(eb) = else_block {
                expand_block(eb, alloc, ctx);
            }
        }
        TirStmtKind::Loop { body } => {
            expand_block(body, alloc, ctx);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expand_expr(scrutinee, alloc, ctx);
            expand_block(then_block, alloc, ctx);
            if let Some(eb) = else_block {
                expand_block(eb, alloc, ctx);
            }
        }
        TirStmtKind::LetDestructure { value, .. } | TirStmtKind::TaskReturn { value, .. } => {
            expand_expr(value, alloc, ctx);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            expand_block(block, alloc, ctx);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            expand_expr(iterable, alloc, ctx);
            expand_block(body, alloc, ctx);
        }
        _ => {}
    }
}

fn expand_expr(expr: &mut TirExpr, alloc: &mut FuncLocalAlloc, ctx: &TemplateCtx) {
    // First, recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::TemplateString { parts } => {
            // Expand sub-expressions within template parts first
            for part in parts.iter_mut() {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    expand_expr(inner, alloc, ctx);
                }
            }
        }
        TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } => {
            expand_block(b, alloc, ctx);
            return;
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expand_expr(condition, alloc, ctx);
            expand_block(then_branch, alloc, ctx);
            if let Some(eb) = else_branch {
                expand_block(eb, alloc, ctx);
            }
            return;
        }
        TirExprKind::Match { expr: s, arms } => {
            expand_expr(s, alloc, ctx);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    expand_expr(guard, alloc, ctx);
                }
                expand_expr(&mut arm.body, alloc, ctx);
            }
            return;
        }
        TirExprKind::Call { args, .. } => {
            for a in args {
                expand_expr(&mut a.expr, alloc, ctx);
            }
            return;
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            expand_expr(receiver, alloc, ctx);
            for a in args {
                expand_expr(&mut a.expr, alloc, ctx);
            }
            return;
        }
        TirExprKind::Binary { left, right, .. } => {
            expand_expr(left, alloc, ctx);
            expand_expr(right, alloc, ctx);
            return;
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            expand_expr(inner, alloc, ctx);
            return;
        }
        TirExprKind::Assign { target, value } => {
            expand_expr(target, alloc, ctx);
            expand_expr(value, alloc, ctx);
            return;
        }
        TirExprKind::Index {
            expr: e,
            index: idx,
        } => {
            expand_expr(e, alloc, ctx);
            expand_expr(idx, alloc, ctx);
            return;
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                expand_expr(&mut f.value, alloc, ctx);
            }
            return;
        }
        TirExprKind::TupleLiteral { elements } => {
            for e in elements {
                expand_expr(e, alloc, ctx);
            }
            return;
        }
        TirExprKind::Closure {
            params,
            body,
            body_locals,
            ..
        } => {
            // Closure bodies own an independent local-index namespace
            // (see `TirExprKind::Closure::body_locals` / `address_taken_locals`).
            // Template synth locals (`__r`, `__f`, …) must be allocated in
            // that namespace; otherwise their indices collide with closure
            // params or body lets and `LocalCollector` in closure planning
            // merges incompatibly-typed locals into the same Wasm slot,
            // producing a module that fails core-Wasm validation.
            //
            // Mirrors the same closure-scope switch in
            // `lower::translate::pattern::lower_expr`'s `Closure` arm.
            let mut closure_alloc = FuncLocalAlloc {
                next_index: (params.len() + body_locals.len()) as u32,
                new_locals: Vec::new(),
            };
            expand_expr(body, &mut closure_alloc, ctx);
            // Surface the new synth locals on the closure so later passes
            // (pattern lowering, closure planning) see a `body_locals`
            // that matches the body's actual let-index range.
            body_locals.extend(closure_alloc.new_locals);
            return;
        }
        TirExprKind::IndirectCall { callee, args } => {
            expand_expr(callee, alloc, ctx);
            for a in args {
                expand_expr(a, alloc, ctx);
            }
            return;
        }
        TirExprKind::CmRawCall { args, .. } => {
            for a in args {
                expand_expr(a, alloc, ctx);
            }
            return;
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                expand_expr(p, alloc, ctx);
            }
            return;
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            expand_expr(value, alloc, ctx);
            return;
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                expand_expr(&mut binding.handler, alloc, ctx);
            }
            expand_block(body, alloc, ctx);
            return;
        }
        TirExprKind::Resume { value } => {
            expand_expr(value, alloc, ctx);
            return;
        }
        _ => return,
    }

    // At this point, expr.kind is TemplateString — expand it
    let span = expr.span;
    let string_type = expr.type_id;
    let parts = if let TirExprKind::TemplateString { parts } = std::mem::replace(
        &mut expr.kind,
        TirExprKind::Unit, // temporary placeholder
    ) {
        parts
    } else {
        unreachable!();
    };

    let expanded = build_template_block(parts, string_type, span, alloc, ctx);
    *expr = expanded;
}

/// Build the `__tmpl: { ... }` labeled block for a template string.
fn build_template_block(
    parts: Vec<TirTemplatePart>,
    string_type: TypeId,
    span: Span,
    alloc: &mut FuncLocalAlloc,
    ctx: &TemplateCtx,
) -> TirExpr {
    let tt = ctx.tt;
    let string_struct_name = tt
        .borrow()
        .compiler_items()
        .struct_name(crate::compiler_item::CompilerItem::String)
        .to_string();
    let with_capacity_qualified =
        crate::name::MethodName::format_local(&string_struct_name, None, "with_capacity");
    let label = "__tmpl".to_string();

    // Estimate capacity: sum of literal lengths + 16 per interpolation
    let capacity_estimate: i64 = parts
        .iter()
        .map(|p| match p {
            TirTemplatePart::Literal(s) => s.len() as i64,
            TirTemplatePart::Interpolation { .. } => 16,
        })
        .sum();

    let buf_index = alloc.alloc(string_type);

    // let mut __r = String::with_capacity(N);
    let with_capacity_call = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::string(),
                name: with_capacity_qualified,
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    string_struct_name.clone(),
                    None,
                    "with_capacity".to_string(),
                )),
            },
            type_args: vec![],
            args: vec![CallArg::new(
                TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: capacity_estimate.cast_unsigned(),
                        repr: capacity_estimate.to_string(),
                    },
                    TypeTable::I32,
                    span,
                ),
                false,
            )],
        },
        string_type,
        span,
    );
    let mut stmts = vec![TirStmt::new(
        TirStmtKind::Let {
            name: "__r".to_string(),
            local_index: buf_index,
            is_mut: true,
            is_reactive: false,
            type_id: string_type,
            value: with_capacity_call,
            skip_value_copy: false,
        },
        span,
    )];

    let formatter_type = tt
        .borrow_mut()
        .make_struct(ctx.names.formatter.clone(), ModuleSource::format());
    let mut_ref_formatter = tt.borrow_mut().make_mut_ref(formatter_type);
    let ref_string_type = tt.borrow_mut().make_ref(string_type);
    let mut fmt_local_index: Option<u32> = None;

    for part in parts {
        match part {
            TirTemplatePart::Literal(s) => {
                let buf_ref = TirExpr::new(
                    TirExprKind::Local {
                        index: buf_index,
                        name: "__r".to_string(),
                    },
                    string_type,
                    span,
                );
                let push_str_qualified =
                    crate::name::MethodName::format_local(&string_struct_name, None, "push_str");
                let literal_ref = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Ref,
                        expr: Box::new(TirExpr::new(
                            TirExprKind::StringLiteral(s),
                            string_type,
                            span,
                        )),
                    },
                    ref_string_type,
                    span,
                );
                let push_str_call = TirExpr::new(
                    TirExprKind::method_call(
                        Box::new(buf_ref),
                        FunctionRef {
                            module_source: ModuleSource::string(),
                            name: push_str_qualified,
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                string_struct_name.clone(),
                                None,
                                "push_str".to_string(),
                            )),
                        },
                        vec![],
                        vec![CallArg::new(literal_ref, false)],
                    ),
                    TypeTable::UNIT,
                    span,
                );
                stmts.push(TirStmt::new(TirStmtKind::Expr(push_str_call), span));
            }
            TirTemplatePart::Interpolation {
                expr: resolved,
                format_spec,
            } => {
                // Strip refs for type-based decisions
                let inner_type = strip_refs(resolved.type_id, tt);

                // If String type (or &String, &&String, ...) with no format spec, just push_str directly
                if inner_type == string_type && format_spec.is_none() {
                    let buf_ref = TirExpr::new(
                        TirExprKind::Local {
                            index: buf_index,
                            name: "__r".to_string(),
                        },
                        string_type,
                        span,
                    );
                    // Normalize to &String regardless of ref level
                    let derefed = deref_to_inner(*resolved, string_type, span);
                    let arg_ref = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(derefed),
                        },
                        ref_string_type,
                        span,
                    );
                    let push_str_qualified = crate::name::MethodName::format_local(
                        &string_struct_name,
                        None,
                        "push_str",
                    );
                    let push_str_call = TirExpr::new(
                        TirExprKind::method_call(
                            Box::new(buf_ref),
                            FunctionRef {
                                module_source: ModuleSource::string(),
                                name: push_str_qualified,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    string_struct_name.clone(),
                                    None,
                                    "push_str".to_string(),
                                )),
                            },
                            vec![],
                            vec![CallArg::new(arg_ref, false)],
                        ),
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(push_str_call), span));
                    continue;
                }

                let is_inspect = format_spec
                    .as_ref()
                    .is_some_and(|fs| fs.type_char == Some('?'));
                let is_alternate = format_spec.as_ref().is_some_and(|fs| fs.alternate);

                let (trait_name, method_name): (&str, &str) = match &format_spec {
                    Some(fs) => match (fs.type_char, fs.alternate) {
                        (Some('b'), true) => (
                            ctx.names.binary_alt.as_str(),
                            ctx.names.binary_alt_method.as_str(),
                        ),
                        (Some('b'), false) => {
                            (ctx.names.binary.as_str(), ctx.names.binary_method.as_str())
                        }
                        (Some('o'), true) => (
                            ctx.names.octal_alt.as_str(),
                            ctx.names.octal_alt_method.as_str(),
                        ),
                        (Some('o'), false) => {
                            (ctx.names.octal.as_str(), ctx.names.octal_method.as_str())
                        }
                        (Some('x'), true) => (
                            ctx.names.lower_hex_alt.as_str(),
                            ctx.names.lower_hex_alt_method.as_str(),
                        ),
                        (Some('x'), false) => (
                            ctx.names.lower_hex.as_str(),
                            ctx.names.lower_hex_method.as_str(),
                        ),
                        (Some('X'), true) => (
                            ctx.names.upper_hex_alt.as_str(),
                            ctx.names.upper_hex_alt_method.as_str(),
                        ),
                        (Some('X'), false) => (
                            ctx.names.upper_hex.as_str(),
                            ctx.names.upper_hex_method.as_str(),
                        ),
                        (Some('e'), _) => (
                            ctx.names.lower_exp.as_str(),
                            ctx.names.lower_exp_method.as_str(),
                        ),
                        (Some('E'), _) => (
                            ctx.names.upper_exp.as_str(),
                            ctx.names.upper_exp_method.as_str(),
                        ),
                        (_, true) => (
                            ctx.names.display_alt.as_str(),
                            ctx.names.display_alt_method.as_str(),
                        ),
                        _ => (
                            ctx.names.display.as_str(),
                            ctx.names.display_method.as_str(),
                        ),
                    },
                    None => (
                        ctx.names.display.as_str(),
                        ctx.names.display_method.as_str(),
                    ),
                };

                // Create or reassign Formatter local
                let fmt_index = if let Some(idx) = fmt_local_index {
                    let formatter_expr = build_formatter_expr(
                        buf_index,
                        string_type,
                        formatter_type,
                        &format_spec,
                        span,
                        tt,
                        ctx.names,
                    );
                    let assign = TirExpr::new(
                        TirExprKind::Assign {
                            target: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: idx,
                                    name: "__f".to_string(),
                                },
                                formatter_type,
                                span,
                            )),
                            value: Box::new(formatter_expr),
                        },
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(assign), span));
                    idx
                } else {
                    let idx = alloc.alloc(formatter_type);
                    fmt_local_index = Some(idx);
                    let formatter_expr = build_formatter_expr(
                        buf_index,
                        string_type,
                        formatter_type,
                        &format_spec,
                        span,
                        tt,
                        ctx.names,
                    );
                    stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name: "__f".to_string(),
                            local_index: idx,
                            is_mut: true,
                            is_reactive: false,
                            type_id: formatter_type,
                            value: formatter_expr,
                            skip_value_copy: false,
                        },
                        span,
                    ));
                    idx
                };

                let fmt_mut_ref = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::MutRef,
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
                                index: fmt_index,
                                name: "__f".to_string(),
                            },
                            formatter_type,
                            span,
                        )),
                    },
                    mut_ref_formatter,
                    span,
                );

                if is_inspect {
                    let (it_name, im_name): (&str, &str) = if is_alternate {
                        (
                            ctx.names.inspect_alt.as_str(),
                            ctx.names.inspect_alt_method.as_str(),
                        )
                    } else {
                        (
                            ctx.names.inspect.as_str(),
                            ctx.names.inspect_method.as_str(),
                        )
                    };
                    let call_stmts = trait_fmt_call(
                        resolved.type_id,
                        *resolved,
                        fmt_mut_ref,
                        it_name,
                        im_name,
                        span,
                        ctx,
                    );
                    stmts.extend(call_stmts);
                } else {
                    // Display/DisplayAlt/Binary/BinaryAlt/etc.
                    let call_stmts = trait_fmt_call(
                        inner_type,
                        *resolved,
                        fmt_mut_ref,
                        trait_name,
                        method_name,
                        span,
                        ctx,
                    );
                    stmts.extend(call_stmts);
                }
            }
        }
    }

    // break __tmpl: __r;
    let buf_final = TirExpr::new(
        TirExprKind::Local {
            index: buf_index,
            name: "__r".to_string(),
        },
        string_type,
        span,
    );
    stmts.push(TirStmt::new(
        TirStmtKind::Break {
            label: Some(label.clone()),
            value: Some(buf_final),
        },
        span,
    ));

    TirExpr::new(
        TirExprKind::LabeledBlock {
            label,
            block: TirBlock::new(stmts, span),
            result_type: string_type,
        },
        string_type,
        span,
    )
}

/// Build a `Formatter::new(&mut __r)` or `Formatter { ... }` expression.
fn build_formatter_expr(
    buf_index: u32,
    string_type: TypeId,
    formatter_type: TypeId,
    parsed: &Option<TemplateFormatSpec>,
    span: Span,
    tt: &Rc<RefCell<TypeTable>>,
    names: &FormatStdlibNames,
) -> TirExpr {
    let mut_ref_string = tt.borrow_mut().make_mut_ref(string_type);
    let buf_mut_ref = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: buf_index,
                    name: "__r".to_string(),
                },
                string_type,
                span,
            )),
        },
        mut_ref_string,
        span,
    );

    let has_custom_spec = parsed.as_ref().is_some_and(|p| {
        p.fill.is_some()
            || p.align.is_some()
            || p.sign_plus
            || p.zero_pad
            || p.width.is_some()
            || p.precision.is_some()
    });

    if !has_custom_spec {
        return TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::format(),
                    name: format!("{}::new", names.formatter),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        names.formatter.clone(),
                        None,
                        "new".to_string(),
                    )),
                },
                type_args: vec![],
                args: vec![CallArg::new(buf_mut_ref, false)],
            },
            formatter_type,
            span,
        );
    }

    let pf = parsed.as_ref().unwrap();
    let alignment_type = tt
        .borrow_mut()
        .make_enum(names.alignment.clone(), ModuleSource::format());
    let fill_char = pf.fill.unwrap_or(if pf.zero_pad { '0' } else { ' ' });
    let (align_index, align_name): (u32, &str) = match pf.align {
        Some('<') => (names.left_index, names.left_name.as_str()),
        Some('^') => (names.center_index, names.center_name.as_str()),
        _ => (names.right_index, names.right_name.as_str()),
    };

    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: formatter_type,
            struct_name: names.formatter.clone(),
            fields: vec![
                TirStructField {
                    name: "fill".to_string(),
                    value: TirExpr::new(TirExprKind::CharLiteral(fill_char), TypeTable::CHAR, span),
                    field_index: 0,
                },
                TirStructField {
                    name: "align".to_string(),
                    value: TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type: alignment_type,
                            case_index: align_index,
                            case_name: align_name.to_string(),
                        },
                        alignment_type,
                        span,
                    ),
                    field_index: 1,
                },
                TirStructField {
                    name: "sign_plus".to_string(),
                    value: TirExpr::new(
                        TirExprKind::BoolLiteral(pf.sign_plus),
                        TypeTable::BOOL,
                        span,
                    ),
                    field_index: 2,
                },
                TirStructField {
                    name: "zero_pad".to_string(),
                    value: TirExpr::new(
                        TirExprKind::BoolLiteral(pf.zero_pad),
                        TypeTable::BOOL,
                        span,
                    ),
                    field_index: 3,
                },
                TirStructField {
                    name: "width".to_string(),
                    value: TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: pf.width.unwrap_or(-1).cast_unsigned(),
                            repr: pf.width.unwrap_or(-1).to_string(),
                        },
                        TypeTable::I32,
                        span,
                    ),
                    field_index: 4,
                },
                TirStructField {
                    name: "precision".to_string(),
                    value: TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: pf.precision.unwrap_or(-1).cast_unsigned(),
                            repr: pf.precision.unwrap_or(-1).to_string(),
                        },
                        TypeTable::I32,
                        span,
                    ),
                    field_index: 5,
                },
                TirStructField {
                    name: "indent".to_string(),
                    value: TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: "0".to_string(),
                        },
                        TypeTable::I32,
                        span,
                    ),
                    field_index: 6,
                },
                TirStructField {
                    name: "buf".to_string(),
                    value: buf_mut_ref,
                    field_index: 7,
                },
            ],
        },
        formatter_type,
        span,
    )
}

/// Strip all `Ref` and `MutRef` wrappers from a type, returning the inner type.
fn strip_refs(type_id: TypeId, tt: &Rc<RefCell<TypeTable>>) -> TypeId {
    let mut current = type_id;
    loop {
        match tt.borrow().get(current).clone() {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => current = inner,
            _ => return current,
        }
    }
}

/// Wrap `expr` in deref operations until its type matches `target_type`.
/// If `expr.type_id` is already `target_type`, returns `expr` unchanged.
fn deref_to_inner(expr: TirExpr, target_type: TypeId, span: Span) -> TirExpr {
    if expr.type_id == target_type {
        return expr;
    }
    // Just wrap in a single Deref — the lower phase handles multi-layer deref
    TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(expr),
        },
        target_type,
        span,
    )
}

/// Unified format trait dispatch.
///
/// Emits a trait method call, delegating to the Wado-level trait implementation
/// (including blanket impls).
fn trait_fmt_call(
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    trait_name: &str,
    method_name: &str,
    span: Span,
    ctx: &TemplateCtx,
) -> Vec<TirStmt> {
    let MethodCallInfo {
        local_name,
        monomorph_info,
        impl_module,
    } = method_call_info_for_type(type_id, trait_name, method_name, ctx);
    let mangled = local_name.to_mangled_name();

    let ref_type = ctx.tt.borrow_mut().make_ref(type_id);
    let receiver = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(val),
        },
        ref_type,
        span,
    );

    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: impl_module,
                name: mangled,
                monomorph_info,
                method_info: Some(local_name),
            },
            vec![],
            vec![CallArg::new(fmt, false)],
        ),
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(call), span)]
}

/// All information needed to build a `FunctionRef` for a trait method call on a given type.
struct MethodCallInfo {
    local_name: LocalMethodName,
    monomorph_info: Option<MonomorphInfo>,
    impl_module: ModuleSource,
}

/// Build `MethodCallInfo` for a trait method call on `type_id`.
///
/// Combines name mangling, module resolution, and monomorphization metadata into
/// one place. For `Ref(T)` / `MutRef(T)`, this produces the `MonomorphInfo` needed
/// to instantiate the generic blanket impl (`impl<T: Trait> Trait for &T`) — no
/// type-specific logic is needed at the call site.
fn method_call_info_for_type(
    type_id: TypeId,
    trait_name: &str,
    method_name: &str,
    ctx: &TemplateCtx,
) -> MethodCallInfo {
    let tt = ctx.tt;
    let resolved = tt.borrow().get(type_id).clone();
    match resolved {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            let struct_name = if matches!(tt.borrow().get(type_id).clone(), ResolvedType::Ref(_)) {
                "&"
            } else {
                "&mut"
            };
            let inner_name = tt.borrow().mangle_type_name(inner);
            let local_name = LocalMethodName::new(
                struct_name.to_string(),
                Some(trait_name.to_string()),
                method_name.to_string(),
            )
            .with_struct_type_args(&[inner_name]);
            let generic_name = LocalMethodName::new(
                struct_name.to_string(),
                Some(trait_name.to_string()),
                method_name.to_string(),
            )
            .to_mangled_name();
            MethodCallInfo {
                local_name,
                monomorph_info: Some(MonomorphInfo {
                    generic_name,
                    impl_type_args: vec![inner],
                    method_type_args: vec![],
                    is_blanket: true,
                }),
                impl_module: ModuleSource::format(),
            }
        }
        _ => {
            let local_name = method_name_for_type(type_id, trait_name, method_name, tt);
            let impl_module = trait_impl_module(&local_name, type_id, ctx);
            MethodCallInfo {
                local_name,
                monomorph_info: None,
                impl_module,
            }
        }
    }
}

/// Build a `LocalMethodName` for a concrete (post-mono) type.
///
/// Extracts the base name from the resolved type and applies type args
/// for parameterized types like `GenericInstance`, etc.
fn method_name_for_type(
    type_id: TypeId,
    trait_name: &str,
    method_name: &str,
    tt: &Rc<RefCell<TypeTable>>,
) -> LocalMethodName {
    let tt_ref = tt.borrow();
    let resolved = tt_ref.get(type_id).clone();
    match resolved {
        ResolvedType::TypeParam { ref name, .. } | ResolvedType::TypePack { ref name, .. } => {
            let mut info = LocalMethodName::new(
                name.clone(),
                Some(trait_name.to_string()),
                method_name.to_string(),
            );
            info.is_type_param_receiver = true;
            info
        }
        ResolvedType::GenericInstance {
            name, type_args, ..
        } => {
            let arg_names: Vec<String> = type_args
                .iter()
                .map(|t| tt_ref.mangle_type_name(*t))
                .collect();
            LocalMethodName::new(name, Some(trait_name.to_string()), method_name.to_string())
                .with_struct_type_args(&arg_names)
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            // Match the mangling used by `synthesis/traits::generate_fn_inspect_fn`:
            // base struct is `Fn`, type args are `[<arity>, <return-type-mangled>]`.
            // Without this arm, the `_` fallback below would call
            // `LocalMethodName::new("Fn<N,Ret>", ...)` whose debug_assert
            // rejects struct names containing `<`.
            let arity = params.len().to_string();
            let ret_name = tt_ref.mangle_type_name(return_type);
            LocalMethodName::new(
                crate::name::CLOSURE_FN_TRAIT.to_string(),
                Some(trait_name.to_string()),
                method_name.to_string(),
            )
            .with_struct_type_args(&[arity, ret_name])
        }
        _ => {
            let name = tt_ref.mangle_type_name(type_id);
            LocalMethodName::new(name, Some(trait_name.to_string()), method_name.to_string())
        }
    }
}

fn trait_impl_module(
    local_name: &LocalMethodName,
    type_id: TypeId,
    ctx: &TemplateCtx,
) -> ModuleSource {
    // The receiver's own module is the disambiguation hint: when two
    // same-named structs from different modules each auto-derive an impl
    // (e.g. `struct Widget` in module A and module B both get a
    // `Widget^Inspect`), `TraitEnv::impl_module_for` would otherwise return
    // whichever module landed first in iteration order. Passing the type's
    // module lets the lookup pick the candidate that actually corresponds
    // to this `type_id`.
    let resolved = ctx.tt.borrow().get(type_id).clone();
    let type_module = match &resolved {
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::Enum { module_source, .. }
        | ResolvedType::Variant { module_source, .. }
        | ResolvedType::Newtype { module_source, .. }
        | ResolvedType::Flags { module_source, .. }
        | ResolvedType::GenericInstance { module_source, .. }
        | ResolvedType::GenericResource { module_source, .. } => Some(module_source.clone()),
        _ => None,
    };

    // Preferred path: consult the resolver's `TraitEnv`, which knows where
    // every user-written `impl Trait for Type` block lives. This handles
    // cross-module impls like `impl Display for String` (defined in
    // `core:prelude/format`, not the module that declares `String`).
    if let Some(trait_name) = local_name
        .base_trait_name
        .as_deref()
        .or(local_name.trait_name.as_deref())
        && let Some(loc) = ctx.trait_env.impl_module_for(
            &local_name.base_struct_name,
            trait_name,
            type_module.as_ref(),
        )
    {
        return loc.clone();
    }
    // Fallbacks for impls `TraitEnv` cannot index:
    //
    // - Auto-derived/synthesized impls (Inspect / Display fallbacks for
    //   structs, enums, variants, newtypes, flags). `synthesize_traits`
    //   places these in the same module as the receiver type, so the
    //   type's `module_source` is correct.
    // - Function types are anonymous and have no defining module, so
    //   their `Fn<N, Ret>^Inspect` / `^InspectAlt` impls are auto-derived
    //   per-module (no cross-module dedup, since
    //   `collect_existing_trait_methods` is per-module). After `link()`
    //   every function's `module_source` is rewritten to its hosting
    //   module, so the impl callable from this template lives under the
    //   current module's namespace.
    if let Some(m) = type_module {
        return m;
    }
    match resolved {
        ResolvedType::Primitive(_) | ResolvedType::Unit => ModuleSource::primitive(),
        ResolvedType::Function { .. } => ctx.module_src.clone(),
        _ => ModuleSource::primitive(),
    }
}
