//! Resource drop elaboration. A CM `own<resource>` handle must be released with
//! `resource.drop` exactly once and Wado has no destructors, so this inserts a
//! drop wherever a value is still owned at the end of its scope. A resource is
//! *transferred* when passed by value, returned, or placed in an aggregate — not
//! by a borrowing receiver or a `&` — so the drop cannot double-free. A
//! temporary that is only borrowed has no other owner, so it is spilled into a
//! local that drops when its statement ends.

use crate::canonical::{CanonicalIntrinsic, CmDecl};
use crate::compiler_item::CompilerItem;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::name::minted_name;
use crate::package::Package;
use crate::synthesis::common;
use crate::synthesis::common::{
    cm_canonical_call, expr_stmt, let_stmt, local_ref, return_stmt, synth_span,
};
use crate::tir::{
    ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal,
    TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirTemplatePart, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;
use crate::{hashmap, tir};

/// An owned resource-bearing value currently live in some scope.
#[derive(Clone)]
struct Live {
    local: u32,
    name: String,
    type_id: TypeId,
    /// A borrowed temporary spilled by [`spill_temporary`], owned by its
    /// statement rather than by the enclosing block.
    temporary: bool,
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

/// Where a `break` or `continue` lands: a labeled block, or a loop
/// (`label: None`). Leaving for it drops every slot from `entry` up.
struct BreakTarget {
    label: Option<String>,
    entry: usize,
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
    /// The enclosing loops and labeled blocks, innermost last.
    targets: Vec<BreakTarget>,
}

impl Cx<'_> {
    /// Allocate a fresh local slot (used to spill values and to bind variant
    /// payloads inside synthesized structural-drop `match`es).
    fn alloc_local(&mut self, type_id: TypeId, what: &str) -> (u32, String) {
        let idx = self.locals.len() as u32;
        let name = minted_name(what, idx);
        self.locals.push(TirLocal {
            name: name.clone(),
            type_id,
            is_mut: false,
            span: Span::default(),
        });
        *self.local_count = self.locals.len() as u32;
        (idx, name)
    }

    fn carries_resource(&self, type_id: TypeId) -> bool {
        carries_resource(self.tt, self.reg, self.struct_fields, type_id)
    }

    /// The first slot a `break label` (or, with `None`, a `break` or
    /// `continue`) leaves behind.
    fn target_entry(&self, label: Option<&str>) -> usize {
        self.targets
            .iter()
            .rev()
            .find(|target| target.label.as_deref() == label)
            .expect("a `break` or `continue` lands in an enclosing loop or labeled block")
            .entry
    }
}

/// Fields of each struct `(name, module)`, in declaration order, as
/// `(index, name, type_id)`. Built once so the drop walk can recurse into
/// struct fields without holding the `TirModule`s.
type StructFieldReg = hashmap::IndexMap<(String, ModuleSource), Vec<(u32, String, TypeId)>>;

fn build_struct_field_reg(project: &Package) -> StructFieldReg {
    let mut reg: StructFieldReg = hashmap::IndexMap::default();
    for module in project.tir_modules.values() {
        for s in &module.structs {
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
    let base = tt.representation_head(type_id);
    if visited.contains(&base) {
        return false;
    }
    visited.push(base);
    let children: Vec<TypeId> = match tt.get(base).clone() {
        ResolvedType::Resource { def } => {
            return !tt.is_unrestricted_resource(def)
                && reg
                    .get_resource_cm_name_by_module(
                        &tt.def_module(def).to_string(),
                        tt.def_name(def),
                    )
                    .is_some();
        }
        ResolvedType::GenericResource { .. } | ResolvedType::Ref(_) | ResolvedType::MutRef(_) => {
            return false;
        }
        ResolvedType::Struct { def, type_args } => sfr
            .get(&(
                tt.struct_rendered_name(def, &type_args),
                tt.struct_head_module(def).clone(),
            ))
            .map(|fields| fields.iter().map(|(_, _, t)| *t).collect())
            .unwrap_or_default(),
        _ => {
            if let Some(elems) = tt.as_tuple(base) {
                elems
            } else if let Some((ok, err)) = tt.as_result(base) {
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
    func.takes_self().then(|| info.base_dispatch_key())
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
                temporary: false,
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
            targets: Vec::new(),
        };
        // The function body is its own scope: parameters (slots
        // `0..owned.len()`) count as declared here, so `entry = 0`
        // makes them drop at body end when never transferred.
        let stmts = std::mem::take(&mut body.stmts);
        body.stmts = elab_block_entry(stmts, &mut owned, &mut cx, 0);
    }
    func.body = Some(body);
}

/// Cheap pre-check: does the body produce or bind any resource-carrying
/// value? Lets the pass skip the (vast majority of) functions that touch no
/// resources.
fn body_has_resource(
    block: &TirBlock,
    tt: &TypeTable,
    reg: &CmInterfaceRegistry,
    sfr: &StructFieldReg,
) -> bool {
    let mut probe = ResourceProbe {
        tt,
        reg,
        sfr,
        found: false,
    };
    probe.visit_block(block);
    probe.found
}

struct ResourceProbe<'a> {
    tt: &'a TypeTable,
    reg: &'a CmInterfaceRegistry,
    sfr: &'a StructFieldReg,
    found: bool,
}

impl TirRefVisitor for ResourceProbe<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.found {
            return;
        }
        if carries_resource(self.tt, self.reg, self.sfr, expr.type_id) {
            self.found = true;
            return;
        }
        self.walk_expr_in_frame(expr);
    }

    fn visit_pattern(&mut self, pattern: &TirPattern) {
        if pattern_carries_resource(pattern, self.tt, self.reg, self.sfr) {
            self.found = true;
        }
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

/// Take every value still owned in the slots from `first` up, and build the
/// statements that drop them, innermost first.
fn drop_slots(owned: &mut Owned, first: usize, cx: &mut Cx) -> Vec<TirStmt> {
    let lives: Vec<Live> = owned[first..]
        .iter_mut()
        .rev()
        .filter_map(Option::take)
        .collect();
    lives.iter().flat_map(|live| drop_one(live, cx)).collect()
}

/// [`drop_slots`], for the temporaries alone: a statement's own spills, which
/// drop when it ends while the bindings it opened stay owned.
fn drop_temporaries(owned: &mut Owned, first: usize, cx: &mut Cx) -> Vec<TirStmt> {
    let lives: Vec<Live> = owned[first..]
        .iter_mut()
        .rev()
        .filter_map(|slot| slot.take_if(|live| live.temporary))
        .collect();
    lives.iter().flat_map(|live| drop_one(live, cx)).collect()
}

/// Drop a value produced and discarded in statement position. Anything but a
/// local is spilled first: a structural drop reads its scrutinee once per
/// field, which would evaluate the expression again each time.
fn drop_discarded(value: TirExpr, cx: &mut Cx) -> Vec<TirStmt> {
    let type_id = value.type_id;
    if matches!(value.kind, TirExprKind::Local { .. }) {
        return drop_value(value, type_id, cx);
    }
    let (local, name) = cx.alloc_local(type_id, "discarded");
    let mut stmts = vec![let_stmt(&name, local, type_id, value)];
    stmts.extend(drop_value(local_ref(local, &name, type_id), type_id, cx));
    stmts
}

/// Build the statements that release every Component Model resource reachable
/// from `scrutinee` (a value of type `type_id`).
fn drop_value(scrutinee: TirExpr, type_id: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    let base = cx.tt.representation_head(type_id);
    match cx.tt.get(base).clone() {
        ResolvedType::Resource { def } if cx.tt.is_unrestricted_resource(def) => Vec::new(),
        ResolvedType::Resource { def } => match cx
            .reg
            .get_resource_cm_name_by_module(&cx.tt.def_module(def).to_string(), cx.tt.def_name(def))
        {
            Some(cm) => vec![expr_stmt(cm_canonical_call(
                CanonicalIntrinsic::ResourceDrop(CmDecl::new(cx.tt.defs(), def, cm)),
                vec![scrutinee],
                TypeTable::UNIT,
            ))],
            None => Vec::new(),
        },
        ResolvedType::Struct { def, type_args } => {
            let module_source = cx.tt.struct_head_module(def).clone();
            let fields = cx
                .struct_fields
                .get(&(cx.tt.struct_rendered_name(def, &type_args), module_source))
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
            } else if let Some((ok_ty, err_ty)) = cx.tt.as_result(type_id) {
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
        if !cx.carries_resource(*field_ty) {
            continue;
        }
        let field = common::field_access(
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
    let ok_has = cx.carries_resource(ok_ty);
    let err_has = cx.carries_resource(err_ty);
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
    let (case_name, case_index) = {
        let (_, _, name, index) = cx.tt.compiler_variant_case(case);
        (name.to_string(), index)
    };
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
            case_index,
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
// Pattern resource bindings
// ---------------------------------------------------------------------------

/// Whether matching `pattern` moves a resource out of the scrutinee.
fn pattern_extracts_resource(pattern: &TirPattern, cx: &Cx) -> bool {
    pattern_carries_resource(pattern, cx.tt, cx.reg, cx.struct_fields)
}

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
            if cx.carries_resource(*type_id) {
                out.push(Live {
                    local: *local_index,
                    name: name.clone(),
                    type_id: *type_id,
                    temporary: false,
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
        // An unrestricted handle is never dropped, so it owes no cleanup.
        TirPattern::Narrow { .. } => {}
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
        let is_tail = idx == last;
        let first_slot = owned.len();
        let first_out = out.len();
        flow = elab_stmt(stmt, owned, cx, is_tail, &mut out);
        if flow == Flow::Normal {
            let drops = drop_temporaries(owned, first_slot, cx);
            if drops.is_empty() {
                continue;
            }
            if is_tail {
                // The block's value is this statement's: keep it past the drops.
                let tail = out.split_off(first_out);
                out.extend(append_block_drops(tail, drops, cx));
            } else {
                out.extend(drops);
            }
        }
    }

    if flow == Flow::Normal {
        // Drop resources declared in this block, innermost (last) first.
        let drops = drop_slots(owned, entry, cx);
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
    let result_ty = tir::block_result_type(cx.tt, &inner);
    if result_ty == TypeTable::UNIT || result_ty == TypeTable::NEVER {
        // The block yields nothing observable; the drops can simply run last.
        let mut out = inner.stmts;
        out.extend(drops);
        return out;
    }
    // Preserve the value: `let $block_v = { <stmts> }; <drops>; $block_v`.
    let (v, vname) = cx.alloc_local(result_ty, "block_v");
    let inner_expr = TirExpr::new(TirExprKind::Block(inner), result_ty, span);
    let mut out = vec![let_stmt(&vname, v, result_ty, inner_expr)];
    out.extend(drops);
    out.push(expr_stmt(local_ref(v, &vname, result_ty)));
    out
}

/// Elaborate a block in place as a fresh nested scope.
fn elab_block(block: &mut TirBlock, owned: &mut Owned, cx: &mut Cx) -> Flow {
    let entry = owned.len();
    let stmts = std::mem::take(&mut block.stmts);
    block.stmts = elab_block_entry(stmts, owned, cx, entry);
    block_flow(&block.stmts)
}

/// [`elab_block`] for the body of a labeled block, which `break label` leaves.
fn elab_labeled_block(label: &str, block: &mut TirBlock, owned: &mut Owned, cx: &mut Cx) -> Flow {
    cx.targets.push(BreakTarget {
        label: Some(label.to_string()),
        entry: owned.len(),
    });
    let flow = elab_block(block, owned, cx);
    cx.targets.pop();
    flow
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
            if cx.carries_resource(type_id) {
                owned.push(Some(Live {
                    local: local_index,
                    name: name.clone(),
                    type_id,
                    temporary: false,
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

        TirStmtKind::LetDestructure { pattern, value } => {
            let value = elab_value_expr(value, owned, cx);
            for live in pattern_resources(&pattern, cx) {
                owned.push(Some(live));
            }
            out.push(TirStmt {
                kind: TirStmtKind::LetDestructure { pattern, value },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::Expr(expr) => {
            let elaborated = elab_value_expr(expr, owned, cx);
            if !is_tail && cx.carries_resource(elaborated.type_id) {
                out.extend(drop_discarded(elaborated, cx));
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
            let drops = drop_slots(owned, 0, cx);
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
                out.extend(drops);
                out.push(return_stmt(Some(local_ref(tmp, &tmp_name, ty))));
            } else {
                out.extend(drops);
                out.push(return_stmt(None));
            }
            Flow::Diverged
        }

        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let condition = elab_value_expr(condition, owned, cx);
            let (then_block, else_block, flow) =
                elab_if_branches(then_block, else_block, span, owned, cx);
            out.push(TirStmt {
                kind: TirStmtKind::If {
                    condition,
                    then_block,
                    else_block,
                },
                span,
            });
            flow
        }

        TirStmtKind::Loop { mut body } => {
            cx.targets.push(BreakTarget {
                label: None,
                entry: owned.len(),
            });
            let mut body_owned = owned.clone();
            elab_block(&mut body, &mut body_owned, cx);
            cx.targets.pop();
            // A resource of an enclosing scope transferred inside the loop
            // body is consumed (possibly every iteration); reflect that so it
            // is never dropped again after the loop.
            release_transferred(owned, &body_owned);
            out.push(TirStmt {
                kind: TirStmtKind::Loop { body },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::LabeledBlock { label, mut block } => {
            let flow = elab_labeled_block(&label, &mut block, owned, cx);
            out.push(TirStmt {
                kind: TirStmtKind::LabeledBlock { label, block },
                span,
            });
            flow
        }

        TirStmtKind::Break { label, value } => {
            let value = value.map(|v| elab_value_expr(v, owned, cx));
            // `break` skips the exit drops of every scope it leaves; emit
            // them here, innermost first.
            let target = cx.target_entry(label.as_deref());
            let drops = drop_slots(owned, target, cx);
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
            let target = cx.target_entry(None);
            out.extend(drop_slots(owned, target, cx));
            out.push(TirStmt {
                kind: TirStmtKind::Continue,
                span,
            });
            Flow::Diverged
        }

        TirStmtKind::VariadicForOf { .. } => {
            // Type-pack `for` expansion is resolved post-monomorphize; leave its
            // body untouched (it holds no resources in practice).
            let mut stmt = stmt;
            if let TirStmtKind::VariadicForOf { iterable, .. } = &mut stmt.kind {
                elab_expr(iterable, true, owned, cx);
            }
            out.push(stmt);
            Flow::Normal
        }
    }
}

/// Elaborate a branch block from the current ownership state. Returns the
/// rewritten block, its post-state (truncated back to the caller's slot
/// count) and its control flow.
fn elab_branch(mut block: TirBlock, base: &Owned, cx: &mut Cx) -> (TirBlock, Owned, Flow) {
    let mut branch_owned = base.clone();
    let flow = elab_block(&mut block, &mut branch_owned, cx);
    (block, branch_owned, flow)
}

/// Elaborate both branches of an `if`, statement or expression, and
/// reconcile ownership after it. Returns the branches and whether the whole
/// `if` diverges.
fn elab_if_branches(
    then_block: TirBlock,
    else_block: Option<TirBlock>,
    span: Span,
    owned: &mut Owned,
    cx: &mut Cx,
) -> (TirBlock, Option<TirBlock>, Flow) {
    let has_else = else_block.is_some();
    let (then_block, then_owned, then_flow) = elab_branch(then_block, owned, cx);
    let (else_stmts, else_owned, else_flow) = match else_block {
        Some(eb) => {
            let (b, o, f) = elab_branch(eb, owned, cx);
            (b.stmts, Some(o), f)
        }
        None => (Vec::new(), None, Flow::Normal),
    };
    let then_span = then_block.span;
    let mut then_stmts = then_block.stmts;
    let mut else_stmts = else_stmts;
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
    let flow = if then_flow == Flow::Diverged && has_else && else_flow == Flow::Diverged {
        Flow::Diverged
    } else {
        Flow::Normal
    };
    let else_block = (has_else || !else_stmts.is_empty()).then(|| TirBlock {
        stmts: else_stmts,
        span,
    });
    (
        TirBlock {
            stmts: then_stmts,
            span: then_span,
        },
        else_block,
        flow,
    )
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

/// Clear every slot of `owned` that `branch`, a clone of it elaborated along
/// one path, transferred. Counting a transfer on one path as one on every path
/// can leak on the others, but never drops twice.
fn release_transferred(owned: &mut Owned, branch: &Owned) {
    for (slot, after) in owned.iter_mut().zip(branch) {
        if after.is_none() {
            *slot = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Expression elaboration
// ---------------------------------------------------------------------------

/// Elaborate an expression whose value is transferred to its consumer: a
/// `let`, a `return`, an argument, a statement's own value.
fn elab_value_expr(mut expr: TirExpr, owned: &mut Owned, cx: &mut Cx) -> TirExpr {
    elab_expr(&mut expr, true, owned, cx);
    expr
}

/// Elaborate `expr` in place, in evaluation order: clear the slot of every
/// owned resource it transfers, spill every temporary it only borrows (see
/// [`spill_temporary`]), and elaborate each nested scope.
///
/// `consuming` is `true` when the position transfers ownership of the value
/// placed in it. A borrowing position (`&x`, a `&self` receiver, a field read,
/// an operator operand, a `matches` test) transfers nothing.
///
/// The match is exhaustive so a new `TirExprKind` cannot silently escape
/// ownership accounting: a missed transfer drops twice, a missed borrow leaks.
fn elab_expr(expr: &mut TirExpr, consuming: bool, owned: &mut Owned, cx: &mut Cx) {
    if !consuming && is_temporary(&expr.kind) && cx.carries_resource(expr.type_id) {
        elab_expr(expr, true, owned, cx);
        spill_temporary(expr, owned, cx);
        return;
    }
    let type_id = expr.type_id;
    let span = expr.span;
    match &mut expr.kind {
        TirExprKind::Local { index, .. } => {
            if consuming {
                release_local(owned, *index);
            }
        }

        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } => elab_expr(inner, false, owned, cx),
        // Casts and projections keep the surrounding ownership context:
        // `&(h as Fields)` must still see `h` as borrowed.
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TupleLen { expr: inner }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            elab_expr(inner, consuming, owned, cx);
        }
        // Reading a field that owns nothing only borrows the aggregate.
        TirExprKind::FieldAccess { expr: inner, .. } => {
            let moves_out = consuming && cx.carries_resource(type_id);
            elab_expr(inner, moves_out, owned, cx);
        }
        // Tag inspection / `matches` test: reads the discriminant only.
        TirExprKind::VariantTag { expr: inner } | TirExprKind::VariantTest { expr: inner, .. } => {
            elab_expr(inner, false, owned, cx);
        }

        TirExprKind::Call { func, args, .. } => {
            // The receiver is transferred exactly when the method takes
            // `self` by value. Extraction (`Result::unwrap`) is such a
            // by-value method, so it is covered here with no aggregate-shape
            // guessing.
            let receiver_consumed = func
                .method_info
                .as_ref()
                .is_some_and(|info| cx.owned_self.contains(&info.base_dispatch_key()));
            let (receiver, rest) = args.split_mut();
            if let Some(receiver) = receiver {
                // `&self` methods auto-reference the receiver; look through
                // that `&` to reach the underlying value.
                let target = match &mut receiver.expr.kind {
                    TirExprKind::Unary {
                        op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                        expr,
                    } => expr.as_mut(),
                    _ => &mut receiver.expr,
                };
                elab_expr(target, receiver_consumed, owned, cx);
            }
            for arg in rest {
                elab_expr(&mut arg.expr, true, owned, cx);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                elab_expr(arg, true, owned, cx);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            elab_expr(callee, true, owned, cx);
            for arg in args {
                elab_expr(arg, true, owned, cx);
            }
        }

        // Every operator takes its operands by reference.
        TirExprKind::Binary { op, left, right } => {
            elab_expr(left, false, owned, cx);
            if matches!(op, TirBinaryOp::And | TirBinaryOp::Or) {
                elab_conditional_expr(right, owned, cx);
            } else {
                elab_expr(right, false, owned, cx);
            }
        }
        TirExprKind::Assign { target, value } => {
            elab_expr(target, false, owned, cx);
            elab_expr(value, true, owned, cx);
        }
        TirExprKind::Index { expr: base, index } => {
            elab_expr(base, consuming, owned, cx);
            elab_expr(index, true, owned, cx);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            elab_expr(value, true, owned, cx);
        }

        TirExprKind::Block(block) => {
            elab_block(block, owned, cx);
        }
        TirExprKind::LabeledBlock { label, block, .. } => {
            elab_labeled_block(label, block, owned, cx);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            elab_expr(condition, true, owned, cx);
            let then_taken = std::mem::replace(then_branch, TirBlock::empty(span));
            let (then_block, else_block, _flow) =
                elab_if_branches(then_taken, else_branch.take(), span, owned, cx);
            *then_branch = then_block;
            *else_branch = else_block;
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => elab_match(scrutinee, arms, owned, cx),
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                elab_expr(&mut binding.handler, true, owned, cx);
            }
            elab_block(body, owned, cx);
        }

        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                elab_expr(&mut field.value, true, owned, cx);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                elab_expr(elem, true, owned, cx);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload) = payload {
                elab_expr(payload, true, owned, cx);
            }
        }
        // Expanded once per pack element, so each expansion is a scope.
        TirExprKind::TypePackExpansion { call_expr, .. } => {
            elab_conditional_expr(call_expr, owned, cx);
        }
        TirExprKind::VariadicTupleComprehension { iterable, body, .. } => {
            elab_expr(iterable, true, owned, cx);
            elab_conditional_expr(body, owned, cx);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    elab_expr(inner, true, owned, cx);
                }
            }
        }
        TirExprKind::Resume { value } => {
            elab_expr(value, true, owned, cx);
        }
        TirExprKind::GlobalVarGet { .. } => {}

        // A closure body addresses captured values through `Capture`, not the
        // enclosing function's `Local`s, so its locals belong to a different
        // index space and must not be elaborated here.
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

/// Whether evaluating `kind` produces a fresh value rather than reading a
/// place. Borrowing such a value leaves it with no owner but the statement.
fn is_temporary(kind: &TirExprKind) -> bool {
    matches!(
        kind,
        TirExprKind::Call { .. }
            | TirExprKind::CmRawCall { .. }
            | TirExprKind::IndirectCall { .. }
            | TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::ArrayLiteral { .. }
            | TirExprKind::VariantConstruct { .. }
            | TirExprKind::Block(_)
            | TirExprKind::LabeledBlock { .. }
            | TirExprKind::If { .. }
            | TirExprKind::Match { .. }
            | TirExprKind::WithHandler { .. }
    )
}

/// Move the borrowed temporary `expr` into a fresh local, in place and so in
/// evaluation order (`{ let $temp = expr; $temp }`), owned until the statement
/// holding it ends.
fn spill_temporary(expr: &mut TirExpr, owned: &mut Owned, cx: &mut Cx) {
    let type_id = expr.type_id;
    let span = expr.span;
    let (local, name) = cx.alloc_local(type_id, "temp");
    let value = std::mem::replace(expr, TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span));
    let stmts = vec![
        let_stmt(&name, local, type_id, value),
        expr_stmt(local_ref(local, &name, type_id)),
    ];
    *expr = TirExpr::new(TirExprKind::Block(TirBlock::new(stmts, span)), type_id, span);
    owned.push(Some(Live {
        local,
        name,
        type_id,
        temporary: true,
    }));
}

fn release_local(owned: &mut Owned, local: u32) {
    for slot in owned.iter_mut() {
        if slot.as_ref().is_some_and(|live| live.local == local) {
            *slot = None;
        }
    }
}

/// Elaborate `expr` as a scope of its own starting at slot `entry`: whatever
/// it leaves owned from `entry` up (its temporaries, and any bindings the
/// caller opened the scope with) drops once its value is computed.
fn elab_expr_scope(expr: &mut TirExpr, owned: &mut Owned, entry: usize, cx: &mut Cx) {
    elab_expr(expr, true, owned, cx);
    let drops = drop_slots(owned, entry, cx);
    owned.truncate(entry);
    if drops.is_empty() {
        return;
    }
    let type_id = expr.type_id;
    let span = expr.span;
    let value = std::mem::replace(expr, TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span));
    let stmts = append_block_drops(vec![expr_stmt(value)], drops, cx);
    *expr = TirExpr::new(TirExprKind::Block(TirBlock::new(stmts, span)), type_id, span);
}

/// Elaborate an expression that runs on some paths only, or more than once,
/// as a scope of its own (see [`release_transferred`]).
fn elab_conditional_expr(expr: &mut TirExpr, owned: &mut Owned, cx: &mut Cx) {
    let mut branch = owned.clone();
    elab_expr_scope(expr, &mut branch, owned.len(), cx);
    release_transferred(owned, &branch);
}

/// Elaborate a `match`: its scrutinee, then each arm from the ownership state
/// after it, then merge the arms' post-states.
fn elab_match(
    scrutinee: &mut TirExpr,
    arms: &mut [TirMatchArm],
    owned: &mut Owned,
    cx: &mut Cx,
) {
    // The scrutinee is consumed only if some arm destructures a resource out
    // of it; a pure `matches`-style test leaves the scrutinee — and its drop
    // obligation — intact.
    let extracts = arms
        .iter()
        .any(|arm| pattern_extracts_resource(&arm.pattern, cx));
    elab_expr(scrutinee, extracts, owned, cx);
    let base = owned.clone();
    let mut arm_states: Vec<(Owned, Flow)> = Vec::with_capacity(arms.len());
    for arm in arms.iter_mut() {
        let mut arm_owned = base.clone();
        arm_owned.extend(pattern_resources(&arm.pattern, cx).into_iter().map(Some));
        if let Some(guard) = arm.guard.as_mut() {
            let entry = arm_owned.len();
            elab_expr_scope(guard, &mut arm_owned, entry, cx);
        }
        let body = std::mem::replace(
            &mut arm.body,
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
        );
        let (body, flow) = elab_arm_body(body, &mut arm_owned, cx, base.len());
        arm.body = body;
        arm_owned.truncate(base.len());
        arm_states.push((arm_owned, flow));
    }
    // Merge: a resource is still owned only if every fall-through arm still
    // owns it; drop it at the end of the fall-through arms that do when
    // another consumed it.
    for (slot, live) in base.iter().enumerate() {
        let Some(live) = live else {
            continue;
        };
        let mut falling_through = arm_states
            .iter()
            .filter(|(_, flow)| *flow == Flow::Normal)
            .peekable();
        if falling_through.peek().is_none()
            || falling_through.all(|(arm_owned, _)| arm_owned[slot].is_some())
        {
            continue;
        }
        for (arm, (arm_owned, flow)) in arms.iter_mut().zip(&arm_states) {
            if *flow == Flow::Normal && arm_owned[slot].is_some() {
                append_arm_drop(arm, live, cx);
            }
        }
        owned[slot] = None;
    }
}

/// Elaborate a `match` arm body as a scope starting at slot `entry`, which
/// holds the arm's pattern bindings.
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
            let mut expr = TirExpr {
                kind: other,
                type_id,
                span,
            };
            elab_expr_scope(&mut expr, owned, entry, cx);
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
