//! Canonical declaration signatures and the one way to instantiate them.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{TypeId, TypeTable};

use super::sem::decls::FunctionSig;

/// What an `impl` block's `const NAME: T = expr;` declares.
#[derive(Debug, Clone)]
pub(crate) struct AssocConstSig {
    /// The impl-declaring module; reify walks [`Self::value`] under it.
    pub(crate) module: ModuleSource,
    pub(crate) ty: TypeId,
    pub(crate) value: crate::ast::Expr,
    /// The declared rung; `None` on a trait impl's constant.
    pub(crate) inherent_visibility: Option<crate::ast::Visibility>,
}

/// Program-wide declaration facts, resolved once by the decl pass and read-only
/// afterwards (WEP 2026-05-26). One entry per source declaration, holding what
/// it *says*, never anything computed from a use site. AST survives inside an
/// entry only where the value is irreducibly AST — parameter defaults,
/// associated-const values, `__DATA__`. Assembled from `ModuleDecls` digests.
#[derive(Default)]
pub(crate) struct Signatures {
    /// Canonical free-function signatures, keyed by the declaration. The
    /// entries are shared with the per-module digests they are assembled from
    /// rather than copied — a signature carries its parameter defaults' AST.
    pub(crate) function_sigs: IndexMap<crate::defs::DefId, Rc<FunctionSig>>,

    /// Canonical method signatures, keyed by the method's [`crate::defs::DefId`]
    /// — `impl`-block methods and `interface` / `resource` operations alike.
    /// Dispatch goes index → signature, never AST.
    pub(crate) method_sigs: IndexMap<crate::defs::DefId, MethodSig>,

    /// The name-keyed index over [`Self::method_sigs`] for `interface` /
    /// `resource` operations, which callers reach by name, not by node.
    pub(crate) resource_method_ids: IndexMap<(crate::defs::DefId, String), crate::defs::DefId>,

    /// Per-`impl`-block facts shared by the block's methods, keyed by the
    /// block's [`crate::defs::DefId`].
    pub(crate) impl_sigs: IndexMap<crate::defs::DefId, ImplSig>,

    /// Per-`trait`-declaration facts, keyed by the declaration's
    /// [`crate::defs::DefId`], so a query reaches a trait's methods without
    /// loading the declaring module's AST.
    pub(crate) trait_sigs: IndexMap<crate::defs::DefId, TraitSig>,

    /// Global-variable declarations, declaring module → name →
    /// `(declared type, is_mut)`.
    pub(crate) globals: IndexMap<ModuleSource, IndexMap<String, (TypeId, bool)>>,

    /// Impl-associated constants, keyed by `(owning type declaration,
    /// constant name)`. The owner is an identity, so two modules' same-named
    /// types cannot share an entry.
    pub(crate) associated_constants: IndexMap<(crate::defs::DefId, String), AssocConstSig>,

    /// Per-module `__DATA__` section contents; modules without one have no
    /// entry.
    pub(crate) data_sections: IndexMap<ModuleSource, String>,
}

impl Signatures {
    /// Canonical signature of the free function `def` declares.
    pub(crate) fn function_sig(&self, def: crate::defs::DefId) -> Option<&FunctionSig> {
        self.function_sigs.get(&def).map(Rc::as_ref)
    }

    /// Canonical signature of the method `def` declares.
    pub(crate) fn method_sig(&self, def: crate::defs::DefId) -> Option<&MethodSig> {
        self.method_sigs.get(&def)
    }

    /// Canonical signature of the operation `name` on the `interface` /
    /// `resource` declaration `decl`.
    pub(crate) fn resource_method_sig(
        &self,
        decl: crate::defs::DefId,
        name: &str,
    ) -> Option<&MethodSig> {
        let method = self.resource_method_ids.get(&(decl, name.to_string()))?;
        self.method_sig(*method)
    }

    /// Declaration facts of the `impl` block `def`.
    pub(crate) fn impl_sig(&self, def: crate::defs::DefId) -> Option<&ImplSig> {
        self.impl_sigs.get(&def)
    }

    /// Declaration facts of the `trait` `def` declares.
    pub(crate) fn trait_sig(&self, def: crate::defs::DefId) -> Option<&TraitSig> {
        self.trait_sigs.get(&def)
    }

    /// Declared type and mutability of the global `name` in `module`.
    pub(crate) fn global(&self, module: &ModuleSource, name: &str) -> Option<(TypeId, bool)> {
        self.globals.get(module)?.get(name).copied()
    }

    /// The constant `name` declared on the type `owner`.
    pub(crate) fn associated_constant(
        &self,
        owner: crate::defs::DefId,
        name: &str,
    ) -> Option<&AssocConstSig> {
        self.associated_constants.get(&(owner, name.to_string()))
    }

    /// The `__DATA__` section of `module`, if it declares one.
    pub(crate) fn data_section(&self, module: &ModuleSource) -> Option<&str> {
        self.data_sections.get(module).map(String::as_str)
    }

    /// Give every trait-`impl` method the parameter defaults its trait declares.
    /// They are copied by position, which is what a call site fills when it
    /// omits a trailing argument, so an impl may rename what it takes. Filled
    /// here rather than in the decl pass: the trait may be declared elsewhere.
    pub(crate) fn inherit_trait_param_defaults(&mut self, defs: &crate::defs::DefTable) {
        let inherited: Vec<(crate::defs::DefId, Vec<Option<crate::ast::Expr>>)> = self
            .method_sigs
            .values()
            .filter(|sig| !sig.params.is_empty())
            .filter_map(|sig| {
                let trait_decl = self.impl_sig(sig.declaring_impl?)?.trait_decl?;
                let declared = self.trait_sig(trait_decl)?.method(defs.name(sig.def))?;
                Some((sig.def, Param::defaults(&declared.sig.params)))
            })
            .collect();
        for (def, defaults) in inherited {
            let sig = self
                .method_sigs
                .get_mut(&def)
                .expect("every key was just read from this map");
            for (param, default) in sig.params.iter_mut().zip(defaults) {
                param.default = default;
            }
        }
    }
}

/// A declaration's parameter and return types, resolved once in its declaring
/// frame and abstract over the positional slots in [`Self::type_params`]. Slot
/// `i` is a `TypeParam` of index `i`, filled positionally; effect parameters and
/// `<F: fn(…)>` bounds are scope entries, not substitution targets, so the list
/// stays dense. Read through [`Self::instantiate`] — except by inference.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeclSig {
    pub(crate) type_params: Vec<(String, TypeId)>,
    pub(crate) param_types: Vec<TypeId>,
    /// `None` when the declaration declares no return type.
    pub(crate) return_type: Option<TypeId>,
}

/// A method's canonical signature: the frame, plus what dispatch needs
/// about the method that is not a type. Those extras are not part of
/// [`DeclSig`] because nothing substitutes into them.
///
/// One record for both kinds of method declaration — an `impl` block's, in
/// the block's frame, and an `interface` / `resource` declaration's, in the
/// declaration's — because dispatch asks them the same questions.
#[derive(Clone, Debug)]
pub(crate) struct MethodSig {
    /// The declaration — the key this signature is filed under, carried
    /// inside so a consumer holding the signature holds the identity too.
    /// A use→def edge is recorded from here, never from a name re-scan.
    pub(crate) def: crate::defs::DefId,
    pub(crate) decl: DeclSig,
    pub(crate) self_kind: crate::ast::SelfKind,
    /// The non-receiver parameters, in order. `decl.param_types` includes
    /// the receiver at index 0 when there is one, so these are offset by
    /// [`Self::first_value_param`].
    pub(crate) params: Vec<Param>,
    /// How many leading entries of `decl.type_params` the *declaration*
    /// contributes — the `impl` block's, or the `interface` / `resource`
    /// declaration's. The method's own follow. A call site numbers the two
    /// separately (`Type<A>::method<B>()`), so it needs the split; nothing
    /// else does, because a slot carries its own index.
    pub(crate) declaring_slot_count: u32,
    /// The `impl` block that declares this method, where one does. How a caller
    /// reaches [`ImplSig::spelled_slots`], which aligns a spelled turbofish
    /// with the block's slots.
    pub(crate) declaring_impl: Option<crate::defs::DefId>,
    /// The method's own slots as the declaration wrote them, parallel to
    /// [`Self::own_type_params`]. Bounds and defaults are irreducibly AST and
    /// live nowhere else, and a use site needs them to enforce the one and
    /// fill the other.
    ///
    /// Carried rather than re-found by name. A name scan cannot tell which
    /// declaration dispatch actually chose, so it could answer with an
    /// unrelated trait's same-named method — and did.
    pub(crate) own_params: Vec<crate::ast::GenericParam>,
    /// Canonical name from `#[cm("…")]`, resolved at the declaration.
    pub(crate) cm_name: Option<String>,
    pub(crate) is_async: bool,
}

/// What a declaration says about one parameter beyond its type. One record
/// per parameter rather than a vector per attribute: callers read them
/// together, and parallel vectors can disagree in length or order.
#[derive(Clone, Debug)]
pub(crate) struct Param {
    pub(crate) name: String,
    pub(crate) is_mut: bool,
    /// Irreducibly AST — re-resolved per call site under the callee's scope
    /// (WEP 2026-04-11). An impl of a trait declares none of its own:
    /// [`Signatures::inherit_trait_param_defaults`] fills in the trait's.
    pub(crate) default: Option<crate::ast::Expr>,
}

impl Param {
    pub(crate) fn defaults(params: &[Self]) -> Vec<Option<crate::ast::Expr>> {
        params.iter().map(|p| p.default.clone()).collect()
    }

    pub(crate) fn names(params: &[Self]) -> Vec<String> {
        params.iter().map(|p| p.name.clone()).collect()
    }

    pub(crate) fn is_mut_flags(params: &[Self]) -> Vec<bool> {
        params.iter().map(|p| p.is_mut).collect()
    }

    pub(crate) fn named_defaults(params: &[Self]) -> Vec<(String, Option<crate::ast::Expr>)> {
        params
            .iter()
            .map(|p| (p.name.clone(), p.default.clone()))
            .collect()
    }
}

/// The slot-consuming subset of a declaration's type parameters, in order —
/// the AST counterpart of [`MethodSig::own_type_params`], filtered by the same
/// rule so the two stay parallel by construction.
pub(super) fn own_params_of(
    type_params: &[crate::ast::GenericParam],
) -> Vec<crate::ast::GenericParam> {
    type_params
        .iter()
        .filter(|p| p.is_real_type_param())
        .cloned()
        .collect()
}

impl MethodSig {
    /// Index of the first non-receiver parameter in `decl.param_types`.
    pub(crate) fn first_value_param(&self) -> usize {
        usize::from(self.self_kind != crate::ast::SelfKind::None)
    }

    /// The non-receiver parameter types, parallel to [`Self::params`].
    pub(crate) fn value_param_types(&self) -> Vec<TypeId> {
        self.decl.param_types[self.first_value_param()..].to_vec()
    }

    /// Where `decl.type_params` splits: the declaring block's slots before
    /// this index, the method's own after it.
    pub(crate) fn declaring_split(&self) -> usize {
        let split = self.declaring_slot_count as usize;
        assert!(
            split <= self.decl.type_params.len(),
            "declaring slots ({split}) outnumber the signature's ({})",
            self.decl.type_params.len()
        );
        split
    }

    /// The slots the declaring block contributes.
    pub(crate) fn declaring_type_params(&self) -> &[(String, TypeId)] {
        &self.decl.type_params[..self.declaring_split()]
    }

    /// The method's own slots — what a use site binds.
    ///
    /// Rebuilding these from a counted offset instead is what let
    /// `impl<T, ..F> Emit for T` number `emit<S>` at a slot its signature does
    /// not use.
    pub(crate) fn own_type_params(&self) -> &[(String, TypeId)] {
        &self.decl.type_params[self.declaring_split()..]
    }

    /// [`Self::own_type_params`] as bare ids.
    pub(crate) fn own_type_param_ids(&self) -> Vec<TypeId> {
        self.own_type_params().iter().map(|(_, id)| *id).collect()
    }

    /// Fill the declaring block's slots from `declaring_args` and the
    /// method's own from `method_args` — how a call site that spells both
    /// (`Type<A>::method<B>()`) reads the signature.
    ///
    /// Each slot is filled by its own index, not by its position in the
    /// list: a generic, `&`-target, blanket or variadic-tuple impl numbers
    /// its slots differently, and a partially-concrete target leaves gaps a
    /// positional list cannot express.
    pub(crate) fn instantiate_call(
        &self,
        type_table: &RefCell<TypeTable>,
        declaring_args: &[TypeId],
        method_args: &[TypeId],
    ) -> InstantiatedSig {
        self.instantiate_call_with(type_table, None, declaring_args, method_args)
    }

    /// [`Self::instantiate_call`] with the declaring block's own alignment.
    ///
    /// `declaring_args` are the *receiver's* type arguments, in the target
    /// type's declaration order. Where the block writes a concrete argument
    /// (`impl … for Map<String, V>`) those do not line up with its slots —
    /// position 0 is pinned and binds nothing — so [`ImplSig::spelled_slots`]
    /// reads them, and whatever it cannot align falls back to the positional
    /// zip, as does every non-impl declaration.
    pub(crate) fn instantiate_call_with(
        &self,
        type_table: &RefCell<TypeTable>,
        declaring: Option<&ImplSig>,
        declaring_args: &[TypeId],
        method_args: &[TypeId],
    ) -> InstantiatedSig {
        let aligned = declaring.and_then(|sig| sig.spelled_slots(type_table, declaring_args));
        let positional = aligned.is_none();
        let mut substitution = aligned.unwrap_or_default();
        {
            let table = type_table.borrow();
            let pairs = positional
                .then(|| self.declaring_type_params().iter().zip(declaring_args))
                .into_iter()
                .flatten()
                .chain(self.own_type_params().iter().zip(method_args));
            for ((_, slot), &arg) in pairs {
                if let crate::tir::ResolvedType::TypeParam { index, .. }
                | crate::tir::ResolvedType::TypePack { index, .. } = table.get(*slot)
                {
                    substitution.insert(*index, arg);
                }
            }
        }
        self.decl.instantiate_slots(type_table, &substitution)
    }
}

/// What a `trait` declaration says, resolved once in the trait's own frame.
///
/// The frame numbers `Self` as slot 0 and the trait's own type parameters
/// from 1, so an `impl` reads a method back by supplying its target as slot
/// 0 and its trait arguments after it.
#[derive(Clone, Debug)]
pub(crate) struct TraitSig {
    /// The declaring module, for the call sites that name the trait's owner.
    pub(crate) module: ModuleSource,
    /// The trait's methods, in declaration order, by name.
    pub(crate) methods: IndexMap<String, TraitMethod>,
}

/// One method of a `trait` declaration.
#[derive(Clone, Debug)]
pub(crate) struct TraitMethod {
    /// The signature in the trait's frame — [`TraitSig::type_params`] are its
    /// leading slots, the method's own follow.
    pub(crate) sig: MethodSig,
    /// Irreducibly AST: walked once per implementing block and reified per
    /// instantiation. `None` marks a required method.
    pub(crate) default_body: Option<Rc<crate::ast::Function>>,
}

impl TraitSig {
    /// The method named `name`, if the trait declares one.
    pub(crate) fn method(&self, name: &str) -> Option<&TraitMethod> {
        self.methods.get(name)
    }

    /// The methods this trait provides a default body for, in declaration
    /// order.
    pub(crate) fn default_methods(
        &self,
    ) -> impl Iterator<Item = (&str, &Rc<crate::ast::Function>)> {
        self.methods.iter().filter_map(|(name, method)| {
            method
                .default_body
                .as_ref()
                .map(|body| (name.as_str(), body))
        })
    }
}

/// One `impl` block's declaration facts, resolved once in the block's own
/// frame — its target type parameters occupying the positional slots the
/// block's methods are numbered against.
///
/// Not part of [`MethodSig`]: these are the *block's* facts, shared by every
/// method it declares, and a use site reads them without naming a method.
#[derive(Clone, Debug)]
pub(crate) struct ImplSig {
    /// The impl target's type arguments (`K`, `V` in `impl … for Map<K, V>`).
    /// A slot appears as its own `TypeParam` / `TypePack`, so aligning a
    /// receiver's arguments against this list says which slot each fills.
    /// Empty when the target is not generic.
    pub(crate) target_type_args: Vec<TypeId>,
    /// The trait reference's type arguments (`K` in `impl Index<K> for …`),
    /// resolved against the same slots. Empty for an inherent impl.
    pub(crate) trait_type_args: Vec<TypeId>,
    /// The block's `type X = …;` bindings, resolved against the same slots.
    pub(crate) associated_types: IndexMap<String, TypeId>,
    /// The target's fq name, qualified by the module that declares it — what
    /// the block's own imports make of the name it wrote. A blanket target
    /// (`impl<T> Trait for T`) is its own binder.
    pub(crate) target_fq: crate::name::FqTypeName,
    /// Which trait declaration the block implements, answered by the header's
    /// own reference site. `None` for an inherent impl.
    pub(crate) trait_decl: Option<crate::defs::DefId>,
}

impl ImplSig {
    /// Fill the block's slots with a receiver's type arguments — the one way
    /// a use site reads an [`ImplSig`], mirroring [`DeclSig::instantiate`].
    pub(crate) fn instantiate(
        &self,
        type_table: &RefCell<TypeTable>,
        receiver_args: &[TypeId],
    ) -> InstantiatedImplSig {
        self.instantiate_slots(type_table, &self.slots(type_table, receiver_args))
    }

    /// [`Self::instantiate`] from a slot map the caller already holds.
    ///
    /// [`Self::slots`] is the alignment a plain generic target implies; a
    /// blanket, `&`-target or variadic-tuple block binds its slots from the
    /// receiver differently, and that caller passes its own map here.
    pub(crate) fn instantiate_slots(
        &self,
        type_table: &RefCell<TypeTable>,
        slots: &IndexMap<u32, TypeId>,
    ) -> InstantiatedImplSig {
        let mut table = type_table.borrow_mut();
        InstantiatedImplSig {
            trait_type_args: self
                .trait_type_args
                .iter()
                .map(|&arg| table.substitute_type_params(arg, slots))
                .collect(),
            associated_types: self
                .associated_types
                .iter()
                .map(|(name, &binding)| {
                    (name.clone(), table.substitute_type_params(binding, slots))
                })
                .collect(),
        }
    }

    /// The slot substitution a receiver's type arguments imply — the one
    /// alignment, shared by [`Self::instantiate`] and by the instantiation
    /// of any [`MethodSig`] the block declares.
    ///
    /// Target position `i` binds a slot only where the impl wrote a type
    /// parameter there; a concrete argument (`u8` in `impl List<u8>`) binds
    /// nothing, which is what makes a partially-concrete target expressible.
    pub(crate) fn slots(
        &self,
        type_table: &RefCell<TypeTable>,
        receiver_args: &[TypeId],
    ) -> IndexMap<u32, TypeId> {
        let table = type_table.borrow();
        self.target_type_args
            .iter()
            .zip(receiver_args)
            .filter_map(|(&declared, &concrete)| match table.get(declared) {
                crate::tir::ResolvedType::TypeParam { index, .. }
                | crate::tir::ResolvedType::TypePack { index, .. } => Some((*index, concrete)),
                _ => None,
            })
            .collect()
    }

    /// [`Self::slots`] where `receiver_args` are a *spelled* argument list, as
    /// a turbofish writes them; `None` where this block's target cannot align
    /// with one — a blanket, `&`-target or variadic-tuple block writes no
    /// `target_type_args` and binds its slots from the receiver differently.
    pub(crate) fn spelled_slots(
        &self,
        type_table: &RefCell<TypeTable>,
        receiver_args: &[TypeId],
    ) -> Option<IndexMap<u32, TypeId>> {
        (!self.target_type_args.is_empty() && self.target_type_args.len() == receiver_args.len())
            .then(|| self.slots(type_table, receiver_args))
    }
}

/// An [`ImplSig`] with its slots filled by a receiver's type arguments.
#[derive(Clone, Debug)]
pub(crate) struct InstantiatedImplSig {
    pub(crate) trait_type_args: Vec<TypeId>,
    pub(crate) associated_types: IndexMap<String, TypeId>,
}

/// A [`DeclSig`] with its slots filled by a use site's type arguments.
#[derive(Clone, Debug)]
pub(crate) struct InstantiatedSig {
    pub(crate) param_types: Vec<TypeId>,
    pub(crate) return_type: TypeId,
}

impl DeclSig {
    /// Fill the signature's slots with `type_args`, positionally.
    ///
    /// Arity is the caller's contract: it owns the diagnostic for a
    /// mismatch, and passing fewer arguments than slots is how a
    /// partially-inferred call site instantiates what it knows, leaving
    /// the trailing slots abstract. Passing none substitutes nothing.
    pub(crate) fn instantiate(
        &self,
        type_table: &RefCell<TypeTable>,
        type_args: &[TypeId],
    ) -> InstantiatedSig {
        let substitution: IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &t)| (i as u32, t))
            .collect();
        self.instantiate_slots(type_table, &substitution)
    }

    /// Fill slots by index rather than by position, for a caller that already
    /// holds a slot map. Equivalent to [`Self::instantiate`] when the map is
    /// dense from zero; the two differ only when the caller knows a
    /// non-contiguous subset, which positional arguments cannot express.
    pub(crate) fn instantiate_slots(
        &self,
        type_table: &RefCell<TypeTable>,
        substitution: &IndexMap<u32, TypeId>,
    ) -> InstantiatedSig {
        self.instantiate_slots_with(
            type_table,
            substitution,
            &crate::tir::SlotProjections::default(),
        )
    }

    /// [`Self::instantiate_slots`] for a use site that also knows what the
    /// projections rooted at a slot mean. A signature written against
    /// `Self::X` is abstract over that too, and only the use site can fill it.
    pub(crate) fn instantiate_slots_with(
        &self,
        type_table: &RefCell<TypeTable>,
        substitution: &IndexMap<u32, TypeId>,
        projections: &crate::tir::SlotProjections,
    ) -> InstantiatedSig {
        let mut table = type_table.borrow_mut();
        InstantiatedSig {
            param_types: self
                .param_types
                .iter()
                .map(|&p| table.substitute_type_params_with(p, substitution, projections))
                .collect(),
            return_type: table.substitute_type_params_with(
                self.return_type.unwrap_or(TypeTable::UNIT),
                substitution,
                projections,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fn id<T>(x: T) -> T` instantiated at `T = i32`.
    fn generic_identity(table: &RefCell<TypeTable>) -> DeclSig {
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        DeclSig {
            type_params: vec![("T".to_string(), t)],
            param_types: vec![t],
            return_type: Some(t),
        }
    }

    #[test]
    fn instantiate_fills_slots_positionally() {
        let table = RefCell::new(TypeTable::new());
        let sig = generic_identity(&table);

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        assert_eq!(inst.param_types, vec![TypeTable::I32]);
        assert_eq!(inst.return_type, TypeTable::I32);
    }

    #[test]
    fn instantiate_without_args_keeps_the_canonical_form() {
        let table = RefCell::new(TypeTable::new());
        let sig = generic_identity(&table);
        let canonical = sig.param_types.clone();

        let inst = sig.instantiate(&table, &[]);

        assert_eq!(inst.param_types, canonical);
        assert_eq!(inst.return_type, canonical[0]);
    }

    #[test]
    fn instantiate_substitutes_inside_containers() {
        let table = RefCell::new(TypeTable::new());
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        let list_of_t = table.borrow_mut().make_ref(t);
        let sig = DeclSig {
            type_params: vec![("T".to_string(), t)],
            param_types: vec![list_of_t],
            return_type: None,
        };

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        let expected = table.borrow_mut().make_ref(TypeTable::I32);
        assert_eq!(inst.param_types, vec![expected]);
        assert_eq!(inst.return_type, TypeTable::UNIT);
    }

    #[test]
    fn a_partial_instantiation_leaves_trailing_slots_abstract() {
        let table = RefCell::new(TypeTable::new());
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        let u = table.borrow_mut().make_type_param("U".to_string(), 1);
        let sig = DeclSig {
            type_params: vec![("T".to_string(), t), ("U".to_string(), u)],
            param_types: vec![t, u],
            return_type: Some(u),
        };

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        assert_eq!(inst.param_types, vec![TypeTable::I32, u]);
        assert_eq!(inst.return_type, u);
    }

    #[test]
    fn a_slotless_signature_is_unchanged_by_instantiation() {
        let table = RefCell::new(TypeTable::new());
        let sig = DeclSig {
            type_params: Vec::new(),
            param_types: vec![TypeTable::I32],
            return_type: Some(TypeTable::BOOL),
        };

        let inst = sig.instantiate(&table, &[]);

        assert_eq!(inst.param_types, vec![TypeTable::I32]);
        assert_eq!(inst.return_type, TypeTable::BOOL);
    }

    /// `impl<V> Index<i32> for Map<u8, V> { type Output = V; }` — the
    /// concrete `u8` position binds nothing, so `V` keeps its own slot.
    fn partially_concrete_impl(table: &RefCell<TypeTable>) -> ImplSig {
        let v = table.borrow_mut().make_type_param("V".to_string(), 1);
        ImplSig {
            target_type_args: vec![TypeTable::U8, v],
            trait_type_args: vec![TypeTable::I32],
            associated_types: [("Output".to_string(), v)].into_iter().collect(),
            target_fq: crate::name::FqTypeName::builtin("Map"),
            trait_decl: None,
        }
    }

    #[test]
    fn only_type_parameter_positions_bind_slots() {
        let table = RefCell::new(TypeTable::new());
        let sig = partially_concrete_impl(&table);

        let slots = sig.slots(&table, &[TypeTable::U8, TypeTable::BOOL]);

        assert_eq!(slots.len(), 1);
        assert_eq!(slots.get(&1), Some(&TypeTable::BOOL));
    }

    #[test]
    fn an_associated_type_instantiates_through_its_slot() {
        let table = RefCell::new(TypeTable::new());
        let sig = partially_concrete_impl(&table);

        let inst = sig.instantiate(&table, &[TypeTable::U8, TypeTable::BOOL]);

        assert_eq!(inst.associated_types.get("Output"), Some(&TypeTable::BOOL));
        assert_eq!(inst.associated_types.get("Item"), None);
    }

    #[test]
    fn instantiate_slots_takes_the_alignment_the_caller_holds() {
        let table = RefCell::new(TypeTable::new());
        let sig = partially_concrete_impl(&table);

        let inst = sig.instantiate_slots(&table, &[(1, TypeTable::BOOL)].into_iter().collect());

        assert_eq!(inst.associated_types.get("Output"), Some(&TypeTable::BOOL));
        assert_eq!(inst.trait_type_args, vec![TypeTable::I32]);
    }

    #[test]
    fn a_concrete_trait_argument_survives_instantiation() {
        let table = RefCell::new(TypeTable::new());
        let sig = partially_concrete_impl(&table);

        let inst = sig.instantiate(&table, &[TypeTable::U8, TypeTable::BOOL]);

        assert_eq!(inst.trait_type_args, vec![TypeTable::I32]);
    }
}
