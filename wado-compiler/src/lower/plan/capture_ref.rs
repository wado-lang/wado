//! Capture by reference a boxed binding a closure only reads, where the owning
//! frame may still write it while the closure lives.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::lower::plan::value_copy::place::place_root;
use crate::name::{capture_ref_name, is_for_body_label};
use crate::tir::{
    CaptureSource, TirBlock, TirExpr, TirExprKind, TirLocal, TirPattern, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
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
    let mut scan = Scan::default();
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
}

/// Where a point of the walk runs: its place in evaluation order, the loops
/// around it, and the C-style `for` bodies around it.
#[derive(Clone)]
struct At {
    clock: u32,
    loops: Vec<u32>,
    for_bodies: Vec<u32>,
}

/// Where a closure is built, and the frame locals it captures, by slot and by
/// the type the capture holds.
struct Site {
    at: At,
    captured: Vec<(u32, u32, TypeId)>,
}

/// A whole or partial assignment to a frame local.
struct Write {
    local: u32,
    at: At,
}

/// The closures of one frame in walk order, and the assignments to its
/// locals, each placed where it runs.
#[derive(Default)]
struct Scan {
    clock: u32,
    loops: Vec<u32>,
    next_loop: u32,
    for_bodies: Vec<u32>,
    /// The loop each `for` body belongs to, by the body's own number.
    for_body_loops: Vec<u32>,
    sites: Vec<Site>,
    writes: Vec<Write>,
    highest_local: Option<u32>,
}

impl Scan {
    /// Whether the closure built at `site` can observe a write to `local`: one
    /// after it is built, or in a loop around it on a later iteration.
    fn written_after(&self, local: u32, site: &Site) -> bool {
        self.writes.iter().any(|w| {
            // A `for` header's update belongs to the next iteration, whose
            // bindings a closure built in this one's body does not hold.
            w.local == local
                && !self.in_header_of(&w.at, &site.at)
                && (w.at.clock > site.at.clock
                    || w.at.loops.iter().any(|l| site.at.loops.contains(l)))
        })
    }

    /// Whether `write` runs in the header of a `for` loop whose body holds
    /// `site`.
    fn in_header_of(&self, write: &At, site: &At) -> bool {
        site.for_bodies.iter().any(|body| {
            !write.for_bodies.contains(body)
                && write.loops.contains(&self.for_body_loops[*body as usize])
        })
    }

    fn now(&mut self) -> At {
        self.clock += 1;
        At {
            clock: self.clock,
            loops: self.loops.clone(),
            for_bodies: self.for_bodies.clone(),
        }
    }

    fn saw_local(&mut self, index: u32) {
        self.highest_local = self.highest_local.max(Some(index));
    }

    fn in_loop(&mut self, walk: impl FnOnce(&mut Self)) {
        self.loops.push(self.next_loop);
        self.next_loop += 1;
        walk(self);
        self.loops.pop();
    }

    fn in_for_body(&mut self, walk: impl FnOnce(&mut Self)) {
        let Some(&enclosing) = self.loops.last() else {
            unreachable!("a `for` body is minted inside the loop it runs in");
        };
        self.for_bodies
            .push(u32::try_from(self.for_body_loops.len()).unwrap());
        self.for_body_loops.push(enclosing);
        walk(self);
        self.for_bodies.pop();
    }
}

impl TirRefVisitor for Scan {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Loop { .. } => self.in_loop(|scan| scan.walk_stmt(stmt)),
            TirStmtKind::VariadicForOf { binding_local, .. } => {
                self.saw_local(*binding_local);
                self.in_loop(|scan| scan.walk_stmt(stmt));
            }
            TirStmtKind::LabeledBlock { label, .. } if is_for_body_label(label) => {
                self.in_for_body(|scan| scan.walk_stmt(stmt));
            }
            TirStmtKind::Let { local_index, .. } => {
                self.saw_local(*local_index);
                self.walk_stmt(stmt);
            }
            TirStmtKind::Expr(_)
            | TirStmtKind::Return { .. }
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::If { .. }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue
            | TirStmtKind::LabeledBlock { .. }
            | TirStmtKind::LetDestructure { .. } => self.walk_stmt(stmt),
        }
    }

    fn visit_pattern(&mut self, pattern: &TirPattern) {
        if let TirPattern::Binding { local_index, .. } | TirPattern::Narrow { local_index, .. } =
            pattern
        {
            self.saw_local(*local_index);
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
            self.saw_local(*binding_local);
            self.in_loop(|scan| scan.walk_expr(expr));
            return;
        }
        self.walk_expr(expr);
        // A write through a dereference lands where the reference was taken
        // from, and taking it already address-took that local.
        if let TirExprKind::Assign { target, .. } = &expr.kind
            && let Some(local) = place_root(target)
        {
            let at = self.now();
            self.writes.push(Write { local, at });
        }
    }
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
