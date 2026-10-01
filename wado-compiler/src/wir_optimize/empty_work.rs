//! Remove the work a function left empty no longer needs.
//!
//! A module whose globals all became constants keeps an empty
//! `$initialize_module`, which the program's initializer still calls behind a
//! once-flag. With the calls gone the flag guards nothing. Runs before DCE,
//! which drops the functions and the flag this leaves unreferenced.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirExportDesc, WirInstr, WirPackage};
use crate::wir_visitor::{WirMutVisitor, WirRefVisitor};

use super::cleanup::clean_body;
use super::nullability::Nullability;
use super::util::{is_side_effect_free, may_trap_in};

/// Remove every call to a defined function whose body is empty, keeping what
/// its arguments do, and every once-guard whose flag nothing else reads.
/// Either can empty a function in turn, so this runs to a fixpoint.
pub(super) fn elide_empty_work(module: &mut WirPackage) {
    while elide_calls_to_empty_functions(module) | remove_dead_once_guards(module) {}
}

fn elide_calls_to_empty_functions(module: &mut WirPackage) -> bool {
    let empty: IndexSet<u32> = module
        .functions
        .iter()
        .enumerate()
        .filter(|(_, func)| func.body.as_ref().is_some_and(Vec::is_empty))
        .map(|(position, _)| module.defined_func_index(position))
        .collect();
    if empty.is_empty() {
        return false;
    }
    let mut changed = false;
    for func in &mut module.functions {
        let Some(body) = &func.body else {
            continue;
        };
        let mut finder = EmptyCallFinder {
            empty: &empty,
            found: false,
        };
        finder.visit_body(body);
        if !finder.found {
            continue;
        }
        let locals = func.declared_locals();
        let body = func.body.as_mut().expect("checked above");
        EmptyCallElider {
            empty: &empty,
            null: &Nullability::new(&locals),
        }
        .visit_body(body);
        clean_body(body);
        changed = true;
    }
    changed
}

fn is_call_into(instr: &WirInstr, empty: &IndexSet<u32>) -> bool {
    matches!(instr, WirInstr::Call { func_id, .. } if empty.contains(&func_id.index()))
}

struct EmptyCallFinder<'a> {
    empty: &'a IndexSet<u32>,
    found: bool,
}

impl WirRefVisitor for EmptyCallFinder<'_> {
    fn visit_instr(&mut self, instr: &WirInstr) {
        self.found |= is_call_into(instr, self.empty);
        if !self.found {
            self.walk_instr(instr);
        }
    }
}

/// A call to an empty function returns nothing, so it only ever stands as a
/// statement of a body.
struct EmptyCallElider<'a> {
    empty: &'a IndexSet<u32>,
    null: &'a Nullability<'a>,
}

impl WirMutVisitor for EmptyCallElider<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        if !body.iter().any(|instr| is_call_into(instr, self.empty)) {
            return;
        }
        let mut kept = Vec::with_capacity(body.len());
        for instr in body.drain(..) {
            match instr {
                WirInstr::Call { func_id, args } if self.empty.contains(&func_id.index()) => {
                    kept.extend(
                        args.into_iter()
                            .filter(|arg| !is_side_effect_free(arg) || may_trap_in(arg, self.null))
                            .map(|arg| WirInstr::Drop(Box::new(arg))),
                    );
                }
                other => kept.push(other),
            }
        }
        *body = kept;
    }
}

/// The flag of `block { br_if 0 (global.get F); global.set F (i32.const _) }`,
/// with `cold_path` markers allowed: a guard whose block, once the flag is
/// clear, does nothing but set it.
fn once_guard_flag(instr: &WirInstr) -> Option<&str> {
    let WirInstr::Block { result: None, body, .. } = instr else {
        return None;
    };
    let [WirInstr::BrIf { depth: 0, condition }, rest @ ..] = body.as_slice() else {
        return None;
    };
    let WirInstr::GlobalGet { name: flag, .. } = condition.peel_hint() else {
        return None;
    };
    rest.iter()
        .all(|instr| match instr {
            WirInstr::ColdPath => true,
            WirInstr::GlobalSet { name, value } => {
                name.fq == flag.fq && matches!(**value, WirInstr::I32Const(_))
            }
            _ => false,
        })
        .then_some(flag.fq.as_str())
}

/// Remove the once-guards whose flag no other instruction reads and no export
/// names: setting such a flag changes nothing anyone can observe.
fn remove_dead_once_guards(module: &mut WirPackage) -> bool {
    let mut reads = FlagReads::default();
    for func in &module.functions {
        if let Some(body) = &func.body {
            reads.visit_body(body);
        }
    }
    for global in &module.globals {
        reads.visit_instr(&global.init);
    }
    let exported: IndexSet<&str> = module
        .exports
        .iter()
        .filter_map(|export| match &export.desc {
            WirExportDesc::Global { name } => Some(name.fq.as_str()),
            _ => None,
        })
        .collect();
    let dead: IndexSet<String> = reads
        .guard_reads
        .iter()
        .filter(|(flag, guards)| reads.all_reads[*flag] == **guards && !exported.contains(flag.as_str()))
        .map(|(flag, _)| flag.clone())
        .collect();
    if dead.is_empty() {
        return false;
    }
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            DeadGuardRemover { dead: &dead }.visit_body(body);
        }
    }
    true
}

/// Per global, how many instructions read it, and how many of those are the
/// condition of a once-guard.
#[derive(Default)]
struct FlagReads {
    all_reads: IndexMap<String, usize>,
    guard_reads: IndexMap<String, usize>,
}

impl WirRefVisitor for FlagReads {
    fn visit_instr(&mut self, instr: &WirInstr) {
        if let WirInstr::GlobalGet { name, .. } = instr {
            *self.all_reads.entry(name.fq.clone()).or_default() += 1;
        }
        if let Some(flag) = once_guard_flag(instr) {
            *self.guard_reads.entry(flag.to_string()).or_default() += 1;
        }
        self.walk_instr(instr);
    }
}

struct DeadGuardRemover<'a> {
    dead: &'a IndexSet<String>,
}

impl WirMutVisitor for DeadGuardRemover<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        body.retain(|instr| !once_guard_flag(instr).is_some_and(|flag| self.dead.contains(flag)));
        self.walk_body(body);
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::wir::{WirFuncId, WirFunction, WirLocals, WirMeta, WirName, WirType, WirTypeId};

    fn func(name: &str, body: Vec<WirInstr>) -> WirFunction {
        WirFunction {
            name: WirName { fq: name.to_string() },
            type_id: WirTypeId::new(0, Rc::from("t")),
            param_names: Vec::new(),
            body: Some(body),
            meta: WirMeta::default(),
            generic_origin: None,
            effects: Vec::new(),
            retains: Vec::new(),
            compiler_item: None,
            export_name: None,
            locals: WirLocals::default(),
        }
    }

    fn call(index: u32, args: Vec<WirInstr>) -> WirInstr {
        WirInstr::Call {
            func_id: WirFuncId::new(index, Rc::from("f")),
            args,
        }
    }

    fn package(functions: Vec<WirFunction>) -> WirPackage {
        let mut module = WirPackage::empty();
        module.functions = functions;
        module
    }

    fn local_get(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }

    fn declare(name: &str) -> WirInstr {
        WirInstr::DeclareLocal {
            name: name.to_string(),
            ty: WirType::I32,
        }
    }

    #[test]
    fn keeps_what_the_arguments_of_an_elided_call_do() {
        let pure = local_get("a");
        let traps = WirInstr::I32DivS(Box::new(local_get("a")), Box::new(local_get("b")));
        let effect = call(2, Vec::new());
        let mut module = package(vec![
            func("empty", Vec::new()),
            func("caller", vec![
                declare("a"),
                declare("b"),
                call(0, vec![pure, traps, effect]),
            ]),
            func("effect", vec![WirInstr::Unreachable]),
        ]);
        elide_empty_work(&mut module);
        let body = module.functions[1].body.as_ref().unwrap();
        let kept: Vec<_> = body
            .iter()
            .filter(|instr| !matches!(instr, WirInstr::DeclareLocal { .. }))
            .collect();
        assert!(
            matches!(kept.as_slice(), [WirInstr::Drop(d), WirInstr::Drop(c)]
                if matches!(**d, WirInstr::I32DivS(..)) && matches!(**c, WirInstr::Call { .. })),
            "{kept:?}"
        );
    }

    #[test]
    fn a_caller_left_empty_loses_its_own_calls() {
        let mut module = package(vec![
            func("empty", Vec::new()),
            func("middle", vec![call(0, Vec::new())]),
            func("top", vec![call(1, Vec::new()), WirInstr::Unreachable]),
        ]);
        elide_empty_work(&mut module);
        assert!(module.functions[1].body.as_ref().unwrap().is_empty());
        assert!(matches!(
            module.functions[2].body.as_ref().unwrap().as_slice(),
            [WirInstr::Unreachable]
        ));
    }

    fn guard(flag: &str) -> WirInstr {
        let name = WirName { fq: flag.to_string() };
        WirInstr::Block {
            label: None,
            result: None,
            body: vec![
                WirInstr::BrIf {
                    depth: 0,
                    condition: Box::new(WirInstr::GlobalGet {
                        name: name.clone(),
                        result_ty: WirType::I32,
                    }),
                },
                WirInstr::ColdPath,
                WirInstr::GlobalSet {
                    name,
                    value: Box::new(WirInstr::I32Const(1)),
                },
            ],
        }
    }

    #[test]
    fn a_guard_is_removed_only_when_nothing_else_reads_its_flag() {
        let read_elsewhere = WirInstr::Drop(Box::new(WirInstr::GlobalGet {
            name: WirName { fq: "read".to_string() },
            result_ty: WirType::I32,
        }));
        let mut module = package(vec![func("f", vec![
            guard("dead"),
            guard("read"),
            read_elsewhere,
        ])]);
        elide_empty_work(&mut module);
        let body = module.functions[0].body.as_ref().unwrap();
        assert_eq!(body.len(), 2);
        assert_eq!(once_guard_flag(&body[0]), Some("read"));
    }
}
