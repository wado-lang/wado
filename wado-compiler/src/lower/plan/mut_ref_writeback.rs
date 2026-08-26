//! `f(&mut s.field)` on a replace-on-assign field hands the callee a detached
//! box — a whole-value write inside `f` lands in the box, not the field. Borrow
//! a temp instead and store it back after the call. See
//! `docs/wep-2026-06-13-reference-representation.md`.

use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::logger::{Bail, ErrorSink};
use crate::module_source::ModuleSource;
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
            escaped: Vec::new(),
        };
        if let Some(body) = func.body.as_mut() {
            pass.visit_block(body);
        }
        func.local_count = pass.local_count;
        func.locals = pass.locals;
        // `reify` marked the address-taken locals before this pass existed, so
        // each temp this pass borrows has to join them — that mark is what makes
        // `boxing` promote the slot to the box the borrow hands out.
        for temp in pass.borrowed_temps {
            func.address_taken_locals.insert(temp);
        }
        if let Some((span, callee)) = pass.escaped.first() {
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

/// Parameter positions each function declares in `stores[...]`. Such a borrow
/// outlives the call, so the call is no place to write back — the elaborator
/// refuses a detached borrow there instead.
fn escaping_params(flat: &FlatPackage) -> IndexMap<(ModuleSource, String), IndexSet<u32>> {
    let mut out: IndexMap<(ModuleSource, String), IndexSet<u32>> = IndexMap::default();
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
        out.insert((func.module_source.clone(), func.name.clone()), positions);
    }
    out
}

struct WriteBack<'a> {
    type_table: &'a TypeTable,
    escaping: &'a IndexMap<(ModuleSource, String), IndexSet<u32>>,
    local_count: u32,
    locals: Vec<TirLocal>,
    borrowed_temps: Vec<u32>,
    /// Detached borrows handed to a `stores` parameter, to report as errors.
    escaped: Vec<(Span, String)>,
}

impl WriteBack<'_> {
    fn alloc_local(&mut self, type_id: TypeId) -> u32 {
        let index = self.local_count;
        self.local_count += 1;
        self.locals.push(TirLocal::synth(index, type_id, true));
        index
    }

    /// Whether a `&mut` to a place of this type is a detached box rather than
    /// the place itself. A `variant` is the only one that reaches a call: the
    /// elaborator refuses the borrow outright for the other replace-on-assign
    /// types.
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

    /// The callee's name and the positions it keeps its borrows past the call.
    /// A direct callee declares them; an indirect one carries them on its
    /// function type, and a callee that is not a function type at all keeps
    /// nothing this pass can name.
    fn callee_stores(&self, call: &TirExpr) -> Option<(String, IndexSet<u32>)> {
        match &call.kind {
            TirExprKind::Call { func, .. } => Some((
                func.name.clone(),
                self.escaping
                    .get(&(func.module_source.clone(), func.name.clone()))
                    .cloned()
                    .unwrap_or_default(),
            )),
            TirExprKind::IndirectCall { callee, .. } => {
                let stores = match self.type_table.get(callee.type_id) {
                    ResolvedType::Function { stores, .. } => stores.iter().copied().collect(),
                    _ => IndexSet::default(),
                };
                Some(("a function value".to_string(), stores))
            }
            _ => None,
        }
    }

    /// `{ let t = place; let r = f(&mut t); place = t; r }` — the call keeps its
    /// position, so a `?` on it still sees the write-back run first.
    fn wrap(&mut self, call: &mut TirExpr) {
        let Some((callee, escaping)) = self.callee_stores(call) else {
            return;
        };
        let args: Vec<&mut TirExpr> = match &mut call.kind {
            TirExprKind::Call { args, .. } => args.iter_mut().map(|a| &mut a.expr).collect(),
            TirExprKind::IndirectCall { args, .. } => args.iter_mut().collect(),
            _ => return,
        };
        let mut prefix: Vec<TirStmt> = Vec::new();
        let mut write_backs: Vec<TirStmt> = Vec::new();
        for (position, arg) in args.into_iter().enumerate() {
            let Some(place) = self.detached_place(arg) else {
                continue;
            };
            // A `stores` parameter keeps the borrow past the call, so no point
            // in the caller is late enough to store the temp back.
            if escaping.contains(&u32::try_from(position).unwrap()) {
                self.escaped.push((arg.span, callee.clone()));
                continue;
            }
            let span = arg.span;
            let place_type = place.type_id;
            let place = place.clone();
            let temp = self.alloc_local(place_type);
            self.borrowed_temps.push(temp);
            let read_temp = || {
                TirExpr::new(
                    TirExprKind::Local {
                        index: temp,
                        name: format!("__write_back_{temp}"),
                    },
                    place_type,
                    span,
                )
            };
            prefix.push(TirStmt::new(
                TirStmtKind::Let {
                    name: format!("__write_back_{temp}"),
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
