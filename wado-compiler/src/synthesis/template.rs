//! Template string expansion synthesis phase.
//!
//! Expands `TirExprKind::TemplateString` nodes into concrete formatting code:
//! a `__tmpl` labeled block containing `String::with_capacity`, `append` calls,
//! `Formatter` construction, and `Display`/inspect dispatch.
//!
//! Pipeline position: after monomorphize, before lower.
//! Running after monomorphization ensures that all type parameters have been
//! substituted with concrete types, enabling correct Display vs Inspect dispatch
//! without markers or deferred resolution.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TemplateFormatSpec, TirBlock, TirExpr, TirExprKind,
    TirModule, TirStmt, TirStmtKind, TirStructField, TirTemplatePart, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::token::Span;

use super::inspect::InspectRegistry;

/// Expand all `TemplateString` nodes in the project.
///
/// This must run after monomorphization so all types are concrete.
/// It replaces each `TemplateString` expression with an expanded `__tmpl`
/// labeled block that builds the formatted string.
pub fn expand_templates(
    module: &TirModule,
    tt: &Rc<RefCell<TypeTable>>,
    all_modules: &[&TirModule],
    inspect_reg: &mut InspectRegistry,
    fmt_type: TypeId,
    module_source: &ModuleSource,
) {
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        let local_count = func.local_count;
        if let Some(ref mut body) = func.body {
            let closure_sources = super::inspect::collect_closure_sources(body);
            let mut alloc = FuncLocalAlloc {
                next_index: local_count,
                new_types: Vec::new(),
            };
            expand_block(
                body,
                tt,
                all_modules,
                inspect_reg,
                fmt_type,
                module_source,
                &mut alloc,
                &closure_sources,
            );
            func.local_count = alloc.next_index;
            func.local_types.extend(alloc.new_types);
        }
    }
}

struct FuncLocalAlloc {
    next_index: u32,
    new_types: Vec<TypeId>,
}

impl FuncLocalAlloc {
    fn alloc(&mut self, type_id: TypeId) -> u32 {
        let idx = self.next_index;
        self.next_index += 1;
        self.new_types.push(type_id);
        idx
    }
}

fn expand_block(
    block: &mut TirBlock,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    reg: &mut InspectRegistry,
    fmt_type: TypeId,
    ms: &ModuleSource,
    alloc: &mut FuncLocalAlloc,
    cs: &IndexMap<u32, String>,
) {
    for stmt in &mut block.stmts {
        expand_stmt(stmt, tt, mods, reg, fmt_type, ms, alloc, cs);
    }
}

fn expand_stmt(
    stmt: &mut TirStmt,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    reg: &mut InspectRegistry,
    fmt_type: TypeId,
    ms: &ModuleSource,
    alloc: &mut FuncLocalAlloc,
    cs: &IndexMap<u32, String>,
) {
    match &mut stmt.kind {
        TirStmtKind::Expr(e) => expand_expr(e, tt, mods, reg, fmt_type, ms, alloc, cs),
        TirStmtKind::Let { value, .. } => {
            expand_expr(value, tt, mods, reg, fmt_type, ms, alloc, cs);
        }
        TirStmtKind::Return { value: Some(e) } | TirStmtKind::Break { value: Some(e), .. } => {
            expand_expr(e, tt, mods, reg, fmt_type, ms, alloc, cs);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expand_expr(condition, tt, mods, reg, fmt_type, ms, alloc, cs);
            expand_block(then_block, tt, mods, reg, fmt_type, ms, alloc, cs);
            if let Some(eb) = else_block {
                expand_block(eb, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
        }
        TirStmtKind::Loop { body } => {
            expand_block(body, tt, mods, reg, fmt_type, ms, alloc, cs);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expand_expr(scrutinee, tt, mods, reg, fmt_type, ms, alloc, cs);
            expand_block(then_block, tt, mods, reg, fmt_type, ms, alloc, cs);
            if let Some(eb) = else_block {
                expand_block(eb, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
        }
        TirStmtKind::LetPattern { value, .. } | TirStmtKind::TaskReturn { value, .. } => {
            expand_expr(value, tt, mods, reg, fmt_type, ms, alloc, cs);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            expand_block(block, tt, mods, reg, fmt_type, ms, alloc, cs);
        }
        _ => {}
    }
}

fn expand_expr(
    expr: &mut TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    reg: &mut InspectRegistry,
    fmt_type: TypeId,
    ms: &ModuleSource,
    alloc: &mut FuncLocalAlloc,
    cs: &IndexMap<u32, String>,
) {
    // First, recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::TemplateString { parts } => {
            // Expand sub-expressions within template parts first
            for part in parts.iter_mut() {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    expand_expr(inner, tt, mods, reg, fmt_type, ms, alloc, cs);
                }
            }
        }
        TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } => {
            expand_block(b, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expand_expr(condition, tt, mods, reg, fmt_type, ms, alloc, cs);
            expand_block(then_branch, tt, mods, reg, fmt_type, ms, alloc, cs);
            if let Some(eb) = else_branch {
                expand_block(eb, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::Match { expr: s, arms } => {
            expand_expr(s, tt, mods, reg, fmt_type, ms, alloc, cs);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    expand_expr(guard, tt, mods, reg, fmt_type, ms, alloc, cs);
                }
                expand_expr(&mut arm.body, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for a in args {
                expand_expr(a, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            expand_expr(receiver, tt, mods, reg, fmt_type, ms, alloc, cs);
            for a in args {
                expand_expr(a, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::Binary { left, right, .. } => {
            expand_expr(left, tt, mods, reg, fmt_type, ms, alloc, cs);
            expand_expr(right, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            expand_expr(inner, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::Assign { target, value } => {
            expand_expr(target, tt, mods, reg, fmt_type, ms, alloc, cs);
            expand_expr(value, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::Index {
            expr: e,
            index: idx,
        } => {
            expand_expr(e, tt, mods, reg, fmt_type, ms, alloc, cs);
            expand_expr(idx, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                expand_expr(&mut f.value, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::TupleLiteral { elements } => {
            for e in elements {
                expand_expr(e, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::Closure { body, .. } => {
            expand_expr(body, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::IndirectCall { callee, args } => {
            expand_expr(callee, tt, mods, reg, fmt_type, ms, alloc, cs);
            for a in args {
                expand_expr(a, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::CmRawCall { args, .. } => {
            for a in args {
                expand_expr(a, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                expand_expr(p, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            return;
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            expand_expr(value, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            expand_expr(functor, tt, mods, reg, fmt_type, ms, alloc, cs);
            return;
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expand_expr(scrutinee, tt, mods, reg, fmt_type, ms, alloc, cs);
            for arm in arms {
                expand_block(arm, tt, mods, reg, fmt_type, ms, alloc, cs);
            }
            expand_block(default, tt, mods, reg, fmt_type, ms, alloc, cs);
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

    let expanded = build_template_block(
        parts,
        string_type,
        span,
        tt,
        mods,
        reg,
        fmt_type,
        ms,
        alloc,
        cs,
    );
    *expr = expanded;
}

/// Build the `__tmpl: { ... }` labeled block for a template string.
fn build_template_block(
    parts: Vec<TirTemplatePart>,
    string_type: TypeId,
    span: Span,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    reg: &mut InspectRegistry,
    fmt_type: TypeId,
    ms: &ModuleSource,
    alloc: &mut FuncLocalAlloc,
    cs: &IndexMap<u32, String>,
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
        TirExprKind::StaticCall {
            func: FunctionRef::External {
                module_source: ModuleSource::string(),
                name: "String::with_capacity".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "String".to_string(),
                    None,
                    "with_capacity".to_string(),
                )),
            },
            args: vec![TirExpr::new(
                TirExprKind::IntLiteral {
                    value: capacity_estimate as u64,
                    repr: capacity_estimate.to_string(),
                },
                TypeTable::I32,
                span,
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
                let append_call = TirExpr::new(
                    TirExprKind::MethodCall {
                        receiver: Box::new(buf_ref),
                        func: FunctionRef::External {
                            module_source: ModuleSource::string(),
                            name: "String::append".to_string(),
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                "String".to_string(),
                                None,
                                "append".to_string(),
                            )),
                        },
                        type_args: vec![],
                        args: vec![TirExpr::new(
                            TirExprKind::StringLiteral(s),
                            string_type,
                            span,
                        )],
                    },
                    TypeTable::UNIT,
                    span,
                );
                stmts.push(TirStmt::new(TirStmtKind::Expr(append_call), span));
            }
            TirTemplatePart::Interpolation {
                expr: resolved,
                format_spec,
            } => {
                // Strip refs for type-based decisions
                let inner_type = strip_refs(resolved.type_id, tt);

                // If String type (or &String, &&String, ...) with no format spec, just append directly
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
                    let derefed = deref_to_inner(resolved, string_type, span);
                    let append_call = TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(buf_ref),
                            func: FunctionRef::External {
                                module_source: ModuleSource::string(),
                                name: "String::append".to_string(),
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    "String".to_string(),
                                    None,
                                    "append".to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![derefed],
                        },
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(append_call), span));
                    continue;
                }

                let is_inspect = format_spec
                    .as_ref()
                    .is_some_and(|fs| fs.type_char == Some('?'));

                let trait_name = match &format_spec {
                    Some(fs) => match fs.type_char {
                        Some('b') => "Binary",
                        Some('o') => "Octal",
                        Some('x') => "LowerHex",
                        Some('X') => "UpperHex",
                        Some('e') => "LowerExp",
                        Some('E') => "UpperExp",
                        _ => "Display",
                    },
                    None => "Display",
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

                // Check for float-with-precision fast path (using inner type)
                let float_fixed_func = if trait_name == "Display"
                    && format_spec
                        .as_ref()
                        .is_some_and(|fs| fs.precision.is_some())
                {
                    match tt.borrow().get(inner_type).clone() {
                        ResolvedType::Primitive(PrimitiveType::F64) => Some("fmt_f64_fixed"),
                        ResolvedType::Primitive(PrimitiveType::F32) => Some("fmt_f32_fixed"),
                        _ => None,
                    }
                } else {
                    None
                };

                if is_inspect {
                    // Closure with alternate mode (#:?): emit source text if available,
                    // otherwise fall back to signature via inspect function
                    let is_closure = matches!(
                        tt.borrow().get(inner_type).clone(),
                        ResolvedType::Function { .. }
                    );
                    let source_text = if is_closure {
                        closure_source_text(&resolved, cs)
                    } else {
                        None
                    };
                    if is_closure
                        && format_spec.as_ref().is_some_and(|fs| fs.alternate)
                        && let Some(text) = source_text
                    {
                        // #:? with source text available: write source directly
                        stmts.push(write_str_stmt(&text, fmt_mut_ref, tt, span));
                    } else {
                        // Inspect: preserve original type including refs so synth_ref
                        // can output the `&`/`&mut` prefix
                        let call_stmt = super::inspect::call_inspect_fn_pub(
                            reg,
                            resolved.type_id,
                            resolved,
                            fmt_mut_ref,
                            tt,
                            fmt_type,
                            ms,
                            span,
                        );
                        stmts.push(call_stmt);
                    }
                } else if let Some(func_name) = float_fixed_func {
                    let precision_value = format_spec.as_ref().unwrap().precision.unwrap();
                    let precision_expr = TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: precision_value as u64,
                            repr: precision_value.to_string(),
                        },
                        TypeTable::I32,
                        span,
                    );
                    let fmt_call = TirExpr::new(
                        TirExprKind::StaticCall {
                            func: FunctionRef::External {
                                module_source: ModuleSource::primitives(),
                                name: func_name.to_string(),
                                monomorph_info: None,
                                method_info: None,
                            },
                            args: vec![
                                deref_to_inner(resolved, inner_type, span),
                                precision_expr,
                                fmt_mut_ref,
                            ],
                        },
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(fmt_call), span));
                } else if has_trait_impl(inner_type, trait_name, tt, mods) {
                    // Has Display (or other format trait) impl — call it directly
                    let call_stmts =
                        trait_fmt_call(inner_type, resolved, fmt_mut_ref, trait_name, tt, span);
                    stmts.extend(call_stmts);
                } else {
                    // No Display impl — fall back to inspect, preserving refs
                    let call_stmt = super::inspect::call_inspect_fn_pub(
                        reg,
                        resolved.type_id,
                        resolved,
                        fmt_mut_ref,
                        tt,
                        fmt_type,
                        ms,
                        span,
                    );
                    stmts.push(call_stmt);
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
            || p.alternate
            || p.zero_pad
            || p.width.is_some()
            || p.precision.is_some()
    });

    if !has_custom_spec {
        return TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: ModuleSource::format(),
                    name: "Formatter::new".to_string(),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        "Formatter".to_string(),
                        None,
                        "new".to_string(),
                    )),
                },
                args: vec![buf_mut_ref],
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
                    name: "alternate".to_string(),
                    value: TirExpr::new(
                        TirExprKind::BoolLiteral(pf.alternate),
                        TypeTable::BOOL,
                        span,
                    ),
                    field_index: 3,
                },
                TirStructField {
                    name: "zero_pad".to_string(),
                    value: TirExpr::new(
                        TirExprKind::BoolLiteral(pf.zero_pad),
                        TypeTable::BOOL,
                        span,
                    ),
                    field_index: 4,
                },
                TirStructField {
                    name: "width".to_string(),
                    value: TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: pf.width.unwrap_or(-1) as u64,
                            repr: pf.width.unwrap_or(-1).to_string(),
                        },
                        TypeTable::I32,
                        span,
                    ),
                    field_index: 5,
                },
                TirStructField {
                    name: "precision".to_string(),
                    value: TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: pf.precision.unwrap_or(-1) as u64,
                            repr: pf.precision.unwrap_or(-1).to_string(),
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

/// Check if a concrete type has a format trait impl (Display, Binary, etc.)
/// in any of the loaded TIR modules.
/// Automatically strips references to check the inner type.
fn has_trait_impl(
    type_id: TypeId,
    trait_name: &str,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
) -> bool {
    let stripped = strip_refs(type_id, tt);
    let base = tt.borrow().get_ultimate_base_type(stripped);
    let type_name = tt.borrow().mangle_type_name(base);
    let expected_name = MethodName::format_local(&type_name, Some(trait_name), "fmt");
    for m in mods {
        for f in &m.functions {
            if let Ok(func) = f.try_borrow()
                && func.name == expected_name
            {
                return true;
            }
        }
    }
    false
}

/// Build a `TraitName::fmt(&expr, &mut f)` call.
/// `type_id` is the inner (non-ref) type for name mangling.
/// `val` is the expression whose actual type may be `&T` or `T`.
fn trait_fmt_call(
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    trait_name: &str,
    tt: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Vec<TirStmt> {
    let base = tt.borrow().get_ultimate_base_type(type_id);
    let type_name = tt.borrow().mangle_type_name(base);
    let impl_mod = trait_impl_module(base, trait_name, tt);

    // Check the expression's actual type (not the stripped type_id)
    let val_resolved_type = tt.borrow().get(val.type_id).clone();
    let receiver = match val_resolved_type {
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => val,
        _ => {
            let ref_type = tt.borrow_mut().make_ref(type_id);
            TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(val),
                },
                ref_type,
                span,
            )
        }
    };

    let mangled = MethodName::format_local(&type_name, Some(trait_name), "fmt");
    let call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: FunctionRef::External {
                module_source: impl_mod,
                name: mangled,
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    type_name,
                    Some(trait_name.to_string()),
                    "fmt".to_string(),
                )),
            },
            type_args: vec![],
            args: vec![fmt],
        },
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(call), span)]
}

fn trait_impl_module(
    type_id: TypeId,
    _trait_name: &str,
    tt: &Rc<RefCell<TypeTable>>,
) -> ModuleSource {
    match tt.borrow().get(type_id).clone() {
        ResolvedType::Primitive(_) => ModuleSource::primitives(),
        ResolvedType::Struct { name, .. } if name == "String" => ModuleSource::format(),
        ResolvedType::Struct { module_source, .. } | ResolvedType::Enum { module_source, .. } => {
            module_source
        }
        _ => ModuleSource::primitives(),
    }
}

/// Extract closure source text from a template interpolation expression.
/// Handles direct closures and locals that were bound to closures.
fn closure_source_text(expr: &TirExpr, cs: &IndexMap<u32, String>) -> Option<String> {
    match &expr.kind {
        TirExprKind::Closure {
            source_text: Some(text),
            ..
        } => Some(text.clone()),
        TirExprKind::Local { index, .. } => cs.get(index).cloned(),
        _ => None,
    }
}

/// Build a `f.write_str("text")` statement using the Formatter's `write_str` method.
fn write_str_stmt(text: &str, fmt: TirExpr, tt: &Rc<RefCell<TypeTable>>, span: Span) -> TirStmt {
    let string_type = tt
        .borrow_mut()
        .make_struct("String".to_string(), ModuleSource::string());
    let call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(fmt),
            func: FunctionRef::External {
                module_source: ModuleSource::format(),
                name: "Formatter::write_str".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "Formatter".into(),
                    None,
                    "write_str".into(),
                )),
            },
            type_args: vec![],
            args: vec![TirExpr::new(
                TirExprKind::StringLiteral(text.to_string()),
                string_type,
                span,
            )],
        },
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}
