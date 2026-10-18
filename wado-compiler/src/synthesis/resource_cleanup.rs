//! Resource drop elaboration (WEP: Affine Types). An affine value is an
//! object, and a place holding one is a nullable reference to it. A move writes
//! null into the place it moved out of, and scope exit drops every affine local
//! that is not null, innermost scope first and in reverse declaration order
//! within one. A `return`, `break` or `continue` drops the same way each scope
//! it leaves. No path is analysed: a local that a path moved is null on that
//! path. A temporary that is only referenced has no other owner, so it is
//! spilled into a local that drops when its statement ends.

use std::cell::RefCell;
use std::rc::Rc;

use crate::canonical::{CanonicalIntrinsic, CmDecl};
use crate::compiler_host::Code;
use crate::component_model::CmInterfaceRegistry;
use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{drop_glue_name, minted_name};
use crate::package::Package;
use crate::resource_holding::{Held, HeldTypes, holds_resource};
use crate::synthesis::SynthesisError;
use crate::synthesis::cm_binding::end_drop_call;
use crate::synthesis::common;
use crate::synthesis::common::{
    assign, builtin_call, cm_canonical_call, expr_stmt, if_stmt, let_stmt, local_ref, not_expr,
    synth_span,
};
use crate::tir;
use crate::tir::{
    FunctionRef, ResolvedType, StructDef, StructFields, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirTemplatePart, TirUnaryOp, TypeId, TypeKey, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;

/// An affine local a scope declared.
#[derive(Clone)]
struct Live {
    local: u32,
    name: String,
    type_id: TypeId,
    /// A referenced temporary spilled by [`spill_temporary`], owned by its
    /// statement rather than by the enclosing block.
    temporary: bool,
}

impl Live {
    fn place(&self) -> TirExpr {
        local_ref(self.local, &self.name, self.type_id)
    }
}

/// Whether a statement sequence falls through to its successor.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Normal,
    Diverged,
}

impl Flow {
    /// The flow where two paths meet: diverged only when both diverge.
    fn join(self, other: Flow) -> Flow {
        if self == Flow::Diverged && other == Flow::Diverged {
            Flow::Diverged
        } else {
            Flow::Normal
        }
    }
}

/// Where a `break` or `continue` lands: a labeled block, or a loop
/// (`label: None`). Leaving for it drops every scope from `depth` up.
struct BreakTarget {
    label: Option<String>,
    depth: usize,
}

struct Cx<'a> {
    resources: Resources<'a>,
    callees: &'a CalleeFacts,
    /// The frame's local slots after its first `local_base`: a closure's
    /// parameters precede its body locals and are not listed.
    locals: &'a mut Vec<TirLocal>,
    local_base: u32,
    /// The function's local count, kept in step; a closure keeps none.
    local_count: Option<&'a mut u32>,
    /// The affine locals each open scope declared, outermost first.
    scopes: Vec<Vec<Live>>,
    /// The enclosing loops and labeled blocks, innermost last.
    targets: Vec<BreakTarget>,
    /// The holders whose drop is being expanded, outermost first.
    expanding: Vec<TypeKey>,
    glue: &'a mut DropGlue,
    /// The module of the function being elaborated.
    module: ModuleSource,
    refused: &'a mut Vec<SynthesisError>,
}

/// The drop glue functions called so far, by name, each with the type it
/// drops. They go in `entry`, the entry module.
struct DropGlue {
    entry: ModuleSource,
    called: IndexMap<String, TypeId>,
}

impl Cx<'_> {
    /// Allocate a fresh local slot, named after `what` it holds.
    fn alloc_local(&mut self, type_id: TypeId, what: &str) -> (u32, String) {
        let idx = self.local_base + self.locals.len() as u32;
        let name = minted_name(what, idx);
        self.locals.push(TirLocal {
            name: name.clone(),
            type_id,
            is_mut: false,
            span: Span::default(),
        });
        if let Some(count) = self.local_count.as_deref_mut() {
            *count = self.locals.len() as u32;
        }
        (idx, name)
    }

    fn carries_resource(&self, type_id: TypeId) -> bool {
        self.resources.carried_by(type_id)
    }

    /// Declare `live` in the innermost scope.
    fn declare(&mut self, live: Live) {
        self.scopes
            .last_mut()
            .expect("a function body opens a scope")
            .push(live);
    }

    /// Refuse a call at `span` that copies a `type_id` holding a resource out
    /// of a shared reference: the copy would be a second owner of its handle.
    fn refuse_copy(&mut self, type_id: TypeId, span: Span) {
        let type_name = self.resources.tt.borrow().type_name(type_id);
        self.refused.push(SynthesisError {
            code: Code::TypeMismatch,
            message: format!(
                "cannot copy resource `{type_name}` out of a reference; a call through `&` \
                 leaves it in place, so take it out through `&mut`, as `pop` or `remove` do"
            ),
            span: Some(span),
            module: Some(Box::new(self.module.clone())),
        });
    }

    /// How many scopes are open outside the target a `break label` (or, with
    /// `None`, a `break` or `continue`) lands on.
    fn target_depth(&self, label: Option<&str>) -> usize {
        self.targets
            .iter()
            .rev()
            .find(|target| target.label.as_deref() == label)
            .expect("a `break` or `continue` lands in an enclosing loop or labeled block")
            .depth
    }

    /// The statements that drop every local the scopes from `depth` up
    /// declared, innermost first.
    fn drop_scopes_from(&mut self, depth: usize) -> Vec<TirStmt> {
        let lives: Vec<Live> = self.scopes[depth..]
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev().cloned())
            .collect();
        drop_lives(&lives, self)
    }
}

/// What a type holds that this pass drops, one level down.
enum Layout<'a> {
    /// A resource, released by `resource.drop` under this CM name.
    Resource(DefId, &'a str),
    /// A stream or future end, released by its own canonical drop.
    End,
    /// An aggregate, instantiating `decl` with `args` where it has a
    /// declaration.
    Holder {
        decl: Option<DefId>,
        args: Vec<TypeId>,
        parts: Parts,
    },
    /// Nothing this pass drops.
    Opaque,
}

/// An aggregate's parts, typed at its instance.
enum Parts {
    /// A struct's fields or a tuple's elements, dropped one by one:
    /// `(index, name, type_id)`.
    Fields(Vec<(u32, String, TypeId)>),
    /// A variant's cases, dropped by a `match`: `(name, index, payload)`, a
    /// case without one carrying `()`.
    Cases(Vec<(String, u32, TypeId)>),
}

impl Parts {
    fn types(&self) -> Vec<TypeId> {
        match self {
            Parts::Fields(fields) => fields.iter().map(|(_, _, ty)| *ty).collect(),
            Parts::Cases(cases) => cases.iter().map(|(_, _, ty)| *ty).collect(),
        }
    }
}

/// The tables that tell which types carry a resource this pass drops.
#[derive(Clone, Copy)]
struct Resources<'a> {
    tt: &'a RefCell<TypeTable>,
    reg: &'a CmInterfaceRegistry,
    struct_fields: &'a StructFields,
    /// What [`Self::carried_by`] has answered, by type.
    answers: &'a RefCell<IndexMap<TypeKey, bool>>,
}

impl<'a> Resources<'a> {
    /// Whether `type_id` owns, or structurally carries, a Component Model
    /// resource that this pass knows how to drop: a bare resource, or a struct
    /// / tuple / variant that transitively contains one.
    fn carried_by(&self, type_id: TypeId) -> bool {
        let key = self.tt.borrow().type_key(type_id);
        if let Some(&carried) = self.answers.borrow().get(&key) {
            return carried;
        }
        let carried = holds_resource(self, type_id);
        self.answers.borrow_mut().insert(key, carried);
        carried
    }

    /// The [`Layout`] of `type_id`.
    ///
    /// A reference is opaque, since a referenced place owns nothing, and so is
    /// an unrestricted resource, which is never dropped.
    fn layout(&self, type_id: TypeId) -> Layout<'a> {
        let (base, resolved) = {
            let tt = self.tt.borrow();
            let base = tt.representation_head(type_id);
            (base, tt.get(base).clone())
        };
        if let ResolvedType::Resource { def } = resolved {
            let tt = self.tt.borrow();
            if tt.is_unrestricted_resource(def) {
                return Layout::Opaque;
            }
            return match self
                .reg
                .get_resource_cm_name_by_module(&tt.def_module(def).to_string(), tt.def_name(def))
            {
                Some(cm) => Layout::Resource(def, cm),
                None => Layout::Opaque,
            };
        }
        if let ResolvedType::GenericResource { .. } = resolved {
            return Layout::End;
        }
        let mut tt = self.tt.borrow_mut();
        if let Some(elems) = tt.as_tuple(base) {
            let fields = elems
                .into_iter()
                .enumerate()
                .map(|(i, ty)| (i as u32, i.to_string(), ty))
                .collect();
            return Layout::Holder {
                decl: None,
                args: Vec::new(),
                parts: Parts::Fields(fields),
            };
        }
        // A generic struct's instance is a `GenericInstance`, as a generic
        // variant's is.
        let (def, args) = match resolved {
            ResolvedType::Struct { def, type_args } => (def, type_args),
            _ => match tt.nominal_def(base) {
                Some(def) => (
                    StructDef::Decl(def),
                    tt.nominal_type_args(base).unwrap_or_default(),
                ),
                None => return Layout::Opaque,
            },
        };
        let decl = match def {
            StructDef::Decl(decl) => Some(decl),
            StructDef::Anon(_) => None,
        };
        let parts = if let Some(fields) = self.struct_fields.get(&(def, Vec::new())) {
            Parts::Fields(
                fields
                    .iter()
                    .map(|field| {
                        let ty = tt.substitute_positional(field.type_id, &args);
                        (field.index, field.name.clone(), ty)
                    })
                    .collect(),
            )
        } else if let Some(cases) = decl
            .and_then(|decl| tt.variant_template_cases(decl))
            .map(<[_]>::to_vec)
        {
            Parts::Cases(
                cases
                    .into_iter()
                    .map(|(name, index, payload)| {
                        (name, index, tt.substitute_positional(payload, &args))
                    })
                    .collect(),
            )
        } else {
            return Layout::Opaque;
        };
        Layout::Holder { decl, args, parts }
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

impl HeldTypes for Resources<'_> {
    fn key(&self, type_id: TypeId) -> TypeKey {
        self.tt.borrow().type_key(type_id)
    }

    fn held(&self, type_id: TypeId) -> Held {
        match self.layout(type_id) {
            Layout::Resource(..) | Layout::End => Held::Resource,
            Layout::Holder { decl, args, parts } => Held::Members {
                decl,
                args,
                members: parts.types(),
            },
            Layout::Opaque => Held::Nothing,
        }
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
            TirPattern::Or(alternatives) => self.visit_pattern(&alternatives[0]),
            // An unrestricted handle is never dropped, so it owes no cleanup.
            TirPattern::Narrow { .. } => {}
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Tuple(..)
            | TirPattern::Variant { .. }
            | TirPattern::Enum { .. }
            | TirPattern::Struct { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. }
            | TirPattern::PerInstance { .. } => self.walk_pattern(pattern),
        }
    }
}

/// What a call reveals about ownership, by callee. Each set holds
/// [`callee_key`]s, which a generic function shares with its instantiations.
#[derive(Default)]
struct CalleeFacts {
    /// Functions that copy their result out of a shared reference: it names a
    /// type parameter some `&` parameter stores and no `&mut` one does, as
    /// `List::index_value` does. The referenced storage keeps what was copied,
    /// so the result is no temporary of the caller's, and taking one that
    /// holds a resource is refused (WEP 2026-05-21, "A call through a shared
    /// reference copies"). Through a `&mut` a call extracts instead, as
    /// `List::pop` does, and the caller owns its result.
    copy_returning: IndexSet<String>,
}

impl CalleeFacts {
    fn record(&mut self, func: &TirFunction, resources: Resources) {
        let key = callee_key(&FunctionRef::from_resolved(
            func,
            func.module_source.clone(),
        ));
        let params: Vec<ResolvedType> = {
            let tt = resources.tt.borrow();
            func.params
                .iter()
                .map(|param| tt.get(param.type_id).clone())
                .collect()
        };
        let stores = Stores {
            resources,
            params: resources.tt.borrow().type_param_indices(func.return_type),
        };
        let mut reads_shared = false;
        let mut reads_exclusive = false;
        for param in &params {
            match param {
                ResolvedType::Ref(inner) => reads_shared |= holds_resource(&stores, *inner),
                ResolvedType::MutRef(inner) => {
                    reads_exclusive |= holds_resource(&stores, *inner);
                }
                _ => {}
            }
        }
        if reads_shared && !reads_exclusive {
            self.copy_returning.insert(key);
        }
    }
}

/// Whether a type stores a value of one of `params`, for [`CalleeFacts`],
/// asked through [`holds_resource`] with such a value as the resource it
/// counts: in a field, an element or a payload, or behind a reference. A function type
/// produces its result rather than storing it, so `AsyncCall<T>`, which holds
/// only a `fn(i32) -> T`, stores no `T`.
struct Stores<'a> {
    resources: Resources<'a>,
    params: IndexSet<u32>,
}

impl HeldTypes for Stores<'_> {
    fn key(&self, type_id: TypeId) -> TypeKey {
        self.resources.key(type_id)
    }

    fn held(&self, type_id: TypeId) -> Held {
        let resolved = self.resources.tt.borrow().get(type_id).clone();
        match resolved {
            ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                if self.params.contains(&index) {
                    Held::Resource
                } else {
                    Held::Nothing
                }
            }
            ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::BuiltinArray(inner) => Held::Members {
                decl: None,
                args: Vec::new(),
                members: vec![inner],
            },
            _ => match self.resources.held(type_id) {
                Held::Resource => Held::Nothing,
                held => held,
            },
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

/// Entry point: elaborate resource drops for every function in the project,
/// and return the calls refused for copying a resource out of a reference.
pub fn elaborate_resource_drops(project: &mut Package) -> Vec<SynthesisError> {
    let Some(type_table) = project
        .tir_modules
        .values()
        .next()
        .map(|m| m.type_table.clone())
    else {
        return Vec::new();
    };
    let struct_fields: StructFields = project
        .tir_modules
        .values()
        .flat_map(|module| tir::struct_fields(&module.structs))
        .collect();
    let answers = RefCell::default();
    let resources = Resources {
        tt: &type_table,
        reg: &project.cm_interface_registry,
        struct_fields: &struct_fields,
        answers: &answers,
    };

    let mut callees = CalleeFacts::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            callees.record(&func_rc.borrow(), resources);
        }
    }

    let mut glue = DropGlue {
        entry: project.entry_module_source.clone(),
        called: IndexMap::default(),
    };
    let mut refused = Vec::new();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            elaborate_function(
                &mut func_rc.borrow_mut(),
                resources,
                &callees,
                &mut glue,
                &mut refused,
            );
        }
    }
    // A glue body can call glue of its own, so the list grows while it is
    // built.
    let mut glue_functions = Vec::new();
    while let Some((name, type_id)) = glue
        .called
        .get_index(glue_functions.len())
        .map(|(name, type_id)| (name.clone(), *type_id))
    {
        glue_functions.push(build_drop_glue(
            name,
            type_id,
            resources,
            &callees,
            &mut glue,
            &mut refused,
        ));
    }
    let entry = project
        .tir_modules
        .get_mut(&project.entry_module_source)
        .expect("the entry module is loaded");
    for function in glue_functions {
        entry.functions.push(Rc::new(RefCell::new(function)));
    }
    refused
}

/// The function that drops a value of `type_id`, a holder that holds itself:
/// expanding its drop in place would never end.
fn build_drop_glue(
    name: String,
    type_id: TypeId,
    resources: Resources,
    callees: &CalleeFacts,
    glue: &mut DropGlue,
    refused: &mut Vec<SynthesisError>,
) -> TirFunction {
    let entry = glue.entry.clone();
    let params = vec![TirParam {
        name: "value".to_string(),
        type_id,
        local_index: 0,
        is_mut: false,
        is_mut_ref: false,
        span: synth_span(),
    }];
    let mut locals = common::locals_from_params(&params);
    let mut local_count = locals.len() as u32;
    let mut cx = Cx {
        resources,
        callees,
        locals: &mut locals,
        local_base: 0,
        local_count: Some(&mut local_count),
        scopes: Vec::new(),
        targets: Vec::new(),
        expanding: Vec::new(),
        glue,
        module: entry.clone(),
        refused,
    };
    let stmts = drop_value(local_ref(0, "value", type_id), type_id, &mut cx);
    let mut function = common::make_synthetic_free_function(
        name,
        params,
        TypeTable::UNIT,
        TirBlock::new(stmts, synth_span()),
        locals,
    );
    function.module_source = entry;
    function
}

/// Elaborate resource drops for a single function body.
fn elaborate_function(
    func: &mut TirFunction,
    resources: Resources,
    callees: &CalleeFacts,
    glue: &mut DropGlue,
    refused: &mut Vec<SynthesisError>,
) {
    let TirFunction {
        body: Some(body),
        params,
        locals,
        local_count,
        module_source,
        ..
    } = func
    else {
        return;
    };

    // A parameter that owns a resource is a local of the body's scope, so it
    // drops at the body's end unless the body moves it. A by-value `self` is
    // one.
    let owned: Vec<Live> = params
        .iter()
        .filter(|param| resources.carried_by(param.type_id))
        .map(|param| Live {
            local: param.local_index,
            name: param.name.clone(),
            type_id: param.type_id,
            temporary: false,
        })
        .collect();
    if owned.is_empty() && !body_has_resource(body, resources) {
        return;
    }

    let mut cx = Cx {
        resources,
        callees,
        locals,
        local_base: 0,
        local_count: Some(local_count),
        scopes: Vec::new(),
        targets: Vec::new(),
        expanding: Vec::new(),
        glue,
        module: module_source.clone(),
        refused,
    };
    elab_scope(body, owned, TemporaryScope::Statement, &mut cx);
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

/// The statements that drop `lives`, in order, each where it is not null.
fn drop_lives(lives: &[Live], cx: &mut Cx) -> Vec<TirStmt> {
    lives
        .iter()
        .flat_map(|live| drop_if_held(live.place(), live.type_id, cx))
        .collect()
}

/// Drop what the place `place` holds, where a move has not emptied it.
fn drop_if_held(place: TirExpr, type_id: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    let drops = drop_value(place.clone(), type_id, cx);
    if drops.is_empty() {
        return drops;
    }
    let span = synth_span();
    let moved = builtin_call("is_moved", vec![place], TypeTable::BOOL);
    vec![if_stmt(
        not_expr(moved, span),
        TirBlock::new(drops, span),
        None,
    )]
}

/// Drop a value nothing keeps: one discarded in statement position, or handed
/// to `builtin::drop`. Anything but a local is spilled first: a structural drop reads its scrutinee once per
/// field, which would evaluate the expression again each time.
fn drop_discarded(value: TirExpr, cx: &mut Cx) -> Vec<TirStmt> {
    let type_id = value.type_id;
    if matches!(value.kind, TirExprKind::Local { .. }) {
        return drop_if_held(value, type_id, cx);
    }
    let (local, name) = cx.alloc_local(type_id, "discarded");
    let mut stmts = vec![let_stmt(&name, local, type_id, value)];
    stmts.extend(drop_value(local_ref(local, &name, type_id), type_id, cx));
    stmts
}

/// Build the statements that release every Component Model resource reachable
/// from `scrutinee`, a value of type `type_id` that is not null. A part a move
/// emptied is skipped.
fn drop_value(scrutinee: TirExpr, type_id: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    match cx.resources.layout(type_id) {
        Layout::Resource(def, cm) => {
            let decl = CmDecl::new(cx.resources.tt.borrow().defs(), def, cm);
            vec![expr_stmt(cm_canonical_call(
                CanonicalIntrinsic::ResourceDrop(decl),
                vec![scrutinee],
                TypeTable::UNIT,
            ))]
        }
        Layout::End => vec![expr_stmt(end_drop_call(
            scrutinee,
            type_id,
            &cx.resources.tt.borrow(),
        ))],
        Layout::Holder { parts, .. } => {
            let key = cx.resources.tt.borrow().type_key(type_id);
            if cx.expanding.contains(&key) {
                return drop_through_glue(scrutinee, type_id, cx);
            }
            cx.expanding.push(key);
            let stmts = match parts {
                Parts::Fields(fields) => drop_projected(scrutinee, &fields, cx),
                Parts::Cases(cases) => drop_variant(scrutinee, type_id, &cases, cx),
            };
            cx.expanding.pop();
            stmts
        }
        Layout::Opaque => Vec::new(),
    }
}

/// Drop `scrutinee`, a holder already being expanded, by calling its drop
/// glue. A holder naming a type parameter has no glue: this pass runs before
/// monomorphization, which would have to instantiate it.
fn drop_through_glue(scrutinee: TirExpr, type_id: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    let name = {
        let tt = cx.resources.tt.borrow();
        if tt.contains_type_param(type_id) {
            return Vec::new();
        }
        drop_glue_name(&tt.mangle_type_arg_for_generic(type_id))
    };
    cx.glue.called.entry(name.clone()).or_insert(type_id);
    vec![expr_stmt(common::entry_call(
        &name,
        vec![scrutinee],
        TypeTable::UNIT,
        cx.glue.entry.clone(),
    ))]
}

/// Drop each resource-carrying field / tuple element of `scrutinee` in
/// declaration order, projecting it with a `FieldAccess`, where a move has not
/// emptied it. `scrutinee` is a place expression (a local reference), so
/// cloning it per field is cheap.
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
        stmts.extend(drop_if_held(field, *field_ty, cx));
    }
    stmts
}

/// Drop a variant structurally via a synthesized `match`, an arm per case.
fn drop_variant(
    scrutinee: TirExpr,
    variant_ty: TypeId,
    cases: &[(String, u32, TypeId)],
    cx: &mut Cx,
) -> Vec<TirStmt> {
    let span = synth_span();
    let arms = cases
        .iter()
        .map(|(name, index, payload)| variant_drop_arm(variant_ty, name, *index, *payload, cx))
        .collect();
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(scrutinee),
            arms,
        },
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(match_expr), span)]
}

/// Build one arm of a variant's structural-drop `match`: the case's payload,
/// where it carries a resource, bound to a fresh local and dropped; an empty
/// body otherwise.
fn variant_drop_arm(
    variant_ty: TypeId,
    case_name: &str,
    case_index: u32,
    payload_ty: TypeId,
    cx: &mut Cx,
) -> TirMatchArm {
    let span = synth_span();
    let (bindings, body_stmts) = if cx.carries_resource(payload_ty) {
        let (payload_local, payload_name) = cx.alloc_local(payload_ty, "drop_v");
        let drops = drop_value(
            local_ref(payload_local, &payload_name, payload_ty),
            payload_ty,
            cx,
        );
        let binding = TirPattern::Binding {
            name: payload_name,
            local_index: payload_local,
            type_id: payload_ty,
        };
        (vec![binding], drops)
    } else {
        (Vec::new(), Vec::new())
    };
    TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: variant_ty,
            variant_name: case_name.to_string(),
            case_index,
            bindings,
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

/// Elaborate `block` in place as a scope that opens declaring `declared`. Its
/// locals drop at its end where it falls through, and its temporaries where
/// `temporaries` says.
fn elab_scope(
    block: &mut TirBlock,
    declared: Vec<Live>,
    temporaries: TemporaryScope,
    cx: &mut Cx,
) -> Flow {
    cx.scopes.push(declared);
    let stmts = std::mem::take(&mut block.stmts);
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
        let first_declared = cx.scopes.last().expect("pushed above").len();
        let first_out = out.len();
        flow = elab_stmt(stmt, cx, is_tail, &mut out);
        if flow == Flow::Normal && temporaries == TemporaryScope::Statement {
            let spilled = take_temporaries(cx, first_declared);
            let drops = drop_lives(&spilled, cx);
            if is_tail {
                // The block's value is this statement's: keep it past the drops.
                let tail = out.split_off(first_out);
                out.extend(append_block_drops(tail, drops, cx));
            } else {
                out.extend(drops);
            }
        }
    }
    let scope = cx.scopes.pop().expect("pushed above");
    let (deferred, lives): (Vec<Live>, Vec<Live>) = scope
        .into_iter()
        .partition(|live| live.temporary && temporaries == TemporaryScope::Enclosing);
    if flow == Flow::Normal {
        let lives: Vec<Live> = lives.into_iter().rev().collect();
        let drops = drop_lives(&lives, cx);
        out = append_block_drops(out, drops, cx);
    }
    if let Some(enclosing) = cx.scopes.last_mut() {
        enclosing.extend(deferred);
    }
    block.stmts = out;
    flow
}

/// Take the temporaries the innermost scope declared from `first` on,
/// innermost first.
fn take_temporaries(cx: &mut Cx, first: usize) -> Vec<Live> {
    let scope = cx.scopes.last_mut().expect("a scope is open");
    let (temporaries, kept): (Vec<Live>, Vec<Live>) =
        scope.drain(first..).partition(|live| live.temporary);
    scope.extend(kept);
    temporaries.into_iter().rev().collect()
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
    let result_ty = tir::block_result_type(&cx.resources.tt.borrow(), &inner);
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

/// Elaborate a block in place as a fresh nested scope.
fn elab_block(block: &mut TirBlock, cx: &mut Cx) -> Flow {
    elab_scope(block, Vec::new(), TemporaryScope::Statement, cx)
}

/// Elaborate a plain block expression in place. Nothing can leave it but a
/// `return` or a `break` past it, so it runs whole as part of the statement
/// holding it, and its temporaries drop when that statement ends. Synthesized
/// code relies on that: an `assert` captures its operand `f(&t())` as
/// `f({ $v = &t(); $v })`, so `t()` must outlive the capture's statement.
fn elab_plain_block(block: &mut TirBlock, cx: &mut Cx) {
    elab_scope(block, Vec::new(), TemporaryScope::Enclosing, cx);
}

/// Elaborate `block`, the body `target` is reached from.
fn elab_target_body(block: &mut TirBlock, label: Option<String>, cx: &mut Cx) -> Flow {
    cx.targets.push(BreakTarget {
        label,
        depth: cx.scopes.len(),
    });
    let flow = elab_block(block, cx);
    cx.targets.pop();
    flow
}

/// Elaborate one statement, appending the result to `out`. Returns whether
/// control falls through to the next statement.
fn elab_stmt(stmt: TirStmt, cx: &mut Cx, is_tail: bool, out: &mut Vec<TirStmt>) -> Flow {
    let span = stmt.span;
    match stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            type_id,
            value,
            storage,
        } => {
            let value = elab_value_expr(value, cx);
            if cx.carries_resource(type_id) {
                cx.declare(Live {
                    local: local_index,
                    name: name.clone(),
                    type_id,
                    temporary: false,
                });
            }
            out.push(TirStmt {
                kind: TirStmtKind::Let {
                    name,
                    local_index,
                    is_mut,
                    type_id,
                    value,
                    storage,
                },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::LetDestructure { pattern, value } => {
            let value = elab_value_expr(value, cx);
            for live in cx.resources.pattern_bindings(&pattern) {
                cx.declare(live);
            }
            out.push(TirStmt {
                kind: TirStmtKind::LetDestructure { pattern, value },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::Expr(expr) => {
            let elaborated = elab_value_expr(expr, cx);
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
            let value = elab_value_expr(value, cx);
            out.push(TirStmt {
                kind: TirStmtKind::TaskReturn { value },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::Return { value } => {
            let value = value.map(|v| elab_value_expr(v, cx));
            let drops = cx.drop_scopes_from(0);
            leave_after_drops(value, drops, span, cx, out, |value| TirStmtKind::Return {
                value,
            })
        }

        TirStmtKind::If {
            condition,
            mut then_block,
            else_block,
        } => {
            let condition = elab_value_expr(condition, cx);
            let then_flow = elab_block(&mut then_block, cx);
            let (else_block, else_flow) = match else_block {
                Some(mut block) => {
                    let flow = elab_block(&mut block, cx);
                    (Some(block), flow)
                }
                None => (None, Flow::Normal),
            };
            out.push(TirStmt {
                kind: TirStmtKind::If {
                    condition,
                    then_block,
                    else_block,
                },
                span,
            });
            then_flow.join(else_flow)
        }

        TirStmtKind::Loop { mut body } => {
            elab_target_body(&mut body, None, cx);
            out.push(TirStmt {
                kind: TirStmtKind::Loop { body },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::LabeledBlock { label, mut block } => {
            elab_target_body(&mut block, Some(label.clone()), cx);
            out.push(TirStmt {
                kind: TirStmtKind::LabeledBlock { label, block },
                span,
            });
            // A `break label` lands after the block whatever its body does.
            Flow::Normal
        }

        TirStmtKind::Break { label, value } => {
            let value = value.map(|v| elab_value_expr(v, cx));
            // `break` drops what every scope it leaves declared.
            let depth = cx.target_depth(label.as_deref());
            let drops = cx.drop_scopes_from(depth);
            leave_after_drops(value, drops, span, cx, out, |value| TirStmtKind::Break {
                label,
                value,
            })
        }

        TirStmtKind::Continue => {
            let depth = cx.target_depth(None);
            let drops = cx.drop_scopes_from(depth);
            leave_after_drops(None, drops, span, cx, out, |_| TirStmtKind::Continue)
        }

        TirStmtKind::VariadicForOf { .. } => {
            // Type-pack `for` expansion is resolved post-monomorphize; leave its
            // body untouched (it holds no resources in practice).
            let mut stmt = stmt;
            if let TirStmtKind::VariadicForOf { iterable, .. } = &mut stmt.kind {
                elab_expr(iterable, true, cx);
            }
            out.push(stmt);
            Flow::Normal
        }
    }
}

/// Emit `drops`, then the statement `leave` builds around `value`. With drops
/// to run, `value` is spilled ahead of them: it may refer to a resource they
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

// ---------------------------------------------------------------------------
// Expression elaboration
// ---------------------------------------------------------------------------

/// Elaborate an expression whose value is transferred to its consumer: a
/// `let`, a `return`, an argument, a statement's own value.
fn elab_value_expr(mut expr: TirExpr, cx: &mut Cx) -> TirExpr {
    elab_expr(&mut expr, true, cx);
    expr
}

/// Elaborate `expr` in place, in evaluation order: empty every place a
/// resource moves out of, spill every temporary it only references (see
/// [`spill_temporary`]), and elaborate each nested scope.
///
/// `consuming` is `true` when the position transfers ownership of the value
/// placed in it. A referencing position (`&x`, a `&self` receiver, a field read,
/// an operator operand, a `matches` test) transfers nothing.
///
/// The match is exhaustive so a new `TirExprKind` cannot silently escape
/// ownership accounting: a missed move drops twice, a missed reference leaks.
fn elab_expr(expr: &mut TirExpr, consuming: bool, cx: &mut Cx) {
    if !consuming && is_temporary(&expr.kind, cx) && cx.carries_resource(expr.type_id) {
        elab_expr(expr, true, cx);
        spill_temporary(expr, cx);
        return;
    }
    let type_id = expr.type_id;
    let span = expr.span;
    let mut dropped = None;
    match &mut expr.kind {
        TirExprKind::Local { .. } => {
            if consuming && cx.carries_resource(type_id) {
                take_place(expr, cx);
            }
        }

        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } => elab_expr(inner, false, cx),
        // A dereference reads through a reference, which owns nothing.
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => elab_expr(inner, false, cx),
        // Casts and projections keep the surrounding ownership context:
        // `&(h as Fields)` must still see `h` as referenced.
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TupleLen { expr: inner } => {
            elab_expr(inner, consuming, cx);
        }
        // A payload cannot be emptied on its own, so taking it moves the
        // variant it is read from.
        TirExprKind::VariantPayload { expr: inner, .. } => {
            if consuming && cx.carries_resource(type_id) && is_place(inner, cx) {
                elab_expr(inner, false, cx);
                let place = (**inner).clone();
                empty_after(expr, place, cx);
            } else {
                elab_expr(inner, consuming, cx);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            if consuming && cx.carries_resource(type_id) {
                take_field(expr, cx);
            } else {
                // Reading a field that owns nothing leaves the aggregate whole.
                elab_expr(inner, false, cx);
            }
        }
        // Tag inspection / `matches` test: reads the discriminant only.
        TirExprKind::VariantTag { expr: inner } | TirExprKind::VariantTest { expr: inner, .. } => {
            elab_expr(inner, false, cx);
        }

        TirExprKind::Call { func, args, .. } => {
            let key = callee_key(func);
            // Only a copy something takes is a second owner: one that is only
            // read where it stands (`list[0].has(…)`) is owned by nothing.
            if consuming && cx.callees.copy_returning.contains(&key) && cx.carries_resource(type_id)
            {
                cx.refuse_copy(type_id, span);
            }
            let (receiver, rest) = args.split_mut();
            if let Some(receiver) = receiver {
                // A `&self` method auto-references its receiver, and a
                // by-value `self` takes it as written, transferring it.
                match &mut receiver.expr.kind {
                    TirExprKind::Unary {
                        op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                        expr,
                    } => elab_expr(expr, false, cx),
                    _ => elab_expr(&mut receiver.expr, true, cx),
                }
            }
            for arg in rest {
                elab_expr(&mut arg.expr, true, cx);
            }
            if func.is_builtin_named("drop") {
                let (_, [arg]) = args.split_mut() else {
                    unreachable!("`builtin::drop` takes one value");
                };
                let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span);
                dropped = Some(std::mem::replace(&mut arg.expr, unit));
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                elab_expr(arg, true, cx);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            elab_expr(callee, true, cx);
            for arg in args {
                elab_expr(arg, true, cx);
            }
        }

        // Every operator takes its operands by reference.
        TirExprKind::Binary { op, left, right } => {
            elab_expr(left, false, cx);
            if matches!(op, TirBinaryOp::And | TirBinaryOp::Or) {
                elab_expr_scope(right, cx);
            } else {
                elab_expr(right, false, cx);
            }
        }
        TirExprKind::Assign { target, value } => {
            elab_expr(target, false, cx);
            elab_expr(value, true, cx);
            elab_store(target, value, cx);
        }
        // An element read leaves its container whole; a copy out of one is
        // the refused call's to report.
        TirExprKind::Index { expr: base, index } => {
            elab_expr(base, false, cx);
            elab_expr(index, true, cx);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            elab_expr(value, true, cx);
        }

        TirExprKind::Block(block) => elab_plain_block(block, cx),
        TirExprKind::LabeledBlock { label, block, .. } => {
            elab_target_body(block, Some(label.clone()), cx);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            elab_expr(condition, true, cx);
            elab_block(then_branch, cx);
            if let Some(else_branch) = else_branch {
                elab_block(else_branch, cx);
            }
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => elab_match(scrutinee, arms, cx),
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                elab_expr(&mut binding.handler, true, cx);
            }
            elab_block(body, cx);
        }

        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                elab_expr(&mut field.value, true, cx);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                elab_expr(elem, true, cx);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload) = payload {
                elab_expr(payload, true, cx);
            }
        }
        // Expanded once per pack element, so each expansion is a scope.
        TirExprKind::TypePackExpansion { call_expr, .. } => {
            elab_expr_scope(call_expr, cx);
        }
        TirExprKind::VariadicTupleComprehension { iterable, body, .. } => {
            elab_expr(iterable, true, cx);
            elab_expr_scope(body, cx);
        }
        // A template formats each hole through a reference.
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    elab_expr(inner, false, cx);
                }
            }
        }
        TirExprKind::GlobalVarGet { .. } => {}

        TirExprKind::Closure {
            params,
            body,
            body_locals,
            ..
        } => elab_closure(params, body, body_locals, cx),

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
    if let Some(value) = dropped {
        let drops = drop_discarded(value, cx);
        *expr = TirExpr::new(
            TirExprKind::Block(TirBlock::new(drops, span)),
            type_id,
            span,
        );
    }
}

/// Elaborate a closure body as the function it is. It reads captured values
/// through `Capture` and numbers its own locals, parameters first, so it gets
/// a frame of its own.
fn elab_closure(
    params: &[(String, TypeId)],
    body: &mut TirExpr,
    body_locals: &mut Vec<TirLocal>,
    cx: &mut Cx,
) {
    let owned: Vec<Live> = params
        .iter()
        .enumerate()
        .filter(|(_, (_, type_id))| cx.carries_resource(*type_id))
        .map(|(index, (name, type_id))| Live {
            local: index as u32,
            name: name.clone(),
            type_id: *type_id,
            temporary: false,
        })
        .collect();
    let mut probe = ResourceProbe {
        resources: cx.resources,
        found: false,
    };
    probe.visit_expr(body);
    if owned.is_empty() && !probe.found {
        return;
    }
    let mut inner = Cx {
        resources: cx.resources,
        callees: cx.callees,
        locals: body_locals,
        local_base: params.len() as u32,
        local_count: None,
        scopes: Vec::new(),
        targets: Vec::new(),
        expanding: Vec::new(),
        glue: cx.glue,
        module: cx.module.clone(),
        refused: cx.refused,
    };
    let mut block = into_block(body);
    elab_scope(&mut block, owned, TemporaryScope::Statement, &mut inner);
    body.kind = TirExprKind::Block(block);
}

/// Whether `expr` names a place a move can empty: a local owning a resource,
/// or a field of one. A field read through a reference owns nothing.
fn is_place(expr: &TirExpr, cx: &Cx) -> bool {
    match &expr.kind {
        TirExprKind::Local { .. } => cx.carries_resource(expr.type_id),
        TirExprKind::FieldAccess { expr, .. } => is_place(expr, cx),
        _ => false,
    }
}

/// Replace the place `expr` with its value, emptying the place:
/// `{ let $moved = place; place = null; $moved }`.
fn take_place(expr: &mut TirExpr, cx: &mut Cx) {
    let place = expr.clone();
    empty_after(expr, place, cx);
}

/// Replace `expr` with its value, computed before `place` is emptied.
fn empty_after(expr: &mut TirExpr, place: TirExpr, cx: &mut Cx) {
    let type_id = expr.type_id;
    let (local, name) = cx.alloc_local(type_id, "moved");
    let null = TirExpr::new(TirExprKind::Null, place.type_id, synth_span());
    wrap_in_block(expr, |value| {
        vec![
            let_stmt(&name, local, type_id, value),
            expr_stmt(assign(place, null)),
            expr_stmt(local_ref(local, &name, type_id)),
        ]
    });
}

/// Move the affine field `expr` reads out of its aggregate, emptying the field.
/// An aggregate that is a temporary is spilled first, so its other fields drop
/// when the statement ends. One read through a reference moves nothing here:
/// the move check reports it.
fn take_field(expr: &mut TirExpr, cx: &mut Cx) {
    let TirExprKind::FieldAccess { expr: base, .. } = &mut expr.kind else {
        unreachable!("`take_field` takes a field access");
    };
    if is_place(base, cx) {
        elab_expr(base, false, cx);
        take_place(expr, cx);
        return;
    }
    if is_temporary(&base.kind, cx) {
        elab_expr(base, true, cx);
        let type_id = base.type_id;
        let (local, name) = spill_as_local(base, cx);
        let value = std::mem::replace(&mut **base, local_ref(local, &name, type_id));
        take_place(expr, cx);
        let TirExprKind::Block(block) = &mut expr.kind else {
            unreachable!("`take_place` leaves a block");
        };
        block
            .stmts
            .insert(0, let_stmt(&name, local, type_id, value));
        return;
    }
    elab_expr(base, false, cx);
}

/// Whether evaluating `kind` produces a fresh value rather than reading a
/// place. Referencing such a value leaves it with no owner but the statement.
fn is_temporary(kind: &TirExprKind, cx: &Cx) -> bool {
    if let TirExprKind::Call { func, .. } = kind {
        return !cx.callees.copy_returning.contains(&callee_key(func));
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

/// Move the referenced temporary `expr` into a fresh local, in place and so in
/// evaluation order (`{ let $temp = expr; $temp }`), owned until the statement
/// holding it ends. Returns the local.
fn spill_temporary(expr: &mut TirExpr, cx: &mut Cx) -> TirExpr {
    let type_id = expr.type_id;
    let (local, name) = spill_as_local(expr, cx);
    let value = std::mem::replace(expr, local_ref(local, &name, type_id));
    wrap_in_block(expr, |read| {
        vec![let_stmt(&name, local, type_id, value), expr_stmt(read)]
    });
    local_ref(local, &name, type_id)
}

/// Declare a local owned by the statement holding `expr`, to hold the
/// temporary `expr` evaluates to; the caller binds it.
fn spill_as_local(expr: &TirExpr, cx: &mut Cx) -> (u32, String) {
    let type_id = expr.type_id;
    let (local, name) = cx.alloc_local(type_id, "temp");
    cx.declare(Live {
        local,
        name: name.clone(),
        type_id,
        temporary: true,
    });
    (local, name)
}

/// What a store into `target` replaces drops once the stored `value` is
/// computed, since computing it may read what it replaces.
fn elab_store(target: &TirExpr, value: &mut TirExpr, cx: &mut Cx) {
    if !cx.carries_resource(target.type_id) || !is_place(target, cx) {
        return;
    }
    let replaced = drop_if_held(target.clone(), target.type_id, cx);
    if replaced.is_empty() {
        return;
    }
    let type_id = value.type_id;
    let (local, name) = cx.alloc_local(type_id, "stored");
    wrap_in_block(value, |computed| {
        let mut stmts = vec![let_stmt(&name, local, type_id, computed)];
        stmts.extend(replaced);
        stmts.push(expr_stmt(local_ref(local, &name, type_id)));
        stmts
    });
}

/// Elaborate `expr` as a scope of its own: whatever it declares (its
/// temporaries) drops once its value is computed. An expression that runs on
/// some paths only, or more than once, is one.
fn elab_expr_scope(expr: &mut TirExpr, cx: &mut Cx) {
    cx.scopes.push(Vec::new());
    elab_expr(expr, true, cx);
    let scope = cx.scopes.pop().expect("pushed above");
    let lives: Vec<Live> = scope.into_iter().rev().collect();
    let drops = drop_lives(&lives, cx);
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

/// Elaborate a `match`: its scrutinee, then each arm as a scope declaring the
/// arm's bindings. An arm that binds a resource out of the scrutinee moves the
/// scrutinee, so the arm empties it before its body runs; a scrutinee that is
/// a temporary is spilled first, so an arm binding nothing leaves it for its
/// statement to drop.
fn elab_match(scrutinee: &mut TirExpr, arms: &mut [TirMatchArm], cx: &mut Cx) {
    let bindings: Vec<Vec<Live>> = arms
        .iter()
        .map(|arm| cx.resources.pattern_bindings(&arm.pattern))
        .collect();
    let extracts = bindings.iter().any(|arm_bindings| !arm_bindings.is_empty());
    let place = if extracts && cx.carries_resource(scrutinee.type_id) {
        if is_place(scrutinee, cx) {
            elab_expr(scrutinee, false, cx);
            Some((*scrutinee).clone())
        } else if is_temporary(&scrutinee.kind, cx) {
            elab_expr(scrutinee, true, cx);
            Some(spill_temporary(scrutinee, cx))
        } else {
            elab_expr(scrutinee, true, cx);
            None
        }
    } else {
        elab_expr(scrutinee, false, cx);
        None
    };
    for (arm, arm_bindings) in arms.iter_mut().zip(bindings) {
        let emptied = (!arm_bindings.is_empty()).then(|| place.clone()).flatten();
        cx.scopes.push(Vec::new());
        if let Some(guard) = arm.guard.as_mut() {
            elab_expr_scope(guard, cx);
        }
        cx.scopes.pop();
        let mut block = into_block(&mut arm.body);
        elab_scope(&mut block, arm_bindings, TemporaryScope::Statement, cx);
        // Inserted once the body is elaborated: emptying the scrutinee is a
        // move, not a store that drops what it replaces.
        if let Some(place) = emptied {
            let null = TirExpr::new(TirExprKind::Null, place.type_id, synth_span());
            block.stmts.insert(0, expr_stmt(assign(place, null)));
        }
        arm.body = TirExpr::new(TirExprKind::Block(block), arm.body.type_id, arm.body.span);
    }
}

/// `body`, an arm's or a closure's, as a block, leaving a unit in its place.
fn into_block(body: &mut TirExpr) -> TirBlock {
    let span = body.span;
    let taken = std::mem::replace(body, TirExpr::new(TirExprKind::Unit, body.type_id, span));
    match taken.kind {
        TirExprKind::Block(block) => block,
        _ => TirBlock::new(vec![expr_stmt(taken)], span),
    }
}
