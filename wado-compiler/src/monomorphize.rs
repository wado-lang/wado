//! Monomorphization for Wado TIR, between type resolution and lowering: collect
//! the generic struct and function definitions, find their instantiation sites,
//! generate a concrete definition per site, and rewrite types and calls onto the
//! monomorphized names.

mod call_instance;
mod call_rewrite;
mod func_inst;
mod instance_patterns;
mod never_dispatch;
mod state;
mod struct_inst;
mod substitute;

pub use instance_patterns::lower_instance_patterns;

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_host::Diagnostic;
use crate::compiler_trace;
use crate::defs::DefId;
use crate::elaborator::TypeError;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{FreeFunctionName, FunctionId, LocalMethodName, TypeHead};
use crate::prelower_reach::{
    Callee, Entry, Roots, callees, expr_callees, expr_types, function_key, init_types, type_head,
};
use crate::synthesis::affine_drop;
use crate::synthesis::affine_drop::Generics;
use crate::synthesis::template::hole_fmt_helpers;
use crate::tir::{
    MonomorphInfo, ResolvedType, StructDef, TemplateId, TirFunction, TirGlobal, TirModule,
    TirStruct, TypeId,
};
use crate::tir_visitor::TirRefVisitor;
use crate::type_table::{AnonStructId, TypeKey, TypeTable};

use state::Monomorphizer;

/// The name an `InstantiationKey` carries: a method's own, a free function's
/// qualified by its module, since no receiver namespaces it.
pub(crate) fn generic_function_name(
    is_method: bool,
    module_source: &ModuleSource,
    name: &str,
) -> String {
    if is_method {
        name.to_string()
    } else {
        FreeFunctionName::from_module_source(module_source, name).to_string()
    }
}

/// Every generic function template, keyed by the declaration a call selected.
#[derive(Default, Clone)]
pub(crate) struct Templates {
    by_id: IndexMap<TemplateId, Rc<RefCell<TirFunction>>>,
}

impl Templates {
    fn insert(&mut self, func_rc: &Rc<RefCell<TirFunction>>) {
        let func = func_rc.borrow();
        assert!(func.is_template(), "`{}` instantiates nothing", func.name);
        let id = func
            .template_id()
            .unwrap_or_else(|| panic!("`{}` is an instance, not a template", func.name));
        if let Some(prior) = self.by_id.insert(id.clone(), Rc::clone(func_rc)) {
            assert!(
                Rc::ptr_eq(&prior, func_rc),
                "two templates share the identity {id:?}"
            );
        }
    }

    /// The template `id` names; `None` where the declaration is emitted as
    /// written.
    fn get(&self, id: &TemplateId) -> Option<&Rc<RefCell<TirFunction>>> {
        self.by_id.get(id)
    }

    /// The templates with a body, which a transitive scan may instantiate.
    fn with_bodies(&self) -> Self {
        Self {
            by_id: self
                .by_id
                .iter()
                .filter(|(_, func)| func.borrow().body.is_some())
                .map(|(id, func)| (id.clone(), Rc::clone(func)))
                .collect(),
        }
    }
}

/// The bodies a run has yet to rewrite: an earlier run left the functions
/// before `scanned` rewritten, and the global initializers, which a resume
/// adds none of.
#[derive(Clone, Copy)]
struct Unrewritten {
    scanned: usize,
}

impl Unrewritten {
    fn functions(self, module: &TirModule) -> &[Rc<RefCell<TirFunction>>] {
        &module.functions[self.scanned..]
    }

    fn globals(self, module: &TirModule) -> &[TirGlobal] {
        if self.scanned == 0 {
            &module.globals
        } else {
            &[]
        }
    }

    fn globals_mut(self, module: &mut TirModule) -> &mut [TirGlobal] {
        if self.scanned == 0 {
            &mut module.globals
        } else {
            &mut []
        }
    }
}

/// The concrete functions no reached body has named yet. A body that names one
/// takes it out, to be scanned like any reached function.
#[derive(Default)]
pub(crate) struct Pool {
    by_key: IndexMap<FreeFunctionName, Rc<RefCell<TirFunction>>>,
    by_template: IndexMap<TemplateId, FreeFunctionName>,
    /// What waits on a type no scanned body has held a value of yet.
    waiting: IndexMap<TypeHead, Vec<FreeFunctionName>>,
    held: IndexSet<TypeKey>,
    /// The types a declaration's members are built over, as [`Self::lay_out`]
    /// has walked them.
    laid_out: IndexSet<TypeKey>,
    /// What a held type released, for the caller to scan.
    released: Vec<Rc<RefCell<TirFunction>>>,
    /// The `GenericInstance`s held since the struct drain last read them.
    instances: Vec<TypeId>,
    /// Each generic variant's type parameter indices and case payloads, which
    /// a held instance of it holds substituted.
    generic_variants: IndexMap<DefId, (Vec<u32>, Vec<TypeId>)>,
    /// Each declaration's field and payload types, as declared. A value holds
    /// its members' values, which drop glue drops through their own impls.
    members: IndexMap<DefId, Vec<TypeId>>,
    /// The declarations whose members [`Self::hold`] has held.
    opened: IndexSet<DefId>,
}

impl Pool {
    fn insert(&mut self, func_rc: Rc<RefCell<TirFunction>>) -> FreeFunctionName {
        let func = func_rc.borrow();
        let key = function_key(&func);
        if let Some(template) = func.template_id() {
            self.by_template.insert(template, key.clone());
        }
        drop(func);
        self.by_key.insert(key.clone(), func_rc);
        key
    }

    fn wait(&mut self, head: TypeHead, func_rc: Rc<RefCell<TirFunction>>) {
        let key = self.insert(func_rc);
        self.waiting.entry(head).or_default().push(key);
    }

    /// Note that a scanned body holds a value of `id`, and so of every type it
    /// is built over and every member of those: release what waited on one,
    /// and keep each `GenericInstance` for the struct drain.
    fn hold(&mut self, id: TypeId, type_table: &TypeTable) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            let found = built_over(id, type_table, &mut self.held);
            self.keep_instances(&found, type_table);
            for id in found {
                if let Some(def) = type_table.nominal_def(id)
                    && let Some(members) = self.members.get(&def)
                    && self.opened.insert(def)
                {
                    stack.extend(members);
                }
                let Some(head) = type_head(type_table, id) else {
                    continue;
                };
                for key in self.waiting.swap_remove(&head).into_iter().flatten() {
                    self.released.extend(self.by_key.swap_remove(&key));
                }
            }
        }
    }

    /// Record `def`'s members, holding them at once where a value of it is
    /// held already.
    fn declare_members(&mut self, def: DefId, members: Vec<TypeId>, type_table: &TypeTable) {
        if self.opened.contains(&def) {
            for id in &members {
                self.hold(*id, type_table);
            }
        }
        self.members.insert(def, members);
    }

    /// Keep for the struct drain each `GenericInstance` that a declaration's
    /// member of type `id` is built over. A declaration is no reached body, so
    /// it releases nothing.
    fn lay_out(&mut self, id: TypeId, type_table: &TypeTable) {
        let found = built_over(id, type_table, &mut self.laid_out);
        self.keep_instances(&found, type_table);
    }

    /// A generic declaration's member names its own parameters, which no
    /// instance of it is.
    fn keep_instances(&mut self, found: &[TypeId], type_table: &TypeTable) {
        self.instances.extend(found.iter().copied().filter(|&id| {
            matches!(type_table.get(id), ResolvedType::GenericInstance { .. })
                && !type_table.contains_type_param(id)
        }));
    }

    /// The function `callee` names, if it is still here. A template's body
    /// may name one by its declaration under a spelling no function carries
    /// (`EnumCase<E>::holds`).
    fn take(&mut self, callee: &Callee) -> Option<Rc<RefCell<TirFunction>>> {
        self.by_key.swap_remove(&callee.key).or_else(|| {
            let key = self.by_template.get(callee.template.as_ref()?)?;
            self.by_key.swap_remove(key)
        })
    }
}

/// `id` and every type it is built over that `seen` does not hold yet, which
/// it then does.
fn built_over(id: TypeId, type_table: &TypeTable, seen: &mut IndexSet<TypeKey>) -> Vec<TypeId> {
    // Most of a body's expressions hold a type held already.
    if seen.contains(&type_table.type_key(id)) {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        if !seen.insert(type_table.type_key(id)) {
            continue;
        }
        stack.extend(type_table.get_newtype_base(id));
        type_table.any_constituent(id, &mut |part| {
            stack.push(part);
            false
        });
        found.push(id);
    }
    found
}

/// Instantiate every generic the linked package reaches, and hand back the
/// session so a pass that adds bodies later can resume it.
pub fn monomorphize(flat: &mut FlatPackage) -> Monomorphization {
    let mut generic_functions = Templates::default();
    for func_rc in &flat.functions {
        if func_rc.borrow().is_template() {
            generic_functions.insert(func_rc);
        }
    }
    let mut session = Monomorphization {
        generic_functions,
        generic_structs: generic_structs_in(&flat.structs).collect(),
        monomorphizer: Monomorphizer::new(flat.trait_env.clone()),
        scanned_functions: 0,
        generics: Generics::of(flat),
        elaborated_functions: 0,
        roots: Some(Roots::new(flat)),
    };
    let pool = &mut session.monomorphizer.pool;
    let type_table = flat.type_table.borrow();
    for variant in &flat.variants {
        let payloads = variant.cases.iter().map(|case| case.payload);
        pool.declare_members(variant.def, payloads.clone().collect(), &type_table);
        if variant.type_params.is_empty() {
            for payload in payloads {
                pool.lay_out(payload, &type_table);
            }
        } else {
            let params = variant
                .type_params
                .iter()
                .map(|param| param.index)
                .collect();
            pool.generic_variants
                .insert(variant.def, (params, payloads.collect()));
        }
    }
    drop(type_table);
    session.run(flat);
    session
}

/// Every generic struct among `structs`, keyed by its declaration.
fn generic_structs_in(structs: &[TirStruct]) -> impl Iterator<Item = (DefId, TirStruct)> + '_ {
    structs
        .iter()
        .filter(|s| !s.type_params.is_empty())
        .map(|s| {
            let def = s
                .def
                .decl()
                .unwrap_or_else(|| panic!("generic struct `{}` declares nothing", s.name));
            (def, s.clone())
        })
}

/// One resumable monomorphization. A run consumes the templates it instantiates
/// and remembers its instances here, so a second run must resume rather than start.
pub struct Monomorphization {
    monomorphizer: Monomorphizer,
    generic_functions: Templates,
    generic_structs: IndexMap<DefId, TirStruct>,
    /// How many leading `flat.functions` a previous run already read call sites
    /// from. It left them rewritten to mangled instance names, and such a call
    /// no longer spells its method type arguments — reading one again queues an
    /// instance keyed by the receiver alone, whose body keeps the method's own
    /// type parameters.
    scanned_functions: usize,
    /// What elaborating the instances' drops reads of the templates.
    generics: Generics,
    /// How many leading `flat.functions` have had their drops elaborated.
    elaborated_functions: usize,
    /// What the first run walks from, leaving the rest in the pool; `None`
    /// once it has.
    roots: Option<Roots>,
}

impl Monomorphization {
    /// Instantiate what the bodies added since the last run reach.
    pub fn resume(&mut self, flat: &mut FlatPackage) {
        self.run(flat);
    }

    /// Drop what no run reached, and record the drop for the assert in
    /// `lower`'s `Interner::resolve`.
    pub fn finish(self, flat: &mut FlatPackage) {
        let pool = self.monomorphizer.pool.by_key;
        compiler_trace!(
            "prelower_reach",
            "monomorphize left {} functions unreached",
            pool.len()
        );
        flat.pruned.extend(pool.into_keys().map(FunctionId::from));
    }

    /// The error refusing a template whose instances never end, with the
    /// module declaring it.
    pub fn expanding_recursion(&self) -> Option<(ModuleSource, Diagnostic)> {
        let template = self.monomorphizer.expanding.as_ref()?;
        let error = TypeError::ExpandingRecursion {
            name: template.name.clone(),
            span: template.span,
        };
        Some((template.module_source.clone(), error.into()))
    }

    /// Instantiate, then elaborate the new instances' drops, until the drop
    /// glue that elaborating adds calls no template left to instantiate.
    fn run(&mut self, flat: &mut FlatPackage) {
        loop {
            self.instantiate(flat);
            if flat.functions.len() == self.scanned_functions {
                break;
            }
        }
    }

    fn instantiate(&mut self, flat: &mut FlatPackage) {
        let mut temp_module = TirModule::new(flat.entry_module_source.clone());
        temp_module.type_table = flat.type_table.clone();
        temp_module.functions = std::mem::take(&mut flat.functions);
        temp_module.structs = std::mem::take(&mut flat.structs);
        temp_module.globals = std::mem::take(&mut flat.globals);
        assert!(temp_module.functions.len() >= self.scanned_functions);

        let temp_module = self.monomorphizer.monomorphize_with_externals(
            temp_module,
            &self.generic_functions,
            &self.generic_structs,
            self.scanned_functions,
            self.roots.take().as_ref(),
        );
        self.scanned_functions = temp_module.functions.len();
        write_back(flat, temp_module);
        // A generic body's drops are elaborated once its types are known
        // (WEP: Affine Types).
        affine_drop::elaborate_instance_drops(flat, self.elaborated_functions, &self.generics);
        self.elaborated_functions = flat.functions.len();
    }
}

fn write_back(flat: &mut FlatPackage, temp_module: TirModule) {
    flat.functions = temp_module.functions;
    flat.structs = temp_module.structs;
    flat.globals = temp_module.globals;

    // Two functions sharing `(module_source, name)` overwrite each other in the
    // registries downstream, surfacing phases later as a confusing type error.
    let mut seen_functions: IndexMap<
        (ModuleSource, String),
        (Option<DefId>, Option<MonomorphInfo>),
    > = IndexMap::default();
    for func_rc in &flat.functions {
        let f = func_rc.borrow();
        let key = (f.module_source.clone(), f.name.clone());
        let this = (f.def_id, f.monomorph_info.clone());
        if let Some(first) = seen_functions.insert(key, this.clone()) {
            panic!(
                "duplicate function `{}` in module `{}` after monomorphization: \
                 first from {first:?}, then from {this:?}. `module_source` is the \
                 canonical namespace; two functions with the same mangled name \
                 landing in the same module indicate a synthesis or \
                 monomorphization bug.",
                f.name, f.module_source
            );
        }
    }

    // The effect checker has run; nothing downstream reads effect params.
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        func.effects.retain(|e| !e.is_param());
    }
}

/// The module a concrete type's trait impls are sought in, when a
/// `T^Trait::method` receiver is substituted.
fn module_source_for_trait_impl(type_table: &TypeTable, type_id: TypeId) -> Option<ModuleSource> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => Some(ModuleSource::of_primitive(*prim)),
        ResolvedType::Unit | ResolvedType::Never => Some(ModuleSource::primitive()),
        ResolvedType::BuiltinArray(_) => Some(ModuleSource::array()),
        ResolvedType::Struct { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::Variant { .. } => type_table.nominal_head(type_id).map(|(_, m)| m),
        // An inherited instance lives in the base type's module.
        ResolvedType::Newtype { base_type, .. } => {
            module_source_for_trait_impl(type_table, *base_type)
        }
        _ => None,
    }
}

impl Monomorphizer {
    /// Hold every type `func`'s signature, locals and body hold a value of. A
    /// local may outlive the expressions of its type: unrolling a pack loop
    /// leaves the loop variable declared at the pack's type.
    fn note_types(&mut self, func: &TirFunction, type_table: &TypeTable) {
        let signature = func.params.iter().map(|param| param.type_id);
        let locals = func.locals.iter().map(|local| local.type_id);
        for id in signature.chain([func.return_type]).chain(locals) {
            self.pool.hold(id, type_table);
        }
        if let Some(body) = &func.body {
            for id in expr_types(body) {
                self.pool.hold(id, type_table);
            }
        }
    }

    /// Take out of the pool, onto `unscanned`, what `func` names and what a
    /// held type released.
    fn take_callees(
        &mut self,
        func: &TirFunction,
        type_table: &TypeTable,
        unscanned: &mut Vec<Rc<RefCell<TirFunction>>>,
    ) {
        if let Some(body) = &func.body {
            self.take_from_pool(&callees(body, type_table), unscanned);
        }
        unscanned.append(&mut self.pool.released);
    }

    fn take_from_pool(
        &mut self,
        callees: &[Callee],
        unscanned: &mut Vec<Rc<RefCell<TirFunction>>>,
    ) {
        unscanned.extend(callees.iter().filter_map(|callee| self.pool.take(callee)));
    }

    /// Ready a function taken out of the pool, its packs expanded and its types
    /// noted and drained, as Phase 6 readied the roots, then queue what it
    /// instantiates and take out what it names.
    fn scan_taken(
        &mut self,
        func_rc: &Rc<RefCell<TirFunction>>,
        type_table: &Rc<RefCell<TypeTable>>,
        generic_functions: &Templates,
        unscanned: &mut Vec<Rc<RefCell<TirFunction>>>,
    ) {
        self.rewrite_types_in_function(&mut func_rc.borrow_mut(), &mut type_table.borrow_mut());
        let func = func_rc.borrow();
        let mut type_table = type_table.borrow_mut();
        if let Some(body) = &func.body {
            func_inst::InstantiationCollector {
                mono: self,
                generic_functions,
                type_table: &mut type_table,
            }
            .visit_block(body);
        }
        self.take_callees(&func, &type_table, unscanned);
    }

    /// Instantiate the queued structs, and those their fields queue, to a
    /// fixpoint; the concrete structs produced.
    fn drain_pending_structs(
        &mut self,
        type_table: &RefCell<TypeTable>,
        generic_structs: &IndexMap<DefId, TirStruct>,
    ) -> Vec<TirStruct> {
        let mut new_structs = Vec::new();
        loop {
            let variant_instances =
                self.collect_instantiation_sites(&type_table.borrow(), generic_structs);
            let payloads = self.variant_payloads(&variant_instances, &mut type_table.borrow_mut());
            for payload in payloads {
                self.pool.lay_out(payload, &type_table.borrow());
            }
            if self.structs.pending.is_empty() && self.pool.instances.is_empty() {
                break;
            }
            while let Some(key) = self.structs.pending.pop() {
                let def = key
                    .def
                    .expect("a struct instantiation names its declaration");
                let concrete = self.instantiate_struct(
                    &generic_structs[&def],
                    &key,
                    &mut type_table.borrow_mut(),
                );
                if let Some(concrete) = concrete {
                    for field in &concrete.fields {
                        self.pool.lay_out(field.type_id, &type_table.borrow());
                    }
                    new_structs.push(concrete);
                }
            }
        }
        new_structs.extend(self.substituted_anon_structs(&mut type_table.borrow_mut()));
        new_structs
    }

    /// The payload types of each of `instances`, a generic variant's
    /// `GenericInstance`, substituted with its type arguments.
    fn variant_payloads(&self, instances: &[TypeId], tt: &mut TypeTable) -> Vec<TypeId> {
        let mut payloads = Vec::new();
        for &id in instances {
            let ResolvedType::GenericInstance { def, type_args } = tt.get(id).clone() else {
                unreachable!("only a `GenericInstance` is kept for the struct drain");
            };
            let (params, cases) = &self.pool.generic_variants[&def];
            let substitution: IndexMap<u32, TypeId> =
                params.iter().copied().zip(type_args).collect();
            for &payload in cases {
                payloads.push(tt.substitute_type_params(payload, &substitution));
            }
        }
        payloads
    }

    /// A struct for each closed anonymous shape a substitution first reached
    /// since the last call, unless a literal wrote it. Each substitution also
    /// awaits its origin's methods.
    fn substituted_anon_structs(&mut self, tt: &mut TypeTable) -> Vec<TirStruct> {
        let (substitutions, end) = tt.closed_shape_substitutions_since(self.structs.anon_scanned);
        self.structs.anon_scanned = end;
        self.structs.pending_shapes.extend(&substitutions);
        let unwritten: Vec<(AnonStructId, usize)> = substitutions
            .into_iter()
            .filter(|&(shape, index)| index == 0 && !tt.anon_struct_is_written(shape))
            .collect();
        unwritten
            .into_iter()
            .map(|(shape, index)| {
                let span = self.structs.anon_sites[&tt.shape_substitution(shape, index).origin];
                let fields = tt.anon_shape_struct_fields(shape);
                TirStruct::anonymous(
                    shape,
                    tt.anon_struct_mangle(shape),
                    tt.anon_struct_module(shape).clone(),
                    &fields,
                    span,
                )
            })
            .collect()
    }

    /// Monomorphize `module` against templates a previous run already holds,
    /// whose `TypeId`s index the module's own table.
    fn monomorphize_with_externals(
        &mut self,
        mut module: TirModule,
        external_generic_functions: &Templates,
        external_generic_structs: &IndexMap<DefId, TirStruct>,
        scanned_functions: usize,
        roots: Option<&Roots>,
    ) -> TirModule {
        // Phase 0: index what written impls define (coherence Rule 1). A generic
        // free function has no rival impl, and indexing it would shadow itself.
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if !func.impl_type_params.is_empty() {
                continue;
            }
            if func.has_real_type_params() && func.method_info.is_none() {
                continue;
            }
            self.functions
                .concrete_names
                .entry(func.module_source.clone())
                .or_default()
                .insert(func.name.clone());
        }

        // Phase 1: Collect all generic struct definitions
        let mut generic_structs = external_generic_structs.clone();
        generic_structs.extend(generic_structs_in(&module.structs));
        let tt = module.type_table.borrow();
        for s in &module.structs {
            if let StructDef::Anon(shape) = s.def {
                self.structs.anon_sites.insert(shape, s.span);
                if tt.anon_shape_is_open(shape) {
                    self.structs
                        .open_shape_heads
                        .insert(tt.fq_struct_head(s.def), shape);
                }
            }
        }
        drop(tt);

        // Phase 7: Collect all generic function definitions
        let mut generic_functions = external_generic_functions.clone();

        for func_rc in &module.functions {
            if func_rc.borrow().is_template() {
                generic_functions.insert(func_rc);
            }
        }

        let generic_functions = Rc::new(generic_functions);
        self.functions.templates = Rc::clone(&generic_functions);

        // The methods synthesis gave each open shape, which every shape
        // substituted from it instantiates. Like a template's, they leave the
        // module: their types are an instance's only once substituted.
        module.functions.retain(|func_rc| {
            let head = func_rc
                .borrow()
                .method_info
                .as_ref()
                .map(LocalMethodName::fq_base_struct_name);
            let Some(&shape) = head.and_then(|head| self.structs.open_shape_heads.get(&head))
            else {
                return true;
            };
            self.structs
                .shape_methods
                .entry(shape)
                .or_default()
                .push(Rc::clone(func_rc));
            false
        });

        // Phase 7.25: leave in the pool what no root is, for a reached body
        // naming it to take out.
        let mut declared_order: IndexMap<FreeFunctionName, usize> = IndexMap::default();
        if let Some(roots) = roots {
            for func_rc in std::mem::take(&mut module.functions) {
                declared_order.insert(function_key(&func_rc.borrow()), declared_order.len());
                let entry = {
                    let func = func_rc.borrow();
                    if func.is_template() {
                        Entry::Root
                    } else {
                        roots.before_monomorphize(&func, &module.type_table.borrow())
                    }
                };
                match entry {
                    Entry::Root => module.functions.push(func_rc),
                    Entry::OnType(head) => self.pool.wait(head, func_rc),
                    Entry::Called => {
                        self.pool.insert(func_rc);
                    }
                }
            }
        }

        let unrewritten = Unrewritten {
            scanned: scanned_functions,
        };

        // Phase 1.5: expand the packs their own site settled, before Phase 2
        // holds the types that interns and Phase 8 collects the calls it makes.
        func_inst::expand_settled_packs(self, &module.type_table, unrewritten.functions(&module));

        // Phase 2: what the drain instantiates — the types the bodies yet to be
        // scanned hold, and the struct fields every declaration lays out.
        {
            let tt = module.type_table.borrow();
            for s in &module.structs {
                for field in &s.fields {
                    self.pool.lay_out(field.type_id, &tt);
                }
                if let StructDef::Decl(def) = s.def
                    && s.monomorph_info.is_none()
                {
                    let fields = s.fields.iter().map(|field| field.type_id).collect();
                    self.pool.declare_members(def, fields, &tt);
                }
            }
            for func_rc in unrewritten.functions(&module) {
                let func = func_rc.borrow();
                if !func.is_template() {
                    self.note_types(&func, &tt);
                }
            }
            for global in unrewritten.globals(&module) {
                self.pool.hold(global.ty, &tt);
                for id in init_types(global.init.slot_expr()) {
                    self.pool.hold(id, &tt);
                }
            }
        }

        // Phase 3-4: a struct's fields may name further instances, hence a fixpoint.
        let new_structs = self.drain_pending_structs(&module.type_table, &generic_structs);
        // Only an instantiated body substitutes into a template shape: no
        // field can name one. Phase 9 gives each its `$hole_fmt`.
        assert!(new_structs.iter().all(|s| match s.def {
            StructDef::Anon(shape) => module.type_table.borrow().template_shape(shape).is_none(),
            StructDef::Decl(_) => true,
        }));
        module.structs.extend(new_structs);

        // Phase 5: Remove generic structs and open shapes from the concrete
        // struct list
        {
            let tt = module.type_table.borrow();
            module.structs.retain(|s| match s.def {
                StructDef::Anon(shape) => !tt.anon_shape_is_open(shape),
                StructDef::Decl(_) => s.type_params.is_empty() || s.monomorph_info.is_some(),
            });
        }

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module, unrewritten);

        // Phase 8: Collect function instantiation sites from Call expressions,
        // and take out of the pool what the bodies name.
        self.collect_function_instantiation_sites(&module, &generic_functions, unrewritten);
        let mut unscanned = Vec::new();
        for func_rc in unrewritten.functions(&module) {
            let func = func_rc.borrow();
            if !func.is_template() {
                self.take_callees(&func, &module.type_table.borrow(), &mut unscanned);
            }
        }
        let tt = module.type_table.borrow();
        for global in unrewritten.globals(&module) {
            let callees = expr_callees(global.init.slot_expr(), &tt);
            self.take_from_pool(&callees, &mut unscanned);
        }
        drop(tt);
        unscanned.append(&mut self.pool.released);

        // Phase 9: instantiate functions and structs to one fixpoint. A bodyless
        // template is a codegen intrinsic, never scanned again.
        let scannable_generic_functions = generic_functions.with_bodies();
        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        loop {
            let taken = std::mem::take(&mut unscanned);
            let mut made_progress = !taken.is_empty();
            func_inst::expand_settled_packs(self, &module.type_table, &taken);
            for func_rc in &taken {
                self.note_types(&func_rc.borrow(), &module.type_table.borrow());
            }

            // Call sites are collected only once the struct drain has run:
            // collecting earlier queues under `GenericInstance` form.
            let mut batch: Vec<TirFunction> = Vec::new();
            while let Some(key) = self.functions.pending.pop() {
                let concrete = {
                    let gf = key
                        .template
                        .as_ref()
                        .and_then(|id| generic_functions.get(id))
                        .unwrap_or_else(|| {
                            panic!("queued `{}` names no registered template", key.name)
                        });
                    let gf_borrowed = gf.borrow();
                    self.instantiate_function(
                        &gf_borrowed,
                        &key,
                        &mut module.type_table.borrow_mut(),
                    )
                };

                if let Some(concrete) = concrete {
                    batch.push(concrete);
                    made_progress = true;
                }
            }

            // Every function scanned below has its types instantiated first,
            // the helpers and shape methods a drain itself adds included.
            let mut noted = 0;
            loop {
                for concrete in &batch[noted..] {
                    self.note_types(concrete, &module.type_table.borrow());
                }
                noted = batch.len();
                let new_structs = self.drain_pending_structs(&module.type_table, &generic_structs);
                if !new_structs.is_empty() {
                    made_progress = true;
                    self.alias_canonical_keys(&mut module.type_table.borrow_mut());
                }
                // A shape only a substitution reaches has no `$hole_fmt` from
                // synthesis; built here, what it calls is instantiated in turn.
                batch.extend(hole_fmt_helpers(
                    &new_structs,
                    &module.type_table,
                    &self.functions.trait_env,
                ));
                module.structs.extend(new_structs);
                for (shape, index) in std::mem::take(&mut self.structs.pending_shapes) {
                    let instances = self.instantiate_shape_substitution(
                        shape,
                        index,
                        &mut module.type_table.borrow_mut(),
                    );
                    made_progress |= !instances.is_empty();
                    batch.extend(instances);
                }
                if noted == batch.len() {
                    break;
                }
            }
            unscanned.append(&mut self.pool.released);

            for func_rc in taken {
                self.scan_taken(
                    &func_rc,
                    &module.type_table,
                    &generic_functions,
                    &mut unscanned,
                );
                module.functions.push(func_rc);
            }

            for mut concrete in batch {
                self.rewrite_types_in_function(&mut concrete, &mut module.type_table.borrow_mut());
                if let Some(body) = &concrete.body {
                    let mut type_table = module.type_table.borrow_mut();
                    let mut collector = func_inst::InstantiationCollector {
                        mono: self,
                        generic_functions: &scannable_generic_functions,
                        type_table: &mut type_table,
                    };
                    collector.visit_block(body);
                }
                self.take_callees(&concrete, &module.type_table.borrow(), &mut unscanned);
                new_functions.push(Rc::new(RefCell::new(concrete)));
            }

            if !made_progress && unscanned.is_empty() {
                break;
            }
        }

        // Phase 10: Add monomorphized functions to module, after the reached
        // functions put back in the order they were declared in rather than
        // reached in, which every later phase emits them by.
        if !declared_order.is_empty() {
            module
                .functions
                .sort_by_cached_key(|func_rc| declared_order[&function_key(&func_rc.borrow())]);
        }
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        module.functions.retain(|f| {
            let func = f.borrow();
            !func.is_template() || func.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module, unrewritten);

        // Phase 12.5: Lower the comparisons on non-primitive types left to trait calls
        func_inst::lower_comparisons_in_module(&mut module, unrewritten, &self.functions.trait_env);
        never_dispatch::trap_never_dispatch_in_module(&mut module, unrewritten);

        // Phase 13: Rewrite types (single pass — unified loop above ensures all structs exist)
        self.rewrite_types_in_module(&mut module, unrewritten);

        module
    }
}
