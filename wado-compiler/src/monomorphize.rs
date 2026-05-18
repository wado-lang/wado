//! Monomorphization pass for Wado TIR
//!
//! This phase instantiates generic structs and functions with concrete types.
//! Monomorphization is a separate compilation phase that runs after type resolution
//! and before the lower phase.
//!
//! The monomorphization process:
//! 1. Collect all generic struct and function definitions
//! 2. Find instantiation sites (`GenericInstance` types, generic function calls)
//! 3. Generate concrete struct and function definitions
//! 4. Rewrite types and function calls to use monomorphized names

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
fn generic_function_name(is_method: bool, module_source: &ModuleSource, name: &str) -> String {
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

/// Monomorphize a `FlatPackage` (`FlatPackage` -> `FlatPackage`)
///
/// This is the main entry point for the monomorphize phase. All per-module data
/// has already been linked into flat lists by the link phase. This function:
/// 1. Collects all generic struct/function definitions (with shadowing)
/// 2. Creates a temporary `TirModule` with the flat data
/// 3. Runs monomorphization to instantiate generics
/// 4. Writes results back to `FlatPackage`
/// 5. Strips effect params (validated by prior effect checker)
pub fn monomorphize(flat: &mut FlatPackage) {
    // Collect all generic functions from the flat list.
    // Link has already set module_source on each function.
    let all_generic_functions: IndexMap<GenericFunctionKey, Rc<RefCell<TirFunction>>> = flat
        .functions
        .iter()
        .filter_map(|func_rc| {
            let func = func_rc.borrow();
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                let key = generic_function_key(func.is_method(), &func.module_source, &func.name);
                Some((key, Rc::clone(func_rc)))
            } else {
                None
            }
        })
        .collect();

    // Collect all generic structs keyed by (name, module_source).
    // This allows same-named generic structs from different modules to coexist.
    let mut resolved_generic_structs: IndexMap<(String, ModuleSource), TirStruct> =
        IndexMap::default();
    for tir_struct in &flat.structs {
        if !tir_struct.type_params.is_empty() {
            let key = (tir_struct.name.clone(), tir_struct.module_source.clone());
            resolved_generic_structs.insert(key, tir_struct.clone());
        }
    }

    // Create a temporary TirModule with all flat data for monomorphization.
    // This reuses the existing Monomorphizer infrastructure without rewriting it.
    let mut temp_module = TirModule::new(flat.entry_module_source.clone());
    temp_module.type_table = flat.type_table.clone();
    temp_module.functions = std::mem::take(&mut flat.functions);
    temp_module.structs = std::mem::take(&mut flat.structs);
    temp_module.globals = std::mem::take(&mut flat.globals);

    // Run monomorphization on the combined module.
    let mut monomorph = Monomorphizer::new(flat.trait_env.clone());
    temp_module = monomorph.monomorphize_with_externals(
        temp_module,
        &all_generic_functions,
        &resolved_generic_structs,
    );

    // Write results back to FlatPackage
    flat.functions = temp_module.functions;
    flat.structs = temp_module.structs;
    flat.globals = temp_module.globals;

    // `module_source` is the canonical namespace, so the
    // `(module_source, name)` pair must be unique across the entire post-mono
    // function set. Two functions sharing a key would silently overwrite
    // each other in the `(module, name)`-keyed registries downstream
    // (`FreeFunctionName`, wasm function naming, `wir_build::func_map`),
    // and the surviving body / signature would then drive codegen for both
    // call sites — producing wasm whose validation typically fails several
    // phases later with a confusing `expected (ref $type), found (ref $type)`
    // message. Detect the collision here so the responsible synthesis or
    // monomorphization path surfaces at its first observable point.
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

/// Determine the module where trait implementations for a concrete type are defined.
/// Used when substituting a type parameter receiver (e.g., `T^Ord::cmp` → `i32^Ord::cmp`)
/// to set the correct `module_source` so DCE can find the target function.
fn module_source_for_trait_impl(type_table: &TypeTable, type_id: TypeId) -> Option<ModuleSource> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(_) => Some(ModuleSource::primitive()),
        ResolvedType::BuiltinArray(_) => Some(ModuleSource::prelude()),
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::GenericInstance { module_source, .. }
        | ResolvedType::Enum { module_source, .. }
        | ResolvedType::Variant { module_source, .. } => Some(module_source.clone()),
        // Newtypes inherit their base type's impls (`type Foo = Array<u8>`
        // gets Array's methods); the body of the inherited generic
        // instantiation lives in the base type's module by convention, so
        // peel through to the base before reading the module source.
        ResolvedType::Newtype { base_type, .. } => {
            module_source_for_trait_impl(type_table, *base_type)
        }
        _ => None,
    }
}

impl Monomorphizer {
    /// Perform monomorphization on a module, optionally with access to external generic
    /// functions and structs from other modules (e.g., Array methods from prelude).
    ///
    /// IMPORTANT: Requires unified type tables - `TypeIds` in external generics
    /// must be valid in the module's `type_table`.
    fn monomorphize_with_externals(
        &mut self,
        mut module: TirModule,
        external_generic_functions: &IndexMap<GenericFunctionKey, Rc<RefCell<TirFunction>>>,
        external_generic_structs: &IndexMap<(String, ModuleSource), TirStruct>,
    ) -> TirModule {
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

        // Build set of valid struct names for collection
        let valid_struct_names: IndexSet<String> = generic_structs
            .keys()
            .map(|(name, _)| name.clone())
            .collect();

        // Phase 2-4: Collect and instantiate structs iteratively
        // This is done in a loop because instantiating a struct (like TreeMap<String,i32>)
        // may create new GenericInstance types in its fields (like BTreeNode<String,i32>)
        // that also need to be instantiated.
        let mut new_structs = Vec::new();
        loop {
            // Collect instantiation sites from current type table
            self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);

            // If no new structs to instantiate, we're done
            if self.structs.pending.is_empty() {
                break;
            }

            // Process all pending struct instantiations
            while let Some(key) = self.structs.pending.pop() {
                let struct_key = (key.name.clone(), key.module_source.clone());
                if let Some(generic_struct) = generic_structs.get(&struct_key)
                    && let Some(concrete) = self.instantiate_struct(
                        generic_struct,
                        &key,
                        &mut module.type_table.borrow_mut(),
                    )
                {
                    new_structs.push(concrete);
                }
            }
        }

        // Add monomorphized structs to module
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

        // Phase 8: Collect function instantiation sites from Call expressions
        self.collect_function_instantiation_sites(&module, &generic_functions);

        // Phase 9: Process function instantiations and generate concrete functions
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

        // Phase 9: Unified instantiation loop
        // Process functions and structs together until fixpoint. Function instantiation
        // may create new GenericInstance types that require struct instantiation, which
        // in turn may create new function instantiation sites. Processing them in a
        // single loop eliminates the need for the separate "Phase 13" second pass.
        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        loop {
            let mut made_progress = false;

            // Batch one round of function instantiations before draining the
            // struct pending. The ordering inside each outer iteration is
            //
            //     instantiate all pending functions
            //     → drain struct pending to fixpoint (once)
            //     → rewrite each new function's body through the now-stable
            //       `GenericInstance → Struct` substitutions
            //     → collect each new function's call sites
            //
            // and is the load-bearing piece of `function_id_for` injectivity
            // over `project.functions`:
            //
            // 1. `instantiate_function` substitutes the function body; for any
            //    `GenericInstance` whose monomorphised `Struct` is not yet
            //    interned, `substitute_type` creates a fresh `GenericInstance`
            //    in the type table (see `substitute.rs`'s `make_generic_instance`
            //    fallback). The body therefore points at `GenericInstance`
            //    `TypeId`s for not-yet-monomorphised types.
            //
            // 2. Drain `self.structs.pending` (after instantiating the whole
            //    batch of functions) to fixpoint — struct monomorphisation
            //    of the new `GenericInstance`s plus any recursively triggered
            //    structs. After this step `self.structs.type_substitutions`
            //    covers every `GenericInstance → Struct` pair reachable from
            //    any body in the batch.
            //
            // 3. `rewrite_types_in_function` rewrites every `TypeId` in each
            //    new body — including `Call`/`MethodCall::type_args` —
            //    through `type_substitutions`, so the body is in canonical
            //    `Struct` form.
            //
            // 4. `collect_function_instantiation_sites` finally walks the
            //    canonicalised body. Every queued `InstantiationKey` carries
            //    `Struct`-form `TypeId`s, matching the form receiver-driven
            //    queue paths use. The two paths produce identical
            //    `Hash`/`Eq` keys, so `try_queue_function`'s dedupe folds
            //    them by construction and `function_id_for` is injective.
            //
            // The previous interleaving (drain all functions while also
            // collecting their calls, then all structs) ran step 4 before
            // step 2, so any function call whose argument types referenced a
            // not-yet-monomorphised `GenericInstance` queued under the
            // `GenericInstance` form; later siblings of the same call (after
            // struct mono caught up) queued under the `Struct` form,
            // producing two `TirFunction`s with the same `function_id_for`.
            //
            // Batching (instead of running the struct drain per function)
            // keeps `collect_instantiation_sites` — an `O(|type_table|)` scan
            // — at one call per outer iteration rather than one per function,
            // which is what the previous design's cost profile relied on.

            // Step 1: instantiate every pending function. Defer
            // rewrite/collect to steps 3/4 once the struct drain has run.
            let mut batch: Vec<TirFunction> = Vec::new();
            while let Some(key) = self.functions.pending.pop() {
                let concrete = {
                    // Templates live in the module that hosts the impl block;
                    // the queueing convention puts the *instantiation* at the
                    // receiver type's module (per the
                    // `inspect_ref_array_field.wado` contract). Try the queue
                    // key's module first, then fall back to `TraitEnv` so
                    // cross-module impls (e.g. `impl<T> Serialize for Option<T>`
                    // in `core:serde`) are still discoverable.
                    // Issue #1110 (1)(2): every producer sets
                    // `FunctionRef::module_source` to the body's home
                    // module — `resolver::method_call::resolve_method_call`,
                    // `synthesis::traits` (via `resolve_impl_module_via_env`),
                    // `synthesis::template::trait_impl_module`, etc.,
                    // all query `TraitEnv` for the impl block's actual
                    // module. The literal `(module_source, name)` lookup
                    // is therefore total: a miss is an unreachable code
                    // path, surfaced as a panic below.
                    let lookup_key = (key.module_source.clone(), key.name.clone());
                    let generic_func = generic_functions.get(&lookup_key);
                    // Generics have a defined home module by convention: a
                    // template that's queued for instantiation must exist in
                    // `generic_functions` either at the queue's own
                    // `module_source` (the common case) or at a module
                    // reachable through `TraitEnv` / the inherent-method
                    // scan (newtype-inherits-base and similar). If no
                    // template is reachable, the call was registered as a
                    // generic but no provider exists — surface it as a
                    // compiler bug rather than silently dropping the
                    // instantiation. A real failure here points at a
                    // missing prelude definition or a synthesis path that
                    // queues a key it never registered a template for.
                    let gf = generic_func.unwrap_or_else(|| {
                        panic!(
                            "no generic template for queued instantiation \
                             `{}` at module `{}` (impl_type_args={:?}, \
                             method_type_args={:?}); a generic dispatch \
                             must always have a defined home module",
                            key.name, key.module_source, key.impl_type_args, key.method_type_args,
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
            loop {
                self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);
                if self.structs.pending.is_empty() {
                    break;
                }
                while let Some(struct_key) = self.structs.pending.pop() {
                    let key_pair = (struct_key.name.clone(), struct_key.module_source.clone());
                    if let Some(generic_struct) = generic_structs.get(&key_pair)
                        && let Some(s) = self.instantiate_struct(
                            generic_struct,
                            &struct_key,
                            &mut module.type_table.borrow_mut(),
                        )
                    {
                        module.structs.push(s);
                        made_progress = true;
                    }
                }
            }

            // Steps 3 + 4: rewrite each new body, then collect its call sites.
            for mut concrete in batch {
                self.rewrite_types_in_function(&mut concrete, &mut module.type_table.borrow_mut());
                if let Some(body) = &concrete.body {
                    let type_table = module.type_table.borrow();
                    let mut collector = func_inst::InstantiationCollector {
                        mono: self,
                        generic_functions: &scannable_generic_functions,
                        type_table: &type_table,
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
