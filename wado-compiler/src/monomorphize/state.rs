//! Monomorphizer state: instantiation tracking and name generation.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::defs::DefId;
use crate::elaborator::trait_env::TraitEnv;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::monomorphize::Templates;
use crate::name::{FqTraitName, FqTypeName, LocalMethodName, MethodName, mangle_generic_name};
use crate::synthesis::template::written_impl_reaches;
use crate::tir::{InstantiationKey, ResolvedType, TemplateId, TirTypeParam, TypeId, TypeTable};

/// Tracks struct monomorphization state
pub(super) struct StructInstState {
    /// Map from (`generic_name`, `type_args`) to mangled name
    pub instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending struct instantiations
    pub pending: Vec<InstantiationKey>,
    /// Map from `GenericInstance` `TypeId` to monomorphized Struct `TypeId`
    pub type_substitutions: IndexMap<TypeId, TypeId>,
    /// Map from `GenericInstance` `TypeId` to mangled struct name
    pub type_to_mangled_name: IndexMap<TypeId, String>,
}

/// Tracks function monomorphization state
pub(super) struct FuncInstState {
    /// Each key to its instance's mangled name, also under its canonical form
    /// ([`Monomorphizer::alias_canonical_keys`]).
    pub instantiated: IndexMap<InstantiationKey, String>,
    /// Each queued instance's `(module, mangled name)` to its template: two keys
    /// can name one body, never two templates.
    pub instantiated_homes: IndexMap<(ModuleSource, String), TemplateId>,
    /// Work queue of pending function instantiations, each a key of
    /// [`Self::instantiated`].
    pub pending: Vec<InstantiationKey>,
    pub trait_env: Arc<TraitEnv>,
    /// Every function template in the project; absence means the callee is
    /// emitted as written.
    pub templates: Rc<Templates>,
    /// Per module, the names written impls define; one for a single instance
    /// wins over the template's (coherence Rule 1, WEP 2026-03-14 §5).
    pub concrete_names: IndexMap<ModuleSource, IndexSet<String>>,
}

impl FuncInstState {
    /// The module owning the impl behind `info`, restricted to fully concrete
    /// impls. Mono needs this rather than the broader
    /// [`TraitEnv::impl_module_for`] because a generic impl's post-substitution
    /// function is materialised in the *receiver's* module. Keyed by the
    /// post-substitution `struct_name`, which a concrete impl keys on too.
    pub fn impl_module(
        &self,
        info: &LocalMethodName,
        type_module: Option<&ModuleSource>,
    ) -> Option<ModuleSource> {
        self.trait_env
            .concrete_impl_module_of(info, type_module)
            .cloned()
    }

    /// Whether a concrete (non-generic) impl block defines `info`.
    pub fn has_impl(&self, info: &LocalMethodName) -> bool {
        self.impl_module(info, None).is_some()
    }

    /// Module of a non-blanket impl, generic ones included, defining `info` on
    /// `instance`; `None` where the head's written impls all miss `instance`.
    pub fn generic_or_concrete_impl_module(
        &self,
        info: &LocalMethodName,
        type_module: Option<&ModuleSource>,
        instance: TypeId,
        type_table: &TypeTable,
    ) -> Option<ModuleSource> {
        if let Some(trait_) = info.trait_decl() {
            let instance = type_table.peel_refs(instance);
            if self
                .trait_env
                .has_any_methodful_impl_by_receiver(&type_table.impl_receiver_key(instance), trait_)
                && !written_impl_reaches(&self.trait_env, trait_, instance, type_table)
            {
                return None;
            }
        }
        self.trait_env
            .any_impl_module_of(info, type_module)
            .cloned()
    }
}

/// Collects generic instantiations and generates concrete types. It has no
/// current module: each `FunctionRef::module_source` names its own (issue #1110).
pub(super) struct Monomorphizer {
    pub structs: StructInstState,
    pub functions: FuncInstState,
    /// How many leading substitution slots of the function being instantiated
    /// are its impl's type params rather than its own.
    pub current_impl_type_param_count: usize,
    /// The receiver head of the impl method being instantiated; only a call on
    /// that same head inherits its impl type args.
    pub current_impl_receiver: Option<FqTypeName>,
    /// Each type param of the function being instantiated, by name, to its
    /// substitution slot: `T^Trait::method` finds its receiver by name.
    pub current_param_substitution_key: IndexMap<String, u32>,
    /// Pack index to the pack's whole tuple while one unrolled element is
    /// substituted, for a `[..T]` spelling that still means the whole.
    pub pack_splice_bindings: RefCell<IndexMap<u32, TypeId>>,
    /// Template local slots an unrolled copy already holds; shared, since a
    /// nested unroll competes for the same slots. Cleared per instantiation.
    pub unrolled_local_claims: RefCell<IndexSet<u32>>,
}

impl Monomorphizer {
    pub fn new(trait_env: Arc<TraitEnv>) -> Self {
        Self {
            structs: StructInstState {
                instantiated: IndexMap::default(),
                pending: Vec::new(),
                type_substitutions: IndexMap::default(),
                type_to_mangled_name: IndexMap::default(),
            },
            functions: FuncInstState {
                instantiated: IndexMap::default(),
                instantiated_homes: IndexMap::default(),
                pending: Vec::new(),
                trait_env,
                templates: Rc::new(Templates::default()),
                concrete_names: IndexMap::default(),
            },
            current_impl_type_param_count: 0,
            current_impl_receiver: None,
            current_param_substitution_key: IndexMap::default(),
            pack_splice_bindings: RefCell::new(IndexMap::default()),
            unrolled_local_claims: RefCell::new(IndexSet::default()),
        }
    }

    /// Claim `local` for an unrolled copy, returning whether it was free. A
    /// taken slot means a previous copy holds it and this one must reallocate.
    pub fn claim_unrolled_local(&self, local: u32) -> bool {
        self.unrolled_local_claims.borrow_mut().insert(local)
    }

    /// Bind `index`'s splice positions to `tuple` for the duration of one
    /// unrolled element body, returning the binding it displaced so a nested
    /// variadic for-of restores it (see [`Self::pack_splice_bindings`]).
    pub fn bind_pack_splice(&self, index: u32, tuple: TypeId) -> Option<TypeId> {
        self.pack_splice_bindings.borrow_mut().insert(index, tuple)
    }

    pub fn restore_pack_splice(&self, index: u32, previous: Option<TypeId>) {
        let mut bindings = self.pack_splice_bindings.borrow_mut();
        match previous {
            Some(tuple) => {
                bindings.insert(index, tuple);
            }
            None => {
                bindings.shift_remove(&index);
            }
        }
    }

    /// Queue a struct instantiation if not already queued. Returns true if newly queued.
    pub fn try_queue_struct(&mut self, key: InstantiationKey, mangled_name: String) -> bool {
        if self.structs.instantiated.contains_key(&key) {
            return false;
        }
        self.structs.instantiated.insert(key.clone(), mangled_name);
        self.structs.pending.push(key);
        true
    }

    /// Look up the mangled name for a function instantiation by key.
    pub fn lookup_function_instantiation(&self, key: &InstantiationKey) -> Option<&String> {
        self.functions.instantiated.get(key)
    }

    /// Coherence Rule 1 (WEP 2026-03-14 §5): whether a written impl already
    /// occupies this instantiation's name, so queueing the template would both
    /// shadow that impl and collide with it in the module namespace.
    ///
    /// Only a template carrying impl-level arguments has such a rival. A
    /// concrete impl's own generic method carries none, so it still reaches its
    /// instantiations even though its base name is what sits in
    /// `concrete_names` — and that base is what a method-generic template is
    /// compared against, the two differing only by the trailing method args.
    fn concrete_impl_owns_name(
        &self,
        key: &InstantiationKey,
        mangled_name: &str,
        type_table: &TypeTable,
    ) -> bool {
        if key.impl_type_args.is_empty() {
            return false;
        }
        let Some(names) = self.functions.concrete_names.get(&key.module_source) else {
            return false;
        };
        if names.contains(mangled_name) {
            return true;
        }
        if key.method_type_args.is_empty() {
            return false;
        }
        let base_key = InstantiationKey {
            def: None,
            method_type_args: Vec::new(),
            ..key.clone()
        };
        names.contains(&self.method_instantiation_name(&base_key, type_table))
    }

    /// Queue a function instantiation unless its body already is. A body is its
    /// module and mangled name, since one type can reach it under two `TypeId`s.
    pub fn try_queue_function(
        &mut self,
        key: InstantiationKey,
        mangled_name: String,
        type_table: &TypeTable,
    ) -> bool {
        if self.functions.instantiated.contains_key(&key) {
            return false;
        }
        assert!(
            !self.concrete_impl_owns_name(&key, &mangled_name, type_table),
            "`{mangled_name}` instantiates a generic block where a written impl answers"
        );
        // The module is part of an instance's identity: `&List<T>`'s and
        // `&Array<T>`'s impls of one trait mangle alike under the `&` head.
        let template = key
            .template
            .clone()
            .expect("a function instance names its template");
        let home = (key.module_source.clone(), mangled_name.clone());
        if let Some(prior) = self.functions.instantiated_homes.get(&home) {
            assert_eq!(
                *prior, template,
                "two templates instantiate `{mangled_name}` in `{}`",
                key.module_source
            );
            self.functions.instantiated.insert(key, mangled_name);
            return false;
        }
        self.functions.instantiated_homes.insert(home, template);
        self.functions
            .instantiated
            .insert(key.clone(), mangled_name);
        self.functions.pending.push(key);
        true
    }

    /// Index every queued instance under its canonical key as well, so a site
    /// asking with a `GenericInstance` and one with its `Struct` reach one body.
    pub fn alias_canonical_keys(&mut self, type_table: &mut TypeTable) {
        let entries: Vec<(InstantiationKey, String)> = self
            .functions
            .instantiated
            .iter()
            .map(|(key, name)| (key.clone(), name.clone()))
            .collect();
        for (key, name) in entries {
            let impl_type_args = key
                .impl_type_args
                .iter()
                .map(|&t| self.rewrite_type_id(t, type_table))
                .collect();
            let method_type_args = key
                .method_type_args
                .iter()
                .map(|&t| self.rewrite_type_id(t, type_table))
                .collect();
            let canonical = InstantiationKey {
                impl_type_args,
                method_type_args,
                ..key
            };
            self.functions.instantiated.entry(canonical).or_insert(name);
        }
    }

    /// Generate a monomorphized struct name: `Box` + `[i32]` → `"Box<i32>"`.
    /// Mangles through `mangle_type_arg_for_generic`, so a named type argument
    /// carries its declaring module — the same form the type-table side produces.
    /// Qualified on one side and not the other produced two wasm-GC types for one
    /// struct, and an "expected (ref $type), found (ref $type)" ICE.
    pub fn instantiation_name(&self, key: &InstantiationKey, type_table: &TypeTable) -> String {
        let args: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|&t| type_table.mangle_type_arg_for_generic(t))
            .collect();
        // The declaration's *rendered* head, which is what every reader of
        // this instantiation spells — a function-local generic struct carries
        // its disambiguator, so two sibling functions' `struct Pair<A, B>` do
        // not register as one wasm-GC type.
        let head = key
            .def
            .map_or_else(|| key.name.clone(), |def| type_table.decl_render_name(def));
        mangle_generic_name(&head, &args)
    }

    /// Generate instantiated function name: `identity` + `[i32]` -> `"identity<i32>"`
    pub fn function_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
    ) -> String {
        // For free functions, all type args are method-level.
        // For fallback from method_instantiation_name_inner (no method_info),
        // combine both for backwards-compatible naming.
        let mut args: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|t| type_table.mangle_type_arg_for_generic(*t))
            .collect();
        args.extend(
            key.method_type_args
                .iter()
                .map(|t| type_table.mangle_type_arg_for_generic(*t)),
        );
        mangle_generic_name(&key.name, &args)
    }

    /// Generate instantiated method name
    /// Format: `StructWithImplArgs::methodWithMethodArgs`
    /// e.g., `Container::transform` with `[i32, i64]` -> `"Container<i32>::transform<i64>"`
    pub fn method_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
    ) -> String {
        self.method_instantiation_name_inner(key, type_table, &[])
    }

    pub fn method_instantiation_name_inner(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params: &[TirTypeParam],
    ) -> String {
        // Use method_info metadata instead of parsing key.name
        let Some(ref method_info) = key.method_info else {
            // Fallback to regular function naming if no method_info
            return self.function_instantiation_name(key, type_table);
        };

        let impl_arg_names: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|t| type_table.mangle_type_arg_for_generic(*t))
            .collect();

        // A blanket impl's receiver IS one of its type params (e.g. "I").
        let is_blanket = method_info
            .receiver()
            .is_declared_binder_of(impl_type_params.iter().map(|p| p.name.as_str()));

        let receiver = if is_blanket && !impl_arg_names.is_empty() {
            // Replace struct name entirely: "I" → "StrCharIter"
            impl_arg_names[0].clone()
        } else if impl_arg_names.is_empty() {
            method_info.struct_name()
        } else {
            // Normal: append type args: "List" → "List<i32>"
            method_info.receiver().mangle(&impl_arg_names)
        };
        let mangled_struct =
            MethodName::format_struct_with_trait(&receiver, method_info.trait_name.as_ref());

        // Build method name: transform<i64> (using method type args)
        let method_arg_names: Vec<String> = key
            .method_type_args
            .iter()
            .map(|t| type_table.mangle_type_arg_for_generic(*t))
            .collect();
        let mangled_method =
            MethodName::format_method_with_args(&method_info.method_name, &method_arg_names);

        MethodName::join_struct_method(&mangled_struct, &mangled_method)
    }

    /// The newtype's own name when `type_id` peels to one that answers this call
    /// with its *own* impl, else `None`. Unlike
    /// [`Self::get_struct_name_from_type`], which peels newtypes transparently,
    /// this preserves the identity where the newtype overrides the method, so the
    /// collect path queues `ByteList^Trait::method` to match the rewrite.
    pub fn newtype_own_struct_name_with_impl(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        method_name: &str,
        trait_name: Option<&FqTraitName>,
    ) -> Option<FqTypeName> {
        type_table
            .newtype_link_owning(type_id, |tid| match trait_name {
                Some(trait_name) => self
                    .functions
                    .trait_env
                    .trait_def_of_fq(trait_name)
                    .is_some_and(|trait_| self.has_own_trait_impl(type_table, tid, trait_)),
                None => self.functions.trait_env.has_inherent_method_by_receiver(
                    &type_table.impl_receiver_key(tid),
                    method_name,
                ),
            })
            // The head an `impl` header writes: the declaration, with any
            // arguments left beside it rather than fused in.
            .map(|tid| type_table.fq_base_type_name(tid))
    }

    /// Whether the declaration `tid` names carries its own `impl <trait> for`
    /// block reaching `tid`.
    pub(super) fn has_own_trait_impl(
        &self,
        type_table: &TypeTable,
        tid: TypeId,
        trait_: DefId,
    ) -> bool {
        written_impl_reaches(&self.functions.trait_env, trait_, tid, type_table)
    }

    /// The first newtype link at or below `type_id` writing its own impl of
    /// `trait_`.
    pub(super) fn newtype_link_with_trait_impl(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        trait_: DefId,
    ) -> Option<TypeId> {
        type_table.newtype_link_owning(type_id, |tid| {
            self.has_own_trait_impl(type_table, tid, trait_)
        })
    }

    pub fn receiver_keeps_newtype_own_impl(
        &self,
        receiver_type_id: TypeId,
        type_table: &TypeTable,
        info: &LocalMethodName,
    ) -> bool {
        let own = self.newtype_own_struct_name_with_impl(
            receiver_type_id,
            type_table,
            &info.method_name,
            info.trait_name.as_ref(),
        );
        // Against the *base* receiver: `newtype_own_name` answers a declaration
        // (`MyArray`), while `struct_name` is an instantiation (`MyArray<i32>`).
        // Comparing those two never matched for a generic newtype, so the guard
        // never fired and the receiver was peeled to the base it inherits from.
        own.as_ref() == Some(&info.fq_base_struct_name())
    }

    /// A generic newtype answers under its own head for what its own impl
    /// declares, under its base for what it inherits.
    pub fn struct_info_for_method(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        method_name: &str,
        trait_name: Option<&FqTraitName>,
    ) -> Option<(String, Vec<TypeId>)> {
        let own = self
            .newtype_own_struct_name_with_impl(type_id, type_table, method_name, trait_name)
            .is_some();
        self.struct_info(type_id, type_table, own)
    }

    /// Without `own_newtype` every newtype level is transparent.
    fn struct_info(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        own_newtype: bool,
    ) -> Option<(String, Vec<TypeId>)> {
        // Generic containers share their dispatch name with the call sites.
        if let Some(info) = type_table.generic_dispatch_components(type_id) {
            return Some(info);
        }
        match type_table.get(type_id) {
            ResolvedType::Struct { def, type_args } => {
                Some((type_table.struct_head_name(*def), type_args.clone()))
            }
            ResolvedType::Newtype { def, type_args, .. }
                if own_newtype && !type_args.is_empty() =>
            {
                Some((type_table.decl_render_name(*def), type_args.clone()))
            }
            ResolvedType::Newtype { base_type, .. } => {
                self.struct_info(*base_type, type_table, own_newtype)
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.struct_info(*inner, type_table, own_newtype)
            }
            _ => None,
        }
    }
}
