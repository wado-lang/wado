//! Resource drop elaboration. A CM `own<resource>` handle must be released with
//! `resource.drop` exactly once and Wado has no destructors, so this inserts a
//! drop wherever a value is still owned at the end of its scope. A resource is
//! *transferred* when passed by value, returned, or placed in an aggregate — not
//! by a `&self` receiver or a `&` — so the drop cannot double-free. A
//! temporary that is only referenced has no other owner, so it is spilled into a
//! local that drops when its statement ends.

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
use crate::synthesis::common;
use crate::synthesis::common::{cm_canonical_call, expr_stmt, let_stmt, local_ref, synth_span};
use crate::tir;
use crate::tir::{
    FunctionRef, ResolvedType, StructDef, StructFields, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirTemplatePart, TirUnaryOp, TypeId,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;
use crate::type_table::{TypeKey, TypeTable};

/// An owned resource-bearing value currently live in some scope.
#[derive(Clone)]
struct Live {
    local: u32,
    name: String,
    type_id: TypeId,
    /// A referenced temporary spilled by [`spill_temporary`], owned by its
    /// statement rather than by the enclosing block.
    temporary: bool,
}

/// A resource value a scope declared, and whether the scope still owns it on
/// the current path: a transfer or a drop clears `held`, and an assignment to
/// its local sets it again.
#[derive(Clone)]
struct Slot {
    live: Live,
    held: bool,
}

impl From<Live> for Slot {
    fn from(live: Live) -> Slot {
        Slot { live, held: true }
    }
}

impl Slot {
    fn take(&mut self) -> Option<Live> {
        self.take_if(|_| true)
    }

    fn take_if(&mut self, which: impl FnOnce(&Live) -> bool) -> Option<Live> {
        if !self.held || !which(&self.live) {
            return None;
        }
        self.held = false;
        Some(self.live.clone())
    }
}

/// Ownership state: a stable slot per resource value. Slots are only ever
/// appended (a new binding) or truncated (a scope's end), so a slot index
/// stays valid across cloned branch states.
type Owned = Vec<Slot>;

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
/// (`label: None`). Leaving for it drops every slot from `entry` up, and each
/// slot below that the state it lands in does not hold.
struct BreakTarget {
    label: Option<String>,
    entry: usize,
    /// What the slots below `entry` hold at a loop's head, where `continue`
    /// lands. `None` while a dry run gathers it, and for a labeled block.
    head: Option<Vec<bool>>,
    /// What the slots below `entry` hold where `break` lands. `None` while a
    /// dry run gathers it.
    exit: Option<Vec<bool>>,
    /// What the slots below `entry` held at each `break` to here.
    breaks: Vec<Vec<bool>>,
    /// What the slots below `entry` held at each `continue` to here.
    continues: Vec<Vec<bool>>,
}

impl BreakTarget {
    fn new(
        label: Option<String>,
        entry: usize,
        head: Option<Vec<bool>>,
        exit: Option<Vec<bool>>,
    ) -> Self {
        Self {
            label,
            entry,
            head,
            exit,
            breaks: Vec::new(),
            continues: Vec::new(),
        }
    }
}

struct Cx<'a> {
    resources: Resources<'a>,
    callees: &'a CalleeFacts,
    locals: &'a mut Vec<TirLocal>,
    local_count: &'a mut u32,
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

    /// Where a `break label` (or, with `None`, a `break` or `continue`)
    /// lands.
    fn target_mut(&mut self, label: Option<&str>) -> &mut BreakTarget {
        self.targets
            .iter_mut()
            .rev()
            .find(|target| target.label.as_deref() == label)
            .expect("a `break` or `continue` lands in an enclosing loop or labeled block")
    }

    /// Run `elaborate` for the states it reaches, then undo what it added to
    /// `self`: the code it rewrote is thrown away, so the locals, glue,
    /// refusals and arrivals it recorded go too.
    fn dry_run<R>(&mut self, elaborate: impl FnOnce(&mut Self) -> R) -> R {
        let locals = self.locals.len();
        let refused = self.refused.len();
        let glue = self.glue.called.len();
        let arrivals: Vec<(usize, usize)> = self
            .targets
            .iter()
            .map(|target| (target.breaks.len(), target.continues.len()))
            .collect();
        let result = elaborate(self);
        self.locals.truncate(locals);
        *self.local_count = locals as u32;
        self.refused.truncate(refused);
        self.glue.called.truncate(glue);
        for (target, (breaks, continues)) in self.targets.iter_mut().zip(arrivals) {
            target.breaks.truncate(breaks);
            target.continues.truncate(continues);
        }
        result
    }
}

/// What a type holds that this pass drops, one level down.
enum Layout<'a> {
    /// A resource, released by `resource.drop` under this CM name.
    Resource(DefId, &'a str),
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
    /// `GenericResource` (`Future` / `Stream`) is opaque: those handles have
    /// their own explicit drop discipline and must not be touched here. So is
    /// a reference — a referenced place owns nothing — and an unrestricted
    /// resource, which is never dropped.
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
            Layout::Resource(..) => Held::Resource,
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
    /// Instance methods whose `self` is taken by value. Calling one transfers
    /// ownership of the receiver (e.g. `Result::unwrap`, which moves the
    /// wrapped value out).
    owned_self: IndexSet<String>,
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
        let is_ref =
            |param: &ResolvedType| matches!(param, ResolvedType::Ref(_) | ResolvedType::MutRef(_));
        if func.method_info.is_some() && func.takes_self() && !is_ref(&params[0]) {
            self.owned_self.insert(key.clone());
        }
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
/// a free function's identity.
fn callee_key(func: &FunctionRef) -> String {
    match &func.method_info {
        Some(info) => info.base_dispatch_key(),
        None => func.function_id().to_string(),
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
        local_count: &mut local_count,
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

    // Parameters that own (or carry) a resource are live from entry. A
    // by-value `self` is just such a parameter: the method that received it
    // owns it and must drop it unless its body transfers it onward.
    let mut owned: Owned = params
        .iter()
        .filter(|param| resources.carried_by(param.type_id))
        .map(|param| {
            Slot::from(Live {
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
        expanding: Vec::new(),
        glue,
        module: module_source.clone(),
        refused,
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
    let lives: Vec<Live> = owned[first..].iter_mut().filter_map(Slot::take).collect();
    drop_all(lives.iter().rev(), cx)
}

/// Take the temporaries still owned in `slots`, in slot order.
fn take_temporaries(slots: &mut [Slot]) -> Vec<Live> {
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
        Layout::Resource(def, cm) => {
            let decl = CmDecl::new(cx.resources.tt.borrow().defs(), def, cm);
            vec![expr_stmt(cm_canonical_call(
                CanonicalIntrinsic::ResourceDrop(decl),
                vec![scrutinee],
                TypeTable::UNIT,
            ))]
        }
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
    owned.extend(deferred.into_iter().map(Slot::from));
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
    let entry = owned.len();
    let label = Some(label.to_string());
    let exit = if entry == 0 {
        Vec::new()
    } else {
        labeled_exit(&label, block, owned, cx)
    };
    let target = BreakTarget::new(label, entry, None, Some(exit.clone()));
    let (flow, target) = elab_target_body(block, owned, target, &exit, cx);
    set_held(owned, &exit);
    if target.breaks.is_empty() {
        flow
    } else {
        Flow::Normal
    }
}

/// Elaborate `block`, the body `target` is reached from, and drop what its end,
/// where it falls through, holds that `end` does not.
fn elab_target_body(
    block: &mut TirBlock,
    owned: &mut Owned,
    target: BreakTarget,
    end: &[bool],
    cx: &mut Cx,
) -> (Flow, BreakTarget) {
    cx.targets.push(target);
    let flow = elab_block(block, owned, cx);
    let target = cx.targets.pop().expect("pushed above");
    if flow == Flow::Normal {
        let drops = conform(owned, end, cx);
        let stmts = std::mem::take(&mut block.stmts);
        block.stmts = append_block_drops(stmts, drops, cx);
    }
    (flow, target)
}

/// What the slots below `owned.len()` hold after a labeled block: where every
/// way out holds them, each `break label` and the end where the body falls
/// through.
fn labeled_exit(label: &Option<String>, block: &TirBlock, owned: &Owned, cx: &mut Cx) -> Vec<bool> {
    let (end, target) = trial(block, owned.clone(), label.clone(), cx);
    meet(target.breaks.iter().chain(&end)).unwrap_or_else(|| held(owned))
}

/// Elaborate a copy of `block`, the body of the target `label` names, from
/// `state` and throw the copy away. Returns what the slots below the body hold
/// where it falls through, if it does, and the target with what each `break`
/// and `continue` to it held.
fn trial(
    block: &TirBlock,
    mut state: Owned,
    label: Option<String>,
    cx: &mut Cx,
) -> (Option<Vec<bool>>, BreakTarget) {
    cx.dry_run(|cx| {
        let mut block = block.clone();
        cx.targets
            .push(BreakTarget::new(label, state.len(), None, None));
        let flow = elab_block(&mut block, &mut state, cx);
        let target = cx.targets.pop().expect("pushed above");
        ((flow == Flow::Normal).then(|| held(&state)), target)
    })
}

/// What a loop's slots below `owned.len()` hold at its head and where it
/// exits. The head holds a slot only where the loop's entry and each
/// iteration's end, falling through or by `continue`, hold it; an iteration
/// starts from the head, so that is a fixpoint. Where it exits holds a slot
/// only where every `break` holds it.
fn loop_states(body: &TirBlock, owned: &Owned, cx: &mut Cx) -> (Vec<bool>, Vec<bool>) {
    let mut head = held(owned);
    if owned.is_empty() {
        return (head.clone(), head);
    }
    loop {
        let mut state = owned.clone();
        set_held(&mut state, &head);
        let (end, target) = trial(body, state, None, cx);
        let next = meet([&head].into_iter().chain(&target.continues).chain(&end))
            .expect("the entry is one way in");
        if next == head {
            let exit = meet(&target.breaks).unwrap_or_else(|| head.clone());
            return (head, exit);
        }
        head = next;
    }
}

/// What each slot holds.
fn held(slots: &[Slot]) -> Vec<bool> {
    slots.iter().map(|slot| slot.held).collect()
}

/// Set what each of the first slots holds.
fn set_held(owned: &mut Owned, holds: &[bool]) {
    for (slot, &held) in owned.iter_mut().zip(holds) {
        slot.held = held;
    }
}

/// What every one of `states` holds alike, if there is one.
fn meet<'s>(states: impl IntoIterator<Item = &'s Vec<bool>>) -> Option<Vec<bool>> {
    states.into_iter().fold(None, |met, state| match met {
        None => Some(state.clone()),
        Some(mut met) => {
            for (held, &also) in met.iter_mut().zip(state) {
                *held &= also;
            }
            Some(met)
        }
    })
}

/// Take the slots `owned` holds and `target` does not, and build the
/// statements that drop them, innermost first: a path joining others that
/// hold less drops its surplus before it joins.
fn conform(owned: &mut Owned, target: &[bool], cx: &mut Cx) -> Vec<TirStmt> {
    let lives: Vec<Live> = owned
        .iter_mut()
        .zip(target)
        .filter(|(_, keep)| !**keep)
        .filter_map(|(slot, _)| slot.take())
        .collect();
    drop_all(lives.iter().rev(), cx)
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
        } => block_flow(&then_block.stmts).join(block_flow(&else_block.stmts)),
        TirStmtKind::If {
            else_block: None, ..
        }
        | TirStmtKind::Let { .. }
        | TirStmtKind::LetDestructure { .. }
        | TirStmtKind::Expr(_)
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::Loop { .. }
        | TirStmtKind::LabeledBlock { .. }
        | TirStmtKind::VariadicForOf { .. } => Flow::Normal,
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
            type_id,
            value,
            storage,
        } => {
            let value = elab_value_expr(value, owned, cx);
            if cx.carries_resource(type_id) {
                owned.push(Slot::from(Live {
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
                    type_id,
                    value,
                    storage,
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
                    .map(Slot::from),
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
            let entry = owned.len();
            let (head, exit) = loop_states(&body, owned, cx);
            out.extend(conform(owned, &head, cx));
            let target = BreakTarget::new(None, entry, Some(head.clone()), Some(exit.clone()));
            elab_target_body(&mut body, owned, target, &head, cx);
            set_held(owned, &exit);
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
            let target = cx.target_mut(label.as_deref());
            let entry = target.entry;
            target.breaks.push(held(&owned[..entry]));
            let exit = target.exit.clone();
            let mut drops = drop_slots(owned, entry, cx);
            if let Some(exit) = exit {
                drops.extend(conform(owned, &exit, cx));
            }
            leave_after_drops(value, drops, span, cx, out, |value| TirStmtKind::Break {
                label,
                value,
            })
        }

        TirStmtKind::Continue => {
            let target = cx.target_mut(None);
            let entry = target.entry;
            target.continues.push(held(&owned[..entry]));
            let head = target.head.clone();
            let mut drops = drop_slots(owned, entry, cx);
            if let Some(head) = head {
                drops.extend(conform(owned, &head, cx));
            }
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
    let mut paths = [(then_owned, then_flow), (else_owned, else_flow)];
    let drops = merge_paths(owned, &mut paths, cx);
    for (block, drops) in [&mut then_block, &mut else_block].into_iter().zip(drops) {
        let stmts = std::mem::take(&mut block.stmts);
        block.stmts = append_block_drops(stmts, drops, cx);
    }
    let else_block = (has_else || !else_block.stmts.is_empty()).then_some(else_block);
    (then_block, else_block, then_flow.join(else_flow))
}

/// Merge the post-states of the paths out of a branch, each `(owned, flow)`
/// elaborated from a clone of `owned`. A resource is owned after the branch
/// only where every path that falls through owns it, whether it kept it or an
/// assignment gave it a new one. Where one does not, the others still owning
/// it drop it at their end: returns, per path, those drops.
fn merge_paths(owned: &mut Owned, paths: &mut [(Owned, Flow)], cx: &mut Cx) -> Vec<Vec<TirStmt>> {
    let falling_through: Vec<Vec<bool>> = paths
        .iter()
        .filter(|(_, flow)| *flow == Flow::Normal)
        .map(|(state, _)| held(state))
        .collect();
    // Where no path falls through, nothing reads the state after.
    let Some(met) = meet(&falling_through) else {
        return vec![Vec::new(); paths.len()];
    };
    set_held(owned, &met);
    paths
        .iter_mut()
        .map(|(state, flow)| match flow {
            Flow::Normal => conform(state, &met, cx),
            Flow::Diverged => Vec::new(),
        })
        .collect()
}

/// Clear every slot of `owned` that `branch`, a clone of it elaborated along
/// one path, transferred. Counting a transfer on one path as one on every path
/// can leak on the others, but never drops twice.
fn release_transferred(owned: &mut Owned, branch: &Owned) {
    for (slot, after) in owned.iter_mut().zip(branch) {
        if !after.held {
            slot.held = false;
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
/// owned resource it transfers, spill every temporary it only references (see
/// [`spill_temporary`]), and elaborate each nested scope.
///
/// `consuming` is `true` when the position transfers ownership of the value
/// placed in it. A referencing position (`&x`, a `&self` receiver, a field read,
/// an operator operand, a `matches` test) transfers nothing.
///
/// The match is exhaustive so a new `TirExprKind` cannot silently escape
/// ownership accounting: a missed transfer drops twice, a missed reference leaks.
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
        // `&(h as Fields)` must still see `h` as referenced.
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TupleLen { expr: inner }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            elab_expr(inner, consuming, owned, cx);
        }
        // Reading a field that owns nothing leaves the aggregate unconsumed.
        TirExprKind::FieldAccess { expr: inner, .. } => {
            let moves_out = consuming && cx.carries_resource(type_id);
            elab_expr(inner, moves_out, owned, cx);
        }
        // Tag inspection / `matches` test: reads the discriminant only.
        TirExprKind::VariantTag { expr: inner } | TirExprKind::VariantTest { expr: inner, .. } => {
            elab_expr(inner, false, owned, cx);
        }

        TirExprKind::Call { func, args, .. } => {
            let key = callee_key(func);
            // Only a copy something takes is a second owner: one that is only
            // read where it stands (`list[0].has(…)`) is owned by nothing.
            if consuming && cx.callees.copy_returning.contains(&key) && cx.carries_resource(type_id)
            {
                cx.refuse_copy(type_id, span);
            }
            // The receiver is transferred exactly when the method takes
            // `self` by value.
            let receiver_consumed = cx.callees.owned_self.contains(&key);
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
            elab_store(target, value, owned, cx);
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
    owned.push(Slot::from(Live {
        local,
        name,
        type_id,
        temporary: true,
    }));
}

fn release_local(owned: &mut Owned, local: u32) {
    if let Some(slot) = slot_of(owned, local) {
        slot.held = false;
    }
}

/// The slot of the binding `local`, where it holds a resource.
fn slot_of(owned: &mut Owned, local: u32) -> Option<&mut Slot> {
    owned.iter_mut().find(|slot| slot.live.local == local)
}

/// What a store into `target` replaces drops once the stored `value` is
/// computed, since computing it may read what it replaces. A binding whose
/// value was transferred owns the one stored into it.
fn elab_store(target: &TirExpr, value: &mut TirExpr, owned: &mut Owned, cx: &mut Cx) {
    if !cx.carries_resource(target.type_id) {
        return;
    }
    let Some(slot) = place_root(target).and_then(|root| slot_of(owned, root)) else {
        return;
    };
    if slot.held {
        let replaced = drop_value(target.clone(), target.type_id, cx);
        let type_id = value.type_id;
        let (local, name) = cx.alloc_local(type_id, "stored");
        wrap_in_block(value, |computed| {
            let mut stmts = vec![let_stmt(&name, local, type_id, computed)];
            stmts.extend(replaced);
            stmts.push(expr_stmt(local_ref(local, &name, type_id)));
            stmts
        });
    }
    if matches!(target.kind, TirExprKind::Local { .. }) {
        slot.held = true;
    }
}

/// The binding a place expression (`x`, `x.a.b`) is a part of.
fn place_root(place: &TirExpr) -> Option<u32> {
    match &place.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr, .. } => place_root(expr),
        _ => None,
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
        arm_owned.extend(arm_bindings.into_iter().map(Slot::from));
        if let Some(guard) = arm.guard.as_mut() {
            let guard_entry = arm_owned.len();
            elab_expr_scope(guard, &mut arm_owned, guard_entry, cx);
        }
        let flow = elab_arm_body(&mut arm.body, &mut arm_owned, cx, entry);
        arm_states.push((arm_owned, flow));
    }
    for (arm, drops) in arms.iter_mut().zip(merge_paths(owned, &mut arm_states, cx)) {
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
