//! Settles the guards reify puts around each `#[ambient]` body, once for the
//! whole component and before anything reads which interfaces it imports. A
//! guard holds where the component has its interface: the world imports it,
//! or code a failing guard does not hide calls into it. The body behind a
//! guard that fails is removed, so no level of optimization imports what only
//! it calls.

use cranelift_entity::EntityRef;

use super::dce::{DescriptorCache, reachable_cm_calls};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::parallel::Executor;
use crate::type_table::TypeTable;

/// The `core:builtin` intrinsic a guard calls.
const GUARD: &str = "ambient_available";

/// One guard: the `if` statement at `stmt` in the function at `func`, whose
/// condition `call` names `interface`.
struct Guard {
    func: usize,
    stmt: StmtId,
    call: ExprId,
    then_block: BlockId,
    interface: String,
    /// The guarded statements while the guard is not known to hold.
    withheld: Option<Vec<StmtId>>,
}

/// Settle every guard; whether there was one.
pub(super) fn settle_ambient_guards(
    project: &mut NirPackage,
    descriptors: &mut DescriptorCache,
    exec: &Executor,
) -> bool {
    let mut guards = find_guards(project);
    if guards.is_empty() {
        return false;
    }
    // A body behind an interface the world lacks starts withheld, and regains
    // itself once its interface proves available: the least set of interfaces
    // that explains itself, so two ambient bodies cannot keep each other's
    // import alive.
    for guard in &mut guards {
        if world_provides(project, &guard.interface) {
            continue;
        }
        let mut func = project.functions[guard.func].borrow_mut();
        let body = guarded_body(&mut func.body);
        guard.withheld = Some(std::mem::take(&mut body.blocks[guard.then_block].stmts));
    }
    while guards.iter().any(|guard| guard.withheld.is_some()) {
        let calls = reachable_cm_calls(project, descriptors, exec);
        let mut regained = false;
        for guard in &mut guards {
            let prefix = format!("{}#", guard.interface);
            if guard.withheld.is_some() && calls.iter().any(|call| call.starts_with(&prefix)) {
                let mut func = project.functions[guard.func].borrow_mut();
                let body = guarded_body(&mut func.body);
                body.blocks[guard.then_block].stmts =
                    guard.withheld.take().expect("checked just above");
                regained = true;
            }
        }
        if !regained {
            break;
        }
    }
    for guard in &guards {
        let mut func = project.functions[guard.func].borrow_mut();
        let body = guarded_body(&mut func.body);
        let holds = body.values.alloc_unshared(
            ValueKind::Bool(guard.withheld.is_none()),
            TypeTable::BOOL,
        );
        let StmtKind::If { condition, .. } = &mut body.stmts[guard.stmt].kind else {
            unreachable!("a guard is an `if` statement");
        };
        *condition = Operand::Value(holds);
        body.exprs[guard.call].kind = ExprKind::Dead;
    }
    true
}

fn guarded_body(body: &mut Option<Body>) -> &mut Body {
    body.as_mut().expect("a guard sits in a body")
}

fn world_provides(project: &NirPackage, interface: &str) -> bool {
    project.is_test_world() || project.world_imports_cm_interface(interface)
}

fn find_guards(project: &NirPackage) -> Vec<Guard> {
    let mut guards = Vec::new();
    for (pos, func) in project.functions.iter().enumerate() {
        let func = func.borrow();
        let Some(body) = &func.body else {
            continue;
        };
        for stmt in body.stmts.keys() {
            let StmtKind::If {
                condition: Operand::Expr(call),
                then_block,
                ..
            } = &body.stmts[stmt].kind
            else {
                continue;
            };
            let ExprKind::Call { func_id, args, .. } = &body.exprs[*call].kind else {
                continue;
            };
            if project.functions[func_id.index()].borrow().intrinsic() != Some(GUARD) {
                continue;
            }
            let (None, [arg]) = args.split() else {
                unreachable!("`{GUARD}` takes one argument");
            };
            guards.push(Guard {
                func: pos,
                stmt,
                call: *call,
                then_block: *then_block,
                interface: literal_text(body, arg.expr),
                withheld: None,
            });
        }
    }
    guards
}

/// The text of the string literal reify passes a guard.
fn literal_text(body: &Body, literal: Operand) -> String {
    let Operand::Expr(literal) = literal else {
        unreachable!("a guard's interface is a string literal");
    };
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[literal].kind else {
        unreachable!("a string literal lowers to its struct");
    };
    let backing = fields
        .iter()
        .find_map(|field| match field.value {
            Operand::Expr(e) => match &body.exprs[e].kind {
                ExprKind::PackedArray(data) => data.as_bytes(),
                _ => None,
            },
            Operand::Value(_) => None,
        })
        .expect("a string literal holds its bytes");
    String::from_utf8(backing.to_vec()).expect("an interface name is UTF-8")
}
