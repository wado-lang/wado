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
use crate::defs::DefId;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::name::minted_name;
use crate::package::Package;
use crate::synthesis::common;
use crate::synthesis::common::{cm_canonical_call, expr_stmt, let_stmt, local_ref, synth_span};
use crate::tir::{
    FunctionRef, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal,
    TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirTemplatePart, TirUnaryOp, TypeId, TypeTable,
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
    resources: Resources<'a>,
    callees: &'a CalleeFacts,
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
        self.resources.carried_by(type_id)
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

/// What a type holds that this pass drops, one level down.
enum Layout<'a> {
    /// A resource, released by `resource.drop` under this CM name.
    Resource(DefId, &'a str),
    /// A struct, dropped field by field: `(index, name, type_id)`.
    Struct(&'a [(u32, String, TypeId)]),
    /// A tuple, dropped element by element.
    Tuple(Vec<TypeId>),
    /// A `Result`, dropped by a `match` on its case: `(ok, err)`.
    Result(TypeId, TypeId),
    /// Nothing this pass drops.
    Opaque,
}

/// The tables that tell which types carry a resource this pass drops.
#[derive(Clone, Copy)]
struct Resources<'a> {
    tt: &'a TypeTable,
    reg: &'a CmInterfaceRegistry,
    struct_fields: &'a StructFieldReg,
}

impl<'a> Resources<'a> {
    /// Whether `type_id` owns, or structurally carries, a Component Model
    /// resource that this pass knows how to drop: a bare resource, or a struct
    /// / tuple / `Result` that transitively contains one.
    fn carried_by(&self, type_id: TypeId) -> bool {
        self.carried_by_rec(type_id, &mut Vec::new())
    }

    fn carried_by_rec(&self, type_id: TypeId, visited: &mut Vec<TypeId>) -> bool {
        let base = self.tt.representation_head(type_id);
        if visited.contains(&base) {
            return false;
        }
        visited.push(base);
        match self.layout(base) {
            Layout::Resource(..) => true,
            Layout::Struct(fields) => fields
                .iter()
                .any(|(_, _, field_ty)| self.carried_by_rec(*field_ty, visited)),
            Layout::Tuple(elems) => elems
                .into_iter()
                .any(|elem_ty| self.carried_by_rec(elem_ty, visited)),
            Layout::Result(ok, err) => {
                self.carried_by_rec(ok, visited) || self.carried_by_rec(err, visited)
            }
            Layout::Opaque => false,
        }
    }

    /// The [`Layout`] of `type_id`.
    ///
    /// `GenericResource` (`Future` / `Stream`) is opaque: those handles have
    /// their own explicit drop discipline and must not be touched here. So is
    /// a reference — a borrowed place owns nothing — and an unrestricted
    /// resource, which is never dropped.
    fn layout(&self, type_id: TypeId) -> Layout<'a> {
        let tt = self.tt;
        let base = tt.representation_head(type_id);
        match tt.get(base) {
            ResolvedType::Resource { def } if !tt.is_unrestricted_resource(*def) => {
                match self.reg.get_resource_cm_name_by_module(
                    &tt.def_module(*def).to_string(),
                    tt.def_name(*def),
                ) {
                    Some(cm) => Layout::Resource(*def, cm),
                    None => Layout::Opaque,
                }
            }
            ResolvedType::Struct { def, type_args } => {
                let key = (
                    tt.struct_rendered_name(*def, type_args),
                    tt.struct_head_module(*def).clone(),
                );
                match self.struct_fields.get(&key) {
                    Some(fields) => Layout::Struct(fields),
                    None => Layout::Opaque,
                }
            }
            _ => {
                if let Some(elems) = tt.as_tuple(base) {
                    Layout::Tuple(elems)
                } else if let Some((ok, err)) = tt.as_result(base) {
                    Layout::Result(ok, err)
                } else {
                    Layout::Opaque
                }
            }
        }
    }

    /// The resource-carrying values `pattern` binds (e.g. `response` in
    /// `let [response, _tx] = Response::new(...)`).
    fn pattern_bindings(&self, pattern: &TirPattern) -> Vec<Live> {
        let mut collector = PatternBindings {
            resources: *self,
            found: Vec::new(),
        };
        collector.visit_pattern(pattern);
        collector.found
    }
}

struct PatternBindings<'a> {
    resources: Resources<'a>,
    found: Vec<Live>,
}

impl TirRefVisitor for PatternBindings<'_> {
    fn visit_pattern(&mut self, pattern: &TirPattern) {
        match pattern {
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                if self.resources.carried_by(*type_id) {
                    self.found.push(Live {
                        local: *local_index,
                        name: name.clone(),
                        type_id: *type_id,
                        temporary: false,
                    });
                }
            }
            // Reify binds every alternative to the first one's locals, so the
            // first names each value once.
            TirPattern::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    self.visit_pattern(first);
                }
            }
            // An unrestricted handle is never dropped, so it owes no cleanup.
            TirPattern::Narrow { .. } => {}
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Tuple(..)
            | TirPattern::Variant { .. }
            | TirPattern::Enum { .. }
            | TirPattern::Struct { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => self.walk_pattern(pattern),
        }
    }
}

/// What a call reveals about ownership, by callee. Each set holds
/// [`callee_key`]s, which a generic function shares with its instantiations.
#[derive(Default)]
struct CalleeFacts {
    /// Instance methods whose `self` is taken by value. Calling one transfers
    /// ownership of the receiver (e.g. `Result::unwrap`, which moves the
    /// wrapped value out).
    owned_self: IndexSet<String>,
    /// Functions that borrow a parameter and return a type parameter, such as
    /// `List::index_value`. The move check cannot see a generic body return a
    /// resource out of its borrow, so the result may alias the borrowed
    /// storage (WEP 2026-05-21, "No move out of a borrow"), and it is no
    /// temporary of the caller's.
    alias_returning: IndexSet<String>,
}

impl CalleeFacts {
    fn record(&mut self, func: &TirFunction, tt: &TypeTable) {
        let is_ref = |type_id| {
            matches!(
                tt.get(type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        };
        let key = callee_key(&FunctionRef::from_resolved(
            func,
            func.module_source.clone(),
        ));
        if func.method_info.is_some() && func.takes_self() && !is_ref(func.params[0].type_id) {
            self.owned_self.insert(key.clone());
        }
        if func.params.iter().any(|param| is_ref(param.type_id))
            && tt.contains_type_param(func.return_type)
        {
            self.alias_returning.insert(key);
        }
    }
}

/// The key [`CalleeFacts`] files a callee under: a method's
/// [`base_dispatch_key`](crate::name::LocalMethodName::base_dispatch_key), or
/// a free function's full name.
fn callee_key(func: &FunctionRef) -> String {
    match &func.method_info {
        Some(info) => info.base_dispatch_key(),
        None => func.full_name(),
    }
}

/// Entry point: elaborate resource drops for every function in the project.
pub fn elaborate_resource_drops(project: &mut Package) {
    let struct_fields = build_struct_field_reg(project);
    let Some(type_table) = project
        .tir_modules
        .values()
        .next()
        .map(|m| m.type_table.clone())
    else {
        return;
    };
    let tt = type_table.borrow();
    let resources = Resources {
        tt: &tt,
        reg: &project.cm_interface_registry,
        struct_fields: &struct_fields,
    };

    let mut callees = CalleeFacts::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            callees.record(&func_rc.borrow(), &tt);
        }
    }

    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            elaborate_function(&mut func_rc.borrow_mut(), resources, &callees);
        }
    }
}

/// Elaborate resource drops for a single function body.
fn elaborate_function(func: &mut TirFunction, resources: Resources, callees: &CalleeFacts) {
    let TirFunction {
        body: Some(body),
        params,
        locals,
        local_count,
        ..
    } = func
    else {
        return;
    };

    // Parameters that own (or carry) a resource are live from entry. A
    // by-value `self` is just such a parameter: the method that received it
    // owns it and must drop it unless its body transfers it onward.
    let mut owned: Owned = params
        .iter()
        .filter(|param| resources.carried_by(param.type_id))
        .map(|param| {
            Some(Live {
                local: param.local_index,
                name: param.name.clone(),
                type_id: param.type_id,
                temporary: false,
            })
        })
        .collect();
    if owned.is_empty() && !body_has_resource(body, resources) {
        return;
    }

    let mut cx = Cx {
        resources,
        callees,
        locals,
        local_count,
        targets: Vec::new(),
    };
    // Parameters count as declared in the body's scope, so they drop at its
    // end when never transferred.
    elab_block_at(body, &mut owned, &mut cx, 0);
}

/// Cheap pre-check: does the body produce or bind any resource-carrying
/// value? Lets the pass skip the (vast majority of) functions that touch no
/// resources.
fn body_has_resource(block: &TirBlock, resources: Resources) -> bool {
    let mut probe = ResourceProbe {
        resources,
        found: false,
    };
    probe.visit_block(block);
    probe.found
}

struct ResourceProbe<'a> {
    resources: Resources<'a>,
    found: bool,
}

impl TirRefVisitor for ResourceProbe<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.found {
            return;
        }
        if self.resources.carried_by(expr.type_id) {
            self.found = true;
            return;
        }
        self.walk_expr_in_frame(expr);
    }

    fn visit_pattern(&mut self, pattern: &TirPattern) {
        if !self.resources.pattern_bindings(pattern).is_empty() {
            self.found = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Drop-statement synthesis
// ---------------------------------------------------------------------------

/// Build the statements that drop `lives`, in order.
fn drop_all<'l>(lives: impl IntoIterator<Item = &'l Live>, cx: &mut Cx) -> Vec<TirStmt> {
    lives
        .into_iter()
        .flat_map(|live| {
            let value = local_ref(live.local, &live.name, live.type_id);
            drop_value(value, live.type_id, cx)
        })
        .collect()
}

/// Take every value still owned in the slots from `first` up, and build the
/// statements that drop them, innermost first.
fn drop_slots(owned: &mut Owned, first: usize, cx: &mut Cx) -> Vec<TirStmt> {
    let lives: Vec<Live> = owned[first..].iter_mut().filter_map(Option::take).collect();
    drop_all(lives.iter().rev(), cx)
}

/// Take the temporaries still owned in `slots`, in slot order.
fn take_temporaries(slots: &mut [Option<Live>]) -> Vec<Live> {
    slots
        .iter_mut()
        .filter_map(|slot| slot.take_if(|live| live.temporary))
        .collect()
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
    match cx.resources.layout(type_id) {
        Layout::Resource(def, cm) => vec![expr_stmt(cm_canonical_call(
            CanonicalIntrinsic::ResourceDrop(CmDecl::new(cx.resources.tt.defs(), def, cm)),
            vec![scrutinee],
            TypeTable::UNIT,
        ))],
        Layout::Struct(fields) => drop_projected(scrutinee, fields, cx),
        Layout::Tuple(elems) => {
            let fields: Vec<(u32, String, TypeId)> = elems
                .into_iter()
                .enumerate()
                .map(|(i, ty)| (i as u32, i.to_string(), ty))
                .collect();
            drop_projected(scrutinee, &fields, cx)
        }
        Layout::Result(ok_ty, err_ty) => drop_result(scrutinee, type_id, ok_ty, err_ty, cx),
        Layout::Opaque => Vec::new(),
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
        let (_, _, name, index) = cx.resources.tt.compiler_variant_case(case);
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
// Block / statement elaboration
// ---------------------------------------------------------------------------

/// Which statement a temporary spilled inside a block belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TemporaryScope {
    /// The block's own statement that spilled it.
    Statement,
    /// The statement holding the block: see [`elab_plain_block`].
    Enclosing,
}

/// Elaborate a block's statements as a scope starting at slot `entry`:
/// resources in slots `>= entry` are declared here and dropped at block exit;
/// resources in slots `< entry` belong to an enclosing scope. The temporaries
/// its statements spill drop where `temporaries` says.
fn elab_scope_stmts(
    stmts: Vec<TirStmt>,
    owned: &mut Owned,
    cx: &mut Cx,
    entry: usize,
    temporaries: TemporaryScope,
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
        if flow == Flow::Normal && temporaries == TemporaryScope::Statement {
            let spilled = take_temporaries(&mut owned[first_slot..]);
            let drops = drop_all(spilled.iter().rev(), cx);
            if is_tail {
                // The block's value is this statement's: keep it past the drops.
                let tail = out.split_off(first_out);
                out.extend(append_block_drops(tail, drops, cx));
            } else {
                out.extend(drops);
            }
        }
    }

    let deferred = match temporaries {
        TemporaryScope::Statement => Vec::new(),
        TemporaryScope::Enclosing => take_temporaries(&mut owned[entry..]),
    };
    if flow == Flow::Normal {
        // Drop resources declared in this block, innermost (last) first.
        let drops = drop_slots(owned, entry, cx);
        out = append_block_drops(out, drops, cx);
    }
    // Block-local slots leave scope; enclosing slots (with any transfers
    // applied within this block) remain visible to the caller.
    owned.truncate(entry);
    owned.extend(deferred.into_iter().map(Some));
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
    if drops.is_empty() {
        return stmts;
    }
    let span = synth_span();
    let inner = TirBlock { stmts, span };
    let result_ty = tir::block_result_type(cx.resources.tt, &inner);
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

/// [`append_block_drops`] for an expression, which keeps its value.
fn append_expr_drops(expr: &mut TirExpr, drops: Vec<TirStmt>, cx: &mut Cx) {
    if drops.is_empty() {
        return;
    }
    if let TirExprKind::Block(block) = &mut expr.kind {
        let stmts = std::mem::take(&mut block.stmts);
        block.stmts = append_block_drops(stmts, drops, cx);
    } else {
        wrap_in_block(expr, |value| {
            append_block_drops(vec![expr_stmt(value)], drops, cx)
        });
    }
}

/// Elaborate `block` in place as a scope starting at slot `entry`; see
/// [`elab_scope_stmts`].
fn elab_block_at(block: &mut TirBlock, owned: &mut Owned, cx: &mut Cx, entry: usize) -> Flow {
    let stmts = std::mem::take(&mut block.stmts);
    block.stmts = elab_scope_stmts(stmts, owned, cx, entry, TemporaryScope::Statement);
    block_flow(&block.stmts)
}

/// Elaborate a block in place as a fresh nested scope.
fn elab_block(block: &mut TirBlock, owned: &mut Owned, cx: &mut Cx) -> Flow {
    let entry = owned.len();
    elab_block_at(block, owned, cx, entry)
}

/// Elaborate a plain block expression in place. Nothing can leave it but a
/// `return` or a `break` past it, so it runs whole as part of the statement
/// holding it, and its temporaries drop when that statement ends. Synthesized
/// code relies on that: an `assert` captures its operand `f(&t())` as
/// `f({ $v = &t(); $v })`, so `t()` must outlive the capture's statement.
fn elab_plain_block(block: &mut TirBlock, owned: &mut Owned, cx: &mut Cx) {
    let entry = owned.len();
    let stmts = std::mem::take(&mut block.stmts);
    block.stmts = elab_scope_stmts(stmts, owned, cx, entry, TemporaryScope::Enclosing);
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
            owned.extend(
                cx.resources
                    .pattern_bindings(&pattern)
                    .into_iter()
                    .map(Some),
            );
            out.push(TirStmt {
                kind: TirStmtKind::LetDestructure { pattern, value },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::Expr(expr) => {
            let elaborated = elab_value_expr(expr, owned, cx);
            // An assignment is typed as the value it stores, which its target
            // now owns: nothing is discarded.
            let discards = !matches!(elaborated.kind, TirExprKind::Assign { .. });
            if !is_tail && discards && cx.carries_resource(elaborated.type_id) {
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
            // function.
            let drops = drop_slots(owned, 0, cx);
            leave_after_drops(value, drops, span, cx, out, |value| TirStmtKind::Return {
                value,
            })
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
            // `break` skips the exit drops of every scope it leaves.
            let target = cx.target_entry(label.as_deref());
            let drops = drop_slots(owned, target, cx);
            leave_after_drops(value, drops, span, cx, out, |value| TirStmtKind::Break {
                label,
                value,
            })
        }

        TirStmtKind::Continue => {
            let target = cx.target_entry(None);
            let drops = drop_slots(owned, target, cx);
            leave_after_drops(None, drops, span, cx, out, |_| TirStmtKind::Continue)
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

/// Emit `drops`, then the statement `leave` builds around `value`. With drops
/// to run, `value` is spilled ahead of them: it may borrow a resource they
/// release.
fn leave_after_drops(
    value: Option<TirExpr>,
    drops: Vec<TirStmt>,
    span: Span,
    cx: &mut Cx,
    out: &mut Vec<TirStmt>,
    leave: impl FnOnce(Option<TirExpr>) -> TirStmtKind,
) -> Flow {
    let value = match value {
        Some(value) if !drops.is_empty() => {
            let ty = value.type_id;
            let (tmp, tmp_name) = cx.alloc_local(ty, "drop_spill");
            out.push(let_stmt(&tmp_name, tmp, ty, value));
            Some(local_ref(tmp, &tmp_name, ty))
        }
        value => value,
    };
    out.extend(drops);
    out.push(TirStmt {
        kind: leave(value),
        span,
    });
    Flow::Diverged
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
/// reconcile ownership after it. A missing `else` is elaborated as an empty
/// one, which gains statements only when it must drop what the `then` branch
/// consumed. Returns the branches and whether the whole `if` diverges.
fn elab_if_branches(
    then_block: TirBlock,
    else_block: Option<TirBlock>,
    span: Span,
    owned: &mut Owned,
    cx: &mut Cx,
) -> (TirBlock, Option<TirBlock>, Flow) {
    let has_else = else_block.is_some();
    let else_block = else_block.unwrap_or_else(|| TirBlock::empty(span));
    let (mut then_block, then_owned, then_flow) = elab_branch(then_block, owned, cx);
    let (mut else_block, else_owned, else_flow) = elab_branch(else_block, owned, cx);
    let drops = merge_paths(owned, &[(then_owned, then_flow), (else_owned, else_flow)]);
    for (block, lives) in [&mut then_block, &mut else_block].into_iter().zip(drops) {
        let stmts = std::mem::take(&mut block.stmts);
        let drops = drop_all(&lives, cx);
        block.stmts = append_block_drops(stmts, drops, cx);
    }
    let flow = if then_flow == Flow::Diverged && else_flow == Flow::Diverged {
        Flow::Diverged
    } else {
        Flow::Normal
    };
    let else_block = (has_else || !else_block.stmts.is_empty()).then_some(else_block);
    (then_block, else_block, flow)
}

/// Merge the post-states of the paths out of a branch, each `(owned, flow)`
/// elaborated from a clone of `owned`. A resource stays owned only where every
/// path that falls through still owns it. Where one consumed it, the others
/// still owning it drop it at their end: returns, per path, what it must drop.
fn merge_paths(owned: &mut Owned, paths: &[(Owned, Flow)]) -> Vec<Vec<Live>> {
    let mut drops = vec![Vec::new(); paths.len()];
    let falling_through: Vec<usize> = paths
        .iter()
        .enumerate()
        .filter(|(_, (_, flow))| *flow == Flow::Normal)
        .map(|(path, _)| path)
        .collect();
    for (slot, state) in owned.iter_mut().enumerate() {
        let Some(live) = state.as_ref() else {
            continue;
        };
        let owners: Vec<usize> = falling_through
            .iter()
            .copied()
            .filter(|&path| paths[path].0[slot].is_some())
            .collect();
        if falling_through.is_empty() || owners.len() == falling_through.len() {
            continue;
        }
        for path in owners {
            drops[path].push(live.clone());
        }
        *state = None;
    }
    drops
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
    if !consuming && is_temporary(&expr.kind, cx) && cx.carries_resource(expr.type_id) {
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
            // `self` by value.
            let receiver_consumed = cx.callees.owned_self.contains(&callee_key(func));
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

        TirExprKind::Block(block) => elab_plain_block(block, owned, cx),
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
fn is_temporary(kind: &TirExprKind, cx: &Cx) -> bool {
    if let TirExprKind::Call { func, .. } = kind {
        return !cx.callees.alias_returning.contains(&callee_key(func));
    }
    matches!(
        kind,
        TirExprKind::CmRawCall { .. }
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
    let (local, name) = cx.alloc_local(type_id, "temp");
    wrap_in_block(expr, |value| {
        vec![
            let_stmt(&name, local, type_id, value),
            expr_stmt(local_ref(local, &name, type_id)),
        ]
    });
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
    append_expr_drops(expr, drops, cx);
}

/// Replace `expr` with a block of the same type, built from its value.
fn wrap_in_block(expr: &mut TirExpr, build: impl FnOnce(TirExpr) -> Vec<TirStmt>) {
    let type_id = expr.type_id;
    let span = expr.span;
    let value = std::mem::replace(expr, TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span));
    *expr = TirExpr::new(
        TirExprKind::Block(TirBlock::new(build(value), span)),
        type_id,
        span,
    );
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
fn elab_match(scrutinee: &mut TirExpr, arms: &mut [TirMatchArm], owned: &mut Owned, cx: &mut Cx) {
    let bindings: Vec<Vec<Live>> = arms
        .iter()
        .map(|arm| cx.resources.pattern_bindings(&arm.pattern))
        .collect();
    // The scrutinee is consumed only if some arm destructures a resource out
    // of it; a pure `matches`-style test leaves the scrutinee — and its drop
    // obligation — intact.
    let extracts = bindings.iter().any(|arm_bindings| !arm_bindings.is_empty());
    elab_expr(scrutinee, extracts, owned, cx);
    let entry = owned.len();
    let mut arm_states: Vec<(Owned, Flow)> = Vec::with_capacity(arms.len());
    for (arm, arm_bindings) in arms.iter_mut().zip(bindings) {
        let mut arm_owned = owned.clone();
        arm_owned.extend(arm_bindings.into_iter().map(Some));
        if let Some(guard) = arm.guard.as_mut() {
            let guard_entry = arm_owned.len();
            elab_expr_scope(guard, &mut arm_owned, guard_entry, cx);
        }
        let flow = elab_arm_body(&mut arm.body, &mut arm_owned, cx, entry);
        arm_states.push((arm_owned, flow));
    }
    for (arm, lives) in arms.iter_mut().zip(merge_paths(owned, &arm_states)) {
        let drops = drop_all(&lives, cx);
        append_expr_drops(&mut arm.body, drops, cx);
    }
}

/// Elaborate a `match` arm body in place as a scope starting at slot `entry`,
/// which holds the arm's pattern bindings.
fn elab_arm_body(body: &mut TirExpr, owned: &mut Owned, cx: &mut Cx, entry: usize) -> Flow {
    if let TirExprKind::Block(block) = &mut body.kind {
        return elab_block_at(block, owned, cx, entry);
    }
    elab_expr_scope(body, owned, entry, cx);
    Flow::Normal
}
