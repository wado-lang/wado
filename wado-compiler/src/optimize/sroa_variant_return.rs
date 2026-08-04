//! Variant return scalarization for Wado NIR — the return-position dual of
//! [`super::sroa_param`].
//!
//! A function returning a `variant` is rewritten to return a tuple of
//! `[tag, payload slots…]`, so nothing downstream sees a variant in the return
//! position:
//!
//! ```text
//! fn parse(s: String) -> Result<i32, String>
//!   ⇒ fn parse(s: String) -> [i32, i32, Option<String>]
//!
//! return Ok(v)                     ⇒  return [0, v, None]
//! match parse(s) { Ok(v) => A, … }  ⇒  let t = parse(s);
//!                                      match t.0 { 0 => { let v = t.1; A } … }
//! ```
//!
//! The Wasm-level ABI needs no new machinery: [`super::multi_value_return`]
//! already flattens a tuple return whose call sites destructure, so the variant
//! stops being a special case. Running in the optimization loop before
//! `nir/inline` is the point — every later pass then reasons about an integer
//! tag and plain locals instead of one opaque boxed value.
//!
//! ## The slot typing rule
//!
//! Padding is the only part NIR cannot express directly: an unused slot needs a
//! value of the slot's type, and NIR types carry no nullability. The rule keys
//! on the payload's *Wasm* representation, which is what decides whether a
//! wrapper costs anything:
//!
//! | payload `T`                                   | slot        | live      | pad     |
//! | --------------------------------------------- | ----------- | --------- | ------- |
//! | scalar (primitive / enum / flags / resource)  | `T`         | `v`       | `0`     |
//! | already nullable (`Option<X>`, `X` a GC ref)  | `T`         | `v`       | `None`  |
//! | any other GC ref                              | `Option<T>` | `Some(v)` | `None`  |
//! | `Unit`                                        | (no slot)   | —         | —       |
//!
//! `wir_optimize::nullable_ref` lowers `Option<T>` for a non-nullable ref `T`
//! to a bare `ref null T`, so row 3's wrapper is free and the emitted signature
//! matches what `wir_optimize::sroa_variant_return` produces today. Row 2 is
//! what stops the wrapper from ever costing a box: `Option<Option<String>>`
//! holds a *nullable* ref, which that lowering refuses to erase.
//!
//! A pad is never read — every reader tests the tag first.
//!
//! ## Termination
//!
//! A rewritten function returns a tuple, so it can never be a candidate again.
//! The candidate set strictly shrinks, and the pass cannot keep the loop alive.
//!
//! ## Soundness does not rest on validation
//!
//! Validation is a snapshot of the call sites that exist when it runs, and
//! `nir/inline` keeps planting new ones afterwards — so a callee rewritten in
//! one iteration can acquire an unvalidated call site in the next, by which
//! point its signature is committed. Two things close that:
//!
//! - The call-site rewrite runs over *everything scalarized so far*, not just
//!   this round's candidates, so a site inlining just planted still gets the
//!   fast path. It declines per temp ([`temp_uses_rewritable`]) rather than
//!   assuming validation vouched for the shape.
//! - [`rebox_stragglers`] takes what is left: any call the fast path did not
//!   bind is wrapped back into the variant it used to return. Validation is a
//!   profitability filter, not a correctness precondition.
//!
//! A rebox is sound but reinstates the allocation the pass exists to remove, so
//! one firing in steady state means the candidate should not have been taken —
//! it is a bug in the candidate set, not a cost to absorb. The tail-call rule
//! in [`check_uses`] is what keeps the count at zero: a `return g(x)` is only a
//! reason to keep `g` when the caller has a tuple to pass it through.
//!
//! Three places have to agree on what "a binding fed by a call" means — the
//! fast path, the repair, and the invariant check. They all go through
//! [`tail_call_site`], which looks through a block tail, because by the time a
//! later iteration sees a binding, `let_block_flatten` and friends may have
//! wrapped its call in a block. A narrower rule in any one of them makes the
//! repair rebox the fast path's own work.
//!
//! ## Not yet handled
//!
//! The pass is on, with `wir_optimize::sroa_variant_return` still running
//! behind it on the shapes declined here. What is left:
//!
//! - A `LabeledBlock` in return-value position, whose value also arrives via
//!   `break L: v`. Those exits are rejected outright
//!   ([`return_value_scalarizable`]), which is sound but gives up the
//!   `?`-through-`unwrap` shape the WIR pass handles via
//!   `all_br_variant_values_are_struct_new`.
//! - The return temp `field_scalarize` leaves — `let t = [...]; <store>;
//!   return t` — which it hoists *after* this pass has run, one `let` per
//!   branch, so no single-def rule reaches it. `multi_value_return` then
//!   declines and the tuple stays boxed. `wir_optimize`'s `return_temp.rs`
//!   still handles it; see the WEP's open questions.
//! - Three `opt_sroa_variant_*` fixtures stay red on the two shapes above.
//!   They are the worklist, not miscompiles — the suite reports no invalid
//!   Wasm anywhere.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionKind, NirBinaryOp, NirFunction, NirLiteralPattern, NirLocal};
use crate::nir_arena::{
    ArmData, BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, PatKind,
    PatNode, StmtId, StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

/// Widest result tuple the scalarized return may use, counting the tag. Matches
/// `super::multi_value_return`'s own cap, so the classifier that turns this
/// tuple into a Wasm multi-value signature never rejects it on arity.
const MAX_TUPLE_ARITY: usize = 8;

/// `Option`'s case indices, as declared in `lib/core/prelude/types.wado`.
const OPTION_SOME: u32 = 0;
const OPTION_NONE: u32 = 1;

/// The value that fills a slot the live case does not use. Resolved once, when
/// the type table is in hand, so the rewrite never has to consult it again.
#[derive(Clone, Copy)]
enum Pad {
    Int(TypeId),
    Float(TypeId),
    Bool,
    Char,
    /// `None` of the slot's own `Option` type.
    NoneOf(TypeId),
}

/// How a variant case's payload reaches the result tuple.
#[derive(Clone, Copy)]
struct CaseSlot {
    /// Index into [`Layout::slot_types`] (0-based, excluding the tag).
    index: usize,
    /// The live payload needs `Some(_)` to fit the slot (row 3 of the rule).
    wrap_in_some: bool,
}

/// The tuple a variant return is scalarized into. Derived from the variant type
/// alone, so every function returning that variant gets the same layout — which
/// is what lets `return g(x)` stay a bare tail call.
#[derive(Clone)]
struct Layout {
    /// The tuple type replacing the variant return type: `[i32, slots…]`.
    tuple_type: TypeId,
    /// Slot types, excluding the leading tag.
    slot_types: Vec<TypeId>,
    /// Pad per slot, positionally.
    slot_pads: Vec<Pad>,
    /// Per case index: where its payload lives, or `None` for a unit case.
    case_slots: Vec<Option<CaseSlot>>,
    /// Case names in declaration order, for resolving a pattern's case name.
    case_names: Vec<String>,
    /// Payload type per case, for typing a pattern binding.
    case_payloads: Vec<TypeId>,
    /// The case a promoted `ValueKind::Null` denotes: `Option::None`, and only
    /// for an `Option` instance. `wir_build` reads a promoted `Null` the same
    /// way (`translate.rs`, `ValueKind::Null`), so this is the same decision,
    /// made earlier.
    null_case: Option<u32>,
}

impl Layout {
    fn case_index_of(&self, case_name: &str) -> Option<u32> {
        self.case_names
            .iter()
            .position(|n| n == case_name)
            .map(|i| u32::try_from(i).expect("variant case index overflow"))
    }
}

/// A confirmed candidate: the function and the layout its variant return maps
/// to.
#[derive(Clone)]
struct Candidate {
    layout: Layout,
    /// The variant type the function returns, so a tail call can check that its
    /// callee shares the layout.
    variant_type: TypeId,
}

pub fn scalarize_variant_returns(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let candidates = collect_and_validate(project, gate);
    let mut changed = !candidates.is_empty();
    for &key in candidates.keys() {
        crate::compiler_trace!(
            "sroa_variant_return",
            "scalarizing {}",
            project.functions[key.index()].borrow().name
        );
    }
    let mut touched: IndexSet<usize> = IndexSet::default();
    rewrite_callees(project, &candidates, &mut touched);
    // Call sites are rewritten for everything scalarized so far, not just this
    // round's candidates. `nir/inline` runs after this pass and keeps copying
    // bodies, so a callee scalarized earlier acquires fresh call sites that no
    // round would otherwise revisit — and those sites are usually perfectly
    // handleable, just not yet bound. Reboxing them without trying would give
    // up the optimization on the very sites inlining just made hot.
    let mut all = candidates.clone();
    for (key, (variant_type, layout)) in scalarized_returns(project) {
        all.entry(key).or_insert(Candidate {
            layout,
            variant_type,
        });
    }
    rewrite_call_sites(project, &all, &mut touched);
    for idx in touched {
        gate.mark_changed(FuncId::new(idx));
    }
    // Only now: whatever the rewrite above declined is a straggler, and it is
    // one the moment its callee's signature changed — not an iteration later.
    changed |= rebox_stragglers(project, gate);
    debug_assert_call_sites_rewritten(project);
    changed
}

// -----------------------------------------------------------------------
// Repair: reboxing a call site the scalarized form never reached
// -----------------------------------------------------------------------

/// What makes the rewrite unconditionally sound.
///
/// Validation is a snapshot: it sees the call sites that exist when it runs.
/// `nir/inline` runs later in the same iteration and keeps running in later
/// ones, and it copies bodies wholesale — so a callee rewritten in iteration N
/// can acquire a brand-new call site in iteration N+1 that nothing validated.
/// The signature is already committed by then, so leaving that site alone is
/// invalid Wasm, not a missed optimization.
///
/// Rather than try to forbid such sites, any call this pass's fast path did not
/// bind is wrapped back into the variant it used to return:
///
/// ```text
/// f(x)  ⇒  { let __t = f(x); match __t.0 { 0 => Ok(__t.1!), _ => Err(__t.2!) } }
/// ```
///
/// The surrounding context then sees the variant-typed expression it was
/// written against and needs no analysis at all. The rebox costs the allocation
/// the scalarization was meant to avoid, so it is a correctness backstop, not a
/// target — `const_fold` and `sroa` collapse most of them once the tag is
/// known. Validation is left as what it now is: a profitability filter.
fn rebox_stragglers(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let scalarized = scalarized_returns(project);
    if scalarized.is_empty() {
        return false;
    }
    let mut changed = false;
    for i in 0..project.functions.len() {
        let mut func = project.functions[i].borrow_mut();
        let own_return = func.return_type;
        let Some(mut body) = func.body.take() else {
            continue;
        };
        let span = func.span;
        let bound = handled_call_sites(&body, &scalarized);
        let mut targets: Vec<ExprId> = Vec::new();
        collect_straggler_calls(
            &body,
            NodeRef::Block(body.root),
            &scalarized,
            &bound,
            own_return,
            &mut targets,
        );
        for call in targets {
            crate::compiler_trace!("sroa_variant_return", "reboxing a call in {}", func.name);
            rebox_call(&mut body, &mut func, call, &scalarized, span);
            changed = true;
        }
        func.body = Some(body);
        if changed {
            gate.mark_changed(FuncId::new(i));
        }
    }
    changed
}

/// Functions this pass has already rewritten, with the variant they used to
/// return. Derived, not remembered: a tuple return whose shape is exactly some
/// variant's layout is one of ours.
fn scalarized_returns(project: &NirPackage) -> IndexMap<FuncId, (TypeId, Layout)> {
    let mut out: IndexMap<FuncId, (TypeId, Layout)> = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(key) = func.id else { continue };
        let Some(variant) = func.scalarized_from else {
            continue;
        };
        let Some(layout) = compute_layout(project, variant) else {
            continue;
        };
        if layout.tuple_type == func.return_type {
            out.insert(key, (variant, layout));
        }
    }
    out
}

/// Call nodes whose consumer already reads a tuple: a `let` whose declared type
/// is the callee's tuple, or an exit from a labeled block of that type. The
/// second is what `nir/inline` leaves behind — a scalarized callee inlined into
/// a rewritten call site becomes a labeled block typed by the tuple, and its
/// own tail calls now exit through `break L: f(x)`.
fn handled_call_sites(
    body: &Body,
    scalarized: &IndexMap<FuncId, (TypeId, Layout)>,
) -> IndexSet<ExprId> {
    let mut out: IndexSet<ExprId> = IndexSet::default();
    // A label reused at two different result types tells us nothing about which
    // block a `break` exits, so it is dropped rather than guessed at: guessing
    // wrong here skips a rebox that was needed.
    let mut label_types: IndexMap<&str, Option<TypeId>> = IndexMap::default();
    for expr in body.exprs.values() {
        if let ExprKind::LabeledBlock {
            label, result_type, ..
        } = &expr.kind
        {
            label_types
                .entry(label.as_str())
                .and_modify(|t| {
                    if *t != Some(*result_type) {
                        *t = None;
                    }
                })
                .or_insert(Some(*result_type));
        }
    }
    let accept = |value: Operand, expected: TypeId, out: &mut IndexSet<ExprId>| {
        // Through a block tail as well as bare: by the time a later iteration
        // sees this binding, `let_block_flatten` and friends may have wrapped
        // the call in a block, and a bare-only match would call its own earlier
        // work a straggler and rebox it under a tuple-typed binding.
        if let Some((func_id, call)) = tail_call_site(body, value)
            && scalarized
                .get(&func_id)
                .is_some_and(|(_, l)| l.tuple_type == expected)
        {
            out.insert(call);
        }
    };
    for stmt in body.stmts.values() {
        match &stmt.kind {
            StmtKind::Let { type_id, value, .. } => accept(*value, *type_id, &mut out),
            StmtKind::Break {
                label: Some(label),
                value: Some(value),
            } => {
                if let Some(&Some(expected)) = label_types.get(label.as_str()) {
                    accept(*value, expected, &mut out);
                }
            }
            _ => {}
        }
    }
    for expr in body.exprs.values() {
        if let ExprKind::LabeledBlock {
            block, result_type, ..
        } = &expr.kind
            && let Some(&last) = body.blocks[*block].stmts.last()
            && let StmtKind::Expr(value) = body.stmts[last].kind
        {
            accept(value, *result_type, &mut out);
        }
    }
    out
}

fn collect_straggler_calls(
    body: &Body,
    node: NodeRef,
    scalarized: &IndexMap<FuncId, (TypeId, Layout)>,
    handled: &IndexSet<ExprId>,
    own_return: TypeId,
    out: &mut Vec<ExprId>,
) {
    if let NodeRef::Expr(e) = node
        && let ExprKind::Call { func_id, .. } = &body.exprs[e].kind
        && let Some((_, layout)) = scalarized.get(func_id)
        && !handled.contains(&e)
        // A tail call inside a co-scalarized function passes the tuple
        // straight through; that is the tail-call rule, not a straggler.
        && own_return != layout.tuple_type
    {
        out.push(e);
    }
    body.for_each_child(node, |c| {
        collect_straggler_calls(body, c, scalarized, handled, own_return, out);
    });
}

/// Wrap `call` into `{ let __t = call; match __t.0 { k => Case(__t.slot) … } }`,
/// typed by the variant the callee used to return.
fn rebox_call(
    body: &mut Body,
    func: &mut NirFunction,
    call: ExprId,
    scalarized: &IndexMap<FuncId, (TypeId, Layout)>,
    span: Span,
) {
    let ExprKind::Call { func_id, .. } = body.exprs[call].kind else {
        return;
    };
    let (variant_type, layout) = scalarized[&func_id].clone();

    let local_index = func.local_count();
    let name = format!("__rebox_{local_index}");
    func.locals.push(NirLocal {
        name: name.clone(),
        type_id: layout.tuple_type,
        is_mut: false,
    });
    // The call moves to a fresh node so `call`'s id can host the wrapping
    // block, keeping every parent operand valid.
    let moved = body.take_expr(call);
    let call_node = body.exprs.push(moved);
    let let_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Let {
            name: name.clone(),
            local_index,
            is_mut: false,
            is_reactive: false,
            type_id: layout.tuple_type,
            value: Operand::Expr(call_node),
            skip_value_copy: true,
        },
        span,
    });

    let mut arms: Vec<ArmData> = Vec::with_capacity(layout.case_names.len());
    for case_index in 0..layout.case_names.len() {
        let case_index = u32::try_from(case_index).expect("variant case index overflow");
        let payload = layout.case_slots[case_index as usize].map(|slot| {
            let read = tuple_field(
                body,
                local_index,
                &name,
                layout.tuple_type,
                slot.index + 1,
                layout.slot_types[slot.index],
                span,
            );
            if slot.wrap_in_some {
                let payload_type = layout.case_payloads[case_index as usize];
                Operand::Expr(body.exprs.push(ExprNode {
                    kind: ExprKind::VariantPayload {
                        expr: read,
                        case_index: OPTION_SOME,
                        payload_type,
                    },
                    type_id: payload_type,
                    span,
                }))
            } else {
                read
            }
        });
        let construct = body.exprs.push(ExprNode {
            kind: ExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name: layout.case_names[case_index as usize].clone(),
                payload,
            },
            type_id: variant_type,
            span,
        });
        // The last case is the catch-all, so the match stays exhaustive
        // whatever the tag holds.
        let is_last = case_index as usize + 1 == layout.case_names.len();
        let pattern = body.pats.push(PatNode {
            kind: if is_last {
                PatKind::Wildcard
            } else {
                PatKind::Literal(NirLiteralPattern::I128(i128::from(case_index)))
            },
            span,
        });
        arms.push(ArmData {
            pattern,
            guard: None,
            body: Operand::Expr(construct),
            span,
        });
    }

    let tag = tuple_field(
        body,
        local_index,
        &name,
        layout.tuple_type,
        0,
        TypeTable::I32,
        span,
    );
    let match_expr = body.exprs.push(ExprNode {
        kind: ExprKind::Match { expr: tag, arms },
        type_id: variant_type,
        span,
    });
    let tail = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(Operand::Expr(match_expr)),
        span,
    });
    let block = body.blocks.push(BlockNode {
        stmts: vec![let_stmt, tail],
        span,
    });
    body.exprs[call].kind = ExprKind::Block(block);
    body.exprs[call].type_id = variant_type;
}

/// `t.<field>` on a named local.
fn tuple_field(
    body: &mut Body,
    local_index: u32,
    local_name: &str,
    tuple_type: TypeId,
    field: usize,
    field_type: TypeId,
    span: Span,
) -> Operand {
    let recv = body.exprs.push(ExprNode {
        kind: ExprKind::Local {
            index: local_index,
            name: local_name.to_string(),
        },
        type_id: tuple_type,
        span,
    });
    Operand::Expr(body.exprs.push(ExprNode {
        kind: ExprKind::FieldAccess {
            expr: Operand::Expr(recv),
            field_index: u32::try_from(field).expect("slot index overflow"),
            field_name: field.to_string(),
        },
        type_id: field_type,
        span,
    }))
}

/// Every binding fed by a rewritten callee must now be typed by the tuple. A
/// survivor is a call site validation admitted but the rewrite did not reach,
/// and it reaches codegen as invalid Wasm rather than as a missed optimization
/// — so it is caught here, at the pass that caused it.
#[cfg(debug_assertions)]
fn debug_assert_call_sites_rewritten(project: &NirPackage) {
    let scalarized = scalarized_returns(project);
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(body) = &func.body else { continue };
        // Reachable statements only. The arena is never compacted, so a rewrite
        // that replaces a node leaves the old one behind; checking those would
        // report a violation in code that no longer runs.
        let mut live = Vec::new();
        collect_stmts(body, NodeRef::Block(body.root), &mut live);
        for stmt_id in live {
            let StmtKind::Let {
                name,
                type_id,
                value,
                ..
            } = &body.stmts[stmt_id].kind
            else {
                continue;
            };
            let Some(callee) = tail_call_callee(body, *value) else {
                continue;
            };
            let Some((_, layout)) = scalarized.get(&callee) else {
                continue;
            };
            assert!(
                *type_id == layout.tuple_type,
                "variant-return SROA left `let {name}` in `{}` typed by the variant \
                 its callee no longer returns",
                func.name
            );
        }
    }
}

#[cfg(not(debug_assertions))]
fn debug_assert_call_sites_rewritten(_: &NirPackage) {}

/// The callee of a `let` initializer and the call node itself, looking through a
/// block whose tail is the call — the shape `nir/inline` leaves when it expands
/// a one-call wrapper, and `let_block_flatten` when it hoists a receiver.
fn tail_call_site(body: &Body, op: Operand) -> Option<(FuncId, ExprId)> {
    let e = op.as_expr()?;
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, .. } => Some((*func_id, e)),
        ExprKind::Block(b) => {
            let last = *body.blocks[*b].stmts.last()?;
            match &body.stmts[last].kind {
                StmtKind::Expr(inner) => tail_call_site(body, *inner),
                _ => None,
            }
        }
        _ => None,
    }
}

fn tail_call_callee(body: &Body, op: Operand) -> Option<FuncId> {
    tail_call_site(body, op).map(|(f, _)| f)
}

// -----------------------------------------------------------------------
// Phase 1: layout
// -----------------------------------------------------------------------

/// The variant declaration behind a return type, with its case payloads
/// substituted against the instance's type arguments.
fn variant_cases(project: &NirPackage, type_id: TypeId) -> Option<(Vec<String>, Vec<TypeId>)> {
    let (name, module_source, type_args) = {
        let type_table = project.type_table.borrow();
        match type_table.get(type_id) {
            ResolvedType::Variant {
                name,
                module_source,
            } => (name.clone(), module_source.clone(), Vec::new()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (name.clone(), module_source.clone(), type_args.clone()),
            _ => return None,
        }
    };
    let decl = project.find_variant(&module_source, &name)?;
    let names: Vec<String> = decl.cases.iter().map(|c| c.name.clone()).collect();
    let templates: Vec<TypeId> = decl.cases.iter().map(|c| c.payload).collect();
    let substitution: IndexMap<u32, TypeId> = type_args
        .iter()
        .enumerate()
        .map(|(i, arg)| (u32::try_from(i).expect("too many type params"), *arg))
        .collect();
    let mut type_table = project.type_table.borrow_mut();
    let payloads = templates
        .into_iter()
        .map(|t| type_table.substitute_type_params(t, &substitution))
        .collect();
    Some((names, payloads))
}

/// How a payload type reaches its slot.
enum SlotShape {
    /// No slot at all (`Unit` payload).
    Absent,
    /// The payload type is its own slot type, with `pad`.
    Direct(Pad),
    /// The slot is `Option<T>`; the live value needs `Some(_)`.
    Wrapped,
}

/// Classify a payload type against the slot rule in the module docs.
///
/// `None` means the payload has no result-vector representation: a
/// function-typed payload is an abstract `structref` (which the WIR layout
/// rejects for the same reason), `v128` has no NIR literal to pad with, and
/// `i128` / `u128` never reach WIR as primitives at all.
fn slot_shape(payload: TypeId, type_table: &TypeTable) -> Option<SlotShape> {
    match type_table.get(payload) {
        ResolvedType::Unit => Some(SlotShape::Absent),
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64 => Some(SlotShape::Direct(Pad::Int(payload))),
            PrimitiveType::F32 | PrimitiveType::F64 => Some(SlotShape::Direct(Pad::Float(payload))),
            PrimitiveType::Bool => Some(SlotShape::Direct(Pad::Bool)),
            PrimitiveType::Char => Some(SlotShape::Direct(Pad::Char)),
            PrimitiveType::V128 | PrimitiveType::I128 | PrimitiveType::U128 => None,
        },
        ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. } => Some(SlotShape::Direct(Pad::Int(payload))),
        ResolvedType::Struct { .. }
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::Variant { .. } => Some(SlotShape::Wrapped),
        ResolvedType::GenericInstance { .. } => {
            // `Option<X>` for a GC-ref `X` is already `ref null X`; wrapping it
            // again yields a variant whose payload is nullable, which
            // `nullable_ref` refuses to erase — so it would cost a real box.
            match type_table.as_option(payload) {
                Some(inner) if is_gc_ref(inner, type_table) => {
                    Some(SlotShape::Direct(Pad::NoneOf(payload)))
                }
                _ => Some(SlotShape::Wrapped),
            }
        }
        ResolvedType::Newtype { base_type, .. } => slot_shape(*base_type, type_table),
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => slot_shape(*inner, type_table),
        ResolvedType::Function { .. }
        | ResolvedType::Never
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::Reactive(_)
        | ResolvedType::Unknown
        | ResolvedType::Error => None,
    }
}

/// Whether a type is a non-nullable GC reference at Wasm level — the payload
/// shape `wir_optimize::nullable_ref` collapses an `Option` around.
fn is_gc_ref(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::BuiltinArray(_) => true,
        ResolvedType::Newtype { base_type, .. } => is_gc_ref(*base_type, type_table),
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => is_gc_ref(*inner, type_table),
        ResolvedType::Primitive(_)
        | ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::Function { .. }
        | ResolvedType::Unit
        | ResolvedType::Never
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. }
        | ResolvedType::AssocTypeProjection { .. }
        | ResolvedType::Reactive(_)
        | ResolvedType::Unknown
        | ResolvedType::Error => false,
    }
}

/// Compute the tuple layout for a variant return type, or `None` when the
/// variant does not fit the small-tuple model.
fn compute_layout(project: &NirPackage, variant_type: TypeId) -> Option<Layout> {
    let (case_names, case_payloads) = variant_cases(project, variant_type)?;
    if case_names.len() < 2 {
        return None;
    }
    // A 2-case `{Unit, GC ref}` variant is what `nullable_ref` erases to a bare
    // nullable ref. That already beats any tuple, so leave it alone.
    if is_nullable_ref_shape(&case_payloads, project) {
        return None;
    }

    let shapes = {
        let type_table = project.type_table.borrow();
        case_payloads
            .iter()
            .map(|&p| slot_shape(p, &type_table))
            .collect::<Option<Vec<_>>>()?
    };

    let mut slot_types: Vec<TypeId> = Vec::new();
    let mut slot_pads: Vec<Pad> = Vec::new();
    let mut case_slots: Vec<Option<CaseSlot>> = Vec::with_capacity(shapes.len());
    for (payload, shape) in case_payloads.iter().zip(&shapes) {
        let (slot_type, pad, wrap_in_some) = match shape {
            SlotShape::Absent => {
                case_slots.push(None);
                continue;
            }
            SlotShape::Direct(pad) => (*payload, *pad, false),
            SlotShape::Wrapped => {
                // `wir_build` registers an `Option<T>` instance off the
                // declaration; without it the minted slot type has no WIR
                // registration and codegen panics.
                project.find_variant(&option_module(project)?, "Option")?;
                let option_type = project.type_table.borrow_mut().make_option(*payload);
                (option_type, Pad::NoneOf(option_type), true)
            }
        };
        // Shared layout: a slot already carrying this exact type is reused, so
        // `Option<char>` stays `[i32, char]` instead of growing a second slot.
        let index = if let Some(i) = slot_types.iter().position(|t| *t == slot_type) {
            i
        } else {
            slot_types.push(slot_type);
            slot_pads.push(pad);
            slot_types.len() - 1
        };
        case_slots.push(Some(CaseSlot {
            index,
            wrap_in_some,
        }));
    }

    // A variant whose every case is a unit case is an `enum` in all but name;
    // the tag alone is not a tuple.
    if slot_types.is_empty() || 1 + slot_types.len() > MAX_TUPLE_ARITY {
        return None;
    }

    let tuple_type = {
        let mut elements = Vec::with_capacity(1 + slot_types.len());
        elements.push(TypeTable::I32);
        elements.extend(slot_types.iter().copied());
        project.type_table.borrow_mut().make_tuple(elements)
    };

    let null_case = project
        .type_table
        .borrow()
        .as_option(variant_type)
        .map(|_| OPTION_NONE);

    Some(Layout {
        tuple_type,
        slot_types,
        slot_pads,
        case_slots,
        case_names,
        case_payloads,
        null_case,
    })
}

/// Where `Option` is declared, per `#[compiler_item("option")]`.
fn option_module(project: &NirPackage) -> Option<crate::module_source::ModuleSource> {
    project
        .type_table
        .borrow()
        .compiler_items()
        .variant_module(crate::compiler_item::CompilerItem::Option)
        .cloned()
}

/// The `{Unit, Payload(GC ref)}` shape `wir_optimize::nullable_ref` erases.
fn is_nullable_ref_shape(payloads: &[TypeId], project: &NirPackage) -> bool {
    if payloads.len() != 2 {
        return false;
    }
    let type_table = project.type_table.borrow();
    let units = payloads
        .iter()
        .filter(|&&p| matches!(type_table.get(p), ResolvedType::Unit))
        .count();
    let refs = payloads
        .iter()
        .filter(|&&p| is_gc_ref(p, &type_table) && type_table.as_option(p).is_none())
        .count();
    units == 1 && refs == 1
}

// -----------------------------------------------------------------------
// Phase 2: candidates and validation
// -----------------------------------------------------------------------

fn is_eligible(func: &NirFunction) -> bool {
    if func.is_dead || func.body.is_none() {
        return false;
    }
    if func.is_export || func.is_cm_export || func.is_cm_binding || func.is_async {
        return false;
    }
    if func.is_dispatch_wrapper || func.is_closure_call() {
        return false;
    }
    if func.has_real_type_params() || !func.impl_type_params.is_empty() {
        return false;
    }
    // A trait method is *not* excluded: after monomorphization it is an
    // ordinary direct-call target, and it is where the traffic is —
    // `Iterator::next` / `Deserializer::*` shapes are ~80% of the functions
    // `wir_optimize`'s variant-return SROA widens on the parser benchmarks.
    match func.kind {
        FunctionKind::Regular => true,
        // `ArrayClone` resolves a value-copy helper by metadata at emit time,
        // and `wir_build` supplies the dispatch stub's body; neither reaches
        // its callee through a NIR call node a signature change would follow.
        FunctionKind::ValueCopy { .. } | FunctionKind::FnCanonicalDispatch { .. } => false,
    }
}

fn collect_and_validate(
    project: &NirPackage,
    gate: &mut FunctionGate,
) -> IndexMap<FuncId, Candidate> {
    let mut candidates: IndexMap<FuncId, Candidate> = IndexMap::default();
    let mut layout_cache: IndexMap<TypeId, Option<Layout>> = IndexMap::default();

    for fid in gate.dirty_funcs(GatedPass::SroaVariantReturn, project.functions.len()) {
        let func = project.functions[fid.index()].borrow();
        if !is_eligible(&func) {
            continue;
        }
        let Some(key) = func.id else { continue };
        let variant_type = func.return_type;
        let layout = layout_cache
            .entry(variant_type)
            .or_insert_with(|| compute_layout(project, variant_type))
            .clone();
        let Some(layout) = layout else { continue };
        candidates.insert(
            key,
            Candidate {
                layout,
                variant_type,
            },
        );
    }

    // The tail-call rule couples caller and callee candidacy in both
    // directions, so refute to a fix-point: optimistic (assume-then-refute), so
    // a mutually tail-recursive group survives.
    // A tail call is fine when its callee already returns this variant's tuple,
    // whether it was scalarized in this round or an earlier one. Refusing the
    // earlier ones reboxed every tail call inlining exposed after its callee
    // was done — 50 of them on `cbor_twitter`, each an allocation the pass
    // exists to remove.
    let already: IndexMap<FuncId, TypeId> = project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            Some((f.id?, f.scalarized_from?))
        })
        .collect();

    while !candidates.is_empty() {
        let mut tail_ok = already.clone();
        for (&key, cand) in &candidates {
            tail_ok.insert(key, cand.variant_type);
        }
        let mut invalid: IndexSet<FuncId> = IndexSet::default();
        for (&key, cand) in &candidates {
            let func = project.functions[key.index()].borrow();
            let body = func
                .body
                .as_ref()
                .expect("is_eligible rejects a body-less function");
            if !returns_are_scalarizable(body, body.root, cand, &tail_ok) {
                invalid.insert(key);
            }
        }
        for func_rc in &project.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                let caller_ok = func
                    .id
                    .is_some_and(|id| candidates.contains_key(&id) || already.contains_key(&id));
                invalidate_bad_call_sites(body, &func, &candidates, caller_ok, &mut invalid);
            }
        }
        if invalid.is_empty() {
            break;
        }
        for k in &invalid {
            candidates.swap_remove(k);
        }
    }

    // A candidate reached from a global initializer has no `let`-bound temp to
    // destructure there, and the initializer bodies carry no locals list to
    // mint one; drop any candidate a global mentions at all.
    let mut used_in_globals: IndexSet<FuncId> = IndexSet::default();
    for global in &project.globals {
        let body = global.init.slot_expr().body();
        collect_called(body, NodeRef::Block(body.root), &mut used_in_globals);
    }
    candidates.retain(|k, _| !used_in_globals.contains(k));

    candidates
}

fn collect_called(body: &Body, node: NodeRef, out: &mut IndexSet<FuncId>) {
    if let NodeRef::Expr(e) = node
        && let ExprKind::Call { func_id, .. } = &body.exprs[e].kind
    {
        out.insert(*func_id);
    }
    body.for_each_child(node, |c| collect_called(body, c, out));
}

/// Whether every `Return` in `block` produces a shape the rewrite can turn into
/// the result tuple.
fn returns_are_scalarizable(
    body: &Body,
    block: BlockId,
    cand: &Candidate,
    tail_ok: &IndexMap<FuncId, TypeId>,
) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|&s| stmt_returns_scalarizable(body, s, cand, tail_ok))
}

fn stmt_returns_scalarizable(
    body: &Body,
    stmt: StmtId,
    cand: &Candidate,
    tail_ok: &IndexMap<FuncId, TypeId>,
) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Return { value: None } => false,
        StmtKind::Return { value: Some(v) } => return_value_scalarizable(body, *v, cand, tail_ok),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            returns_are_scalarizable(body, *then_block, cand, tail_ok)
                && else_block.is_none_or(|b| returns_are_scalarizable(body, b, cand, tail_ok))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            returns_are_scalarizable(body, *b, cand, tail_ok)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            nested_returns_scalarizable(body, *value, cand, tail_ok)
        }
        StmtKind::Expr(e) => nested_returns_scalarizable(body, *e, cand, tail_ok),
        StmtKind::Break { value, .. } => {
            value.is_none_or(|v| nested_returns_scalarizable(body, v, cand, tail_ok))
        }
        StmtKind::Continue => true,
    }
}

/// Every `Return` nested anywhere in an expression — a `?`-desugared
/// `return Err(…)` inside a `let` initializer or an `if` condition included.
fn nested_returns_scalarizable(
    body: &Body,
    op: Operand,
    cand: &Candidate,
    tail_ok: &IndexMap<FuncId, TypeId>,
) -> bool {
    let Some(expr) = op.as_expr() else {
        return true;
    };
    let mut stmts = Vec::new();
    collect_stmts(body, NodeRef::Expr(expr), &mut stmts);
    stmts
        .iter()
        .all(|&s| stmt_returns_scalarizable(body, s, cand, tail_ok))
}

fn collect_stmts(body: &Body, node: NodeRef, out: &mut Vec<StmtId>) {
    if let NodeRef::Stmt(s) = node {
        out.push(s);
    }
    body.for_each_child(node, |c| collect_stmts(body, c, out));
}

/// The value of a `return`, in tail position.
fn return_value_scalarizable(
    body: &Body,
    op: Operand,
    cand: &Candidate,
    tail_ok: &IndexMap<FuncId, TypeId>,
) -> bool {
    let Some(expr) = op.as_expr() else {
        return null_return_case(body, op, cand).is_some();
    };
    if body.exprs[expr].type_id == TypeTable::NEVER {
        return true;
    }
    match &body.exprs[expr].kind {
        ExprKind::VariantConstruct { variant_type, .. } => *variant_type == cand.variant_type,
        // `return g(x)` where `g` shares the variant, hence the layout: `g`
        // already returns this function's tuple, whichever round rewrote it.
        ExprKind::Call { func_id, .. } => tail_ok.get(func_id) == Some(&cand.variant_type),
        ExprKind::Local { index, .. } => single_def_variant_construct(body, *index, cand).is_some(),
        // A plain block's value is its tail statement. A `LabeledBlock`'s is
        // not: a `break L: v` anywhere inside also produces it, and those exits
        // are neither validated nor rewritten here — so the whole shape is
        // rejected rather than half-handled.
        ExprKind::Block(b) => {
            let b = *b;
            returns_are_scalarizable(body, b, cand, tail_ok)
                && block_tail_scalarizable(body, b, cand, tail_ok)
        }
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            let (then_branch, else_branch) = (*then_branch, *else_branch);
            returns_are_scalarizable(body, then_branch, cand, tail_ok)
                && block_tail_scalarizable(body, then_branch, cand, tail_ok)
                && else_branch.is_some_and(|b| {
                    returns_are_scalarizable(body, b, cand, tail_ok)
                        && block_tail_scalarizable(body, b, cand, tail_ok)
                })
        }
        ExprKind::Match { arms, .. } => {
            let bodies: Vec<Operand> = arms.iter().map(|a| a.body).collect();
            bodies
                .iter()
                .all(|&b| return_value_scalarizable(body, b, cand, tail_ok))
        }
        ExprKind::Switch { arms, default, .. } => {
            let blocks: Vec<BlockId> = arms
                .iter()
                .copied()
                .chain(std::iter::once(*default))
                .collect();
            blocks.iter().all(|&b| {
                returns_are_scalarizable(body, b, cand, tail_ok)
                    && block_tail_scalarizable(body, b, cand, tail_ok)
            })
        }
        _ => false,
    }
}

/// The case index a `return null` in this candidate produces. `null` is
/// Wado's `Option::None` literal, and it reaches NIR as a promoted pure value
/// with no `ExprId` — the return type is what pins which variant it means.
fn null_return_case(body: &Body, op: Operand, cand: &Candidate) -> Option<u32> {
    let value = op.as_value()?;
    if !matches!(body.values.kind(value), ValueKind::Null) {
        return None;
    }
    cand.layout.null_case
}

fn block_tail_scalarizable(
    body: &Body,
    block: BlockId,
    cand: &Candidate,
    tail_ok: &IndexMap<FuncId, TypeId>,
) -> bool {
    let Some(&last) = body.blocks[block].stmts.last() else {
        return false;
    };
    match &body.stmts[last].kind {
        StmtKind::Expr(e) => return_value_scalarizable(body, *e, cand, tail_ok),
        StmtKind::Return { .. } => true,
        _ => false,
    }
}

/// A `let t = VariantConstruct(…)` whose only mention is the `return t` asking
/// about it — the shape `field_scalarize` leaves when a construction has to
/// happen before a later side effect. The rewrite retypes the binding in place,
/// so nothing moves and no trap or ordering analysis is needed.
fn single_def_variant_construct(body: &Body, local: u32, cand: &Candidate) -> Option<StmtId> {
    let mut defs: Vec<StmtId> = Vec::new();
    let mut reads = 0usize;
    count_local_def_use(
        body,
        NodeRef::Block(body.root),
        local,
        &mut defs,
        &mut reads,
    );
    if defs.len() != 1 || reads != 1 {
        return None;
    }
    let def = defs[0];
    let StmtKind::Let { value, is_mut, .. } = &body.stmts[def].kind else {
        return None;
    };
    if *is_mut {
        return None;
    }
    let init = value.as_expr()?;
    match &body.exprs[init].kind {
        ExprKind::VariantConstruct { variant_type, .. } if *variant_type == cand.variant_type => {
            Some(def)
        }
        _ => None,
    }
}

fn count_local_def_use(
    body: &Body,
    node: NodeRef,
    local: u32,
    defs: &mut Vec<StmtId>,
    reads: &mut usize,
) {
    match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
                && *local_index == local
            {
                defs.push(s);
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::Local { index, .. } = &body.exprs[e].kind
                && *index == local
            {
                *reads += 1;
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    body.for_each_child(node, |c| {
        count_local_def_use(body, c, local, defs, reads);
    });
}

// -----------------------------------------------------------------------
// Phase 2b: call-site validation
// -----------------------------------------------------------------------

/// Locals bound directly from a candidate call, and the callee they came from.
fn bound_temps(body: &Body, candidates: &IndexMap<FuncId, Candidate>) -> IndexMap<u32, FuncId> {
    let mut out: IndexMap<u32, FuncId> = IndexMap::default();
    collect_bound_temps(body, NodeRef::Block(body.root), candidates, &mut out);
    out
}

fn collect_bound_temps(
    body: &Body,
    node: NodeRef,
    candidates: &IndexMap<FuncId, Candidate>,
    out: &mut IndexMap<u32, FuncId>,
) {
    if let NodeRef::Stmt(s) = node
        && let StmtKind::Let {
            local_index,
            value,
            is_mut,
            ..
        } = &body.stmts[s].kind
        && !*is_mut
        // Through a block tail as well as bare, so this agrees with
        // `handled_call_sites` and `debug_assert_call_sites_rewritten` on what
        // counts as a binding fed by a call. A narrower rule here leaves the
        // binding un-retyped and the two wider ones then disagree with it.
        && let Some((f, _)) = tail_call_site(body, *value)
        && candidates.contains_key(&f)
    {
        out.insert(*local_index, f);
    }
    body.for_each_child(node, |c| collect_bound_temps(body, c, candidates, out));
}

/// Invalidate any candidate whose result is consumed by something other than an
/// immediate destructure.
fn invalidate_bad_call_sites(
    body: &Body,
    func: &NirFunction,
    candidates: &IndexMap<FuncId, Candidate>,
    caller_ok: bool,
    invalid: &mut IndexSet<FuncId>,
) {
    let bound = bound_temps(body, candidates);
    // A bound temp whose address is taken, or which a `stores` parameter
    // aliases, is not a pure destructure target.
    for (&local, &f) in &bound {
        if func.address_taken_locals.contains(&local) || func.stores_aliased_locals.contains(&local)
        {
            invalid.insert(f);
        }
    }
    check_uses(
        body,
        NodeRef::Block(body.root),
        candidates,
        &bound,
        &cell_locals(func),
        caller_ok,
        invalid,
    );
}

fn check_uses(
    body: &Body,
    node: NodeRef,
    candidates: &IndexMap<FuncId, Candidate>,
    bound: &IndexMap<u32, FuncId>,
    cells: &IndexSet<u32>,
    caller_ok: bool,
    invalid: &mut IndexSet<FuncId>,
) {
    match node {
        NodeRef::Stmt(s) => {
            // The two accepted call positions: a `let` initializer that binds
            // the result, and a tail `return` inside a co-candidate. Both are
            // handled by the rewrite, so the call node itself is not an escape.
            match &body.stmts[s].kind {
                StmtKind::Let { value, is_mut, .. } if !*is_mut => {
                    if call_callee(body, *value).is_some_and(|f| candidates.contains_key(&f)) {
                        return;
                    }
                }
                // A tail call passes the callee's tuple straight through — but
                // only if this function has a tuple to pass it through. When it
                // does not, the site needs a rebox, and a rebox is the
                // allocation the pass exists to remove: refuse the callee
                // instead. The fix-point loop propagates the refusal.
                StmtKind::Return { value: Some(v) } => {
                    if let Some(f) = call_callee(body, *v)
                        && candidates.contains_key(&f)
                    {
                        if !caller_ok {
                            invalid.insert(f);
                        }
                        return;
                    }
                }
                _ => {}
            }
        }
        NodeRef::Expr(e) => {
            match &body.exprs[e].kind {
                // A destructuring read of a bound temp; its `Local` child must
                // not then be counted as a bare escape.
                ExprKind::VariantTag { expr }
                | ExprKind::VariantTest { expr, .. }
                | ExprKind::VariantPayload { expr, .. } => {
                    if bound_callee(body, *expr, bound).is_some() {
                        return;
                    }
                }
                ExprKind::Match { expr: scrut, arms } => {
                    let via_temp = bound_callee(body, *scrut, bound);
                    let via_call = call_callee(body, *scrut).filter(|f| candidates.contains_key(f));
                    if via_temp.is_some() || via_call.is_some() {
                        let arms = arms.clone();
                        if !arms.iter().all(|a| arm_is_one_level(body, a, cells)) {
                            invalid.extend(via_temp.into_iter().chain(via_call));
                        }
                        for a in &arms {
                            if let Some(g) = a.guard {
                                check_uses_operand(
                                    body, g, candidates, bound, cells, caller_ok, invalid,
                                );
                            }
                            check_uses_operand(
                                body, a.body, candidates, bound, cells, caller_ok, invalid,
                            );
                        }
                        return;
                    }
                }
                // A bare mention of the temp escapes the destructure.
                ExprKind::Local { index, .. } => {
                    if let Some(f) = bound.get(index) {
                        invalid.insert(*f);
                    }
                    return;
                }
                // A candidate call anywhere else has no place to bind results.
                ExprKind::Call { func_id, .. } => {
                    if candidates.contains_key(func_id) {
                        invalid.insert(*func_id);
                    }
                }
                _ => {}
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    body.for_each_child(node, |c| {
        check_uses(body, c, candidates, bound, cells, caller_ok, invalid);
    });
}

fn check_uses_operand(
    body: &Body,
    op: Operand,
    candidates: &IndexMap<FuncId, Candidate>,
    bound: &IndexMap<u32, FuncId>,
    cells: &IndexSet<u32>,
    caller_ok: bool,
    invalid: &mut IndexSet<FuncId>,
) {
    if let Some(e) = op.as_expr() {
        check_uses(
            body,
            NodeRef::Expr(e),
            candidates,
            bound,
            cells,
            caller_ok,
            invalid,
        );
    }
}

fn bound_callee(body: &Body, op: Operand, bound: &IndexMap<u32, FuncId>) -> Option<FuncId> {
    let e = op.as_expr()?;
    let ExprKind::Local { index, .. } = &body.exprs[e].kind else {
        return None;
    };
    bound.get(index).copied()
}

fn call_callee(body: &Body, op: Operand) -> Option<FuncId> {
    let e = op.as_expr()?;
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, .. } => Some(*func_id),
        _ => None,
    }
}

/// Locals a payload binding cannot be re-minted for. `bind_payload_before`
/// writes a plain `let`, but a local whose address is taken — or which a
/// `stores` parameter aliases — is lowered as a reference cell, and storing the
/// bare slot read into it hands a scalar to a struct-typed local.
fn cell_locals(func: &NirFunction) -> IndexSet<u32> {
    let mut out = func.address_taken_locals.clone();
    out.extend(func.stores_aliased_locals.iter().copied());
    out
}

/// One level deep over the scrutinee's own variant, binding at most one name
/// the rewrite can re-mint. A nested pattern (`Ok(Some(x))`) is rejected rather
/// than peeled — peeling is where the WIR pass's complexity lives.
fn arm_is_one_level(body: &Body, arm: &ArmData, cells: &IndexSet<u32>) -> bool {
    match &body.pats[arm.pattern].kind {
        PatKind::Wildcard => true,
        PatKind::Variant { bindings, .. } => match bindings.as_slice() {
            [] => true,
            [b] => match body.pats[*b].kind {
                PatKind::Binding { local_index, .. } => !cells.contains(&local_index),
                PatKind::Wildcard => true,
                _ => false,
            },
            _ => false,
        },
        PatKind::Binding { .. }
        | PatKind::Literal(_)
        | PatKind::Tuple(_, _)
        | PatKind::Enum { .. }
        | PatKind::Struct { .. }
        | PatKind::Or(_)
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => false,
    }
}

// -----------------------------------------------------------------------
// Phase 3a: callee rewrite
// -----------------------------------------------------------------------

fn rewrite_callees(
    project: &mut NirPackage,
    candidates: &IndexMap<FuncId, Candidate>,
    touched: &mut IndexSet<usize>,
) {
    for (&key, cand) in candidates {
        let mut func = project.functions[key.index()].borrow_mut();
        func.scalarized_from = Some(cand.variant_type);
        func.return_type = cand.layout.tuple_type;
        let span = func.span;
        let Some(mut body) = func.body.take() else {
            touched.insert(key.index());
            continue;
        };
        let root = body.root;
        let mut retyped: Vec<u32> = Vec::new();
        rewrite_returns(&mut body, NodeRef::Block(root), cand, span, &mut retyped);
        // A `let t = Ok(v); … return t` had its initializer rewritten in place,
        // so the binding now holds the tuple: its declared type has to follow,
        // or WIR declares the local by the variant it no longer carries.
        for local in retyped {
            func.locals[local as usize].type_id = cand.layout.tuple_type;
            retype_let(&mut body, local, cand.layout.tuple_type);
        }
        func.body = Some(body);
        touched.insert(key.index());
    }
}

/// Rewrite every `Return` value leaf into the result tuple, retyping the merge
/// points it passes through on the way.
fn rewrite_returns(
    body: &mut Body,
    node: NodeRef,
    cand: &Candidate,
    span: Span,
    retyped: &mut Vec<u32>,
) {
    if let NodeRef::Stmt(s) = node
        && let StmtKind::Return { value: Some(v) } = body.stmts[s].kind
        && let Some(new) = rewrite_return_value(body, v, cand, span, retyped)
    {
        body.stmts[s].kind = StmtKind::Return { value: Some(new) };
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        rewrite_returns(body, c, cand, span, retyped);
    }
}

/// Rewrites in place where the value is a skeleton node, and returns a
/// replacement operand where it is not — a promoted `null`, which has no
/// `ExprId` to overwrite.
#[must_use]
fn rewrite_return_value(
    body: &mut Body,
    op: Operand,
    cand: &Candidate,
    span: Span,
    retyped: &mut Vec<u32>,
) -> Option<Operand> {
    let Some(expr) = op.as_expr() else {
        let case = null_return_case(body, op, cand)?;
        let tuple = build_result_tuple(body, cand, case, None, span);
        return Some(Operand::Expr(body.exprs.push(ExprNode {
            kind: tuple,
            type_id: cand.layout.tuple_type,
            span,
        })));
    };
    if body.exprs[expr].type_id == TypeTable::NEVER {
        return None;
    }
    match body.exprs[expr].kind.clone() {
        ExprKind::VariantConstruct {
            case_index,
            payload,
            ..
        } => {
            let tuple = build_result_tuple(body, cand, case_index, payload, span);
            body.exprs[expr].kind = tuple;
            body.exprs[expr].type_id = cand.layout.tuple_type;
        }
        // A tail call to a co-candidate already returns this tuple.
        ExprKind::Call { .. } => {
            body.exprs[expr].type_id = cand.layout.tuple_type;
        }
        ExprKind::Local { index, .. } => {
            if let Some(def) = single_def_variant_construct(body, index, cand) {
                rewrite_let_construct(body, def, cand, span);
                body.exprs[expr].type_id = cand.layout.tuple_type;
                retyped.push(index);
            }
        }
        ExprKind::Block(b) => {
            body.exprs[expr].type_id = cand.layout.tuple_type;
            rewrite_block_tail(body, b, cand, span, retyped);
        }
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            body.exprs[expr].type_id = cand.layout.tuple_type;
            rewrite_block_tail(body, then_branch, cand, span, retyped);
            if let Some(eb) = else_branch {
                rewrite_block_tail(body, eb, cand, span, retyped);
            }
        }
        ExprKind::Match { arms, .. } => {
            body.exprs[expr].type_id = cand.layout.tuple_type;
            let replacements: Vec<(usize, Operand)> = arms
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    rewrite_return_value(body, a.body, cand, span, retyped).map(|new| (i, new))
                })
                .collect();
            if let ExprKind::Match { arms, .. } = &mut body.exprs[expr].kind {
                for (i, new) in replacements {
                    arms[i].body = new;
                }
            }
        }
        ExprKind::Switch { arms, default, .. } => {
            body.exprs[expr].type_id = cand.layout.tuple_type;
            for b in arms.into_iter().chain(std::iter::once(default)) {
                rewrite_block_tail(body, b, cand, span, retyped);
            }
        }
        _ => {}
    }
    None
}

fn rewrite_block_tail(
    body: &mut Body,
    block: BlockId,
    cand: &Candidate,
    span: Span,
    retyped: &mut Vec<u32>,
) {
    let Some(&last) = body.blocks[block].stmts.last() else {
        return;
    };
    if let StmtKind::Expr(e) = body.stmts[last].kind
        && let Some(new) = rewrite_return_value(body, e, cand, span, retyped)
    {
        body.stmts[last].kind = StmtKind::Expr(new);
    }
}

/// Turn `let t = Ok(v)` into `let t = [0, v, None]`.
fn rewrite_let_construct(body: &mut Body, def: StmtId, cand: &Candidate, span: Span) {
    let StmtKind::Let { value, .. } = body.stmts[def].kind else {
        return;
    };
    let Some(init) = value.as_expr() else { return };
    let ExprKind::VariantConstruct {
        case_index,
        payload,
        ..
    } = body.exprs[init].kind.clone()
    else {
        return;
    };
    let tuple = build_result_tuple(body, cand, case_index, payload, span);
    body.exprs[init].kind = tuple;
    body.exprs[init].type_id = cand.layout.tuple_type;
}

/// `[tag, slot_0, …]` for one case: the live payload in its slot, pads
/// elsewhere.
fn build_result_tuple(
    body: &mut Body,
    cand: &Candidate,
    case_index: u32,
    payload: Option<Operand>,
    span: Span,
) -> ExprKind {
    let layout = &cand.layout;
    let tag = constant(
        body,
        ValueKind::Int(u64::from(case_index), TypeTable::I32),
        TypeTable::I32,
    );
    let mut elements: Vec<Operand> = Vec::with_capacity(1 + layout.slot_types.len());
    elements.push(tag);

    let slot = layout
        .case_slots
        .get(case_index as usize)
        .copied()
        .flatten();
    for i in 0..layout.slot_types.len() {
        let live = slot.filter(|s| s.index == i).and_then(|s| {
            payload.map(|p| {
                if s.wrap_in_some {
                    wrap_in_some(body, p, layout.slot_types[i], span)
                } else {
                    p
                }
            })
        });
        elements.push(live.unwrap_or_else(|| pad_value(body, layout.slot_pads[i], span)));
    }
    ExprKind::TupleLiteral { elements }
}

/// `Some(v)` for a slot whose type is `Option<payload>`.
fn wrap_in_some(body: &mut Body, payload: Operand, slot_type: TypeId, span: Span) -> Operand {
    Operand::Expr(body.exprs.push(ExprNode {
        kind: ExprKind::VariantConstruct {
            variant_type: slot_type,
            case_index: OPTION_SOME,
            case_name: "Some".to_string(),
            payload: Some(payload),
        },
        type_id: slot_type,
        span,
    }))
}

/// A constant operand carrying its use-site type. `ValuePool::intern` is
/// type-erased and records no type, which the WIR extractor requires; every
/// constant this pass mints therefore goes through `alloc_unshared`.
fn constant(body: &mut Body, kind: ValueKind, type_id: TypeId) -> Operand {
    Operand::Value(body.values.alloc_unshared(kind, type_id))
}

/// The value that fills a slot the live case does not use. Never read — every
/// reader tests the tag first.
fn pad_value(body: &mut Body, pad: Pad, span: Span) -> Operand {
    match pad {
        Pad::Int(ty) => constant(body, ValueKind::Int(0, ty), ty),
        Pad::Float(ty) => constant(body, ValueKind::Float(0.0f64.to_bits(), ty), ty),
        Pad::Bool => constant(body, ValueKind::Bool(false), TypeTable::BOOL),
        Pad::Char => constant(body, ValueKind::Char('\0'), TypeTable::CHAR),
        Pad::NoneOf(option_type) => Operand::Expr(body.exprs.push(ExprNode {
            kind: ExprKind::VariantConstruct {
                variant_type: option_type,
                case_index: OPTION_NONE,
                case_name: "None".to_string(),
                payload: None,
            },
            type_id: option_type,
            span,
        })),
    }
}

// -----------------------------------------------------------------------
// Phase 3b: call-site rewrite
// -----------------------------------------------------------------------

fn rewrite_call_sites(
    project: &mut NirPackage,
    candidates: &IndexMap<FuncId, Candidate>,
    touched: &mut IndexSet<usize>,
) {
    for i in 0..project.functions.len() {
        let mut func = project.functions[i].borrow_mut();
        let Some(mut body) = func.body.take() else {
            continue;
        };
        let span = func.span;
        // Every call to a rewritten callee now yields the tuple, wherever it
        // sits — a `let` initializer, a tail `return`, or the scrutinee about
        // to be hoisted into a `let`.
        let mut changed = retype_candidate_calls(&mut body, candidates);
        changed |= hoist_call_scrutinees(&mut body, &mut func, candidates, span);
        let mut bound = bound_temps(&body, candidates);
        // A site validated this round is known handleable; one inherited from an
        // earlier round is not, so each temp is re-checked. A temp that fails is
        // dropped here and reboxed instead of half-rewritten.
        let cells = cell_locals(&func);
        bound.retain(|&local, _| temp_uses_rewritable(&body, local, &cells));
        if !bound.is_empty() {
            for (&local, &f) in &bound {
                let tuple_type = candidates[&f].layout.tuple_type;
                func.locals[local as usize].type_id = tuple_type;
                retype_let(&mut body, local, tuple_type);
            }
            let names: Vec<String> = func.locals.iter().map(|l| l.name.clone()).collect();
            let root = body.root;
            let mut cx = SiteCx {
                bound: &bound,
                candidates,
                names: &names,
                span,
            };
            rewrite_temp_uses(&mut body, NodeRef::Block(root), &mut cx);
            changed = true;
        }
        func.body = Some(body);
        if changed {
            touched.insert(i);
        }
    }
}

/// Retype every call to a rewritten callee. The node's own `type_id` is what
/// downstream passes and `wir_build` read; leaving the variant there would make
/// a tuple-returning call claim to produce a boxed variant.
fn retype_candidate_calls(body: &mut Body, candidates: &IndexMap<FuncId, Candidate>) -> bool {
    let mut changed = false;
    for node in body.exprs.values_mut() {
        if let ExprKind::Call { func_id, .. } = &node.kind
            && let Some(cand) = candidates.get(func_id)
        {
            node.type_id = cand.layout.tuple_type;
            changed = true;
        }
    }
    changed
}

/// Retype the binding statement and every read of the bound temp. The local's
/// own entry is retyped by the caller; a `Let` and each `Local` node carry
/// their own copy.
fn retype_let(body: &mut Body, local: u32, tuple_type: TypeId) {
    for stmt in body.stmts.values_mut() {
        if let StmtKind::Let {
            local_index,
            type_id,
            ..
        } = &mut stmt.kind
            && *local_index == local
        {
            *type_id = tuple_type;
        }
    }
    for node in body.exprs.values_mut() {
        if let ExprKind::Local { index, .. } = &node.kind
            && *index == local
        {
            node.type_id = tuple_type;
        }
    }
}

/// Read-only context for the call-site rewrite over one body.
struct SiteCx<'a> {
    bound: &'a IndexMap<u32, FuncId>,
    candidates: &'a IndexMap<FuncId, Candidate>,
    /// Local names, so a rebuilt `Local` node keeps the binding's own name.
    names: &'a [String],
    span: Span,
}

impl SiteCx<'_> {
    fn layout_of(&self, local: u32) -> &Layout {
        &self.candidates[&self.bound[&local]].layout
    }
}

/// `match f(x) { … }` has no binding for the tuple to land in, so give it one:
/// `{ let __vr = f(x); match __vr { … } }`. Every call site is then the single
/// `let`-bound shape the rest of the rewrite handles.
fn hoist_call_scrutinees(
    body: &mut Body,
    func: &mut NirFunction,
    candidates: &IndexMap<FuncId, Candidate>,
    span: Span,
) -> bool {
    let mut targets: Vec<ExprId> = Vec::new();
    let cells = cell_locals(func);
    collect_call_scrutinees(
        body,
        NodeRef::Block(body.root),
        candidates,
        &cells,
        &mut targets,
    );
    if targets.is_empty() {
        return false;
    }
    for match_expr in targets {
        let ExprKind::Match { expr: scrut, arms } = body.exprs[match_expr].kind.clone() else {
            panic!("variant-return SROA: hoist target is not a `Match`");
        };
        let call = scrut
            .as_expr()
            .expect("variant-return SROA: hoist target has a promoted scrutinee");
        let call_type = body.exprs[call].type_id;

        let local_index = func.local_count();
        let name = format!("__vr_{local_index}");
        func.locals.push(NirLocal {
            name: name.clone(),
            type_id: call_type,
            is_mut: false,
        });

        // The `Match` moves into a fresh node so the original id can host the
        // wrapping `Block`, keeping every parent operand valid. The call node
        // is reused as the `let` initializer.
        let match_type = body.exprs[match_expr].type_id;
        let temp_read = body.exprs.push(ExprNode {
            kind: ExprKind::Local {
                index: local_index,
                name: name.clone(),
            },
            type_id: call_type,
            span,
        });
        let inner = body.exprs.push(ExprNode {
            kind: ExprKind::Match {
                expr: Operand::Expr(temp_read),
                arms,
            },
            type_id: match_type,
            span,
        });
        let let_stmt = body.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name,
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id: call_type,
                value: Operand::Expr(call),
                // A call result is fresh; nothing aliases it.
                skip_value_copy: true,
            },
            span,
        });
        let tail = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(inner)),
            span,
        });
        let block = body.blocks.push(BlockNode {
            stmts: vec![let_stmt, tail],
            span,
        });
        body.exprs[match_expr].kind = ExprKind::Block(block);
    }
    true
}

fn collect_call_scrutinees(
    body: &Body,
    node: NodeRef,
    candidates: &IndexMap<FuncId, Candidate>,
    cells: &IndexSet<u32>,
    out: &mut Vec<ExprId>,
) {
    // The arms have to be rewritable before a binding is minted for them: a
    // hoist the use-rewrite then declines would leave a tuple-typed temp under
    // variant patterns. Declining here leaves the site to `rebox_stragglers`.
    if let NodeRef::Expr(e) = node
        && let ExprKind::Match { expr: scrut, arms } = &body.exprs[e].kind
        && call_callee(body, *scrut).is_some_and(|f| candidates.contains_key(&f))
        && arms.iter().all(|a| arm_is_one_level(body, a, cells))
    {
        out.push(e);
    }
    body.for_each_child(node, |c| {
        collect_call_scrutinees(body, c, candidates, cells, out);
    });
}

/// Rewrite every destructuring read of a tuple-bound temp.
fn rewrite_temp_uses(body: &mut Body, node: NodeRef, cx: &mut SiteCx) {
    if let NodeRef::Expr(e) = node {
        match body.exprs[e].kind.clone() {
            ExprKind::VariantTag { expr } => {
                if let Some(local) = temp_local(body, expr, cx.bound) {
                    let tuple_type = cx.layout_of(local).tuple_type;
                    let kind = tag_read(body, local, tuple_type, cx);
                    body.exprs[e].kind = kind;
                    body.exprs[e].type_id = TypeTable::I32;
                    return;
                }
            }
            ExprKind::VariantTest {
                expr, case_index, ..
            } => {
                if let Some(local) = temp_local(body, expr, cx.bound) {
                    let tuple_type = cx.layout_of(local).tuple_type;
                    let kind = tag_read(body, local, tuple_type, cx);
                    let tag = body.exprs.push(ExprNode {
                        kind,
                        type_id: TypeTable::I32,
                        span: cx.span,
                    });
                    let k = constant(
                        body,
                        ValueKind::Int(u64::from(case_index), TypeTable::I32),
                        TypeTable::I32,
                    );
                    body.exprs[e].kind = ExprKind::Binary {
                        left: Operand::Expr(tag),
                        op: NirBinaryOp::Eq,
                        right: k,
                    };
                    body.exprs[e].type_id = TypeTable::BOOL;
                    return;
                }
            }
            ExprKind::VariantPayload {
                expr, case_index, ..
            } => {
                if let Some(local) = temp_local(body, expr, cx.bound) {
                    let layout = cx.layout_of(local).clone();
                    let read = slot_read(body, local, &layout, case_index, cx);
                    body.exprs[e].kind = body.exprs[read].kind.clone();
                    body.exprs[e].type_id = body.exprs[read].type_id;
                    return;
                }
            }
            ExprKind::Match { expr: scrut, arms } => {
                if let Some(local) = temp_local(body, scrut, cx.bound) {
                    let layout = cx.layout_of(local).clone();
                    rewrite_match_on_temp(body, e, local, &layout, arms, cx);
                }
            }
            _ => {}
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        rewrite_temp_uses(body, c, cx);
    }
}

/// Whether every mention of `local` is a destructure the rewrite lowers. A bare
/// read escapes the tuple, and a `Match` arm deeper than one level has no tag
/// form — either leaves the site to the rebox.
fn temp_uses_rewritable(body: &Body, local: u32, cells: &IndexSet<u32>) -> bool {
    let mut ok = true;
    check_temp_uses(body, NodeRef::Block(body.root), local, cells, &mut ok);
    ok
}

fn check_temp_uses(body: &Body, node: NodeRef, local: u32, cells: &IndexSet<u32>, ok: &mut bool) {
    if !*ok {
        return;
    }
    if let NodeRef::Stmt(s) = node
        && let StmtKind::Let {
            local_index, value, ..
        } = &body.stmts[s].kind
        && *local_index == local
    {
        // The binding's own initializer is the call, not a use.
        if let Some(e) = value.as_expr()
            && matches!(&body.exprs[e].kind, ExprKind::Call { .. })
        {
            return;
        }
    }
    if let NodeRef::Expr(e) = node {
        match &body.exprs[e].kind {
            ExprKind::VariantTag { expr }
            | ExprKind::VariantTest { expr, .. }
            | ExprKind::VariantPayload { expr, .. } => {
                if is_local(body, *expr, local) {
                    return;
                }
            }
            ExprKind::Match { expr: scrut, arms } => {
                if is_local(body, *scrut, local) {
                    if !arms.iter().all(|a| arm_is_one_level(body, a, cells)) {
                        *ok = false;
                        return;
                    }
                    let arms = arms.clone();
                    for a in &arms {
                        if let Some(g) = a.guard
                            && let Some(ge) = g.as_expr()
                        {
                            check_temp_uses(body, NodeRef::Expr(ge), local, cells, ok);
                        }
                        if let Some(be) = a.body.as_expr() {
                            check_temp_uses(body, NodeRef::Expr(be), local, cells, ok);
                        }
                    }
                    return;
                }
            }
            ExprKind::Local { index, .. } if *index == local => {
                *ok = false;
                return;
            }
            _ => {}
        }
    }
    body.for_each_child(node, |c| check_temp_uses(body, c, local, cells, ok));
}

fn is_local(body: &Body, op: Operand, local: u32) -> bool {
    op.as_expr().is_some_and(
        |e| matches!(&body.exprs[e].kind, ExprKind::Local { index, .. } if *index == local),
    )
}

fn temp_local(body: &Body, op: Operand, bound: &IndexMap<u32, FuncId>) -> Option<u32> {
    let e = op.as_expr()?;
    let ExprKind::Local { index, .. } = &body.exprs[e].kind else {
        return None;
    };
    bound.contains_key(index).then_some(*index)
}

fn local_read(body: &mut Body, local: u32, type_id: TypeId, cx: &SiteCx) -> ExprId {
    body.exprs.push(ExprNode {
        kind: ExprKind::Local {
            index: local,
            name: cx.names[local as usize].clone(),
        },
        type_id,
        span: cx.span,
    })
}

/// `t.0` — the tag.
fn tag_read(body: &mut Body, local: u32, tuple_type: TypeId, cx: &SiteCx) -> ExprKind {
    let recv = local_read(body, local, tuple_type, cx);
    ExprKind::FieldAccess {
        expr: Operand::Expr(recv),
        field_index: 0,
        field_name: "0".to_string(),
    }
}

/// `t.k`, unwrapped through `Some` when the slot is an `Option` wrapper.
///
/// Panics on a unit case: validation only admits a `VariantPayload` /
/// pattern binding for a case the layout gave a slot, so a missing one is a
/// hole in that check, not an input the rewrite may silently drop.
fn slot_read(body: &mut Body, local: u32, layout: &Layout, case_index: u32, cx: &SiteCx) -> ExprId {
    let slot = layout.case_slots[case_index as usize]
        .unwrap_or_else(|| panic!("variant-return SROA: payload read on unit case {case_index}"));
    let slot_type = layout.slot_types[slot.index];
    let field = slot.index + 1;
    let recv = local_read(body, local, layout.tuple_type, cx);
    let read = body.exprs.push(ExprNode {
        kind: ExprKind::FieldAccess {
            expr: Operand::Expr(recv),
            field_index: u32::try_from(field).expect("slot index overflow"),
            field_name: field.to_string(),
        },
        type_id: slot_type,
        span: cx.span,
    });
    if !slot.wrap_in_some {
        return read;
    }
    let payload_type = layout.case_payloads[case_index as usize];
    body.exprs.push(ExprNode {
        kind: ExprKind::VariantPayload {
            expr: Operand::Expr(read),
            case_index: OPTION_SOME,
            payload_type,
        },
        type_id: payload_type,
        span: cx.span,
    })
}

/// `match t { Ok(v) => A, … }` ⇒ `match t.0 { 0 => { let v = t.1; A }, … }`.
/// The scrutinee becomes a dense integer, so `match_to_switch` lowers it to a
/// `br_table` afterwards.
fn rewrite_match_on_temp(
    body: &mut Body,
    match_expr: ExprId,
    local: u32,
    layout: &Layout,
    arms: Vec<ArmData>,
    cx: &SiteCx,
) {
    let mut new_arms: Vec<ArmData> = Vec::with_capacity(arms.len());
    for arm in arms {
        let (case_index, binding) = match body.pats[arm.pattern].kind.clone() {
            PatKind::Wildcard => {
                new_arms.push(arm);
                continue;
            }
            PatKind::Variant {
                variant_name,
                bindings,
                ..
            } => {
                let case_index = layout.case_index_of(&variant_name).unwrap_or_else(|| {
                    panic!(
                        "variant-return SROA: arm names case `{variant_name}`, not in the layout"
                    )
                });
                let binding = bindings.first().and_then(|&b| match &body.pats[b].kind {
                    PatKind::Binding {
                        local_index,
                        type_id,
                        ..
                    } => Some((*local_index, *type_id)),
                    _ => None,
                });
                (case_index, binding)
            }
            // Validation admits only a wildcard or a one-level variant pattern;
            // anything else here means the scrutinee was retyped to a tuple
            // under a pattern that still expects the variant.
            other => panic!("variant-return SROA: unvalidated arm pattern {other:?}"),
        };

        let pattern = body.pats.push(PatNode {
            kind: PatKind::Literal(NirLiteralPattern::I128(i128::from(case_index))),
            span: body.pats[arm.pattern].span,
        });
        let (guard, arm_body) = match binding {
            None => (arm.guard, arm.body),
            Some(binding) => {
                // The binding is re-read in the guard as well as the body: a
                // guard runs before the body and may name it. Both reads are
                // pure field access, and the value graph hash-conses them.
                let guard = arm
                    .guard
                    .map(|g| bind_payload_before(body, local, layout, case_index, binding, g, cx));
                let arm_body =
                    bind_payload_before(body, local, layout, case_index, binding, arm.body, cx);
                (guard, arm_body)
            }
        };
        new_arms.push(ArmData {
            pattern,
            guard,
            body: arm_body,
            span: arm.span,
        });
    }

    let kind = tag_read(body, local, layout.tuple_type, cx);
    let tag = body.exprs.push(ExprNode {
        kind,
        type_id: TypeTable::I32,
        span: cx.span,
    });
    body.exprs[match_expr].kind = ExprKind::Match {
        expr: Operand::Expr(tag),
        arms: new_arms,
    };
}

/// `{ let v = t.k; <value> }` — the arm's payload binding, restored ahead of
/// whatever reads it.
fn bind_payload_before(
    body: &mut Body,
    local: u32,
    layout: &Layout,
    case_index: u32,
    binding: (u32, TypeId),
    value: Operand,
    cx: &SiteCx,
) -> Operand {
    let (binding_local, binding_type) = binding;
    let read = slot_read(body, local, layout, case_index, cx);
    let let_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Let {
            name: cx.names[binding_local as usize].clone(),
            local_index: binding_local,
            is_mut: false,
            is_reactive: false,
            type_id: binding_type,
            value: Operand::Expr(read),
            // The payload already went through whatever value copy `lower`
            // decided when it was a variant payload; this read introduces none.
            skip_value_copy: true,
        },
        span: cx.span,
    });
    let value_type = body.operand_type(value);
    let tail = body.stmts.push(StmtNode {
        kind: StmtKind::Expr(value),
        span: cx.span,
    });
    let block = body.blocks.push(BlockNode {
        stmts: vec![let_stmt, tail],
        span: cx.span,
    });
    Operand::Expr(body.exprs.push(ExprNode {
        kind: ExprKind::Block(block),
        type_id: value_type,
        span: cx.span,
    }))
}
