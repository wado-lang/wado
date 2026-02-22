//! Inspect synthesis phase.
//!
//! Replaces `builtin::inspect(expr, &mut f)` marker calls with synthesized
//! TIR that writes the debug representation of `expr` to a `Formatter`.
//!
//! Pipeline position: after `effect_check`, before `cm_adapter_gen`.
//! This ensures synthesized code goes through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-21-inspect-debug-output.md` for design details.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::project::Project;
use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind,
    TirFlags, TirModule, TirStmt, TirStmtKind, TirUnaryOp, TirVariantDecl, TypeId, TypeTable,
};
use crate::token::Span;

/// Run the inspect synthesis pass on the entire project.
///
/// Walks all TIR modules and replaces `builtin::inspect` marker calls
/// with synthesized inspect code appropriate for each type.
pub fn synthesize_inspect(project: Project) -> Project {
    let module_sources: Vec<ModuleSource> = project.tir_modules.keys().cloned().collect();

    for module_source in &module_sources {
        let module = project.tir_modules.get(module_source).unwrap();
        let type_table = module.type_table.clone();
        let functions: Vec<Rc<RefCell<_>>> = module.functions.clone();
        let all_modules: Vec<&TirModule> = project.tir_modules.values().collect();

        for func_rc in &functions {
            let mut func = func_rc.borrow_mut();
            let start_index = func.local_count;
            if let Some(ref mut body) = func.body {
                let closure_sources = collect_closure_sources(body);
                let mut alloc = LocalAlloc {
                    next_index: start_index,
                    new_types: Vec::new(),
                };
                synthesize_in_block(
                    body,
                    &type_table,
                    &all_modules,
                    &mut alloc,
                    &closure_sources,
                );
                func.local_count = alloc.next_index;
                func.local_types.extend(alloc.new_types);
            }
        }
    }

    project
}

/// Tracks local variable allocation during synthesis.
struct LocalAlloc {
    next_index: u32,
    new_types: Vec<TypeId>,
}

impl LocalAlloc {
    fn alloc(&mut self, type_id: TypeId) -> u32 {
        let idx = self.next_index;
        self.next_index += 1;
        self.new_types.push(type_id);
        idx
    }
}

/// Collect closure source texts from Let statements in a function body.
/// Maps `local_index` → `source_text` for closures that have pre-desugar source.
fn collect_closure_sources(block: &TirBlock) -> IndexMap<u32, String> {
    let mut map = IndexMap::new();
    collect_closure_sources_block(block, &mut map);
    map
}

fn collect_closure_sources_block(block: &TirBlock, map: &mut IndexMap<u32, String>) {
    for stmt in &block.stmts {
        collect_closure_sources_stmt(stmt, map);
    }
}

fn collect_closure_sources_stmt(stmt: &TirStmt, map: &mut IndexMap<u32, String>) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if let Some(text) = extract_closure_source(value) {
                map.insert(*local_index, text);
            }
            collect_closure_sources_expr(value, map);
        }
        TirStmtKind::Expr(e) => collect_closure_sources_expr(e, map),
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            collect_closure_sources_block(then_block, map);
            if let Some(b) = else_block {
                collect_closure_sources_block(b, map);
            }
        }
        TirStmtKind::Loop { body } => collect_closure_sources_block(body, map),
        TirStmtKind::IfPattern {
            then_block,
            else_block,
            ..
        } => {
            collect_closure_sources_block(then_block, map);
            if let Some(b) = else_block {
                collect_closure_sources_block(b, map);
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_closure_sources_block(block, map);
        }
        TirStmtKind::LetPattern { value, .. } | TirStmtKind::TaskReturn { value, .. } => {
            collect_closure_sources_expr(value, map);
        }
        _ => {}
    }
}

fn collect_closure_sources_expr(expr: &TirExpr, map: &mut IndexMap<u32, String>) {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_closure_sources_block(block, map);
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            collect_closure_sources_block(then_branch, map);
            if let Some(b) = else_branch {
                collect_closure_sources_block(b, map);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_closure_sources_expr(body, map);
        }
        _ => {}
    }
}

/// Extract closure source text from an expression.
/// Handles both direct closures and mutable closures wrapped in a Block.
fn extract_closure_source(expr: &TirExpr) -> Option<String> {
    match &expr.kind {
        TirExprKind::Closure {
            source_text: Some(text),
            ..
        } => Some(text.clone()),
        // Mutable closures are wrapped: Block { let __ref_x = &mut x; Closure { ... } }
        TirExprKind::Block(block) => block.stmts.last().and_then(|stmt| {
            if let TirStmtKind::Expr(inner) = &stmt.kind {
                extract_closure_source(inner)
            } else {
                None
            }
        }),
        _ => None,
    }
}

// ─── TIR tree walking ───

fn synthesize_in_block(
    block: &mut TirBlock,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    closure_sources: &IndexMap<u32, String>,
) {
    let mut i = 0;
    while i < block.stmts.len() {
        let needs_expansion = if let TirStmtKind::Expr(ref expr) = block.stmts[i].kind {
            is_inspect_marker(expr)
        } else {
            false
        };

        if needs_expansion {
            let stmt = block.stmts.remove(i);
            if let TirStmtKind::Expr(expr) = stmt.kind
                && let TirExprKind::StaticCall { args, .. } = expr.kind
            {
                let mut args = args;
                let fmt_ref = args.pop().unwrap();
                let value_expr = args.pop().unwrap();
                let type_id = value_expr.type_id;
                let span = expr.span;

                let new_stmts = synthesize_for_type(
                    type_id,
                    value_expr,
                    fmt_ref,
                    tt,
                    mods,
                    alloc,
                    closure_sources,
                    span,
                );
                for (j, s) in new_stmts.into_iter().enumerate() {
                    block.stmts.insert(i + j, s);
                }
                continue;
            }
        } else {
            synthesize_in_stmt(&mut block.stmts[i], tt, mods, alloc, closure_sources);
        }
        i += 1;
    }
}

fn is_inspect_marker(expr: &TirExpr) -> bool {
    matches!(&expr.kind, TirExprKind::StaticCall { func, .. } if func.name() == "builtin::inspect")
}

fn synthesize_in_stmt(
    stmt: &mut TirStmt,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
) {
    match &mut stmt.kind {
        TirStmtKind::Expr(e) => walk_expr(e, tt, mods, alloc, cs),
        TirStmtKind::Let { value, .. } => walk_expr(value, tt, mods, alloc, cs),
        TirStmtKind::Return { value: Some(e) } | TirStmtKind::Break { value: Some(e), .. } => {
            walk_expr(e, tt, mods, alloc, cs);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            walk_expr(condition, tt, mods, alloc, cs);
            synthesize_in_block(then_block, tt, mods, alloc, cs);
            if let Some(eb) = else_block {
                synthesize_in_block(eb, tt, mods, alloc, cs);
            }
        }
        TirStmtKind::Loop { body } => synthesize_in_block(body, tt, mods, alloc, cs),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            walk_expr(scrutinee, tt, mods, alloc, cs);
            synthesize_in_block(then_block, tt, mods, alloc, cs);
            if let Some(eb) = else_block {
                synthesize_in_block(eb, tt, mods, alloc, cs);
            }
        }
        TirStmtKind::LetPattern { value, .. } => walk_expr(value, tt, mods, alloc, cs),
        _ => {}
    }
}

fn walk_expr(
    expr: &mut TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
) {
    match &mut expr.kind {
        TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } => {
            synthesize_in_block(b, tt, mods, alloc, cs);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr(condition, tt, mods, alloc, cs);
            synthesize_in_block(then_branch, tt, mods, alloc, cs);
            if let Some(eb) = else_branch {
                synthesize_in_block(eb, tt, mods, alloc, cs);
            }
        }
        TirExprKind::Match { expr: s, arms } => {
            walk_expr(s, tt, mods, alloc, cs);
            for arm in arms {
                walk_expr(&mut arm.body, tt, mods, alloc, cs);
            }
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for a in args {
                walk_expr(a, tt, mods, alloc, cs);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            walk_expr(receiver, tt, mods, alloc, cs);
            for a in args {
                walk_expr(a, tt, mods, alloc, cs);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            walk_expr(left, tt, mods, alloc, cs);
            walk_expr(right, tt, mods, alloc, cs);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::IsNotNull { expr: inner }
        | TirExprKind::UnwrapOption { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            walk_expr(inner, tt, mods, alloc, cs);
        }
        TirExprKind::Assign { target, value } => {
            walk_expr(target, tt, mods, alloc, cs);
            walk_expr(value, tt, mods, alloc, cs);
        }
        TirExprKind::Index {
            expr: e,
            index: idx,
        } => {
            walk_expr(e, tt, mods, alloc, cs);
            walk_expr(idx, tt, mods, alloc, cs);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                walk_expr(&mut f.value, tt, mods, alloc, cs);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for e in elements {
                walk_expr(e, tt, mods, alloc, cs);
            }
        }
        TirExprKind::Closure { body, .. } => walk_expr(body, tt, mods, alloc, cs),
        TirExprKind::IndirectCall { callee, args } => {
            walk_expr(callee, tt, mods, alloc, cs);
            for a in args {
                walk_expr(a, tt, mods, alloc, cs);
            }
        }
        _ => {}
    }
}

// ─── Synthesis helpers ───

/// Resolve string `TypeId`.
fn str_type(tt: &Rc<RefCell<TypeTable>>) -> TypeId {
    tt.borrow_mut().make_struct(
        "String".to_string(),
        ModuleSource::core("prelude/string.wado"),
    )
}

/// Build a `f.write_str("text")` statement.
fn ws(text: &str, fmt: TirExpr, tt: &Rc<RefCell<TypeTable>>, span: Span) -> TirStmt {
    let s = str_type(tt);
    let call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(fmt),
            func: FunctionRef::External {
                module_source: ModuleSource::core("prelude/format.wado"),
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
                s,
                span,
            )],
        },
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Build a `f.write_char(c)` statement.
fn wc(c: char, fmt: TirExpr, span: Span) -> TirStmt {
    let call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(fmt),
            func: FunctionRef::External {
                module_source: ModuleSource::core("prelude/format.wado"),
                name: "Formatter::write_char".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "Formatter".into(),
                    None,
                    "write_char".into(),
                )),
            },
            type_args: vec![],
            args: vec![TirExpr::new(
                TirExprKind::CharLiteral(c),
                TypeTable::CHAR,
                span,
            )],
        },
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Build a `Display::fmt` call: `(&expr).fmt(fmt_ref)`.
fn display_fmt(
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Vec<TirStmt> {
    let base = tt.borrow().get_ultimate_base_type(type_id);
    let type_name = tt.borrow().mangle_type_name(base);
    let impl_mod = display_impl_module(base, tt);

    let ref_type = tt.borrow_mut().make_ref(type_id);
    let receiver = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(val),
        },
        ref_type,
        span,
    );

    let mangled = MethodName::format_local(&type_name, Some("Display"), "fmt");
    let call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: FunctionRef::External {
                module_source: impl_mod,
                name: mangled,
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    type_name,
                    Some("Display".into()),
                    "fmt".into(),
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

fn display_impl_module(type_id: TypeId, tt: &Rc<RefCell<TypeTable>>) -> ModuleSource {
    match tt.borrow().get(type_id).clone() {
        ResolvedType::Primitive(_) => ModuleSource::core("prelude/primitives.wado"),
        ResolvedType::Struct { name, .. } if name == "String" => {
            ModuleSource::core("prelude/format.wado")
        }
        ResolvedType::Struct { module_source, .. } | ResolvedType::Enum { module_source, .. } => {
            module_source
        }
        _ => ModuleSource::core("prelude/primitives.wado"),
    }
}

// ─── Type-driven synthesis ───

fn synthesize_for_type(
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    let resolved = tt.borrow().get(type_id).clone();

    match resolved {
        ResolvedType::Primitive(PrimitiveType::Char) => {
            let mut s = Vec::new();
            s.push(wc('\'', fmt.clone(), span));
            s.extend(display_fmt(type_id, val, fmt.clone(), tt, span));
            s.push(wc('\'', fmt, span));
            s
        }
        ResolvedType::Primitive(_) => display_fmt(type_id, val, fmt, tt, span),
        ResolvedType::Unit => {
            vec![ws("()", fmt, tt, span)]
        }
        ResolvedType::Struct {
            ref name,
            ref module_source,
            ..
        } => {
            let n = name.clone();
            let ms = module_source.clone();
            if n == "String" && ms == ModuleSource::core("prelude/string.wado") {
                synth_string(val, fmt, tt, span)
            } else {
                synth_struct(&n, type_id, val, fmt, tt, mods, alloc, cs, span)
            }
        }
        ResolvedType::Enum { ref name, .. } => {
            let n = name.clone();
            synth_enum(&n, type_id, val, fmt, tt, mods, span)
        }
        ResolvedType::Option(inner) => {
            synth_option(type_id, inner, val, fmt, tt, mods, alloc, cs, span)
        }
        ResolvedType::Tuple(ref elems) => {
            let e = elems.clone();
            synth_tuple(&e, val, fmt, tt, mods, alloc, cs, span)
        }
        ResolvedType::GenericInstance {
            ref name,
            ref type_args,
            ..
        } if name == "Array" && type_args.len() == 1 => {
            let elem = type_args[0];
            synth_array(type_id, elem, val, fmt, tt, mods, alloc, cs, span)
        }
        ResolvedType::Ref(inner) => synth_ref(false, inner, val, fmt, tt, mods, alloc, cs, span),
        ResolvedType::MutRef(inner) => synth_ref(true, inner, val, fmt, tt, mods, alloc, cs, span),
        ResolvedType::Newtype {
            ref name,
            base_type,
            ..
        } => {
            let n = name.clone();
            synth_newtype(&n, base_type, val, fmt, tt, mods, alloc, cs, span)
        }
        ResolvedType::Variant { ref name, .. } => {
            let n = name.clone();
            synth_variant(&n, type_id, val, fmt, tt, mods, alloc, cs, span)
        }
        ResolvedType::Resource { ref name, .. } => {
            let n = name.clone();
            synth_resource(&n, val, fmt, tt, span)
        }
        ResolvedType::Function {
            ref params,
            return_type,
            ..
        } => {
            // Try to find pre-desugar source text from the value expression
            let source = match &val.kind {
                TirExprKind::Closure {
                    source_text: Some(text),
                    ..
                } => Some(text.clone()),
                TirExprKind::Local { index, .. } => cs.get(index).cloned(),
                _ => None,
            };
            let p = params.clone();
            synth_fn_type(&p, return_type, source.as_deref(), fmt, tt, span)
        }
        _ => {
            let tn = tt.borrow().type_name(type_id);
            vec![ws(&format!("<{tn}>"), fmt, tt, span)]
        }
    }
}

fn synth_string(
    val: TirExpr,
    fmt: TirExpr,
    _tt: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Vec<TirStmt> {
    let mut s = Vec::new();
    s.push(wc('"', fmt.clone(), span));
    let call = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::External {
                module_source: ModuleSource::core("internal"),
                name: "write_escaped_string".to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args: vec![val, fmt.clone()],
        },
        TypeTable::UNIT,
        span,
    );
    s.push(TirStmt::new(TirStmtKind::Expr(call), span));
    s.push(wc('"', fmt, span));
    s
}

fn synth_struct(
    name: &str,
    _type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    let fields = find_struct_fields(name, mods);
    let mut s = Vec::new();

    match fields {
        Some(fields) if !fields.is_empty() => {
            s.push(ws(&format!("{name} {{ "), fmt.clone(), tt, span));
            for (i, (fn_, ft, fi)) in fields.iter().enumerate() {
                if i > 0 {
                    s.push(ws(", ", fmt.clone(), tt, span));
                }
                s.push(ws(&format!("{fn_}: "), fmt.clone(), tt, span));
                let fa = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(val.clone()),
                        field_index: *fi,
                        field_name: fn_.clone(),
                    },
                    *ft,
                    span,
                );
                s.extend(synthesize_for_type(
                    *ft,
                    fa,
                    fmt.clone(),
                    tt,
                    mods,
                    alloc,
                    cs,
                    span,
                ));
            }
            s.push(ws(" }", fmt, tt, span));
        }
        Some(_) => s.push(ws(&format!("{name} {{}}"), fmt, tt, span)),
        None => s.push(ws(&format!("{name} {{ ... }}"), fmt, tt, span)),
    }
    s
}

fn find_struct_fields(name: &str, mods: &[&TirModule]) -> Option<Vec<(String, TypeId, u32)>> {
    for m in mods {
        if let Some(s) = m.find_struct(name) {
            return Some(
                s.fields
                    .iter()
                    .map(|f| (f.name.clone(), f.type_id, f.index))
                    .collect(),
            );
        }
    }
    None
}

fn synth_enum(
    name: &str,
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    span: Span,
) -> Vec<TirStmt> {
    let cases = find_enum_cases(name, mods);
    match cases {
        Some(cases) if !cases.is_empty() => {
            let mut chain: Option<TirExpr> = None;
            for (cn, ci) in cases.iter().rev() {
                let text = format!("{name}::{cn}");
                let then_block = TirBlock::new(vec![ws(&text, fmt.clone(), tt, span)], span);
                let cond = TirExpr::new(
                    TirExprKind::Binary {
                        left: Box::new(val.clone()),
                        op: TirBinaryOp::Eq,
                        right: Box::new(TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: u64::from(*ci),
                                repr: ci.to_string(),
                            },
                            type_id,
                            span,
                        )),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let if_expr = TirExpr::new(
                    TirExprKind::If {
                        condition: Box::new(cond),
                        then_branch: then_block,
                        else_branch: chain.map(|e| {
                            TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(e), span)], span)
                        }),
                    },
                    TypeTable::UNIT,
                    span,
                );
                chain = Some(if_expr);
            }
            chain.map_or_else(Vec::new, |e| vec![TirStmt::new(TirStmtKind::Expr(e), span)])
        }
        _ => vec![ws(name, fmt, tt, span)],
    }
}

fn find_enum_cases(name: &str, mods: &[&TirModule]) -> Option<Vec<(String, u32)>> {
    for m in mods {
        if let Some(e) = m.find_enum(name) {
            return Some(e.cases.iter().map(|c| (c.name.clone(), c.index)).collect());
        }
    }
    None
}

fn synth_option(
    _opt_type: TypeId,
    inner: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    let is_some = TirExpr::new(
        TirExprKind::IsNotNull {
            expr: Box::new(val.clone()),
        },
        TypeTable::BOOL,
        span,
    );
    let unwrapped = TirExpr::new(
        TirExprKind::UnwrapOption {
            expr: Box::new(val),
            inner_type: inner,
        },
        inner,
        span,
    );
    let mut then_stmts = Vec::new();
    then_stmts.push(ws("Some(", fmt.clone(), tt, span));
    then_stmts.extend(synthesize_for_type(
        inner,
        unwrapped,
        fmt.clone(),
        tt,
        mods,
        alloc,
        cs,
        span,
    ));
    then_stmts.push(wc(')', fmt.clone(), span));

    let else_stmts = vec![ws("null", fmt, tt, span)];

    let if_expr = TirExpr::new(
        TirExprKind::If {
            condition: Box::new(is_some),
            then_branch: TirBlock::new(then_stmts, span),
            else_branch: Some(TirBlock::new(else_stmts, span)),
        },
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(if_expr), span)]
}

fn synth_tuple(
    elems: &[TypeId],
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    let mut s = vec![wc('[', fmt.clone(), span)];
    for (i, et) in elems.iter().enumerate() {
        if i > 0 {
            s.push(ws(", ", fmt.clone(), tt, span));
        }
        let ea = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(val.clone()),
                field_index: i as u32,
                field_name: i.to_string(),
            },
            *et,
            span,
        );
        s.extend(synthesize_for_type(
            *et,
            ea,
            fmt.clone(),
            tt,
            mods,
            alloc,
            cs,
            span,
        ));
    }
    s.push(wc(']', fmt, span));
    s
}

fn synth_array(
    _arr_type: TypeId,
    elem: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    let mut s = vec![wc('[', fmt.clone(), span)];

    let len_call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(val.clone()),
            func: FunctionRef::External {
                module_source: ModuleSource::core("prelude/array.wado"),
                name: "Array::len".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new("Array".into(), None, "len".into())),
            },
            type_args: vec![],
            args: vec![],
        },
        TypeTable::I32,
        span,
    );

    let len_idx = alloc.alloc(TypeTable::I32);
    let i_idx = alloc.alloc(TypeTable::I32);

    s.push(TirStmt::new(
        TirStmtKind::Let {
            name: "__inspect_len".into(),
            local_index: len_idx,
            is_mut: false,
            is_reactive: false,
            type_id: TypeTable::I32,
            value: len_call,
        },
        span,
    ));

    s.push(TirStmt::new(
        TirStmtKind::Let {
            name: "__inspect_i".into(),
            local_index: i_idx,
            is_mut: true,
            is_reactive: false,
            type_id: TypeTable::I32,
            value: TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".into(),
                },
                TypeTable::I32,
                span,
            ),
        },
        span,
    ));

    let i_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: i_idx,
                name: "__inspect_i".into(),
            },
            TypeTable::I32,
            span,
        )
    };
    let len_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: len_idx,
                name: "__inspect_len".into(),
            },
            TypeTable::I32,
            span,
        )
    };

    let cond = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(i_local()),
            op: TirBinaryOp::Lt,
            right: Box::new(len_local()),
        },
        TypeTable::BOOL,
        span,
    );

    let mut body = Vec::new();

    let comma_cond = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(i_local()),
            op: TirBinaryOp::Gt,
            right: Box::new(TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".into(),
                },
                TypeTable::I32,
                span,
            )),
        },
        TypeTable::BOOL,
        span,
    );
    body.push(TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::If {
                condition: Box::new(comma_cond),
                then_branch: TirBlock::new(vec![ws(", ", fmt.clone(), tt, span)], span),
                else_branch: None,
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    ));

    let idx_access = TirExpr::new(
        TirExprKind::Index {
            expr: Box::new(val),
            index: Box::new(i_local()),
        },
        elem,
        span,
    );
    body.extend(synthesize_for_type(
        elem,
        idx_access,
        fmt.clone(),
        tt,
        mods,
        alloc,
        cs,
        span,
    ));

    body.push(TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(i_local()),
                value: Box::new(TirExpr::new(
                    TirExprKind::Binary {
                        left: Box::new(i_local()),
                        op: TirBinaryOp::Add,
                        right: Box::new(TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: 1,
                                repr: "1".into(),
                            },
                            TypeTable::I32,
                            span,
                        )),
                    },
                    TypeTable::I32,
                    span,
                )),
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    ));

    let break_if_done = TirStmt::new(
        TirStmtKind::If {
            condition: TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Not,
                    expr: Box::new(cond),
                },
                TypeTable::BOOL,
                span,
            ),
            then_block: TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                )],
                span,
            ),
            else_block: None,
        },
        span,
    );
    body.insert(0, break_if_done);
    s.push(TirStmt::new(
        TirStmtKind::Loop {
            body: TirBlock::new(body, span),
        },
        span,
    ));
    s.push(wc(']', fmt, span));
    s
}

fn synth_ref(
    is_mut: bool,
    inner: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    let mut s = Vec::new();
    if is_mut {
        s.push(ws("&mut ", fmt.clone(), tt, span));
    } else {
        s.push(wc('&', fmt.clone(), span));
    }
    let deref = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(val),
        },
        inner,
        span,
    );
    s.extend(synthesize_for_type(
        inner, deref, fmt, tt, mods, alloc, cs, span,
    ));
    s
}

fn synth_newtype(
    name: &str,
    base: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    if let Some(fd) = find_flags(name, mods) {
        return synth_flags(&fd, val, fmt, tt, alloc, span);
    }

    let cast = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(val),
            target_type: base,
        },
        base,
        span,
    );
    let mut s = synthesize_for_type(base, cast, fmt.clone(), tt, mods, alloc, cs, span);
    s.push(ws(&format!(" as {name}"), fmt, tt, span));
    s
}

fn find_flags(name: &str, mods: &[&TirModule]) -> Option<TirFlags> {
    for m in mods {
        for f in &m.flags {
            if f.name == name {
                return Some(f.clone());
            }
        }
    }
    None
}

fn synth_variant(
    name: &str,
    _type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    mods: &[&TirModule],
    alloc: &mut LocalAlloc,
    cs: &IndexMap<u32, String>,
    span: Span,
) -> Vec<TirStmt> {
    let vd = find_variant(name, mods);
    match vd {
        Some(decl) => {
            let cases = decl.cases.clone();
            let mut chain: Option<TirExpr> = None;

            for case in cases.iter().rev() {
                let is_unit = case.payload == TypeTable::UNIT;
                let mut then_stmts = Vec::new();

                if is_unit {
                    then_stmts.push(ws(&format!("{name}::{}", case.name), fmt.clone(), tt, span));
                } else {
                    then_stmts.push(ws(
                        &format!("{name}::{}(", case.name),
                        fmt.clone(),
                        tt,
                        span,
                    ));
                    let payload = TirExpr::new(
                        TirExprKind::VariantPayload {
                            expr: Box::new(val.clone()),
                            case_index: case.index,
                            payload_type: case.payload,
                        },
                        case.payload,
                        span,
                    );
                    then_stmts.extend(synthesize_for_type(
                        case.payload,
                        payload,
                        fmt.clone(),
                        tt,
                        mods,
                        alloc,
                        cs,
                        span,
                    ));
                    then_stmts.push(wc(')', fmt.clone(), span));
                }

                let cond = TirExpr::new(
                    TirExprKind::VariantTest {
                        expr: Box::new(val.clone()),
                        case_index: case.index,
                        case_name: case.name.clone(),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let if_expr = TirExpr::new(
                    TirExprKind::If {
                        condition: Box::new(cond),
                        then_branch: TirBlock::new(then_stmts, span),
                        else_branch: chain.map(|e| {
                            TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(e), span)], span)
                        }),
                    },
                    TypeTable::UNIT,
                    span,
                );
                chain = Some(if_expr);
            }
            chain.map_or_else(Vec::new, |e| vec![TirStmt::new(TirStmtKind::Expr(e), span)])
        }
        None => vec![ws(&format!("{name}::???"), fmt, tt, span)],
    }
}

fn find_variant(name: &str, mods: &[&TirModule]) -> Option<TirVariantDecl> {
    for m in mods {
        for v in &m.variants {
            if v.name == name {
                return Some(v.clone());
            }
        }
    }
    None
}

fn synth_resource(
    name: &str,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Vec<TirStmt> {
    let mut s = Vec::new();
    s.push(ws(&format!("{name}#"), fmt.clone(), tt, span));
    let cast = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(val),
            target_type: TypeTable::I32,
        },
        TypeTable::I32,
        span,
    );
    s.extend(display_fmt(TypeTable::I32, cast, fmt, tt, span));
    s
}

fn synth_flags(
    fd: &TirFlags,
    val: TirExpr,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    alloc: &mut LocalAlloc,
    span: Span,
) -> Vec<TirStmt> {
    let name = &fd.name;

    let u32_val = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(val),
            target_type: TypeTable::U32,
        },
        TypeTable::U32,
        span,
    );

    let is_none = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(u32_val.clone()),
            op: TirBinaryOp::Eq,
            right: Box::new(TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".into(),
                },
                TypeTable::U32,
                span,
            )),
        },
        TypeTable::BOOL,
        span,
    );

    let none_text = format!("{name}::none()");
    let then_block = TirBlock::new(vec![ws(&none_text, fmt.clone(), tt, span)], span);

    let first_idx = alloc.alloc(TypeTable::BOOL);
    let mut else_stmts = Vec::new();
    else_stmts.push(TirStmt::new(
        TirStmtKind::Let {
            name: "__inspect_first".into(),
            local_index: first_idx,
            is_mut: true,
            is_reactive: false,
            type_id: TypeTable::BOOL,
            value: TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span),
        },
        span,
    ));

    let first_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: first_idx,
                name: "__inspect_first".into(),
            },
            TypeTable::BOOL,
            span,
        )
    };

    for member in &fd.members {
        let bit_check = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(TirExpr::new(
                    TirExprKind::Binary {
                        left: Box::new(u32_val.clone()),
                        op: TirBinaryOp::BitAnd,
                        right: Box::new(TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: u64::from(member.bitmask),
                                repr: member.bitmask.to_string(),
                            },
                            TypeTable::U32,
                            span,
                        )),
                    },
                    TypeTable::U32,
                    span,
                )),
                op: TirBinaryOp::NotEq,
                right: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: 0,
                        repr: "0".into(),
                    },
                    TypeTable::U32,
                    span,
                )),
            },
            TypeTable::BOOL,
            span,
        );

        let mut member_stmts = Vec::new();
        let sep_cond = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(first_local()),
            },
            TypeTable::BOOL,
            span,
        );
        member_stmts.push(TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(sep_cond),
                    then_branch: TirBlock::new(vec![ws(" | ", fmt.clone(), tt, span)], span),
                    else_branch: None,
                },
                TypeTable::UNIT,
                span,
            )),
            span,
        ));
        member_stmts.push(ws(
            &format!("{name}::{}", member.name),
            fmt.clone(),
            tt,
            span,
        ));
        member_stmts.push(TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::Assign {
                    target: Box::new(first_local()),
                    value: Box::new(TirExpr::new(
                        TirExprKind::BoolLiteral(false),
                        TypeTable::BOOL,
                        span,
                    )),
                },
                TypeTable::UNIT,
                span,
            )),
            span,
        ));

        else_stmts.push(TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(bit_check),
                    then_branch: TirBlock::new(member_stmts, span),
                    else_branch: None,
                },
                TypeTable::UNIT,
                span,
            )),
            span,
        ));
    }

    let if_expr = TirExpr::new(
        TirExprKind::If {
            condition: Box::new(is_none),
            then_branch: then_block,
            else_branch: Some(TirBlock::new(else_stmts, span)),
        },
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(if_expr), span)]
}

fn synth_fn_type(
    params: &[TypeId],
    ret: TypeId,
    source_text: Option<&str>,
    fmt: TirExpr,
    tt: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Vec<TirStmt> {
    if let Some(text) = source_text {
        return vec![ws(text, fmt, tt, span)];
    }
    let t = tt.borrow();
    let mut sig = String::from("|");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(&t.type_name(*p));
    }
    sig.push('|');
    if ret != TypeTable::UNIT {
        sig.push_str(" -> ");
        sig.push_str(&t.type_name(ret));
    }
    drop(t);
    vec![ws(&sig, fmt, tt, span)]
}
