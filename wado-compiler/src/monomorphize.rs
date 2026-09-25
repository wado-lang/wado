//! Monomorphization for Wado TIR, between type resolution and lowering: collect
//! the generic struct and function definitions, find their instantiation sites,
//! generate a concrete definition per site, and rewrite types and calls onto the
//! monomorphized names.

mod call_instance;
mod call_rewrite;
mod func_inst;
mod state;
mod struct_inst;
mod substitute;

use std::cell::RefCell;
use std::rc::Rc;

use crate::defs::DefId;
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::FreeFunctionName;
use crate::tir::{
    MonomorphInfo, ResolvedType, TemplateId, TirFunction, TirModule, TirStruct, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

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
    };
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
}

impl Monomorphization {
    /// Instantiate what the bodies added since the last run reach.
    pub fn resume(&mut self, flat: &mut FlatPackage) {
        self.run(flat);
    }

    fn run(&mut self, flat: &mut FlatPackage) {
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
        );
        self.scanned_functions = temp_module.functions.len();
        write_back(flat, temp_module);
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

    flat.rebuild_variant_indices();
}

/// The module a concrete type's trait impls are sought in, when a
/// `T^Trait::method` receiver is substituted.
fn module_source_for_trait_impl(type_table: &TypeTable, type_id: TypeId) -> Option<ModuleSource> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => Some(ModuleSource::of_primitive(*prim)),
        ResolvedType::Unit => Some(ModuleSource::primitive()),
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
    /// Instantiate the queued structs, and those their fields queue, to a
    /// fixpoint; the concrete structs produced.
    fn drain_pending_structs(
        &mut self,
        type_table: &RefCell<TypeTable>,
        generic_structs: &IndexMap<DefId, TirStruct>,
    ) -> Vec<TirStruct> {
        let mut new_structs = Vec::new();
        loop {
            self.collect_instantiation_sites(&type_table.borrow(), generic_structs);
            if self.structs.pending.is_empty() {
                break;
            }
            while let Some(key) = self.structs.pending.pop() {
                let def = key
                    .def
                    .expect("a struct instantiation names its declaration");
                if let Some(concrete) = self.instantiate_struct(
                    &generic_structs[&def],
                    &key,
                    &mut type_table.borrow_mut(),
                ) {
                    new_structs.push(concrete);
                }
            }
        }
        new_structs
    }

    /// Monomorphize `module` against templates a previous run already holds,
    /// whose `TypeId`s index the module's own table.
    fn monomorphize_with_externals(
        &mut self,
        mut module: TirModule,
        external_generic_functions: &Templates,
        external_generic_structs: &IndexMap<DefId, TirStruct>,
        scanned_functions: usize,
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

        // Phase 2-4: a struct's fields may name further instances, hence a fixpoint.
        let new_structs = self.drain_pending_structs(&module.type_table, &generic_structs);
        module.structs.extend(new_structs);

        // Phase 5: Remove generic structs from the concrete struct list
        module
            .structs
            .retain(|s| s.type_params.is_empty() || s.monomorph_info.is_some());

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module);

        // Phase 7: Collect all generic function definitions
        let mut generic_functions = external_generic_functions.clone();

        for func_rc in &module.functions {
            if func_rc.borrow().is_template() {
                generic_functions.insert(func_rc);
            }
        }

        let generic_functions = Rc::new(generic_functions);
        self.functions.templates = Rc::clone(&generic_functions);

        // Phase 7.5: expand the packs their own site settled, before Phase 8
        // collects the calls that produces.
        func_inst::expand_settled_packs_in_module(self, &mut module);

        // Phase 8: Collect function instantiation sites from Call expressions
        self.collect_function_instantiation_sites(&module, &generic_functions, scanned_functions);

        // Phase 9: instantiate functions and structs to one fixpoint. A bodyless
        // template is a codegen intrinsic, never scanned again.
        let scannable_generic_functions = generic_functions.with_bodies();
        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        loop {
            let mut made_progress = false;

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

            // Once per batch: the drain scans the whole type table.
            let new_structs = self.drain_pending_structs(&module.type_table, &generic_structs);
            if !new_structs.is_empty() {
                made_progress = true;
                self.alias_canonical_keys(&mut module.type_table.borrow_mut());
            }
            module.structs.extend(new_structs);

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
                new_functions.push(Rc::new(RefCell::new(concrete)));
            }

            if !made_progress {
                break;
            }
        }

        // Phase 10: Add monomorphized functions to module
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        module.functions.retain(|f| {
            let func = f.borrow();
            !func.is_template() || func.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        // Phase 12.5: Lower the comparisons on non-primitive types left to trait calls
        func_inst::lower_comparisons_in_module(&mut module, &self.functions.trait_env);

        // Phase 13: Rewrite types (single pass — unified loop above ensures all structs exist)
        self.rewrite_types_in_module(&mut module);

        module
    }
}
