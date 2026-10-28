//! Dead-store elimination over every local, the ones `wir_build` synthesises
//! included, whose names do not exist at TIR for `optimize::elide_local` to
//! see. A store no read can see ([`dead_stores`]) loses it: a `LocalSet(x, v)`
//! becomes nothing when `v` is side-effect-free, else a bare `Drop(v)`, and a
//! `LocalTee(x, v)` becomes `v`. So does a `LocalSet` of an `x` read only by its
//! own values, which liveness keeps live around a loop that feeds `x` to
//! itself. Runs to a fixed point, since eliding one write can strand another.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::liveness::dead_stores;
use super::nullability::Nullability;
use super::util::{count_local_gets, is_side_effect_free, may_trap_in};

pub(super) fn elide_dead_stores(module: &mut WirPackage) {
    for func in &mut module.functions {
        // Declared locals are stable across the sweep (elision only nops stores;
        // `DeclareLocal`s go later in `cleanup`), so read the SSoT once.
        let locals = func.declared_locals();
        if let Some(body) = &mut func.body {
            let null = Nullability::new(&locals);
            while elide_dead_stores_in_body(body, &null) {}
        }
    }
}

fn elide_dead_stores_in_body(body: &mut [WirInstr], null: &Nullability) -> bool {
    let dead = dead_stores(body);
    if !dead.is_empty() {
        let mut stores = ElideDeadStores {
            dead: &dead,
            null,
            preorder: 0,
        };
        for instr in body.iter_mut() {
            stores.visit_instr(instr);
        }
    }
    let mut read_counts: IndexMap<String, u32> = IndexMap::default();
    for instr in body.iter() {
        count_local_gets(instr, &mut read_counts);
    }

    let mut visitor = ElideWriteOnly {
        read_counts: &read_counts,
        null,
        changed: false,
    };
    for instr in body.iter_mut() {
        visitor.visit_instr(instr);
    }
    !dead.is_empty() || visitor.changed
}

/// Removes the stores [`dead_stores`] found, numbering nodes as it does.
struct ElideDeadStores<'a> {
    dead: &'a IndexSet<u32>,
    null: &'a Nullability<'a>,
    preorder: u32,
}

impl WirMutVisitor for ElideDeadStores<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        let id = self.preorder;
        self.preorder += 1;
        self.walk_instr(instr);
        if !self.dead.contains(&id) {
            return;
        }
        *instr = match instr {
            WirInstr::LocalSet { value, .. } => discard(value, self.null),
            WirInstr::LocalTee { value, .. } => std::mem::replace(value.as_mut(), WirInstr::Nop),
            other => unreachable!("dead store #{id} numbered {other:?}"),
        };
    }
}

/// What is left of a store of `value` nobody reads: nothing, or `value`'s
/// effects and traps, which Wado's semantics keep even for an unused binding.
fn discard(value: &mut WirInstr, null: &Nullability) -> WirInstr {
    let value = std::mem::replace(value, WirInstr::Nop);
    if is_side_effect_free(&value) && !may_trap_in(&value, null) {
        WirInstr::Nop
    } else {
        WirInstr::Drop(Box::new(value))
    }
}

struct ElideWriteOnly<'a> {
    /// Whole-body `LocalGet` counts, collected before this sweep. Elisions
    /// during the sweep only remove reads, so a stale count over-approximates
    /// — a store can be kept a round too long, never wrongly elided; the
    /// fixed-point loop converges the leftovers.
    read_counts: &'a IndexMap<String, u32>,
    null: &'a Nullability<'a>,
    changed: bool,
}

impl ElideWriteOnly<'_> {
    /// Is `name` read anywhere outside `own_value` (this store's RHS)?
    /// A store whose only reads sit in its own RHS is still write-only:
    /// dropping it drops those reads with it.
    fn is_read_elsewhere(&self, name: &str, own_value: &WirInstr) -> bool {
        let total = self.read_counts.get(name).copied().unwrap_or(0);
        if total == 0 {
            return false;
        }
        let mut own_counts: IndexMap<String, u32> = IndexMap::default();
        count_local_gets(own_value, &mut own_counts);
        total > own_counts.get(name).copied().unwrap_or(0)
    }
}

impl WirMutVisitor for ElideWriteOnly<'_> {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        if let WirInstr::LocalSet { name, value } = instr
            && !self.is_read_elsewhere(name, value)
        {
            *instr = discard(value, self.null);
            self.changed = true;
            return;
        }
        // `drop(side_effect_free_expr)` is dead — the value is discarded by
        // definition. Catches the statement-position `Expr(struct.new …)` /
        // `Expr(self.field)` shapes the TIR pass leaves alone, not knowing what
        // they were before lowering. At WIR level every effect a call had is
        // already its own node, so a `is_side_effect_free` sub-tree is dead.
        if let WirInstr::Drop(value) = instr
            && is_side_effect_free(value)
            && !may_trap_in(value, self.null)
        {
            *instr = WirInstr::Nop;
            self.changed = true;
            return;
        }
        // A dropped sequence drops only its value: `drop({ s; v })` is
        // `s; drop(v)`, which exposes `v` to the rules above and below.
        if let WirInstr::Drop(value) = instr
            && let WirInstr::Seq(stmts) = value.as_mut()
            && let Some(last) = stmts.pop()
        {
            let mut stmts = std::mem::take(stmts);
            stmts.push(WirInstr::Drop(Box::new(last)));
            *instr = WirInstr::Seq(stmts);
            self.changed = true;
            self.visit_instr(instr);
            return;
        }
        // A dropped allocation or conversion keeps only what its operands do:
        // `drop(struct.new { call(), x })` is `drop(call()); drop(x)`. A
        // discarded `stream.new` / `future.new` pair is this shape.
        if let WirInstr::Drop(value) = instr
            && let Some(operands) = discarded_operands(value)
        {
            *instr = WirInstr::Seq(
                operands
                    .into_iter()
                    .map(|operand| WirInstr::Drop(Box::new(operand)))
                    .collect(),
            );
            self.changed = true;
            self.visit_instr(instr);
            return;
        }
        // Full recursion: statement lists also hide in value positions (an
        // if-expression arm inside a `LocalSet` value, a `Seq` inside a call
        // argument), and a write-only `LocalSet` there is just as dead.
        self.walk_instr(instr);
    }
}

/// The operands of `value`, in evaluation order, where `value` itself neither
/// traps nor has an effect, so discarding it leaves only theirs.
fn discarded_operands(value: &mut WirInstr) -> Option<Vec<WirInstr>> {
    match value {
        WirInstr::StructNew { fields, .. } => Some(std::mem::take(fields)),
        WirInstr::I32WrapI64(operand)
        | WirInstr::I64ExtendI32S(operand)
        | WirInstr::I64ExtendI32U(operand) => {
            Some(vec![std::mem::replace(operand.as_mut(), WirInstr::Nop)])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::{WirFuncId, WirLocals, WirType, WirTypeId};
    use std::assert_matches;
    use std::rc::Rc;

    fn lget(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }
    fn lset(name: &str, value: WirInstr) -> WirInstr {
        WirInstr::LocalSet {
            name: name.to_string(),
            value: Box::new(value),
        }
    }
    fn increment(name: &str) -> WirInstr {
        lset(
            name,
            WirInstr::I32Add(Box::new(lget(name)), Box::new(WirInstr::I32Const(1))),
        )
    }
    fn ret(name: &str) -> WirInstr {
        WirInstr::Return {
            value: Some(Box::new(lget(name))),
        }
    }
    fn call() -> WirInstr {
        WirInstr::Call {
            func_id: WirFuncId::new(0, Rc::from("f")),
            args: vec![],
        }
    }
    fn is_store_of(instr: &WirInstr, local: &str) -> bool {
        matches!(instr, WirInstr::LocalSet { name, .. } if name == local)
    }
    fn elide(body: &mut [WirInstr]) {
        while elide_dead_stores_in_body(body, &Nullability::new(&WirLocals::default())) {}
    }

    /// `x = x + 1` in a loop, with no other reads of `x`, is a dead store,
    /// though the back edge carries `x` to its own RHS.
    #[test]
    fn self_referencing_dead_store_elides() {
        let mut body = vec![WirInstr::Loop {
            label: None,
            body: vec![
                increment("x"),
                WirInstr::BrIf {
                    depth: 0,
                    condition: Box::new(lget("c")),
                },
            ],
        }];
        elide(&mut body);
        let WirInstr::Loop { body, .. } = &body[0] else {
            panic!("expected the loop to survive");
        };
        assert_matches!(body[0], WirInstr::Nop);
    }

    /// A read outside the store's own RHS keeps it.
    #[test]
    fn externally_read_store_is_kept() {
        let mut body = vec![increment("x"), ret("x")];
        elide(&mut body);
        assert!(
            is_store_of(&body[0], "x"),
            "x is read by the return and must survive"
        );
    }

    /// Removing a dead store frees the locals only it read.
    #[test]
    fn a_dead_store_frees_what_it_read() {
        let mut body = vec![increment("x"), lset("sink", lget("x"))];
        elide(&mut body);
        assert_matches!(body.as_slice(), [WirInstr::Nop, WirInstr::Nop]);
    }

    /// A dropped allocation keeps only its operands' effects.
    #[test]
    fn dropped_struct_new_keeps_only_its_operands_effects() {
        let mut body = vec![WirInstr::Drop(Box::new(WirInstr::StructNew {
            type_id: WirTypeId::new(0, Rc::from("pair")),
            fields: vec![WirInstr::I32WrapI64(Box::new(call())), lget("x")],
        }))];
        assert!(elide_dead_stores_in_body(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        let WirInstr::Seq(stmts) = &body[0] else {
            panic!("expected the operands' drops, got {:?}", body[0]);
        };
        assert_matches!(
            stmts.as_slice(),
            [WirInstr::Seq(call), WirInstr::Nop]
                if matches!(call.as_slice(), [WirInstr::Drop(c)] if matches!(**c, WirInstr::Call { .. }))
        );
    }

    /// A tee of a local nothing else reads becomes the value it stores.
    #[test]
    fn unread_tee_becomes_its_value() {
        let tee = WirInstr::LocalTee {
            name: "t".to_string(),
            value: Box::new(lget("x")),
        };
        let mut body = vec![
            lset("y", tee),
            WirInstr::Return {
                value: Some(Box::new(lget("y"))),
            },
        ];
        assert!(elide_dead_stores_in_body(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        let WirInstr::LocalSet { value, .. } = &body[0] else {
            panic!("y is read by the return and must survive");
        };
        assert_matches!(value.as_ref(), WirInstr::LocalGet { name, .. } if name == "x");
    }

    /// A tee's own value reading the local is a live read: the tee's result
    /// carries what that read saw out to the consumer, iteration after
    /// iteration in a loop.
    #[test]
    fn self_reading_tee_is_kept() {
        let tee = WirInstr::LocalTee {
            name: "i".to_string(),
            value: Box::new(WirInstr::I32Add(
                Box::new(lget("i")),
                Box::new(WirInstr::I32Const(1)),
            )),
        };
        let mut body = vec![WirInstr::Loop {
            label: None,
            body: vec![WirInstr::BrIf {
                depth: 0,
                condition: Box::new(WirInstr::I32Ne(
                    Box::new(tee),
                    Box::new(WirInstr::I32Const(3)),
                )),
            }],
        }];
        elide_dead_stores_in_body(&mut body, &Nullability::new(&WirLocals::default()));
        let WirInstr::Loop { body, .. } = &body[0] else {
            panic!("expected the loop to survive");
        };
        let WirInstr::BrIf { condition, .. } = &body[0] else {
            panic!("expected the br_if to survive");
        };
        let WirInstr::I32Ne(lhs, _) = condition.as_ref() else {
            panic!("expected the comparison to survive");
        };
        assert_matches!(lhs.as_ref(), WirInstr::LocalTee { .. });
    }

    /// A store overwritten before any read is dead, though the local is read.
    #[test]
    fn overwritten_store_elides() {
        let mut body = vec![
            lset("x", WirInstr::I32Const(1)),
            lset("x", WirInstr::I32Const(2)),
            ret("x"),
        ];
        elide(&mut body);
        assert_matches!(body[0], WirInstr::Nop);
        assert!(is_store_of(&body[1], "x"));
    }

    /// A store a loop overwrites at its top, before any read, is dead.
    #[test]
    fn store_overwritten_at_loop_top_elides() {
        let mut body = vec![
            lset("x", WirInstr::I32Const(0)),
            WirInstr::Loop {
                label: None,
                body: vec![
                    lset("x", call()),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(lget("x")),
                    },
                ],
            },
            ret("x"),
        ];
        elide(&mut body);
        assert_matches!(body[0], WirInstr::Nop);
        let WirInstr::Loop { body: inner, .. } = &body[1] else {
            panic!("expected the loop to survive");
        };
        assert!(is_store_of(&inner[0], "x"));
    }

    /// A store read on the next iteration, through the back edge, is live.
    #[test]
    fn store_read_across_the_back_edge_is_kept() {
        let mut body = vec![
            lset("x", WirInstr::I32Const(0)),
            WirInstr::Loop {
                label: None,
                body: vec![
                    lset("y", lget("x")),
                    lset("x", call()),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(lget("y")),
                    },
                ],
            },
            ret("y"),
        ];
        elide(&mut body);
        assert!(is_store_of(&body[0], "x"));
        let WirInstr::Loop { body: inner, .. } = &body[1] else {
            panic!("expected the loop to survive");
        };
        assert!(is_store_of(&inner[1], "x"));
    }

    /// A store in an inner loop that only the outer back edge carries to a
    /// read is live, though the outer loop's first pass, under an empty head,
    /// finds it dead.
    #[test]
    fn a_store_the_outer_back_edge_reads_is_kept() {
        let br_if_c = || WirInstr::BrIf {
            depth: 0,
            condition: Box::new(lget("c")),
        };
        let mut body = vec![
            WirInstr::Loop {
                label: None,
                body: vec![
                    lset("y", lget("x")),
                    WirInstr::Loop {
                        label: None,
                        body: vec![lset("x", WirInstr::I32Const(1)), br_if_c()],
                    },
                    br_if_c(),
                ],
            },
            ret("y"),
        ];
        elide(&mut body);
        let WirInstr::Loop { body: outer, .. } = &body[0] else {
            panic!("expected the outer loop to survive");
        };
        let WirInstr::Loop { body: inner, .. } = &outer[1] else {
            panic!("expected the inner loop to survive");
        };
        assert!(is_store_of(&inner[0], "x"));
    }

    /// A `br_table` carries what any of its targets reads.
    #[test]
    fn a_br_table_joins_its_targets() {
        let body_with = |targets: Vec<u32>| {
            vec![
                WirInstr::Block {
                    label: None,
                    result: None,
                    body: vec![
                        WirInstr::Block {
                            label: None,
                            result: None,
                            body: vec![
                                lset("x", WirInstr::I32Const(1)),
                                WirInstr::BrTable {
                                    index: Box::new(lget("c")),
                                    targets,
                                    default: 1,
                                },
                            ],
                        },
                        ret("x"),
                    ],
                },
                lset("x", WirInstr::I32Const(2)),
                ret("x"),
            ]
        };
        let first_store = |body: &[WirInstr]| {
            let WirInstr::Block { body: outer, .. } = &body[0] else {
                panic!("expected the outer block to survive");
            };
            let WirInstr::Block { body: inner, .. } = &outer[0] else {
                panic!("expected the inner block to survive");
            };
            is_store_of(&inner[0], "x")
        };
        let mut body = body_with(vec![0]);
        elide(&mut body);
        assert!(first_store(&body), "target 0 reads x = 1");
        let mut body = body_with(vec![1]);
        elide(&mut body);
        assert!(!first_store(&body), "every target overwrites x");
    }

    /// A multi-value bind overwrites its targets, and is itself kept.
    #[test]
    fn a_multi_value_bind_overwrites_its_targets() {
        let mut body = vec![
            lset("x", WirInstr::I32Const(1)),
            WirInstr::MultiValueLocalBind {
                instr: Box::new(call()),
                locals: vec![Some("x".to_string()), None],
            },
            ret("x"),
        ];
        elide(&mut body);
        assert_matches!(body[0], WirInstr::Nop);
        assert_matches!(body[1], WirInstr::MultiValueLocalBind { .. });
    }

    /// An overwritten tee becomes the value it stores.
    #[test]
    fn an_overwritten_tee_becomes_its_value() {
        let tee = WirInstr::LocalTee {
            name: "x".to_string(),
            value: Box::new(WirInstr::I32Const(1)),
        };
        let mut body = vec![
            lset("y", tee),
            lset("x", WirInstr::I32Const(2)),
            WirInstr::Return {
                value: Some(Box::new(WirInstr::I32Add(
                    Box::new(lget("x")),
                    Box::new(lget("y")),
                ))),
            },
        ];
        elide(&mut body);
        let WirInstr::LocalSet { value, .. } = &body[0] else {
            panic!("y is read by the return and must survive");
        };
        assert_matches!(value.as_ref(), WirInstr::I32Const(1));
    }

    /// A store a branch carries to a read is live; one whose branch skips
    /// every read is dead.
    #[test]
    fn a_branch_carries_liveness_to_its_target() {
        let mut body = vec![
            WirInstr::Block {
                label: None,
                result: None,
                body: vec![
                    lset("x", WirInstr::I32Const(1)),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(lget("c")),
                    },
                    lset("x", WirInstr::I32Const(2)),
                ],
            },
            WirInstr::Block {
                label: None,
                result: None,
                body: vec![lset("y", WirInstr::I32Const(3)), WirInstr::Br { depth: 0 }],
            },
            lset("y", lget("x")),
            ret("y"),
        ];
        elide(&mut body);
        let WirInstr::Block { body: first, .. } = &body[0] else {
            panic!("expected the first block to survive");
        };
        assert!(
            is_store_of(&first[0], "x"),
            "the br_if carries x = 1 to the read"
        );
        let WirInstr::Block { body: second, .. } = &body[1] else {
            panic!("expected the second block to survive");
        };
        assert_matches!(
            second[0],
            WirInstr::Nop,
            "y = 3 is overwritten after the br"
        );
    }

    /// A store one arm overwrites is dead on that arm only.
    #[test]
    fn an_if_arm_without_the_store_keeps_it_live() {
        let arm = |value| WirInstr::If {
            condition: Box::new(lget("c")),
            result: None,
            then_body: vec![lset("x", WirInstr::I32Const(value))],
            else_body: None,
        };
        let mut body = vec![lset("x", WirInstr::I32Const(1)), arm(2), ret("x")];
        elide(&mut body);
        assert!(is_store_of(&body[0], "x"), "the false path reads x = 1");
        let mut body = vec![
            lset("x", WirInstr::I32Const(1)),
            WirInstr::If {
                condition: Box::new(lget("c")),
                result: None,
                then_body: vec![lset("x", WirInstr::I32Const(2))],
                else_body: Some(vec![lset("x", WirInstr::I32Const(3))]),
            },
            ret("x"),
        ];
        elide(&mut body);
        assert_matches!(body[0], WirInstr::Nop, "both arms overwrite x");
    }

    /// A dead store whose value has an effect keeps the effect.
    #[test]
    fn a_dead_store_keeps_its_value_effect() {
        let mut body = vec![
            lset("x", call()),
            lset("x", WirInstr::I32Const(2)),
            ret("x"),
        ];
        elide(&mut body);
        assert_matches!(&body[0], WirInstr::Drop(value) if matches!(**value, WirInstr::Call { .. }));
    }

    /// A write-only local buried in a value-position statement list (an
    /// if-expression arm) is reachable and elided.
    #[test]
    fn write_only_local_in_if_expression_arm_elides() {
        let mut body = vec![
            lset(
                "z",
                WirInstr::If {
                    condition: Box::new(lget("c")),
                    result: Some(WirType::I32),
                    then_body: vec![lset("w", WirInstr::I32Const(5)), WirInstr::I32Const(1)],
                    else_body: Some(vec![WirInstr::I32Const(2)]),
                },
            ),
            WirInstr::Return {
                value: Some(Box::new(lget("z"))),
            },
        ];
        assert!(elide_dead_stores_in_body(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        let WirInstr::LocalSet { value, .. } = &body[0] else {
            panic!("z is read by the return and must survive");
        };
        let WirInstr::If { then_body, .. } = value.as_ref() else {
            panic!("expected if-expression value");
        };
        assert_matches!(
            then_body[0],
            WirInstr::Nop,
            "write-only w inside the arm must elide"
        );
    }
}
