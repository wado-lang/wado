//! Capture by reference a boxed binding a closure only reads, where the owning
//! frame may still write it while the closure lives.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::place::place_root;
use crate::name::{capture_ref_name, is_for_body_label};
use crate::tir::{
    CaptureSource, TirBlock, TirExpr, TirExprKind, TirLocal, TirPattern, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable, capture_source_locals,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};
use crate::token::Span;

pub fn capture_observed_by_ref(flat: &mut FlatPackage) {
    let type_table = flat.type_table.clone();
    let mut type_table = type_table.borrow_mut();
    for func in &flat.functions {
        let mut func = func.borrow_mut();
        let func = &mut *func;
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let mut locals = FrameLocals::Function {
            locals: &mut func.locals,
            local_count: &mut func.local_count,
        };
        rewrite_frame(
            Body::Block(body),
            &mut locals,
            &mut func.address_taken_locals,
            &mut type_table,
        );
    }
}

enum Body<'a> {
    Block(&'a mut TirBlock),
    Expr(&'a mut TirExpr),
}

/// The local table of one frame: a function's, or a closure body's, whose
/// parameters are numbered ahead of its body locals.
enum FrameLocals<'a> {
    Function {
        locals: &'a mut Vec<TirLocal>,
        local_count: &'a mut u32,
    },
    Closure {
        params: &'a [(String, TypeId)],
        body_locals: &'a mut Vec<TirLocal>,
    },
}

impl FrameLocals<'_> {
    /// `None` past the table, which a synthesized frame may leave empty: its
    /// slots cannot be told apart from a fresh one, so it gets no proxy.
    fn local(&self, index: u32) -> Option<(&str, TypeId)> {
        let index = index as usize;
        match self {
            Self::Function { locals, .. } => locals
                .get(index)
                .map(|local| (local.name.as_str(), local.type_id)),
            Self::Closure {
                params,
                body_locals,
            } => match params.get(index) {
                Some((name, type_id)) => Some((name, *type_id)),
                None => body_locals
                    .get(index - params.len())
                    .map(|local| (local.name.as_str(), local.type_id)),
            },
        }
    }

    /// The declared local; `None` for a closure parameter, which no loop of the
    /// body it heads can declare.
    fn declared(&self, index: u32) -> Option<&TirLocal> {
        let index = index as usize;
        match self {
            Self::Function { locals, .. } => locals.get(index),
            Self::Closure {
                params,
                body_locals,
            } => index
                .checked_sub(params.len())
                .and_then(|index| body_locals.get(index)),
        }
    }

    fn alloc(&mut self, name: String, type_id: TypeId) -> u32 {
        let local = TirLocal {
            name,
            type_id,
            is_mut: false,
            span: Span::default(),
        };
        match self {
            Self::Function {
                locals,
                local_count,
            } => {
                assert_eq!(**local_count as usize, locals.len());
                locals.push(local);
                **local_count += 1;
                **local_count - 1
            }
            Self::Closure {
                params,
                body_locals,
            } => {
                body_locals.push(local);
                u32::try_from(params.len() + body_locals.len() - 1).unwrap()
            }
        }
    }
}

fn rewrite_frame(
    mut body: Body,
    locals: &mut FrameLocals,
    address_taken: &mut IndexSet<u32>,
    type_table: &mut TypeTable,
) {
    let mut headers = ForHeaders {
        locals,
        loop_spans: Vec::new(),
        captured: IndexMap::default(),
    };
    match &body {
        Body::Block(block) => headers.visit_block(block),
        Body::Expr(expr) => headers.visit_expr(expr),
    }
    let mut scan = Scan {
        boundaries: headers.captured,
        ..Scan::default()
    };
    match &body {
        Body::Block(block) => scan.visit_block(block),
        Body::Expr(expr) => scan.visit_expr(expr),
    }
    let complete = scan
        .highest_local
        .is_none_or(|index| locals.local(index).is_some());
    let by_ref = scan
        .sites
        .iter()
        .map(|site| {
            site.captured
                .iter()
                .filter(|(_, local, type_id)| {
                    complete
                        && type_table.is_boxed_reference_target(*type_id)
                        && (address_taken.contains(local) || scan.written_after(*local, site))
                })
                .map(|(slot, _, _)| *slot)
                .collect()
        })
        .collect();
    let mut rewriter = Rewriter {
        by_ref,
        next_site: 0,
        locals,
        address_taken,
        type_table,
    };
    match &mut body {
        Body::Block(block) => rewriter.visit_block(block),
        Body::Expr(expr) => rewriter.visit_expr(expr),
    }
    let mut redeclare = Redeclare {
        after: scan
            .boundaries
            .into_iter()
            .map(|(label, headers)| {
                let stmts = headers
                    .into_iter()
                    .filter(|header| address_taken.contains(header))
                    .map(|header| redeclaration(locals, header))
                    .collect();
                (label, stmts)
            })
            .collect(),
    };
    match &mut body {
        Body::Block(block) => redeclare.visit_block(block),
        Body::Expr(expr) => redeclare.visit_expr(expr),
    }
}

/// Finds, by `for` body label, the mutable bindings a C-style `for` header
/// declares that a closure built in its body captures, itself or through a borrow.
struct ForHeaders<'a, 'l> {
    locals: &'a FrameLocals<'l>,
    loop_spans: Vec<Span>,
    captured: IndexMap<String, Vec<u32>>,
}

impl TirRefVisitor for ForHeaders<'_, '_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Loop { .. } => {
                self.loop_spans.push(stmt.span);
                self.walk_stmt(stmt);
                self.loop_spans.pop();
                return;
            }
            TirStmtKind::LabeledBlock { label, block } if is_for_body_label(label) => {
                let Some(&loop_span) = self.loop_spans.last() else {
                    unreachable!("a `for` body is minted inside the loop it runs in");
                };
                let mut body = BodyCaptures::default();
                body.visit_block(block);
                let headers = body
                    .captured
                    .iter()
                    .map(|local| body.borrows.get(local).copied().unwrap_or(*local))
                    .filter(|&local| {
                        self.locals.declared(local).is_some_and(|declared| {
                            declared.is_mut
                                && encloses(&loop_span, &declared.span)
                                && !encloses(&stmt.span, &declared.span)
                        })
                    })
                    .collect::<IndexSet<_>>();
                if !headers.is_empty() {
                    self.captured
                        .insert(label.clone(), headers.into_iter().collect());
                }
            }
            TirStmtKind::Expr(_)
            | TirStmtKind::Let { .. }
            | TirStmtKind::LetDestructure { .. }
            | TirStmtKind::Return { .. }
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::If { .. }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue
            | TirStmtKind::LabeledBlock { .. }
            | TirStmtKind::VariadicForOf { .. } => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        self.walk_expr_in_frame(expr);
    }
}

/// The frame locals the closures built in a body capture, and the locals the
/// body binds to a borrow of another, by the borrowed one.
#[derive(Default)]
struct BodyCaptures {
    captured: IndexSet<u32>,
    borrows: IndexMap<u32, u32>,
}

impl TirRefVisitor for BodyCaptures {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr,
            } = &value.kind
            && let TirExprKind::Local { index, .. } = &expr.kind
        {
            self.borrows.insert(*local_index, *index);
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Closure { captures, .. } = &expr.kind {
            self.captured.extend(capture_source_locals(captures));
        }
        self.walk_expr_in_frame(expr);
    }
}

/// `let mut header = header;`: a fresh binding for the next iteration, so the
/// closures of this one keep the box they captured.
fn redeclaration(locals: &FrameLocals, header: u32) -> TirStmt {
    let Some((name, type_id)) = locals.local(header) else {
        unreachable!("`ForHeaders` takes only locals the frame declares");
    };
    TirStmt::new(
        TirStmtKind::Let {
            name: name.to_string(),
            local_index: header,
            is_mut: true,
            is_reactive: false,
            type_id,
            value: TirExpr::new(
                TirExprKind::Local {
                    index: header,
                    name: name.to_string(),
                },
                type_id,
                Span::default(),
            ),
            skip_value_copy: false,
        },
        Span::default(),
    )
}

/// Inserts each `for` body's redeclarations after it, where a `continue` lands
/// and the update and the next condition run.
struct Redeclare {
    after: IndexMap<String, Vec<TirStmt>>,
}

impl TirMutVisitor for Redeclare {
    fn visit_block(&mut self, block: &mut TirBlock) {
        self.walk_block(block);
        let Some(at) = block.stmts.iter().position(|stmt| {
            matches!(&stmt.kind, TirStmtKind::LabeledBlock { label, .. } if self.after.contains_key(label))
        }) else {
            return;
        };
        let TirStmtKind::LabeledBlock { label, .. } = &block.stmts[at].kind else {
            unreachable!("`position` found a labeled block");
        };
        let Some(stmts) = self.after.swap_remove(label) else {
            unreachable!("`position` found a key of `after`");
        };
        let next = at + 1;
        block.stmts.splice(next..next, stmts);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // A closure body is a frame of its own, settled when `Rewriter` reaches it.
        if let TirExprKind::Closure { .. } = &expr.kind {
            return;
        }
        self.walk_expr(expr);
    }
}

/// Where a point of the walk runs: its place in evaluation order, and the loops
/// around it.
#[derive(Clone)]
struct At {
    clock: u32,
    loops: Vec<u32>,
}

/// Where a closure is built, and the frame locals it captures, by slot and by
/// the type the capture holds.
struct Site {
    at: At,
    captured: Vec<(u32, u32, TypeId)>,
}

/// A frame local's binding taking effect, or a whole or partial assignment to it.
struct Event {
    local: u32,
    at: At,
}

/// The closures of one frame in walk order, and the bindings and assignments
/// of its locals, each placed where it runs.
#[derive(Default)]
struct Scan {
    clock: u32,
    loops: Vec<u32>,
    loop_count: u32,
    /// What [`ForHeaders`] found: each `for` body's header bindings, which the
    /// iteration after it binds afresh.
    boundaries: IndexMap<String, Vec<u32>>,
    sites: Vec<Site>,
    bindings: Vec<Event>,
    writes: Vec<Event>,
    highest_local: Option<u32>,
}

impl Scan {
    /// Whether the closure built at `site` can observe a write to `local`: one
    /// that reaches the binding it captured, in this iteration or a later one.
    fn written_after(&self, local: u32, site: &Site) -> bool {
        let site = &site.at;
        let bindings = self
            .bindings
            .iter()
            .filter(|b| b.local == local)
            .map(|b| &b.at)
            .collect::<Vec<_>>();
        self.writes.iter().any(|w| {
            let write = &w.at;
            w.local == local
                && if write.clock > site.clock {
                    !bindings
                        .iter()
                        .any(|b| site.clock < b.clock && b.clock < write.clock)
                } else {
                    write.loops.iter().any(|l| {
                        site.loops.contains(l) && !bindings.iter().any(|b| b.loops.contains(l))
                    })
                }
        })
    }

    fn now(&mut self) -> At {
        self.clock += 1;
        At {
            clock: self.clock,
            loops: self.loops.clone(),
        }
    }

    fn saw_local(&mut self, index: u32) {
        self.highest_local = self.highest_local.max(Some(index));
    }

    fn bind(&mut self, local: u32) {
        self.saw_local(local);
        let at = self.now();
        self.bindings.push(Event { local, at });
    }

    fn in_loop(&mut self, walk: impl FnOnce(&mut Self)) {
        self.loops.push(self.loop_count);
        self.loop_count += 1;
        walk(self);
        self.loops.pop();
    }
}

impl TirRefVisitor for Scan {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Loop { .. } => self.in_loop(|scan| scan.walk_stmt(stmt)),
            TirStmtKind::VariadicForOf { binding_local, .. } => self.in_loop(|scan| {
                scan.bind(*binding_local);
                scan.walk_stmt(stmt);
            }),
            TirStmtKind::LabeledBlock { label, .. } => {
                self.walk_stmt(stmt);
                if let Some(headers) = self.boundaries.get(label).cloned() {
                    for header in headers {
                        self.bind(header);
                    }
                }
            }
            TirStmtKind::Let { local_index, .. } => {
                self.walk_stmt(stmt);
                self.bind(*local_index);
            }
            TirStmtKind::Expr(_)
            | TirStmtKind::Return { .. }
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::If { .. }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue
            | TirStmtKind::LetDestructure { .. } => self.walk_stmt(stmt),
        }
    }

    fn visit_pattern(&mut self, pattern: &TirPattern) {
        if let TirPattern::Binding { local_index, .. } | TirPattern::Narrow { local_index, .. } =
            pattern
        {
            self.bind(*local_index);
        }
        self.walk_pattern(pattern);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        let at = self.now();
        if let TirExprKind::Closure { captures, body, .. } = &expr.kind {
            let captured = captures
                .iter()
                .enumerate()
                .filter_map(|(slot, capture)| {
                    let local = capture.source.local()?;
                    let slot = u32::try_from(slot).unwrap();
                    (!reads_through_ref(body, slot)).then_some((slot, local, capture.type_id))
                })
                .collect::<Vec<_>>();
            for (_, local, _) in &captured {
                self.saw_local(*local);
            }
            self.sites.push(Site { at, captured });
            return;
        }
        if let TirExprKind::Local { index, .. } = &expr.kind {
            self.saw_local(*index);
        }
        if let TirExprKind::VariadicTupleComprehension { binding_local, .. } = &expr.kind {
            self.in_loop(|scan| {
                scan.bind(*binding_local);
                scan.walk_expr(expr);
            });
            return;
        }
        self.walk_expr(expr);
        // A write through a dereference lands where the reference was taken
        // from, and taking it already address-took that local.
        if let TirExprKind::Assign { target, .. } = &expr.kind
            && let Some(local) = place_root(target)
        {
            let at = self.now();
            self.writes.push(Event { local, at });
        }
    }
}

fn encloses(outer: &Span, inner: &Span) -> bool {
    outer.space == inner.space && outer.start <= inner.start && inner.end <= outer.end
}

/// Whether `body` reads slot `slot` through a dereference: the binding was
/// already boxed where the closure was built, so the slot holds its box.
fn reads_through_ref(body: &TirExpr, slot: u32) -> bool {
    let mut finder = DerefRead { slot, found: false };
    finder.visit_expr(body);
    finder.found
}

struct DerefRead {
    slot: u32,
    found: bool,
}

impl TirRefVisitor for DerefRead {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } = &expr.kind
            && let TirExprKind::Capture { index, .. } = &inner.kind
            && *index == self.slot
        {
            self.found = true;
            return;
        }
        // A nested body numbers its own environment: follow the slot only
        // where the closure takes it on.
        if let TirExprKind::Closure { captures, body, .. } = &expr.kind {
            for (slot, capture) in captures.iter().enumerate() {
                if capture.source == CaptureSource::Capture(self.slot) {
                    self.found |= reads_through_ref(body, u32::try_from(slot).unwrap());
                }
            }
            return;
        }
        self.walk_expr(expr);
    }
}

/// Moves each capture `Scan` settled on onto a `&T` proxy bound just ahead of
/// the closure, then settles the closure's own frame the same way.
struct Rewriter<'a, 'l> {
    by_ref: Vec<Vec<u32>>,
    next_site: usize,
    locals: &'a mut FrameLocals<'l>,
    address_taken: &'a mut IndexSet<u32>,
    type_table: &'a mut TypeTable,
}

impl TirMutVisitor for Rewriter<'_, '_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        let TirExprKind::Closure {
            params,
            body,
            captures,
            address_taken_locals,
            body_locals,
            ..
        } = &mut expr.kind
        else {
            self.walk_expr(expr);
            return;
        };
        let slots = std::mem::take(&mut self.by_ref[self.next_site]);
        self.next_site += 1;
        let span = expr.span;
        let mut proxies = Vec::with_capacity(slots.len());
        for slot in slots {
            let capture = &mut captures[slot as usize];
            let Some(local) = capture.source.local() else {
                unreachable!("`Scan` takes only captures read off a frame local");
            };
            let Some((name, local_type)) = self.locals.local(local) else {
                unreachable!("`rewrite_frame` gives no proxy in a frame it cannot number");
            };
            let name = name.to_string();
            let ref_type = self.type_table.make_ref(capture.type_id);
            let proxy_name = capture_ref_name(&name);
            let proxy = self.locals.alloc(proxy_name.clone(), ref_type);
            self.address_taken.insert(local);
            let borrow = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(TirExpr::new(
                        TirExprKind::Local { index: local, name },
                        local_type,
                        span,
                    )),
                },
                ref_type,
                span,
            );
            proxies.push(TirStmt::new(
                TirStmtKind::Let {
                    name: proxy_name,
                    local_index: proxy,
                    is_mut: false,
                    is_reactive: false,
                    type_id: ref_type,
                    value: borrow,
                    skip_value_copy: false,
                },
                span,
            ));
            capture.source = CaptureSource::Local(proxy);
            capture.type_id = ref_type;
            SlotThroughRef { slot, ref_type }.visit_expr(body);
        }
        let mut closure_locals = FrameLocals::Closure {
            params: params.as_slice(),
            body_locals,
        };
        rewrite_frame(
            Body::Expr(body),
            &mut closure_locals,
            address_taken_locals,
            self.type_table,
        );
        if proxies.is_empty() {
            return;
        }
        let type_id = expr.type_id;
        let closure = std::mem::replace(expr, TirExpr::new(TirExprKind::Unit, type_id, span));
        proxies.push(TirStmt::new(TirStmtKind::Expr(closure), span));
        *expr = TirExpr::new(
            TirExprKind::Block(TirBlock {
                stmts: proxies,
                span,
            }),
            type_id,
            span,
        );
    }
}

/// Reads environment slot `slot` through the `&T` it now holds, in this body
/// and in every nested closure handed the slot on.
struct SlotThroughRef {
    slot: u32,
    ref_type: TypeId,
}

impl TirMutVisitor for SlotThroughRef {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Capture { index, .. } = &expr.kind
            && *index == self.slot
        {
            let value_type = expr.type_id;
            let span = expr.span;
            let mut capture =
                std::mem::replace(expr, TirExpr::new(TirExprKind::Unit, value_type, span));
            capture.type_id = self.ref_type;
            *expr = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: Box::new(capture),
                },
                value_type,
                span,
            );
            return;
        }
        // A nested body numbers its own environment: follow the slot only
        // where the closure takes it on.
        if let TirExprKind::Closure { captures, body, .. } = &mut expr.kind {
            for (slot, capture) in captures.iter_mut().enumerate() {
                if capture.source == CaptureSource::Capture(self.slot) {
                    capture.type_id = self.ref_type;
                    SlotThroughRef {
                        slot: u32::try_from(slot).unwrap(),
                        ref_type: self.ref_type,
                    }
                    .visit_expr(body);
                }
            }
            return;
        }
        self.walk_expr(expr);
    }
}
