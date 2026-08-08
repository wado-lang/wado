//! Resource drop elaboration.
//!
//! A Component Model `own<resource>` handle (`Request`, `Response`, `Fields`,
//! `RequestOptions`, …) must be released with `resource.drop` exactly once.
//! Wado's surface language has no destructors, so without this pass every
//! resource a guest creates or receives — and never explicitly transfers —
//! leaks a host resource-table slot. Under `wado serve`'s pooled instance
//! reuse the table eventually exhausts (issue #1133).
//!
//! This pass runs pre-monomorphize, before CM-binding synthesis rewrites
//! resource method calls into canonical intrinsics. For every
//! function body it tracks each owned resource value and inserts a
//! `resource.drop` (emitted as a `CmRawCall` to the `resource-drop:<cm>`
//! canonical intrinsic) on every control-flow path where the value is still
//! owned at the end of its scope.
//!
//! ## Ownership model
//!
//! A resource value is *owned* by the binding it is stored in (a parameter or
//! a `let`). Ownership is *transferred* when the value is:
//!
//! - passed as a by-value argument to a call (constructors such as
//!   `Response::new(fields, …)`, consumers such as
//!   `Request::consume_body(request, …)`, or any user function taking the
//!   resource by value),
//! - returned, or placed into an aggregate / variant payload.
//!
//! A resource used only as a borrowing method receiver (`fields.has(…)`), a
//! `matches` test, or behind `&` is *not* transferred. The pass drops exactly
//! those owned resources that are never transferred on a given path, so the
//! drop is always the unique remaining owner — it can never double-free.
//!
//! A binding whose type only *contains* a resource (e.g.
//! `Result<Fields, HeaderError>` returned by `Fields::from_list`) is dropped
//! structurally: a synthesized `match` extracts and drops the resource in the
//! case(s) that carry one.
//!
//! ## Relationship to the ownership system
//!
//! Ownership transfer is read straight off the surface program: a value is
//! transferred when it is passed by value, returned, placed in an aggregate,
//! or used as the receiver of a by-value (`self`) method. Extraction is a
//! by-value method (`Result::unwrap(self) -> T`), so it needs no special
//! casing — the receiver is consumed like any other by-value argument. The
//! pass therefore drops "every owned binding not transferred on this path"
//! with no aggregate-shape guessing.

use crate::compiler_item::CompilerItem;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::package::Package;
use crate::synthesis::common::{
    cm_raw_call, expr_stmt, let_stmt, local_ref, return_stmt, synth_span,
};
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirMatchArm, TirPattern,
    TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};

/// An owned resource-bearing value currently live in some scope.
#[derive(Clone)]
struct Live {
    local: u32,
    name: String,
    type_id: TypeId,
}

/// Ownership state: a stable slot per resource value. `None` means the value
/// has been transferred or already dropped on the current path. Slots are
/// only ever appended (a new binding) or cleared (a transfer / drop), so a
/// slot index stays valid across cloned branch states.
type Owned = Vec<Option<Live>>;

/// Whether a statement sequence falls through to its successor.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Normal,
    Diverged,
}

struct Cx<'a> {
    tt: &'a TypeTable,
    reg: &'a CmInterfaceRegistry,
    struct_fields: &'a StructFieldReg,
    /// Base dispatch keys (monomorphization-invariant, see
    /// [`crate::name::LocalMethodName::base_dispatch_key`]) of instance methods
    /// whose `self` is taken by value. Calling such a method transfers ownership
    /// of the receiver (e.g. `Result::unwrap`, which moves the wrapped value
    /// out). A generic method and its instantiations share one key.
    owned_self: &'a IndexSet<String>,
    locals: &'a mut Vec<TirLocal>,
    local_count: &'a mut u32,
}

impl Cx<'_> {
    /// Allocate a fresh local slot (used to spill values and to bind variant
    /// payloads inside synthesized structural-drop `match`es).
    fn alloc_local(&mut self, type_id: TypeId, prefix: &str) -> (u32, String) {
        let idx = self.locals.len() as u32;
        let name = format!("__{prefix}_{idx}");
        self.locals.push(TirLocal {
            name: name.clone(),
            type_id,
            is_mut: false,
        });
        *self.local_count = self.locals.len() as u32;
        (idx, name)
    }
}

/// `Result<_, _>` recognised by name; the only structural carrier of an owned
/// resource that the fixtures exercise (`Fields::from_list`).
fn result_args(tt: &TypeTable, type_id: TypeId) -> Option<(TypeId, TypeId)> {
    match tt.get(tt.get_ultimate_base_type(type_id)) {
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == "Result" && type_args.len() == 2 => Some((type_args[0], type_args[1])),
        _ => None,
    }
}

/// Fields of each struct `(name, module)`, in declaration order, as
/// `(index, name, type_id)`. Built once so the drop walk can recurse into
/// struct fields without holding the `TirModule`s.
type StructFieldReg = crate::hashmap::IndexMap<(String, ModuleSource), Vec<(u32, String, TypeId)>>;

fn build_struct_field_reg(project: &Package) -> StructFieldReg {
    let mut reg: StructFieldReg = crate::hashmap::IndexMap::default();
    for module in project.tir_modules.values() {
        let structs = module.structs.iter().chain(module.generic_structs.values());
        for s in structs {
            reg.entry((s.name.clone(), s.module_source.clone()))
                .or_insert_with(|| {
                    s.fields
                        .iter()
                        .map(|f| (f.index, f.name.clone(), f.type_id))
                        .collect()
                });
        }
    }
    reg
}

/// Whether `type_id` owns, or structurally carries, a Component Model
/// resource that this pass knows how to drop: a bare resource, or a struct /
/// tuple / variant / `Result` that transitively contains one.
///
/// `GenericResource` (`Future` / `Stream`) is excluded: those handles have
/// their own explicit drop discipline and must not be touched here. A
/// reference stops the walk — a borrowed place owns nothing.
fn carries_resource(
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    sfr: &StructFieldReg,
    type_id: TypeId,
) -> bool {
    carries_resource_rec(tt, reg, sfr, type_id, &mut Vec::new())
}

fn carries_resource_rec(
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    sfr: &StructFieldReg,
    type_id: TypeId,
    visited: &mut Vec<TypeId>,
) -> bool {
    let base = tt.get_ultimate_base_type(type_id);
    if visited.contains(&base) {
        return false;
    }
    visited.push(base);
    let children: Vec<TypeId> = match tt.get(base).clone() {
        ResolvedType::Resource {
            name,
            module_source,
        } => {
            return reg
                .get_resource_cm_name_by_module(&module_source.to_string(), &name)
                .is_some();
        }
        ResolvedType::GenericResource { .. } | ResolvedType::Ref(_) | ResolvedType::MutRef(_) => {
            return false;
        }
        ResolvedType::Struct {
            decl_name,
            module_source,
            type_args,
        } => sfr
            .get(&(
                tt.struct_rendered_name(&decl_name, &type_args),
                module_source,
            ))
            .map(|fields| fields.iter().map(|(_, _, t)| *t).collect())
            .unwrap_or_default(),
        _ => {
            if let Some(elems) = tt.as_tuple(base) {
                elems
            } else if let Some((ok, err)) = result_args(tt, base) {
                vec![ok, err]
            } else {
                Vec::new()
            }
        }
    };
    children
        .into_iter()
        .any(|t| carries_resource_rec(tt, reg, sfr, t, visited))
}

/// The mangled name identifying a method for the `owned_self` set, or `None`
/// if `func` is not an instance method.
fn instance_method_key(func: &TirFunction) -> Option<String> {
    let info = func.method_info.as_ref()?;
    let first = func.params.first()?;
    if first.name == "self" {
        Some(info.base_dispatch_key())
    } else {
        None
    }
}

/// Record `func` if it is an instance method whose `self` is taken by value
/// (no `&`), i.e. a call to it transfers ownership of the receiver.
fn record_owned_self(func: &TirFunction, tt: &TypeTable, out: &mut IndexSet<String>) {
    let Some(key) = instance_method_key(func) else {
        return;
    };
    let self_ty = func.params[0].type_id;
    let by_value = !matches!(
        tt.get(self_ty),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    );
    if by_value {
        out.insert(key);
    }
}

/// Entry point: elaborate resource drops for every function in the project.
pub fn elaborate_resource_drops(project: &mut Package) {
    let struct_fields = build_struct_field_reg(project);
    let reg = &project.cm_interface_registry;
    let Some(type_table) = project
        .tir_modules
        .values()
        .next()
        .map(|m| m.type_table.clone())
    else {
        return;
    };
    let tt = type_table.borrow();

    // The receiver of a method call is consumed only when the method takes
    // `self` by value; collect those methods up front.
    let mut owned_self: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            record_owned_self(&func_rc.borrow(), &tt, &mut owned_self);
        }
    }

    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            elaborate_function(
                &mut func_rc.borrow_mut(),
                &tt,
                reg,
                &struct_fields,
                &owned_self,
            );
        }
    }
}

/// Elaborate resource drops for a single function body.
fn elaborate_function(
    func: &mut TirFunction,
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    struct_fields: &StructFieldReg,
    owned_self: &IndexSet<String>,
) {
    if func.body.is_none() {
        return;
    }

    // Parameters that own (or carry) a resource are live from entry. A
    // by-value `self` is just such a parameter: the method that received it
    // owns it and must drop it unless its body transfers it onward.
    let mut owned: Owned = Vec::new();
    for param in &func.params {
        if carries_resource(tt, reg, struct_fields, param.type_id) {
            owned.push(Some(Live {
                local: param.local_index,
                name: param.name.clone(),
                type_id: param.type_id,
            }));
        }
    }
    if owned.is_empty() && !body_has_resource(func.body.as_ref().unwrap(), tt, reg, struct_fields) {
        return;
    }

    let mut body = func.body.take().expect("body present");
    {
        let TirFunction {
            locals,
            local_count,
            ..
        } = &mut *func;
        let mut cx = Cx {
            tt,
            reg,
            struct_fields,
            owned_self,
            locals,
            local_count,
        };
        // The function body is its own scope: parameters (slots
        // `0..owned.len()`) count as declared here, so `entry = 0`
        // makes them drop at body end when never transferred.
        let stmts = std::mem::take(&mut body.stmts);
        body.stmts = elab_block_entry(stmts, &mut owned, &mut cx, 0);
    }
    func.body = Some(body);
}

/// Cheap pre-check: does the body bind any resource-carrying `let`? Lets the
/// pass skip the (vast majority of) functions that touch no resources.
fn body_has_resource(
    block: &TirBlock,
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    sfr: &StructFieldReg,
) -> bool {
    block.stmts.iter().any(|stmt| match &stmt.kind {
        TirStmtKind::Let { type_id, .. } => carries_resource(tt, reg, sfr, *type_id),
        TirStmtKind::LetDestructure { pattern, .. } => {
            pattern_carries_resource(pattern, tt, reg, sfr)
        }
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            body_has_resource(then_block, tt, reg, sfr)
                || else_block
                    .as_ref()
                    .is_some_and(|b| body_has_resource(b, tt, reg, sfr))
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            body_has_resource(body, tt, reg, sfr)
        }
        TirStmtKind::Expr(expr) => expr_has_resource(expr, tt, reg, sfr),
        _ => false,
    })
}

fn expr_has_resource(
    expr: &TirExpr,
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    sfr: &StructFieldReg,
) -> bool {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            body_has_resource(block, tt, reg, sfr)
        }
        TirExprKind::Match { arms, .. } => arms.iter().any(|arm| {
            pattern_carries_resource(&arm.pattern, tt, reg, sfr)
                || expr_has_resource(&arm.body, tt, reg, sfr)
        }),
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            body_has_resource(then_branch, tt, reg, sfr)
                || else_branch
                    .as_ref()
                    .is_some_and(|b| body_has_resource(b, tt, reg, sfr))
        }
        _ => false,
    }
}

fn pattern_carries_resource(
    pattern: &TirPattern,
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    sfr: &StructFieldReg,
) -> bool {
    match pattern {
        TirPattern::Binding { type_id, .. } => carries_resource(tt, reg, sfr, *type_id),
        TirPattern::Tuple(pats, _) | TirPattern::Or(pats) => pats
            .iter()
            .any(|p| pattern_carries_resource(p, tt, reg, sfr)),
        TirPattern::Variant { bindings, .. } => bindings
            .iter()
            .any(|p| pattern_carries_resource(p, tt, reg, sfr)),
        TirPattern::Struct { fields, .. } => fields
            .iter()
            .any(|f| pattern_carries_resource(&f.pattern, tt, reg, sfr)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Drop-statement synthesis
// ---------------------------------------------------------------------------

/// Build the statements that drop `live` — a `resource.drop` for a bare
/// resource, or a structural `match` for a resource-carrying aggregate.
fn drop_one(live: &Live, cx: &mut Cx) -> Vec<TirStmt> {
    drop_value(
        local_ref(live.local, &live.name, live.type_id),
        live.type_id,
        cx,
    )
}

/// Build the statements that release every Component Model resource reachable
/// from `scrutinee` (a value of type `type_id`).
fn drop_value(scrutinee: TirExpr, type_id: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    let base = cx.tt.get_ultimate_base_type(type_id);
    match cx.tt.get(base).clone() {
        ResolvedType::Resource {
            name,
            module_source,
        } => match cx
            .reg
            .get_resource_cm_name_by_module(&module_source.to_string(), &name)
        {
            Some(cm) => vec![expr_stmt(cm_raw_call(
                &format!("resource-drop:{cm}"),
                vec![scrutinee],
                TypeTable::UNIT,
            ))],
            None => Vec::new(),
        },
        ResolvedType::Struct {
            decl_name,
            module_source,
            type_args,
        } => {
            let fields = cx
                .struct_fields
                .get(&(
                    cx.tt.struct_rendered_name(&decl_name, &type_args),
                    module_source,
                ))
                .cloned()
                .unwrap_or_default();
            drop_projected(scrutinee, &fields, cx)
        }
        _ => {
            if let Some(elems) = cx.tt.as_tuple(base) {
                let fields: Vec<(u32, String, TypeId)> = elems
                    .into_iter()
                    .enumerate()
                    .map(|(i, ty)| (i as u32, i.to_string(), ty))
                    .collect();
                drop_projected(scrutinee, &fields, cx)
            } else if let Some((ok_ty, err_ty)) = result_args(cx.tt, type_id) {
                drop_result(scrutinee, type_id, ok_ty, err_ty, cx)
            } else {
                Vec::new()
            }
        }
    }
}

/// Drop each resource-carrying field / tuple element of `scrutinee` in
/// declaration order, projecting it with a `FieldAccess`. `scrutinee` is a
/// place expression (a local reference), so cloning it per field is cheap.
fn drop_projected(
    scrutinee: TirExpr,
    fields: &[(u32, String, TypeId)],
    cx: &mut Cx,
) -> Vec<TirStmt> {
    let mut stmts = Vec::new();
    for (index, name, field_ty) in fields {
        if !carries_resource(cx.tt, cx.reg, cx.struct_fields, *field_ty) {
            continue;
        }
        let field = crate::synthesis::common::field_access(
            scrutinee.clone(),
            *index,
            name.clone(),
            *field_ty,
            synth_span(),
        );
        stmts.extend(drop_value(field, *field_ty, cx));
    }
    stmts
}

/// Drop a `Result<ok, err>` structurally via a synthesized `match`.
fn drop_result(
    scrutinee: TirExpr,
    result_ty: TypeId,
    ok_ty: TypeId,
    err_ty: TypeId,
    cx: &mut Cx,
) -> Vec<TirStmt> {
    let ok_has = carries_resource(cx.tt, cx.reg, cx.struct_fields, ok_ty);
    let err_has = carries_resource(cx.tt, cx.reg, cx.struct_fields, err_ty);
    if !ok_has && !err_has {
        return Vec::new();
    }
    let span = synth_span();
    let ok_arm = result_drop_arm(result_ty, CompilerItem::ResultOk, ok_ty, ok_has, cx);
    let err_arm = result_drop_arm(result_ty, CompilerItem::ResultErr, err_ty, err_has, cx);
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(scrutinee),
            arms: vec![ok_arm, err_arm],
        },
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(match_expr), span)]
}

/// Build one arm of a `Result` structural-drop `match`. The payload is bound
/// to a fresh local and recursively dropped when `drop_payload` is set;
/// otherwise the arm body is empty.
fn result_drop_arm(
    result_ty: TypeId,
    case: CompilerItem,
    payload_ty: TypeId,
    drop_payload: bool,
    cx: &mut Cx,
) -> TirMatchArm {
    let span = synth_span();
    let case_name = cx.tt.compiler_variant_case(case).2.to_string();
    let (payload_local, payload_name) = cx.alloc_local(payload_ty, "drop_v");
    let body_stmts = if drop_payload {
        drop_value(
            local_ref(payload_local, &payload_name, payload_ty),
            payload_ty,
            cx,
        )
    } else {
        Vec::new()
    };
    TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: result_ty,
            variant_name: case_name,
            bindings: vec![TirPattern::Binding {
                name: payload_name,
                local_index: payload_local,
                type_id: payload_ty,
            }],
            payload_type: payload_ty,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(TirBlock::new(body_stmts, span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    }
}

// ---------------------------------------------------------------------------
// Transfer detection
// ---------------------------------------------------------------------------

/// Scan `expr` and clear every owned slot whose resource is transferred.
fn apply_transfers(owned: &mut Owned, expr: &TirExpr, cx: &Cx) {
    apply_transfers_root(owned, expr, true, cx);
}

/// Like [`apply_transfers`] but with an explicit `root_consuming` flag: a
/// `match` / `if let` scrutinee that no arm destructures a resource out of is
/// only inspected, not consumed, so its own resources still need dropping.
fn apply_transfers_root(owned: &mut Owned, expr: &TirExpr, root_consuming: bool, cx: &Cx) {
    let mut consumed: Vec<u32> = Vec::new();
    scan_transfers(expr, root_consuming, &mut consumed, cx);
    for local in consumed {
        for slot in owned.iter_mut() {
            if slot.as_ref().is_some_and(|l| l.local == local) {
                *slot = None;
            }
        }
    }
}

/// Whether matching `pattern` moves a resource out of the scrutinee.
fn pattern_extracts_resource(
    pattern: &TirPattern,
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    sfr: &StructFieldReg,
) -> bool {
    pattern_carries_resource(pattern, tt, reg, sfr)
}

/// Collect the local indices of resources transferred (consumed) by `expr`.
///
/// `consuming` is `true` when the current position transfers ownership of any
/// value placed in it. A resource local reached in a consuming position is
/// recorded; a borrowing position (`&x`, a method receiver, a `matches` test)
/// is not.
///
/// The match is exhaustive so a new `TirExprKind` cannot silently escape
/// transfer accounting — missing a transfer would risk a double drop.
fn scan_transfers(expr: &TirExpr, consuming: bool, consumed: &mut Vec<u32>, cx: &Cx) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            if consuming {
                consumed.push(*index);
            }
        }

        TirExprKind::Unary { op, expr: inner } => match op {
            // `&x` / `&mut x` borrows — never transfers the inner resource.
            TirUnaryOp::Ref | TirUnaryOp::MutRef => {
                scan_transfers(inner, false, consumed, cx);
            }
            _ => scan_transfers(inner, consuming, consumed, cx),
        },

        // Casts and projections keep the surrounding ownership context:
        // `&(h as Fields)` must still see `h` as borrowed.
        TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TupleLen { expr: inner }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            scan_transfers(inner, consuming, consumed, cx);
        }

        // Tag inspection / `matches` test: reads the discriminant only, never
        // transfers the value.
        TirExprKind::VariantTag { expr: inner } | TirExprKind::VariantTest { expr: inner, .. } => {
            scan_transfers(inner, false, consumed, cx);
        }

        TirExprKind::Call {
            func,
            args,
            has_receiver,
            ..
        } => {
            let rest = match has_receiver.then(|| args.split_first()).flatten() {
                Some((receiver, rest)) => {
                    // `&self` methods auto-reference the receiver; look through
                    // that `&` to reach the underlying value.
                    let recv_inner = match &receiver.expr.kind {
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                            expr,
                        } => expr.as_ref(),
                        _ => &receiver.expr,
                    };
                    // The receiver is transferred exactly when the method takes
                    // `self` by value. Extraction (`Result::unwrap`) is such a
                    // by-value method, so it is covered here with no
                    // aggregate-shape guessing.
                    let receiver_consumed = func
                        .method_info
                        .as_ref()
                        .is_some_and(|info| cx.owned_self.contains(&info.base_dispatch_key()));
                    scan_transfers(recv_inner, receiver_consumed, consumed, cx);
                    rest
                }
                None => args.as_slice(),
            };
            for arg in rest {
                scan_transfers(&arg.expr, true, consumed, cx);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                scan_transfers(arg, true, consumed, cx);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            scan_transfers(callee, true, consumed, cx);
            for arg in args {
                scan_transfers(arg, true, consumed, cx);
            }
        }

        TirExprKind::Binary { left, right, .. } => {
            scan_transfers(left, true, consumed, cx);
            scan_transfers(right, true, consumed, cx);
        }
        TirExprKind::Assign { target, value } => {
            scan_transfers(target, false, consumed, cx);
            scan_transfers(value, true, consumed, cx);
        }
        TirExprKind::Index { expr: base, index } => {
            scan_transfers(base, consuming, consumed, cx);
            scan_transfers(index, true, consumed, cx);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            scan_transfers(value, true, consumed, cx);
        }

        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            scan_block_transfers(block, consumed, cx);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            scan_transfers(condition, true, consumed, cx);
            scan_block_transfers(then_branch, consumed, cx);
            if let Some(eb) = else_branch {
                scan_block_transfers(eb, consumed, cx);
            }
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            // The scrutinee is consumed only if some arm destructures a
            // resource out of it; a pure `matches`-style test leaves the
            // scrutinee — and its drop obligation — intact.
            let extracts = arms.iter().any(|arm| {
                pattern_extracts_resource(&arm.pattern, cx.tt, cx.reg, cx.struct_fields)
            });
            scan_transfers(scrutinee, extracts, consumed, cx);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    scan_transfers(guard, true, consumed, cx);
                }
                scan_transfers(&arm.body, true, consumed, cx);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                scan_transfers(&field.value, true, consumed, cx);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                scan_transfers(elem, true, consumed, cx);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload) = payload {
                scan_transfers(payload, true, consumed, cx);
            }
        }
        TirExprKind::TypePackExpansion { call_expr, .. } => {
            scan_transfers(call_expr, true, consumed, cx);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let crate::tir::TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    scan_transfers(inner, true, consumed, cx);
                }
            }
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                scan_transfers(&binding.handler, true, consumed, cx);
            }
            scan_block_transfers(body, consumed, cx);
        }
        TirExprKind::Resume { value } => {
            scan_transfers(value, true, consumed, cx);
        }
        TirExprKind::GlobalVarGet { .. } => {}

        // A closure body addresses captured values through `Capture`, not the
        // enclosing function's `Local`s, so its locals belong to a different
        // index space and must not be scanned here.
        TirExprKind::Closure { .. } => {}

        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Scan a block solely for transfers (used inside expression position, where
/// the pass does not insert drops — only ownership accounting matters).
fn scan_block_transfers(block: &TirBlock, consumed: &mut Vec<u32>, cx: &Cx) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let { value, .. }
            | TirStmtKind::Expr(value)
            | TirStmtKind::TaskReturn { value }
            | TirStmtKind::LetDestructure { value, .. } => {
                scan_transfers(value, true, consumed, cx);
            }
            TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    scan_transfers(value, true, consumed, cx);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                scan_transfers(condition, true, consumed, cx);
                scan_block_transfers(then_block, consumed, cx);
                if let Some(eb) = else_block {
                    scan_block_transfers(eb, consumed, cx);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                scan_block_transfers(body, consumed, cx);
            }
            TirStmtKind::VariadicForOf { iterable, body, .. } => {
                scan_transfers(iterable, true, consumed, cx);
                scan_block_transfers(body, consumed, cx);
            }
            TirStmtKind::Continue => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Pattern resource bindings
// ---------------------------------------------------------------------------

/// Collect resource-carrying values bound by `pattern` (e.g. `response` in
/// `let [response, _tx] = Response::new(...)`).
fn pattern_resources(pattern: &TirPattern, cx: &Cx) -> Vec<Live> {
    let mut out = Vec::new();
    collect_pattern_resources(pattern, cx, &mut out);
    out
}

fn collect_pattern_resources(pattern: &TirPattern, cx: &Cx, out: &mut Vec<Live>) {
    match pattern {
        TirPattern::Binding {
            name,
            local_index,
            type_id,
        } => {
            if carries_resource(cx.tt, cx.reg, cx.struct_fields, *type_id) {
                out.push(Live {
                    local: *local_index,
                    name: name.clone(),
                    type_id: *type_id,
                });
            }
        }
        TirPattern::Tuple(pats, _) | TirPattern::Or(pats) => {
            for p in pats {
                collect_pattern_resources(p, cx, out);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                collect_pattern_resources(p, cx, out);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                collect_pattern_resources(&f.pattern, cx, out);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Block / statement elaboration
// ---------------------------------------------------------------------------

/// Elaborate a block whose own scope starts at slot `entry`: resources in
/// slots `>= entry` are declared here and dropped at block exit; resources in
/// slots `< entry` belong to an enclosing scope.
fn elab_block_entry(
    stmts: Vec<TirStmt>,
    owned: &mut Owned,
    cx: &mut Cx,
    entry: usize,
) -> Vec<TirStmt> {
    let mut out: Vec<TirStmt> = Vec::new();
    let mut flow = Flow::Normal;

    let last = stmts.len().wrapping_sub(1);
    for (idx, stmt) in stmts.into_iter().enumerate() {
        if flow == Flow::Diverged {
            // Unreachable after a diverging statement; preserve verbatim.
            out.push(stmt);
            continue;
        }
        flow = elab_stmt(stmt, owned, cx, entry, idx == last, &mut out);
    }

    if flow == Flow::Normal {
        // Drop resources declared in this block, innermost (last) first.
        let drops: Vec<TirStmt> = (entry..owned.len())
            .rev()
            .filter_map(|slot| owned[slot].take())
            .flat_map(|live| drop_one(&live, cx))
            .collect();
        if !drops.is_empty() {
            out = append_block_drops(out, drops, cx);
        }
    }
    // Block-local slots leave scope; enclosing slots (with any transfers
    // applied within this block) remain visible to the caller.
    owned.truncate(entry);
    out
}

/// Append end-of-scope `drops` to a block's statements without changing the
/// block's value.
///
/// A block expression's value is its last statement (see
/// [`crate::tir::block_result_type`]). Naively appending the drop statements
/// would make the block evaluate to the last drop (`Unit`) and miscompile
/// `let x = { ... }`. So when the block yields a value, the original
/// statements are nested into an inner block expression, its value bound to a
/// fresh local, the drops run, and that local is re-emitted as the value.
fn append_block_drops(stmts: Vec<TirStmt>, drops: Vec<TirStmt>, cx: &mut Cx) -> Vec<TirStmt> {
    let span = synth_span();
    let inner = TirBlock { stmts, span };
    let result_ty = crate::tir::block_result_type(&inner);
    if result_ty == TypeTable::UNIT || result_ty == TypeTable::NEVER {
        // The block yields nothing observable; the drops can simply run last.
        let mut out = inner.stmts;
        out.extend(drops);
        return out;
    }
    // Preserve the value: `let __block_v = { <stmts> }; <drops>; __block_v`.
    let (v, vname) = cx.alloc_local(result_ty, "block_v");
    let inner_expr = TirExpr::new(TirExprKind::Block(inner), result_ty, span);
    let mut out = vec![let_stmt(&vname, v, result_ty, inner_expr)];
    out.extend(drops);
    out.push(expr_stmt(local_ref(v, &vname, result_ty)));
    out
}

/// Elaborate a block as a fresh nested scope.
fn elab_block(block: TirBlock, owned: &mut Owned, cx: &mut Cx) -> (TirBlock, Flow) {
    let entry = owned.len();
    let span = block.span;
    let stmts = elab_block_entry(block.stmts, owned, cx, entry);
    let flow = block_flow(&stmts);
    (TirBlock { stmts, span }, flow)
}

/// Whether a finished statement list diverges (its last statement does not
/// fall through).
fn block_flow(stmts: &[TirStmt]) -> Flow {
    match stmts.last() {
        Some(stmt) => stmt_flow(stmt),
        None => Flow::Normal,
    }
}

fn stmt_flow(stmt: &TirStmt) -> Flow {
    match &stmt.kind {
        TirStmtKind::Return { .. } | TirStmtKind::Break { .. } | TirStmtKind::Continue => {
            Flow::Diverged
        }
        TirStmtKind::If {
            then_block,
            else_block: Some(else_block),
            ..
        } => {
            if block_flow(&then_block.stmts) == Flow::Diverged
                && block_flow(&else_block.stmts) == Flow::Diverged
            {
                Flow::Diverged
            } else {
                Flow::Normal
            }
        }
        _ => Flow::Normal,
    }
}

/// Elaborate one statement, appending the result to `out`. Returns whether
/// control falls through to the next statement.
fn elab_stmt(
    stmt: TirStmt,
    owned: &mut Owned,
    cx: &mut Cx,
    entry: usize,
    is_tail: bool,
    out: &mut Vec<TirStmt>,
) -> Flow {
    let span = stmt.span;
    match stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
            skip_value_copy,
        } => {
            let value = elab_value_expr(value, owned, cx);
            if carries_resource(cx.tt, cx.reg, cx.struct_fields, type_id) {
                owned.push(Some(Live {
                    local: local_index,
                    name: name.clone(),
                    type_id,
                }));
            }
            out.push(TirStmt {
                kind: TirStmtKind::Let {
                    name,
                    local_index,
                    is_mut,
                    is_reactive,
                    type_id,
                    value,
                    skip_value_copy,
                },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::LetDestructure {
            pattern,
            is_mut,
            value,
        } => {
            let value = elab_value_expr(value, owned, cx);
            for live in pattern_resources(&pattern, cx) {
                owned.push(Some(live));
            }
            out.push(TirStmt {
                kind: TirStmtKind::LetDestructure {
                    pattern,
                    is_mut,
                    value,
                },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::Expr(expr) => {
            let elaborated = elab_value_expr(expr, owned, cx);
            if !is_tail && carries_resource(cx.tt, cx.reg, cx.struct_fields, elaborated.type_id) {
                let ty = elaborated.type_id;
                out.extend(drop_value(elaborated, ty, cx));
            } else {
                out.push(TirStmt {
                    kind: TirStmtKind::Expr(elaborated),
                    span,
                });
            }
            Flow::Normal
        }

        TirStmtKind::TaskReturn { value } => {
            let value = elab_value_expr(value, owned, cx);
            out.push(TirStmt {
                kind: TirStmtKind::TaskReturn { value },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::Return { value } => {
            let value = value.map(|v| elab_value_expr(v, owned, cx));
            // Everything still owned must be dropped before leaving the
            // function, innermost first.
            let drops: Vec<Live> = owned
                .iter_mut()
                .rev()
                .filter_map(std::option::Option::take)
                .collect();
            if drops.is_empty() {
                out.push(TirStmt {
                    kind: TirStmtKind::Return { value },
                    span,
                });
                return Flow::Diverged;
            }
            if let Some(value) = value {
                // Spill the return value so the drops run after it is
                // computed (the value may borrow a dropped resource).
                let ty = value.type_id;
                let (tmp, tmp_name) = cx.alloc_local(ty, "drop_spill");
                out.push(let_stmt(&tmp_name, tmp, ty, value));
                for live in &drops {
                    let stmts = drop_one(live, cx);
                    out.extend(stmts);
                }
                out.push(return_stmt(Some(local_ref(tmp, &tmp_name, ty))));
            } else {
                for live in &drops {
                    let stmts = drop_one(live, cx);
                    out.extend(stmts);
                }
                out.push(return_stmt(None));
            }
            Flow::Diverged
        }

        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            apply_transfers(owned, &condition, cx);
            let (then_block, then_owned, then_flow) = elab_branch(then_block, owned, &[], cx);
            let (else_block, else_owned, else_flow) = match else_block {
                Some(eb) => {
                    let (b, o, f) = elab_branch(eb, owned, &[], cx);
                    (Some(b), Some(o), f)
                }
                None => (None, None, Flow::Normal),
            };
            let mut then_stmts = then_block.stmts;
            let mut else_stmts = else_block
                .as_ref()
                .map(|b| b.stmts.clone())
                .unwrap_or_default();
            reconcile(
                owned,
                &then_owned,
                then_flow,
                &mut then_stmts,
                else_owned.as_ref(),
                else_flow,
                &mut else_stmts,
                cx,
            );
            let diverged = matches!(then_flow, Flow::Diverged)
                && else_block.is_some()
                && matches!(else_flow, Flow::Diverged);
            let new_else = if else_block.is_some() || !else_stmts.is_empty() {
                Some(TirBlock {
                    stmts: else_stmts,
                    span,
                })
            } else {
                None
            };
            out.push(TirStmt {
                kind: TirStmtKind::If {
                    condition,
                    then_block: TirBlock {
                        stmts: then_stmts,
                        span: then_block.span,
                    },
                    else_block: new_else,
                },
                span,
            });
            if diverged {
                Flow::Diverged
            } else {
                Flow::Normal
            }
        }

        TirStmtKind::Loop { body } => {
            let mut body_owned = owned.clone();
            let (body, _flow) = elab_block(body, &mut body_owned, cx);
            // A resource of an enclosing scope transferred inside the loop
            // body is consumed (possibly every iteration); reflect that so it
            // is never dropped again after the loop.
            for (slot, after) in owned.iter_mut().zip(body_owned.iter()) {
                if after.is_none() {
                    *slot = None;
                }
            }
            out.push(TirStmt {
                kind: TirStmtKind::Loop { body },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::LabeledBlock { label, block } => {
            let (block, flow) = elab_block(block, owned, cx);
            out.push(TirStmt {
                kind: TirStmtKind::LabeledBlock { label, block },
                span,
            });
            flow
        }

        TirStmtKind::Break { label, value } => {
            let value = value.map(|v| elab_value_expr(v, owned, cx));
            // `break` skips this block's exit drops; emit drops for resources
            // declared in this block here, innermost first.
            let drops: Vec<TirStmt> = (entry..owned.len())
                .rev()
                .filter_map(|slot| owned[slot].take())
                .flat_map(|live| drop_one(&live, cx))
                .collect();
            match value {
                // Spill the break value so the drops run after it is computed
                // (it may borrow a resource being dropped).
                Some(value) if !drops.is_empty() => {
                    let ty = value.type_id;
                    let (tmp, tmp_name) = cx.alloc_local(ty, "drop_spill");
                    out.push(let_stmt(&tmp_name, tmp, ty, value));
                    out.extend(drops);
                    out.push(TirStmt {
                        kind: TirStmtKind::Break {
                            label,
                            value: Some(local_ref(tmp, &tmp_name, ty)),
                        },
                        span,
                    });
                }
                _ => {
                    out.extend(drops);
                    out.push(TirStmt {
                        kind: TirStmtKind::Break { label, value },
                        span,
                    });
                }
            }
            Flow::Diverged
        }

        TirStmtKind::Continue => {
            for slot in (entry..owned.len()).rev() {
                if let Some(live) = owned[slot].take() {
                    let drops = drop_one(&live, cx);
                    out.extend(drops);
                }
            }
            out.push(TirStmt {
                kind: TirStmtKind::Continue,
                span,
            });
            Flow::Diverged
        }

        TirStmtKind::VariadicForOf { .. } => {
            // Type-pack `for` expansion is resolved post-monomorphize; leave it
            // untouched (its body holds no resources in practice).
            if let TirStmtKind::VariadicForOf { iterable, .. } = &stmt.kind {
                apply_transfers(owned, iterable, cx);
            }
            out.push(stmt);
            Flow::Normal
        }
    }
}

/// Elaborate a branch block from the current ownership state, optionally with
/// extra pattern-bound resources scoped to that branch. Returns the rewritten
/// block, its post-state (truncated back to the caller's slot count) and its
/// control flow.
fn elab_branch(
    block: TirBlock,
    base: &Owned,
    extra: &[Live],
    cx: &mut Cx,
) -> (TirBlock, Owned, Flow) {
    let base_len = base.len();
    let mut branch_owned = base.clone();
    for live in extra {
        branch_owned.push(Some(live.clone()));
    }
    let span = block.span;
    // `entry = base_len` makes the pattern bindings (slots `>= base_len`)
    // drop at the branch's end.
    let stmts = elab_block_entry(block.stmts, &mut branch_owned, cx, base_len);
    let flow = block_flow(&stmts);
    branch_owned.truncate(base_len);
    (TirBlock { stmts, span }, branch_owned, flow)
}

/// Reconcile ownership across the two arms of an `if` so the post-state is
/// consistent: a resource still owned on one fall-through path but consumed on
/// the other is dropped at the end of the path that still owns it.
#[allow(clippy::too_many_arguments)]
fn reconcile(
    owned: &mut Owned,
    then_owned: &Owned,
    then_flow: Flow,
    then_stmts: &mut Vec<TirStmt>,
    else_owned: Option<&Owned>,
    else_flow: Flow,
    else_stmts: &mut Vec<TirStmt>,
    cx: &mut Cx,
) {
    let then_div = matches!(then_flow, Flow::Diverged);
    let else_div = matches!(else_flow, Flow::Diverged);
    let has_else = else_owned.is_some();

    for slot in 0..owned.len() {
        let Some(live) = owned[slot].clone() else {
            continue;
        };
        // A diverged branch has already dropped everything it owned, so it
        // contributes nothing to the merge.
        let then_has = !then_div && then_owned[slot].is_some();
        let else_has = if !has_else {
            // No else block: the else path keeps whatever was owned.
            true
        } else if else_div {
            false
        } else {
            else_owned.unwrap()[slot].is_some()
        };

        match (then_div, else_div && has_else) {
            (true, true) => {
                // Whole `if` diverges; the post-state is irrelevant.
            }
            (true, _) => {
                owned[slot] = if else_has { Some(live) } else { None };
            }
            (_, true) => {
                owned[slot] = if then_has { Some(live) } else { None };
            }
            (false, false) => match (then_has, else_has) {
                (true, true) => {}
                (false, false) => owned[slot] = None,
                (true, false) => {
                    let drops = drop_one(&live, cx);
                    let s = std::mem::take(then_stmts);
                    *then_stmts = append_block_drops(s, drops, cx);
                    owned[slot] = None;
                }
                (false, true) => {
                    let drops = drop_one(&live, cx);
                    let s = std::mem::take(else_stmts);
                    *else_stmts = append_block_drops(s, drops, cx);
                    owned[slot] = None;
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Expression-position elaboration
// ---------------------------------------------------------------------------

/// Elaborate an expression used in statement position. Control-flow shapes
/// that can introduce resource bindings (`match`, block expressions) are
/// elaborated recursively; everything else only needs transfer accounting.
fn elab_value_expr(expr: TirExpr, owned: &mut Owned, cx: &mut Cx) -> TirExpr {
    let type_id = expr.type_id;
    let span = expr.span;
    match expr.kind {
        TirExprKind::Block(block) => {
            let (block, _flow) = elab_block(block, owned, cx);
            TirExpr {
                kind: TirExprKind::Block(block),
                type_id,
                span,
            }
        }
        TirExprKind::WithHandler {
            bindings,
            body,
            result_type,
        } => {
            for binding in &bindings {
                apply_transfers(owned, &binding.handler, cx);
            }
            let (body, _flow) = elab_block(body, owned, cx);
            TirExpr {
                kind: TirExprKind::WithHandler {
                    bindings,
                    body,
                    result_type,
                },
                type_id,
                span,
            }
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            let extracts = arms.iter().any(|arm| {
                pattern_extracts_resource(&arm.pattern, cx.tt, cx.reg, cx.struct_fields)
            });
            apply_transfers_root(owned, &scrutinee, extracts, cx);
            let base = owned.clone();
            let mut new_arms = Vec::with_capacity(arms.len());
            // Per-arm post-state for the cross-arm merge.
            let mut arm_states: Vec<(Owned, Flow, usize)> = Vec::new();
            for (idx, arm) in arms.into_iter().enumerate() {
                if let Some(guard) = &arm.guard {
                    apply_transfers(owned, guard, cx);
                }
                let pat = pattern_resources(&arm.pattern, cx);
                let mut arm_owned = base.clone();
                for live in &pat {
                    arm_owned.push(Some(live.clone()));
                }
                let body = elab_arm_body(arm.body, &mut arm_owned, cx, base.len());
                arm_owned.truncate(base.len());
                arm_states.push((arm_owned, body.1, idx));
                new_arms.push(TirMatchArm {
                    pattern: arm.pattern,
                    guard: arm.guard,
                    body: body.0,
                    span: arm.span,
                });
            }
            // Merge: a resource is still owned only if every fall-through arm
            // still owns it; drop it at the end of arms that diverge from the
            // merged state.
            for slot in 0..base.len() {
                if base[slot].is_none() {
                    continue;
                }
                let live = base[slot].clone().unwrap();
                let live_arm_idxs: Vec<usize> = arm_states
                    .iter()
                    .filter(|(_, flow, _)| *flow == Flow::Normal)
                    .map(|(_, _, idx)| *idx)
                    .collect();
                if live_arm_idxs.is_empty() {
                    continue;
                }
                let kept = arm_states
                    .iter()
                    .filter(|(_, flow, _)| *flow == Flow::Normal)
                    .all(|(o, _, _)| o[slot].is_some());
                if kept {
                    continue;
                }
                for (o, _, idx) in &arm_states {
                    if live_arm_idxs.contains(idx) && o[slot].is_some() {
                        append_arm_drop(&mut new_arms[*idx], &live, cx);
                    }
                }
                owned[slot] = None;
            }
            TirExpr {
                kind: TirExprKind::Match {
                    expr: scrutinee,
                    arms: new_arms,
                },
                type_id,
                span,
            }
        }
        other => {
            let expr = TirExpr {
                kind: other,
                type_id,
                span,
            };
            apply_transfers(owned, &expr, cx);
            expr
        }
    }
}

/// Elaborate a `match` arm body. A block body is elaborated as a nested scope;
/// any other body only needs transfer accounting.
fn elab_arm_body(body: TirExpr, owned: &mut Owned, cx: &mut Cx, entry: usize) -> (TirExpr, Flow) {
    let type_id = body.type_id;
    let span = body.span;
    match body.kind {
        TirExprKind::Block(block) => {
            let stmts = elab_block_entry(block.stmts, owned, cx, entry);
            let flow = block_flow(&stmts);
            (
                TirExpr {
                    kind: TirExprKind::Block(TirBlock {
                        stmts,
                        span: block.span,
                    }),
                    type_id,
                    span,
                },
                flow,
            )
        }
        other => {
            let expr = TirExpr {
                kind: other,
                type_id,
                span,
            };
            apply_transfers(owned, &expr, cx);
            (expr, Flow::Normal)
        }
    }
}

/// Append a `resource.drop` to a `match` arm body, preserving the arm's value
/// (via [`append_block_drops`]) so the drop runs after the value is computed.
fn append_arm_drop(arm: &mut TirMatchArm, live: &Live, cx: &mut Cx) {
    let body = std::mem::replace(
        &mut arm.body,
        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
    );
    let type_id = body.type_id;
    let span = body.span;
    let stmts = match body.kind {
        TirExprKind::Block(block) => block.stmts,
        other => vec![expr_stmt(TirExpr {
            kind: other,
            type_id,
            span,
        })],
    };
    let drops = drop_one(live, cx);
    let stmts = append_block_drops(stmts, drops, cx);
    arm.body = TirExpr {
        kind: TirExprKind::Block(TirBlock { stmts, span }),
        type_id,
        span,
    };
}
