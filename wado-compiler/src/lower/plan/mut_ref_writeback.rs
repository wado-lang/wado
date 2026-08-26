//! Borrow a temp for a `&mut` to a variant field and store it back after the
//! call. See `docs/wep-2026-06-13-reference-representation.md`.

use super::value_copy::funcset::FuncKeyMap;
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::logger::{Bail, ErrorSink};
use crate::tir::{
    ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirLocal, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr, opt_walk_stmt};
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
            local_base: 0,
            locals,
            borrowed_temps: Vec::new(),
            refused: None,
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
        if let Some((span, reason)) = &pass.refused {
            return Err(errors.fatal_in(
                &func.module_source,
                Diagnostic {
                    severity: Severity::Error,
                    code: Code::ImmutableAssignment,
                    message: format!(
                        "a mutable reference to a field or element of a variant is a detached \
                         copy, so a whole-value write through it would be lost: {reason}"
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
    /// Locals from `local_base` up. A function owns its parameters' slots too,
    /// so its base is 0; a closure keeps parameters outside `body_locals`.
    local_base: u32,
    locals: Vec<TirLocal>,
    borrowed_temps: Vec<u32>,
    /// The first detached borrow with no write-back point, and why.
    refused: Option<(Span, String)>,
}

/// A local this pass planted, and the reads of it the rewrite needs.
struct Temp {
    index: u32,
    name: String,
    type_id: TypeId,
    span: Span,
}

impl Temp {
    fn read(&self) -> TirExpr {
        TirExpr::new(
            TirExprKind::Local {
                index: self.index,
                name: self.name.clone(),
            },
            self.type_id,
            self.span,
        )
    }
}

impl WriteBack<'_> {
    /// Bind `value` to a fresh local. Every temp this pass plants stands in for
    /// storage its caller still owns, so none of them copy.
    fn bind(
        &mut self,
        prefix: &mut Vec<TirStmt>,
        kind: &str,
        value: TirExpr,
        is_mut: bool,
    ) -> Temp {
        assert_eq!(
            u32::try_from(self.locals.len()).unwrap() + self.local_base,
            self.local_count
        );
        let (type_id, span) = (value.type_id, value.span);
        let index = self.local_count;
        self.local_count += 1;
        self.locals.push(TirLocal::synth(index, type_id, true));
        let name = format!("__write_back_{kind}{index}");
        prefix.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.clone(),
                local_index: index,
                is_mut,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: true,
            },
            span,
        ));
        Temp {
            index,
            name,
            type_id,
            span,
        }
    }

    fn refuse(&mut self, span: Span, reason: String) {
        self.refused.get_or_insert((span, reason));
    }

    /// Refuse a detached borrow put into storage outliving the expression that
    /// took it, which no point in this body is late enough to write back.
    fn refuse_stored(&mut self, expr: &TirExpr, sink: &str) {
        for place in self.detached_in_value_position(expr) {
            self.refuse(place, format!("{sink} holds it past the borrow"));
        }
    }

    /// The detached borrows `expr` can yield, following the value positions one
    /// reaches storage through.
    fn detached_in_value_position(&self, expr: &TirExpr) -> Vec<Span> {
        if let Some(place) = self.detached_borrow(expr) {
            return vec![place.span];
        }
        let blocks: Vec<&TirBlock> = match &expr.kind {
            TirExprKind::Block(block) => vec![block],
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => std::iter::once(then_branch).chain(else_branch).collect(),
            TirExprKind::Match { arms, .. } => {
                return arms
                    .iter()
                    .flat_map(|arm| self.detached_in_value_position(&arm.body))
                    .collect();
            }
            _ => return Vec::new(),
        };
        blocks
            .into_iter()
            .filter_map(block_value)
            .flat_map(|value| self.detached_in_value_position(value))
            .collect()
    }

    /// Walk a closure body against its own local namespace: its parameters hold
    /// the slots below `body_locals`, and the temps it borrows belong to its
    /// own address-taken set, which is what `boxing` reads on the way in.
    fn visit_closure(
        &mut self,
        param_count: usize,
        body: &mut TirExpr,
        address_taken_locals: &mut IndexSet<u32>,
        body_locals: &mut Vec<TirLocal>,
    ) -> bool {
        let local_base = u32::try_from(param_count).unwrap();
        let mut inner = WriteBack {
            type_table: self.type_table,
            escaping: self.escaping,
            local_count: local_base + u32::try_from(body_locals.len()).unwrap(),
            local_base,
            locals: std::mem::take(body_locals),
            borrowed_temps: Vec::new(),
            refused: None,
        };
        let changed = inner.visit_expr(body);
        *body_locals = inner.locals;
        address_taken_locals.extend(inner.borrowed_temps);
        if let Some((span, reason)) = inner.refused {
            self.refuse(span, reason);
        }
        changed
    }

    /// Whether a `&mut` to a place of this type is a detached box — of the
    /// replace types only a `variant` gets here, generic or not.
    fn detaches(&self, place_type: TypeId) -> bool {
        let peeled = self.type_table.get_ultimate_base_type(place_type);
        match self.type_table.get(peeled) {
            ResolvedType::Variant { .. } => true,
            ResolvedType::GenericInstance { def, .. } => {
                self.type_table.variant_template_cases(*def).is_some()
            }
            _ => false,
        }
    }

    /// The place a detached `&mut` borrows, if `arg` is one. A borrow of a
    /// local is not detached — that box *is* the local's slot — but a borrow
    /// through anything else lands in a box nothing else can see. `&mut xs[i]`
    /// arrives here as `&mut *xs.index_ref(i)`, hence the deref.
    fn detached_borrow<'e>(&self, arg: &'e TirExpr) -> Option<&'e TirExpr> {
        let TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: place,
        } = &arg.kind
        else {
            return None;
        };
        let detached = match &place.kind {
            TirExprKind::FieldAccess { .. } | TirExprKind::Index { .. } => true,
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => !matches!(
                inner.kind,
                TirExprKind::Local { .. } | TirExprKind::Capture { .. }
            ),
            _ => false,
        };
        (detached && self.detaches(place.type_id)).then_some(place.as_ref())
    }

    /// The subset [`Self::wrap`] can also store back to: a place this pass can
    /// spell as an assignment target. An element needs an `index_assign` the
    /// pass cannot synthesise, so it is left to the refusals.
    fn detached_place<'e>(&self, arg: &'e TirExpr) -> Option<&'e TirExpr> {
        let place = self.detached_borrow(arg)?;
        matches!(
            place.kind,
            TirExprKind::FieldAccess { .. } | TirExprKind::Index { .. }
        )
        .then_some(place)
    }

    /// Read every step of `place` that is not already a slot into a temp, so
    /// spelling the place a second time re-runs no side effect.
    fn hoist_place(&mut self, place: &mut TirExpr, prefix: &mut Vec<TirStmt>) {
        match &mut place.kind {
            TirExprKind::FieldAccess { expr, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr,
            } => self.hoist_place(expr, prefix),
            TirExprKind::Index { expr, index } => {
                self.hoist_place(expr, prefix);
                self.hoist_operand(index, prefix);
            }
            _ => self.hoist_operand(place, prefix),
        }
    }

    /// Bind `expr` to a temp and leave a read of it behind, unless re-reading
    /// it already costs nothing.
    fn hoist_operand(&mut self, expr: &mut TirExpr, prefix: &mut Vec<TirStmt>) {
        if matches!(
            expr.kind,
            TirExprKind::Local { .. }
                | TirExprKind::Capture { .. }
                | TirExprKind::IntLiteral { .. }
                | TirExprKind::BoolLiteral(_)
                | TirExprKind::CharLiteral(_)
        ) {
            return;
        }
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span);
        let value = std::mem::replace(expr, placeholder);
        *expr = self.bind(prefix, "place_", value, false).read();
    }

    /// `{ let t = place; let b = t; let r = f(&mut t); if t !== b { place = t }; r }`
    /// — the call keeps its position, so a `?` on it still sees the write-back
    /// run first.
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
            TirExprKind::IndirectCall { callee, args } => {
                let stores = match self.type_table.get(callee.type_id) {
                    ResolvedType::Function { stores, .. } => stores.iter().copied().collect(),
                    // A callee whose type says nothing has declared nothing it
                    // keeps, which is not the same as keeping nothing.
                    _ => (0..u32::try_from(args.len()).unwrap()).collect(),
                };
                (
                    "a function value".to_string(),
                    stores,
                    args.iter_mut().collect(),
                )
            }
            _ => return,
        };
        let detached: Vec<bool> = args
            .iter()
            .map(|arg| self.detached_place(arg).is_some())
            .collect();
        if !detached.iter().any(|&d| d) {
            return;
        }
        let mut prefix: Vec<TirStmt> = Vec::new();
        let mut write_backs: Vec<TirStmt> = Vec::new();
        for (position, arg) in args.into_iter().enumerate() {
            if !detached[position] {
                // The prefix runs ahead of the call, so an argument left in the
                // call would evaluate after a place this loop already read.
                // Reading it here keeps the arguments in source order.
                self.hoist_operand(arg, &mut prefix);
                continue;
            }
            let place = self.detached_place(arg).expect("detached above");
            if escaping.contains(&u32::try_from(position).unwrap()) {
                self.refuse(
                    arg.span,
                    format!("'{callee}' stores it, so it outlives the call"),
                );
                continue;
            }
            let span = arg.span;
            let mut place = place.clone();
            // The place is spelled twice — read before the call, assigned
            // after — so anything in it that is not a plain slot is read once
            // into a temp first.
            self.hoist_place(&mut place, &mut prefix);
            // The temp aliases the place, so payload mutation through it lands
            // without any store back.
            let temp = self.bind(&mut prefix, "", place.clone(), true);
            self.borrowed_temps.push(temp.index);
            // An identity witness, never a copy: a copy would make every call
            // look like a whole-value write.
            let before = self.bind(&mut prefix, "before_", temp.read(), false);
            // Store back only what the callee replaced. An unconditional store
            // would also undo a write the callee made through another route to
            // the same place — `self`, or a sibling `&mut` argument.
            write_backs.push(TirStmt::new(
                TirStmtKind::If {
                    condition: TirExpr::new(
                        TirExprKind::Binary {
                            left: Box::new(temp.read()),
                            op: TirBinaryOp::RefNotEq,
                            right: Box::new(before.read()),
                        },
                        TypeTable::BOOL,
                        span,
                    ),
                    then_block: TirBlock {
                        stmts: vec![TirStmt::new(
                            TirStmtKind::Expr(TirExpr::new(
                                TirExprKind::Assign {
                                    target: Box::new(place),
                                    value: Box::new(temp.read()),
                                },
                                TypeTable::UNIT,
                                span,
                            )),
                            span,
                        )],
                        span,
                    },
                    else_block: None,
                },
                span,
            ));
            *arg = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::MutRef,
                    expr: Box::new(temp.read()),
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
        let mut stmts = prefix;
        let result = self.bind(&mut stmts, "result_", original, false);
        stmts.append(&mut write_backs);
        stmts.push(TirStmt::new(TirStmtKind::Expr(result.read()), span));
        *call = TirExpr::new(
            TirExprKind::Block(TirBlock { stmts, span }),
            result_type,
            span,
        );
    }
}

/// The expression a block evaluates to, if its last statement is one.
fn block_value(block: &TirBlock) -> Option<&TirExpr> {
    match block.stmts.last().map(|stmt| &stmt.kind) {
        Some(TirStmtKind::Expr(expr)) => Some(expr),
        _ => None,
    }
}

impl TirOptVisitor for WriteBack<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        match &stmt.kind {
            // A TIR `Let` is a single binding; a destructuring `let` lowered to
            // a pattern, which binds through the borrow rather than keeping it.
            TirStmtKind::Let { value, .. } => self.refuse_stored(value, "a variable"),
            TirStmtKind::Return { value: Some(value) } | TirStmtKind::TaskReturn { value } => {
                self.refuse_stored(value, "the returned value");
            }
            _ => {}
        }
        opt_walk_stmt(self, stmt)
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        if let TirExprKind::Closure {
            params,
            body,
            address_taken_locals,
            body_locals,
            ..
        } = &mut expr.kind
        {
            return self.visit_closure(params.len(), body, address_taken_locals, body_locals);
        }
        match &expr.kind {
            TirExprKind::VariantConstruct {
                payload: Some(payload),
                ..
            } => self.refuse_stored(payload, "a variant payload"),
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.refuse_stored(&field.value, "a struct field");
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    self.refuse_stored(element, "an element");
                }
            }
            TirExprKind::Assign { value, .. } => self.refuse_stored(value, "a place"),
            _ => {}
        }
        let changed = opt_walk_expr(self, expr);
        let planted = self.borrowed_temps.len();
        self.wrap(expr);
        changed || self.borrowed_temps.len() != planted
    }
}
