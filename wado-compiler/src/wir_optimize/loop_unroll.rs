//! Run several iterations of a small loop that reads GC arrays per test of its
//! condition. A loop that ends each iteration by stepping its variable,
//!
//! ```text
//! block { loop { br_if 1 EXIT; WORK; v = STEP; br 0 } }
//! ```
//!
//! gains a copy in front of it that leaves once any of the next `n` tests
//! would, `EXIT | EXIT[v := STEP] | …`, and otherwise runs `WORK; v = STEP`
//! `n` times. The original loop then runs what is left, so every iteration runs
//! as it did: the sums keep their order, and a float result its bits.
//!
//! wasmtime checks each `array.get` against its array's length and shares part
//! of that check only between gets in one basic block, so iterations in one
//! block pay less per get than one alone. A matrix-vector product's inner loop
//! ran +32% at four, and one twice its size +9% at two, where four ran slower.
//! Gemma 3's next token came +30% sooner.
//!
//! The tests ahead are computed before the first `WORK` runs, so the pass needs
//! `EXIT` and `STEP` to read only locals `WORK` never writes, and to compute
//! nothing that traps. `WORK` may not branch to the loop or its block, which
//! would leave the copy into the original loop rather than past both.

use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::util::{Footprint, is_side_effect_free, own_traps};

/// The most nodes the copy's iterations may hold together, which picks the
/// largest of [`FACTORS`] that fits.
const UNROLLED_NODES: usize = 192;

const FACTORS: [usize; 2] = [4, 2];

pub(super) fn unroll_array_loops(module: &mut WirPackage) {
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            Unroller.visit_body(body);
        }
    }
}

struct Unroller;

impl WirMutVisitor for Unroller {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        let mut at = 0;
        while at < body.len() {
            let before = at.checked_sub(1).map(|prev| &body[prev]);
            if let Some(unrolled) = unrolled(&body[at], before) {
                body.insert(at, unrolled);
                at += 1;
            }
            at += 1;
        }
    }
}

/// The unrolled copy to run ahead of `instr`, when it is a loop this pass
/// unrolls. `before` is the statement ahead of it, which may give the loop
/// variable a constant the copy would leave on at once: a loop of fewer
/// iterations than the copy runs.
fn unrolled(instr: &WirInstr, before: Option<&WirInstr>) -> Option<WirInstr> {
    let WirInstr::Block {
        result: None,
        body: outer,
        ..
    } = instr
    else {
        return None;
    };
    let [WirInstr::Loop { body, .. }] = outer.as_slice() else {
        return None;
    };
    let [
        WirInstr::BrIf {
            depth: 1,
            condition: exit,
        },
        work @ ..,
        WirInstr::LocalSet {
            name: var,
            value: step,
        },
        WirInstr::Br { depth: 0 },
    ] = body.as_slice()
    else {
        return None;
    };
    if !is_local_arith(exit) || !is_local_arith(step) {
        return None;
    }
    let factor = unroll_factor(work)?;
    let written = Footprint::writes_all(work, None);
    if written.has_local(var)
        || written.overlaps(&Footprint::reads(exit))
        || written.overlaps(&Footprint::reads(step))
    {
        return None;
    }
    let mut leave = exit.as_ref().clone();
    let mut ahead = step.as_ref().clone();
    for _ in 1..factor {
        let mut test = exit.as_ref().clone();
        substitute(&mut test, var, &ahead);
        leave = WirInstr::I32Or(Box::new(leave), Box::new(test));
        let mut next = step.as_ref().clone();
        substitute(&mut next, var, &ahead);
        ahead = next;
    }
    if let Some(WirInstr::LocalSet { name, value }) = before
        && name == var
        && let WirInstr::I32Const(entry) = value.as_ref()
        && eval_i32(&leave, var, *entry).is_some_and(|leaves| leaves != 0)
    {
        return None;
    }
    let mut looped = Vec::with_capacity(factor * (work.len() + 1) + 2);
    looped.push(WirInstr::BrIf {
        depth: 1,
        condition: Box::new(leave),
    });
    for _ in 0..factor {
        looped.extend(work.iter().cloned());
        looped.push(WirInstr::LocalSet {
            name: var.clone(),
            value: step.clone(),
        });
    }
    looped.push(WirInstr::Br { depth: 0 });
    Some(WirInstr::Block {
        label: None,
        result: None,
        body: vec![WirInstr::Loop {
            label: None,
            body: looped,
        }],
    })
}

/// How many iterations the copy of `work` runs, when it is a loop body the copy
/// pays for: small, reading a GC array, holding no loop of its own, declaring
/// no local, and never branching to the loop (depth 0) or its block (depth 1).
fn unroll_factor(work: &[WirInstr]) -> Option<usize> {
    let mut nodes = 0;
    let mut reads_array = false;
    let mut refused = false;
    for instr in work {
        scan(instr, 0, &mut |node, nesting| {
            nodes += 1;
            reads_array |= matches!(
                node,
                WirInstr::ArrayGet { .. } | WirInstr::ArrayGetS { .. } | WirInstr::ArrayGetU { .. }
            );
            refused |= match node {
                WirInstr::Loop { .. } | WirInstr::DeclareLocal { .. } => true,
                WirInstr::Br { depth } | WirInstr::BrIf { depth, .. } => {
                    (nesting..nesting + 2).contains(depth)
                }
                WirInstr::BrTable {
                    targets, default, ..
                } => targets
                    .iter()
                    .chain(std::iter::once(default))
                    .any(|depth| (nesting..nesting + 2).contains(depth)),
                _ => false,
            };
        });
    }
    if !reads_array || refused {
        return None;
    }
    FACTORS
        .into_iter()
        .find(|factor| factor * nodes <= UNROLLED_NODES)
}

/// Visit `instr` and each node under it with the labels it sits inside of,
/// counted from the loop body.
fn scan(instr: &WirInstr, nesting: u32, f: &mut impl FnMut(&WirInstr, u32)) {
    f(instr, nesting);
    let inner = match instr {
        WirInstr::Block { .. } | WirInstr::Loop { .. } => nesting + 1,
        WirInstr::If { condition, .. } => {
            scan(condition, nesting, f);
            nesting + 1
        }
        _ => nesting,
    };
    match instr {
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            for child in then_body.iter().chain(else_body.iter().flatten()) {
                scan(child, inner, f);
            }
        }
        _ => instr.for_each_child(&mut |child| scan(child, inner, f)),
    }
}

/// Whether `instr` computes from locals alone, writing nothing and trapping
/// nowhere, so evaluating it early or twice changes nothing.
fn is_local_arith(instr: &WirInstr) -> bool {
    is_side_effect_free(instr) && !Footprint::reads(instr).touches_heap() && !may_trap(instr)
}

fn may_trap(instr: &WirInstr) -> bool {
    let mut trap = own_traps(instr).any();
    instr.for_each_child(&mut |child| trap = trap || may_trap(child));
    trap
}

/// What the i32 `instr` computes with `var` holding `value`, where it is
/// integer arithmetic this knows.
fn eval_i32(instr: &WirInstr, var: &str, value: i32) -> Option<i32> {
    use WirInstr as I;
    let eval = |operand: &WirInstr| eval_i32(operand, var, value);
    let both = |a: &WirInstr, b: &WirInstr| Some((eval(a)?, eval(b)?));
    Some(match instr {
        I::I32Const(c) => *c,
        I::LocalGet { name, .. } if name == var => value,
        I::I32Add(a, b) => both(a, b).map(|(a, b)| a.wrapping_add(b))?,
        I::I32Sub(a, b) => both(a, b).map(|(a, b)| a.wrapping_sub(b))?,
        I::I32Mul(a, b) => both(a, b).map(|(a, b)| a.wrapping_mul(b))?,
        I::I32Or(a, b) => both(a, b).map(|(a, b)| a | b)?,
        I::I32And(a, b) => both(a, b).map(|(a, b)| a & b)?,
        I::I32Eqz(a) => i32::from(eval(a)? == 0),
        I::I32Eq(a, b) => both(a, b).map(|(a, b)| i32::from(a == b))?,
        I::I32Ne(a, b) => both(a, b).map(|(a, b)| i32::from(a != b))?,
        I::I32LtS(a, b) => both(a, b).map(|(a, b)| i32::from(a < b))?,
        I::I32LeS(a, b) => both(a, b).map(|(a, b)| i32::from(a <= b))?,
        I::I32GtS(a, b) => both(a, b).map(|(a, b)| i32::from(a > b))?,
        I::I32GeS(a, b) => both(a, b).map(|(a, b)| i32::from(a >= b))?,
        I::I32LtU(a, b) => both(a, b).map(|(a, b)| i32::from((a as u32) < (b as u32)))?,
        I::I32LeU(a, b) => both(a, b).map(|(a, b)| i32::from((a as u32) <= (b as u32)))?,
        I::I32GtU(a, b) => both(a, b).map(|(a, b)| i32::from((a as u32) > (b as u32)))?,
        I::I32GeU(a, b) => both(a, b).map(|(a, b)| i32::from((a as u32) >= (b as u32)))?,
        _ => return None,
    })
}

/// Replace each read of `var` under `instr` with `value`.
fn substitute(instr: &mut WirInstr, var: &str, value: &WirInstr) {
    if instr.local_read() == Some(var) {
        *instr = value.clone();
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute(child, var, value));
}
