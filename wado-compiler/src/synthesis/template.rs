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

use crate::name::{LocalMethodName, ModuleSource};
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, TemplateFormatSpec, TirBlock, TirExpr,
    TirExprKind, TirLocal, TirModule, TirStmt, TirStmtKind, TirStructField, TirTemplatePart,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Expand all `TemplateString` nodes in a module.
///
/// Runs as part of the pre-mono synthesis phase. Template expansion emits
/// trait method calls (`Display::fmt`, `Inspect::inspect`) that the monomorphizer
/// subsequently resolves to concrete implementations.
pub fn expand_templates(module: &mut TirModule, tt: &Rc<RefCell<TypeTable>>) {
    let module_src = module.module_source.clone();
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        let local_count = func.local_count;
        if let Some(ref mut body) = func.body {
            let mut alloc = FuncLocalAlloc {
                next_index: local_count,
                new_locals: Vec::new(),
            };
            expand_block(body, tt, &mut alloc, &module_src);
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
                expand_block(body, tt, &mut alloc, &module_src);
                method.local_count = alloc.next_index;
                method.locals.extend(alloc.new_locals);
            }
        }
    }
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

fn expand_block(
    block: &mut TirBlock,
    tt: &Rc<RefCell<TypeTable>>,
    alloc: &mut FuncLocalAlloc,
    module_src: &ModuleSource,
) {
    for stmt in &mut block.stmts {
        expand_stmt(stmt, tt, alloc, module_src);
    }
}

fn expand_stmt(
    stmt: &mut TirStmt,
    tt: &Rc<RefCell<TypeTable>>,
    alloc: &mut FuncLocalAlloc,
    module_src: &ModuleSource,
) {
    match &mut stmt.kind {
        TirStmtKind::Expr(e) => expand_expr(e, tt, alloc, module_src),
        TirStmtKind::Let { value, .. } => {
            expand_expr(value, tt, alloc, module_src);
        }
        TirStmtKind::Return { value: Some(e) } | TirStmtKind::Break { value: Some(e), .. } => {
            expand_expr(e, tt, alloc, module_src);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expand_expr(condition, tt, alloc, module_src);
            expand_block(then_block, tt, alloc, module_src);
            if let Some(eb) = else_block {
                expand_block(eb, tt, alloc, module_src);
            }
        }
        TirStmtKind::Loop { body } => {
            expand_block(body, tt, alloc, module_src);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expand_expr(scrutinee, tt, alloc, module_src);
            expand_block(then_block, tt, alloc, module_src);
            if let Some(eb) = else_block {
                expand_block(eb, tt, alloc, module_src);
            }
        }
        TirStmtKind::LetDestructure { value, .. } | TirStmtKind::TaskReturn { value, .. } => {
            expand_expr(value, tt, alloc, module_src);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            expand_block(block, tt, alloc, module_src);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            expand_expr(iterable, tt, alloc, module_src);
            expand_block(body, tt, alloc, module_src);
        }
        _ => {}
    }
}

fn expand_expr(
    expr: &mut TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    alloc: &mut FuncLocalAlloc,
    module_src: &ModuleSource,
) {
    // First, recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::TemplateString { parts } => {
            // Expand sub-expressions within template parts first
            for part in parts.iter_mut() {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    expand_expr(inner, tt, alloc, module_src);
                }
            }
        }
        TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } => {
            expand_block(b, tt, alloc, module_src);
            return;
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expand_expr(condition, tt, alloc, module_src);
            expand_block(then_branch, tt, alloc, module_src);
            if let Some(eb) = else_branch {
                expand_block(eb, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::Match { expr: s, arms } => {
            expand_expr(s, tt, alloc, module_src);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    expand_expr(guard, tt, alloc, module_src);
                }
                expand_expr(&mut arm.body, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::Call { args, .. } => {
            for a in args {
                expand_expr(&mut a.expr, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            expand_expr(receiver, tt, alloc, module_src);
            for a in args {
                expand_expr(&mut a.expr, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::Binary { left, right, .. } => {
            expand_expr(left, tt, alloc, module_src);
            expand_expr(right, tt, alloc, module_src);
            return;
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            expand_expr(inner, tt, alloc, module_src);
            return;
        }
        TirExprKind::Assign { target, value } => {
            expand_expr(target, tt, alloc, module_src);
            expand_expr(value, tt, alloc, module_src);
            return;
        }
        TirExprKind::Index {
            expr: e,
            index: idx,
        } => {
            expand_expr(e, tt, alloc, module_src);
            expand_expr(idx, tt, alloc, module_src);
            return;
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                expand_expr(&mut f.value, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::TupleLiteral { elements } => {
            for e in elements {
                expand_expr(e, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::Closure { body, .. } => {
            expand_expr(body, tt, alloc, module_src);
            return;
        }
        TirExprKind::IndirectCall { callee, args } => {
            expand_expr(callee, tt, alloc, module_src);
            for a in args {
                expand_expr(a, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::CmRawCall { args, .. } => {
            for a in args {
                expand_expr(a, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                expand_expr(p, tt, alloc, module_src);
            }
            return;
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            expand_expr(value, tt, alloc, module_src);
            return;
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            expand_expr(functor, tt, alloc, module_src);
            return;
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expand_expr(scrutinee, tt, alloc, module_src);
            for arm in arms {
                expand_block(arm, tt, alloc, module_src);
            }
            expand_block(default, tt, alloc, module_src);
            return;
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                expand_expr(&mut binding.handler, tt, alloc, module_src);
            }
            expand_block(body, tt, alloc, module_src);
            return;
        }
        TirExprKind::Resume { value } => {
            expand_expr(value, tt, alloc, module_src);
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

    let expanded = build_template_block(parts, string_type, span, tt, alloc, module_src);
    *expr = expanded;
}

/// Build the `__tmpl: { ... }` labeled block for a template string.
fn build_template_block(
    parts: Vec<TirTemplatePart>,
    string_type: TypeId,
    span: Span,
    tt: &Rc<RefCell<TypeTable>>,
    alloc: &mut FuncLocalAlloc,
    module_src: &ModuleSource,
) -> TirExpr {
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
                name: "String::with_capacity".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "String".to_string(),
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
        .make_struct("Formatter".to_string(), ModuleSource::format());
    let mut_ref_formatter = tt.borrow_mut().make_mut_ref(formatter_type);
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
                let push_str_call = TirExpr::new(
                    TirExprKind::method_call(
                        Box::new(buf_ref),
                        FunctionRef {
                            module_source: ModuleSource::string(),
                            name: "String::push_str".to_string(),
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                "String".to_string(),
                                None,
                                "push_str".to_string(),
                            )),
                        },
                        vec![],
                        vec![CallArg::new(
                            TirExpr::new(TirExprKind::StringLiteral(s), string_type, span),
                            false,
                        )],
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
                    // Deref if needed (&String → String)
                    let derefed = deref_to_inner(*resolved, string_type, span);
                    let push_str_call = TirExpr::new(
                        TirExprKind::method_call(
                            Box::new(buf_ref),
                            FunctionRef {
                                module_source: ModuleSource::string(),
                                name: "String::push_str".to_string(),
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    "String".to_string(),
                                    None,
                                    "push_str".to_string(),
                                )),
                            },
                            vec![],
                            vec![CallArg::new(derefed, false)],
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

                let (trait_name, method_name) = match &format_spec {
                    Some(fs) => match (fs.type_char, fs.alternate) {
                        (Some('b'), true) => ("BinaryAlt", "fmt_alt"),
                        (Some('b'), false) => ("Binary", "fmt"),
                        (Some('o'), true) => ("OctalAlt", "fmt_alt"),
                        (Some('o'), false) => ("Octal", "fmt"),
                        (Some('x'), true) => ("LowerHexAlt", "fmt_alt"),
                        (Some('x'), false) => ("LowerHex", "fmt"),
                        (Some('X'), true) => ("UpperHexAlt", "fmt_alt"),
                        (Some('X'), false) => ("UpperHex", "fmt"),
                        (Some('e'), _) => ("LowerExp", "fmt"),
                        (Some('E'), _) => ("UpperExp", "fmt"),
                        (_, true) => ("DisplayAlt", "fmt_alt"),
                        _ => ("Display", "fmt"),
                    },
                    None => ("Display", "fmt"),
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
                    let (it_name, im_name) = if is_alternate {
                        ("InspectAlt", "inspect_alt")
                    } else {
                        ("Inspect", "inspect")
                    };
                    let call_stmts = trait_fmt_call(
                        resolved.type_id,
                        *resolved,
                        fmt_mut_ref,
                        it_name,
                        im_name,
                        tt,
                        span,
                        module_src,
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
                        tt,
                        span,
                        module_src,
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
                    name: "Formatter::new".to_string(),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        "Formatter".to_string(),
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
        .make_enum("Alignment".to_string(), ModuleSource::format());
    let fill_char = pf.fill.unwrap_or(if pf.zero_pad { '0' } else { ' ' });
    let align_index: u32 = match pf.align {
        Some('<') => 0,
        Some('^') => 1,
        _ => 2,
    };
    let align_name = match align_index {
        0 => "Left",
        1 => "Center",
        _ => "Right",
    };

    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: formatter_type,
            struct_name: "Formatter".to_string(),
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
/// Handles ALL type kinds for both Display and Inspect format traits:
/// - `Tuple`: writes `[elem1, elem2, ...]` with per-element Inspect recursion.
/// - `Function`: writes the type signature `|params| -> ret` (Inspect only).
/// - All other types (including `Unit`, `Ref(T)` / `MutRef(T)`): emits a trait method call,
///   delegating to the Wado-level trait implementation (including blanket impls).
fn trait_fmt_call(
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    trait_name: &str,
    method_name: &str,
    tt: &Rc<RefCell<TypeTable>>,
    span: Span,
    module_src: &ModuleSource,
) -> Vec<TirStmt> {
    let resolved = tt.borrow().get(type_id).clone();
    match resolved {
        ResolvedType::GenericInstance {
            ref name,
            ref type_args,
            ref module_source,
        } if TypeTable::is_tuple_type(name, module_source) => {
            let elements = type_args.clone();
            let mut stmts = Vec::new();
            stmts.push(write_str_stmt("[", fmt.clone(), tt, span));
            for (i, elem_type) in elements.iter().enumerate() {
                if i > 0 {
                    stmts.push(write_str_stmt(", ", fmt.clone(), tt, span));
                }
                let field_access = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(val.clone()),
                        field_index: i as u32,
                        field_name: i.to_string(),
                    },
                    *elem_type,
                    span,
                );
                stmts.extend(trait_fmt_call(
                    *elem_type,
                    field_access,
                    fmt.clone(),
                    "Inspect",
                    "inspect",
                    tt,
                    span,
                    module_src,
                ));
            }
            stmts.push(write_str_stmt("]", fmt, tt, span));
            stmts
        }
        _ => {
            let MethodCallInfo {
                local_name,
                monomorph_info,
                impl_module,
            } = method_call_info_for_type(type_id, trait_name, method_name, tt, module_src);
            let mangled = local_name.to_mangled_name();

            let ref_type = tt.borrow_mut().make_ref(type_id);
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
    }
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
    tt: &Rc<RefCell<TypeTable>>,
    module_src: &ModuleSource,
) -> MethodCallInfo {
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
        _ => MethodCallInfo {
            local_name: method_name_for_type(type_id, trait_name, method_name, tt),
            monomorph_info: None,
            impl_module: trait_impl_module(type_id, trait_name, tt, module_src),
        },
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
                "Fn".to_string(),
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
    type_id: TypeId,
    _trait_name: &str,
    tt: &Rc<RefCell<TypeTable>>,
    module_src: &ModuleSource,
) -> ModuleSource {
    match tt.borrow().get(type_id).clone() {
        ResolvedType::Primitive(_) => ModuleSource::primitive(),
        ResolvedType::Struct { name, .. } if name == "String" => ModuleSource::format(),
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::Enum { module_source, .. }
        | ResolvedType::Variant { module_source, .. }
        | ResolvedType::Newtype { module_source, .. }
        | ResolvedType::Flags { module_source, .. } => module_source,
        // Function types are anonymous: their `Fn<N, Ret>^Inspect` /
        // `Fn<N, Ret>^InspectAlt` impls are auto-derived per-module by
        // `synthesize_traits` (no cross-module dedup, since
        // `collect_existing_trait_methods` is per-module). After `link()`
        // every function's `module_source` is rewritten to its hosting
        // module, so the impl callable from this template lives under
        // the current module's namespace.
        ResolvedType::Function { .. } => module_src.clone(),
        _ => ModuleSource::primitive(),
    }
}

/// Build a `f.write_str("text")` statement using the Formatter's `write_str` method.
fn write_str_stmt(text: &str, fmt: TirExpr, tt: &Rc<RefCell<TypeTable>>, span: Span) -> TirStmt {
    let string_type = tt
        .borrow_mut()
        .make_struct("String".to_string(), ModuleSource::string());
    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(fmt),
            FunctionRef {
                module_source: ModuleSource::format(),
                name: "Formatter::write_str".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "Formatter".into(),
                    None,
                    "write_str".into(),
                )),
            },
            vec![],
            vec![CallArg::new(
                TirExpr::new(
                    TirExprKind::StringLiteral(text.to_string()),
                    string_type,
                    span,
                ),
                false,
            )],
        ),
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}
