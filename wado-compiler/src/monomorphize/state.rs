//! Monomorphizer state: instantiation tracking and name generation.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::elaborator::trait_env::{ImplReceiver, ReceiverCandidate, TraitEnv};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, RefKind, mangle_generic_name};
use crate::tir::{InstantiationKey, ResolvedType, TirFunction, TypeId, TypeTable};

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
    /// Reverse lookup: mangled struct name -> `InstantiationKey`
    pub mangled_to_key: IndexMap<String, InstantiationKey>,
}

/// Tracks function monomorphization state
pub(super) struct FuncInstState {
    /// Map from canonicalised `InstantiationKey` to the mangled function
    /// name. Keys are canonicalised by [`Monomorphizer::canonicalize_key`]
    /// before insert/lookup so that pre-/post-substitution `TypeId`
    /// variants of the same logical type collapse onto a single entry.
    pub instantiated: IndexMap<InstantiationKey, String>,
    /// The mangled names present in [`Self::instantiated`], for O(1)
    /// name-membership during blanket dedup (`instantiated` is grow-only, so
    /// this stays a faithful mirror of its value set).
    pub instantiated_names: IndexSet<String>,
    /// Work queue of pending function instantiations. Holds canonicalised
    /// keys.
    pub pending: Vec<InstantiationKey>,
    /// Project-wide trait knowledge inherited from the package. Used by
    /// receiver-substitution and comparison-lowering paths to find the
    /// module that owns `impl <trait> for <type>` without rebuilding a
    /// parallel "mangled name → module" index.
    pub trait_env: Arc<TraitEnv>,
    /// Every generic function template in the project, keyed as
    /// `collect_function_instantiation_sites` keys them. Read-only for the
    /// whole run and shared rather than cloned.
    ///
    /// A template is registered only when it declares type params, so absence
    /// is the answer "this callee is not generic" — which is what the
    /// post-variadic-expansion type-arg inference needs in order to tell a
    /// method type param from an ordinary parameter.
    pub templates: Rc<IndexMap<(ModuleSource, String), Rc<RefCell<TirFunction>>>>,
    /// Per module, the names written impls already define. An impl for one
    /// instantiation (`impl Tag for Box_<i32>`) emits exactly the function a
    /// template instantiation would, which is what lets the specific impl win
    /// over the general one — coherence Rule 1 (WEP 2026-03-14 §5).
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
        let trait_name = info.base_trait_name()?;
        if let Some(m) = self.trait_env.concrete_impl_module_for(
            ImplReceiver::Instantiated(&info.mangled_struct_name()),
            trait_name,
            type_module,
        ) {
            return Some(m.clone());
        }
        // Then the receiver identity. Not a same-spelling retry: the
        // instantiated form above asks the mangled namespace only, while an
        // identity is answered from whichever namespace holds it — a receiver
        // that never carried its declaring module is reachable only here.
        if let Some(m) = self.trait_env.concrete_impl_module_for(
            ImplReceiver::Of(info.receiver()),
            trait_name,
            type_module,
        ) {
            return Some(m.clone());
        }
        None
    }

    /// `true` when `info` denotes a trait method whose impl is already
    /// known to the project as a concrete (non-generic) impl block. The
    /// existence check used by mono's blanket-vs-concrete branch needs to
    /// match the legacy `trait_method_locations.contains_key` semantics,
    /// which only catalogued non-generic impl methods.
    pub fn has_impl(&self, info: &LocalMethodName) -> bool {
        // Existence-only check; any candidate module suffices so no hint is
        // needed.
        self.impl_module(info, None).is_some()
    }

    /// Module of any non-blanket `impl <trait> for <Type>` block — broader
    /// than [`Self::impl_module`] in that it also returns generic impls
    /// (`impl<T> IntoIterator for List<T>`) which live in the receiver
    /// type's own module by convention. Used by the type-param dispatch
    /// path to distinguish "the receiver type has a non-blanket impl,
    /// fall through to the receiver's module" from "no impl at all, use
    /// the blanket's module".
    pub fn generic_or_concrete_impl_module(
        &self,
        info: &LocalMethodName,
        type_module: Option<&ModuleSource>,
    ) -> Option<ModuleSource> {
        let trait_name = info.base_trait_name()?;
        if let Some(m) = self.trait_env.impl_module_for(
            ImplReceiver::Instantiated(&info.mangled_struct_name()),
            trait_name,
            type_module,
        ) {
            return Some(m.clone());
        }
        if let Some(m) = self.trait_env.impl_module_for(
            ImplReceiver::Of(info.receiver()),
            trait_name,
            type_module,
        ) {
            return Some(m.clone());
        }
        None
    }
}

/// Monomorphizer collects generic instantiations and generates concrete types.
///
/// Per issue #1110 (4): there is no "current module" notion at this layer.
/// Every `FunctionRef::module_source` is set by its producer to the body's
/// home module, and the monomorphizer reads that field directly — never the
/// monomorphizer's own location — to key into `generic_functions` /
/// `instantiated`. Keeping a `current_module_source` here would invite the
/// "fall back to the current module" pattern the issue forbids.
pub(super) struct Monomorphizer {
    pub structs: StructInstState,
    pub functions: FuncInstState,
    /// Number of impl-level type params in the function currently being instantiated.
    /// Set by `instantiate_function` before calling `substitute_types_in_block`.
    /// Used to distinguish impl-level (struct) type params from method-level type params
    /// in the substitution map when rewriting static method calls.
    pub current_impl_type_param_count: usize,
    /// Base struct name of the impl block being instantiated (e.g., `TreeMap` for
    /// `impl<K,V> TreeMap<K,V>`), or `None` when the current function is not an
    /// impl method. Used to restrict impl type arg propagation to calls on the
    /// same struct — calls to other structs within the same impl block must not
    /// receive these type args.
    pub current_impl_struct_name: Option<String>,
    /// Maps each type-parameter *name* of the function currently being
    /// instantiated to its key in the substitution map (impl-level params use
    /// their own index; method-level params are offset past the impl params).
    /// Set by `instantiate_function`. A type-param-receiver static call
    /// (`T^Trait::method`) resolves its concrete receiver by *name* through
    /// this map — the receiver is the param named `base_struct_name`, not
    /// positionally the lowest-index param (which breaks for `fn f<U, T: Tr>`).
    pub current_param_substitution_key: IndexMap<String, u32>,
    /// Pack index → the pack's own tuple, while a variadic for-of body is
    /// substituted for one unrolled element.
    ///
    /// That substitution binds the pack to the element it walks, which is what
    /// a bare `T` in the body means. A tuple *spelling* the pack (`[..T]` — the
    /// type of a local declared outside the loop) still means the whole tuple,
    /// so the splice reads this instead. Empty outside the expansion.
    pub pack_splice_bindings: RefCell<IndexMap<u32, TypeId>>,
    /// Template local slots already claimed by an unrolled copy in the function
    /// being instantiated.
    ///
    /// Every unroll — a variadic for-of iteration, a comprehension element —
    /// clones the same template body, so its locals collide with every other
    /// copy's. The first copy keeps the template slot and the rest reallocate;
    /// the claims must be shared, because an inner unroll nested in an outer
    /// one competes for the very same slots. Cleared per instantiation.
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
                mangled_to_key: IndexMap::default(),
            },
            functions: FuncInstState {
                instantiated: IndexMap::default(),
                instantiated_names: IndexSet::default(),
                pending: Vec::new(),
                trait_env,
                templates: Rc::new(IndexMap::default()),
                concrete_names: IndexMap::default(),
            },
            current_impl_type_param_count: 0,
            current_impl_struct_name: None,
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
        self.structs
            .instantiated
            .insert(key.clone(), mangled_name.clone());
        self.structs
            .mangled_to_key
            .insert(mangled_name, key.clone());
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

    /// Queue a function instantiation unless already queued, deduping on the full
    /// `InstantiationKey` alone. With faithful Ref/MutRef mangling and
    /// `TypeRewriter`'s post-monomorphisation rewrite of every reachable
    /// `TypeId`, every site arrives with canonicalised args — which makes
    /// `function_id_for` injective, as DCE's position-based retain asserts.
    pub fn try_queue_function(
        &mut self,
        key: InstantiationKey,
        mangled_name: String,
        type_table: &TypeTable,
    ) -> bool {
        if self.functions.instantiated.contains_key(&key) {
            return false;
        }
        if self.concrete_impl_owns_name(&key, &mangled_name, type_table) {
            return false;
        }
        // A blanket instance reaches one function from two dispatch sites whose
        // derived args can be distinct-but-equivalent `TypeId`s, so the keys
        // differ while the mangled name matches; dedup on the name, either body
        // being complete. Only a *universal* `&T` blanket qualifies for the ref
        // case, or a newtype-peeled `&^Trait` shape impl would dedup wrongly.
        let is_ref_universal_blanket = key.impl_type_args.len() == 1
            && key.method_info.as_ref().is_some_and(|i| {
                i.ref_receiver().is_some_and(|ref_kind| {
                    i.trait_decl().is_some_and(|trait_| {
                        self.functions
                            .trait_env
                            .has_universal_ref_blanket(trait_, ref_kind == RefKind::Mut)
                    })
                })
            });
        let is_blanket_key = key.impl_type_args.len() == 2 || is_ref_universal_blanket;
        // A deduped blanket key is intentionally dropped without an
        // `instantiated` entry: a call site that re-derives it misses the
        // literal lookup and resolves through
        // `lookup_instantiation_with_trait_fallback`'s blanket-module fallback,
        // which finds the single queued instance under the blanket's home
        // module. `instantiated_names` gives this membership test O(1) instead
        // of scanning every queued name.
        if is_blanket_key && self.functions.instantiated_names.contains(&mangled_name) {
            return false;
        }
        self.functions
            .instantiated_names
            .insert(mangled_name.clone());
        self.functions
            .instantiated
            .insert(key.clone(), mangled_name);
        self.functions.pending.push(key);
        true
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
        impl_type_params: &[crate::tir::TirTypeParam],
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

        // Blanket impl: struct name IS the type param (e.g., "I").
        // Detected by checking if base_struct_name matches an impl type param name.
        let is_blanket = impl_type_params
            .iter()
            .any(|p| p.name == method_info.base_struct_name());

        let mangled_struct = if is_blanket && !impl_arg_names.is_empty() {
            // Replace struct name entirely: "I" → "StrCharIter"
            MethodName::format_struct_with_args(
                &impl_arg_names[0],
                None,
                &[],
                method_info.trait_name.as_ref(),
            )
        } else {
            // Normal: append type args: "List" → "List<i32>"
            MethodName::format_struct_with_args(
                &method_info.struct_name(),
                method_info.receiver().ref_kind(),
                &impl_arg_names,
                method_info.trait_name.as_ref(),
            )
        };

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

    /// Get the struct name from a `type_id`, unwrapping references if needed
    /// For generic instances, returns the mangled name with type args (e.g., "List<i32>")
    pub fn get_struct_name_from_type(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<String> {
        match type_table.get(type_id) {
            // The identity a method name is built from, so the rendered
            // spelling: `Slice<u8>::internal_repr`, not `Slice`.
            ResolvedType::Struct { def, type_args } => {
                Some(type_table.struct_rendered_name(*def, type_args))
            }
            ResolvedType::Enum { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Flags { def } => Some(type_table.def_name(*def).to_string()),
            ResolvedType::Primitive(prim) => Some(prim.as_str().to_string()),
            // `()` names its impls under the same spelling the source writes
            // (`impl Trait for ()`), so a unit receiver finds them like a
            // primitive does.
            ResolvedType::Unit => Some(TypeTable::UNIT_TYPE_NAME.to_string()),
            ResolvedType::GenericInstance { def, type_args } => {
                // Return the mangled name with type args (e.g., "List<i32>", "Box<String>")
                let args: Vec<String> = type_args
                    .iter()
                    .map(|arg| type_table.mangle_type_arg_for_generic(*arg))
                    .collect();
                Some(mangle_generic_name(type_table.def_name(*def), &args))
            }
            ResolvedType::BuiltinArray(elem) => {
                let arg = type_table.mangle_type_name(*elem);
                Some(mangle_generic_name(TypeTable::ARRAY_TYPE_NAME, &[arg]))
            }
            // Newtypes are transparent for method lookup — unwrap to base type,
            // same as Ref/MutRef. The elaborator already resolves methods through
            // newtypes, so the monomorphizer needs to see the base type to find
            // the correct generic function template.
            ResolvedType::Newtype { base_type, .. } => {
                self.get_struct_name_from_type(*base_type, type_table)
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_name_from_type(*inner, type_table)
            }
            _ => None,
        }
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
        trait_name: Option<&crate::name::FqTraitName>,
    ) -> Option<FqTypeName> {
        self.newtype_own_name(type_id, type_table, |_, tid| match trait_name {
            Some(trait_name) => self
                .functions
                .trait_env
                .trait_def_of_fq(trait_name)
                .is_some_and(|trait_| self.has_own_trait_impl(type_table, tid, trait_)),
            None => self
                .functions
                .trait_env
                .has_inherent_method_by_receiver(&type_table.impl_receiver_key(tid), method_name),
        })
    }

    /// Whether the declaration `tid` names carries its own `impl <trait> for`
    /// block. The impl index keys the head as source writes it, so the query
    /// goes through [`TypeTable::impl_receiver_key`] rather than a mangled
    /// name — which would carry the declaring module the index never stores.
    pub(super) fn has_own_trait_impl(
        &self,
        type_table: &TypeTable,
        tid: TypeId,
        trait_: crate::defs::DefId,
    ) -> bool {
        self.functions
            .trait_env
            .has_any_methodful_impl_by_receiver(&type_table.impl_receiver_key(tid), trait_)
    }

    /// Peel refs/newtypes to the first newtype level satisfying `has_own_impl`
    /// (evaluated on that level's name and its `TypeId`), returning that name.
    ///
    /// Reads the unerased view: erasure redirects a newtype id to its base
    /// before monomorphize, so the erased view never reports a `Newtype` level
    /// at all and every newtype would look like one without its own impl.
    fn newtype_own_name(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        has_own_impl: impl Fn(&FqTypeName, TypeId) -> bool,
    ) -> Option<FqTypeName> {
        let mut tid = type_id;
        loop {
            match type_table.get_unerased(tid) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => tid = *inner,
                ResolvedType::Newtype { base_type, def, .. } => {
                    let base = *base_type;
                    // The head an `impl` header writes: the declaration, with
                    // any arguments left beside it rather than fused in.
                    let own = FqTypeName::declared(type_table.defs(), *def);
                    if has_own_impl(&own, tid) {
                        return Some(own);
                    }
                    tid = base;
                }
                _ => return None,
            }
        }
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

    /// The ordered `(mangled_method_name, trait_name)` formats to probe when
    /// resolving a generic method call, a newtype's own impl before its base so
    /// resolution lands on `ByteList^serialize` rather than `List^serialize`.
    /// Each candidate name yields both the inherent and trait-qualified forms.
    /// Shared by the collect and rewrite paths, keeping them in lockstep.
    pub fn newtype_aware_method_names(
        &self,
        receiver_type_id: TypeId,
        type_table: &TypeTable,
        method_name: &str,
        trait_name: Option<&crate::name::FqTraitName>,
    ) -> (
        Option<String>,
        Vec<(String, Option<crate::name::FqTraitName>)>,
    ) {
        let own_name = self.newtype_own_struct_name_with_impl(
            receiver_type_id,
            type_table,
            method_name,
            trait_name,
        );
        let mut names: Vec<(String, Option<crate::name::FqTraitName>)> = Vec::new();
        let mut push_for = |s: FqTypeName| {
            names.push((MethodName::format_local(&s, None, method_name), None));
            if let Some(tn) = trait_name {
                names.push((
                    MethodName::format_local(&s, Some(tn), method_name),
                    Some(tn.clone()),
                ));
            }
        };
        if let Some(own) = own_name.clone() {
            push_for(own);
        }
        // The key's `impl_type_args` are empty here — the instantiation is
        // spelled into the name, so the receiver keeps its type arguments.
        push_for(super::dispatch_receiver_name(type_table, receiver_type_id));
        (own_name.map(|n| n.to_mangled()), names)
    }

    /// Build the candidate struct-name set for trait-fallback template lookup,
    /// ordered newtype-own first, then the method's base/impl struct names, then
    /// the receiver's struct name. Mirrors the ordering of the name list from
    /// [`Self::newtype_aware_method_names`].
    pub fn newtype_aware_candidates<'a>(
        &self,
        own_name: Option<&'a str>,
        info: Option<&'a LocalMethodName>,
        struct_name: &'a str,
    ) -> Vec<ReceiverCandidate> {
        let mangled = |s: &str| ReceiverCandidate::Instantiated(crate::name::MangledName::new(s));
        let mut c: Vec<ReceiverCandidate> = Vec::new();
        // `own_name` is `FqTypeName::to_mangled`, so it carries its module.
        if let Some(own) = own_name {
            c.push(mangled(own));
        }
        if let Some(info) = info {
            c.push(ReceiverCandidate::Of(info.receiver.clone()));
            c.push(mangled(&info.struct_name()));
        }
        // `struct_name` is `get_struct_name_from_type`'s rendered spelling —
        // the declaration's own name, with no module. Asking the mangled map
        // for it reaches nothing, because every key there is module-qualified.
        c.push(ReceiverCandidate::Declared(crate::name::DeclName::new(
            struct_name,
        )));
        c
    }

    /// A generic newtype answers under its own head for what its own impl
    /// declares, under its base for what it inherits.
    pub fn struct_info_for_method(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        method_name: &str,
        trait_name: Option<&crate::name::FqTraitName>,
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
