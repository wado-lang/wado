//! Borrow a temp for a `&mut` to a variant field and store it back after the
//! call. See `docs/wep-2026-06-13-reference-representation.md`.

use super::value_copy::funcset::FuncKeyMap;
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::logger::{Bail, ErrorSink};
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirLocal, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr};
use crate::token::Span;

/// Runs before [`super::boxing::prepare_types`], while `&mut T` is still
/// distinguishable from `&T` and the referent type is readable.
pub fn insert_write_backs(flat: &mut FlatPackage, errors: &dyn ErrorSink) -> Result<(), Bail> {
    let escaping = escaping_params(flat);
    let type_table = flat.type_table.clone();
    let type_table = type_table.borrow();
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        let local_count = func.local_count;
        let locals = std::mem::take(&mut func.locals);
        let mut pass = WriteBack {
            type_table: &type_table,
            escaping: &escaping,
            local_count,
            locals,
            borrowed_temps: Vec::new(),
            escaped: None,
        };
        if let Some(body) = func.body.as_mut() {
            pass.visit_block(body);
        }
        func.local_count = pass.local_count;
        func.locals = pass.locals;
        // `reify` marks the address-taken locals before this pass runs, and that
        // mark is what makes `boxing` promote a slot to the box a borrow needs.
        for temp in pass.borrowed_temps {
            func.address_taken_locals.insert(temp);
        }
        if let Some((span, callee)) = &pass.escaped {
            return Err(errors.fatal_in(
                &func.module_source,
                Diagnostic {
                    severity: Severity::Error,
                    code: Code::ImmutableAssignment,
                    message: format!(
                        "cannot pass a mutable reference to a field or element of a variant to \
                         '{callee}', which stores it: the reference is a detached copy, so a \
                         whole-value write through it would be lost after the call returns"
                    ),
                    span: Some(DiagnosticSpan::from_span(span, None)),
                },
            ));
        }
    }
    Ok(())
}

/// Parameter positions each function declares in `stores[...]`: a borrow handed
/// to one outlives the call, so the call is no place to write it back.
fn escaping_params(flat: &FlatPackage) -> FuncKeyMap<IndexSet<u32>> {
    let mut out = FuncKeyMap::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if func.stores.is_empty() {
            continue;
        }
        let positions = func
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| func.stores.contains(&p.name))
            .map(|(i, _)| u32::try_from(i).unwrap())
            .collect();
        out.insert(func.module_source.clone(), func.name.clone(), positions);
    }
    out
}

struct WriteBack<'a> {
    type_table: &'a TypeTable,
    escaping: &'a FuncKeyMap<IndexSet<u32>>,
    local_count: u32,
    locals: Vec<TirLocal>,
    borrowed_temps: Vec<u32>,
    /// The first detached borrow handed to a `stores` parameter, to report.
    escaped: Option<(Span, String)>,
}

impl WriteBack<'_> {
    fn alloc_local(&mut self, type_id: TypeId) -> u32 {
        let index = self.local_count;
        self.local_count += 1;
        self.locals.push(TirLocal::synth(index, type_id, true));
        index
    }

    /// Whether a `&mut` to a place of this type is a detached box. Only a
    /// `variant` reaches a call; the elaborator refuses the other replace types.
    fn detaches(&self, place_type: TypeId) -> bool {
        let peeled = self.type_table.get_ultimate_base_type(place_type);
        matches!(self.type_table.get(peeled), ResolvedType::Variant { .. })
    }

    /// The place a detached `&mut` argument borrows, if this argument is one.
    fn detached_place<'e>(&self, arg: &'e TirExpr) -> Option<&'e TirExpr> {
        let TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: place,
        } = &arg.kind
        else {
            return None;
        };
        let is_projection = matches!(
            place.kind,
            TirExprKind::FieldAccess { .. } | TirExprKind::Index { .. }
        );
        (is_projection && self.detaches(place.type_id)).then_some(place.as_ref())
    }

    /// `{ let t = place; let r = f(&mut t); place = t; r }` — the call keeps its
    /// position, so a `?` on it still sees the write-back run first.
    fn wrap(&mut self, call: &mut TirExpr) {
        // A direct callee declares the positions it keeps a borrow past the
        // call; an indirect one carries them on its function type.
        let (callee, escaping, args): (_, _, Vec<&mut TirExpr>) = match &mut call.kind {
            TirExprKind::Call { func, args, .. } => (
                func.name.clone(),
                self.escaping
                    .get(&func.module_source, &func.name)
                    .cloned()
                    .unwrap_or_default(),
                args.iter_mut().map(|a| &mut a.expr).collect(),
            ),
            TirExprKind::IndirectCall { callee, args } => (
                "a function value".to_string(),
                match self.type_table.get(callee.type_id) {
                    ResolvedType::Function { stores, .. } => stores.iter().copied().collect(),
                    _ => IndexSet::default(),
                },
                args.iter_mut().collect(),
            ),
            _ => return,
        };
        let mut prefix: Vec<TirStmt> = Vec::new();
        let mut write_backs: Vec<TirStmt> = Vec::new();
        for (position, arg) in args.into_iter().enumerate() {
            let Some(place) = self.detached_place(arg) else {
                continue;
            };
            if escaping.contains(&u32::try_from(position).unwrap()) {
                self.escaped
                    .get_or_insert_with(|| (arg.span, callee.clone()));
                continue;
            }
            let span = arg.span;
            let place_type = place.type_id;
            let place = place.clone();
            let temp = self.alloc_local(place_type);
            self.borrowed_temps.push(temp);
            let name = format!("__write_back_{temp}");
            let read_temp = || {
                TirExpr::new(
                    TirExprKind::Local {
                        index: temp,
                        name: name.clone(),
                    },
                    place_type,
                    span,
                )
            };
            prefix.push(TirStmt::new(
                TirStmtKind::Let {
                    name: name.clone(),
                    local_index: temp,
                    is_mut: true,
                    is_reactive: false,
                    type_id: place_type,
                    value: place.clone(),
                    // The temp aliases the place: interior mutation through it
                    // lands directly, and the store below is then a no-op.
                    skip_value_copy: true,
                },
                span,
            ));
            write_backs.push(TirStmt::new(
                TirStmtKind::Expr(TirExpr::new(
                    TirExprKind::Assign {
                        target: Box::new(place),
                        value: Box::new(read_temp()),
                    },
                    TypeTable::UNIT,
                    span,
                )),
                span,
            ));
            *arg = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::MutRef,
                    expr: Box::new(read_temp()),
                },
                arg.type_id,
                span,
            );
        }
        if prefix.is_empty() {
            return;
        }

        let span = call.span;
        let result_type = call.type_id;
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span);
        let original = std::mem::replace(call, placeholder);
        let result = self.alloc_local(result_type);
        let result_name = format!("__write_back_result_{result}");
        let mut stmts = prefix;
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: result_name.clone(),
                local_index: result,
                is_mut: false,
                is_reactive: false,
                type_id: result_type,
                value: original,
                skip_value_copy: true,
            },
            span,
        ));
        stmts.append(&mut write_backs);
        stmts.push(TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::Local {
                    index: result,
                    name: result_name,
                },
                result_type,
                span,
            )),
            span,
        ));
        *call = TirExpr::new(
            TirExprKind::Block(TirBlock { stmts, span }),
            result_type,
            span,
        );
    }
}

impl TirOptVisitor for WriteBack<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        let mut changed = opt_walk_expr(self, expr);
        if matches!(
            expr.kind,
            TirExprKind::Call { .. } | TirExprKind::IndirectCall { .. }
        ) {
            let before = self.borrowed_temps.len();
            self.wrap(expr);
            changed |= self.borrowed_temps.len() != before;
        }
        changed
    }
}
