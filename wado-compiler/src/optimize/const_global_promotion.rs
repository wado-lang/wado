//! Constant Global Promotion (CGP) optimization
//!
//! After lowering, globals with non-constant initializers (e.g., `global B: i32 = A + 10`)
//! are forced to be Wasm-mutable and initialized at runtime in `__initialize_module()`.
//! After constant propagation and folding optimize the init function body, some of these
//! runtime initializations may have been reduced to scalar constants (e.g., `B = 15`).
//!
//! This pass promotes such globals back to immutable compile-time constants:
//! 1. Scans all functions for `GlobalVarSet` statements with constant values targeting
//!    promotable globals (user-declared immutable, currently Wasm-mutable)
//! 2. Updates the global's initializer and marks it immutable
//! 3. Removes all `GlobalVarSet` to promoted globals from all functions

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmtKind};

use crate::tir_visitor::{TirOptVisitor, opt_walk_block, opt_walk_expr};

type GlobalKey = (ModuleSource, String);

/// Try to promote constant globals in the project.
pub fn promote_constant_globals(project: &mut FlatPackage) -> bool {
    // Build a lookup of promotable globals: currently Wasm-mutable but user-declared immutable
    let mut promotable: IndexMap<GlobalKey, usize> = IndexMap::default();
    for (idx, global) in project.globals.iter().enumerate() {
        if global.mutable && !global.wado_mutable {
            promotable.insert((global.module_source.clone(), global.name.clone()), idx);
        }
    }
    if promotable.is_empty() {
        return false;
    }

    // Phase 1: scan all functions for GlobalVarSet to promotable globals with constant values
    let mut collector = PromotionCollector {
        promotable: &promotable,
        promotions: IndexMap::default(),
    };
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            collector.visit_block(body);
        }
    }
    let promotions = collector.promotions;
    if promotions.is_empty() {
        return false;
    }

    // Phase 2: apply promotions — update global initializers and mark immutable
    for (key, init_kind) in &promotions {
        if let Some(&idx) = promotable.get(key) {
            let global = &mut project.globals[idx];
            global.initializer.kind = init_kind.clone();
            global.mutable = false;
        }
    }

    // Phase 3: remove GlobalVarSet stmts to promoted globals from all functions
    let mut remover = PromotionRemover {
        promotions: &promotions,
    };
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            remover.visit_block(body);
        }
    }

    true
}

struct PromotionCollector<'a> {
    promotable: &'a IndexMap<GlobalKey, usize>,
    promotions: IndexMap<GlobalKey, TirExprKind>,
}

impl TirOptVisitor for PromotionCollector<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        if let TirExprKind::GlobalVarSet {
            module_source,
            name,
            value,
        } = &expr.kind
        {
            let key = (module_source.clone(), name.clone());
            if self.promotable.contains_key(&key) && is_scalar_constant(&value.kind) {
                self.promotions
                    .entry(key)
                    .or_insert_with(|| value.kind.clone());
            }
        }
        opt_walk_expr(self, expr)
    }
}

struct PromotionRemover<'a> {
    promotions: &'a IndexMap<GlobalKey, TirExprKind>,
}

impl TirOptVisitor for PromotionRemover<'_> {
    fn visit_block(&mut self, block: &mut TirBlock) -> bool {
        // Recurse into nested blocks first
        opt_walk_block(self, block);
        // Then remove promoted GlobalVarSet stmts at this level
        let before = block.stmts.len();
        block.stmts.retain(|stmt| {
            let TirStmtKind::Expr(expr) = &stmt.kind else {
                return true;
            };
            let TirExprKind::GlobalVarSet {
                module_source,
                name,
                ..
            } = &expr.kind
            else {
                return true;
            };
            let key = (module_source.clone(), name.clone());
            !self.promotions.contains_key(&key)
        });
        block.stmts.len() != before
    }
}

fn is_scalar_constant(kind: &TirExprKind) -> bool {
    matches!(
        kind,
        TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
    )
}
