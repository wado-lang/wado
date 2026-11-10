//! Dead field elimination: a struct field no code in the program reads leaves
//! its type, every literal that builds it, every assignment to it, and every
//! pattern that names it.
//!
//! A field is read where a `FieldAccess` names it outside an assignment's
//! target, where a struct pattern binds or tests it, and where a reference to
//! it is taken, which is a `FieldAccess` too. A call to a body-less function
//! whose declaration lets it read everything its arguments reach, as
//! `builtin::black_box` does, reads every field of the structs they reach. A
//! literal's slot for the field
//! and an assignment to it name it without reading it: what they hold is read
//! only if the field is. So liveness is a least fixed point. Every field starts
//! dead, and the operands its slots and assignments hold are walked once it
//! turns live. A copy that reads each field only to build another, such as a
//! `$value_copy$S` helper, keeps none of them alive by itself.
//!
//! An operand that may have an effect, a trap included, runs whether its field
//! lives or not. The walk reads it at once, and the rewrite keeps it where it
//! ran, binding the operands around it so they still run in order.
//!
//! A struct's layout reaches the linear memory of a Component Model call only
//! through the lift and lower code synthesized for it, which reads offsets off
//! the WIT record and each field through a `FieldAccess`. A struct whose layout
//! `wir_build` reads past NIR (`CompilerItem::has_backend_layout`) starts with
//! every field live.

use std::ops::ControlFlow;

use cranelift_entity::EntityRef;

use crate::compiler_item::CompilerItem;
use crate::const_eval::Value;
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, NirLocal, StructInstance, struct_instance};
use crate::nir_arena::{
    ArenaCallArg, ArenaStructField, BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, NodeRef,
    Operand, PatId, PatKind, StmtId, StmtKind, StmtNode, Tracked,
};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{ValueId, ValueKind};
use crate::parallel::Executor;
use crate::primitive::PrimitiveType;
use crate::tir::TypeId;
use crate::type_table::TypeTable;

use super::bounds::Proofs;
use super::dce::deletable_value;
use super::mod_ref::{CallFacts, FnSummaries, summarize};

/// A field of a concrete struct type, by its index.
type FieldKey = (StructInstance, u32);

/// Each struct losing a field: its old field indices mapped to the new ones,
/// `None` for a removed field.
type Renumbering = IndexMap<StructInstance, Vec<Option<u32>>>;

/// Remove every field no code reads. Must run before `promote_fields`, so no
/// promoted value reads a field and every read is a skeleton node.
pub(super) fn remove_dead_fields(project: &mut NirPackage, exec: &Executor) -> bool {
    let summaries = summarize(project, exec).0;
    let types = project.type_table.clone();
    let types = &*types.borrow();
    let no_proofs = Proofs::default();
    let global_calls = CallFacts {
        effects: &summaries.effects,
        proofs: &no_proofs,
    };
    let field_slot = FieldSlot::of_package(project);
    let live = live_fields(project, types, field_slot, &summaries, global_calls);
    let renumbering = renumber(project, &live);
    if renumbering.is_empty() {
        return false;
    }
    let rewrite = |calls| Rewrite {
        types,
        calls,
        field_slot,
        renumbering: &renumbering,
    };
    for (i, func) in project.functions.iter().enumerate() {
        let mut func = func.borrow_mut();
        let parts = func.parts();
        if let Some(body) = parts.body {
            rewrite(summaries.of_body(i)).body(body, Some(parts.locals));
        }
    }
    for global in &mut project.globals {
        rewrite(global_calls).body(global.init.slot_expr_mut().body_mut(), None);
    }
    for decl in &mut project.structs {
        let Some(map) = decl.instance().and_then(|i| renumbering.get(&i)) else {
            continue;
        };
        decl.fields.retain(|f| map[f.index as usize].is_some());
        for field in &mut decl.fields {
            field.index = map[field.index as usize].expect("a kept field has an index");
        }
    }
    true
}

/// The struct instance a value of `type_id` is, through references and
/// newtypes.
fn instance_of(types: &TypeTable, type_id: TypeId) -> Option<StructInstance> {
    struct_instance(types.strip_handles(type_id), types)
}

fn renumber(project: &NirPackage, live: &IndexSet<FieldKey>) -> Renumbering {
    let mut renumbering = Renumbering::default();
    for decl in &project.structs {
        let Some(instance) = decl.instance() else {
            continue;
        };
        let mut next = 0;
        let map: Vec<Option<u32>> = decl
            .fields_in_order()
            .iter()
            .map(|f| {
                live.contains(&(instance.clone(), f.index)).then(|| {
                    next += 1;
                    next - 1
                })
            })
            .collect();
        if map.contains(&None) {
            renumbering.insert(instance, map);
        }
    }
    renumbering
}

/// Where an operand sits: the function or global whose body holds it.
#[derive(Clone, Copy)]
enum Owner {
    Function(usize),
    Global(usize),
}

/// The fields the program reads.
fn live_fields(
    project: &NirPackage,
    types: &TypeTable,
    field_slot: FieldSlot,
    summaries: &FnSummaries,
    global_calls: CallFacts,
) -> IndexSet<FieldKey> {
    let pinned = project.structs.iter().filter(|decl| {
        decl.def
            .decl()
            .and_then(|def| types.compiler_type_item(def))
            .is_some_and(CompilerItem::has_backend_layout)
    });
    let reads_all_reached = project
        .functions
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            let f = f.borrow();
            f.body.is_none() && project.builtin_declarations.reads_all_reached(&*f)
        })
        .map(|(i, _)| FuncId::new(i))
        .collect();
    let mut liveness = Liveness {
        types,
        field_slot,
        reads_all_reached,
        handed: IndexSet::default(),
        live: pinned
            .filter_map(|decl| Some((decl.instance()?, &decl.fields)))
            .flat_map(|(instance, fields)| fields.iter().map(move |f| (instance.clone(), f.index)))
            .collect(),
        held: IndexMap::default(),
        pending: Vec::new(),
    };
    let calls = |owner: Owner| match owner {
        Owner::Function(i) => summaries.of_body(i),
        Owner::Global(_) => global_calls,
    };
    let with_body = |owner: Owner, f: &mut dyn FnMut(&Body)| match owner {
        Owner::Function(i) => {
            if let Some(body) = &project.functions[i].borrow().body {
                f(body);
            }
        }
        Owner::Global(i) => f(project.globals[i].init.slot_expr().body()),
    };
    let owners = (0..project.functions.len())
        .map(Owner::Function)
        .chain((0..project.globals.len()).map(Owner::Global));
    for owner in owners {
        with_body(owner, &mut |body| {
            liveness.walk(owner, body, calls(owner), NodeRef::Block(body.root()));
        });
    }
    loop {
        while let Some((owner, operand)) = liveness.pending.pop() {
            with_body(owner, &mut |body| {
                liveness.walk_operand(owner, body, calls(owner), operand);
            });
        }
        let reached: Vec<FieldKey> = project
            .structs
            .iter()
            .filter_map(|decl| Some((decl.instance()?, decl)))
            .filter(|(instance, decl)| {
                decl.fields
                    .iter()
                    .any(|f| !liveness.live.contains(&(instance.clone(), f.index)))
                    && liveness.reaches(instance)
            })
            .flat_map(|(instance, decl)| {
                decl.fields.iter().map(move |f| (instance.clone(), f.index))
            })
            .collect();
        if reached.is_empty() {
            return liveness.live;
        }
        for key in reached {
            liveness.read(key);
        }
    }
}

struct Liveness<'t> {
    types: &'t TypeTable,
    field_slot: FieldSlot,
    /// The body-less functions a call to which may read everything its
    /// arguments reach.
    reads_all_reached: IndexSet<FuncId>,
    /// The types of the arguments handed to those calls.
    handed: IndexSet<TypeId>,
    live: IndexSet<FieldKey>,
    /// What each dead field's slots and assignments hold, walked once it lives.
    held: IndexMap<FieldKey, Vec<(Owner, Operand)>>,
    /// Operands a field that turned live released, still to walk.
    pending: Vec<(Owner, Operand)>,
}

impl Liveness<'_> {
    /// Whether an argument handed to a call that may read everything it reaches
    /// reaches a struct of `instance`.
    fn reaches(&self, (def, args): &StructInstance) -> bool {
        let object = self
            .types
            .find_struct_instance(*def, args)
            .expect("a concrete struct declaration has its type");
        self.handed.iter().any(|&ty| {
            self.types
                .reaches_object_data(ty, self.types.object_kind(object))
        })
    }

    fn read(&mut self, key: FieldKey) {
        if self.live.insert(key.clone())
            && let Some(held) = self.held.swap_remove(&key)
        {
            self.pending.extend(held);
        }
    }

    fn key(&self, body: &Body, receiver: Operand, field_index: u32) -> Option<FieldKey> {
        let instance = instance_of(self.types, body.operand_type(receiver))?;
        Some((instance, field_index))
    }

    fn walk_operand(&mut self, owner: Owner, body: &Body, calls: CallFacts, operand: Operand) {
        match operand {
            Operand::Expr(e) => self.walk(owner, body, calls, NodeRef::Expr(e)),
            Operand::Value(v) => assert_reads_no_field(body, v),
        }
    }

    /// Walk `operand`, which the field `key` holds: now if it may have an
    /// effect or `key` lives, else once `key` turns live.
    fn hold(
        &mut self,
        owner: Owner,
        body: &Body,
        calls: CallFacts,
        key: Option<FieldKey>,
        operand: Operand,
    ) {
        if let Some(key) = key
            && !self.live.contains(&key)
            && deletable_value(body, operand, self.types, calls)
        {
            self.held.entry(key).or_default().push((owner, operand));
        } else {
            self.walk_operand(owner, body, calls, operand);
        }
    }

    fn walk(&mut self, owner: Owner, body: &Body, calls: CallFacts, root: NodeRef) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if let NodeRef::Expr(e) = node
                && self.walk_field_use(owner, body, calls, e)
            {
                continue;
            }
            if let NodeRef::Pat(p) = node
                && let PatKind::Struct {
                    struct_type,
                    fields,
                    ..
                } = &body.pats[p].kind
                && let Some(instance) = instance_of(self.types, *struct_type)
            {
                for field in fields {
                    if !matches!(body.pats[field.pattern].kind, PatKind::Wildcard) {
                        self.read((instance.clone(), field.field_index));
                    }
                }
            }
            if let NodeRef::Block(b) = node {
                for &s in &body.blocks[b].stmts {
                    if !discards_deletable(body, b, s, self.types, calls) {
                        stack.push(NodeRef::Stmt(s));
                    }
                }
                continue;
            }
            body.for_each_operand(node, |op| {
                if let Operand::Value(v) = op {
                    assert_reads_no_field(body, v);
                }
            });
            body.for_each_child(node, |child| stack.push(child));
        }
    }

    /// Walk `e` if it reads a field, builds a struct, or assigns to a field,
    /// answering whether it did.
    fn walk_field_use(&mut self, owner: Owner, body: &Body, calls: CallFacts, e: ExprId) -> bool {
        match &body.exprs[e].kind {
            ExprKind::FieldAccess {
                expr, field_index, ..
            } => {
                if let Some(key) = self.key(body, *expr, *field_index) {
                    self.read(key);
                }
                self.walk_operand(owner, body, calls, *expr);
            }
            ExprKind::StructLiteral {
                struct_type,
                fields,
                ..
            } => {
                let instance = instance_of(self.types, *struct_type);
                for field in fields {
                    let key = instance.clone().map(|i| (i, field.field_index));
                    self.hold(owner, body, calls, key, field.value);
                }
            }
            ExprKind::Assign { target, value } => {
                let ExprKind::FieldAccess {
                    expr, field_index, ..
                } = &body.exprs[*target].kind
                else {
                    return false;
                };
                let key = self.key(body, *expr, *field_index);
                self.hold(owner, body, calls, key.clone(), *expr);
                self.hold(owner, body, calls, key, *value);
            }
            ExprKind::Call { func_id, args, .. } => {
                if let Some((holder, field_index)) = self.field_slot.of(body, *func_id, args)
                    && let Some(key) = self.key(body, holder, field_index)
                {
                    self.read(key);
                }
                if self.reads_all_reached.contains(func_id) {
                    self.handed
                        .extend(args.iter().map(|arg| body.operand_type(arg.expr)));
                }
                return false;
            }
            ExprKind::Dead
            | ExprKind::PackedArray(_)
            | ExprKind::Local { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::GlobalVarSet { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Cast { .. }
            | ExprKind::CmRawCall { .. }
            | ExprKind::Index { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::TupleLiteral { .. }
            | ExprKind::ArrayLiteral { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::ClosureToCanonical { .. }
            | ExprKind::VariantConstruct { .. }
            | ExprKind::EnumConstruct { .. }
            | ExprKind::LabeledBlock { .. }
            | ExprKind::VariantTag { .. }
            | ExprKind::VariantTest { .. }
            | ExprKind::VariantPayload { .. }
            | ExprKind::Switch { .. } => return false,
        }
        true
    }
}

/// `builtin::field_slot(holder, index)`, which lowering emits for a reference
/// to a field held in a cell: a read of the field by its constant index.
#[derive(Clone, Copy)]
struct FieldSlot(Option<FuncId>);

impl FieldSlot {
    fn of_package(project: &NirPackage) -> Self {
        Self(project.builtin_func_id("field_slot"))
    }

    /// The holder and the field index of a call to `field_slot`.
    fn of(self, body: &Body, func_id: FuncId, args: &[ArenaCallArg]) -> Option<(Operand, u32)> {
        if self.0 != Some(func_id) {
            return None;
        }
        let [holder, index] = args else {
            unreachable!("`field_slot` takes a holder and an index")
        };
        let index = body
            .operand_const_int(index.expr)
            .expect("`field_slot` takes a constant field index");
        Some((
            holder.expr,
            u32::try_from(index).expect("a field index fits u32"),
        ))
    }
}

/// Whether the statement `s` of block `b` computes a deletable value only to
/// discard it: an expression statement other than the last, which may be the
/// block's value. Lowering leaves one for a `_` in a destructuring, say. Such a
/// statement reads nothing, and the rewrite deletes it.
fn discards_deletable(
    body: &Body,
    b: BlockId,
    s: StmtId,
    types: &TypeTable,
    calls: CallFacts,
) -> bool {
    let StmtKind::Expr(op) = body.stmts[s].kind else {
        return false;
    };
    body.blocks[b].stmts.last() != Some(&s) && deletable_value(body, op, types, calls)
}

fn assert_reads_no_field(body: &Body, v: ValueId) {
    let mut seen = IndexSet::default();
    let read = body.values.walk_values(v, &mut seen, |_, kind| {
        if matches!(kind, ValueKind::FieldAccess { .. }) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(true)
        }
    });
    assert!(
        read.is_none(),
        "dead field elimination must run before a field read is promoted"
    );
}

/// The rewrite of one body: dead fields leave its literals, assignments and
/// patterns, and the live ones take their new indices.
struct Rewrite<'a> {
    types: &'a TypeTable,
    calls: CallFacts<'a>,
    field_slot: FieldSlot,
    renumbering: &'a Renumbering,
}

impl Rewrite<'_> {
    fn map(&self, type_id: TypeId) -> Option<&[Option<u32>]> {
        self.renumbering
            .get(&instance_of(self.types, type_id)?)
            .map(Vec::as_slice)
    }

    /// The new index of the field `e` reads, by a `FieldAccess` or a
    /// `field_slot` call, when its struct loses a field: `None` inside for a
    /// removed one.
    fn reads(&self, body: &Body, e: ExprId) -> Option<Option<u32>> {
        let kind = &body.exprs[e].kind;
        let (receiver, field_index) = if let ExprKind::FieldAccess {
            expr, field_index, ..
        } = kind
        {
            (*expr, *field_index)
        } else if let ExprKind::Call { func_id, args, .. } = kind {
            self.field_slot.of(body, *func_id, args)?
        } else {
            return None;
        };
        Some(self.map(body.operand_type(receiver))?[field_index as usize])
    }

    /// Top down, so a dropped operand is never visited: what it reads may be a
    /// removed field. `locals` is `None` for a global's initializer, which
    /// declares none.
    fn body(&self, body: &mut Body, mut locals: Option<&mut Tracked<Vec<NirLocal>>>) {
        let mut built: IndexSet<ExprId> = IndexSet::default();
        let mut stack = vec![NodeRef::Block(body.root())];
        while let Some(node) = stack.pop() {
            match node {
                NodeRef::Expr(e) if !built.contains(&e) => {
                    self.expr(body, e, &mut locals, &mut built);
                }
                NodeRef::Pat(p) => self.pattern(body, p),
                NodeRef::Block(b) => self.drop_discarded(body, b),
                NodeRef::Expr(_) | NodeRef::Stmt(_) => {}
            }
            body.for_each_child(node, |child| stack.push(child));
        }
    }

    /// Drop each statement of block `b` that liveness never walked.
    fn drop_discarded(&self, body: &mut Body, b: BlockId) {
        let kept: Vec<StmtId> = body.blocks[b]
            .stmts
            .iter()
            .copied()
            .filter(|&s| !discards_deletable(body, b, s, self.types, self.calls))
            .collect();
        if kept.len() != body.blocks[b].stmts.len() {
            body.blocks[b].stmts = kept;
        }
    }

    fn expr(
        &self,
        body: &mut Body,
        e: ExprId,
        locals: &mut Option<&mut Tracked<Vec<NirLocal>>>,
        built: &mut IndexSet<ExprId>,
    ) {
        if let Some(new) = self.reads(body, e) {
            let new = new.expect("a field the program reads is live");
            match &mut body.exprs[e].kind {
                ExprKind::FieldAccess { field_index, .. } => *field_index = new,
                ExprKind::Call { args, .. } => {
                    let index = Value::Int {
                        value: u64::from(new),
                        prim: PrimitiveType::I32,
                    };
                    let index = body.values.intern_const(index, TypeTable::I32);
                    args[1].expr = Operand::Value(index);
                }
                other => unreachable!("only a field access or a `field_slot` reads, not {other:?}"),
            }
            return;
        }
        match &body.exprs[e].kind {
            ExprKind::Assign { target, value } => {
                if let ExprKind::FieldAccess {
                    expr, field_index, ..
                } = &body.exprs[*target].kind
                    && let Some(map) = self.map(body.operand_type(*expr))
                    && map[*field_index as usize].is_none()
                {
                    let parts = [*expr, *value];
                    self.drop_store(body, e, parts);
                }
            }
            ExprKind::StructLiteral { struct_type, .. } => {
                if let Some(map) = self.map(*struct_type) {
                    let map = map.to_vec();
                    self.literal(body, e, &map, locals, built);
                }
            }
            ExprKind::Dead
            | ExprKind::PackedArray(_)
            | ExprKind::Local { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::GlobalVarSet { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Call { .. }
            | ExprKind::FieldAccess { .. }
            | ExprKind::CmRawCall { .. }
            | ExprKind::Index { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::TupleLiteral { .. }
            | ExprKind::ArrayLiteral { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::ClosureToCanonical { .. }
            | ExprKind::VariantConstruct { .. }
            | ExprKind::EnumConstruct { .. }
            | ExprKind::LabeledBlock { .. }
            | ExprKind::VariantTag { .. }
            | ExprKind::VariantTest { .. }
            | ExprKind::VariantPayload { .. }
            | ExprKind::Switch { .. } => {}
        }
    }

    /// Replace the assignment `e` to a removed field with a block running what
    /// of its receiver and value may have an effect.
    fn drop_store(&self, body: &mut Body, e: ExprId, parts: [Operand; 2]) {
        let span = body.exprs[e].span;
        let stmts = parts
            .into_iter()
            .filter(|&op| !deletable_value(body, op, self.types, self.calls))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|op| {
                body.stmts.push(StmtNode {
                    kind: StmtKind::Expr(op),
                    span,
                })
            })
            .collect();
        let block = body.blocks.push(BlockNode { stmts, span });
        body.exprs[e].kind = ExprKind::plain_block(block, TypeTable::UNIT, "dead_store");
    }

    /// Drop the removed slots of the literal `e`. A removed slot's operand that
    /// may have an effect still runs in its place, and so the kept operands
    /// are bound to locals first, in order.
    fn literal(
        &self,
        body: &mut Body,
        e: ExprId,
        map: &[Option<u32>],
        locals: &mut Option<&mut Tracked<Vec<NirLocal>>>,
        built: &mut IndexSet<ExprId>,
    ) {
        let ExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } = body.exprs[e].kind.clone()
        else {
            unreachable!("`literal` takes a struct literal")
        };
        let renumbered = |f: &ArenaStructField| {
            map[f.field_index as usize].map(|field_index| ArenaStructField {
                name: f.name.clone(),
                value: f.value,
                field_index,
            })
        };
        let runs: Vec<bool> = fields
            .iter()
            .map(|f| {
                map[f.field_index as usize].is_none()
                    && !deletable_value(body, f.value, self.types, self.calls)
            })
            .collect();
        if !runs.contains(&true) {
            let kept = fields.iter().filter_map(renumbered).collect();
            body.exprs[e].kind = ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields: kept,
            };
            return;
        }
        let locals = locals
            .as_mut()
            .expect("a global's initializer is constant, so no removed slot has an effect");
        let span = body.exprs[e].span;
        let mut stmts = Vec::new();
        let mut kept = Vec::new();
        for (field, runs) in fields.iter().zip(runs) {
            if runs {
                stmts.push(body.stmts.push(StmtNode {
                    kind: StmtKind::Expr(field.value),
                    span,
                }));
                continue;
            }
            let Some(mut field) = renumbered(field) else {
                continue;
            };
            let (bind, read) =
                NirLocal::bind_minted(locals, body, "dead_field_slot", field.value, true, span);
            stmts.push(bind);
            field.value = Operand::Expr(read);
            kept.push(field);
        }
        let type_id = body.exprs[e].type_id;
        let literal = body.exprs.push(ExprNode {
            kind: ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields: kept,
            },
            type_id,
            span,
        });
        built.insert(literal);
        stmts.push(body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(literal)),
            span,
        }));
        let block = body.blocks.push(BlockNode { stmts, span });
        body.exprs[e].kind = ExprKind::plain_block(block, type_id, "dead_field");
    }

    /// Drop a removed field from a struct pattern and renumber the rest.
    fn pattern(&self, body: &mut Body, p: PatId) {
        let PatKind::Struct {
            struct_type,
            fields,
            ..
        } = &body.pats[p].kind
        else {
            return;
        };
        let Some(map) = self.map(*struct_type).map(<[_]>::to_vec) else {
            return;
        };
        for field in fields {
            assert!(
                map[field.field_index as usize].is_some()
                    || matches!(body.pats[field.pattern].kind, PatKind::Wildcard),
                "a pattern that binds or tests a field reads it"
            );
        }
        let PatKind::Struct {
            fields, has_rest, ..
        } = &mut body.pats[p].kind
        else {
            unreachable!("matched above")
        };
        fields.retain(|f| map[f.field_index as usize].is_some());
        for field in fields.iter_mut() {
            field.field_index = map[field.field_index as usize].expect("retained above");
        }
        *has_rest = true;
    }
}
