//! Capture by reference a binding a closure only reads, where the owning frame
//! may still write it while the closure lives. Anywhere else the value itself
//! is captured: nothing can change it, so it reads what the reference would.
//!
//! Only a binding whose reference is a `Box<T>` cell needs the proxy. Any other
//! `&T` is `T`'s own handle, which the value capture already is.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::name::capture_ref_name;
use crate::tir::{
    CaptureSource, TirBlock, TirExpr, TirExprKind, TirLocal, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

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
    fn local(&self, index: u32) -> (&str, TypeId) {
        let index = index as usize;
        match self {
            Self::Function { locals, .. } => (&locals[index].name, locals[index].type_id),
            Self::Closure {
                params,
                body_locals,
            } => match params.get(index) {
                Some((name, type_id)) => (name, *type_id),
                None => {
                    let local = &body_locals[index - params.len()];
                    (&local.name, local.type_id)
                }
            },
        }
    }

    fn alloc(&mut self, name: String, type_id: TypeId) -> u32 {
        let local = TirLocal {
            name,
            type_id,
            is_mut: false,
            span: Default::default(),
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
    let by_ref = scan
        .sites
        .iter()
        .map(|site| {
            site.captured
                .iter()
                .filter(|(_, local)| {
                    type_table.is_boxed_reference_target(locals.local(*local).1)
                        && (address_taken.contains(local) || scan.written_after(*local, site))
                })
                .map(|(slot, _)| *slot)
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

/// Where a closure is built, and the frame locals it captures by slot.
struct Site {
    at: u32,
    loops: Vec<u32>,
    captured: Vec<(u32, u32)>,
}

/// A whole or partial assignment to a frame local.
struct Write {
    local: u32,
    at: u32,
    loops: Vec<u32>,
}

/// The closures of one frame in walk order, and the assignments to its
/// locals, each placed in evaluation order and in the loops enclosing it.
#[derive(Default)]
struct Scan {
    clock: u32,
    loops: Vec<u32>,
    next_loop: u32,
    sites: Vec<Site>,
    writes: Vec<Write>,
}

impl Scan {
    /// A write the closure can observe: one that runs after it is built, or
    /// shares a loop with it and so may run after it on a later iteration.
    fn written_after(&self, local: u32, site: &Site) -> bool {
        self.writes.iter().any(|w| {
            w.local == local && (w.at > site.at || w.loops.iter().any(|l| site.loops.contains(l)))
        })
    }

    fn tick(&mut self) -> u32 {
        self.clock += 1;
        self.clock
    }

    fn in_loop(&mut self, walk: impl FnOnce(&mut Self)) {
        self.loops.push(self.next_loop);
        self.next_loop += 1;
        walk(self);
        self.loops.pop();
    }
}

impl TirRefVisitor for Scan {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Loop { .. } | TirStmtKind::VariadicForOf { .. } => {
                self.in_loop(|scan| scan.walk_stmt(stmt));
            }
            TirStmtKind::Let { .. }
            | TirStmtKind::Expr(_)
            | TirStmtKind::Return { .. }
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::If { .. }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue
            | TirStmtKind::LabeledBlock { .. }
            | TirStmtKind::LetDestructure { .. } => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        let at = self.tick();
        if let TirExprKind::Closure { captures, .. } = &expr.kind {
            let captured = captures
                .iter()
                .enumerate()
                .filter_map(|(slot, capture)| {
                    let local = capture.source.local()?;
                    Some((u32::try_from(slot).unwrap(), local))
                })
                .collect();
            self.sites.push(Site {
                at,
                loops: self.loops.clone(),
                captured,
            });
            return;
        }
        if matches!(expr.kind, TirExprKind::VariadicTupleComprehension { .. }) {
            self.in_loop(|scan| scan.walk_expr(expr));
            return;
        }
        self.walk_expr(expr);
        if let TirExprKind::Assign { target, .. } = &expr.kind
            && let Some(local) = assigned_local(target)
        {
            let at = self.tick();
            self.writes.push(Write {
                local,
                at,
                loops: self.loops.clone(),
            });
        }
    }
}

/// The frame local an assignment to `place` writes into. A write through a
/// dereference lands in whatever the reference was taken from, and taking it
/// already address-took that local.
fn assigned_local(place: &TirExpr) -> Option<u32> {
    match &place.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::Index { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => assigned_local(expr),
        _ => None,
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
            let (name, local_type) = self.locals.local(local);
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
