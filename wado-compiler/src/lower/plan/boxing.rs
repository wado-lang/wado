//! Address-taken storage. A local whose address is taken, of a type a
//! reference reaches through a cell (`TypeTable::is_boxed_reference_target`),
//! lives in one: its declaration becomes `&mut T`, and an address-taken
//! parameter gets a cell-holding shadow local. No type changes meaning. The
//! reads and writes through a cell are the TIR → NIR fold's
//! ([`crate::lower::translate`]).

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::tir::{
    LetStorage, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirPattern, TirStmt,
    TirStmtKind, TirUnaryOp, TypeId,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};
use crate::token::Span;
use crate::type_table::TypeTable;

/// The locals each function keeps in a cell.
#[derive(Default)]
pub struct BoxPlan {
    cell_locals: IndexMap<(ModuleSource, String), IndexSet<u32>>,
}

impl BoxPlan {
    /// The locals `func` keeps in a cell, each declared `&mut T` of the `T` it
    /// holds.
    pub fn cell_locals(&self, func: &TirFunction) -> Option<&IndexSet<u32>> {
        self.cell_locals
            .get(&(func.module_source.clone(), func.name.clone()))
    }

    /// Move each address-taken local of `flat`'s functions into a cell.
    fn shadow_functions(&mut self, flat: &FlatPackage) {
        let mut type_table = flat.type_table.borrow_mut();
        for func_rc in &flat.functions {
            let mut func = func_rc.borrow_mut();
            let cells = shadow_one_function(&mut func, &mut type_table);
            if !cells.is_empty() {
                self.cell_locals
                    .insert((func.module_source.clone(), func.name.clone()), cells);
            }
        }
    }
}

/// Mint the cell of each type a slot holds in one, and move every function's
/// address-taken locals into cells.
pub fn prepare(flat: &FlatPackage) -> BoxPlan {
    {
        let mut type_table = flat.type_table.borrow_mut();
        let slots: Vec<TypeId> = type_table
            .iter_type_ids()
            .filter(|&ty| type_table.boxes_slot(ty))
            .collect();
        for ty in slots {
            type_table.register_slot_box(ty);
        }
    }
    let mut plan = BoxPlan::default();
    plan.shadow_functions(flat);
    plan
}

/// For an address-taken parameter, a cell shadow local, a prelude `Let` and a
/// remap of its `&` (and a `mut` one's reads); for any other address-taken
/// local, its declaration retagged to its cell. Answers the cell locals.
fn shadow_one_function(func: &mut TirFunction, type_table: &mut TypeTable) -> IndexSet<u32> {
    let address_taken = still_taken(func);
    let param_count = u32::try_from(func.params.len()).unwrap();

    // (param_idx, shadow_local_idx, original_type_id, name)
    let mut shadowed_params: Vec<(u32, u32, TypeId, String)> = Vec::new();
    let mut effective_address_taken: IndexSet<u32> = address_taken.keys().copied().collect();
    let mut cells = IndexSet::default();

    for (&local_idx, &read_type) in &address_taken {
        let is_param = local_idx < param_count;
        // A parameter's cell holds what `&param` reads, which is not its
        // declaration in a clone specialized to a closure literal: that param
        // is a `&$Closure_N`, and `&param` a `&fn(...)` of the canonical value.
        let local_type_id = if is_param {
            read_type
        } else {
            func.locals[local_idx as usize].type_id
        };
        if !type_table.is_boxed_reference_target(local_type_id) {
            continue;
        }
        let cell = type_table.make_mut_ref(local_type_id);
        if is_param {
            let shadow_idx = func.local_count;
            func.local_count += 1;
            let name = func.params[local_idx as usize].name.clone();
            func.locals.push(TirLocal {
                name: format!("$boxed_param_{local_idx}"),
                type_id: cell,
                is_mut: false,
                span: Span::default(),
            });
            effective_address_taken.swap_remove(&local_idx);
            effective_address_taken.insert(shadow_idx);
            shadowed_params.push((local_idx, shadow_idx, local_type_id, name));
            cells.insert(shadow_idx);
        } else {
            func.locals[local_idx as usize].type_id = cell;
            cells.insert(local_idx);
        }
    }

    if !shadowed_params.is_empty()
        && let Some(body) = &mut func.body
    {
        // An immutable parameter's box only gives `&param` an address: nothing
        // writes it, so every other read stays on the parameter, where a
        // constant argument stays visible to `param_spec` in the same round.
        let (mutable, immutable): (Vec<_>, Vec<_>) = shadowed_params
            .iter()
            .map(|(param_idx, shadow_idx, _, _)| (*param_idx, *shadow_idx))
            .partition(|(param_idx, _)| func.params[*param_idx as usize].is_mut);
        remap_locals_in_block(body, &mutable.into_iter().collect());
        RemapReferences(immutable.into_iter().collect()).visit_block(body);

        // The shadow is declared as a cell `let` local is: of the parameter's
        // own type, which the lowering wraps in the cell, sharing the
        // parameter's storage. A `let` of the cell itself, or one that owns its
        // value, would hide from every analysis that the cell holds what the
        // parameter does, the caller's storage included.
        let mut prelude_stmts: Vec<TirStmt> = Vec::with_capacity(shadowed_params.len());
        for (param_idx, shadow_idx, original_type_id, name) in &shadowed_params {
            let span = func.span;
            let param_read = TirExpr::new(
                TirExprKind::Local {
                    index: *param_idx,
                    name: name.clone(),
                },
                *original_type_id,
                span,
            );
            prelude_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: format!("$boxed_param_{param_idx}"),
                    local_index: *shadow_idx,
                    is_mut: false,
                    type_id: *original_type_id,
                    value: param_read,
                    storage: LetStorage::Aliased,
                },
                span,
            ));
        }
        body.stmts.splice(0..0, prelude_stmts);
    }

    // Persist the effective set so downstream optimization passes see shadow
    // locals in place of the original param indices.
    func.address_taken_locals = effective_address_taken;
    cells
}

/// The recorded address-taken locals whose `&` the body still takes, each with
/// the type the body reads it at. A rewrite after the record may fold one away,
/// as a receiver's `*&r` folds to `r`, and boxing that local would cost a box
/// nothing reads through.
fn still_taken(func: &TirFunction) -> IndexMap<u32, TypeId> {
    struct Taken(IndexMap<u32, TypeId>);
    impl TirRefVisitor for Taken {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if let TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: inner,
            } = &expr.kind
                && let TirExprKind::Local { index, .. } = inner.kind
            {
                self.0.insert(index, inner.type_id);
            }
            self.walk_expr_in_frame(expr);
        }
    }
    let Some(body) = &func.body else {
        return IndexMap::default();
    };
    let mut taken = Taken(IndexMap::default());
    taken.visit_block(body);
    func.address_taken_locals
        .iter()
        .filter_map(|local| Some((*local, *taken.0.get(local)?)))
        .collect()
}

/// Point each `&local` and `&mut local` at the local's shadow, leaving its
/// value reads alone. A closure body numbers its own locals, so it is not
/// entered.
struct RemapReferences(IndexMap<u32, u32>);

impl TirMutVisitor for RemapReferences {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: inner,
            } if matches!(inner.kind, TirExprKind::Local { .. }) => {
                if let TirExprKind::Local { index, .. } = &mut inner.kind
                    && let Some(&shadow) = self.0.get(index)
                {
                    *index = shadow;
                }
            }
            TirExprKind::Closure { .. } => {}
            _ => self.walk_expr(expr),
        }
    }
}

/// Rewrite every `Local { index }` reference in `block` according to
/// `remap`. Used by [`shadow_one_function`] to redirect an address-taken
/// `mut` parameter's reads to its cell shadow local.
///
/// Closure bodies are skipped: closure locals live in their own scope
/// and reuse the same indices, so descending would cause unrelated
/// remappings.
fn remap_locals_in_block(block: &mut TirBlock, remap: &IndexMap<u32, u32>) {
    for stmt in &mut block.stmts {
        remap_locals_in_stmt(stmt, remap);
    }
}

fn remap_locals_in_stmt(stmt: &mut TirStmt, remap: &IndexMap<u32, u32>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => remap_locals_in_expr(value, remap),
        TirStmtKind::Expr(expr) => remap_locals_in_expr(expr, remap),
        TirStmtKind::Return { value: Some(v) } => remap_locals_in_expr(v, remap),
        TirStmtKind::Return { value: None } => {}
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            remap_locals_in_expr(condition, remap);
            remap_locals_in_block(then_block, remap);
            if let Some(eb) = else_block {
                remap_locals_in_block(eb, remap);
            }
        }
        TirStmtKind::Loop { body } => remap_locals_in_block(body, remap),
        TirStmtKind::Break { value: Some(v), .. } => remap_locals_in_expr(v, remap),
        TirStmtKind::Break { value: None, .. } | TirStmtKind::Continue => {}
        TirStmtKind::LabeledBlock { block, .. } => remap_locals_in_block(block, remap),
        TirStmtKind::LetDestructure { value, pattern, .. } => {
            remap_locals_in_expr(value, remap);
            remap_locals_in_pattern(pattern, remap);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            remap_locals_in_expr(iterable, remap);
            remap_locals_in_block(body, remap);
        }
    }
}

fn remap_locals_in_expr(expr: &mut TirExpr, remap: &IndexMap<u32, u32>) {
    match &mut expr.kind {
        TirExprKind::Local { index, .. } => {
            if let Some(&new_idx) = remap.get(index) {
                *index = new_idx;
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            remap_locals_in_expr(left, remap);
            remap_locals_in_expr(right, remap);
        }
        TirExprKind::Unary { expr: inner, .. } => remap_locals_in_expr(inner, remap),
        TirExprKind::Assign { target, value } => {
            remap_locals_in_expr(target, remap);
            remap_locals_in_expr(value, remap);
        }
        TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TupleLen { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::VariantTest { expr: inner, .. } => {
            remap_locals_in_expr(inner, remap);
        }
        TirExprKind::VariadicTupleComprehension { .. } => {
            unreachable!("VariadicTupleComprehension should be expanded during monomorphization")
        }
        TirExprKind::Move { .. } => unreachable!("drop elaboration removes every move"),
        TirExprKind::ListViewOf { .. } => {
            unreachable!("synthesis::list_view expands every conversion")
        }
        TirExprKind::Widen { .. } => unreachable!("synthesis::widen expands every widening"),
        TirExprKind::Index { expr: e, index, .. } => {
            remap_locals_in_expr(e, remap);
            remap_locals_in_expr(index, remap);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                remap_locals_in_expr(&mut arg.expr, remap);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                remap_locals_in_expr(arg, remap);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            remap_locals_in_expr(callee, remap);
            for arg in args {
                remap_locals_in_expr(arg, remap);
            }
        }
        TirExprKind::Block(block) => remap_locals_in_block(block, remap),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remap_locals_in_expr(condition, remap);
            remap_locals_in_block(then_branch, remap);
            if let Some(eb) = else_branch {
                remap_locals_in_block(eb, remap);
            }
        }
        TirExprKind::Match { expr: e, arms } => {
            remap_locals_in_expr(e, remap);
            for arm in arms {
                remap_locals_in_pattern(&mut arm.pattern, remap);
                if let Some(g) = &mut arm.guard {
                    remap_locals_in_expr(g, remap);
                }
                remap_locals_in_expr(&mut arm.body, remap);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                remap_locals_in_expr(&mut field.value, remap);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                remap_locals_in_expr(elem, remap);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                remap_locals_in_expr(p, remap);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => remap_locals_in_expr(value, remap),
        TirExprKind::LabeledBlock { block, .. } => remap_locals_in_block(block, remap),
        TirExprKind::WithHandler { bindings, body, .. } => {
            for b in bindings {
                remap_locals_in_expr(&mut b.handler, remap);
            }
            remap_locals_in_block(body, remap);
        }
        // A closure body and a `Capture` read live in the closure's own
        // local-index scope, so neither is descended into. What a capture
        // *names* is a local here, which shadowing a parameter moves.
        TirExprKind::Closure { captures, .. } => {
            for capture in captures {
                capture.source = capture
                    .source
                    .map_local(|index| remap.get(&index).copied().unwrap_or(index));
            }
        }
        TirExprKind::Capture { .. } => {}
        // Leaf nodes with no sub-expressions or no Local references.
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

fn remap_locals_in_pattern(pattern: &mut TirPattern, remap: &IndexMap<u32, u32>) {
    match pattern {
        TirPattern::Binding { local_index, .. } => {
            if let Some(&new_idx) = remap.get(local_index) {
                *local_index = new_idx;
            }
        }
        TirPattern::Tuple(sub, _) => {
            for p in sub {
                remap_locals_in_pattern(p, remap);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                remap_locals_in_pattern(&mut f.pattern, remap);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                remap_locals_in_pattern(p, remap);
            }
        }
        TirPattern::Or(alts) => {
            for p in alts {
                remap_locals_in_pattern(p, remap);
            }
        }
        TirPattern::ConstantValue { expr } => remap_locals_in_expr(expr, remap),
        TirPattern::Narrow {
            local_index, test, ..
        } => {
            if let Some(&new_idx) = remap.get(local_index) {
                *local_index = new_idx;
            }
            remap_locals_in_expr(test, remap);
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::Range { .. }
        | TirPattern::PerInstance { .. } => {}
    }
}
