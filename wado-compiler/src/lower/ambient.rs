//! Settle the guards reify puts around each `#[ambient]` body. The standard
//! library's bodies are reified once into the snapshot every compilation
//! shares, so a guard names its interface and is decided here, where the
//! target world is known: it holds where the world imports the interface,
//! under `wado test`, and in a `--lib` build, whose world imports what the
//! library uses. A body whose guard fails is removed before anything plans
//! imports or strings from it.

use crate::flat_package::FlatPackage;
use crate::name::AMBIENT_GUARD_INTRINSIC;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmtKind};
use crate::tir_visitor::TirMutVisitor;
use crate::world_registry::TEST_WORLD;

pub fn lower(flat: &FlatPackage) {
    flat.visit_bodies_mut(&mut Settle { flat });
}

struct Settle<'a> {
    flat: &'a FlatPackage,
}

impl Settle<'_> {
    fn holds(&self, interface: &str) -> bool {
        if self.flat.target_world == TEST_WORLD {
            return true;
        }
        // The registry holds every fixed world; a `--lib` world is synthesized
        // per package.
        self.flat
            .world_registry
            .get(&self.flat.target_world)
            .is_none_or(|world| world.imports_cm_interface(interface))
    }
}

impl TirMutVisitor for Settle<'_> {
    fn visit_block(&mut self, block: &mut TirBlock) {
        block.stmts.retain_mut(|stmt| {
            let TirStmtKind::If { condition, .. } = &mut stmt.kind else {
                return true;
            };
            let Some(interface) = guarded_interface(condition) else {
                return true;
            };
            let holds = self.holds(interface);
            *condition = TirExpr::new(
                TirExprKind::BoolLiteral(true),
                condition.type_id,
                condition.span,
            );
            holds
        });
        self.walk_block(block);
    }
}

/// The interface `condition` guards on, where it is a guard. The intrinsic is
/// `internal` to the standard library, so reify is its only caller, and it
/// passes a literal.
fn guarded_interface(condition: &TirExpr) -> Option<&str> {
    let TirExprKind::Call { func, args, .. } = &condition.kind else {
        return None;
    };
    if !func.is_builtin_named(AMBIENT_GUARD_INTRINSIC) {
        return None;
    }
    let (None, [arg]) = args.split() else {
        unreachable!("the guard takes one argument");
    };
    let TirExprKind::StringLiteral(interface) = &arg.expr.kind else {
        unreachable!("reify names a guard's interface by a literal");
    };
    Some(interface)
}
