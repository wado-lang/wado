//! Writing a value back into the IR: what becomes of an expression once the
//! projection says what it denotes. Every edit goes through an [`EditSink`], so
//! the same rewrites serve the throwaway frame body and the real one. A scalar
//! promotes to a pure operand, and every other value to the literal tree the
//! lower phase emits for it.

use crate::compiler_item::SeqField;
use crate::const_eval::Value;
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    ArenaStructField, ArmData, BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, PatId,
    PatKind, StmtId, StmtKind,
};
use crate::nir_value_graph::ValueKind;
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

use super::lattice::is_provably_exhaustive;
use super::pattern::PatternMatch;
use super::{BodySink, EditSink, Interpreter, Lattice, PatBindings};

/// A set of local indices, in the form [`crate::nir_value_graph::ValueGraph`]'s
/// opaque-local collection hands back.
type LocalIndexSet = crate::hashmap::IndexSet<u32>;

/// Widest literal tree the writer will emit, counted in the operands it would
/// place. A value the engine holds is bounded per sequence and by nothing across
/// nesting, and past this the loop that computes it is the smaller program.
///
/// A safety valve rather than a tuning knob: over the benchmark and `wasm-size`
/// corpora every value written comes in under 16 operands, so any ceiling from
/// there up emits the same bytes.
const MAX_MATERIALIZED_LEAVES: usize = 1024;

impl Interpreter<'_> {
    /// Splice each constant-condition `if` statement of `block` into `block`
    /// itself, leaving the arm the condition chooses.
    pub fn reduce_local_block<S: EditSink>(&mut self, sink: &mut S, block: BlockId) -> bool {
        let body = sink.body();
        let has_constant_if = body.blocks[block].stmts.iter().any(|s| {
            matches!(
                &body.stmts[*s].kind,
                StmtKind::If { condition, .. }
                    if operand_bool(body, *condition).is_some()
            )
        });
        if !has_constant_if {
            return false;
        }
        let old_stmts = body.blocks[block].stmts.clone();
        let mut new_stmts: Vec<StmtId> = Vec::new();
        for s in old_stmts {
            let body = sink.body();
            let spliced = if let StmtKind::If {
                condition,
                then_block,
                else_block,
            } = &body.stmts[s].kind
            {
                operand_bool(body, *condition).map(|value| (value, *then_block, *else_block))
            } else {
                None
            };
            if let Some((value, then_block, else_block)) = spliced {
                if value {
                    new_stmts.extend(sink.body().blocks[then_block].stmts.clone());
                } else if let Some(eb) = else_block {
                    new_stmts.extend(sink.body().blocks[eb].stmts.clone());
                }
                continue;
            }
            new_stmts.push(s);
        }
        sink.set_block_stmts(block, new_stmts);
        true
    }

    /// Reduce `e` to its flow-sensitive constant value or collapse a constant
    /// branch, committing every edit through `sink`. Both value sources — the
    /// flow-sensitive candidate and a self-contained region run — land through
    /// `Self::commit_fold`, so what is promoted, materialized, memoized, and
    /// refused is decided in one place.
    pub fn reduce_local<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        if let Some(value) = self.flow_fold_candidate(sink.body(), e)
            && self.commit_fold(sink, e, value)
        {
            return true;
        }
        if let Some(value) = self.try_region_fold(sink.body(), e)
            && self.commit_fold(sink, e, value)
        {
            return true;
        }
        if rewrite_short_circuit_via(sink, e) {
            return true;
        }
        if self.rewrite_if_expr_via(sink, e) {
            return true;
        }
        self.rewrite_match_expr_via(sink, e)
    }

    /// Take `value` over `e`: promote a scalar, write an aggregate back as the
    /// literal the lower phase emits for it, memoize what the sink declined, and
    /// report whether the node was rewritten. A reference-typed node is refused —
    /// a fresh literal where the program yields an alias, which `ref.eq` can tell
    /// apart — and so is a leaf under it.
    fn commit_fold<S: EditSink>(&mut self, sink: &mut S, e: ExprId, value: Value) -> bool {
        let node_type = sink.body().exprs[e].type_id;
        if value.is_scalar() {
            if self.type_table.is_reference_shaped(node_type) {
                crate::compiler_trace!("region_seed", "commit {e:?}: refused, reference-shaped");
                return false;
            }
            if sink.replace_with_value(e, value.clone()) {
                return true;
            }
            self.frame.scratch_folds.insert(e, value);
            return false;
        }
        // A backing array is storage, not a value: it reaches the IR as its
        // container's field, where the length says how much of it is live.
        // Writing a literal over the `array_new` that opens a buffer would hand
        // the writes that follow a data segment's worth of zeros instead.
        if matches!(value, Value::Seq { .. }) {
            crate::compiler_trace!("region_seed", "commit {e:?}: a bare backing array");
            return false;
        }
        if sink.edits_the_program() && !self.yields_own_object(sink.body(), e) {
            crate::compiler_trace!("region_seed", "commit {e:?}: not an object of its own");
            return false;
        }
        let consumes = consumes_its_source(&sink.body().exprs[e].kind);
        let committed = self.materialize_via(sink, e, &value);
        crate::compiler_trace!(
            "region_seed",
            "commit {e:?} ({}): aggregate materialize -> {committed} (consumes={consumes})",
            self.type_table.type_name(node_type)
        );
        if consumes {
            self.frame.scratch_folds.insert(e, value);
        }
        committed
    }

    /// Whether `e` yields an object nothing else in the body reaches, so writing
    /// a literal over it keeps the program's identities. A call and a region
    /// hand back what they built, and a literal is one already. A read of a
    /// local does too while nothing else reaches what it holds: two reads, or a
    /// `&p` beside one, leave the program two objects where it had one, which
    /// `ref.eq` tells apart.
    fn yields_own_object(&self, body: &Body, e: ExprId) -> bool {
        match &body.exprs[e].kind {
            ExprKind::Call { .. }
            | ExprKind::LabeledBlock { .. }
            | ExprKind::StructLiteral { .. }
            | ExprKind::TupleLiteral { .. }
            | ExprKind::ArrayLiteral { .. }
            | ExprKind::PackedArray(_)
            | ExprKind::VariantConstruct { .. } => true,
            ExprKind::Local { index, .. } => self.frame.unshared_locals.contains(*index),
            _ => false,
        }
    }

    /// A sequence container still computing its contents: the shape whose
    /// value is worth asking the lattice for, because writing the constant
    /// over one drops an allocation and a copy.
    fn is_unmaterialized_seq_literal(&self, body: &Body, e: ExprId) -> bool {
        self.seq_literal_backing(body, e)
            .is_some_and(|b| !matches!(b, ExprKind::PackedArray(_)))
    }

    /// The backing array a sequence container literal is built over.
    fn seq_literal_backing<'b>(&self, body: &'b Body, e: ExprId) -> Option<&'b ExprKind> {
        let ExprKind::StructLiteral {
            struct_type,
            fields,
            ..
        } = &body.exprs[e].kind
        else {
            return None;
        };
        if !self.type_table.is_seq_container(*struct_type) {
            return None;
        }
        fields
            .iter()
            .find(|f| f.field_index == SeqField::Backing.index())?
            .value
            .as_expr()
            .map(|b| &body.exprs[b].kind)
    }

    /// Write `value` over `e` as the literal tree the lower phase emits for it,
    /// reporting whether anything changed. The walk reuses every node that
    /// already spells its part of the value, so a node the writer itself
    /// produced is left alone and the worklist settles.
    fn materialize_via<S: EditSink>(&self, sink: &mut S, e: ExprId, value: &Value) -> bool {
        let ExprNode { type_id, span, .. } = sink.body().exprs[e];
        let mut budget = MAX_MATERIALIZED_LEAVES;
        if charge_leaves(value, self.type_table, &mut budget).is_none() {
            return false;
        }
        let Some(written) = self.write_value(sink, Some(Operand::Expr(e)), value, type_id, span)
        else {
            return false;
        };
        match written {
            Operand::Expr(fresh) if fresh != e => {
                sink.become_expr(e, fresh);
                true
            }
            _ => false,
        }
    }

    /// The operand spelling `value` at type `ty`, reusing `existing` where it
    /// already spells it and allocating where it does not. `None` refuses the
    /// whole tree: a reference leaf, or a shape the engine cannot name.
    fn write_value<S: EditSink>(
        &self,
        sink: &mut S,
        existing: Option<Operand>,
        value: &Value,
        ty: TypeId,
        span: crate::token::Span,
    ) -> Option<Operand> {
        // A reference names storage; a literal is a fresh object, and `ref.eq`
        // tells the two apart. The node's own type answers for the root, and
        // this answers for every leaf under it.
        if self.type_table.is_reference_shaped(ty) {
            return None;
        }
        match value {
            Value::Aggregate { type_id, .. } if self.type_table.is_seq_container(*type_id) => {
                self.write_container(sink, existing, value, *type_id, ty, span)
            }
            Value::Aggregate { type_id, fields } => {
                self.write_aggregate(sink, existing, fields, *type_id, ty, span)
            }
            Value::Seq { type_id, elements } => {
                self.write_seq(sink, existing, elements, *type_id, ty, span)
            }
            Value::Variant {
                type_id,
                case_name,
                payload,
            } => self.write_variant(
                sink,
                existing,
                *type_id,
                case_name,
                payload.as_deref(),
                ty,
                span,
            ),
            scalar => {
                let kind = scalar_kind(scalar, ty)?;
                if let Some(Operand::Value(v)) = existing
                    && sink.body().values.kind(v) == &kind
                    && sink.body().values.type_of(v) == Some(ty)
                {
                    return existing;
                }
                Some(sink.const_operand(kind, ty))
            }
        }
    }

    /// A `String` or a `List<T>`: the struct over a backing array and a length
    /// the lower phase writes. The backing is cut to that length — capacity is
    /// not observable, and cutting it is what puts a formatted string's bytes in
    /// a data segment sized to the string. Identified by type, never by shape:
    /// over a `Chunk { data, tag }` the literal would read `tag` as a length.
    fn write_container<S: EditSink>(
        &self,
        sink: &mut S,
        existing: Option<Operand>,
        value: &Value,
        type_id: TypeId,
        ty: TypeId,
        span: crate::token::Span,
    ) -> Option<Operand> {
        if ty != type_id {
            return None;
        }
        let Some(Value::Seq {
            type_id: backing_type,
            elements,
        }) = value.field(SeqField::Backing.index())
        else {
            return None;
        };
        let length = value.field(SeqField::Len.index())?;
        // `Value::Int` holds the sign-extended bit pattern, so a negative length
        // reads as a huge `u64` until it is decoded at its own width.
        let (used, PrimitiveType::I32) = length.as_int()? else {
            return None;
        };
        let used = usize::try_from(used as i32).ok()?;
        if used > elements.len() {
            return None;
        }
        // An empty container carries nothing, so a literal over one can only
        // trade a buffer for a smaller buffer — and an opened buffer is exactly
        // what the region about to fill it is holding.
        if used == 0 && existing.is_some() {
            return existing;
        }
        let cut = Value::seq(*backing_type, elements[..used].to_vec())?;
        let previous = existing_fields(sink.body(), existing, type_id);
        let slot = |index: SeqField| slot_at(&previous, index.index() as usize);
        let backing = self.write_value(sink, slot(SeqField::Backing), &cut, *backing_type, span)?;
        let len = self.write_value(sink, slot(SeqField::Len), length, TypeTable::I32, span)?;
        let fields = [(SeqField::Backing, backing), (SeqField::Len, len)];
        assert!(
            fields
                .iter()
                .enumerate()
                .all(|(k, (field, _))| field.index() as usize == k)
        );
        self.struct_literal(
            sink,
            existing,
            previous,
            type_id,
            fields
                .into_iter()
                .map(|(field, op)| (field.field_name().to_string(), op))
                .collect(),
            span,
        )
    }

    /// A struct or a tuple. A tuple's element types come from the type itself;
    /// every other struct needs the declaration shape, and a type the index does
    /// not name is refused.
    fn write_aggregate<S: EditSink>(
        &self,
        sink: &mut S,
        existing: Option<Operand>,
        fields: &[(u32, Value)],
        type_id: TypeId,
        ty: TypeId,
        span: crate::token::Span,
    ) -> Option<Operand> {
        if ty != type_id {
            return None;
        }
        let tuple = self.type_table.as_tuple(type_id);
        let names: Vec<(String, TypeId)> = match &tuple {
            Some(elements) => elements
                .iter()
                .enumerate()
                .map(|(i, t)| (i.to_string(), *t))
                .collect(),
            None => self
                .facts
                .shapes?
                .fields(type_id)?
                .iter()
                .map(|f| (f.name.clone(), f.type_id))
                .collect(),
        };
        if fields.len() != names.len() {
            return None;
        }
        let previous = if tuple.is_some() {
            existing_elements(sink.body(), existing)
        } else {
            existing_fields(sink.body(), existing, type_id)
        };
        let mut written = Vec::with_capacity(names.len());
        for (index, (name, field_type)) in names.into_iter().enumerate() {
            let (recorded, field) = &fields[index];
            // The literal lowers positionally, so a value whose fields do not
            // cover `0..N` in order has no literal to be written as.
            if *recorded != index as u32 {
                return None;
            }
            let slot = slot_at(&previous, index);
            written.push((name, self.write_value(sink, slot, field, field_type, span)?));
        }
        if tuple.is_some() {
            let elements: Vec<Operand> = written.into_iter().map(|(_, op)| op).collect();
            if already_holds(&previous, elements.iter().copied()) {
                return existing;
            }
            return Some(Operand::Expr(sink.alloc_expr(
                ExprKind::TupleLiteral { elements },
                type_id,
                span,
            )));
        }
        self.struct_literal(sink, existing, previous, type_id, written, span)
    }

    /// `elements` as the backing array itself: bytes pack into a `PackedArray`,
    /// which reaches WIR as one `array.new_data`, and everything else into an
    /// `ArrayLiteral`.
    fn write_seq<S: EditSink>(
        &self,
        sink: &mut S,
        existing: Option<Operand>,
        elements: &[Value],
        type_id: TypeId,
        ty: TypeId,
        span: crate::token::Span,
    ) -> Option<Operand> {
        if ty != type_id {
            return None;
        }
        let ResolvedType::BuiltinArray(element_type) = *self.type_table.get(type_id) else {
            return None;
        };
        if element_type == TypeTable::U8 {
            let mut bytes = Vec::with_capacity(elements.len());
            for element in elements {
                let (byte, PrimitiveType::U8) = element.as_int()? else {
                    return None;
                };
                bytes.push(u8::try_from(byte).ok()?);
            }
            if let Some(Operand::Expr(previous)) = existing
                && matches!(&sink.body().exprs[previous].kind, ExprKind::PackedArray(b) if *b == bytes)
            {
                return existing;
            }
            return Some(Operand::Expr(sink.alloc_expr(
                ExprKind::PackedArray(bytes),
                type_id,
                span,
            )));
        }
        let previous = existing_elements(sink.body(), existing);
        let mut written = Vec::with_capacity(elements.len());
        for (index, element) in elements.iter().enumerate() {
            let slot = slot_at(&previous, index);
            written.push(self.write_value(sink, slot, element, element_type, span)?);
        }
        if already_holds(&previous, written.iter().copied()) {
            return existing;
        }
        Some(Operand::Expr(sink.alloc_expr(
            ExprKind::ArrayLiteral { elements: written },
            type_id,
            span,
        )))
    }

    /// A variant case and its payload. The case index and the payload's type are
    /// the declaration's, which only the shape index names.
    #[allow(clippy::too_many_arguments)]
    fn write_variant<S: EditSink>(
        &self,
        sink: &mut S,
        existing: Option<Operand>,
        type_id: TypeId,
        case_name: &str,
        payload: Option<&Value>,
        ty: TypeId,
        span: crate::token::Span,
    ) -> Option<Operand> {
        if ty != type_id {
            return None;
        }
        let case = self.facts.shapes?.case(type_id, case_name)?;
        let previous = match existing.and_then(Operand::as_expr) {
            Some(e) => match &sink.body().exprs[e].kind {
                ExprKind::VariantConstruct {
                    variant_type,
                    case_index,
                    payload,
                    ..
                } if *variant_type == type_id && *case_index == case.index => Some(*payload),
                _ => None,
            },
            None => None,
        };
        let written = match payload {
            Some(payload) => {
                Some(self.write_value(sink, previous.flatten(), payload, case.payload, span)?)
            }
            None => None,
        };
        if previous == Some(written) {
            return existing;
        }
        Some(Operand::Expr(sink.alloc_expr(
            ExprKind::VariantConstruct {
                variant_type: type_id,
                case_index: case.index,
                case_name: case_name.to_string(),
                payload: written,
            },
            type_id,
            span,
        )))
    }

    /// `fields` as a `StructLiteral`, reusing `existing` when `previous` already
    /// holds exactly these operands.
    fn struct_literal<S: EditSink>(
        &self,
        sink: &mut S,
        existing: Option<Operand>,
        previous: Option<Vec<Option<Operand>>>,
        type_id: TypeId,
        fields: Vec<(String, Operand)>,
        span: crate::token::Span,
    ) -> Option<Operand> {
        if already_holds(&previous, fields.iter().map(|(_, op)| *op)) {
            return existing;
        }
        Some(Operand::Expr(
            sink.alloc_expr(
                ExprKind::StructLiteral {
                    struct_type: type_id,
                    struct_name: self.type_table.type_name(type_id),
                    fields: fields
                        .into_iter()
                        .enumerate()
                        .map(|(index, (name, value))| ArenaStructField {
                            name,
                            value,
                            field_index: index as u32,
                        })
                        .collect(),
                },
                type_id,
                span,
            ),
        ))
    }

    /// The environment-free constant scalar `e` denotes: literal arithmetic,
    /// projection out of a constant aggregate, and pure CTFE. `None` when `e`
    /// needs per-function state to fold.
    ///
    /// A caller must keep the interpreter's `env` empty, since a projection's
    /// receiver resolves through it. Env-bound reads belong to
    /// [`Self::flow_fold_value`] instead.
    pub fn const_fold_value(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.const_fold_candidate(body, e).filter(Value::is_scalar)
    }

    fn const_fold_candidate(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        if let Lattice::Const(v) = self.try_fold(body, e) {
            return Some(v);
        }
        if let Some(v) = self.field_projection_value(body, e) {
            return Some(v);
        }
        if let Lattice::Const(v) = self.try_call_fold(body, e) {
            return Some(v);
        }
        if let Some(v) = self.seq_literal_value(body, e) {
            return Some(v);
        }
        None
    }

    /// The constant a sequence container still computing its contents denotes.
    /// The sources above answer for operators, projections and calls; a
    /// container literal's value comes from the projection, and is worth asking
    /// for only where [`Self::commit_fold`] can write it back.
    fn seq_literal_value(&self, body: &Body, e: ExprId) -> Option<Value> {
        if !self.is_unmaterialized_seq_literal(body, e) {
            return None;
        }
        self.expr_to_lattice(body, e).as_const()
    }

    /// The constant a `receiver.field` node reads, when the receiver is a
    /// constant aggregate. Discarding the receiver is safe precisely because it
    /// is constant: nothing observable is dropped and the read cannot trap.
    ///
    /// A call receiver folds here rather than in
    /// [`Self::field_access_lattice`], which cannot run CTFE from `&self` —
    /// that is what lets `factory().field` reduce.
    fn field_projection_value(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } = &body.exprs[e].kind
        else {
            return None;
        };
        let (inner, field_index) = (*inner, *field_index);
        if let Some(v) = self
            .field_access_lattice(body, inner, field_index, field_name)
            .as_const()
        {
            return Some(v);
        }
        let receiver = self.try_call_fold(body, inner.as_expr()?).as_const()?;
        receiver.field(field_index).cloned()
    }

    /// The flow-sensitive constant scalar `e` denotes: everything
    /// [`Self::const_fold_value`] answers, plus `env`-bound locals and
    /// immutable globals. The structural collapses are not included.
    ///
    /// A scalar because only one has an operand form: a constant aggregate
    /// keeps its skeleton node, and only the scalars projected out of it fold.
    pub fn flow_fold_value(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.flow_fold_candidate(body, e).filter(Value::is_scalar)
    }

    fn flow_fold_candidate(&mut self, body: &Body, e: ExprId) -> Option<Value> {
        self.const_fold_candidate(body, e)
            .or_else(|| self.bound_read_value(body, e))
    }

    /// The constant a bare read stands for, out of the per-function state the
    /// environment-free path has none of. Only a `Local` or a `GlobalVarGet`
    /// node reaches an answer here, and neither is a shape
    /// [`Self::const_fold_candidate`] can decide, so which of the two runs
    /// first does not change what folds.
    ///
    /// This is what still folds a `let x = <const>; … x …` that store→load
    /// forwarding missed. A mutable local is recorded `NonConst`, so the value
    /// read here cannot be a stale one.
    fn bound_read_value(&self, body: &Body, e: ExprId) -> Option<Value> {
        match &body.exprs[e].kind {
            ExprKind::Local { .. } => self.expr_to_lattice(body, e).as_const(),
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => self.global_lattice(module_source, name).as_const(),
            _ => None,
        }
    }

    /// Bottom-up reduce the subtree rooted at `e`, so a child fold is
    /// observable at its parent. Used by CTFE to evaluate a callee body whose
    /// children no outer walk has pre-reduced.
    ///
    /// The children come from [`Body::for_each_child`], so a node kind added to
    /// the IR is walked here without anyone remembering to.
    ///
    /// This keeps no flow-sensitive env: reducing an expression is not running
    /// it, and a walk that performs nothing must not record bindings.
    pub fn reduce_in_place(&mut self, body: &mut Body, e: ExprId) -> bool {
        self.reduce_in_place_node(body, NodeRef::Expr(e))
    }

    fn reduce_in_place_node(&mut self, body: &mut Body, node: NodeRef) -> bool {
        let mut changed = match node {
            NodeRef::Expr(e) => self.reduce_children(body, e),
            NodeRef::Block(_) | NodeRef::Stmt(_) | NodeRef::Pat(_) => {
                self.walk_children(body, node)
            }
        };
        changed |= match node {
            NodeRef::Expr(e) => self.reduce_local(&mut BodySink { body }, e),
            NodeRef::Block(b) => self.reduce_local_block(&mut BodySink { body }, b),
            NodeRef::Stmt(_) | NodeRef::Pat(_) => false,
        };
        changed
    }

    /// The children of an expression, with the two the generic walk must not
    /// hand over as-is.
    ///
    /// A `Match` arm reduces under the bindings its own pattern makes, so each
    /// is walked in its own scope. An `Assign` target names storage rather than
    /// a value, so it is not walked at all: a literal cannot stand where the
    /// program writes. Only the receiver it projects out of is descended into.
    fn reduce_children(&mut self, body: &mut Body, e: ExprId) -> bool {
        match &body.exprs[e].kind {
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee = *scrutinee;
                let arm_data: Vec<(Option<Operand>, PatId, Operand)> =
                    arms.iter().map(|a| (a.guard, a.pattern, a.body)).collect();
                let mut changed = self.reduce_in_place_operand(body, scrutinee);
                for (guard, pattern, arm_body) in arm_data {
                    let binds = self.arm_bindings(body, scrutinee, pattern);
                    let scope = self.enter_arm(&binds);
                    if let Some(g) = guard {
                        changed |= self.reduce_in_place_operand(body, g);
                    }
                    changed |= self.reduce_in_place_operand(body, arm_body);
                    self.leave_arm(scope);
                }
                changed
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                let mut changed = self.reduce_in_place_operand(body, value);
                let receiver = match &body.exprs[target].kind {
                    ExprKind::FieldAccess { expr: inner, .. }
                    | ExprKind::Index { expr: inner, .. } => Some(*inner),
                    _ => None,
                };
                if let Some(receiver) = receiver {
                    changed |= self.reduce_in_place_operand(body, receiver);
                }
                changed
            }
            _ => self.walk_children(body, NodeRef::Expr(e)),
        }
    }

    fn walk_children(&mut self, body: &mut Body, node: NodeRef) -> bool {
        let mut children = Vec::new();
        body.for_each_child(node, |c| children.push(c));
        let mut changed = false;
        for child in children {
            changed |= self.reduce_in_place_node(body, child);
        }
        changed
    }

    /// Reduce an operand in place: a no-op (`false`) for a promoted pure value
    /// (already reduced), else reduce the skeleton subtree.
    fn reduce_in_place_operand(&mut self, body: &mut Body, op: Operand) -> bool {
        op.as_expr().is_some_and(|e| self.reduce_in_place(body, e))
    }

    /// Project `e` to a lattice, assuming its children are already reduced (the
    /// const-fold visitor walks bottom-up): `try_fold` sees folded children
    /// directly, and a non-foldable node falls through to `expr_to_lattice`.
    pub fn reduce_to_lattice(&self, body: &Body, e: ExprId) -> Lattice {
        match self.try_fold(body, e) {
            Lattice::Unevaluated => self.expr_to_lattice(body, e),
            other => other,
        }
    }

    /// Collapse an `if` with a constant condition or equal arms.
    fn rewrite_if_expr_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        let (condition, then_branch, else_branch) = match &sink.body().exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => (*condition, *then_branch, *else_branch),
            _ => return false,
        };
        if let Lattice::Const(Value::Bool(taken)) = self.operand_to_lattice(sink.body(), condition)
        {
            splice_chosen_if_branch(sink, e, taken, then_branch, else_branch);
            return true;
        }
        let Some(else_branch) = else_branch else {
            return false;
        };
        let (Lattice::Const(then_value), Lattice::Const(else_value)) = (
            self.block_lattice(sink.body(), then_branch),
            self.block_lattice(sink.body(), else_branch),
        ) else {
            return false;
        };
        collapse_bool_arms(sink, e, condition, &then_value, &else_value)
            || collapse_equal_arms(sink, e, condition, then_value, else_value)
    }

    /// Collapse a `match` with a constant scrutinee or a bool-discriminator
    /// shape.
    ///
    /// A constant scrutinee decides the whole rewrite: the arm it picks is the
    /// only sound one, so failing to pick it rules out the other collapses too.
    fn rewrite_match_expr_via<S: EditSink>(&mut self, sink: &mut S, e: ExprId) -> bool {
        let ExprKind::Match {
            expr: scrutinee,
            arms,
        } = &sink.body().exprs[e].kind
        else {
            return false;
        };
        if arms.is_empty() {
            return false;
        }
        let (scrutinee, arms) = (*scrutinee, ArmParts::of(arms));
        if let Lattice::Const(scrutinee_value) = self.operand_to_lattice(sink.body(), scrutinee) {
            return self.splice_chosen_match_arm(sink, e, &scrutinee_value, &arms);
        }
        rewrite_bool_discriminator(sink, e, scrutinee, &arms)
            || self.collapse_equal_match_arms(sink, e, scrutinee, &arms)
    }

    /// Replace the `match` with the arm a constant scrutinee selects, wrapped in
    /// a block.
    ///
    /// A guard is only meaningful with the arm's bindings in scope, and an
    /// undecided one may still be taken, leaving every later arm unreachable.
    /// Splicing also strips the pattern, so an arm whose body still reads a
    /// binding is left alone rather than left dangling.
    fn splice_chosen_match_arm<S: EditSink>(
        &mut self,
        sink: &mut S,
        e: ExprId,
        scrutinee_value: &Value,
        arms_data: &[ArmParts],
    ) -> bool {
        let mut chosen: Option<(&ArmParts, PatBindings)> = None;
        for arm in arms_data {
            let mut binds = PatBindings::new();
            match self.pattern_matches(sink.body(), scrutinee_value, arm.pattern, &mut binds) {
                PatternMatch::No => continue,
                PatternMatch::Unknown => return false,
                PatternMatch::Yes => {}
            }
            match arm.guard {
                None => {}
                Some(g) => match self.guard_under_bindings(sink.body(), g, &binds) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => return false,
                },
            }
            chosen = Some((arm, binds));
            break;
        }
        let Some((arm, binds)) = chosen else {
            return false;
        };
        let body_op = arm.body;
        if operand_reads_any_local(sink.body(), body_op, &binds) {
            return false;
        }
        // Splicing keeps the body and drops the guard, which is sound only while
        // the guard is a test. A nested pattern lowers its sub-bindings into one
        // (`… && { let x = p.end.x; true }`), and dropping that leaves the body
        // reading a local nothing binds any more.
        if arm
            .guard
            .is_some_and(|g| guard_bindings_escape(sink.body(), g, body_op))
        {
            return false;
        }
        let span = match body_op {
            Operand::Expr(ex) => sink.body().exprs[ex].span,
            Operand::Value(_) => arm.span,
        };
        let stmt = sink.alloc_stmt(StmtKind::Expr(body_op), span);
        let block = sink.alloc_block(vec![stmt], span);
        let ty = sink.body().exprs[e].type_id;
        sink.replace_kind(e, ExprKind::plain_block(block, ty, "arm"));
        true
    }

    /// Every arm constant and equal makes the `match` denote that constant,
    /// once the scrutinee is one the rewrite may delete. A promoted
    /// `Operand::Value` scrutinee is itself a constant.
    fn collapse_equal_match_arms<S: EditSink>(
        &mut self,
        sink: &mut S,
        e: ExprId,
        scrutinee: Operand,
        arms_data: &[ArmParts],
    ) -> bool {
        if !is_discardable_operand(sink.body(), scrutinee) {
            return false;
        }
        if arms_data.iter().any(|a| a.guard.is_some()) {
            return false;
        }
        if !is_provably_exhaustive(sink.body(), ArmParts::coverage(arms_data)) {
            return false;
        }
        let mut common: Option<Value> = None;
        for arm in arms_data {
            let Lattice::Const(v) = self.operand_to_lattice(sink.body(), arm.body) else {
                return false;
            };
            match common {
                None => common = Some(v),
                Some(ref c) if !c.denotes_same(&v) => return false,
                Some(_) => {}
            }
        }
        sink.replace_with_value(e, common.expect("at least one arm"))
    }
}

/// The parts of a match arm the rewrites read, lifted out of the body so the
/// sink can be borrowed mutably while they are consulted.
pub(super) struct ArmParts {
    guard: Option<Operand>,
    pattern: PatId,
    body: Operand,
    span: crate::token::Span,
}

impl ArmParts {
    fn of(arms: &[ArmData]) -> Vec<Self> {
        arms.iter()
            .map(|a| Self {
                guard: a.guard,
                pattern: a.pattern,
                body: a.body,
                span: a.span,
            })
            .collect()
    }

    /// What [`is_provably_exhaustive`] asks of each arm.
    fn coverage(arms: &[Self]) -> impl Iterator<Item = (Option<Operand>, PatId)> + '_ {
        arms.iter().map(|a| (a.guard, a.pattern))
    }
}

/// Rewrite `match X { Case => true, _ => false }` to the equality test it is.
/// The scrutinee moves inside the synthesised `Binary`, and the `Match` node
/// keeps its own span — only its kind is replaced.
fn rewrite_bool_discriminator<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    scrutinee: Operand,
    arms_data: &[ArmParts],
) -> bool {
    let Some(replacement) = try_match_bool_discriminator(sink.body(), arms_data) else {
        return false;
    };
    let right = sink.alloc_expr(
        ExprKind::EnumConstruct {
            enum_type: replacement.enum_type,
            case_index: replacement.case_index,
            case_name: replacement.case_name,
        },
        replacement.enum_type,
        replacement.span,
    );
    sink.replace_kind(
        e,
        ExprKind::Binary {
            left: scrutinee,
            op: NirBinaryOp::Eq,
            right: right.into(),
        },
    );
    true
}

/// Replace the `if` with the branch its constant condition chooses. A missing
/// `else` evaluates to unit, whose skeleton form is an empty block — the unit
/// value has no node.
fn splice_chosen_if_branch<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    taken: bool,
    then_branch: BlockId,
    else_branch: Option<BlockId>,
) {
    let ExprNode { span, type_id, .. } = sink.body().exprs[e];
    let block = match (taken, else_branch) {
        (true, _) => then_branch,
        (false, Some(eb)) => eb,
        (false, None) => sink.alloc_block(Vec::new(), span),
    };
    sink.replace_kind(e, ExprKind::plain_block(block, type_id, "branch"));
}

/// `if c { true } else { false }` ≡ `c`, and the mirrored form ≡ `!c`. Splicing
/// the condition in needs its skeleton node — a promoted value has none to
/// clone.
fn collapse_bool_arms<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    condition: Operand,
    then_value: &Value,
    else_value: &Value,
) -> bool {
    let (Value::Bool(then_bool), Value::Bool(else_bool)) = (then_value, else_value) else {
        return false;
    };
    if then_bool == else_bool {
        return false;
    }
    if *then_bool {
        let Some(cond_e) = condition.as_expr() else {
            return false;
        };
        let cond_kind = sink.body().exprs[cond_e].kind.clone();
        sink.replace_kind(e, cond_kind);
    } else {
        sink.replace_kind(
            e,
            ExprKind::Unary {
                op: NirUnaryOp::Not,
                expr: condition,
            },
        );
    }
    true
}

/// Two equal constant arms make the `if` denote that constant, once the
/// condition is one the rewrite may delete.
fn collapse_equal_arms<S: EditSink>(
    sink: &mut S,
    e: ExprId,
    condition: Operand,
    then_value: Value,
    else_value: Value,
) -> bool {
    if !then_value.denotes_same(&else_value) {
        return false;
    }
    if !is_discardable_operand(sink.body(), condition) {
        return false;
    }
    sink.replace_with_value(e, then_value)
}

/// The locals a match-arm guard declares. Non-empty means the guard is more
/// than a test: this lowering puts a nested pattern's sub-bindings inside it,
/// and the arm body reads them, so neither folding it to its value nor
/// splicing the arm without it preserves the program.
///
/// A promoted guard is a pure value, so it declares nothing.
fn guard_bound_locals(body: &Body, guard: Operand) -> LocalIndexSet {
    struct Bound {
        locals: LocalIndexSet,
    }

    impl NirRefVisitor for Bound {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Stmt(s) = node
                && let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
            {
                self.locals.insert(*local_index);
            }
            self.walk_node(body, node);
        }
    }

    let mut bound = Bound {
        locals: LocalIndexSet::default(),
    };
    if let Some(g) = guard.as_expr() {
        bound.visit_node(body, NodeRef::Expr(g));
    }
    bound.locals
}

/// Whether `guard` is more than a test — see [`guard_bound_locals`].
pub(crate) fn guard_declares_locals(body: &Body, guard: Operand) -> bool {
    !guard_bound_locals(body, guard).is_empty()
}

/// Whether `guard` binds a local that `body_op` reads — the locals a nested
/// pattern's sub-bindings occupy, which only the guard declares.
fn guard_bindings_escape(body: &Body, guard: Operand, body_op: Operand) -> bool {
    let bound = guard_bound_locals(body, guard);
    !bound.is_empty() && operand_reads_any_of(body, body_op, &bound)
}

/// Whether the subtree under `op` reads any of the locals `binds` binds.
pub(super) fn operand_reads_any_local(body: &Body, op: Operand, binds: &PatBindings) -> bool {
    let locals: LocalIndexSet = binds.iter().map(|(bound, _)| *bound).collect();
    operand_reads_any_of(body, op, &locals)
}

/// Whether the subtree under `op` reads any of `locals`, in either form: a
/// skeleton `Local` node, or a promoted operand that extracts back to one.
fn operand_reads_any_of(body: &Body, op: Operand, locals: &LocalIndexSet) -> bool {
    struct Reads<'a> {
        locals: &'a LocalIndexSet,
        found: bool,
    }
    impl Reads<'_> {
        fn read_value(&mut self, body: &Body, op: Operand) {
            let Some(v) = op.as_value() else {
                return;
            };
            let mut opaque = LocalIndexSet::default();
            body.values.collect_opaque_locals(v, &mut opaque);
            self.found |= opaque.iter().any(|l| self.locals.contains(l));
        }
    }
    impl NirRefVisitor for Reads<'_> {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node
                && let ExprKind::Local { index, .. } = &body.exprs[e].kind
                && self.locals.contains(index)
            {
                self.found = true;
            }
            // A promoted operand reads its local just as a skeleton `Local` does.
            body.for_each_operand(node, |op| self.read_value(body, op));
            self.walk_node(body, node);
        }
    }
    let mut reads = Reads {
        locals,
        found: false,
    };
    match op {
        Operand::Expr(e) => reads.visit_node(body, NodeRef::Expr(e)),
        Operand::Value(_) => reads.read_value(body, op),
    }
    reads.found
}

/// Simplify a short-circuit one operand already decides. The neutral element
/// keeps the other operand (`true && x` / `false || x` — and their mirrors —
/// become `x`); the absorbing element becomes the result (`false && x` /
/// `true || x` become `false` / `true`).
///
/// A fully-constant short-circuit is left to the const-fold path, which has a
/// value to promote.
pub(super) fn rewrite_short_circuit_via<S: EditSink>(sink: &mut S, e: ExprId) -> bool {
    if let Some(absorbing) = absorbing_short_circuit(sink.body(), e) {
        return sink.replace_with_value(e, Value::Bool(absorbing));
    }
    let body = sink.body();
    let keep: Operand = match &body.exprs[e].kind {
        ExprKind::Binary { left, op, right } => {
            let (left, op, right) = (*left, *op, *right);
            match (operand_bool(body, left), op, operand_bool(body, right)) {
                (Some(false), NirBinaryOp::Or, _) | (Some(true), NirBinaryOp::And, _) => right,
                (_, NirBinaryOp::Or, Some(false)) | (_, NirBinaryOp::And, Some(true)) => left,
                _ => return false,
            }
        }
        _ => return false,
    };
    let Some(keep_e) = keep.as_expr() else {
        return false;
    };
    sink.become_expr(e, keep_e);
    true
}

/// The value a short-circuit collapses to when one operand is its absorbing
/// element — `true` for `||`, `false` for `&&`. `None` unless the *other*
/// operand is discardable: `x || true` still evaluates `x` first, so deleting
/// it is only sound when it can neither trap nor be observed.
pub(super) fn absorbing_short_circuit(body: &Body, e: ExprId) -> Option<bool> {
    let ExprKind::Binary { left, op, right } = &body.exprs[e].kind else {
        return None;
    };
    let (left, op, right) = (*left, *op, *right);
    let absorbing = match op {
        NirBinaryOp::Or => true,
        NirBinaryOp::And => false,
        _ => return None,
    };
    let discarded = if operand_bool(body, left) == Some(absorbing) {
        right
    } else if operand_bool(body, right) == Some(absorbing) {
        left
    } else {
        return None;
    };
    is_discardable_operand(body, discarded).then_some(absorbing)
}

/// Whether `e` can be deleted outright: side-effect-free, and trap-free on top
/// of that.
///
/// A `Cast` and a `FieldAccess` are excluded even though nothing observes them:
/// a float-to-int cast lowers to the trapping `trunc` family and a field read
/// traps on a null reference, and deleting either erases a trap the program is
/// entitled to observe.
pub(super) fn is_discardable(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, op, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_discardable_operand(body, *left)
                && is_discardable_operand(body, *right)
        }
        ExprKind::Unary { op, expr: inner } => {
            !matches!(op, NirUnaryOp::Deref) && is_discardable_operand(body, *inner)
        }
        _ => false,
    }
}

/// Operand form of [`is_discardable`]: a promoted pure value (a constant) is
/// always discardable.
pub(super) fn is_discardable_operand(body: &Body, op: crate::nir_arena::Operand) -> bool {
    op.as_expr().is_none_or(|e| is_discardable(body, e))
}

/// The shapes whose fold consumes what produced the value: a call body run to
/// completion, a region run as a frame. Rewriting one replaces the node with a
/// kind no rewrite matches again, so re-materializing cannot ping the worklist
/// forever — and the value is worth memoizing, since a revisit would otherwise
/// run the body again.
fn consumes_its_source(kind: &ExprKind) -> bool {
    matches!(kind, ExprKind::Call { .. } | ExprKind::LabeledBlock { .. })
}

/// Charge `budget` one per operand writing `value` would place, failing as soon
/// as it runs out. A byte sequence costs one, since it packs into a single
/// `PackedArray` however long it is.
fn charge_leaves(value: &Value, type_table: &TypeTable, budget: &mut usize) -> Option<()> {
    let children: &[Value] = match value {
        Value::Aggregate { fields, .. } => {
            for (_, field) in fields.iter() {
                charge_leaves(field, type_table, budget)?;
            }
            return Some(());
        }
        Value::Seq { type_id, elements } => {
            if matches!(
                type_table.get(*type_id),
                ResolvedType::BuiltinArray(e) if *e == TypeTable::U8
            ) {
                *budget = budget.checked_sub(1)?;
                return Some(());
            }
            elements
        }
        Value::Variant { payload, .. } => {
            return match payload.as_deref() {
                Some(payload) => charge_leaves(payload, type_table, budget),
                None => Some(()),
            };
        }
        _ => {
            *budget = budget.checked_sub(1)?;
            return Some(());
        }
    };
    for child in children {
        charge_leaves(child, type_table, budget)?;
    }
    Some(())
}

/// The pooled constant a scalar becomes at `ty`. `None` for an aggregate, which
/// the pool models no operand form for.
fn scalar_kind(value: &Value, ty: TypeId) -> Option<ValueKind> {
    Some(match value {
        Value::Int { value, .. } => ValueKind::Int(*value, ty),
        Value::Float { value, .. } => ValueKind::Float(value.to_bits(), ty),
        Value::Bool(b) => ValueKind::Bool(*b),
        Value::Char(c) => ValueKind::Char(*c),
        Value::Null => ValueKind::Null,
        Value::Unit => ValueKind::Unit,
        Value::Aggregate { .. } | Value::Seq { .. } | Value::Variant { .. } => return None,
    })
}

/// The operands a struct literal of `type_id` already holds, in `field_index`
/// order. `None` when `existing` is not one, which makes every slot fresh.
fn existing_fields(
    body: &Body,
    existing: Option<Operand>,
    type_id: TypeId,
) -> Option<Vec<Option<Operand>>> {
    let ExprKind::StructLiteral {
        struct_type,
        fields,
        ..
    } = &body.exprs[existing?.as_expr()?].kind
    else {
        return None;
    };
    if *struct_type != type_id {
        return None;
    }
    let mut slots = vec![None; fields.len()];
    for field in fields {
        *slots.get_mut(field.field_index as usize)? = Some(field.value);
    }
    Some(slots)
}

/// The operand `previous` holds at `index`, or `None` where it holds none.
fn slot_at(previous: &Option<Vec<Option<Operand>>>, index: usize) -> Option<Operand> {
    previous.as_ref()?.get(index).copied().flatten()
}

/// Whether `previous` already spells `written`, so the node holding it stands.
fn already_holds(
    previous: &Option<Vec<Option<Operand>>>,
    written: impl IntoIterator<Item = Operand>,
) -> bool {
    previous
        .as_ref()
        .is_some_and(|p| p.iter().copied().eq(written.into_iter().map(Some)))
}

/// The operands a positional literal already holds. `None` when `existing` is
/// not one.
fn existing_elements(body: &Body, existing: Option<Operand>) -> Option<Vec<Option<Operand>>> {
    let (ExprKind::ArrayLiteral { elements } | ExprKind::TupleLiteral { elements }) =
        &body.exprs[existing?.as_expr()?].kind
    else {
        return None;
    };
    Some(elements.iter().copied().map(Some).collect())
}

/// The boolean value of an operand: a promoted `ValueKind::Bool` in the pool.
/// `None` for any other operand.
pub(super) fn operand_bool(body: &Body, op: Operand) -> Option<bool> {
    match body.values.kind(op.as_value()?) {
        ValueKind::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Recognize `match X { Case => true, _ => false }` as an equality test.
pub(super) fn try_match_bool_discriminator(
    body: &Body,
    arms: &[ArmParts],
) -> Option<EnumEqReplacement> {
    let [yes_arm, no_arm] = arms else {
        return None;
    };
    if yes_arm.guard.is_some() || no_arm.guard.is_some() {
        return None;
    }
    if !matches!(body.pats[no_arm.pattern].kind, PatKind::Wildcard) {
        return None;
    }
    if operand_bool(body, yes_arm.body) != Some(true) {
        return None;
    }
    if operand_bool(body, no_arm.body) != Some(false) {
        return None;
    }
    let PatKind::Enum {
        enum_type,
        case_name,
        case_index,
    } = &body.pats[yes_arm.pattern].kind
    else {
        return None;
    };
    Some(EnumEqReplacement {
        enum_type: *enum_type,
        case_index: *case_index,
        case_name: case_name.clone(),
        span: yes_arm.span,
    })
}

/// The comparison [`try_match_bool_discriminator`] recognised, less the
/// scrutinee, which the caller plugs in.
///
/// Enums only. A `PatKind::Variant` would need the variant decl's case list to
/// synthesise its `VariantTest`, and the pattern does not carry it.
pub(super) struct EnumEqReplacement {
    enum_type: TypeId,
    case_index: u32,
    case_name: String,
    span: crate::token::Span,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::niri::BodySink;
    use crate::token::Span;

    /// `20 + 22`, as the two pooled operands the skeleton carries.
    fn add_body(left: u64, right: u64) -> (Body, ExprId) {
        let mut body = Body::empty();
        let l = body
            .values
            .alloc_unshared(ValueKind::Int(left, TypeTable::I32), TypeTable::I32);
        let r = body
            .values
            .alloc_unshared(ValueKind::Int(right, TypeTable::I32), TypeTable::I32);
        let e = body.exprs.push(ExprNode {
            kind: ExprKind::Binary {
                left: Operand::Value(l),
                op: NirBinaryOp::Add,
                right: Operand::Value(r),
            },
            type_id: TypeTable::I32,
            span: Span::default(),
        });
        let stmt = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(e)),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![stmt],
            span: Span::default(),
        });
        (body, e)
    }

    fn int(value: u64) -> Value {
        Value::Int {
            value,
            prim: crate::tir::PrimitiveType::I32,
        }
    }

    /// A declined fold is remembered whichever backend declined it: the node a
    /// value was folded from does not always survive the rewrite that takes it,
    /// so a later read cannot be asked to recompute.
    #[test]
    fn a_declined_fold_is_remembered() {
        let table = TypeTable::new();
        let mut interp = Interpreter::new(&table);
        let (mut body, e) = add_body(20, 22);

        interp.reduce_local(&mut BodySink { body: &mut body }, e);

        assert_eq!(interp.expr_to_lattice(&body, e), Lattice::Const(int(42)));
    }
}
