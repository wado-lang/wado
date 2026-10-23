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

use crate::compiler_item::{CompilerItem, SeqField};
use crate::component_model::CmInterfaceRegistry;
use crate::defs::DefId;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{
    DROP_GLUE_PREFIX, Receiver, checked_name, drop_glue_name, is_contract_holder,
    is_template_block, minted_name,
};
use crate::package::Package;
use crate::resource_holding::{Held, HeldTypes, holds_resource};
use crate::synthesis::cm_binding::{end_drop_call, resource_drop_call};
use crate::synthesis::common;
use crate::synthesis::common::{
    assign, binary, builtin_call, deref_expr, expr_stmt, if_stmt, let_mut_stmt, let_stmt,
    local_ref, not_expr, synth_span,
};
use crate::tir;
use crate::tir::{
    FunctionRef, ResolvedType, StructDef, StructFields, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirTemplatePart, TirUnaryOp, TypeId,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};
use crate::token::Span;
use crate::type_table::{TypeKey, TypeTable};

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

/// An open scope: the affine locals it declared, and which statement owns the
/// temporaries among them.
struct Scope {
    lives: Vec<Live>,
    temporaries: TemporaryScope,
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
    /// The frame's local slots after its first `local_base`: a closure's
    /// parameters precede its body locals and are not listed.
    locals: &'a mut Vec<TirLocal>,
    local_base: u32,
    /// The function's local count, kept in step; a closure keeps none.
    local_count: Option<&'a mut u32>,
    /// The affine locals each open scope declared, outermost first.
    scopes: Vec<Scope>,
    /// The declarations of the temporaries spilled since the statement owning
    /// them began, which go ahead of that statement: its drops read them after
    /// the expression that assigns them, so no block inside it may declare them.
    declarations: Vec<TirStmt>,
    /// The enclosing loops and labeled blocks, innermost last.
    targets: Vec<BreakTarget>,
    /// The holders whose drop is being expanded, outermost first.
    expanding: Vec<TypeKey>,
    glue: &'a mut DropGlue,
    /// The module of the function being elaborated.
    module: ModuleSource,
    /// The `self` of a `Drop::drop` body, which drops what it still holds field
    /// by field: dropping it whole would run `drop` again.
    drop_self: Option<u32>,
}

/// The drop glue functions called so far, by name, each with the type it
/// drops. They go in `entry`, the entry module.
struct DropGlue {
    entry: ModuleSource,
    called: IndexMap<String, TypeId>,
    /// The glue that exists already, from elaborating before monomorphization.
    prebuilt: IndexSet<String>,
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
            .lives
            .push(live);
    }

    fn open_scope(&mut self, lives: Vec<Live>, temporaries: TemporaryScope) {
        self.scopes.push(Scope { lives, temporaries });
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
    /// declared, innermost first. The temporaries of the scopes from `depth`
    /// that defer theirs are left out: the statement owning them lies outside
    /// the jump, which lands inside it.
    fn drop_scopes_from(&mut self, depth: usize) -> Vec<TirStmt> {
        let scopes = &self.scopes[depth..];
        let deferring = scopes
            .iter()
            .take_while(|scope| scope.temporaries == TemporaryScope::Enclosing)
            .count();
        let lives: Vec<Live> = scopes
            .iter()
            .enumerate()
            .rev()
            .flat_map(|(idx, scope)| {
                let kept = idx < deferring;
                scope
                    .lives
                    .iter()
                    .rev()
                    .filter(move |live| !(kept && live.temporary))
                    .cloned()
            })
            .collect();
        drop_lives(&lives, self)
    }
}

/// What a type holds that this pass drops, one level down.
enum Layout<'a> {
    /// A resource with a Component Model name, released by its `drop`.
    Resource,
    /// A stream or future end, released by its own canonical drop.
    End,
    /// A `List` of `elem`, whose backing array of type `slots` its drop glue
    /// walks.
    List {
        decl: Option<DefId>,
        slots: TypeId,
        elem: TypeId,
        fields: Vec<(u32, String, TypeId)>,
    },
    /// An `Array` of `elem`, whose drop glue walks every slot.
    Array { elem: TypeId },
    /// An aggregate, instantiating `decl` with `args` where it has a
    /// declaration, and dropped by `drop_impl` where it implements `Drop`.
    Holder {
        decl: Option<DefId>,
        args: Vec<TypeId>,
        parts: Parts,
        drop_impl: Option<&'a FunctionRef>,
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
    /// Each type's `Drop::drop`, by the type's declaration.
    drop_impls: &'a DropImpls,
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
            let named = self
                .reg
                .get_resource_cm_name_by_module(&tt.def_module(def).to_string(), tt.def_name(def))
                .is_some();
            return if named {
                Layout::Resource
            } else {
                Layout::Opaque
            };
        }
        if let ResolvedType::GenericResource { .. } = resolved {
            return Layout::End;
        }
        if let ResolvedType::BuiltinArray(elem) = resolved {
            return Layout::Array { elem };
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
                drop_impl: None,
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
        // Monomorphization registers each instance's fields under its arguments.
        let parts = if let Some(fields) = self.struct_fields.get(&(def, args.clone())) {
            Parts::Fields(
                fields
                    .iter()
                    .map(|field| (field.index, field.name.clone(), field.type_id))
                    .collect(),
            )
        } else if let Some(fields) = self.struct_fields.get(&(def, Vec::new())) {
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
        if tt.is_compiler_item_type(base, CompilerItem::List) {
            let Parts::Fields(fields) = parts else {
                unreachable!("`List` is a struct");
            };
            return Layout::List {
                decl,
                slots: fields[SeqField::Backing.index() as usize].2,
                elem: args[0],
                fields,
            };
        }
        let drop_impl = decl.and_then(|decl| self.drop_impls.get(&decl));
        Layout::Holder {
            decl,
            args,
            parts,
            drop_impl,
        }
    }

    /// Whether a place holding a `type_id` holds an object that a store writes
    /// into: a resource's box or a struct.
    fn is_object(&self, type_id: TypeId) -> bool {
        match self.layout(type_id) {
            Layout::Resource | Layout::End | Layout::List { .. } => true,
            Layout::Holder {
                parts: Parts::Fields(_),
                ..
            } => {
                let tt = self.tt.borrow();
                tt.as_tuple(tt.representation_head(type_id)).is_none()
            }
            Layout::Array { .. } | Layout::Holder { .. } | Layout::Opaque => false,
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

impl HeldTypes for Resources<'_> {
    fn key(&self, type_id: TypeId) -> TypeKey {
        self.tt.borrow().type_key(type_id)
    }

    fn held(&self, type_id: TypeId) -> Held {
        match self.layout(type_id) {
            Layout::Resource
            | Layout::End
            | Layout::Holder {
                drop_impl: Some(_), ..
            } => Held::Resource,
            Layout::List { decl, elem, .. } => Held::Members {
                decl,
                args: vec![elem],
                members: vec![elem],
            },
            Layout::Array { elem } => Held::Members {
                decl: None,
                args: vec![elem],
                members: vec![elem],
            },
            Layout::Holder {
                decl, args, parts, ..
            } => Held::Members {
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

/// Entry point: elaborate resource drops for every function in the project.
pub fn elaborate_resource_drops(project: &mut Package) {
    let Some(type_table) = project
        .tir_modules
        .values()
        .next()
        .map(|m| m.type_table.clone())
    else {
        return;
    };
    let struct_fields: StructFields = project
        .tir_modules
        .values()
        .flat_map(|module| tir::struct_fields(&module.structs))
        .collect();
    let drop_impls = drop_impls(
        project.tir_modules.iter().flat_map(|(source, module)| {
            module.functions.iter().map(|func| (source.clone(), func))
        }),
        &type_table.borrow(),
    );
    let answers = RefCell::default();
    let resources = Resources {
        tt: &type_table,
        reg: &project.cm_interface_registry,
        struct_fields: &struct_fields,
        drop_impls: &drop_impls,
        answers: &answers,
    };

    let mut glue = DropGlue {
        entry: project.entry_module_source.clone(),
        called: IndexMap::default(),
        prebuilt: IndexSet::default(),
    };
    // A generic body is elaborated once per instance, where its types are
    // known: see `elaborate_instance_drops`.
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            if func_rc.borrow().is_template() {
                continue;
            }
            elaborate_function(&mut func_rc.borrow_mut(), resources, &mut glue);
        }
    }
    let glue_functions = build_called_glue(resources, &mut glue);
    let entry = project
        .tir_modules
        .get_mut(&project.entry_module_source)
        .expect("the entry module is loaded");
    for function in glue_functions {
        entry.functions.push(Rc::new(RefCell::new(function)));
    }
}

/// What monomorphization takes out of a [`FlatPackage`] that elaborating its
/// instances still reads.
pub struct Generics {
    /// Each type's `Drop::drop`, a generic one by its template.
    drop_impls: DropImpls,
    /// The fields of each generic struct. A `List` instance is never made a
    /// struct of its own, so its fields are read from here.
    struct_fields: StructFields,
}

impl Generics {
    /// The generics of `flat`, read before monomorphization takes them out.
    pub fn of(flat: &FlatPackage) -> Self {
        Self {
            drop_impls: drop_impls(
                flat.functions
                    .iter()
                    .map(|func| (func.borrow().module_source.clone(), func)),
                &flat.type_table.borrow(),
            ),
            struct_fields: flat
                .structs
                .iter()
                .filter(|s| !s.type_params.is_empty())
                .map(|s| ((s.def, Vec::new()), s.fields.clone()))
                .collect(),
        }
    }
}

/// Run `f` with the [`Resources`] of `flat`.
fn with_flat_resources<R>(
    flat: &FlatPackage,
    generics: &Generics,
    f: impl FnOnce(Resources) -> R,
) -> R {
    let mut struct_fields = generics.struct_fields.clone();
    struct_fields.extend(tir::struct_fields(&flat.structs));
    let answers = RefCell::default();
    f(Resources {
        tt: &flat.type_table,
        reg: &flat.cm_interface_registry,
        struct_fields: &struct_fields,
        drop_impls: &generics.drop_impls,
        answers: &answers,
    })
}

/// Elaborate the drops of every instance among `flat.functions` from `first`
/// on, a generic body whose types are now known (WEP: Affine Types).
pub fn elaborate_instance_drops(flat: &mut FlatPackage, first: usize, generics: &Generics) {
    let glue_functions = with_flat_resources(flat, generics, |resources| {
        elaborate_instances(flat, first, resources)
    });
    flat.functions
        .extend(glue_functions.into_iter().map(|f| Rc::new(RefCell::new(f))));
}

/// Elaborate the instances `elaborate_instance_drops` names, returning the
/// glue they call that does not exist yet.
fn elaborate_instances(flat: &FlatPackage, first: usize, resources: Resources) -> Vec<TirFunction> {
    let mut glue = DropGlue {
        entry: flat.entry_module_source.clone(),
        called: IndexMap::default(),
        prebuilt: flat
            .functions
            .iter()
            .map(|func| func.borrow().name.clone())
            .filter(|name| name.starts_with(DROP_GLUE_PREFIX))
            .collect(),
    };
    for func_rc in &flat.functions[first..] {
        if func_rc.borrow().monomorph_info.is_none() {
            continue;
        }
        elaborate_function(&mut func_rc.borrow_mut(), resources, &mut glue);
    }
    build_called_glue(resources, &mut glue)
}

/// Build the glue called so far and not built already. A glue body can call
/// glue of its own, so the list grows while it is built.
fn build_called_glue(resources: Resources, glue: &mut DropGlue) -> Vec<TirFunction> {
    let mut glue_functions = Vec::new();
    let mut next = 0;
    while let Some((name, type_id)) = glue
        .called
        .get_index(next)
        .map(|(name, type_id)| (name.clone(), *type_id))
    {
        next += 1;
        if glue.prebuilt.contains(&name) {
            continue;
        }
        glue_functions.push(build_drop_glue(name, type_id, resources, glue));
    }
    glue_functions
}

/// The function that drops a value of `type_id`, a holder that holds itself:
/// expanding its drop in place would never end.
fn build_drop_glue(
    name: String,
    type_id: TypeId,
    resources: Resources,
    glue: &mut DropGlue,
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
        locals: &mut locals,
        local_base: 0,
        local_count: Some(&mut local_count),
        scopes: Vec::new(),
        declarations: Vec::new(),
        targets: Vec::new(),
        expanding: Vec::new(),
        glue,
        module: entry.clone(),
        drop_self: None,
    };
    let value = local_ref(0, "value", type_id);
    let stmts = match cx.resources.layout(type_id) {
        Layout::List { slots, elem, .. } => drop_list_slots(value, slots, elem, &mut cx),
        Layout::Array { elem } => drop_array_slots(value, elem, &mut cx),
        Layout::Holder {
            drop_impl: Some(drop_impl),
            ..
        } => vec![call_drop_impl(value, drop_impl)],
        _ => drop_value(value, type_id, &mut cx),
    };
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
fn elaborate_function(func: &mut TirFunction, resources: Resources, glue: &mut DropGlue) {
    if copied_to_callers(func) {
        strip_moves(func);
        return;
    }
    let implements_drop = drop_impl_of(func, &resources.tt.borrow()).is_some();
    let TirFunction {
        body: Some(body),
        params,
        locals,
        local_count,
        module_source,
        return_type,
        ..
    } = func
    else {
        return;
    };
    let drop_self = implements_drop.then(|| params[0].local_index);

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
        StripMoves.visit_block(body);
        return;
    }

    let mut cx = Cx {
        resources,
        locals,
        local_base: 0,
        local_count: Some(local_count),
        scopes: Vec::new(),
        declarations: Vec::new(),
        targets: Vec::new(),
        expanding: Vec::new(),
        glue,
        module: module_source.clone(),
        drop_self,
    };
    let yields = yields(*return_type);
    elab_scope(body, owned, TemporaryScope::Statement, yields, &mut cx);
}

/// Each type's `Drop::drop`, by the type's declaration.
type DropImpls = IndexMap<DefId, FunctionRef>;

/// The `Drop::drop` impls among `functions`, each with the module defining it:
/// before link a function's own `module_source` is not set. An instance is
/// left out, so a generic impl is named by its template.
fn drop_impls<'f>(
    functions: impl IntoIterator<Item = (ModuleSource, &'f Rc<RefCell<TirFunction>>)>,
    tt: &TypeTable,
) -> DropImpls {
    functions
        .into_iter()
        .filter_map(|(module, func_rc)| {
            let func = func_rc.borrow();
            if func.monomorph_info.is_some() {
                return None;
            }
            let implementor = drop_impl_of(&func, tt)?;
            Some((implementor, FunctionRef::from_resolved(&func, module)))
        })
        .collect()
}

/// The type `func` implements `Drop` for, where `func` is that impl's `drop`.
fn drop_impl_of(func: &TirFunction, tt: &TypeTable) -> Option<DefId> {
    let info = func.method_info.as_ref()?;
    let implemented = info.trait_name.as_ref()?.canonical()?;
    if !tt.is_compiler_item(implemented, CompilerItem::Drop) {
        return None;
    }
    let Receiver::Type(implementor) = info.receiver() else {
        unreachable!("an impl names the type it implements a trait for");
    };
    implementor.head().def()
}

/// Whether lowering copies `func` to each caller, so it owns no parameter: a
/// trait's clause holder, clauses alone, or a function's `contract` clauses and
/// a call handing every parameter to its `unchecked` twin, which owns them.
fn copied_to_callers(func: &TirFunction) -> bool {
    if is_contract_holder(&func.name) {
        return true;
    }
    let Some(TirStmt {
        kind: TirStmtKind::Expr(call) | TirStmtKind::Return { value: Some(call) },
        ..
    }) = func.body.as_ref().and_then(|body| body.stmts.last())
    else {
        return false;
    };
    matches!(&call.kind, TirExprKind::Call { func: callee, .. }
        if callee.name != func.name && checked_name(&callee.name) == func.name)
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
        let moves = matches!(&expr.kind, TirExprKind::Call { func, args, .. }
        if func.intrinsic().is_some_and(|name| {
            MovingBuiltin::named(name, args.first().map(|arg| &arg.expr), self.resources).is_some()
        }));
        if moves || self.resources.carried_by(expr.type_id) {
            self.found = true;
            return;
        }
        // Into closures too: a body owning nothing affine still holds the
        // closures whose drops this pass elaborates.
        self.walk_expr(expr);
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
        .flat_map(|live| {
            if cx.drop_self != Some(live.local) {
                return drop_if_held(live.place(), live.type_id, cx);
            }
            let Layout::Holder { parts, .. } = cx.resources.layout(live.type_id) else {
                unreachable!("only a holder implements `Drop`");
            };
            let drops = drop_parts(live.place(), live.type_id, parts, cx);
            unless_moved(live.place(), drops)
        })
        .collect()
}

/// Drop what the place `place` holds, where a move has not emptied it.
fn drop_if_held(place: TirExpr, type_id: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    let drops = drop_value(place.clone(), type_id, cx);
    unless_moved(place, drops)
}

/// `drops`, run where a move has not emptied `place`.
fn unless_moved(place: TirExpr, drops: Vec<TirStmt>) -> Vec<TirStmt> {
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
        Layout::Resource => vec![expr_stmt(resource_drop_call(
            scrutinee,
            type_id,
            &cx.resources.tt.borrow(),
        ))],
        Layout::End if is_future_writable(type_id, cx) => vec![unwritten_future_trap(type_id, cx)],
        Layout::End => vec![expr_stmt(end_drop_call(
            scrutinee,
            type_id,
            &cx.resources.tt.borrow(),
        ))],
        Layout::List { .. } | Layout::Array { .. } => drop_through_glue(scrutinee, type_id, cx),
        // A generic impl's instance is monomorphization's to make. It scans
        // the glue this pass adds, but not an instance body this pass
        // elaborates, which it has already scanned.
        Layout::Holder {
            drop_impl: Some(_),
            args,
            ..
        } if !args.is_empty() => drop_through_glue(scrutinee, type_id, cx),
        Layout::Holder {
            drop_impl: Some(drop_impl),
            ..
        } => vec![call_drop_impl(scrutinee, drop_impl)],
        Layout::Holder { parts, .. } => drop_parts(scrutinee, type_id, parts, cx),
        Layout::Opaque => Vec::new(),
    }
}

fn call_drop_impl(scrutinee: TirExpr, drop_impl: &FunctionRef) -> TirStmt {
    expr_stmt(TirExpr::new(
        TirExprKind::method_call(Box::new(scrutinee), drop_impl.clone(), vec![], vec![]),
        TypeTable::UNIT,
        synth_span(),
    ))
}

fn is_future_writable(type_id: TypeId, cx: &Cx) -> bool {
    let tt = cx.resources.tt.borrow();
    tt.is_compiler_item_type(
        tt.representation_head(type_id),
        CompilerItem::FutureWritable,
    )
}

/// A `FutureWritable` reaching a drop was never written, since its write
/// consumes it. The Component Model traps on that drop, and nothing a drop
/// could write stands in for the value the reader waits for, so the trap names
/// the end (WEP: Affine Types).
fn unwritten_future_trap(type_id: TypeId, cx: &Cx) -> TirStmt {
    let type_name = cx.resources.tt.borrow().type_name(type_id);
    expr_stmt(panic_with(
        format!("`{type_name}` dropped before its write: its reader would wait forever"),
        cx,
    ))
}

/// A `panic(message)` call.
fn panic_with(message: String, cx: &Cx) -> TirExpr {
    let span = synth_span();
    let string_ty = {
        let tt = cx.resources.tt.borrow();
        let def = tt.require_compiler_item_def(CompilerItem::String);
        tt.find_struct_type(StructDef::Decl(def))
            .expect("`String` is in the type table")
    };
    common::panic_call(common::string_lit(message, string_ty, span), span)
}

/// Drop each part of `scrutinee`, a holder of type `type_id`, and not the
/// holder as a whole.
fn drop_parts(scrutinee: TirExpr, type_id: TypeId, parts: Parts, cx: &mut Cx) -> Vec<TirStmt> {
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
/// emptied it, and leave it null. `scrutinee` is a place expression (a local
/// reference), so cloning it per field is cheap.
fn drop_projected(
    scrutinee: TirExpr,
    fields: &[(u32, String, TypeId)],
    cx: &mut Cx,
) -> Vec<TirStmt> {
    let mut stmts = Vec::new();
    for field in affine_fields(&scrutinee, fields, cx) {
        let field_ty = field.type_id;
        stmts.extend(drop_if_held(field.clone(), field_ty, cx));
        stmts.push(empty_place(field));
    }
    stmts
}

/// The resource-carrying fields of `scrutinee`, each projected.
fn affine_fields(scrutinee: &TirExpr, fields: &[(u32, String, TypeId)], cx: &Cx) -> Vec<TirExpr> {
    fields
        .iter()
        .filter(|(_, _, field_ty)| cx.carries_resource(*field_ty))
        .map(|(index, name, field_ty)| {
            common::field_access(
                scrutinee.clone(),
                *index,
                name.clone(),
                *field_ty,
                synth_span(),
            )
        })
        .collect()
}

/// `place = null`: what a move leaves in the place it moved out of.
fn empty_place(place: TirExpr) -> TirStmt {
    let null = TirExpr::new(TirExprKind::Null, place.type_id, synth_span());
    expr_stmt(assign(place, null))
}

/// Drop each slot of the list `list` below its length, where a move has not
/// emptied it. A slot at or past the length holds nothing.
fn drop_list_slots(list: TirExpr, slots_ty: TypeId, elem: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    let span = synth_span();
    let field = |seq: SeqField, type_id| {
        common::field_access(
            list.clone(),
            seq.index(),
            seq.field_name().to_string(),
            type_id,
            span,
        )
    };
    let (slots, slots_name) = cx.alloc_local(slots_ty, "slots");
    let slots_ref_ty = cx.resources.tt.borrow_mut().make_ref(slots_ty);
    let array = common::ref_expr(local_ref(slots, &slots_name, slots_ty), slots_ref_ty, span);
    let mut stmts = vec![let_stmt(
        &slots_name,
        slots,
        slots_ty,
        field(SeqField::Backing, slots_ty),
    )];
    stmts.extend(drop_slots(
        &array,
        common::i32_const(0),
        field(SeqField::Len, TypeTable::I32),
        elem,
        cx,
    ));
    stmts
}

/// Drop each slot of `array`, where a move has not emptied it.
fn drop_array_slots(array: TirExpr, elem: TypeId, cx: &mut Cx) -> Vec<TirStmt> {
    let span = synth_span();
    let array_ref_ty = cx.resources.tt.borrow_mut().make_ref(array.type_id);
    let array = common::ref_expr(array, array_ref_ty, span);
    let len = builtin_call("array_len", vec![array.clone()], TypeTable::I32);
    drop_slots(&array, common::i32_const(0), len, elem, cx)
}

/// The loop that drops each slot of `array` from `start` up to `end`, where a
/// move has not emptied it.
fn drop_slots(
    array: &TirExpr,
    start: TirExpr,
    end: TirExpr,
    elem: TypeId,
    cx: &mut Cx,
) -> Vec<TirStmt> {
    let span = synth_span();
    let (end_local, end_name) = cx.alloc_local(TypeTable::I32, "end");
    let (index, index_name) = cx.alloc_local(TypeTable::I32, "index");
    cx.locals.last_mut().expect("allocated above").is_mut = true;
    let (slot, slot_name) = cx.alloc_local(elem, "slot");
    let index_ref = || local_ref(index, &index_name, TypeTable::I32);
    let in_range = binary(
        TirBinaryOp::Lt,
        index_ref(),
        local_ref(end_local, &end_name, TypeTable::I32),
        TypeTable::BOOL,
    );
    let mut body = vec![
        if_stmt(
            not_expr(in_range, span),
            TirBlock::new(vec![common::break_stmt()], span),
            None,
        ),
        let_stmt(&slot_name, slot, elem, read_slot(array, index_ref(), elem)),
    ];
    body.extend(drop_if_held(local_ref(slot, &slot_name, elem), elem, cx));
    body.push(expr_stmt(assign(
        index_ref(),
        binary(
            TirBinaryOp::Add,
            index_ref(),
            common::i32_const(1),
            TypeTable::I32,
        ),
    )));
    vec![
        let_stmt(&end_name, end_local, TypeTable::I32, end),
        common::let_mut_stmt(&index_name, index, TypeTable::I32, start),
        common::loop_stmt(TirBlock::new(body, span)),
    ]
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
        consumes: true,
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

/// Where a block's last statement's value goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    /// Nowhere: it is discarded as any other statement's is.
    Discarded,
    /// To what consumes the block.
    Consumed,
    /// To a referencing position, which reads the place the block ends in
    /// (see [`is_temporary`]).
    Referenced,
}

/// Where the value of a block producing `type_id` goes when it is consumed.
fn yields(type_id: TypeId) -> Tail {
    if type_id == TypeTable::UNIT || type_id == TypeTable::NEVER {
        Tail::Discarded
    } else {
        Tail::Consumed
    }
}

/// Elaborate `block` in place as a scope that opens declaring `declared`. Its
/// locals drop at its end where it falls through, and its temporaries where
/// `temporaries` says. Its last statement's value goes where `tail` says.
fn elab_scope(
    block: &mut TirBlock,
    declared: Vec<Live>,
    temporaries: TemporaryScope,
    tail: Tail,
    cx: &mut Cx,
) -> Flow {
    cx.open_scope(declared, temporaries);
    let stmts = std::mem::take(&mut block.stmts);
    let mut out: Vec<TirStmt> = Vec::new();
    let mut flow = Flow::Normal;
    let last = stmts.len().wrapping_sub(1);
    for (idx, mut stmt) in stmts.into_iter().enumerate() {
        if flow == Flow::Diverged {
            // No path reaches it, so nothing drops there and a move takes nothing.
            StripMoves.visit_stmt(&mut stmt);
            out.push(stmt);
            continue;
        }
        let stmt_tail = if idx == last { tail } else { Tail::Discarded };
        let first_declared = cx.scopes.last().expect("pushed above").lives.len();
        let first_out = out.len();
        let first_declaration = cx.declarations.len();
        flow = elab_stmt(stmt, cx, stmt_tail, &mut out);
        if temporaries == TemporaryScope::Statement {
            if flow == Flow::Normal {
                let spilled = take_temporaries(cx, first_declared);
                let drops = drop_lives(&spilled, cx);
                if stmt_tail == Tail::Discarded {
                    out.extend(drops);
                } else {
                    // The block's value is this statement's: keep it past the drops.
                    let tail = out.split_off(first_out);
                    out.extend(append_block_drops(tail, drops, cx));
                }
            }
            let declarations = cx.declarations.split_off(first_declaration);
            out.splice(first_out..first_out, declarations);
        }
    }
    let scope = cx.scopes.pop().expect("pushed above");
    let (deferred, lives): (Vec<Live>, Vec<Live>) = scope
        .lives
        .into_iter()
        .partition(|live| live.temporary && temporaries == TemporaryScope::Enclosing);
    if flow == Flow::Normal {
        let lives: Vec<Live> = lives.into_iter().rev().collect();
        let drops = drop_lives(&lives, cx);
        out = append_block_drops(out, drops, cx);
    }
    if let Some(enclosing) = cx.scopes.last_mut() {
        enclosing.lives.extend(deferred);
    }
    block.stmts = out;
    flow
}

/// Take the temporaries the innermost scope declared from `first` on,
/// innermost first.
fn take_temporaries(cx: &mut Cx, first: usize) -> Vec<Live> {
    let scope = cx.scopes.last_mut().expect("a scope is open");
    let (temporaries, kept): (Vec<Live>, Vec<Live>) =
        scope.lives.drain(first..).partition(|live| live.temporary);
    scope.lives.extend(kept);
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
fn elab_block(block: &mut TirBlock, tail: Tail, cx: &mut Cx) -> Flow {
    elab_scope(block, Vec::new(), TemporaryScope::Statement, tail, cx)
}

/// Elaborate a plain block expression in place. Nothing can leave it but a
/// `return` or a `break` past it, so it runs whole as part of the statement
/// holding it, and its temporaries drop when that statement ends. Synthesized
/// code relies on that: an `assert` captures its operand `f(&t())` as
/// `f({ $v = &t(); $v })`, so `t()` must outlive the capture's statement.
fn elab_plain_block(block: &mut TirBlock, tail: Tail, cx: &mut Cx) {
    elab_scope(block, Vec::new(), TemporaryScope::Enclosing, tail, cx);
}

/// Elaborate `block`, the body `target` is reached from. An expanded template
/// string's block is part of the statement holding the template, as a plain
/// block is: its temporaries outlive the string it breaks out with.
fn elab_target_body(block: &mut TirBlock, label: Option<String>, tail: Tail, cx: &mut Cx) -> Flow {
    let temporaries = if label.as_deref().is_some_and(is_template_block) {
        TemporaryScope::Enclosing
    } else {
        TemporaryScope::Statement
    };
    cx.targets.push(BreakTarget {
        label,
        depth: cx.scopes.len(),
    });
    let flow = elab_scope(block, Vec::new(), temporaries, tail, cx);
    cx.targets.pop();
    flow
}

/// Elaborate one statement, appending the result to `out`. Returns whether
/// control falls through to the next statement.
fn elab_stmt(stmt: TirStmt, cx: &mut Cx, tail: Tail, out: &mut Vec<TirStmt>) -> Flow {
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

        // The pattern consumes its value as an arm's does: the parts no
        // binding takes drop once the bindings hold theirs.
        TirStmtKind::LetDestructure { pattern, mut value } => {
            let bindings = cx.resources.pattern_bindings(&pattern);
            let extracts = !bindings.is_empty();
            let entry = consumed_scrutinee(&mut value, &[(&pattern, true)], extracts, cx)
                .map_or_else(Vec::new, |place| consume_matched(&pattern, place, cx));
            for live in bindings {
                cx.declare(live);
            }
            out.push(TirStmt {
                kind: TirStmtKind::LetDestructure { pattern, value },
                span,
            });
            out.extend(entry);
            Flow::Normal
        }

        TirStmtKind::Expr(expr) => {
            let mut elaborated = expr;
            elab_expr(&mut elaborated, tail != Tail::Referenced, cx);
            // An assignment is typed as the value it stores, which its target
            // now owns: nothing is discarded.
            let discards = !matches!(elaborated.kind, TirExprKind::Assign { .. });
            if tail == Tail::Discarded && discards && cx.carries_resource(elaborated.type_id) {
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
            let then_flow = elab_block(&mut then_block, Tail::Discarded, cx);
            let (else_block, else_flow) = match else_block {
                Some(mut block) => {
                    let flow = elab_block(&mut block, Tail::Discarded, cx);
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
            elab_target_body(&mut body, None, Tail::Discarded, cx);
            out.push(TirStmt {
                kind: TirStmtKind::Loop { body },
                span,
            });
            Flow::Normal
        }

        TirStmtKind::LabeledBlock { label, mut block } => {
            elab_target_body(&mut block, Some(label.clone()), Tail::Discarded, cx);
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
    if !consuming && is_temporary(expr, cx) && cx.carries_resource(expr.type_id) {
        elab_expr(expr, true, cx);
        spill_temporary(expr, cx);
        return;
    }
    let type_id = expr.type_id;
    let span = expr.span;
    let mut dropped = None;
    let mut stores = false;
    let mut moving = None;
    match &mut expr.kind {
        TirExprKind::Move { .. } => {
            assert!(
                consuming,
                "a move sits where its value is consumed: {span:?}"
            );
            unwrap_move(expr);
            take_moved(expr, cx);
        }
        TirExprKind::Local { .. } => {
            assert!(
                !consuming || !cx.carries_resource(type_id),
                "a place a resource moves out of is marked `Move`: {span:?}"
            );
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
            assert!(
                !consuming || !cx.carries_resource(type_id) || !is_place(inner, cx),
                "a payload a resource moves out of is marked `Move`: {span:?}"
            );
            elab_expr(inner, consuming, cx);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            if consuming && cx.carries_resource(type_id) {
                assert!(
                    !is_place(inner, cx),
                    "a field a resource moves out of is marked `Move`: {span:?}"
                );
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
            let (receiver, rest) = args.split_mut();
            // `is_moved` reads the place it tests; taking it would empty it.
            let consumes_args = !func.is_builtin_named("is_moved");
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
                elab_expr(&mut arg.expr, consumes_args, cx);
            }
            moving = func.intrinsic().and_then(|name| {
                MovingBuiltin::named(name, args.first().map(|arg| &arg.expr), cx.resources)
            });
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
            stores = cx.carries_resource(target.type_id);
        }
        // An element read leaves its container whole.
        TirExprKind::Index { expr: base, index } => {
            elab_expr(base, false, cx);
            elab_expr(index, true, cx);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            elab_expr(value, true, cx);
        }

        TirExprKind::Block(block) => {
            let tail = match yields(type_id) {
                Tail::Consumed if !consuming => Tail::Referenced,
                tail => tail,
            };
            elab_plain_block(block, tail, cx);
        }
        TirExprKind::LabeledBlock { label, block, .. } => {
            elab_target_body(block, Some(label.clone()), yields(type_id), cx);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            elab_expr(condition, true, cx);
            elab_block(then_branch, yields(type_id), cx);
            if let Some(else_branch) = else_branch {
                elab_block(else_branch, yields(type_id), cx);
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
            elab_block(body, yields(type_id), cx);
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
    if stores {
        elab_store(expr, cx);
    }
    if let Some(builtin) = moving {
        elab_moving_builtin(expr, builtin, cx);
    }
}

/// A storage builtin that moves an affine element, the places `core` is
/// trusted rather than checked (WEP: Affine Types).
#[derive(Clone, Copy)]
enum MovingBuiltin {
    /// `replace(r, v)`: store `v` where `r` points and return what was there.
    /// It has no lowering of its own, so it expands for every type.
    Replace,
    /// `array_take`: reads a slot, and empties it where the element is affine.
    /// Like `replace`, it expands for every type.
    Take { elem: TypeId, affine: bool },
    /// `array_set`: drops what the slot held, and writes into the object a
    /// slot holding one holds.
    Set(TypeId),
    /// `array_move`: empties the source slots the destination does not cover.
    Move,
    /// `array_release`: drops what the slots held.
    Release(TypeId),
}

impl MovingBuiltin {
    /// The moving builtin `name` names, given its first argument `first`; an
    /// array builtin over an element that is not affine moves nothing.
    fn named(name: &str, first: Option<&TirExpr>, resources: Resources) -> Option<MovingBuiltin> {
        match name {
            "replace" => return Some(MovingBuiltin::Replace),
            "array_take" | "array_set" | "array_move" | "array_release" => {}
            _ => return None,
        }
        let array = first.expect("an array builtin takes its array first");
        let elem = {
            let tt = resources.tt.borrow();
            match tt.get(tt.peel_refs(array.type_id)) {
                ResolvedType::BuiltinArray(elem) => *elem,
                other => unreachable!("an array builtin takes an array, not {other:?}"),
            }
        };
        let affine = resources.carried_by(elem);
        Some(match name {
            "array_take" => MovingBuiltin::Take { elem, affine },
            _ if !affine => return None,
            "array_set" => MovingBuiltin::Set(elem),
            "array_move" => MovingBuiltin::Move,
            _ => MovingBuiltin::Release(elem),
        })
    }
}

/// Expand the call `expr` of `builtin`, its arguments elaborated already.
fn elab_moving_builtin(expr: &mut TirExpr, builtin: MovingBuiltin, cx: &mut Cx) {
    let span = expr.span;
    let type_id = expr.type_id;
    let TirExprKind::Call { func, args, .. } = &mut expr.kind else {
        unreachable!("a builtin is called");
    };
    let name = func.intrinsic().expect("a builtin").to_string();
    let mut stmts = Vec::new();
    let args: Vec<TirExpr> = std::mem::take(args)
        .into_vec()
        .into_iter()
        .map(|arg| {
            let ty = arg.expr.type_id;
            let (local, local_name) = cx.alloc_local(ty, "arg");
            stmts.push(let_stmt(&local_name, local, ty, arg.expr));
            local_ref(local, &local_name, ty)
        })
        .collect();
    let raw = |args: &[TirExpr]| builtin_call(&name, args.to_vec(), type_id);
    let tail = match builtin {
        MovingBuiltin::Replace => Some(replace_through(&args[0], &args[1], &mut stmts, cx)),
        MovingBuiltin::Take { elem, affine } => {
            let read = read_slot(&args[0], args[1].clone(), elem);
            if affine {
                let (local, local_name) = cx.alloc_local(elem, "taken");
                stmts.push(let_stmt(&local_name, local, elem, read));
                stmts.push(release_slots(
                    &args[0],
                    args[1].clone(),
                    common::i32_const(1),
                ));
                Some(local_ref(local, &local_name, elem))
            } else {
                Some(read)
            }
        }
        MovingBuiltin::Set(elem) => {
            let (local, local_name) = cx.alloc_local(elem, "held");
            stmts.push(let_stmt(
                &local_name,
                local,
                elem,
                read_slot(&args[0], args[1].clone(), elem),
            ));
            let held = local_ref(local, &local_name, elem);
            if cx.resources.is_object(elem) {
                let repoint = expr_stmt(raw(&args));
                stmts.push(store_into_object(held, args[2].clone(), repoint, cx));
            } else {
                stmts.extend(drop_if_held(held, elem, cx));
                stmts.push(expr_stmt(raw(&args)));
            }
            None
        }
        MovingBuiltin::Move => {
            stmts.push(expr_stmt(raw(&args)));
            stmts.extend(vacate_moved(&args, cx));
            None
        }
        MovingBuiltin::Release(elem) => {
            let end = binary(
                TirBinaryOp::Add,
                args[1].clone(),
                args[2].clone(),
                TypeTable::I32,
            );
            stmts.extend(drop_slots(&args[0], args[1].clone(), end, elem, cx));
            stmts.push(expr_stmt(raw(&args)));
            None
        }
    };
    stmts.extend(tail.map(expr_stmt));
    *expr = TirExpr::new(
        TirExprKind::Block(TirBlock::new(stmts, span)),
        type_id,
        span,
    );
}

/// `array_get_value(array, index)`: the slot's value, which stays in it.
fn read_slot(array: &TirExpr, index: TirExpr, elem: TypeId) -> TirExpr {
    builtin_call("array_get_value", vec![array.clone(), index], elem)
}

/// `array_release(array, offset, len)`: the slots emptied, what they held
/// owned elsewhere.
fn release_slots(array: &TirExpr, offset: TirExpr, len: TirExpr) -> TirStmt {
    expr_stmt(builtin_call(
        "array_release",
        vec![array.clone(), offset, len],
        TypeTable::UNIT,
    ))
}

/// Empty the source slots of `array_move(dst, dst_offset, src, src_offset,
/// len)` that the destination range does not cover: what they held moved, so
/// a later drop of the source range drops nothing twice. Within one array the
/// ranges may overlap; across two the whole source range empties.
fn vacate_moved(args: &[TirExpr], cx: &Cx) -> Vec<TirStmt> {
    let [dst, dst_offset, src, src_offset, len] = args else {
        unreachable!("`array_move` takes five arguments");
    };
    let array = |reference: &TirExpr| {
        let tt = cx.resources.tt.borrow();
        let (ResolvedType::Ref(array) | ResolvedType::MutRef(array)) = *tt.get(reference.type_id)
        else {
            unreachable!("`array_move` takes its arrays by reference");
        };
        deref_expr(reference.clone(), array, synth_span())
    };
    let same_array = builtin_call("ref_eq", vec![array(dst), array(src)], TypeTable::BOOL);
    let int = |op, left: &TirExpr, right: &TirExpr| {
        binary(op, left.clone(), right.clone(), TypeTable::I32)
    };
    let lesser = |a: TirExpr, b: TirExpr, op| {
        let less = binary(op, a.clone(), b.clone(), TypeTable::BOOL);
        builtin_call("select", vec![less, a, b], TypeTable::I32)
    };
    let min = |a, b| lesser(a, b, TirBinaryOp::Lt);
    let max = |a, b| lesser(a, b, TirBinaryOp::Gt);
    let span = synth_span();
    // Moving right leaves `src_offset..min(dst_offset, src_end)`; moving left
    // leaves `max(dst_end, src_offset)..src_end`.
    let src_end = int(TirBinaryOp::Add, src_offset, len);
    let right = release_slots(
        src,
        src_offset.clone(),
        min(int(TirBinaryOp::Sub, dst_offset, src_offset), len.clone()),
    );
    let left_start = max(int(TirBinaryOp::Add, dst_offset, len), src_offset.clone());
    let left = release_slots(
        src,
        left_start.clone(),
        binary(TirBinaryOp::Sub, src_end, left_start, TypeTable::I32),
    );
    let moves_right = binary(
        TirBinaryOp::Gt,
        dst_offset.clone(),
        src_offset.clone(),
        TypeTable::BOOL,
    );
    let moves_left = binary(
        TirBinaryOp::Lt,
        dst_offset.clone(),
        src_offset.clone(),
        TypeTable::BOOL,
    );
    let shifted = if_stmt(
        moves_right,
        TirBlock::new(vec![right], span),
        Some(TirBlock::new(
            vec![if_stmt(moves_left, TirBlock::new(vec![left], span), None)],
            span,
        )),
    );
    let across = release_slots(src, src_offset.clone(), len.clone());
    vec![if_stmt(
        same_array,
        TirBlock::new(vec![shifted], span),
        Some(TirBlock::new(vec![across], span)),
    )]
}

/// `replace(reference, value)` on the locals holding its arguments: what
/// `reference` points to is returned, and `value` takes its place. An object
/// keeps its identity, so references to it see the new content: the two
/// exchange what they hold, and `value`'s object, holding the old content, is
/// returned. Anything else is read out and the place repointed.
fn replace_through(
    reference: &TirExpr,
    value: &TirExpr,
    stmts: &mut Vec<TirStmt>,
    cx: &mut Cx,
) -> TirExpr {
    let type_id = value.type_id;
    let span = synth_span();
    let place = common::deref_expr(reference.clone(), type_id, span);
    let layout = if cx.carries_resource(type_id) {
        cx.resources.layout(type_id)
    } else {
        Layout::Opaque
    };
    match layout {
        Layout::Resource | Layout::End => {
            stmts.push(expr_stmt(builtin_call(
                "swap_handles",
                vec![place, value.clone()],
                TypeTable::UNIT,
            )));
        }
        Layout::List { fields, .. }
        | Layout::Holder {
            parts: Parts::Fields(fields),
            ..
        } => {
            for (index, name, field_ty) in fields {
                let project = |of: &TirExpr| {
                    common::field_access(of.clone(), index, name.clone(), field_ty, span)
                };
                let (local, local_name) = cx.alloc_local(field_ty, "exchanged");
                stmts.push(let_stmt(&local_name, local, field_ty, project(&place)));
                stmts.push(expr_stmt(assign(project(&place), project(value))));
                stmts.push(expr_stmt(assign(
                    project(value),
                    local_ref(local, &local_name, field_ty),
                )));
            }
        }
        Layout::Array { .. } | Layout::Holder { .. } | Layout::Opaque => {
            let (local, name) = cx.alloc_local(type_id, "replaced");
            stmts.push(let_stmt(&name, local, type_id, place.clone()));
            stmts.push(expr_stmt(assign(place, value.clone())));
            return local_ref(local, &name, type_id);
        }
    }
    value.clone()
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
        StripMoves.visit_expr(body);
        return;
    }
    let mut inner = Cx {
        resources: cx.resources,
        locals: body_locals,
        local_base: params.len() as u32,
        local_count: None,
        scopes: Vec::new(),
        declarations: Vec::new(),
        targets: Vec::new(),
        expanding: Vec::new(),
        glue: cx.glue,
        module: cx.module.clone(),
        drop_self: None,
    };
    let yields = yields(body.type_id);
    let mut block = into_block(body);
    elab_scope(
        &mut block,
        owned,
        TemporaryScope::Statement,
        yields,
        &mut inner,
    );
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
    wrap_in_block(expr, |value| {
        vec![
            let_stmt(&name, local, type_id, value),
            empty_place(place),
            expr_stmt(local_ref(local, &name, type_id)),
        ]
    });
}

/// Replace the move `expr` with the place it moves out of.
fn unwrap_move(expr: &mut TirExpr) {
    let TirExprKind::Move { expr: place } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
    else {
        unreachable!("`unwrap_move` takes a move");
    };
    *expr = *place;
}

/// Remove the moves of `function`, which no drop elaboration will read.
pub fn strip_moves(function: &mut TirFunction) {
    if let Some(body) = &mut function.body {
        StripMoves.visit_block(body);
    }
}

/// Remove the moves of a body drop elaboration leaves as it is, one owning no
/// resource: a move there takes nothing.
struct StripMoves;

impl TirMutVisitor for StripMoves {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if matches!(expr.kind, TirExprKind::Move { .. }) {
            unwrap_move(expr);
        }
        self.walk_expr(expr);
    }
}

/// Take the value of the place `expr`, which a `Move` marked, emptying the
/// place. A payload cannot be emptied on its own, so taking it empties the
/// variant it is read from.
fn take_moved(expr: &mut TirExpr, cx: &mut Cx) {
    // An instance whose `affine` parameter is bound to a type holding no
    // resource has nothing to take.
    if !cx.carries_resource(expr.type_id) {
        elab_expr(expr, false, cx);
        return;
    }
    let span = expr.span;
    match &mut expr.kind {
        TirExprKind::Local { .. } => take_place(expr, cx),
        TirExprKind::FieldAccess { expr: base, .. } => {
            assert!(is_place(base, cx), "a field moves out of a place: {span:?}");
            elab_expr(base, false, cx);
            take_place(expr, cx);
        }
        TirExprKind::VariantPayload { expr: base, .. } => {
            assert!(
                is_place(base, cx),
                "a payload moves out of a place: {span:?}"
            );
            elab_expr(base, false, cx);
            let place = (**base).clone();
            empty_after(expr, place, cx);
        }
        _ => unreachable!("only a place moves: {span:?}"),
    }
}

/// Move the affine field `expr` reads out of the aggregate a temporary holds,
/// spilling it first, so its other fields drop when the statement ends. One
/// read through a reference moves nothing here: the move check reports it.
fn take_field(expr: &mut TirExpr, cx: &mut Cx) {
    let TirExprKind::FieldAccess { expr: base, .. } = &mut expr.kind else {
        unreachable!("`take_field` takes a field access");
    };
    if is_temporary(base, cx) {
        elab_expr(base, true, cx);
        let type_id = base.type_id;
        let (local, name) = spill_as_local(base, cx);
        let value = std::mem::replace(&mut **base, local_ref(local, &name, type_id));
        take_place(expr, cx);
        let TirExprKind::Block(block) = &mut expr.kind else {
            unreachable!("`take_place` leaves a block");
        };
        let spill = assign(local_ref(local, &name, type_id), value);
        block.stmts.insert(0, expr_stmt(spill));
        return;
    }
    elab_expr(base, false, cx);
}

/// Whether evaluating `expr` produces a fresh value rather than reading a
/// place. Referencing such a value leaves it with no owner but the statement.
/// A block ending in a place it does not move reads that place: synthesized
/// code builds one to run statements ahead of a borrow.
fn is_temporary(expr: &TirExpr, cx: &Cx) -> bool {
    if let TirExprKind::Block(block) = &expr.kind {
        return !block.stmts.last().is_some_and(
            |stmt| matches!(&stmt.kind, TirStmtKind::Expr(tail) if is_place(tail, cx)),
        );
    }
    matches!(
        expr.kind,
        TirExprKind::Call { .. }
            | TirExprKind::CmRawCall { .. }
            | TirExprKind::IndirectCall { .. }
            | TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::ArrayLiteral { .. }
            | TirExprKind::VariantConstruct { .. }
            | TirExprKind::LabeledBlock { .. }
            | TirExprKind::If { .. }
            | TirExprKind::Match { .. }
            | TirExprKind::WithHandler { .. }
    )
}

/// Move the referenced temporary `expr` into a fresh local, in place and so in
/// evaluation order (`{ $temp = expr; $temp }`), owned until the statement
/// holding it ends. Returns the local.
fn spill_temporary(expr: &mut TirExpr, cx: &mut Cx) -> TirExpr {
    let type_id = expr.type_id;
    let (local, name) = spill_as_local(expr, cx);
    let value = std::mem::replace(expr, local_ref(local, &name, type_id));
    wrap_in_block(expr, |read| {
        vec![
            expr_stmt(assign(local_ref(local, &name, type_id), value)),
            expr_stmt(read),
        ]
    });
    local_ref(local, &name, type_id)
}

/// Declare a local owned by the statement holding `expr`, to hold the
/// temporary `expr` evaluates to; the caller assigns it. It is declared empty
/// ahead of that statement (see [`Cx::declarations`]).
fn spill_as_local(expr: &TirExpr, cx: &mut Cx) -> (u32, String) {
    let type_id = expr.type_id;
    let (local, name) = cx.alloc_local(type_id, "temp");
    cx.locals[(local - cx.local_base) as usize].is_mut = true;
    let empty = TirExpr::new(TirExprKind::Null, type_id, synth_span());
    cx.declarations
        .push(let_mut_stmt(&name, local, type_id, empty));
    cx.declare(Live {
        local,
        name: name.clone(),
        type_id,
        temporary: true,
    });
    (local, name)
}

/// Elaborate the store `expr` of an affine value. What the place held drops
/// once the stored value is computed, since computing it may read it. A place
/// holding an object takes the stored value's content, so every reference to
/// the place sees it, and the stored value's own object is zeroed. A null
/// place, one a move emptied, takes the stored reference.
fn elab_store(expr: &mut TirExpr, cx: &mut Cx) {
    let span = expr.span;
    let TirExprKind::Assign { target, value } = &mut expr.kind else {
        unreachable!("`elab_store` takes an assignment");
    };
    let type_id = target.type_id;
    let through = match &target.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: reference,
        } => Some((**reference).clone()),
        _ if is_place(target, cx) => None,
        _ => return,
    };
    let target = (**target).clone();
    let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span);
    let value = std::mem::replace(&mut **value, unit);
    let (local, name) = cx.alloc_local(type_id, "stored");
    let stored = local_ref(local, &name, type_id);
    let mut stmts = vec![let_stmt(&name, local, type_id, value)];
    let is_object = cx.resources.is_object(type_id);
    match through {
        Some(reference) => {
            // Read the reference once: dropping a struct reads it per field.
            let reference = if matches!(reference.kind, TirExprKind::Local { .. }) {
                reference
            } else {
                let ref_ty = reference.type_id;
                let (local, name) = cx.alloc_local(ref_ty, "store_ref");
                stmts.push(let_stmt(&name, local, ref_ty, reference));
                local_ref(local, &name, ref_ty)
            };
            let place = common::deref_expr(reference, type_id, span);
            if is_object {
                stmts.push(refuse_self_store(place.clone(), stored.clone(), cx));
            }
            stmts.extend(drop_if_held(place.clone(), type_id, cx));
            stmts.push(expr_stmt(assign(place, stored.clone())));
            if is_object {
                stmts.extend(zero_object(stored, cx));
            }
        }
        None if is_object => {
            let repoint = expr_stmt(assign(target.clone(), stored.clone()));
            stmts.push(store_into_object(target, stored, repoint, cx));
        }
        None => {
            stmts.extend(drop_if_held(target.clone(), type_id, cx));
            stmts.push(expr_stmt(assign(target, stored)));
        }
    }
    *expr = TirExpr::new(
        TirExprKind::Block(TirBlock::new(stmts, span)),
        TypeTable::UNIT,
        span,
    );
}

/// Drop what the object `place` holds and move `stored`'s content into it, so
/// every reference to the place sees it, zeroing `stored`'s own object. Where a
/// move emptied `place`, `repoint` stores the reference instead.
fn store_into_object(place: TirExpr, stored: TirExpr, repoint: TirStmt, cx: &mut Cx) -> TirStmt {
    let span = place.span;
    let type_id = place.type_id;
    let mut write = drop_value(place.clone(), type_id, cx);
    let ref_ty = cx.resources.tt.borrow_mut().make_mut_ref(type_id);
    let into = common::deref_expr(
        common::mut_ref_expr(place.clone(), ref_ty, span),
        type_id,
        span,
    );
    write.push(expr_stmt(assign(into, stored.clone())));
    write.extend(zero_object(stored, cx));
    let moved = builtin_call("is_moved", vec![place], TypeTable::BOOL);
    if_stmt(
        moved,
        TirBlock::new(vec![repoint], span),
        Some(TirBlock::new(write, span)),
    )
}

/// Trap where a store through a reference would move `stored` into the object
/// it already is, `place`: the move into `stored` nulled the only place that
/// owned it, and a reference owns nothing (WEP: Affine Types).
fn refuse_self_store(place: TirExpr, stored: TirExpr, cx: &Cx) -> TirStmt {
    let type_name = cx.resources.tt.borrow().type_name(stored.type_id);
    let trap = panic_with(
        format!(
            "a store through a reference moves `{type_name}` into the object it was moved out \
             of, which nothing would own"
        ),
        cx,
    );
    if_stmt(
        builtin_call("ref_eq", vec![place, stored], TypeTable::BOOL),
        TirBlock::new(vec![expr_stmt(trap)], synth_span()),
        None,
    )
}

/// Zero `value`'s object, whose content moved into another: a box's handle
/// becomes 0 and a struct's affine fields null.
fn zero_object(value: TirExpr, cx: &Cx) -> Vec<TirStmt> {
    match cx.resources.layout(value.type_id) {
        Layout::Resource | Layout::End => vec![expr_stmt(builtin_call(
            "zero_handle",
            vec![value],
            TypeTable::UNIT,
        ))],
        Layout::Holder {
            parts: Parts::Fields(fields),
            ..
        } => affine_fields(&value, &fields, cx)
            .into_iter()
            .map(empty_place)
            .collect(),
        Layout::List { .. } | Layout::Array { .. } | Layout::Holder { .. } | Layout::Opaque => {
            Vec::new()
        }
    }
}

/// Elaborate `expr` as a scope of its own: whatever it declares (its
/// temporaries) drops once its value is computed. An expression that runs on
/// some paths only, or more than once, is one.
fn elab_expr_scope(expr: &mut TirExpr, cx: &mut Cx) {
    cx.open_scope(Vec::new(), TemporaryScope::Statement);
    let first_declaration = cx.declarations.len();
    elab_expr(expr, true, cx);
    let scope = cx.scopes.pop().expect("pushed above");
    let lives: Vec<Live> = scope.lives.into_iter().rev().collect();
    let drops = drop_lives(&lives, cx);
    append_expr_drops(expr, drops, cx);
    let declarations = cx.declarations.split_off(first_declaration);
    if declarations.is_empty() {
        return;
    }
    if !matches!(expr.kind, TirExprKind::Block(_)) {
        wrap_in_block(expr, |value| vec![expr_stmt(value)]);
    }
    let TirExprKind::Block(block) = &mut expr.kind else {
        unreachable!("wrapped above");
    };
    block.stmts.splice(0..0, declarations);
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
/// arm's bindings. Entering an arm consumes a scrutinee that moved or is a
/// temporary: the parts no binding takes drop, and the scrutinee is emptied,
/// before the body runs. A temporary is spilled first, so an arm that does not
/// consume leaves it for its statement to drop.
fn elab_match(scrutinee: &mut TirExpr, arms: &mut [TirMatchArm], cx: &mut Cx) {
    // An arm that does not consume the scrutinee owns nothing it binds.
    let bindings: Vec<Vec<Live>> = arms
        .iter()
        .map(|arm| {
            if arm.consumes {
                cx.resources.pattern_bindings(&arm.pattern)
            } else {
                Vec::new()
            }
        })
        .collect();
    let patterns: Vec<(&TirPattern, bool)> = arms
        .iter()
        .map(|arm| (&arm.pattern, arm.consumes))
        .collect();
    let extracts = bindings.iter().any(|arm_bindings| !arm_bindings.is_empty());
    let place = consumed_scrutinee(scrutinee, &patterns, extracts, cx);
    for (arm, arm_bindings) in arms.iter_mut().zip(bindings) {
        let consumed = place.clone().filter(|_| arm.consumes);
        cx.open_scope(Vec::new(), TemporaryScope::Statement);
        if let Some(guard) = arm.guard.as_mut() {
            elab_expr_scope(guard, cx);
        }
        cx.scopes.pop();
        let mut block = into_block(&mut arm.body);
        let yields = yields(arm.body.type_id);
        elab_scope(
            &mut block,
            arm_bindings,
            TemporaryScope::Statement,
            yields,
            cx,
        );
        // Inserted once the body is elaborated: emptying the scrutinee is a
        // move, not a store that drops what it replaces.
        if let Some(place) = consumed {
            let entry = consume_matched(&arm.pattern, place, cx);
            block.stmts.splice(0..0, entry);
        }
        arm.body = TirExpr::new(TirExprKind::Block(block), arm.body.type_id, arm.body.span);
    }
}

/// Elaborate the scrutinee of `patterns`, each paired with whether it
/// consumes, answering the place a consuming pattern empties: the place a move
/// names, or the local a value no place holds spills into. A place read without
/// a move is only borrowed, so no consuming pattern may bind a resource out of
/// it. A temporary every pattern consumes whole, binding all it holds, needs no
/// local. `extracts`: whether a consuming pattern binds a resource out of it.
fn consumed_scrutinee(
    scrutinee: &mut TirExpr,
    patterns: &[(&TirPattern, bool)],
    extracts: bool,
    cx: &mut Cx,
) -> Option<TirExpr> {
    // A moved place stays put until a pattern consuming it is entered.
    let moved = matches!(scrutinee.kind, TirExprKind::Move { .. });
    if moved {
        unwrap_move(scrutinee);
    }
    if !cx.carries_resource(scrutinee.type_id) {
        elab_expr(scrutinee, false, cx);
        return None;
    }
    if is_place(scrutinee, cx) {
        assert!(
            moved || !extracts,
            "a scrutinee a resource moves out of is marked `Move`: {:?}",
            scrutinee.span
        );
        elab_expr(scrutinee, false, cx);
        return moved.then(|| scrutinee.clone());
    }
    assert!(!moved, "only a place moves: {:?}", scrutinee.span);
    // A value read through a reference (`*self`), or a block ending in a place,
    // is still its owner's: entering an arm drops none of it.
    if !is_temporary(scrutinee, cx) {
        elab_expr(scrutinee, extracts, cx);
        return None;
    }
    // `drop_unbound` reads its place only to build the drops, so a probe
    // answers whether there are any.
    let probe = TirExpr::new(TirExprKind::Null, scrutinee.type_id, synth_span());
    let leaves = patterns.iter().any(|(pattern, consumes)| {
        !consumes || !drop_unbound(pattern, probe.clone(), cx).is_empty()
    });
    elab_expr(scrutinee, true, cx);
    leaves.then(|| spill_temporary(scrutinee, cx))
}

/// What entering `pattern`, which matched `place`, does to it: the parts no
/// binding takes drop, and the place is emptied.
fn consume_matched(pattern: &TirPattern, place: TirExpr, cx: &mut Cx) -> Vec<TirStmt> {
    let mut stmts = drop_unbound(pattern, place.clone(), cx);
    stmts.push(empty_place(place));
    stmts
}

/// Drop each part of `place` that `pattern`, which it matched, binds nothing
/// of: what a by-value pattern leaves of its scrutinee (WEP: Affine Types).
fn drop_unbound(pattern: &TirPattern, place: TirExpr, cx: &mut Cx) -> Vec<TirStmt> {
    let type_id = place.type_id;
    if !cx.carries_resource(type_id) {
        return Vec::new();
    }
    // The `self` of a `Drop::drop` body gives up its fields one by one.
    let destructures = match cx.resources.layout(type_id) {
        Layout::Holder {
            drop_impl: None, ..
        } => true,
        Layout::Holder { .. } => {
            matches!(place.kind, TirExprKind::Local { index, .. } if cx.drop_self == Some(index))
        }
        Layout::Resource
        | Layout::End
        | Layout::List { .. }
        | Layout::Array { .. }
        | Layout::Opaque => false,
    };
    match pattern {
        TirPattern::Binding { .. } | TirPattern::Narrow { name: Some(_), .. } => Vec::new(),
        TirPattern::Struct { fields, .. } if destructures => {
            let listed: IndexMap<u32, &TirPattern> = fields
                .iter()
                .map(|field| (field.field_index, &field.pattern))
                .collect();
            drop_unbound_fields(&place, |index| listed.get(&index).copied(), cx)
        }
        TirPattern::Tuple(elements, _) => {
            drop_unbound_fields(&place, |index| elements.get(index as usize), cx)
        }
        TirPattern::Variant {
            case_index,
            bindings,
            payload_type,
            ..
        } if destructures => {
            assert!(bindings.len() <= 1, "a case has one payload");
            let payload = TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(place),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                *payload_type,
                synth_span(),
            );
            match bindings.first() {
                Some(binding) => drop_unbound(binding, payload, cx),
                None if cx.carries_resource(*payload_type) => {
                    drop_if_held(payload, *payload_type, cx)
                }
                None => Vec::new(),
            }
        }
        TirPattern::Or(alternatives) if !cx.resources.pattern_bindings(pattern).is_empty() => {
            drop_unbound_alternatives(alternatives, place, cx)
        }
        // What implements `Drop` gives up no part, which the move check holds.
        TirPattern::Struct { .. } | TirPattern::Variant { .. } | TirPattern::Or(_) => {
            assert!(
                cx.resources.pattern_bindings(pattern).is_empty(),
                "nothing moves out of a type that implements `Drop`: {pattern:?}"
            );
            drop_if_held(place, type_id, cx)
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. }
        | TirPattern::Narrow { name: None, .. }
        | TirPattern::PerInstance { .. } => drop_if_held(place, type_id, cx),
    }
}

/// Drop each resource-carrying field of `place` that its pattern, which
/// `listed` answers by field index, binds nothing of. A field the pattern
/// leaves out binds nothing.
fn drop_unbound_fields<'p>(
    place: &TirExpr,
    listed: impl Fn(u32) -> Option<&'p TirPattern>,
    cx: &mut Cx,
) -> Vec<TirStmt> {
    let Layout::Holder {
        parts: Parts::Fields(fields),
        ..
    } = cx.resources.layout(place.type_id)
    else {
        unreachable!("a struct or tuple pattern matches a holder of fields");
    };
    let mut stmts = Vec::new();
    for field in affine_fields(place, &fields, cx) {
        let TirExprKind::FieldAccess { field_index, .. } = field.kind else {
            unreachable!("`affine_fields` projects each field");
        };
        let field_ty = field.type_id;
        stmts.extend(match listed(field_index) {
            Some(pattern) => drop_unbound(pattern, field, cx),
            None => drop_if_held(field, field_ty, cx),
        });
    }
    stmts
}

/// Drop what the alternative of an or-pattern that matched `place` binds
/// nothing of, through a `match` that finds which one it was.
fn drop_unbound_alternatives(
    alternatives: &[TirPattern],
    place: TirExpr,
    cx: &mut Cx,
) -> Vec<TirStmt> {
    let span = synth_span();
    let arms = alternatives
        .iter()
        .map(|alternative| {
            let drops = drop_unbound(alternative, place.clone(), cx);
            let mut pattern = alternative.clone();
            BindNothing.visit_pattern(&mut pattern);
            TirMatchArm {
                pattern,
                guard: None,
                body: TirExpr::new(
                    TirExprKind::Block(TirBlock::new(drops, span)),
                    TypeTable::UNIT,
                    span,
                ),
                span,
                consumes: true,
            }
        })
        .collect();
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(place),
            arms,
        },
        TypeTable::UNIT,
        span,
    );
    vec![expr_stmt(match_expr)]
}

/// Turn each binding of a pattern into a wildcard, so matching it again stores
/// into no local the arm's own match bound.
struct BindNothing;

impl TirMutVisitor for BindNothing {
    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        if matches!(pattern, TirPattern::Binding { .. }) {
            *pattern = TirPattern::Wildcard;
        }
        self.walk_pattern(pattern);
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
