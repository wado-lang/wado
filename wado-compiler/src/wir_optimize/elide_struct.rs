//! Box-local elimination passes for WIR.
//!
//! - **Adjacent-use box elision** (`elide_adjacent_box_locals`): substitutes a
//!   single-field `StructNew` local's `inner` at its one `StructGet` use, even
//!   for a heap-reading `inner`, sound because it moves `inner` only into a
//!   single-use site whose preceding ops are all pure and unconditional. Targets
//!   the `Box<T>` locals lowering mints for `&primitive` payload bindings
//!   (`match r { Token(i) => f(*i) }`); these are born during NIR->WIR lowering
//!   (`lower::plan::boxing`), so NIR's `elide_box_local` never sees them.
//! - **Flatten seq assignments**: canonicalizes `LocalSet(x, Seq([preamble, final]))`.
//!
//! Struct locals NIR can see are decomposed there, by `sroa` / `sroa_param` /
//! `elide_box_local`.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

#[derive(Default)]
struct LocalStats {
    /// Every `LocalGet(name)` anywhere in the tree (including those wrapped in `StructGet`).
    total_localgets: u32,
    /// Every `StructGet(LocalGet(name), _)` occurrence.
    structget_uses: u32,
    /// Every `LocalSet(name, _)` or `LocalTee(name, _)` occurrence.
    defs: u32,
    /// Per-field use counts for `StructGet(LocalGet(name), field)`.
    field_uses: IndexMap<String, u32>,
}

/// Single traversal: populate `stats` with def/use counts per local name and
/// record every `LocalSet(name, StructNew { [inner] })` name in `candidates`.
fn collect_stats(
    instr: &WirInstr,
    stats: &mut IndexMap<String, LocalStats>,
    candidates: &mut IndexSet<String>,
) {
    match instr {
        WirInstr::LocalGet { name, .. } => {
            stats.entry(name.clone()).or_default().total_localgets += 1;
        }
        WirInstr::LocalSet { name, value } => {
            stats.entry(name.clone()).or_default().defs += 1;
            if let WirInstr::StructNew { fields, .. } = value.as_ref()
                && fields.len() == 1
            {
                candidates.insert(name.clone());
            }
            collect_stats(value, stats, candidates);
        }
        WirInstr::LocalTee { name, value } => {
            stats.entry(name.clone()).or_default().defs += 1;
            collect_stats(value, stats, candidates);
        }
        WirInstr::StructGet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                let s = stats.entry(name.clone()).or_default();
                s.total_localgets += 1;
                s.structget_uses += 1;
                *s.field_uses.entry(field_name.clone()).or_insert(0) += 1;
                // Don't recurse into expr — it's the LocalGet we just counted.
            } else {
                collect_stats(expr, stats, candidates);
            }
        }
        _ => {
            instr.for_each_child(&mut |child| {
                collect_stats(child, stats, candidates);
            });
        }
    }
}

/// Walk `instr` to check whether any descendant `LocalGet(name)` has its name in
/// `candidates` and not equal to `exclude`.
fn inner_refs_any_candidate(
    instr: &WirInstr,
    candidates: &IndexSet<String>,
    exclude: &str,
) -> bool {
    if let WirInstr::LocalGet { name, .. } = instr {
        return candidates.contains(name) && name != exclude;
    }
    let mut found = false;
    instr.for_each_child(&mut |child| {
        if !found && inner_refs_any_candidate(child, candidates, exclude) {
            found = true;
        }
    });
    found
}

/// Flatten `LocalSet { name, value: Seq([preamble..., final]) }` into
/// `[preamble..., LocalSet { name, value: final }]` at all levels of each function.
///
/// This canonicalizes the pattern the WIR builder produces for tuple
/// destructuring, exposing the trailing copy to `propagate_trivial_copies`.
pub(super) fn flatten_seq_assignments(module: &mut WirPackage) {
    let mut visitor = FlattenSeqAssignments;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            visitor.visit_body(body);
        }
    }
}

struct FlattenSeqAssignments;

impl WirMutVisitor for FlattenSeqAssignments {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        // First recurse into nested bodies.
        self.walk_body(body);
        // Then expand any LocalSet { value: Seq([..., final]) } at this level.
        let old = std::mem::take(body);
        for instr in old {
            match instr {
                WirInstr::LocalSet { name, value } if matches!(value.as_ref(), WirInstr::Seq(seq) if !seq.is_empty()) => {
                    if let WirInstr::Seq(mut seq) = *value {
                        let final_val = seq.pop().unwrap();
                        body.extend(seq);
                        body.push(WirInstr::LocalSet {
                            name,
                            value: Box::new(final_val),
                        });
                    }
                }
                other => body.push(other),
            }
        }
    }
}

/// Outcome of scanning a using statement in evaluation order for the box's use.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoxUseWalk {
    /// The use is reached with only pure, unconditional ops before it.
    Found,
    /// Pure subtree with no use.
    Pure,
    /// A call, write, or conditional is reached before the use, or the use is
    /// conditionally evaluated.
    Blocked,
}

/// Elide single-field (`Box<T>`) struct locals with a heap-reading initializer
/// when the def and its single use are adjacent in straight-line code. A
/// heap-reading `inner` is only safe to move into a single use whose preceding
/// ops are all pure and unconditional, which is what the adjacency buys.
/// Fix-point per function over the monotonic def/use `stats` oracle.
pub(super) fn elide_adjacent_box_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        loop {
            let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
            let mut candidates: IndexSet<String> = IndexSet::default();
            for instr in body.iter() {
                collect_stats(instr, &mut stats, &mut candidates);
            }
            if candidates.is_empty() {
                break;
            }
            let mut elider = AdjacentBoxElider {
                stats: &stats,
                candidate_names: &candidates,
                changed: false,
            };
            elider.visit_body(body);
            if !elider.changed {
                break;
            }
        }
    }
}

struct AdjacentBoxElider<'a> {
    stats: &'a IndexMap<String, LocalStats>,
    candidate_names: &'a IndexSet<String>,
    changed: bool,
}

impl WirMutVisitor for AdjacentBoxElider<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        self.process_stmt_list(body);
    }
}

impl AdjacentBoxElider<'_> {
    fn process_stmt_list(&mut self, stmts: &mut [WirInstr]) {
        for p in 0..stmts.len() {
            let Some((name, field, inner)) = self.describe_box_def(&stmts[p]) else {
                continue;
            };
            let Some(k) = find_adjacent_box_use(stmts, p + 1, &name, &field) else {
                continue;
            };
            if substitute_box_use(&mut stmts[k], &name, &field, &inner) {
                stmts[p] = WirInstr::Nop;
                self.changed = true;
            }
        }
    }

    /// `LocalSet(name, StructNew{[inner]})` defined once, read once as
    /// `StructGet(LocalGet(name), field)`, with an `inner` safe to relocate to
    /// the use site (no own effect, no other-candidate read).
    fn describe_box_def(&self, stmt: &WirInstr) -> Option<(String, String, WirInstr)> {
        let WirInstr::LocalSet { name, value } = stmt else {
            return None;
        };
        let WirInstr::StructNew { fields, .. } = value.as_ref() else {
            return None;
        };
        if fields.len() != 1 {
            return None;
        }
        let s = self.stats.get(name)?;
        if s.defs != 1 || s.structget_uses != 1 || s.total_localgets != 1 {
            return None;
        }
        if s.field_uses.len() != 1 {
            return None;
        }
        let inner = &fields[0];
        if inner_has_effect(inner) || inner_refs_any_candidate(inner, self.candidate_names, name) {
            return None;
        }
        let field = s.field_uses.keys().next()?.clone();
        Some((name.clone(), field, inner.clone()))
    }
}

/// Whether `inner`'s subtree performs a call or write ([`is_effect_barrier`]).
/// Relocating such an effect past the intervening reads the adjacency walk
/// classifies as pure would reorder it; heap reads and pure computation cannot.
fn inner_has_effect(instr: &WirInstr) -> bool {
    if is_effect_barrier(instr) {
        return true;
    }
    let mut found = false;
    instr.for_each_child(&mut |c| {
        if !found && inner_has_effect(c) {
            found = true;
        }
    });
    found
}

/// Index of the use when it is the leftmost effect of the immediately-following
/// non-Nop statement — the only statement the pass reorders across (none: the
/// reorder is confined inside that one statement, per [`leftmost_box_use`]).
fn find_adjacent_box_use(
    stmts: &[WirInstr],
    from: usize,
    name: &str,
    field: &str,
) -> Option<usize> {
    let k = (from..stmts.len()).find(|&k| !matches!(stmts[k], WirInstr::Nop))?;
    match leftmost_box_use(&stmts[k], name, field) {
        BoxUseWalk::Found => Some(k),
        BoxUseWalk::Pure | BoxUseWalk::Blocked => None,
    }
}

/// Is `instr` the target `StructGet(LocalGet(name), field)`?
fn is_box_use(instr: &WirInstr, name: &str, field: &str) -> bool {
    matches!(
        instr,
        WirInstr::StructGet { field_name, expr, .. }
            if field_name == field
                && matches!(expr.as_ref(), WirInstr::LocalGet { name: n, .. } if n == name)
    )
}

/// A node whose own operation calls or writes. Its operands evaluate before that
/// effect, so a use in an operand is still valid; a use past the effect is not.
fn is_effect_barrier(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::Call { .. }
            | WirInstr::CallIndirect { .. }
            | WirInstr::CallRef { .. }
            | WirInstr::LocalSet { .. }
            | WirInstr::LocalTee { .. }
            | WirInstr::GlobalSet { .. }
            | WirInstr::StructSet { .. }
            | WirInstr::ArraySet { .. }
            | WirInstr::ArrayCopy { .. }
            | WirInstr::ArrayFill { .. }
            | WirInstr::MemoryFill { .. }
            | WirInstr::MemoryGrow(_)
            | WirInstr::TableSet { .. }
            | WirInstr::MultiValueLocalBind { .. }
            | WirInstr::I32Store { .. }
            | WirInstr::I32Store8 { .. }
            | WirInstr::I32Store16 { .. }
            | WirInstr::I64Store { .. }
            | WirInstr::V128Store { .. }
    )
}

/// A node that evaluates children conditionally or transfers control, so a use
/// inside it may run conditionally and cannot anchor an elision.
fn is_control_barrier(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::Block { .. }
            | WirInstr::Loop { .. }
            | WirInstr::If { .. }
            | WirInstr::BranchHint { .. }
            | WirInstr::Br { .. }
            | WirInstr::BrIf { .. }
            | WirInstr::BrTable { .. }
            | WirInstr::Return { .. }
            | WirInstr::Unreachable
            | WirInstr::ColdPath
            | WirInstr::Select { .. }
    )
}

/// Classify `instr` for the box use in evaluation order. Nodes not covered by
/// [`is_effect_barrier`] / [`is_control_barrier`] are pure and evaluate their
/// operands left to right; the barrier lists must stay exhaustive over every
/// call, write, and branch for the pure-by-default arm to be sound. A trap or
/// heap read before the use is harmless — the relocated initializer is effect-free.
fn leftmost_box_use(instr: &WirInstr, name: &str, field: &str) -> BoxUseWalk {
    if is_box_use(instr, name, field) {
        return BoxUseWalk::Found;
    }
    if is_control_barrier(instr) {
        return BoxUseWalk::Blocked;
    }
    let mut acc = BoxUseWalk::Pure;
    instr.for_each_child(&mut |c| {
        if acc == BoxUseWalk::Pure {
            acc = leftmost_box_use(c, name, field);
        }
    });
    match (is_effect_barrier(instr), acc) {
        (_, BoxUseWalk::Found) => BoxUseWalk::Found,
        (true, _) => BoxUseWalk::Blocked,
        (false, other) => other,
    }
}

/// Replace the box's single `StructGet(LocalGet(name), field)` use with `inner`.
fn substitute_box_use(instr: &mut WirInstr, name: &str, field: &str, inner: &WirInstr) -> bool {
    if is_box_use(instr, name, field) {
        *instr = inner.clone();
        return true;
    }
    let mut done = false;
    instr.for_each_boxed_child_mut(&mut |child| {
        if !done {
            done = substitute_box_use(child, name, field, inner);
        }
    });
    done
}

#[cfg(test)]
mod adjacent_box_tests {
    use super::*;
    use crate::wir::{WirFuncId, WirType, WirTypeId};
    use std::rc::Rc;

    const NAME: &str = "b";
    const FIELD: &str = "value";

    fn tid() -> WirTypeId {
        WirTypeId::new(0, Rc::from("Box"))
    }
    fn lget(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }
    /// The target `StructGet(LocalGet("b"), "value")`.
    fn use_box() -> WirInstr {
        WirInstr::StructGet {
            type_id: tid(),
            field_name: FIELD.to_string(),
            expr: Box::new(lget(NAME)),
            result_ty: WirType::I32,
        }
    }
    fn call(args: Vec<WirInstr>) -> WirInstr {
        WirInstr::Call {
            func_id: WirFuncId::new(0, Rc::from("f")),
            args,
        }
    }
    fn walk(instr: &WirInstr) -> BoxUseWalk {
        leftmost_box_use(instr, NAME, FIELD)
    }

    /// `f(other, *b)` — the use is a later call arg after a pure `LocalGet`.
    /// Found: nothing side-effecting runs before the use.
    #[test]
    fn found_use_after_pure_arg() {
        let instr = call(vec![lget("other"), use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// `sum + *b` — the use is the right operand of a pure add. Found.
    #[test]
    fn found_use_in_arithmetic() {
        let instr = WirInstr::I32Add(Box::new(lget("sum")), Box::new(use_box()));
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// `f(g(), *b)` — a call is evaluated before the use, and it could mutate
    /// what the boxed initializer reads. Blocked.
    #[test]
    fn blocked_by_call_before_use() {
        let instr = call(vec![call(vec![]), use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// `*b` nested inside an `If` — the use is conditionally evaluated, so
    /// moving the (possibly-trapping) box init there could drop a trap. Blocked.
    #[test]
    fn blocked_by_conditional() {
        let instr = WirInstr::If {
            condition: Box::new(WirInstr::I32Const(1)),
            result: Some(WirType::I32),
            then_body: vec![use_box()],
            else_body: Some(vec![WirInstr::I32Const(0)]),
        };
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// A heap write before the use blocks: the write could change what the
    /// boxed read observes if moved after it.
    #[test]
    fn blocked_by_heap_write_before_use() {
        let write = WirInstr::StructSet {
            type_id: tid(),
            field_name: "f".to_string(),
            expr: Box::new(lget("obj")),
            value: Box::new(WirInstr::I32Const(1)),
        };
        // `(struct.set …, *b)` modelled as a two-arg call so both are siblings.
        let instr = call(vec![write, use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// A statement with no box use and no side effect is Pure — the scan may
    /// safely skip it (it never does in practice, but the classification must
    /// not report a spurious barrier).
    #[test]
    fn pure_when_no_use() {
        let instr = WirInstr::I32Add(Box::new(lget("x")), Box::new(WirInstr::I32Const(1)));
        assert!(matches!(walk(&instr), BoxUseWalk::Pure));
    }

    /// A heap read (non-target `StructGet`) before the use is fine — reads
    /// never change what another read observes, and a trap aborts before the
    /// pure moved read matters. Found.
    #[test]
    fn found_use_after_heap_read() {
        let other_read = WirInstr::StructGet {
            type_id: tid(),
            field_name: "other".to_string(),
            expr: Box::new(lget("obj")),
            result_ty: WirType::I32,
        };
        let instr = call(vec![other_read, use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// The moved initializer must be free of calls and writes: a heap read is
    /// relocatable, but a `Call` or a `LocalTee`/write is not (its effect would
    /// be reordered past the intervening pure reads the walk allows).
    #[test]
    fn inner_effect_gate() {
        // pure heap read (the intended payload-read case) → relocatable
        let read = WirInstr::StructGet {
            type_id: tid(),
            field_name: "payload_0".to_string(),
            expr: Box::new(lget("scrut")),
            result_ty: WirType::I32,
        };
        assert!(!inner_has_effect(&read));
        // a call → not relocatable
        assert!(inner_has_effect(&call(vec![])));
        // a call nested inside otherwise-pure arithmetic → not relocatable
        let nested = WirInstr::I32Add(Box::new(lget("x")), Box::new(call(vec![])));
        assert!(inner_has_effect(&nested));
        // a local.tee (write) → not relocatable
        let tee = WirInstr::LocalTee {
            name: "v".to_string(),
            value: Box::new(WirInstr::I32Const(1)),
        };
        assert!(inner_has_effect(&tee));
    }

    /// End-to-end substitution: `substitute_box_use` replaces the single use
    /// with the initializer and reports success.
    #[test]
    fn substitute_replaces_single_use() {
        let inner = WirInstr::I32Const(42);
        let mut instr = call(vec![lget("other"), use_box()]);
        assert!(substitute_box_use(&mut instr, NAME, FIELD, &inner));
        let WirInstr::Call { args, .. } = &instr else {
            panic!("expected call");
        };
        assert!(matches!(args[1], WirInstr::I32Const(42)));
    }
}
