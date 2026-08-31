//! Monomorphization for Wado TIR, between type resolution and lowering: collect
//! the generic struct and function definitions, find their instantiation sites,
//! generate a concrete definition per site, and rewrite types and calls onto the
//! monomorphized names.

mod call_rewrite;
mod func_inst;
mod state;
mod struct_inst;
mod substitute;

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::FreeFunctionName;

/// Key used to store/look up a generic function in the global function map.
///
/// `module_source` is the function body's home module (where it is registered
/// in `Package::functions` as `(ModuleSource, name)`). Two generic methods
/// that share a mangled name across different modules — for instance, a
/// `struct Tuple` in a user module vs. core's variadic-tuple impl in
/// `core:prelude/tuple` — coexist in this map because their keys differ by
/// `module_source`. Without this disambiguation, the second insertion silently
/// overwrites the first and `try_queue_function` picks up the wrong template.
pub(crate) type GenericFunctionKey = (ModuleSource, String);

/// The bare-string form of the function name used inside `InstantiationKey.name`
/// (which downstream codegen feeds into `mangle_generic_name`).
///
/// Methods stay unqualified — the struct name already provides namespace.
/// Free functions get a module-qualified `FreeFunctionName` string so the
/// post-mono mangled name is stable across modules.
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

/// `(module_source, generic_function_name(...))` — the canonical lookup key.
fn generic_function_key(
    is_method: bool,
    module_source: &ModuleSource,
    name: &str,
) -> GenericFunctionKey {
    (
        module_source.clone(),
        generic_function_name(is_method, module_source, name),
    )
}

use crate::flat_package::FlatPackage;
use crate::tir::{ResolvedType, TirFunction, TirModule, TirStruct, TypeId, TypeTable};

use state::Monomorphizer;

/// Instantiate every generic the linked package reaches, and hand back the
/// session so a pass that adds bodies later can resume it.
pub fn monomorphize(flat: &mut FlatPackage) -> Monomorphization {
    let mut session = Monomorphization {
        generic_functions: flat
            .functions
            .iter()
            .filter_map(|func_rc| {
                let func = func_rc.borrow();
                if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                    let key =
                        generic_function_key(func.is_method(), &func.module_source, &func.name);
                    Some((key, Rc::clone(func_rc)))
                } else {
                    None
                }
            })
            .collect(),
        // Keyed by `(name, module_source)`, so same-named generic structs from
        // different modules coexist.
        generic_structs: flat
            .structs
            .iter()
            .filter(|s| !s.type_params.is_empty())
            .map(|s| ((s.name.clone(), s.module_source.clone()), s.clone()))
            .collect(),
        monomorphizer: Monomorphizer::new(flat.trait_env.clone()),
        scanned_functions: 0,
    };
    session.run(flat);
    session
}

/// One resumable monomorphization. A run consumes the templates it instantiates
/// and remembers its instances here, so a second run must resume rather than start.
pub struct Monomorphization {
    monomorphizer: Monomorphizer,
    generic_functions: IndexMap<GenericFunctionKey, Rc<RefCell<TirFunction>>>,
    generic_structs: IndexMap<(String, ModuleSource), TirStruct>,
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

    // `(module_source, name)` must be unique across the post-mono function set:
    // two functions sharing a key overwrite each other in the `(module, name)`
    // registries downstream, and the survivor then drives codegen for both call
    // sites — surfacing several phases later as a confusing
    // `expected (ref $type), found (ref $type)`. Catch it at its origin.
    let mut seen_functions: IndexMap<(ModuleSource, String), ()> = IndexMap::default();
    for func_rc in &flat.functions {
        let f = func_rc.borrow();
        let key = (f.module_source.clone(), f.name.clone());
        assert!(
            seen_functions.insert(key, ()).is_none(),
            "duplicate function `{}` in module `{}` after monomorphization. \
             `module_source` is the canonical namespace; two functions with \
             the same mangled name landing in the same module indicate a \
             synthesis or monomorphization bug.",
            f.name,
            f.module_source
        );
    }

    // Strip effect params from all functions. Effect params have been validated by the
    // effect checker (which runs before monomorphization) and are not needed downstream.
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        func.effects.retain(|e| !e.is_param());
    }

    // Rebuild variant index since structs may have changed
    flat.rebuild_variant_indices();
}

/// Peel `&`/`&mut` and newtypes off a dispatch receiver — the same
/// transparency the struct-info lookups apply when they report the struct a
/// call keys on. A newtype that inherits its base's impl is dispatched through
/// the base, so keeping the newtype's own identity would name a template that
/// does not exist.
fn dispatch_receiver_type(tt: &TypeTable, type_id: TypeId) -> TypeId {
    let mut tid = tt.peel_refs(type_id);
    loop {
        match tt.get_unerased(tid) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => tid = *inner,
            // A newtype that inherits its base's impl dispatches through the
            // base; one that declares its own is its own receiver. `flags` is
            // always the latter — its impls are written against the flags
            // name, never against the `u32` it erases to.
            ResolvedType::Newtype { base_type, .. } => tid = *base_type,
            _ => return tid,
        }
    }
}

/// The declared identity `type_id` names, when erasure has replaced it with a
/// representation. `fq_base_type_name` reads the erased view, which answers
/// `u32` for every `flags` type and so names a template no impl declares —
/// impls on a `flags` type are written against the flags name.
fn dispatch_receiver_identity(tt: &TypeTable, type_id: TypeId) -> Option<crate::name::FqTypeName> {
    match tt.get_unerased(type_id) {
        ResolvedType::Flags { def } => Some(crate::name::FqTypeName::declared(tt.defs(), *def)),
        _ => None,
    }
}

/// The receiver *head* a dispatch template is named after: no type arguments,
/// for keys that carry them in `impl_type_args`. Prefer it over a
/// struct-instantiation key's `name`, which carries no module.
fn dispatch_receiver_head(tt: &TypeTable, type_id: TypeId) -> crate::name::FqTypeName {
    let tid = dispatch_receiver_type(tt, type_id);
    dispatch_receiver_identity(tt, tid).unwrap_or_else(|| tt.fq_base_type_name(tid))
}

/// The receiver's full instantiated name, for keys whose `impl_type_args` are
/// empty because the instantiation is spelled into the name itself
/// (`StructField<Thing,i32>::get`). Using the bare head here would collapse
/// every instantiation onto one key and mint an instance whose body still
/// carries the impl's type parameters.
fn dispatch_receiver_name(tt: &TypeTable, type_id: TypeId) -> crate::name::FqTypeName {
    let tid = dispatch_receiver_type(tt, type_id);
    dispatch_receiver_identity(tt, tid).unwrap_or_else(|| tt.fq_type_name(tid))
}

/// Determine the module where trait implementations for a concrete type are defined.
/// Used when substituting a type parameter receiver (e.g., `T^Ord::cmp` → `i32^Ord::cmp`)
/// to set the correct `module_source` so DCE can find the target function.
fn module_source_for_trait_impl(type_table: &TypeTable, type_id: TypeId) -> Option<ModuleSource> {
    match type_table.get(type_id) {
        // `()` is a builtin like the primitives: it names no declaring module,
        // and an `impl Trait for ()` is reached through the same hint-then-scan
        // lookup they are.
        ResolvedType::Primitive(_) | ResolvedType::Unit => Some(ModuleSource::primitive()),
        ResolvedType::BuiltinArray(_) => Some(ModuleSource::array()),
        ResolvedType::Struct { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::Variant { .. } => type_table.nominal_head(type_id).map(|(_, m)| m),
        // Newtypes inherit their base type's impls (`type Foo = List<u8>`
        // gets List's methods); the body of the inherited generic
        // instantiation lives in the base type's module by convention, so
        // peel through to the base before reading the module source.
        ResolvedType::Newtype { base_type, .. } => {
            module_source_for_trait_impl(type_table, *base_type)
        }
        _ => None,
    }
}

impl Monomorphizer {
    /// Drain `self.structs.pending` to fixpoint, instantiating each queued
    /// struct plus any structs its fields transitively queue. Returns the
    /// concrete structs produced (the caller appends them to the module).
    fn drain_pending_structs(
        &mut self,
        type_table: &RefCell<TypeTable>,
        generic_structs: &IndexMap<(String, ModuleSource), TirStruct>,
        valid_struct_names: &IndexSet<String>,
    ) -> Vec<TirStruct> {
        let mut new_structs = Vec::new();
        loop {
            self.collect_instantiation_sites(&type_table.borrow(), valid_struct_names);
            if self.structs.pending.is_empty() {
                break;
            }
            while let Some(key) = self.structs.pending.pop() {
                let struct_key = (key.name.clone(), key.module_source.clone());
                // The template map is keyed by `TirStruct::name`, the storage
                // spelling — mangled for a function-local declaration — while
                // an instantiation names the declaration. When the two differ,
                // resolve the declaration and ask again under what it renders
                // to; otherwise a local `struct Box<T>` is never instantiated
                // and nothing downstream is registered for it.
                let rendered_key = {
                    let tt = type_table.borrow();
                    key.def
                        .map(|def| (tt.decl_render_name(def), key.module_source.clone()))
                        .filter(|k| *k != struct_key)
                };
                let generic_struct = generic_structs
                    .get(&struct_key)
                    .or_else(|| rendered_key.as_ref().and_then(|k| generic_structs.get(k)));
                if let Some(generic_struct) = generic_struct
                    && let Some(concrete) =
                        self.instantiate_struct(generic_struct, &key, &mut type_table.borrow_mut())
                {
                    new_structs.push(concrete);
                }
            }
        }
        new_structs
    }

    /// Perform monomorphization on a module, optionally with access to external generic
    /// functions and structs from other modules (e.g., List methods from prelude).
    ///
    /// IMPORTANT: Requires unified type tables - `TypeIds` in external generics
    /// must be valid in the module's `type_table`.
    fn monomorphize_with_externals(
        &mut self,
        mut module: TirModule,
        external_generic_functions: &IndexMap<GenericFunctionKey, Rc<RefCell<TirFunction>>>,
        external_generic_structs: &IndexMap<(String, ModuleSource), TirStruct>,
        scanned_functions: usize,
    ) -> TirModule {
        // Phase 0: index the functions a written impl already defines, so a
        // template instantiation landing on one of those names is never queued
        // (coherence Rule 1, WEP 2026-03-14 §5).
        //
        // An impl member is indexed under its base name even when the method
        // declares its own type params, since the rivalry is between the
        // *impls*. A free generic function has no such rival, and indexing it
        // would shadow its own instantiations.
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

        // Phase 1: Collect all generic struct definitions keyed by (name, module_source).
        // Same-named structs from different modules coexist; the InstantiationKey's
        // module_source selects the correct template at instantiation time.
        let mut generic_structs: IndexMap<(String, ModuleSource), TirStruct> =
            external_generic_structs.clone();

        for tir_struct in &module.structs {
            if !tir_struct.type_params.is_empty() {
                let key = (tir_struct.name.clone(), tir_struct.module_source.clone());
                generic_structs.insert(key, tir_struct.clone());
            }
        }

        // Store in module for later phases
        module.generic_structs.clone_from(&generic_structs);

        // The names instantiation sites spell — the *declared* ones. The map
        // is keyed by storage spelling, which for a function-local generic
        // struct carries a disambiguator no site writes, so keying the filter
        // off the keys dropped every local generic before it could become
        // pending. `drain_pending_structs` disambiguates by identity once a
        // site is admitted.
        let valid_struct_names: IndexSet<String> = {
            let tt = module.type_table.borrow();
            generic_structs
                .values()
                .map(|s| {
                    s.def
                        .decl()
                        .map_or_else(|| s.name.clone(), |d| tt.def_name(d).to_string())
                })
                .collect()
        };

        // Phase 2-4: Collect and instantiate structs iteratively
        // This is done in a loop because instantiating a struct (like TreeMap<String,i32>)
        // may create new GenericInstance types in its fields (like BTreeNode<String,i32>)
        // that also need to be instantiated.
        let new_structs =
            self.drain_pending_structs(&module.type_table, &generic_structs, &valid_struct_names);
        module.structs.extend(new_structs);

        // Phase 5: Remove generic structs from the concrete struct list
        module
            .structs
            .retain(|s| s.type_params.is_empty() || s.monomorph_info.is_some());

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module);

        // Phase 7: Collect all generic function definitions
        // Include both local functions AND external generic functions from other modules
        let mut generic_functions: IndexMap<GenericFunctionKey, Rc<RefCell<TirFunction>>> =
            external_generic_functions.clone();

        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                let key = generic_function_key(func.is_method(), &func.module_source, &func.name);
                generic_functions.insert(key, Rc::clone(func_rc));
            }
        }

        // Store in module for later phases
        module.generic_functions.clone_from(&generic_functions);

        // Hand the same registry to the monomorphizer: variadic-for-of
        // expansion reads it to tell a callee's method type params from its
        // ordinary parameters. Grow-only above this point, so one snapshot
        // shared by `Rc` serves the whole run.
        self.functions.templates = Rc::new(generic_functions.clone());

        // Phase 8: Collect function instantiation sites from Call expressions
        self.collect_function_instantiation_sites(&module, &generic_functions, scanned_functions);

        // Phase 9: Process function instantiations and generate concrete functions.
        // Use iterative approach: each newly instantiated function may have method calls
        // that need to be instantiated too (e.g., a generic method calling another generic
        // method on self, like sort() -> sort_by())
        //
        // For transitive scanning, exclude bodyless functions (builtins like array_new,
        // array_set, etc.) which are codegen intrinsics and must not be re-monomorphized.
        let scannable_generic_functions: IndexMap<GenericFunctionKey, Rc<RefCell<TirFunction>>> =
            generic_functions
                .iter()
                .filter(|(_, f)| f.borrow().body.is_some())
                .map(|(k, v)| (k.clone(), Rc::clone(v)))
                .collect();

        // Unified instantiation loop.
        // Process functions and structs together until fixpoint. Function instantiation
        // may create new GenericInstance types that require struct instantiation, which
        // in turn may create new function instantiation sites. Processing them in a
        // single loop removes the second function-instantiation pass the pipeline
        // previously needed.
        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        loop {
            let mut made_progress = false;

            // Each outer iteration instantiates every pending function, drains
            // the struct pending to fixpoint, rewrites the new bodies through
            // the now-stable `GenericInstance → Struct` substitutions, and only
            // then collects call sites — collecting earlier would queue under
            // `GenericInstance` form and break `function_id_for` injectivity.

            // Step 1: instantiate every pending function. Defer
            // rewrite/collect to steps 3/4 once the struct drain has run.
            let mut batch: Vec<TirFunction> = Vec::new();
            while let Some(key) = self.functions.pending.pop() {
                let concrete = {
                    // Every producer routes through `TraitEnv` and so sets
                    // `FunctionRef::module_source` to the body's home module,
                    // making the literal `(module_source, name)` lookup total: a
                    // miss is a producer bug, surfaced as the panic below.
                    let lookup_key = (key.module_source.clone(), key.name.clone());
                    let generic_func = generic_functions.get(&lookup_key);
                    // No fallback: every queue producer above (in
                    // `func_inst::collect_function_instantiation_sites`)
                    // reads `module_source` straight off the matched
                    // template, so the literal lookup is total. A miss
                    // means a producer skipped the routing rules — a
                    // compiler bug, not a missing-impl-at-the-call-site
                    // condition.
                    let gf = generic_func.unwrap_or_else(|| {
                        let available: Vec<&ModuleSource> = generic_functions
                            .keys()
                            .filter(|(_, n)| n == &key.name)
                            .map(|(m, _)| m)
                            .collect();
                        panic!(
                            "no generic template for queued instantiation \
                             `{}` at module `{}` (impl_type_args={:?}, \
                             method_type_args={:?}); templates with this name \
                             exist at: {:?}. Producer set the wrong \
                             `FunctionRef::module_source` — issue #1110 (1)",
                            key.name,
                            key.module_source,
                            key.impl_type_args,
                            key.method_type_args,
                            available,
                        )
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

            // Step 2: drain struct pending to fixpoint, once for the whole
            // batch. `collect_instantiation_sites` scans the type table —
            // doing it per-function would be `O(N · |type_table|)` and is
            // the source of the historical compiler-time regression.
            let new_structs = self.drain_pending_structs(
                &module.type_table,
                &generic_structs,
                &valid_struct_names,
            );
            if !new_structs.is_empty() {
                made_progress = true;
            }
            module.structs.extend(new_structs);

            // Steps 3 + 4: rewrite each new body, then collect its call sites.
            for mut concrete in batch {
                self.rewrite_types_in_function(&mut concrete, &mut module.type_table.borrow_mut());
                if let Some(body) = &concrete.body {
                    let mut type_table = module.type_table.borrow_mut();
                    let mut collector = func_inst::InstantiationCollector {
                        mono: self,
                        generic_functions: &scannable_generic_functions,
                        type_table: &mut type_table,
                    };
                    use crate::tir_visitor::TirRefVisitor;
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
            (!func.has_real_type_params() && func.impl_type_params.is_empty())
                || func.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        // Phase 12.5: Lower remaining comparison operators on non-primitive types to
        // trait method calls. This handles comparisons in concrete (non-generic) functions
        // on Struct/Variant/GenericInstance types that weren't resolved at resolve time.
        func_inst::lower_comparisons_in_module(&mut module, &self.functions.trait_env);

        // Phase 13: Rewrite types (single pass — unified loop above ensures all structs exist)
        self.rewrite_types_in_module(&mut module);

        module
    }
}
