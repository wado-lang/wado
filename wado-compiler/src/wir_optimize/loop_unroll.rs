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
//! block pay less per get than one alone. A matrix product's inner loop ran
//! +35% at four, and one twice its size +9% at two, where four ran slower.
//!
//! The tests ahead are computed before the first `WORK` runs, so the pass needs
//! `EXIT` and `STEP` to read only locals `WORK` never writes, and to compute
//! nothing that traps. `WORK` may not branch to the loop or its block, which
//! would leave the copy into the original loop rather than past both.

use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::util::Footprint;

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
            if let Some(unrolled) = unrolled(&body[at]) {
                body.insert(at, unrolled);
                at += 1;
            }
            at += 1;
        }
    }
}

/// The unrolled copy to run ahead of `instr`, when it is a loop this pass
/// unrolls.
fn unrolled(instr: &WirInstr) -> Option<WirInstr> {
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
        WirInstr::LocalSet { name: var, value: step },
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

/// Whether `instr` computes from locals and constants by integer arithmetic
/// that cannot trap, so evaluating it early or twice changes nothing.
fn is_local_arith(instr: &WirInstr) -> bool {
    use WirInstr as I;
    let node_ok = matches!(
        instr,
        I::LocalGet { .. }
            | I::I32Const(_)
            | I::I64Const(_)
            | I::I32Add(..)
            | I::I32Sub(..)
            | I::I32Mul(..)
            | I::I32And(..)
            | I::I32Or(..)
            | I::I32Xor(..)
            | I::I32Shl(..)
            | I::I32ShrS(..)
            | I::I32ShrU(..)
            | I::I32Eqz(_)
            | I::I32Eq(..)
            | I::I32Ne(..)
            | I::I32LtS(..)
            | I::I32LtU(..)
            | I::I32GtS(..)
            | I::I32GtU(..)
            | I::I32LeS(..)
            | I::I32LeU(..)
            | I::I32GeS(..)
            | I::I32GeU(..)
            | I::I32WrapI64(_)
            | I::I64Add(..)
            | I::I64Sub(..)
            | I::I64Mul(..)
            | I::I64And(..)
            | I::I64Or(..)
            | I::I64Xor(..)
            | I::I64Shl(..)
            | I::I64ShrS(..)
            | I::I64ShrU(..)
            | I::I64Eqz(_)
            | I::I64Eq(..)
            | I::I64Ne(..)
            | I::I64LtS(..)
            | I::I64LtU(..)
            | I::I64GtS(..)
            | I::I64GtU(..)
            | I::I64LeS(..)
            | I::I64LeU(..)
            | I::I64GeS(..)
            | I::I64GeU(..)
            | I::I64ExtendI32S(_)
            | I::I64ExtendI32U(_)
    );
    let mut ok = node_ok;
    instr.for_each_child(&mut |child| ok = ok && is_local_arith(child));
    ok
}

/// Replace each read of `var` under `instr` with `value`.
fn substitute(instr: &mut WirInstr, var: &str, value: &WirInstr) {
    if instr.local_read() == Some(var) {
        *instr = value.clone();
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute(child, var, value));
}
