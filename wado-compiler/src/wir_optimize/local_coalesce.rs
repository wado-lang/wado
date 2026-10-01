//! Local coalescing: locals of one Wasm type whose live ranges never overlap
//! share one slot, as a register allocator shares a register. A `bool` and a
//! `u32` are both an `i32` there, so they may share. A range runs from a
//! local's first access to its last in evaluation order. It widens to cover a
//! loop whose back edge the value may cross, and back to the function entry
//! where a read may see the zero Wasm starts a local at. A parameter is live
//! from the entry and can absorb a local whose range starts after its own ends.

use std::collections::BTreeSet;

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirLocals, WirPackage, WirType};
use crate::wir_optimize::local_layout::WasmClass;
use crate::wir_optimize::local_nullability::relax_unset_nonnull_locals_in;
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

/// Rename each local to the slot it shares, dropping the declarations and the
/// self-copies the renaming leaves.
pub(super) fn coalesce_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let func_type = module.types[func.type_id.index() as usize].expect_func();
        let params: Vec<(&str, &WirType)> = func
            .param_names
            .iter()
            .map(String::as_str)
            .zip(&func_type.params)
            .collect();
        coalesce_body(body, &params);
    }
}

/// A slot holds one Wasm type, so each local's nullability is settled before
/// any shares one.
fn coalesce_body(body: &mut Vec<WirInstr>, params: &[(&str, &WirType)]) {
    relax_unset_nonnull_locals_in(body);
    let renames = plan_slots(body, params);
    if !renames.is_empty() {
        Rename { renames: &renames }.visit_body(body);
    }
}

/// Each local merged into another slot, mapped to that slot's name.
fn plan_slots(body: &[WirInstr], params: &[(&str, &WirType)]) -> IndexMap<String, String> {
    let declared = WirLocals::scan(body);
    // Parameters first, so an index below `params.len()` is one.
    let locals: IndexSet<&str> = params
        .iter()
        .map(|&(name, _)| name)
        .chain(declared.iter().map(|(name, _)| name))
        .collect();
    assert_eq!(
        locals.len(),
        params.len() + declared.iter().count(),
        "[WIR] a local is declared under a parameter's name"
    );
    let is_param = |i: usize| i < params.len();
    let mut ranges = Ranges::new(&locals);
    ranges.walk_body(body);
    let mut assignment = DefiniteAssignment::new(&locals, params.len());
    assignment.walk_body(body);

    let mut intervals: Vec<Option<Interval>> = (0..locals.len())
        .map(|i| {
            if is_param(i) {
                return Some(Interval::new(params[i].1, 0, ranges.last[i], true));
            }
            let first = ranges.first[i];
            (first != 0).then(|| {
                let start = if assignment.maybe_unset[i] { 0 } else { first };
                let ty = declared
                    .get(locals[i])
                    .expect("indexed from the declarations");
                Interval::new(ty, start, ranges.last[i], false)
            })
        })
        .collect();
    for lp in &ranges.loops {
        for (&i, &set_first) in &lp.first_access {
            let interval = intervals[i].as_mut().expect("an accessed local is ranged");
            let inside = lp.start <= interval.start && interval.end <= lp.end;
            if !(inside && set_first) {
                interval.start = interval.start.min(lp.start);
                interval.end = interval.end.max(lp.end);
            }
        }
    }

    let mut intervals: Vec<(usize, Interval)> = intervals
        .into_iter()
        .enumerate()
        .filter_map(|(i, interval)| Some((i, interval?)))
        .collect();
    intervals.sort_by_key(|&(i, ref interval)| {
        let order = if interval.is_param {
            i as u32
        } else {
            ranges.first[i]
        };
        (interval.start, !interval.is_param, order)
    });
    // Per Wasm type, each slot's end and index. A local takes the slot freed
    // last before it starts: after a copy `b = a` that ends `a`, that is
    // `a`'s, and the copy goes.
    let mut pools: Vec<(WasmClass, BTreeSet<(u32, usize)>)> = Vec::new();
    let mut slot_owners: Vec<usize> = Vec::new();
    let mut renames = IndexMap::default();
    for (i, interval) in intervals {
        let pool_index = pools
            .iter()
            .position(|(class, _)| *class == interval.class)
            .unwrap_or_else(|| {
                pools.push((interval.class, BTreeSet::new()));
                pools.len() - 1
            });
        let pool = &mut pools[pool_index].1;
        let free = pool.range(..(interval.start, 0)).next_back().copied();
        let slot = match free {
            Some(entry @ (_, slot)) if !interval.is_param => {
                pool.remove(&entry);
                renames.insert(locals[i].to_string(), locals[slot_owners[slot]].to_string());
                slot
            }
            _ => {
                slot_owners.push(i);
                slot_owners.len() - 1
            }
        };
        pool.insert((interval.end, slot));
    }
    renames
}

struct Interval {
    class: WasmClass,
    start: u32,
    end: u32,
    is_param: bool,
}

impl Interval {
    fn new(ty: &WirType, start: u32, end: u32, is_param: bool) -> Self {
        Self {
            class: WasmClass::of(ty),
            start,
            end,
            is_param,
        }
    }
}

/// Each local's first and last access, numbered in evaluation order from 1 (0
/// where it has none), and the span of each loop.
struct Ranges<'a> {
    locals: &'a IndexSet<&'a str>,
    position: u32,
    first: Vec<u32>,
    last: Vec<u32>,
    /// Closed loops, inner ones first.
    loops: Vec<LoopSpan>,
    open_loops: Vec<LoopSpan>,
    /// The statement being visited sits directly in the innermost loop's body.
    at_loop_top: bool,
}

struct LoopSpan {
    start: u32,
    end: u32,
    /// Each local accessed in the loop, and whether its first access writes
    /// it from a statement directly in the loop body, so no value of it
    /// crosses the back edge.
    first_access: IndexMap<usize, bool>,
}

impl<'a> Ranges<'a> {
    fn new(locals: &'a IndexSet<&'a str>) -> Self {
        Self {
            locals,
            position: 0,
            first: vec![0; locals.len()],
            last: vec![0; locals.len()],
            loops: Vec::new(),
            open_loops: Vec::new(),
            at_loop_top: false,
        }
    }

    fn tick(&mut self) -> u32 {
        self.position += 1;
        self.position
    }

    /// Access `names` at one position: the targets of a multi-value bind are
    /// written at once, so none of them is dead while another is written.
    fn access<'n>(&mut self, names: impl IntoIterator<Item = &'n str>, top_write: bool) {
        let position = self.tick();
        let innermost = self.open_loops.len().saturating_sub(1);
        for name in names {
            let i = local_index(self.locals, name);
            if self.first[i] == 0 {
                self.first[i] = position;
            }
            self.last[i] = position;
            for (depth, lp) in self.open_loops.iter_mut().enumerate() {
                lp.first_access
                    .entry(i)
                    .or_insert(top_write && depth == innermost);
            }
        }
    }
}

fn local_index(locals: &IndexSet<&str>, name: &str) -> usize {
    locals.get_index_of(name).unwrap_or_else(|| {
        panic!("[WIR] `{name}` is accessed but neither declared nor a parameter")
    })
}

impl WirRefVisitor for Ranges<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        let at_loop_top = std::mem::replace(&mut self.at_loop_top, false);
        if let WirInstr::Loop { body, .. } = instr {
            let start = self.tick();
            self.open_loops.push(LoopSpan {
                start,
                end: start,
                first_access: IndexMap::default(),
            });
            for stmt in body {
                self.at_loop_top = true;
                self.visit_instr(stmt);
            }
            let mut lp = self.open_loops.pop().expect("the loop pushed above");
            lp.end = self.tick();
            self.loops.push(lp);
            return;
        }
        self.walk_instr(instr);
        if let Some(name) = instr.local_read() {
            self.access([name], false);
        }
        let mut writes = instr.local_writes().peekable();
        if writes.peek().is_some() {
            self.access(writes, at_loop_top);
        }
    }
}

/// Which locals a read may see before any write on some path, following Wasm's
/// branches.
struct DefiniteAssignment<'a> {
    locals: &'a IndexSet<&'a str>,
    /// The locals written on every path reaching here, `None` where no path
    /// reaches.
    state: Option<Bits>,
    maybe_unset: Vec<bool>,
    /// One per enclosing label, innermost last.
    labels: Vec<Label>,
}

/// Where a branch to an enclosing label goes.
enum Label {
    /// The end of a block or an `if`: the meet of the states branching there.
    End(Option<Bits>),
    /// The start of a loop. A back edge writes nothing a read in the loop has
    /// not already seen on entry.
    LoopStart,
}

impl<'a> DefiniteAssignment<'a> {
    /// The leading `params` locals hold their arguments from the entry.
    fn new(locals: &'a IndexSet<&'a str>, params: usize) -> Self {
        let mut entry = Bits::new(locals.len());
        for i in 0..params {
            entry.insert(i);
        }
        Self {
            locals,
            state: Some(entry),
            maybe_unset: vec![false; locals.len()],
            labels: Vec::new(),
        }
    }

    /// A branch to `depth`: the current state joins the target label's.
    fn branch(&mut self, depth: u32) {
        let Some(target) = (self.labels.len() as u32)
            .checked_sub(depth + 1)
            .map(|i| &mut self.labels[i as usize])
        else {
            return; // the function body: a return
        };
        if let (Label::End(pending), Some(state)) = (target, &self.state) {
            match pending {
                Some(pending) => pending.intersect(state),
                None => *pending = Some(state.clone()),
            }
        }
    }

    fn labeled(&mut self, label: Label, body: &[WirInstr]) {
        self.labels.push(label);
        self.walk_body(body);
        if let Label::End(pending) = self.labels.pop().expect("pushed above") {
            self.state = meet(self.state.take(), pending);
        }
    }
}

impl WirRefVisitor for DefiniteAssignment<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::Block { body, .. } => self.labeled(Label::End(None), body),
            WirInstr::Loop { body, .. } => self.labeled(Label::LoopStart, body),
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit_instr(condition);
                let entry = self.state.clone();
                self.labeled(Label::End(None), then_body);
                let then_state = std::mem::replace(&mut self.state, entry);
                if let Some(else_body) = else_body {
                    self.labeled(Label::End(None), else_body);
                }
                self.state = meet(then_state, self.state.take());
            }
            WirInstr::Br { depth } => {
                self.branch(*depth);
                self.state = None;
            }
            WirInstr::BrIf { depth, condition } => {
                self.visit_instr(condition);
                self.branch(*depth);
            }
            WirInstr::BrTable {
                index,
                targets,
                default,
            } => {
                self.visit_instr(index);
                for depth in targets.iter().chain([default]) {
                    self.branch(*depth);
                }
                self.state = None;
            }
            WirInstr::Return { .. } | WirInstr::Unreachable => {
                self.walk_instr(instr);
                self.state = None;
            }
            _ => {
                self.walk_instr(instr);
                let Some(written) = &mut self.state else {
                    return;
                };
                if let Some(name) = instr.local_read() {
                    let i = local_index(self.locals, name);
                    if !written.contains(i) {
                        self.maybe_unset[i] = true;
                    }
                }
                for name in instr.local_writes() {
                    written.insert(local_index(self.locals, name));
                }
            }
        }
    }
}

/// The locals written on both paths; a path no execution reaches adds nothing.
fn meet(a: Option<Bits>, b: Option<Bits>) -> Option<Bits> {
    match (a, b) {
        (Some(mut a), Some(b)) => {
            a.intersect(&b);
            Some(a)
        }
        (a, None) => a,
        (None, b) => b,
    }
}

/// A set of local indices, one bit each.
#[derive(Clone)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(len: usize) -> Self {
        Self(vec![0; len.div_ceil(64)])
    }

    fn insert(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }

    fn contains(&self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }

    fn intersect(&mut self, other: &Self) {
        for (word, other) in self.0.iter_mut().zip(&other.0) {
            *word &= other;
        }
    }
}

struct Rename<'a> {
    renames: &'a IndexMap<String, String>,
}

impl WirMutVisitor for Rename<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        body.retain(|instr| !matches!(instr, WirInstr::Nop));
    }

    fn visit_instr(&mut self, instr: &mut WirInstr) {
        self.walk_instr(instr);
        let renames = self.renames;
        if let WirInstr::DeclareLocal { name, .. } = instr
            && renames.contains_key(name.as_str())
        {
            *instr = WirInstr::Nop;
            return;
        }
        instr.for_each_local_name_mut(|name| {
            if let Some(slot) = renames.get(name.as_str()) {
                name.clone_from(slot);
            }
        });
        let self_copy = matches!(
            instr,
            WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value }
                if matches!(&**value, WirInstr::LocalGet { name: source, .. } if source == name)
        );
        if self_copy {
            *instr = match instr {
                WirInstr::LocalTee { value, .. } => std::mem::replace(&mut **value, WirInstr::Nop),
                _ => WirInstr::Nop,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::{WirAbstractHeapType, WirTypeId};

    fn decl(name: &str, ty: WirType) -> WirInstr {
        WirInstr::DeclareLocal {
            name: name.to_string(),
            ty,
        }
    }

    fn set(name: &str, value: WirInstr) -> WirInstr {
        WirInstr::LocalSet {
            name: name.to_string(),
            value: Box::new(value),
        }
    }

    fn get(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }

    fn use_of(name: &str) -> WirInstr {
        WirInstr::Drop(Box::new(get(name)))
    }

    fn renames(body: &[WirInstr], params: &[(&str, &WirType)]) -> Vec<(String, String)> {
        plan_slots(body, params).into_iter().collect()
    }

    fn merged(from: &str, into: &str) -> (String, String) {
        (from.to_string(), into.to_string())
    }

    #[test]
    fn disjoint_locals_share_a_slot() {
        let body = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
            set("b", WirInstr::I32Const(2)),
            use_of("b"),
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }

    #[test]
    fn locals_of_one_wasm_type_share_a_slot() {
        let body = [
            decl("a", WirType::Bool),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
            set("b", WirInstr::I32Const(2)),
            use_of("b"),
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }

    #[test]
    fn overlapping_locals_or_types_keep_their_slots() {
        let overlapping = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            set("b", WirInstr::I32Const(2)),
            use_of("a"),
            use_of("b"),
        ];
        assert_eq!(renames(&overlapping, &[]), []);
        let typed = [
            decl("a", WirType::I32),
            decl("b", WirType::I64),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
            set("b", WirInstr::I64Const(2)),
            use_of("b"),
        ];
        assert_eq!(renames(&typed, &[]), []);
    }

    /// `x` is read at the top of each iteration after its last access in
    /// order, so it stays live over the whole loop and `y` cannot take it.
    #[test]
    fn a_value_crossing_the_back_edge_holds_its_slot_over_the_loop() {
        let body = [
            decl("x", WirType::I32),
            decl("y", WirType::I32),
            set("x", WirInstr::I32Const(0)),
            WirInstr::Loop {
                label: None,
                body: vec![
                    use_of("x"),
                    set("y", WirInstr::I32Const(1)),
                    use_of("y"),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(WirInstr::I32Const(1)),
                    },
                ],
            },
        ];
        assert_eq!(renames(&body, &[]), []);
    }

    /// Both locals are written first, at the loop's top level, every
    /// iteration: neither value crosses the back edge.
    #[test]
    fn locals_written_fresh_each_iteration_share_a_slot() {
        let body = [
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            WirInstr::Loop {
                label: None,
                body: vec![
                    set("a", WirInstr::I32Const(1)),
                    use_of("a"),
                    set("b", WirInstr::I32Const(2)),
                    use_of("b"),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(WirInstr::I32Const(1)),
                    },
                ],
            },
        ];
        assert_eq!(renames(&body, &[]), [merged("b", "a")]);
    }

    fn branch_writing(name: &str, value: i32) -> Vec<WirInstr> {
        vec![set(name, WirInstr::I32Const(value))]
    }

    #[test]
    fn a_local_written_on_every_path_starts_at_its_write() {
        let body = [
            decl("z", WirType::I32),
            decl("a", WirType::I32),
            set("z", WirInstr::I32Const(5)),
            use_of("z"),
            WirInstr::If {
                condition: Box::new(WirInstr::I32Const(1)),
                then_body: branch_writing("a", 1),
                else_body: Some(branch_writing("a", 2)),
                result: None,
            },
            use_of("a"),
        ];
        assert_eq!(renames(&body, &[]), [merged("a", "z")]);
    }

    /// The read after the one-armed `if` may see the zero `a` starts at, which
    /// `z`'s slot no longer holds.
    #[test]
    fn a_local_read_before_any_write_keeps_the_entry_zero() {
        let body = [
            decl("z", WirType::I32),
            decl("a", WirType::I32),
            set("z", WirInstr::I32Const(5)),
            use_of("z"),
            WirInstr::If {
                condition: Box::new(WirInstr::I32Const(1)),
                then_body: branch_writing("a", 1),
                else_body: None,
                result: None,
            },
            use_of("a"),
        ];
        assert_eq!(renames(&body, &[]), []);
    }

    /// A multi-value bind writes its targets at once: a dead first target
    /// still holds its slot while the second is written.
    #[test]
    fn targets_of_one_bind_never_share_a_slot() {
        let body = [
            decl("q", WirType::I32),
            decl("r", WirType::I32),
            WirInstr::MultiValueLocalBind {
                instr: Box::new(WirInstr::Unreachable),
                locals: vec![Some("q".to_string()), Some("r".to_string())],
            },
            use_of("r"),
        ];
        assert_eq!(renames(&body, &[]), []);
    }

    /// `b` is written in both arms and read after them, which Wasm's
    /// block-scoped rule rejects for a non-null local, so `b` is declared
    /// nullable. Sharing `a`'s slot would make `a` nullable too.
    #[test]
    fn a_local_read_unset_to_wasm_keeps_out_of_a_non_null_slot() {
        let type_id = WirTypeId::new(0, "T".into());
        let ty = WirType::non_null_ref(type_id.clone());
        let read = |name: &str| {
            WirInstr::Drop(Box::new(WirInstr::LocalGet {
                name: name.to_string(),
                result_ty: ty.clone(),
            }))
        };
        let new = || WirInstr::RefCast {
            type_id: type_id.clone(),
            nullable: false,
            expr: Box::new(WirInstr::RefNull {
                heap_type: WirAbstractHeapType::Any,
            }),
        };
        let mut body = vec![
            decl("a", ty.clone()),
            decl("b", ty.clone()),
            set("a", new()),
            read("a"),
            WirInstr::If {
                condition: Box::new(WirInstr::I32Const(1)),
                then_body: vec![set("b", new())],
                else_body: Some(vec![set("b", new())]),
                result: None,
            },
            read("b"),
        ];
        coalesce_body(&mut body, &[]);
        let declared = WirLocals::scan(&body);
        assert!(declared.is_nonnull_ref("a"));
        assert!(declared.is_nullable_ref("b"));
    }

    #[test]
    fn a_parameter_absorbs_a_local_after_its_last_read() {
        let body = [
            decl("a", WirType::I32),
            use_of("p"),
            set("a", WirInstr::I32Const(1)),
            use_of("a"),
        ];
        assert_eq!(renames(&body, &[("p", &WirType::I32)]), [merged("a", "p")]);
    }

    #[test]
    fn a_copy_between_merged_locals_disappears() {
        let mut body = vec![
            decl("a", WirType::I32),
            decl("b", WirType::I32),
            set("a", WirInstr::I32Const(1)),
            set("b", get("a")),
            use_of("b"),
        ];
        let renames = plan_slots(&body, &[]);
        Rename { renames: &renames }.visit_body(&mut body);
        assert_eq!(body.len(), 3);
        assert!(
            matches!(&body[2], WirInstr::Drop(read) if matches!(&**read, WirInstr::LocalGet { name, .. } if name == "a"))
        );
    }
}
