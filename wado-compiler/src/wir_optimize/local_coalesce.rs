//! Local coalescing: locals of one Wasm type whose live ranges never overlap
//! share one slot, as a register allocator shares a register. A `bool` and a
//! `u32` are both an `i32` there, so they may share. A range runs from a
//! local's first access to its last in evaluation order. It widens to cover a
//! loop whose back edge the value may cross, and back to the function entry
//! where a read may see the zero Wasm starts a local at. A parameter is live
//! from the entry and can absorb a local whose range starts after its own ends.

use std::collections::BTreeSet;

use crate::hashmap::IndexMap;
use crate::wir::{WirInstr, WirLocals, WirPackage, WirType, WirTypeDef};
use crate::wir_optimize::local_layout::WasmClass;
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

/// Rename each local to the slot it shares, dropping the declarations and the
/// self-copies the renaming leaves.
pub(super) fn coalesce_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let WirTypeDef::Func(func_type) = &module.types[func.type_id.index() as usize] else {
            unreachable!("a function's type is a func type");
        };
        let params: Vec<(&str, &WirType)> = func
            .param_names
            .iter()
            .map(String::as_str)
            .zip(&func_type.params)
            .collect();
        let renames = plan_slots(body, &params);
        if !renames.is_empty() {
            Rename { renames: &renames }.visit_body(body);
        }
    }
}

/// Each local merged into another slot, mapped to that slot's name.
fn plan_slots(body: &[WirInstr], params: &[(&str, &WirType)]) -> IndexMap<String, String> {
    let is_param = |name: &str| params.iter().any(|&(param, _)| param == name);
    let declared = WirLocals::scan(body);
    let mut ranges = Ranges::default();
    ranges.walk_body(body);

    let mut assignment = DefiniteAssignment::new(&ranges.first);
    let entry = ranges.first.keys().map(|name| is_param(name)).collect();
    assignment.walk_body(body, &mut Some(entry));

    let mut intervals: IndexMap<&str, Interval> = IndexMap::default();
    for &(name, ty) in params {
        let end = ranges.last.get(name).copied().unwrap_or(0);
        intervals.insert(name, Interval::new(ty, 0, end, true));
    }
    for (i, (name, &first)) in ranges.first.iter().enumerate() {
        let Some(ty) = declared.get(name) else {
            assert!(
                is_param(name),
                "[WIR] `{name}` is accessed but neither declared nor a parameter"
            );
            continue;
        };
        let start = if assignment.maybe_unset[i] { 0 } else { first };
        intervals.insert(name, Interval::new(ty, start, ranges.last[name], false));
    }
    for lp in &ranges.loops {
        for (name, &set_first) in &lp.first_access {
            let interval = &mut intervals[name.as_str()];
            let inside = lp.start <= interval.start && interval.end <= lp.end;
            if !(inside && set_first) {
                interval.start = interval.start.min(lp.start);
                interval.end = interval.end.max(lp.end);
            }
        }
    }

    let mut intervals: Vec<(&str, Interval)> = intervals.into_iter().collect();
    intervals.sort_by_key(|(_, interval)| (interval.start, !interval.is_param));
    // Per Wasm type, each slot's end and index. A local takes the slot freed
    // last before it starts: after a copy `b = a` that ends `a`, that is
    // `a`'s, and the copy goes.
    let mut pools: Vec<(WasmClass, BTreeSet<(u32, usize)>)> = Vec::new();
    let mut slot_names: Vec<&str> = Vec::new();
    let mut renames = IndexMap::default();
    for (name, interval) in intervals {
        let i = pools
            .iter()
            .position(|(class, _)| *class == interval.class)
            .unwrap_or_else(|| {
                pools.push((interval.class, BTreeSet::new()));
                pools.len() - 1
            });
        let pool = &mut pools[i].1;
        let free = pool.range(..(interval.start, 0)).next_back().copied();
        let slot = match free {
            Some(entry @ (_, slot)) if !interval.is_param => {
                pool.remove(&entry);
                renames.insert(name.to_string(), slot_names[slot].to_string());
                slot
            }
            _ => {
                slot_names.push(name);
                slot_names.len() - 1
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

/// Each local's first and last access, numbered in evaluation order from 1,
/// and the span of each loop.
#[derive(Default)]
struct Ranges {
    position: u32,
    first: IndexMap<String, u32>,
    last: IndexMap<String, u32>,
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
    first_access: IndexMap<String, bool>,
}

impl Ranges {
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
            self.first.entry(name.to_string()).or_insert(position);
            self.last.insert(name.to_string(), position);
            for (depth, lp) in self.open_loops.iter_mut().enumerate() {
                lp.first_access
                    .entry(name.to_string())
                    .or_insert(top_write && depth == innermost);
            }
        }
    }
}

impl WirRefVisitor for Ranges {
    fn visit_instr(&mut self, instr: &WirInstr) {
        let at_loop_top = std::mem::replace(&mut self.at_loop_top, false);
        match instr {
            WirInstr::LocalGet { name, .. } => self.access([name.as_str()], false),
            WirInstr::LocalSet { name, value } => {
                self.visit_instr(value);
                self.access([name.as_str()], at_loop_top);
            }
            WirInstr::LocalTee { name, value } => {
                self.visit_instr(value);
                self.access([name.as_str()], false);
            }
            WirInstr::MultiValueLocalBind { instr, locals } => {
                self.visit_instr(instr);
                self.access(locals.iter().flatten().map(String::as_str), at_loop_top);
            }
            WirInstr::Loop { body, .. } => {
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
            }
            _ => self.walk_instr(instr),
        }
    }
}

/// Which locals a read may see before any write on some path, following Wasm's
/// branches: a state is the locals written on every path so far, `None` where
/// no path reaches.
struct DefiniteAssignment<'a> {
    index: &'a IndexMap<String, u32>,
    maybe_unset: Vec<bool>,
    /// One per enclosing label, innermost last.
    labels: Vec<Label>,
}

type State = Option<Vec<bool>>;

/// Where a branch to an enclosing label goes.
enum Label {
    /// The end of a block or an `if`: the meet of the states branching there.
    End(State),
    /// The start of a loop. A back edge writes nothing a read in the loop has
    /// not already seen on entry.
    LoopStart,
}

impl<'a> DefiniteAssignment<'a> {
    fn new(index: &'a IndexMap<String, u32>) -> Self {
        Self {
            index,
            maybe_unset: vec![false; index.len()],
            labels: Vec::new(),
        }
    }

    fn slot(&self, name: &str) -> usize {
        self.index
            .get_index_of(name)
            .expect("every accessed local is ranged")
    }

    fn write(&self, name: &str, state: &mut State) {
        if let Some(written) = state {
            written[self.slot(name)] = true;
        }
    }

    /// A branch to `depth`: its state joins the target label's.
    fn branch(&mut self, depth: u32, state: &State) {
        let Some(target) = (self.labels.len() as u32)
            .checked_sub(depth + 1)
            .map(|i| &mut self.labels[i as usize])
        else {
            return; // the function body: a return
        };
        if let Label::End(pending) = target {
            *pending = meet(pending.take(), state.clone());
        }
    }

    fn walk_body(&mut self, body: &[WirInstr], state: &mut State) {
        for instr in body {
            self.visit(instr, state);
        }
    }

    fn labeled(&mut self, label: Label, body: &[WirInstr], state: &mut State) {
        self.labels.push(label);
        self.walk_body(body, state);
        if let Label::End(pending) = self.labels.pop().expect("pushed above") {
            *state = meet(state.take(), pending);
        }
    }

    fn visit(&mut self, instr: &WirInstr, state: &mut State) {
        match instr {
            WirInstr::LocalGet { name, .. } => {
                let slot = self.slot(name);
                if state.as_ref().is_some_and(|written| !written[slot]) {
                    self.maybe_unset[slot] = true;
                }
            }
            WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } => {
                self.visit(value, state);
                self.write(name, state);
            }
            WirInstr::MultiValueLocalBind { instr, locals } => {
                self.visit(instr, state);
                for name in locals.iter().flatten() {
                    self.write(name, state);
                }
            }
            WirInstr::Block { body, .. } => self.labeled(Label::End(None), body, state),
            WirInstr::Loop { body, .. } => self.labeled(Label::LoopStart, body, state),
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit(condition, state);
                let mut else_state = state.clone();
                self.labeled(Label::End(None), then_body, state);
                if let Some(else_body) = else_body {
                    self.labeled(Label::End(None), else_body, &mut else_state);
                }
                *state = meet(state.take(), else_state);
            }
            WirInstr::Br { depth } => {
                self.branch(*depth, state);
                *state = None;
            }
            WirInstr::BrIf { depth, condition } => {
                self.visit(condition, state);
                self.branch(*depth, state);
            }
            WirInstr::BrTable {
                index,
                targets,
                default,
            } => {
                self.visit(index, state);
                for depth in targets.iter().chain([default]) {
                    self.branch(*depth, state);
                }
                *state = None;
            }
            WirInstr::Return { value } => {
                if let Some(value) = value {
                    self.visit(value, state);
                }
                *state = None;
            }
            WirInstr::Unreachable => *state = None,
            _ => instr.for_each_child(&mut |child| self.visit(child, state)),
        }
    }
}

/// The locals written on both paths; a path no execution reaches adds nothing.
fn meet(a: State, b: State) -> State {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.iter().zip(&b).map(|(x, y)| *x && *y).collect()),
        (a, None) => a,
        (None, b) => b,
    }
}

struct Rename<'a> {
    renames: &'a IndexMap<String, String>,
}

impl Rename<'_> {
    fn rename(&self, name: &mut String) {
        if let Some(slot) = self.renames.get(name.as_str()) {
            name.clone_from(slot);
        }
    }
}

impl WirMutVisitor for Rename<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        body.retain(|instr| !matches!(instr, WirInstr::Nop));
    }

    fn visit_instr(&mut self, instr: &mut WirInstr) {
        self.walk_instr(instr);
        match instr {
            WirInstr::DeclareLocal { name, .. } if self.renames.contains_key(name.as_str()) => {
                *instr = WirInstr::Nop;
            }
            WirInstr::LocalGet { name, .. } => self.rename(name),
            WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } => {
                self.rename(name);
                if matches!(&**value, WirInstr::LocalGet { name: source, .. } if source == name) {
                    *instr = match instr {
                        WirInstr::LocalTee { value, .. } => {
                            std::mem::replace(&mut **value, WirInstr::Nop)
                        }
                        _ => WirInstr::Nop,
                    };
                }
            }
            WirInstr::MultiValueLocalBind { locals, .. } => {
                for name in locals.iter_mut().flatten() {
                    self.rename(name);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
