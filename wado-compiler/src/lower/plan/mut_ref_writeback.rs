//! Borrow a temp for a `&mut` to a variant field and store it back after the
//! call. See `docs/wep-2026-06-13-reference-representation.md`.

use super::value_copy::callgraph::CallGraph;
use super::value_copy::funcset::FuncKeyMap;
use super::whole_value_writes::{self, WholeValueWrites};
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
pub fn insert_write_backs(
    flat: &mut FlatPackage,
    call_graph: &CallGraph,
    errors: &dyn ErrorSink,
) -> Result<(), Bail> {
    let escaping = escaping_params(flat);
    let type_table = flat.type_table.clone();
    let type_table = type_table.borrow();
    let replaced = whole_value_writes::compute(flat, call_graph, &type_table);
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        let local_count = func.local_count;
        let locals = std::mem::take(&mut func.locals);
        let replaced_locals = func
            .body
            .as_ref()
            .map(|body| {
                whole_value_writes::replaced_locals(
                    whole_value_writes::Body::Block(body),
                    &replaced,
                    &type_table,
                )
            })
            .unwrap_or_default();
        let mut pass = WriteBack {
            type_table: &type_table,
            escaping: &escaping,
            replaced: &replaced,
            replaced_locals,
            local_count,
            local_base: 0,
            locals,
            detached_locals: IndexSet::default(),
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
    /// Positions a callee replaces outright — what a lost write-back costs.
    replaced: &'a WholeValueWrites,
    /// Local slots this body replaces through, so a binding used only to mutate
    /// a payload is not refused.
    replaced_locals: IndexSet<u32>,
    local_count: u32,
    /// Locals from `local_base` up. A function owns its parameters' slots too,
    /// so its base is 0; a closure keeps parameters outside `body_locals`.
    local_base: u32,
    locals: Vec<TirLocal>,
    /// Local slots this body bound a detached borrow to. Reading one yields
    /// that borrow, so a sink reached through the variable is the same escape
    /// as spelling the borrow there.
    detached_locals: IndexSet<u32>,
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
    /// Bind `value` to a fresh local. `skip_copy` holds for a temp standing in
    /// for storage the caller still owns — a place, or a borrow of one — and
    /// for a fresh result. A by-value argument is neither, and value semantics
    /// still owe it its copy.
    fn bind(
        &mut self,
        prefix: &mut Vec<TirStmt>,
        kind: &str,
        value: TirExpr,
        is_mut: bool,
        skip_copy: bool,
    ) -> Temp {
        assert_eq!(
            u32::try_from(self.locals.len()).unwrap() + self.local_base,
            self.local_count
        );
        let (type_id, span) = (value.type_id, value.span);
        let index = self.local_count;
        self.local_count += 1;
        self.locals.push(TirLocal::synth(index, type_id, is_mut));
        let name = format!("__write_back_{kind}{index}");
        prefix.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.clone(),
                local_index: index,
                is_mut,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: skip_copy,
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
        if let Some(place) = self.detached_in_value_position(expr) {
            self.refuse(place, format!("{sink} holds it past the borrow"));
        }
    }

    /// A detached borrow `expr` can yield, following the value positions one
    /// reaches storage through. The first is the whole answer, since only the
    /// first refusal is reported.
    fn detached_in_value_position(&self, expr: &TirExpr) -> Option<Span> {
        if let Some(place) = self.detached_borrow(expr) {
            return Some(place.span);
        }
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                self.detached_locals.contains(index).then_some(expr.span)
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                block_value(block).and_then(|value| self.detached_in_value_position(value))
            }
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => std::iter::once(then_branch)
                .chain(else_branch)
                .filter_map(block_value)
                .find_map(|value| self.detached_in_value_position(value)),
            TirExprKind::Match { arms, .. } => arms
                .iter()
                .find_map(|arm| self.detached_in_value_position(&arm.body)),
            _ => None,
        }
    }

    /// Walk a closure body against its own local namespace, seeding its own
    /// address-taken set — which is what `boxing` reads on the way in.
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
            replaced: self.replaced,
            // A closure body's slots are its own, so what it replaces through
            // is its own question too.
            replaced_locals: whole_value_writes::replaced_locals(
                whole_value_writes::Body::Expr(body),
                self.replaced,
                self.type_table,
            ),
            local_count: local_base + u32::try_from(body_locals.len()).unwrap(),
            local_base,
            locals: std::mem::take(body_locals),
            detached_locals: IndexSet::default(),
            borrowed_temps: Vec::new(),
            refused: None,
        };
        // An expression-bodied closure yields its body; a block-bodied one must
        // spell `return`, which `visit_stmt` catches.
        inner.refuse_stored(body, "the returned value");
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
        let peeled = self.type_table.representation_head(place_type);
        match self.type_table.get(peeled) {
            ResolvedType::Variant { .. } => true,
            ResolvedType::GenericInstance { def, .. } => {
                self.type_table.variant_template_cases(*def).is_some()
            }
            _ => false,
        }
    }

    /// Whether `arg` *takes* a detached borrow, by any route. Forwarding one it
    /// was already handed is not taking one — the box is the caller's, and
    /// whether that detached is the caller's question. This is the safety test;
    /// the shape tests below only decide whether a write-back can be emitted.
    fn takes_a_detached_borrow(&self, arg: &TirExpr) -> bool {
        self.detached_in_value_position(arg).is_some()
    }

    /// The place a detached `&mut` borrows, if `arg` is one. Of the places only
    /// `&mut local` is undetached — [`super::boxing`] collapses it to the
    /// local's own slot, so that box *is* the storage. Every other one, a
    /// capture and a deref alike, is wrapped in a fresh box nothing else can
    /// see: `&mut *p` re-boxes what `p` points at, whatever `p` is. `&mut xs[i]`
    /// arrives here as `&mut *xs.index_ref(i)`, hence the deref.
    fn detached_borrow<'e>(&self, arg: &'e TirExpr) -> Option<&'e TirExpr> {
        let TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: place,
        } = &arg.kind
        else {
            return None;
        };
        let detached = matches!(
            place.kind,
            TirExprKind::FieldAccess { .. }
                | TirExprKind::Index { .. }
                | TirExprKind::Capture { .. }
                | TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    ..
                }
        );
        (detached && self.detaches(place.type_id)).then_some(place.as_ref())
    }

    /// The subset [`Self::wrap`] can also store back to: a place whose
    /// assignment reaches the storage the temp was read from. A projection
    /// always does — it lands on whatever object the base evaluated to, which
    /// is the one the read came from. A whole `*p` does only when `p` names a
    /// slot: a source `xs[i]` reaches here as `*xs.index_ref(i)`, whose box is
    /// already detached from the element, so storing back through it needs an
    /// `index_assign` the pass cannot synthesise. The `Index` arm is for a
    /// tuple's positional field. A whole capture is out for the same reason a
    /// list element is: assigning it lands in the closure's environment, which
    /// closure lowering filled with a copy of the enclosing slot before this
    /// pass ran, so the store would be silently dropped. A projection *through*
    /// a capture stays in — it lands on the object the capture copied.
    fn detached_place<'e>(&self, arg: &'e TirExpr) -> Option<&'e TirExpr> {
        let place = self.detached_borrow(arg)?;
        match &place.kind {
            TirExprKind::FieldAccess { .. } | TirExprKind::Index { .. } => Some(place),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => names_a_slot(inner).then_some(place),
            _ => None,
        }
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
                self.hoist_operand(index, prefix, true);
            }
            // A step of the place stands in for the place's own storage.
            _ => self.hoist_operand(place, prefix, true),
        }
    }

    /// Bind `expr` to a temp and leave a read of it behind, unless re-reading
    /// it costs nothing and still names the same thing — a slot read as a step
    /// of a place, which the place's own assignment re-reads the same way.
    /// `skip_copy` as in [`Self::bind`].
    fn hoist_operand(&mut self, expr: &mut TirExpr, prefix: &mut Vec<TirStmt>, skip_copy: bool) {
        if matches!(
            expr.kind,
            TirExprKind::Local { .. } | TirExprKind::Capture { .. }
        ) {
            return;
        }
        self.hoist_argument(expr, prefix, skip_copy);
    }

    /// The same for an argument moved ahead of the call, where a slot is *not*
    /// free to re-read: a place this loop reads after it can write that slot —
    /// `f(n, &mut xs[bump(&mut n)].item)` reads `n` before `bump`. Only a
    /// literal, which no prefix statement can move, stays in the call.
    fn hoist_argument(&mut self, expr: &mut TirExpr, prefix: &mut Vec<TirStmt>, skip_copy: bool) {
        if matches!(
            expr.kind,
            TirExprKind::IntLiteral { .. }
                | TirExprKind::FloatLiteral { .. }
                | TirExprKind::BoolLiteral(_)
                | TirExprKind::CharLiteral(_)
                | TirExprKind::StringLiteral(_)
                | TirExprKind::Unit
                | TirExprKind::Null
        ) {
            return;
        }
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span);
        let value = std::mem::replace(expr, placeholder);
        *expr = self.bind(prefix, "place_", value, false, skip_copy).read();
    }

    /// `{ let t = place; let b = t; let r = f(&mut t); if t !== b { place = t }; r }`
    /// — the call keeps its position, so a `?` on it still sees the write-back
    /// run first.
    fn wrap(&mut self, call: &mut TirExpr) {
        // Every call node in the program reaches here, so nothing is allocated
        // until an argument is one of the few this pass has anything to say
        // about.
        let touched = match &call.kind {
            TirExprKind::Call { args, .. } => args
                .iter()
                .any(|arg| self.takes_a_detached_borrow(&arg.expr)),
            TirExprKind::IndirectCall { args, .. } => {
                args.iter().any(|arg| self.takes_a_detached_borrow(arg))
            }
            _ => return,
        };
        if !touched {
            return;
        }
        let mut prefix: Vec<TirStmt> = Vec::new();
        // An indirect callee is an expression, and it runs before the
        // arguments, so it has to reach the prefix before any place does — a
        // slot no more freely than anything else, since a place read after it
        // can write that slot: `g(&mut xs[swap(&mut g)].item)` calls the `g` it
        // was before `swap`.
        if let TirExprKind::IndirectCall { callee, args } = &mut call.kind
            && args.iter().any(|arg| self.detached_place(arg).is_some())
        {
            self.hoist_argument(callee, &mut prefix, true);
        }
        // A direct callee declares the positions it keeps a borrow past the
        // call; an indirect one carries them on its function type.
        let (callee, escaping, replaced, has_receiver, args): (_, _, _, _, Vec<&mut TirExpr>) =
            match &mut call.kind {
                TirExprKind::Call {
                    func,
                    args,
                    has_receiver,
                    ..
                } => (
                    func.name.clone(),
                    self.escaping
                        .get(&func.module_source, &func.name)
                        .cloned()
                        .unwrap_or_default(),
                    self.replaced
                        .get(&func.module_source, &func.name)
                        .cloned()
                        .unwrap_or_default(),
                    *has_receiver,
                    args.iter_mut().map(|a| &mut a.expr).collect(),
                ),
                TirExprKind::IndirectCall { callee, args } => {
                    let every =
                        || -> IndexSet<u32> { (0..u32::try_from(args.len()).unwrap()).collect() };
                    let stores = match self.type_table.get(callee.type_id) {
                        ResolvedType::Function { stores, .. } => stores.iter().copied().collect(),
                        // A callee whose type says nothing has declared nothing
                        // it keeps, which is not the same as keeping nothing.
                        _ => every(),
                    };
                    // A functor says nothing about what it replaces either.
                    let replaced = every();
                    (
                        "a function value".to_string(),
                        stores,
                        replaced,
                        false,
                        args.iter_mut().collect(),
                    )
                }
                _ => return,
            };
        // What is at risk is decided by the argument's *type* and what the
        // callee does with it, never by the shape of the expression: a shape
        // this pass fails to recognise then costs a refusal, not a lost write.
        let at_risk: Vec<bool> = args
            .iter()
            .enumerate()
            .map(|(position, arg)| {
                self.takes_a_detached_borrow(arg)
                    && (escaping.contains(&u32::try_from(position).unwrap())
                        || replaced.contains(&u32::try_from(position).unwrap()))
            })
            .collect();
        // Only a position the callee can replace has anything to store back.
        // `replaced` over-approximates, so gating on it never hides a write:
        // saying "replaces" where it does not costs a redundant store, and the
        // analysis has no path that says "does not" where it does.
        let detached: Vec<bool> = args
            .iter()
            .enumerate()
            .map(|(position, arg)| {
                replaced.contains(&u32::try_from(position).unwrap())
                    && self.detached_place(arg).is_some()
            })
            .collect();
        // Everything up to the last place moves to the prefix; what follows it
        // already evaluates after that place, so it stays in the call.
        let last_place = detached.iter().rposition(|&d| d);
        if last_place.is_none() && !at_risk.iter().any(|&r| r) {
            return;
        }
        let last_place = last_place.unwrap_or(0);
        let mut write_backs: Vec<TirStmt> = Vec::new();
        for (position, arg) in args.into_iter().enumerate() {
            if !detached[position] {
                // No write-back is possible for this shape. It only matters if
                // the callee would actually replace what it names.
                if at_risk[position] {
                    // Why there is no write-back point differs: a stored borrow
                    // outlives every point in this body, where a replaced one
                    // has a point but no place this pass can spell.
                    let why = if escaping.contains(&u32::try_from(position).unwrap()) {
                        "stores it, so it outlives the call"
                    } else {
                        "replaces it, and no place here can be stored back to"
                    };
                    self.refuse(arg.span, format!("'{callee}' {why}"));
                    continue;
                }
                // The prefix runs ahead of the call, so an argument left in the
                // call would evaluate after a place this loop already read.
                // Reading it here keeps the arguments in source order — as the
                // call's own argument, so value semantics still copy it. A
                // receiver is the exception: `lower` never copies one, so its
                // temp stands in for the caller's storage rather than a
                // snapshot of it.
                if position < last_place {
                    self.hoist_argument(arg, &mut prefix, has_receiver && position == 0);
                }
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
            let temp = self.bind(&mut prefix, "", place.clone(), true, true);
            self.borrowed_temps.push(temp.index);
            // An identity witness, never a copy: a copy would make every call
            // look like a whole-value write.
            let before = self.bind(&mut prefix, "before_", temp.read(), false, true);
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
        let result = self.bind(&mut stmts, "result_", original, false, true);
        stmts.append(&mut write_backs);
        stmts.push(TirStmt::new(TirStmtKind::Expr(result.read()), span));
        *call = TirExpr::new(
            TirExprKind::Block(TirBlock { stmts, span }),
            result_type,
            span,
        );
    }
}

/// Whether `expr` names storage this body can spell again — a projection chain
/// rooted at a local or a capture, rather than a value a call produced.
fn names_a_slot(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::Local { .. } | TirExprKind::Capture { .. } => true,
        TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::Index { expr, .. }
        | TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr,
        } => names_a_slot(expr),
        _ => false,
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
            // Only a binding something replaces through loses a write — one used
            // to mutate a payload lands through the box either way.
            TirStmtKind::Let {
                value, local_index, ..
            } => {
                let local_index = *local_index;
                if self.detached_in_value_position(value).is_some() {
                    if self.replaced_locals.contains(&local_index) {
                        self.refuse_stored(value, "a variable");
                    }
                    // Reading the binding hands the same detached borrow on, so
                    // a later sink is the same escape.
                    self.detached_locals.insert(local_index);
                }
            }
            TirStmtKind::Return { value: Some(value) } | TirStmtKind::TaskReturn { value } => {
                self.refuse_stored(value, "the returned value");
            }
            // A labeled block yields through its breaks as well as its tail,
            // which `detached_in_value_position` reads.
            TirStmtKind::Break {
                value: Some(value), ..
            } => self.refuse_stored(value, "the block's value"),
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
