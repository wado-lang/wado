//! Remove the work a function left empty no longer needs.
//!
//! A module whose globals all became constants keeps an empty
//! `$initialize_module`, which the program's initializer still calls behind a
//! once-flag. With the calls gone the flag guards nothing. Runs before DCE,
//! which drops the functions and the flag this leaves unreferenced.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirExportDesc, WirFunction, WirInstr, WirPackage};
use crate::wir_visitor::WirMutVisitor;

use super::cleanup::clean_body;
use super::nullability::Nullability;
use super::util::{is_side_effect_free, may_trap_in};

/// Remove every call to a defined function that does nothing, keeping what its
/// arguments do, and every once-guard whose flag nothing else reads.
/// Either can empty a function in turn, so this runs to a fixpoint.
pub(super) fn elide_empty_work(module: &mut WirPackage) {
    while elide_calls_to_empty_functions(module) | remove_dead_once_guards(module) {}
}

fn elide_calls_to_empty_functions(module: &mut WirPackage) -> bool {
    let empty: IndexSet<u32> = module
        .functions
        .iter()
        .enumerate()
        .filter(|(_, func)| does_nothing(module, func))
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
        if !body.iter().any(|instr| holds_call_into(instr, &empty)) {
            continue;
        }
        let locals = func.declared_locals();
        let body = func.body.as_mut().expect("checked above");
        EmptyCallElider {
            empty: &empty,
            null: &Nullability::new(&locals),
        }
        .visit_body(body);
        assert!(
            !body.iter().any(|instr| holds_call_into(instr, &empty)),
            "a call returning nothing stands only as a statement of a body, which the elider reaches",
        );
        clean_body(body);
        changed = true;
    }
    changed
}

/// Emit ends a function with results in `unreachable`, so an empty body does
/// nothing only where the function returns nothing.
fn does_nothing(module: &WirPackage, func: &WirFunction) -> bool {
    func.body.as_ref().is_some_and(Vec::is_empty)
        && module.types[func.type_id.index() as usize]
            .expect_func()
            .results
            .is_empty()
}

fn is_call_into(instr: &WirInstr, empty: &IndexSet<u32>) -> bool {
    matches!(instr, WirInstr::Call { func_id, .. } if empty.contains(&func_id.index()))
}

fn holds_call_into(instr: &WirInstr, empty: &IndexSet<u32>) -> bool {
    let mut found = is_call_into(instr, empty);
    instr.for_each_child(&mut |child| found = found || holds_call_into(child, empty));
    found
}

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
    let WirInstr::Block {
        result: None, body, ..
    } = instr
    else {
        return None;
    };
    let [
        WirInstr::BrIf {
            depth: 0,
            condition,
        },
        rest @ ..,
    ] = body.as_slice()
    else {
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
    let mut guards: IndexMap<String, usize> = IndexMap::default();
    for_each_instr_in(module, &mut |instr| {
        if let Some(flag) = once_guard_flag(instr) {
            *guards.entry(flag.to_string()).or_default() += 1;
        }
    });
    for export in &module.exports {
        if let WirExportDesc::Global { name } = &export.desc {
            guards.shift_remove(&name.fq);
        }
    }
    if guards.is_empty() {
        return false;
    }
    let mut reads: IndexMap<&str, usize> = guards.keys().map(|flag| (flag.as_str(), 0)).collect();
    for_each_instr_in(module, &mut |instr| {
        if let WirInstr::GlobalGet { name, .. } = instr
            && let Some(count) = reads.get_mut(name.fq.as_str())
        {
            *count += 1;
        }
    });
    let dead: IndexSet<String> = guards
        .iter()
        .filter(|(flag, count)| reads[flag.as_str()] == **count)
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

/// Every instruction of every function body and global initializer.
fn for_each_instr_in(module: &WirPackage, f: &mut impl FnMut(&WirInstr)) {
    fn walk(instr: &WirInstr, f: &mut impl FnMut(&WirInstr)) {
        f(instr);
        instr.for_each_child(&mut |child| walk(child, f));
    }
    let bodies = module
        .functions
        .iter()
        .filter_map(|func| func.body.as_ref());
    for instr in bodies.flatten() {
        walk(instr, f);
    }
    for global in &module.globals {
        walk(&global.init, f);
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
    use crate::wir::{
        WirFuncId, WirFuncType, WirFunction, WirLocals, WirMeta, WirName, WirType, WirTypeDef,
        WirTypeId,
    };

    fn func(name: &str, body: Vec<WirInstr>) -> WirFunction {
        WirFunction {
            name: WirName {
                fq: name.to_string(),
            },
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

    const RETURNS_I32: u32 = 1;

    fn func_type(results: Vec<WirType>) -> WirTypeDef {
        WirTypeDef::Func(WirFuncType {
            name: WirName { fq: "t".into() },
            params: Vec::new(),
            results,
        })
    }

    /// Type 0 returns nothing and type [`RETURNS_I32`] an `i32`.
    fn package(functions: Vec<WirFunction>) -> WirPackage {
        let mut module = WirPackage::empty();
        module.types = vec![func_type(Vec::new()), func_type(vec![WirType::I32])];
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
            func(
                "caller",
                vec![
                    declare("a"),
                    declare("b"),
                    call(0, vec![pure, traps, effect]),
                ],
            ),
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

    #[test]
    fn an_empty_function_with_results_traps_so_its_calls_stay() {
        let mut traps = func("traps", Vec::new());
        traps.type_id = WirTypeId::new(RETURNS_I32, Rc::from("t"));
        let mut module = package(vec![
            traps,
            func("caller", vec![WirInstr::Drop(Box::new(call(0, Vec::new())))]),
        ]);
        elide_empty_work(&mut module);
        assert!(matches!(
            module.functions[1].body.as_ref().unwrap().as_slice(),
            [WirInstr::Drop(c)] if matches!(**c, WirInstr::Call { .. })
        ));
    }

    fn guard(flag: &str) -> WirInstr {
        let name = WirName {
            fq: flag.to_string(),
        };
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
            name: WirName {
                fq: "read".to_string(),
            },
            result_ty: WirType::I32,
        }));
        let mut module = package(vec![func(
            "f",
            vec![guard("dead"), guard("read"), read_elsewhere],
        )]);
        elide_empty_work(&mut module);
        let body = module.functions[0].body.as_ref().unwrap();
        assert_eq!(body.len(), 2);
        assert_eq!(once_guard_flag(&body[0]), Some("read"));
    }
}
