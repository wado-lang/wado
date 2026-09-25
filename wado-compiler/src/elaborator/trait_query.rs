//! Trait query functions: checking trait implementations, bounds validation,
//! and associated type resolution.

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, Type};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::defs::DefId;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, Receiver, RefKind, TypeHead};
use crate::primitive::PrimitiveType;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::callee::CalleeRef;
use super::method_lookup::ImplParamSlots;
use super::scope::{
    BinderInScope, BoundSelf, ElaboratedBound, Scope, ScopedBound, TraitCheckFrame,
    trait_params_from_impl,
};
use super::sig::Param;
use super::trait_env::{ImplMethodHeader, InheritedBound, ViaClause};
use super::type_resolution::ParamSpace;
use super::types::{
    MethodInfo, MethodOwner, ResolvedTraitMethod, TraitMethodMatch, TypeError, TypeLookup,
};
use super::tysys::TypeSystem;
use super::util::bound_param_name;
use crate::ast::{AstId, SelfKind};
use crate::elaborator::sig;
use crate::elaborator::sig::TraitSig;
use crate::elaborator::synth::{ArgClass, ArgSource, param_takes};
use crate::elaborator::trait_env::{
    BlanketBound, BlanketImpl, BlanketReceiver, ImplHeader, TraitDeclHeader, TraitEnv,
    get_type_name_static, header_answers_bound_args, written_arg_nodes, written_type_arg,
};
use crate::elaborator::types::{RequiredTrait, StructFieldInfo, VariantInfo};
use crate::name::{DeclName, FqTraitName};
use crate::resolve::{Resolution, Resolutions, head_site};
use crate::tir::{SlotProjections, TraitRef};

/// Proof that a bound was asked and answered no. Its field is private here, so
/// [`TypeError::TraitBoundNotSatisfied`] can be raised from nowhere else.
#[derive(Clone, Debug)]
pub struct BoundUnmet(());

/// Whether a bound query may follow a newtype to its base. Dispatch does; rank
/// 2 does not (`docs/wep-2026-09-01-trait-resolution.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NewtypePeel {
    Follow,
    Here,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OnBoundTrait {
    Eq,
    Ord,
    Serialize,
    Deserialize,
    WireNumbered,
    Default,
    Reflect,
    ReflectStruct,
    ReflectVariant,
    ReflectEnum,
    ReflectFlags,
    ReflectNewtype,
    ReflectTemplate,
    Ref,
    RefMut,
    Inspect,
}

impl OnBoundTrait {
    /// The compiler item this classification came from. The traits that drive
    /// synthesis are all registered items, so a request records the
    /// declaration the registry holds rather than the spelling a bound wrote.
    pub(super) fn compiler_item(self) -> CompilerItem {
        match self {
            Self::Eq => CompilerItem::Eq,
            Self::Ord => CompilerItem::Ord,
            Self::Serialize => CompilerItem::Serialize,
            Self::Deserialize => CompilerItem::Deserialize,
            Self::WireNumbered => CompilerItem::WireNumbered,
            Self::Default => CompilerItem::Default,
            Self::Reflect => CompilerItem::Reflect,
            Self::ReflectStruct => CompilerItem::ReflectStruct,
            Self::ReflectVariant => CompilerItem::ReflectVariant,
            Self::ReflectEnum => CompilerItem::ReflectEnum,
            Self::ReflectFlags => CompilerItem::ReflectFlags,
            Self::ReflectNewtype => CompilerItem::ReflectNewtype,
            Self::ReflectTemplate => CompilerItem::ReflectTemplate,
            Self::Ref => CompilerItem::Ref,
            Self::RefMut => CompilerItem::RefMut,
            Self::Inspect => CompilerItem::Inspect,
        }
    }

    /// The inverse of [`Self::compiler_item`]. `None` for an item that drives
    /// no bound-time synthesis.
    pub(super) fn of_compiler_item(item: CompilerItem) -> Option<Self> {
        let found = match item {
            CompilerItem::Eq => Self::Eq,
            CompilerItem::Ord => Self::Ord,
            CompilerItem::Serialize => Self::Serialize,
            CompilerItem::Deserialize => Self::Deserialize,
            CompilerItem::WireNumbered => Self::WireNumbered,
            CompilerItem::Default => Self::Default,
            CompilerItem::Reflect => Self::Reflect,
            CompilerItem::ReflectStruct => Self::ReflectStruct,
            CompilerItem::ReflectVariant => Self::ReflectVariant,
            CompilerItem::ReflectEnum => Self::ReflectEnum,
            CompilerItem::ReflectFlags => Self::ReflectFlags,
            CompilerItem::ReflectNewtype => Self::ReflectNewtype,
            CompilerItem::ReflectTemplate => Self::ReflectTemplate,
            CompilerItem::Ref => Self::Ref,
            CompilerItem::RefMut => Self::RefMut,
            CompilerItem::Inspect => Self::Inspect,
            _ => return None,
        };
        Some(found)
    }

    pub(super) fn is_serde(self) -> bool {
        matches!(self, Self::Serialize | Self::Deserialize)
    }

    /// Holds for every type, so the bound is satisfied before any body exists.
    /// `Display` is not: a `T: Display` bound is checked against a real impl.
    pub(super) fn is_total(self) -> bool {
        matches!(self, Self::Inspect)
    }

    pub(super) fn is_field_recursive(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ord | Self::Serialize | Self::Deserialize
        )
    }

    pub(super) fn is_reflect(self) -> bool {
        matches!(
            self,
            Self::Reflect
                | Self::ReflectStruct
                | Self::ReflectVariant
                | Self::ReflectEnum
                | Self::ReflectFlags
                | Self::ReflectNewtype
                | Self::ReflectTemplate
        )
    }
}

#[derive(Clone, Copy)]
enum StructuralMember<'a> {
    Field(&'a str),
    Case(&'a str),
}

impl StructuralMember<'_> {
    fn describe(self) -> String {
        match self {
            Self::Field(name) => format!("field `{name}`"),
            Self::Case(name) => format!("variant `{name}`"),
        }
    }
}

/// The declaration a `Type::CONST` use site qualifies its constant with, read
/// off the qualifier's own reference site. `None` for a bare name, which
/// qualifies nothing and so names no associated constant, and for a qualifier
/// that reaches no declaration. Shared by annotate and reify so both resolve a
/// constant to the same identity.
pub(super) fn assoc_const_owner(
    qualifier: Option<&ast::Type>,
    resolutions: &Resolutions,
) -> Option<DefId> {
    let site = match qualifier? {
        ast::Type::Named(t) => t.id,
        ast::Type::Generic(t) => t.id,
        ast::Type::NamespacedGeneric(t) => t.id,
        _ => return None,
    };
    match resolutions.get(site) {
        Resolution::Def(def) => Some(def),
        _ => None,
    }
}

/// [`assoc_const_owner`] for a qualified path written in expression position
/// (`f64::PI`, `ns::Config::MAX`): the segment before the constant's own name
/// is the qualifier, and the resolve walk answered for it — so the owner is
/// read off that site rather than off the fused spelling `IdentExpr::name`
/// holds.
pub(super) fn assoc_const_owner_of_path(
    ident: &ast::IdentExpr,
    resolutions: &Resolutions,
) -> Option<DefId> {
    match resolutions.get(ident.owner_segment()?.id) {
        Resolution::Def(def) => Some(def),
        _ => None,
    }
}

/// Whether `ty` spells one of the declaration's own type packs
/// (`Trait<Assoc = [..P]>`). Such a binding names a parameter to project into,
/// not an expectation to enforce — and `..P` belongs to the declaration's
/// scope, so resolving it at a use site would not find it.
fn mentions_type_pack(ty: &ast::Type) -> bool {
    ty.any(&mut |ty| matches!(ty, ast::Type::TypePackSpread(..)))
}

/// A bound as the asking site states it: its arguments are spelled in the
/// declaring item's parameter space, and become what the site wrote there.
/// `None` where one stays a binder, which belongs to the site's own caller.
fn asked_at(trait_: FqTraitName, at_call: &[(FqTypeName, FqTypeName)]) -> Option<FqTraitName> {
    let asked = at_call
        .iter()
        .fold(trait_, |trait_, (param, arg)| trait_.substitute(param, arg));
    (!asked.args_mention_binder()).then_some(asked)
}

/// What a reference points at, and whether it was a mutable one.
fn referent(tt: &TypeTable, type_id: TypeId) -> (TypeId, bool) {
    match tt.get(type_id) {
        ResolvedType::Ref(inner) => (*inner, false),
        ResolvedType::MutRef(inner) => (*inner, true),
        _ => (type_id, false),
    }
}

/// Whether a constraint written `expected` is what `actual` satisfies. It holds
/// at either level — collecting `&T` into a `List<T>` reads each element as its
/// value — but never across a mutability the two disagree on.
fn satisfies(tt: &TypeTable, expected: TypeId, actual: TypeId) -> bool {
    let (expected_referent, expected_mut) = referent(tt, expected);
    let (actual_referent, actual_mut) = referent(tt, actual);
    expected_referent == actual_referent && expected_mut == actual_mut
}

/// What a bound's `Self::Assoc` projects off at a call: the receiver, and the
/// trait whose declaration wrote the constraint.
#[derive(Clone, Copy, Debug)]
pub(super) struct SelfBinding {
    pub(super) type_id: TypeId,
    pub(super) declaring_trait: Option<DefId>,
}

impl TypeSystem {
    /// [`SelfBinding`] for a call on `receiver`, read past its references to
    /// the type an `impl` block targets.
    pub(super) fn base_self_binding(
        &self,
        receiver: TypeId,
        declaring_trait: Option<DefId>,
    ) -> SelfBinding {
        SelfBinding {
            type_id: self.get_base_type(receiver),
            declaring_trait,
        }
    }
}

/// The recorded declaration facts of the trait `decl` declares, for a caller
/// holding the inputs rather than an `Elaborator` — reify's default-method
/// pass. Reads the digest the decl pass recorded, never the declaring module's
/// AST, and answers `None` for a declaration that is no trait.
pub(crate) fn trait_sig_of_with<'a>(
    decl: DefId,
    trait_env: &TraitEnv,
    signatures: &'a sig::Signatures,
) -> Option<&'a TraitSig> {
    if !trait_env.declares_trait(&decl) {
        return None;
    }
    signatures.trait_sig(decl)
}

/// The structural-conformance rule's answer for one type: whether every member
/// satisfies the trait, and which one decided when they do not.
#[derive(Debug, PartialEq, Eq)]
enum StructuralConformance {
    /// Not a shape the rule applies to.
    NotApplicable,
    Holds,
    Fails {
        member: String,
        type_id: TypeId,
    },
}

impl TypeSystem {
    /// The traits the compiler auto-derives for eligible aggregate types
    /// (`struct` / `variant` / `enum` / generic instance) and exposes through
    /// method-call and operator dispatch, each paired with the method it
    /// declares. The single source for the auto-derive method ↔ trait mapping:
    /// the dispatch sites read it instead of hardcoding the `"eq"` / `"cmp"`
    /// strings and the per-trait return type. Adding a new auto-derived trait
    /// is one entry here (plus its synthesis in `synthesis::traits`).
    const AUTO_DERIVED_METHODS: &'static [(CompilerItem, &'static str)] =
        &[(CompilerItem::Eq, "eq"), (CompilerItem::Ord, "cmp")];

    /// The return type an auto-derived trait fixes, regardless of what any user
    /// impl writes (`Eq` → `bool`, `Ord` → `Ordering`).
    fn auto_derive_return_type(&self, item: CompilerItem) -> TypeId {
        match item {
            CompilerItem::Eq => TypeTable::BOOL,
            _ => self
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::Ordering),
        }
    }

    /// Resolve the auto-derived trait that declares `method_name`, returning its
    /// trait name and fixed return type, or `None` when no auto-derived trait
    /// declares that method.
    pub(super) fn auto_derive_by_method(
        &self,
        method_name: &str,
    ) -> Option<(CompilerItem, String, TypeId)> {
        let item = Self::AUTO_DERIVED_METHODS
            .iter()
            .find(|(_, m)| *m == method_name)
            .map(|(it, _)| *it)?;
        let trait_name = self
            .type_table
            .borrow()
            .compiler_trait_name(item)
            .to_string();
        Some((item, trait_name, self.auto_derive_return_type(item)))
    }

    /// Mirror of [`Self::auto_derive_by_method`] keyed by trait name, for
    /// operator dispatch which already knows the trait. Returns the compiler
    /// item the name matched and its fixed return type, or `None` when
    /// `trait_name` is not an auto-derived trait.
    ///
    /// Returning the item, not just the type, is what lets the caller name the
    /// trait by its declaration rather than re-deriving one from the spelling.
    pub(super) fn auto_derive_by_trait(&self, trait_name: &str) -> Option<(CompilerItem, TypeId)> {
        let item = Self::AUTO_DERIVED_METHODS.iter().find_map(|(item, _)| {
            let name = self
                .type_table
                .borrow()
                .compiler_trait_name(*item)
                .to_string();
            (name == trait_name).then_some(*item)
        })?;
        Some((item, self.auto_derive_return_type(item)))
    }

    /// Check that concrete type args at non-type-parameter positions match the impl type.
    /// e.g. `impl From<…> for TreeMap<String, V>` with `TreeMap<i32, String>` should fail
    /// because position 0 expects String but got i32.
    pub(crate) fn verify_impl_type_compatibility(
        &self,
        impl_ty: &Type,
        concrete_type_args: &[TypeId],
        declared_type_params: &IndexSet<String>,
    ) -> bool {
        if declared_type_params.is_empty() {
            return true; // No filtering available, assume compatible
        }
        let Type::Generic(g) = impl_ty else {
            return true;
        };
        let tt = self.type_table.borrow();
        for (i, arg) in g.args.iter().enumerate() {
            let Some(&concrete_id) = concrete_type_args.get(i) else {
                continue;
            };
            if !Self::impl_type_matches_concrete(arg, concrete_id, declared_type_params, &tt) {
                return false;
            }
        }
        true
    }

    /// Recursively check whether an impl type argument matches a concrete type ID.
    /// - `Type::Named` that is a declared type param → always matches (free type param)
    /// - `Type::Named` not in type params → concrete name must equal `type_table.type_name()`
    /// - `Type::Generic` → concrete must be a `GenericInstance` with same outer name; inner args checked recursively
    /// - Other types → not validated (return true)
    pub(crate) fn impl_type_matches_concrete(
        impl_ty: &Type,
        concrete_id: TypeId,
        declared_type_params: &IndexSet<String>,
        type_table: &TypeTable,
    ) -> bool {
        match impl_ty {
            Type::Named(n) => {
                if declared_type_params.contains(&n.name) {
                    true // free type param — matches anything
                } else {
                    type_table.type_name(concrete_id) == n.name
                }
            }
            Type::Generic(g) => {
                let resolved = type_table.get(concrete_id).clone();
                match resolved {
                    ResolvedType::GenericInstance { def, type_args } => {
                        if type_table.def_name(def) != g.name {
                            return false;
                        }
                        for (i, inner) in g.args.iter().enumerate() {
                            let Some(&inner_id) = type_args.get(i) else {
                                return false;
                            };
                            if !Self::impl_type_matches_concrete(
                                inner,
                                inner_id,
                                declared_type_params,
                                type_table,
                            ) {
                                return false;
                            }
                        }
                        true
                    }
                    _ => false,
                }
            }
            _ => true,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// The written head of `impl Trait for T`, its name, and the declaration it
    /// names. The head carries its own reference site, so an aliased
    /// `impl B for T` answers `Base` rather than a spelling two modules share.
    fn impl_trait_head<'i>(
        &self,
        impl_block: &'i ast::ImplBlock,
    ) -> Option<(&'i ast::Type, String, DefId)> {
        let trait_type = impl_block.trait_type.as_ref()?;
        let trait_name = self.get_type_name(trait_type);
        let decl = head_site(trait_type).and_then(|site| self.decl_key_at(site, &trait_name))?;
        Some((trait_type, trait_name, decl))
    }

    /// Enforce a trait's associated-type bounds (`type X: Bound`) against an
    /// impl's bindings (`type X = Concrete`), skipping still-parametric
    /// bindings. Only the bound's trait is checked, not its associated-type
    /// equality constraints (`Iterator<Item = Self::Item>`).
    pub(super) fn enforce_impl_assoc_type_bounds(&mut self, impl_block: &ast::ImplBlock) {
        let Some((_, _, trait_decl)) = self.impl_trait_head(impl_block) else {
            return;
        };
        for binding in &impl_block.associated_types {
            let bounds: Vec<(String, Option<FqTraitName>)> = self
                .tysys
                .trait_assoc_type_decl(&trait_decl, &binding.name)
                .into_iter()
                .flat_map(|decl| &decl.bounds)
                .filter(|bound| bound.names_a_trait())
                .map(|bound| self.tysys.bound_named_written(bound))
                .collect();
            if bounds.is_empty() {
                continue;
            }
            let type_id = self
                .annotate_ctx
                .trait_ctx
                .assoc_type_bindings
                .get(&binding.name)
                .copied()
                .unwrap_or_else(|| self.resolve_type(&binding.ty));
            if self.tysys.type_table.borrow().contains_type_param(type_id) {
                continue;
            }
            for (bound_name, bound_trait) in &bounds {
                self.enforce_single_bound_args(
                    type_id,
                    bound_name,
                    bound_trait.as_ref(),
                    &binding.name,
                    binding.span,
                );
            }
        }
    }

    /// Enforce a trait's supertraits against `impl Trait for T`. The whole
    /// closure, not just the direct ones: a supertrait satisfied structurally
    /// has no impl block of its own to carry the rest of the chain.
    pub(super) fn enforce_impl_supertraits(&mut self, impl_block: &ast::ImplBlock) {
        let Some((trait_type, trait_name, trait_decl)) = self.impl_trait_head(impl_block) else {
            return;
        };
        let self_type = self.resolve_type(&impl_block.ty);
        // The header's arguments as this impl answers them, `Self` included:
        // resolving is what turns `Self::Base` into the type the block binds,
        // and a clause inherited through another trait is answered the same.
        let arg_ids: Vec<TypeId> = written_arg_nodes(trait_type)
            .iter()
            .map(|arg| self.resolve_type(arg))
            .collect();
        let (params, closure) = self
            .tysys
            .trait_env
            .supertrait_closure_declared(&trait_decl);
        let params = params.to_vec();
        let clauses: Vec<(DefId, ast::TraitBound, Vec<ViaClause>)> = closure
            .iter()
            .map(|inherited| {
                (
                    inherited.decl,
                    inherited.bound.clone(),
                    inherited.via.clone(),
                )
            })
            .collect();
        let (_, args) = self.trait_params_at_impl(
            &params,
            &arg_ids,
            SelfBinding {
                type_id: self_type,
                declaring_trait: Some(trait_decl),
            },
        );
        for (decl, bound, via) in clauses {
            let named = self
                .tysys
                .bound_written(&bound)
                .unwrap_or_else(|| FqTraitName::declared(self.tysys.resolutions.defs(), decl));
            // Resolved in the space the clause was written in, which the chain
            // from this impl's own arguments reaches: `X::Item` under `X = Feed`
            // is what `impl Src for Feed` binds it to. A spelling carries none
            // of that.
            let space = self.inherited_space(trait_decl, &args, &via);
            let pick = vec![true; bound.type_args.len()];
            let supertrait_trait = self.in_space(&space, |e| {
                e.trait_named_with_resolved_args(named, &bound, &pick)
            });
            let supertrait = self.tysys.resolutions.defs().name(decl).to_string();
            if self.tysys.type_implements_trait(
                &self.annotate_ctx,
                &self.type_lookup(),
                self_type,
                &supertrait_trait,
            ) {
                continue;
            }
            let type_name = self.tysys.type_id_to_string(self_type);
            let reason = self.tysys.trait_unimpl_reason_chain(
                &self.annotate_ctx,
                &self.type_lookup(),
                self_type,
                &supertrait,
            );
            let _ = self.emit(TypeError::SupertraitNotSatisfied {
                type_name,
                trait_name: trait_name.clone(),
                supertrait,
                reason,
                span: impl_block.span,
            });
        }
    }

    /// `params` paired with what the impl answers for each: its own written
    /// arguments, then a declared default for every position it leaves out.
    ///
    /// A default resolves against the positions settled before it and the impl's
    /// target, so `P<V, W = V>` at `impl P<String>` binds `W` to `String` and
    /// `Add<Rhs = Self>` binds `Rhs` to the target.
    fn trait_params_at_impl(
        &mut self,
        params: &[ast::GenericParam],
        written: &[TypeId],
        target: SelfBinding,
    ) -> (Vec<String>, Vec<TypeId>) {
        let mut names: Vec<String> = Vec::new();
        let mut args: Vec<TypeId> = Vec::new();
        for (index, param) in params.iter().enumerate() {
            let arg = if let Some(&arg) = written.get(index) {
                arg
            } else {
                let Some(default) = param.default.clone() else {
                    break;
                };
                let (settled_names, settled_args) = (names.clone(), args.clone());
                self.with_self_binding(target, |e| {
                    e.with_type_param_args(&settled_names, &settled_args, |e| {
                        e.resolve_type(&default)
                    })
                })
            };
            names.push(param.name.clone());
            args.push(arg);
        }
        (names, args)
    }

    /// `trait_` with every argument that reads an associated type resolved at
    /// `type_args`, what the site answers for `params`. `T::Assoc` denotes no
    /// type until `T` is one, so re-spelling it at the site drops its base.
    pub(super) fn bound_trait_at_args(
        &mut self,
        trait_: FqTraitName,
        bound: &ast::TraitBound,
        params: &[ast::GenericParam],
        type_args: &[TypeId],
        self_binding: Option<SelfBinding>,
    ) -> FqTraitName {
        let pick = self.args_reading_a_projection(bound, params, self_binding);
        if !pick.contains(&true) {
            return trait_;
        }
        let names: Vec<String> = params.iter().map(|param| param.name.clone()).collect();
        self.with_type_params_bound(&names, type_args, |e| {
            e.under_self_binding(self_binding, |e| {
                e.trait_named_with_resolved_args(trait_, bound, &pick)
            })
        })
    }

    /// Which of `bound`'s arguments read an associated type off a parameter the
    /// site supplies: a written `T::Assoc`, or a base a substitution put there.
    /// Such an argument names no type until its base is one, so the site
    /// resolves it at its own arguments rather than re-spelling it
    /// (WEP-2026-08-12).
    ///
    /// Matched by the binder the projection stands on, not by its spelling.
    /// `Self` is the one base no binder carries, and a receiver is what supplies
    /// it, so an argument mentioning it is read only where one is bound.
    fn args_reading_a_projection(
        &self,
        bound: &ast::TraitBound,
        params: &[ast::GenericParam],
        self_binding: Option<SelfBinding>,
    ) -> Vec<bool> {
        let binders: Vec<AstId> = params.iter().map(|param| param.id).collect();
        bound
            .type_args
            .iter()
            .map(|ty| {
                self.tysys.reads_a_projection(ty, &binders)
                    || (self_binding.is_some() && ty.mentions("Self"))
            })
            .collect()
    }

    /// `fq` with each argument `pick` marks resolved in this frame rather than
    /// read as the spelling it was written with. `pick` is by position, since
    /// deciding it may need the resolutions the walk cannot borrow.
    pub(super) fn trait_named_with_resolved_args(
        &mut self,
        fq: FqTraitName,
        bound: &ast::TraitBound,
        pick: &[bool],
    ) -> FqTraitName {
        debug_assert_eq!(pick.len(), bound.type_args.len());
        let resolved: Vec<Option<TypeId>> = bound
            .type_args
            .iter()
            .zip(pick)
            .map(|(ty, &take)| take.then(|| self.resolve_type(ty)))
            .collect();
        if resolved.iter().all(Option::is_none) {
            return fq;
        }
        let table = self.tysys.type_table.borrow();
        trait_named_by_position(fq, &table, |i| resolved.get(i).copied().flatten())
    }

    pub(super) fn find_trait_decl_type_params(
        &self,
        trait_name: &str,
    ) -> Option<Vec<ast::GenericParam>> {
        // `decl_key_or_local` is local-first (issue #1298), so the type-param
        // list and the default-method bodies resolve to the same trait.
        if let Some(params) = self
            .decl_key_or_local(trait_name)
            .and_then(|key| self.tysys.trait_decl_type_params_of(&key))
        {
            return Some(params);
        }
        // The same headers, reached by module and name, for a trait declared
        // here that the name resolved to no key at all.
        let defs = self.tysys.resolutions.defs();
        self.tysys
            .trait_env
            .trait_decl_headers
            .iter()
            .find(|(key, header)| {
                *defs.module(**key) == self.current_module_source && header.name == trait_name
            })
            .map(|(_, header)| header.type_params.clone())
    }
}

impl TypeSystem {
    /// The declared type parameters of an already-identified trait: the
    /// `<T, U>` of `trait Foo<T, U>`.
    pub(super) fn trait_decl_type_params_of(&self, key: &DefId) -> Option<Vec<ast::GenericParam>> {
        self.trait_decl_header_of(key)
            .map(|header| header.type_params.clone())
    }
}

impl TypeSystem {
    /// The declaration header of a trait already identified, so a caller
    /// answers about the declaration its site resolved to, not a spelling.
    pub(super) fn trait_decl_header_of(&self, key: &DefId) -> Option<&TraitDeclHeader> {
        self.trait_env.decl_header_of(key)
    }

    /// The trait's declaration of the associated type `assoc_name`, or `None`
    /// when it declares no such type.
    pub(super) fn trait_assoc_type_decl(
        &self,
        key: &DefId,
        assoc_name: &str,
    ) -> Option<&ast::AssociatedTypeDecl> {
        self.trait_env.assoc_type_decl(key, assoc_name)
    }

    fn reads_a_projection(&self, ty: &ast::Type, binders: &[AstId]) -> bool {
        ty.any(&mut |ty| {
            matches!(ty, ast::Type::NamespacedGeneric(ns)
                if matches!(self.resolutions.get(ns.id),
                    Resolution::Projection(base) if binders.contains(&base)))
        })
    }
}

impl TypeSystem {
    /// The trait a compiler item names, as an identity.
    ///
    /// A compiler item is a declaration the compiler knows by construction, so
    /// a check phrased against one asks for *that* declaration — never for
    /// whatever a module's `Iterator` happens to be.
    pub(super) fn compiler_trait_def(&self, item: CompilerItem) -> Option<DefId> {
        let decl = self.type_table.borrow().compiler_items().trait_decl(item)?;
        self.resolutions.defs().of_ast_id(decl)
    }

    /// [`Self::compiler_trait_def`] as a trait reference. A compiler item
    /// declares its own parameter defaults, so the reference writes no argument.
    pub(super) fn compiler_trait(&self, item: CompilerItem) -> Option<FqTraitName> {
        Some(FqTraitName::declared(
            self.resolutions.defs(),
            self.compiler_trait_def(item)?,
        ))
    }

    /// The declaration `type_id` is an instance of. `None` where its head names
    /// none: a type parameter, a projection, an anonymous shape.
    pub(crate) fn type_def(&self, type_id: TypeId) -> Option<DefId> {
        let table = self.type_table.borrow();
        let peeled = table.peel_refs(type_id);
        if let Some(def) = table.nominal_def(peeled) {
            return Some(def);
        }
        // `Array<T>` is declared definitionless, so its instantiation carries no
        // `def` and the compiler item names the declaration instead.
        let decl = table.decl_of_type(peeled)?;
        drop(table);
        self.resolutions.defs().of_ast_id(decl)
    }

    /// Whether `type_id` implements `trait_`, at the arguments `trait_` writes
    /// for the trait's own parameters.
    ///
    /// The trait arrives as one value carrying both. There is no name beside it
    /// to compare instead, and no way to pass the identity while dropping the
    /// arguments — the two shapes that let this query answer about a trait the
    /// caller did not ask about.
    pub(super) fn type_implements_trait(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_: &FqTraitName,
    ) -> bool {
        // A binder-headed name declares nothing, so no impl answers it.
        let Some(decl) = trait_.canonical() else {
            return false;
        };
        let wanted = trait_.args();
        // Where a written argument decides, the answer is the solver's — the
        // one path that reads arguments (WEP 2026-09-01).
        if !wanted.is_empty()
            && let Some(bridge) = self.solver.as_ref()
            && let Some(answer) = bridge.answer(self, ctx, scope, type_id, trait_)
        {
            return answer;
        }
        let resolved = self.type_table.borrow().get(type_id).clone();
        let result = Self::asking(ctx, type_id, decl, wanted, || {
            self.type_implements_trait_inner(ctx, scope, type_id, &resolved, trait_)
        });
        self.check_solver_agreement(ctx, scope, type_id, trait_, result);
        result
    }

    /// The trait a bound names, with the arguments it writes for that trait's
    /// own parameters, spelled the way the impl that answers it spells them.
    pub(super) fn bound_written(&self, bound: &ast::TraitBound) -> Option<FqTraitName> {
        let decl = self.resolutions.declared(bound.id)?;
        let named = FqTraitName::declared(self.resolutions.defs(), decl);
        if bound.type_args.is_empty() {
            return Some(named);
        }
        Some(
            self.trait_env
                .fq_trait_named_by_bound(named, bound, &self.resolutions),
        )
    }

    /// Each transitive supertrait in `closure`, written in `params`' own
    /// parameter space and re-spelled at `written`.
    ///
    /// Substitution over names, not over a spelling: an argument that projects
    /// travels as a base, a member and the trait declaring it, which no
    /// spelling holds (WEP-2026-08-12).
    fn supertrait_names(
        &self,
        params: &[ast::GenericParam],
        closure: &[InheritedBound],
        written: &[FqTypeName],
    ) -> Vec<(DefId, FqTraitName)> {
        closure
            .iter()
            .map(|inherited| {
                let named = self.bound_written(&inherited.bound).unwrap_or_else(|| {
                    FqTraitName::declared(self.resolutions.defs(), inherited.decl)
                });
                let at_site = params.iter().zip(written).fold(named, |acc, (param, arg)| {
                    acc.substitute(&FqTypeName::binder(&param.name), arg)
                });
                (inherited.decl, at_site)
            })
            .collect()
    }

    /// [`Self::bound_written`] with the bound's spelling, for the diagnostics
    /// that name a trait the resolution did not reach.
    pub(super) fn bound_named_written(
        &self,
        bound: &ast::TraitBound,
    ) -> (String, Option<FqTraitName>) {
        (bound.name.clone(), self.bound_written(bound))
    }

    /// Whether `type_id` answers every one of `bounds`, each at the arguments it
    /// writes.
    fn bounds_hold(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        bounds: &[FqTraitName],
    ) -> bool {
        bounds
            .iter()
            .all(|trait_| self.type_implements_trait(ctx, scope, type_id, trait_))
    }

    /// `answer` under the recursion guard. A question already open answers
    /// without it: a repeat reached through a member is a recursive type and
    /// holds; one reached through bounds alone grounds nothing (WEP 2026-09-01).
    fn asking(
        ctx: &Scope,
        type_id: TypeId,
        trait_: DefId,
        wanted: &[FqTypeName],
        answer: impl FnOnce() -> bool,
    ) -> bool {
        let member_edges = ctx.member_edges.get();
        // Keyed by the arguments too: the same trait asked at two
        // instantiations is two questions, and only one of them may hold.
        let repeated = ctx
            .trait_check_stack
            .borrow()
            .iter()
            .find(|f| f.type_id == type_id && f.trait_ == trait_ && f.wanted == wanted)
            .map(|open| member_edges > open.member_edges);
        if let Some(repeated) = repeated {
            return repeated;
        }
        ctx.trait_check_stack.borrow_mut().push(TraitCheckFrame {
            type_id,
            trait_,
            wanted: wanted.to_vec(),
            member_edges,
        });
        let result = answer();
        ctx.trait_check_stack.borrow_mut().pop();
        result
    }

    /// The differential of WEP 2026-09-01: in debug builds, the solver must
    /// answer an outermost bound question as this path did.
    fn check_solver_agreement(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_: &FqTraitName,
        expected: bool,
    ) {
        // The solver is built in every profile, since selection asks it, but
        // this check is a differential and stays a debug-build cost.
        if !cfg!(debug_assertions) || !ctx.trait_check_stack.borrow().is_empty() {
            return;
        }
        let Some(bridge) = self.solver.as_ref() else {
            return;
        };
        let Some(actual) = bridge.answer(self, ctx, scope, type_id, trait_) else {
            return;
        };
        assert_eq!(
            actual,
            expected,
            "the trait solver disagrees with type_implements_trait: `{}: {}` is {expected} to the compiler and {actual} to the solver ({})",
            self.type_table.borrow().type_name(type_id),
            trait_.base_name(),
            bridge.explain(self, ctx, scope, type_id, trait_),
        );
    }

    /// Whether `type_id` satisfies `trait_` at the type itself, without peeling
    /// a newtype — rank 1's question, where [`Self::type_implements_trait`]
    /// answers dispatch's (`docs/wep-2026-09-01-trait-resolution.md`).
    pub(super) fn type_implements_trait_here(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_: &FqTraitName,
    ) -> bool {
        let is_newtype = matches!(
            self.type_table.borrow().get(type_id),
            ResolvedType::Newtype { .. }
        );
        if !is_newtype {
            return self.type_implements_trait(ctx, scope, type_id, trait_);
        }
        // The two facts a newtype owns rather than inherits: it has a name, and
        // it is a newtype (WEP 2026-06-13). Both are synthesized, so no impl
        // block exists for the index search below to find, and both hold at
        // depth 0 — which is what makes a `ReflectNewtype`-keyed blanket
        // outrank one the base satisfies.
        let Some(decl) = trait_.canonical() else {
            return false;
        };
        if matches!(
            self.on_bound_of(decl),
            Some(OnBoundTrait::Reflect | OnBoundTrait::ReflectNewtype)
        ) {
            return true;
        }
        let wanted = trait_.args();
        let receiver = self.type_table.borrow().impl_receiver_key(type_id);
        Self::asking(ctx, type_id, decl, wanted, || {
            self.find_trait_impl_for_subject(
                ctx,
                scope,
                Some(type_id),
                &receiver,
                decl,
                NewtypePeel::Here,
                wanted,
            )
        })
    }

    /// Whether every member of `resolved` satisfies `trait_` under `tr`'s
    /// structural rule, and which one decided when they do not. One walk: the
    /// check takes the yes and the diagnostic the no, so they cannot disagree.
    fn structural_conformance(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        resolved: &ResolvedType,
        tr: OnBoundTrait,
        trait_: &FqTraitName,
    ) -> StructuralConformance {
        let mut failing: Option<(String, TypeId)> = None;
        let walked =
            self.walk_structural_derive_members(scope, resolved, tr, &mut |member, member_tid| {
                ctx.member_edges.set(ctx.member_edges.get() + 1);
                let holds = self.type_implements_trait(ctx, scope, member_tid, trait_);
                ctx.member_edges.set(ctx.member_edges.get() - 1);
                if holds {
                    true
                } else {
                    failing = Some((member.describe(), member_tid));
                    false
                }
            });
        match (walked, failing) {
            (Some(true), _) => StructuralConformance::Holds,
            (_, Some((member, type_id))) => StructuralConformance::Fails { member, type_id },
            _ => StructuralConformance::NotApplicable,
        }
    }

    /// Explain *why* `type_id` does not implement `trait_name` by walking the
    /// auto-derive / `on_bound` structure. Each returned entry is one step of a
    /// reason chain, deepest cause last; an empty result means no structural
    /// explanation is available (the type is itself a leaf — e.g. a function
    /// type — whose non-conformance the headline message already states).
    ///
    /// Only the `Eq` / `Ord` (`automatic` policy) and `Serialize` /
    /// `Deserialize` (`on_bound` policy) structural-conformance rules are
    /// explained, since those are the ones a diagnostic can usefully unfold.
    pub(super) fn trait_unimpl_reason_chain(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_name: &str,
    ) -> Vec<String> {
        let mut chain = Vec::new();
        self.collect_trait_unimpl_reason(ctx, scope, type_id, trait_name, &mut chain);
        chain
    }

    fn collect_trait_unimpl_reason(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_name: &str,
        chain: &mut Vec<String>,
    ) {
        // Bound the depth so a pathologically nested (or cyclic) type cannot
        // produce an unbounded chain.
        if chain.len() >= 8 {
            return;
        }
        let Some(tr) = self.classify_on_bound_trait(scope, trait_name) else {
            return;
        };
        if !tr.is_field_recursive() {
            return;
        }
        let resolved = self.type_table.borrow().get(type_id).clone();

        // Every trait that drives a structural derivation is a compiler item,
        // so the declaration comes off the registry. Resolving the spelling in
        // the frame instead answers nothing for a module that never named
        // `Serialize` — which is exactly the module the chain reports for.
        let Some(trait_) = self.compiler_trait(tr.compiler_item()) else {
            return;
        };
        if let StructuralConformance::Fails {
            member: label,
            type_id: member_tid,
        } = self.structural_conformance(ctx, scope, &resolved, tr, &trait_)
        {
            let owner = self.type_id_to_string(type_id);
            let member_ty = self.type_id_to_string(member_tid);
            chain.push(format!(
                "`{owner}` does not implement `{trait_name}` because {label} of type `{member_ty}` does not implement `{trait_name}`"
            ));
            self.collect_trait_unimpl_reason(ctx, scope, member_tid, trait_name, chain);
        }
    }

    /// Whether the members `kind` exposes can be enumerated at `scope`: a
    /// struct needs every field reachable, a variant needs its cases known
    /// here. The other kinds expose members no visibility hides, and a
    /// declaration this scope cannot name exposes nothing.
    fn reflect_members_visible(
        &self,
        scope: &TypeLookup,
        resolved: &ResolvedType,
        kind: CompilerItem,
    ) -> bool {
        match kind {
            // Through the head, so an anonymous shape answers from
            // `anon_struct_fields` — it declares fields like any other struct,
            // and `walk_structural_derive_members` reads it the same way.
            CompilerItem::ReflectStruct => match resolved {
                ResolvedType::Struct { def, .. } => scope.struct_fields_of_head(*def),
                ResolvedType::GenericInstance { def, .. } => scope.struct_fields_of(*def),
                _ => None,
            }
            .is_some_and(|info| info.fields_visible_from(scope.current_module_source)),
            CompilerItem::ReflectVariant => match resolved {
                ResolvedType::Variant { def } | ResolvedType::GenericInstance { def, .. } => {
                    scope.variant_cases_of(*def).is_some()
                }
                _ => false,
            },
            // A template's holes are the site's own expressions: nothing to
            // hide from anyone.
            CompilerItem::ReflectTemplate => true,
            _ => true,
        }
    }

    /// Whether a declaration can be reflected, via the shared eligibility
    /// predicate reflect synthesis reads.
    fn is_reflect_eligible(&self, type_id: TypeId) -> bool {
        self.type_table.borrow().is_reflect_eligible(type_id)
    }

    /// The declaration a synthesis-driving trait names, from the compiler-item
    /// registry rather than the spelling that classified it.
    pub(super) fn synth_trait_key(&self, on_bound: OnBoundTrait) -> Option<DefId> {
        self.type_table
            .borrow()
            .compiler_items()
            .trait_def(on_bound.compiler_item())
    }

    /// Which [`OnBoundTrait`] `trait_` is, by identity.
    pub(super) fn on_bound_of(&self, trait_: DefId) -> Option<OnBoundTrait> {
        OnBoundTrait::of_compiler_item(self.compiler_item_of_trait(trait_)?)
    }

    /// Whether `trait_` is the prelude's `Display`. Not an [`OnBoundTrait`] —
    /// it is never auto-derived except for a plain enum.
    pub(super) fn is_display_trait_of(&self, trait_: DefId) -> bool {
        self.compiler_trait_def(CompilerItem::Display) == Some(trait_)
    }

    /// [`Self::on_bound_of`] for a caller holding a spelling with no reference
    /// site — a `#[derive(...)]` prefix.
    pub(super) fn classify_on_bound_trait(
        &self,
        scope: &TypeLookup,
        trait_name: &str,
    ) -> Option<OnBoundTrait> {
        let (on_bound, compiler_module) = {
            let tt = self.type_table.borrow();
            let items = tt.compiler_items();
            let of = |item: CompilerItem, on_bound: OnBoundTrait| {
                items.trait_module(item).map(|m| (on_bound, m.clone()))
            };
            if trait_name == items.trait_name(CompilerItem::Eq) {
                of(CompilerItem::Eq, OnBoundTrait::Eq)
            } else if trait_name == items.trait_name(CompilerItem::Ord) {
                of(CompilerItem::Ord, OnBoundTrait::Ord)
            } else if items.trait_name_opt(CompilerItem::Serialize) == Some(trait_name) {
                of(CompilerItem::Serialize, OnBoundTrait::Serialize)
            } else if items.trait_name_opt(CompilerItem::Deserialize) == Some(trait_name) {
                of(CompilerItem::Deserialize, OnBoundTrait::Deserialize)
            } else if items.trait_name_opt(CompilerItem::WireNumbered) == Some(trait_name) {
                of(CompilerItem::WireNumbered, OnBoundTrait::WireNumbered)
            } else if trait_name == items.trait_name(CompilerItem::Default) {
                of(CompilerItem::Default, OnBoundTrait::Default)
            } else if trait_name == items.trait_name(CompilerItem::Reflect) {
                of(CompilerItem::Reflect, OnBoundTrait::Reflect)
            } else if trait_name == items.trait_name(CompilerItem::ReflectStruct) {
                of(CompilerItem::ReflectStruct, OnBoundTrait::ReflectStruct)
            } else if trait_name == items.trait_name(CompilerItem::ReflectVariant) {
                of(CompilerItem::ReflectVariant, OnBoundTrait::ReflectVariant)
            } else if trait_name == items.trait_name(CompilerItem::ReflectEnum) {
                of(CompilerItem::ReflectEnum, OnBoundTrait::ReflectEnum)
            } else if trait_name == items.trait_name(CompilerItem::ReflectFlags) {
                of(CompilerItem::ReflectFlags, OnBoundTrait::ReflectFlags)
            } else if trait_name == items.trait_name(CompilerItem::ReflectNewtype) {
                of(CompilerItem::ReflectNewtype, OnBoundTrait::ReflectNewtype)
            } else if trait_name == items.trait_name(CompilerItem::ReflectTemplate) {
                of(CompilerItem::ReflectTemplate, OnBoundTrait::ReflectTemplate)
            } else if trait_name == items.trait_name(CompilerItem::Ref) {
                of(CompilerItem::Ref, OnBoundTrait::Ref)
            } else if trait_name == items.trait_name(CompilerItem::RefMut) {
                of(CompilerItem::RefMut, OnBoundTrait::RefMut)
            } else if trait_name == items.trait_name(CompilerItem::Inspect) {
                of(CompilerItem::Inspect, OnBoundTrait::Inspect)
            } else {
                None
            }
        }?;
        match self.scoped_trait_decl_module(scope, trait_name) {
            Some(module) => (*module == compiler_module).then_some(on_bound),
            None => Some(on_bound),
        }
    }

    /// `true` when `trait_name` resolves to the compiler's prelude `Display`
    /// trait in this scope (not a same-name user trait). `Display` is not an
    /// [`OnBoundTrait`] — it is never auto-derived except for plain enums — so
    /// its identity is checked here rather than through `classify_on_bound_trait`.
    pub(super) fn is_display_trait(&self, scope: &TypeLookup, trait_name: &str) -> bool {
        let compiler_module = {
            let tt = self.type_table.borrow();
            let items = tt.compiler_items();
            if trait_name != items.trait_name(CompilerItem::Display) {
                return false;
            }
            let Some(module) = items.trait_module(CompilerItem::Display) else {
                return false;
            };
            module.clone()
        };
        match self.scoped_trait_decl_module(scope, trait_name) {
            Some(module) => *module == compiler_module,
            None => true,
        }
    }

    /// The trait declaration `name` binds to in `scope`, following an alias to
    /// the name the declaration calls itself.
    ///
    /// For the `TypeSystem` queries that hold a scope and a spelling rather
    /// than a reference site; a caller with a site asks the site instead.
    fn scoped_trait_decl_key(&self, scope: &TypeLookup, name: &str) -> Option<DefId> {
        let key = scope.declaration(name)?;
        self.trait_env.declares_trait(&key).then_some(key)
    }

    /// Whether what a bound writes for `trait_`'s own parameters answers
    /// `wanted`, a position it leaves open taking the trait's declared default.
    /// A `Self` default names whatever is answering, which no written argument
    /// equals, so only a default naming a type answers here.
    fn args_answer(&self, args: &[FqTypeName], trait_: DefId, wanted: &[FqTypeName]) -> bool {
        wanted.iter().enumerate().all(|(i, want)| {
            args.get(i)
                .or_else(|| self.trait_env.named_default_arg(trait_, i))
                == Some(want)
        })
    }

    /// Whether `bound` on a type parameter supplies `trait_` at the arguments
    /// `wanted` writes — the bound itself writing them, or a supertrait that
    /// does (`AsStrSlice: Eq<String>`).
    fn bound_supplies(
        &self,
        scope: &TypeLookup,
        bound: &ast::TraitBound,
        trait_: DefId,
        wanted: &[FqTypeName],
    ) -> bool {
        let args_of = |named: Option<FqTraitName>| {
            self.args_answer(
                &named.map(|n| n.args().to_vec()).unwrap_or_default(),
                trait_,
                wanted,
            )
        };
        if self.scoped_trait_decl_key(scope, &bound.name) == Some(trait_) {
            return args_of(self.bound_written(bound));
        }
        self.scoped_supertrait_names(scope, &bound.name, &bound.type_args)
            .iter()
            .any(|(decl, named)| *decl == trait_ && self.args_answer(named.args(), trait_, wanted))
    }

    /// The transitive supertraits of `trait_name` as seen from `scope`, each
    /// named at the arguments the reading site writes for it.
    fn scoped_supertrait_names(
        &self,
        scope: &TypeLookup,
        trait_name: &str,
        written: &[ast::Type],
    ) -> Vec<(DefId, FqTraitName)> {
        let written: Vec<FqTypeName> = written
            .iter()
            .map(|arg| written_type_arg(arg, &self.resolutions))
            .collect();
        let (params, closure) = match self.scoped_trait_decl_key(scope, trait_name) {
            Some(key) => self.trait_env.supertrait_closure_declared(&key),
            None => self.trait_env.supertrait_closure_declared_named(trait_name),
        };
        self.supertrait_names(params, closure, &written)
    }

    /// The trait declaration `trait_name` binds to in scope (local, else an
    /// explicit import); `None` when it falls through to the ambient compiler
    /// trait. Lets a same-name user `trait` be distinguished from the compiler's.
    fn scoped_trait_decl_module<'a>(
        &self,
        scope: &TypeLookup<'a>,
        trait_name: &str,
    ) -> Option<&'a ModuleSource> {
        let def = scope.declaration(trait_name)?;
        self.trait_env
            .declares_trait(&def)
            .then(|| scope.resolutions.defs().module(def))
    }

    fn walk_structural_derive_members(
        &self,
        scope: &TypeLookup,
        resolved: &ResolvedType,
        tr: OnBoundTrait,
        visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool,
    ) -> Option<bool> {
        // A member is read at the instance: `items: List<T>` is `List<i32>`
        // at `Gen<i32>`, however deep the parameter sits.
        let at_instance =
            |type_args: &[TypeId], tid: TypeId| self.substitute_type_params(tid, type_args);
        let walk_struct = |info: &StructFieldInfo,
                           type_args: &[TypeId],
                           visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool|
         -> bool {
            info.fields.iter().all(|(fname, tid, _)| {
                visit(StructuralMember::Field(fname), at_instance(type_args, *tid))
            })
        };
        let walk_variant = |info: &VariantInfo,
                            type_args: &[TypeId],
                            visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool|
         -> bool {
            info.cases
                .iter()
                .filter(|c| c.payload != TypeTable::UNIT)
                .all(|c| {
                    visit(
                        StructuralMember::Case(&c.name),
                        at_instance(type_args, c.payload),
                    )
                })
        };
        match resolved {
            ResolvedType::Enum { .. } => Some(true),
            // A bitmask has no members to recurse into, like a plain `enum`.
            ResolvedType::Flags { .. } => Some(true),
            // The host interns handles, so two are equal exactly when they name one object.
            ResolvedType::Resource { def }
                if tr == OnBoundTrait::Eq
                    && self.type_table.borrow().is_unrestricted_resource(*def) =>
            {
                Some(true)
            }
            ResolvedType::Struct { def, .. } => {
                // An anonymous struct has fields to walk like any other; it
                // just has no declaration to reach them through. Asking the
                // head answers for both, which is what lets a `{ ..ctx, x }`
                // literal satisfy a structural bound at all.
                let info = scope.struct_fields_of_head(*def)?;
                Some(walk_struct(info, &[], visit))
            }
            ResolvedType::Variant { def } => {
                if tr == OnBoundTrait::Ord {
                    return None;
                }
                let info = scope.variant_cases_of(*def)?;
                Some(walk_variant(info, &[], visit))
            }
            ResolvedType::GenericInstance { def, type_args } => {
                if let Some(info) = scope.struct_fields_of(*def) {
                    Some(walk_struct(info, type_args, visit))
                } else if tr != OnBoundTrait::Ord
                    && let Some(info) = scope.variant_cases_of(*def)
                {
                    Some(walk_variant(info, type_args, visit))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub(super) fn structurally_derivable_for_explicit_request(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_name: &str,
    ) -> bool {
        let Some(tr) = self.classify_on_bound_trait(scope, trait_name) else {
            return false;
        };
        let Some(trait_) = self.compiler_trait(tr.compiler_item()) else {
            return false;
        };
        if tr.is_total() {
            return true;
        }
        if tr == OnBoundTrait::Default {
            return self.is_defaultable_struct(scope, type_id);
        }
        if tr == OnBoundTrait::WireNumbered {
            return self.is_numbered_struct(scope, type_id);
        }
        let resolved = self.type_table.borrow().get(type_id).clone();
        match &resolved {
            ResolvedType::Newtype { base_type, .. } => {
                self.type_implements_trait(ctx, scope, *base_type, &trait_)
            }
            ResolvedType::Flags { .. } => {
                self.type_implements_trait(ctx, scope, TypeTable::U32, &trait_)
            }
            nominal => {
                // A marker on a generic declaration (`impl<T> Eq for Pair<T>;`)
                // asks whether the *declaration* derives, and its body is
                // emitted per instantiation: a member that is one of the
                // declaration's own parameters is that instantiation's to
                // answer (WEP 2026-06-25). Not the receiver-versus-impl-bound
                // question `enforce_impl_type_arg_bounds` asks, which admits no
                // such deferral.
                self.walk_structural_derive_members(scope, nominal, tr, &mut |_, member| {
                    matches!(
                        self.type_table.borrow().get(member),
                        ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
                    ) || self.type_implements_trait(ctx, scope, member, &trait_)
                }) == Some(true)
            }
        }
    }

    fn is_defaultable_struct(&self, scope: &TypeLookup, type_id: TypeId) -> bool {
        let name = {
            let tt = self.type_table.borrow();
            if !matches!(tt.get(type_id), ResolvedType::Struct { .. }) {
                return false;
            }
            tt.type_name(type_id)
        };
        self.auto_derive_default_struct_type(scope, &name).is_some()
    }

    /// The `WireNumbered` bound's eligibility: a struct whose every field
    /// carries `#[wire(number = N)]`. A struct with no fields qualifies, and
    /// encodes as the empty record protobuf reads it as.
    fn is_numbered_struct(&self, scope: &TypeLookup, type_id: TypeId) -> bool {
        let Some(def) = ({
            let tt = self.type_table.borrow();
            match tt.get(type_id) {
                ResolvedType::Struct { def, .. } => def.decl(),
                _ => None,
            }
        }) else {
            return false;
        };
        scope
            .struct_fields_of(def)
            .is_some_and(|info| info.field_wire_numbers.iter().all(Option::is_some))
    }

    /// The `Ref` marker's eligibility: whether a value of this type is a Wasm GC
    /// reference (WEP 2026-01-20). A `Newtype` follows its base type.
    pub(super) fn is_ref_identity(&self, resolved: &ResolvedType) -> bool {
        match resolved {
            // `i128` / `u128` are prelude structs, so they answer through the
            // `Struct` arm rather than here.
            ResolvedType::Primitive(_) => false,
            ResolvedType::Struct { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::Function { .. }
            | ResolvedType::Ref(_)
            | ResolvedType::MutRef(_) => true,
            ResolvedType::Newtype { base_type, .. } => {
                let base = self.type_table.borrow().get(*base_type).clone();
                self.is_ref_identity(&base)
            }
            ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Reactive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Unknown
            | ResolvedType::Error
            | ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::AssocTypeProjection { .. } => false,
        }
    }

    /// The `RefMut` marker's eligibility: a `Ref` type whose value is mutated in
    /// place rather than replaced on assign (WEP 2026-01-20). `variant` and `fn`
    /// are `Ref` but boxed (replace-on-assign), so a `&mut` cannot write through
    /// them; every other `Ref` type qualifies. A `Newtype` follows its base.
    pub(super) fn is_ref_mut_identity(
        &self,
        is_variant: &dyn Fn(DefId) -> bool,
        resolved: &ResolvedType,
    ) -> bool {
        match resolved {
            ResolvedType::Variant { .. } | ResolvedType::Function { .. } => false,
            ResolvedType::GenericInstance { def, .. } => {
                if is_variant(*def) {
                    false
                } else {
                    self.is_ref_identity(resolved)
                }
            }
            ResolvedType::Newtype { base_type, .. } => {
                let base = self.type_table.borrow().get(*base_type).clone();
                self.is_ref_mut_identity(is_variant, &base)
            }
            _ => self.is_ref_identity(resolved),
        }
    }

    /// Whether a reference is denied `trait_`'s bound, which it otherwise
    /// inherits from its pointee by auto-deref at the call. `==` on a reference
    /// is identity, so `&T` is no `Ord`; and see below for the other rule.
    pub(super) fn ref_denies_bound(&self, on_bound: Option<OnBoundTrait>, trait_: DefId) -> bool {
        if on_bound == Some(OnBoundTrait::Ord) {
            return true;
        }
        // A receiverless method has no receiver to deref, so `&T` inherits it
        // by forwarding — which works only where `Self` is absent from the
        // signature: `kind() -> String` forwards, `-> Option<Self>` cannot.
        trait_sig_of_with(trait_, &self.trait_env, &self.signatures).is_some_and(|sig| {
            sig.methods.values().any(|m| {
                m.sig.self_kind == SelfKind::None && self.receiverless_method_mentions_self(&m.sig)
            })
        })
    }

    /// Whether a receiverless method's signature names `Self` — in a parameter,
    /// the return type, or a bound on one of its own type parameters. Slot 0 of
    /// a trait method's frame is `Self`.
    fn receiverless_method_mentions_self(&self, sig: &sig::MethodSig) -> bool {
        let table = self.type_table.borrow();
        let in_types = sig
            .decl
            .param_types
            .iter()
            .chain(sig.decl.return_type.iter())
            .any(|t| table.contains_type_param_index(*t, 0));
        in_types
            || sig
                .own_params
                .iter()
                .any(|p| p.bounds.iter().any(ast::TraitBound::writes_self))
    }

    fn type_implements_trait_inner(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        resolved: &ResolvedType,
        trait_: &FqTraitName,
    ) -> bool {
        let Some(decl) = trait_.canonical() else {
            return false;
        };
        let wanted = trait_.args();
        let on_bound = self.on_bound_of(decl);

        if on_bound.is_some_and(OnBoundTrait::is_total) {
            return true;
        }

        // A plain `enum` auto-derives `Display` (the bare case name), so its
        // bound holds before `synthesize_traits` emits the body.
        if matches!(resolved, ResolvedType::Enum { .. }) && self.is_display_trait_of(decl) {
            return true;
        }

        if let Some(name) = bound_param_name(resolved) {
            return ctx
                .trait_ctx
                .type_param_bounds
                .get(name)
                .is_some_and(|bounds| {
                    bounds
                        .iter()
                        .any(|b| self.bound_supplies(scope, b, decl, wanted))
                });
        }

        if on_bound == Some(OnBoundTrait::Ref) {
            return self.is_ref_identity(resolved);
        }

        if on_bound == Some(OnBoundTrait::RefMut) {
            return self
                .is_ref_mut_identity(&|def| scope.variant_cases_of(def).is_some(), resolved);
        }

        let is_eq = on_bound == Some(OnBoundTrait::Eq);
        let is_eq_or_ord = is_eq || on_bound == Some(OnBoundTrait::Ord);

        // Primitives have built-in implementations for certain traits
        if let ResolvedType::Primitive(prim) = &resolved {
            // A derived impl writes no argument, so it answers `Eq<Self>` and
            // `Ord<Self>`. A bound writing one needs an impl that writes it.
            if is_eq_or_ord && wanted.is_empty() {
                return true;
            }
            // Numeric primitives implement the operator traits the compiler
            // supplies — the items, so a user `trait Rem` gets none of it.
            if self
                .compiler_item_of_trait(decl)
                .is_some_and(|op| primitive_has_operator(prim.as_str(), op))
            {
                return true;
            }
            return self.find_trait_impl_for_subject(
                ctx,
                scope,
                Some(type_id),
                &Receiver::Type(FqTypeName::builtin(prim.as_str())),
                decl,
                NewtypePeel::Follow,
                wanted,
            );
        }

        // Read the head out before the block: a `borrow()` held in a
        // `let`-chain lives for the whole body, and the body borrows mutably
        // to record the synthesis request.
        let nominal = self.type_table.borrow().nominal_head(type_id);
        // A structural derivation writes no argument, so it answers the trait's
        // declared defaults. A bound writing one needs an impl that writes it.
        if let Some(tr) = on_bound
            && tr.is_field_recursive()
            && wanted.is_empty()
            && let Some((_, module_source)) = nominal
        {
            let receiver = self.type_table.borrow().impl_receiver_key(type_id);
            let serde_blocked = tr.is_serde()
                && self.has_real_trait_impl_for_type(ctx, scope, Some(type_id), &receiver, decl);
            if !serde_blocked
                && self.structural_conformance(ctx, scope, resolved, tr, trait_)
                    == StructuralConformance::Holds
            {
                if let Some(key) = self.synth_trait_key(tr) {
                    self.type_table
                        .borrow_mut()
                        .record_bound_driven_synth_request_for(type_id, &module_source, &key);
                }
                return true;
            }
        }

        if on_bound == Some(OnBoundTrait::WireNumbered) {
            return self.is_numbered_struct(scope, type_id);
        }

        if let ResolvedType::Struct { def, .. } = &resolved
            && on_bound == Some(OnBoundTrait::Default)
            && let Some(name) = def
                .decl()
                .map(|d| self.type_table.borrow().def_name(d).to_string())
            && self.auto_derive_default_struct_type(scope, &name).is_some()
        {
            if let Some(key) = on_bound.and_then(|t| self.synth_trait_key(t)) {
                let module_source = self
                    .type_table
                    .borrow()
                    .def_module(def.decl().unwrap())
                    .clone();
                self.type_table
                    .borrow_mut()
                    .record_bound_driven_synth_request_for(type_id, &module_source, &key);
            }
            return true;
        }

        // Which kind the type is has one answer, `TypeTable::reflect_kind`;
        // this adds what the type table cannot see, whether the members the
        // kind exposes are visible here (WEP 2026-06-13). The identity root
        // skips that gate — naming a type is not enumerating it — and a kind
        // a newtype does not own is left to the recursion below.
        let reflect_kind = self.type_table.borrow().reflect_kind(type_id);
        if let Some(bound) = on_bound.filter(|b| b.is_reflect())
            && let Some(kind) = reflect_kind
            && (bound == OnBoundTrait::Reflect
                || (kind == bound.compiler_item()
                    && self.reflect_members_visible(scope, resolved, kind)))
            && self.is_reflect_eligible(type_id)
        {
            // A declaration's impl is synthesized in the module walk; an
            // instance's is minted on request, so record one here.
            if matches!(resolved, ResolvedType::GenericInstance { .. })
                && let Some(key) = self.synth_trait_key(bound)
            {
                let (_, module_source) = self
                    .type_table
                    .borrow()
                    .nominal_head(type_id)
                    .expect("a generic instance names a declaration");
                self.type_table
                    .borrow_mut()
                    .record_bound_driven_synth_request_for(type_id, &module_source, &key);
            }
            return true;
        }

        // A newtype inherits its base's impls (WEP 2026-01-29), so a structure
        // kind holds for it exactly when it holds for what it wraps. Asked of
        // the base, not of the ultimate one, so a chain answers a link at a
        // time and each still names itself.
        if let ResolvedType::Newtype { base_type, .. } = &resolved
            && !matches!(on_bound, Some(OnBoundTrait::ReflectNewtype))
            && on_bound.is_some_and(OnBoundTrait::is_reflect)
        {
            let base = *base_type;
            let base_resolved = self.type_table.borrow().get(base).clone();
            return self.type_implements_trait_inner(ctx, scope, base, &base_resolved, trait_);
        }

        // Get the type name and type args for looking up implementations
        let (type_name, type_args) = match &resolved {
            ResolvedType::Struct { .. }
            | ResolvedType::Enum { .. }
            | ResolvedType::Variant { .. } => {
                (self.type_table.borrow().fq_base_type_name(type_id), None)
            }
            // The raw GC array `Array<T>` carries its element as a single type
            // arg, so trait impls (`impl IntoIterator for Array<T>`) resolve
            // under the canonical name "Array".
            ResolvedType::BuiltinArray(_) => (
                FqTypeName::builtin(TypeTable::ARRAY_TYPE_NAME),
                self.impl_position_args(type_id),
            ),
            ResolvedType::GenericInstance { .. } => (
                self.type_table.borrow().fq_base_type_name(type_id),
                self.impl_position_args(type_id),
            ),
            ResolvedType::Ref(inner) => {
                // References always implement Eq via ref.eq (identity
                // comparison), which is `Eq<Self>`. A bound writing an argument
                // asks the pointee instead.
                if is_eq && wanted.is_empty() {
                    return true;
                }
                // Check for a specific impl Trait for &T first (e.g., impl Inspect for &T)
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args(
                    ctx,
                    scope,
                    Some(type_id),
                    &Receiver::Ref(RefKind::Shared),
                    decl,
                    self.impl_position_args(type_id).as_deref(),
                    NewtypePeel::Follow,
                    wanted,
                ) {
                    return true;
                }
                if self.ref_denies_bound(on_bound, decl) {
                    return false;
                }
                return self.type_implements_trait(ctx, scope, inner_id, trait_);
            }
            ResolvedType::MutRef(inner) => {
                if is_eq && wanted.is_empty() {
                    return true;
                }
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args(
                    ctx,
                    scope,
                    Some(type_id),
                    &Receiver::Ref(RefKind::Mut),
                    decl,
                    self.impl_position_args(type_id).as_deref(),
                    NewtypePeel::Follow,
                    wanted,
                ) {
                    return true;
                }
                if self.ref_denies_bound(on_bound, decl) {
                    return false;
                }
                return self.type_implements_trait(ctx, scope, inner_id, trait_);
            }
            ResolvedType::AssocTypeProjection { bounds, .. } => {
                // An associated type projection T::Assoc implements a trait if
                // the trait declaration for Assoc declares that bound, at the
                // arguments the bound writes.
                return bounds.iter().any(|b| {
                    b.canonical() == Some(decl) && self.args_answer(b.args(), decl, wanted)
                });
            }
            ResolvedType::Newtype { base_type, .. } => {
                // Check for a direct impl on the newtype first (e.g., impl Describe for Meters)
                let receiver = self.type_table.borrow().impl_receiver_key(type_id);
                if self.find_trait_impl_for_subject(
                    ctx,
                    scope,
                    Some(type_id),
                    &receiver,
                    decl,
                    NewtypePeel::Follow,
                    wanted,
                ) {
                    return true;
                }
                // Fall back to base type's trait implementation
                let base_id = *base_type;
                return self.type_implements_trait(ctx, scope, base_id, trait_);
            }
            // `()` names no declaring module, so an `impl Trait for ()` is
            // indexed under the builtin spelling the unit type mangles as.
            ResolvedType::Unit => (FqTypeName::builtin(TypeTable::UNIT_TYPE_NAME), None),
            ResolvedType::Flags { .. } => {
                let receiver = self.type_table.borrow().impl_receiver_key(type_id);
                if self.find_trait_impl_for_subject(
                    ctx,
                    scope,
                    Some(type_id),
                    &receiver,
                    decl,
                    NewtypePeel::Follow,
                    wanted,
                ) {
                    return true;
                }
                return self.type_implements_trait(ctx, scope, TypeTable::U32, trait_);
            }
            _ => return false,
        };

        self.find_trait_impl_for_type_with_args(
            ctx,
            scope,
            Some(type_id),
            &Receiver::Type(type_name),
            decl,
            type_args.as_deref(),
            NewtypePeel::Follow,
            wanted,
        )
    }

    /// Whether an impl block makes `type_key` implement `decl`. `subject` is
    /// the receiver's own `TypeId` where the caller holds one: a blanket
    /// pinning an assoc type to its receiver is decidable only against that.
    pub(super) fn find_trait_impl_for_subject(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        subject: Option<TypeId>,
        type_key: &Receiver,
        trait_: DefId,
        peel: NewtypePeel,
        wanted: &[FqTypeName],
    ) -> bool {
        self.find_trait_impl_for_type_with_args(
            ctx, scope, subject, type_key, trait_, None, peel, wanted,
        )
    }

    pub(super) fn has_real_trait_impl_for_type(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        subject: Option<TypeId>,
        type_key: &Receiver,
        trait_: DefId,
    ) -> bool {
        self.trait_env
            .has_any_methodful_impl_by_receiver(type_key, trait_)
            || self.blanket_trait_impl_applies(
                ctx,
                scope,
                subject,
                type_key,
                trait_,
                NewtypePeel::Follow,
            )
    }

    /// Whether a bound writing `wanted` selects the header — see
    /// [`super::trait_env::header_answers_bound_args`].
    fn header_answers_bound_args(&self, header: &ImplHeader, wanted: &[FqTypeName]) -> bool {
        let Some(decl) = header.trait_def() else {
            return true;
        };
        let Some(decl_header) = self.trait_env.decl_header_of(&decl) else {
            return true;
        };
        header_answers_bound_args(
            header.trait_arg_ids(),
            &header.ty,
            &decl_header.type_params,
            &self.resolutions,
            wanted,
        )
    }

    /// Check if there's a trait impl for a type, with optional type args for bounds checking.
    /// For `impl<T: Eq> Eq for List<T>`, when checking `List<Foo>`, passes `[Foo]` as `type_args`.
    pub(super) fn find_trait_impl_for_type_with_args(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        subject: Option<TypeId>,
        type_key: &Receiver,
        trait_: DefId,
        type_args: Option<&[TypeId]>,
        peel: NewtypePeel,
        wanted: &[FqTypeName],
    ) -> bool {
        let trait_env = self.trait_env.clone();
        {
            for entry in trait_env.entries_by_receiver_vec(type_key) {
                let Some(header) = trait_env.impl_headers.get(&entry) else {
                    continue;
                };
                // Both sides are declarations: the query's comes from the
                // reference site that asked (a bound, a `T::method()` prefix),
                // the header's from the site it writes, and each was resolved by
                // the module that wrote it. Comparing spellings instead is what
                // made an aliased bound unsatisfiable and a same-named foreign
                // trait satisfied (#1785).
                if header.trait_def() == Some(trait_)
                    && self.header_answers_bound_args(header, wanted)
                    && self.inherent_impl_type_args_match(&header.ty, type_args)
                    && self.check_impl_block_bounds(
                        ctx,
                        scope,
                        &header.type_params,
                        &header.ty,
                        subject,
                        type_args,
                    )
                {
                    return true;
                }
            }
        }

        // The current module's trait impls are already covered by
        // `impl_index` above (the index is built from every loaded module,
        // including this one), so no separate current-module scan is needed.

        self.blanket_trait_impl_applies(ctx, scope, subject, type_key, trait_, peel)
    }

    /// [`Self::type_implements_trait_inner`]'s primitive arm, asked of a
    /// receiver key. An impl-index lookup finds `impl Ord for i32` but not the
    /// compiler-supplied `Add`, so a blanket bounded by one needs this.
    fn primitive_satisfies_builtin_trait(&self, type_key: &Receiver, bound: &BlanketBound) -> bool {
        let Receiver::Type(fq) = type_key else {
            return false;
        };
        let TypeHead::Builtin(name) = fq.head() else {
            return false;
        };
        if !PrimitiveType::is_primitive_name(name) {
            return false;
        }
        let Some(trait_) = bound.decl() else {
            return false;
        };
        matches!(
            self.on_bound_of(trait_),
            Some(OnBoundTrait::Eq | OnBoundTrait::Ord)
        ) || self
            .compiler_item_of_trait(trait_)
            .is_some_and(|op| primitive_has_operator(name, op))
    }

    /// Which compiler item `trait_` is, or `None` for a trait the compiler does
    /// not know. The one reverse lookup: the spelling answers for a user trait
    /// that shares the name.
    pub(super) fn compiler_item_of_trait(&self, trait_: DefId) -> Option<CompilerItem> {
        let decl = self.resolutions.defs().ast_id(trait_);
        self.type_table
            .borrow()
            .compiler_items()
            .trait_item_of_decl(decl)
    }

    /// Whether one of a blanket's receiver bounds holds at the level `peel`
    /// names. `Follow` also asks the subject query, the one entry a
    /// structurally derived `Eq` / `Ord` has; `Here` asks only the guarded one,
    /// since an unguarded index lookup cannot stop a cycle among bounds.
    fn blanket_bound_holds(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        subject: Option<TypeId>,
        type_key: &Receiver,
        bound_trait: &FqTraitName,
        peel: NewtypePeel,
    ) -> bool {
        match (peel, subject) {
            (NewtypePeel::Here, Some(id)) => {
                self.type_implements_trait_here(ctx, scope, id, bound_trait)
            }
            // The general question already searches the impls, under the guard.
            (NewtypePeel::Follow, Some(id)) => {
                self.type_implements_trait(ctx, scope, id, bound_trait)
            }
            (_, None) => bound_trait.canonical().is_some_and(|decl| {
                self.find_trait_impl_for_subject(
                    ctx,
                    scope,
                    subject,
                    type_key,
                    decl,
                    peel,
                    bound_trait.args(),
                )
            }),
        }
    }

    fn blanket_trait_impl_applies(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        subject: Option<TypeId>,
        type_key: &Receiver,
        trait_: DefId,
        peel: NewtypePeel,
    ) -> bool {
        // A structural obligation is the member walk's to answer, so a
        // `Reflect*`-bounded blanket does not get to answer it: its bound holds
        // for every type of that kind, while the bound that decides eligibility
        // is the pack's (`..F: Serialize`), which this index does not carry.
        // Letting it answer would admit a type whose own members refuse the
        // trait being asked for, and lose the reason chain that says which one.
        // Only that shape is skipped — a user-written blanket over the same
        // trait still answers for itself.
        let structural = self
            .on_bound_of(trait_)
            .is_some_and(OnBoundTrait::is_field_recursive);
        let trait_env = self.trait_env.clone();
        for blanket in trait_env
            .blanket_impls
            .get(&trait_)
            .into_iter()
            .flatten()
            // A value blanket mints no instance for a reference, so it does not
            // answer one. This is what left `&i32: Sum` holding with nothing to
            // dispatch to.
            .filter(|b| {
                b.receiver == BlanketReceiver::Value && !matches!(type_key, Receiver::Ref(_))
            })
            .filter(|b| !(structural && self.is_reflect_bounded(scope, b)))
        {
            let bounds_satisfied = blanket.bounds.iter().all(|bound| {
                self.synthesized_reflect_bound_holds(scope, &type_key.decl_key(), &bound.name)
                    || self.primitive_satisfies_builtin_trait(type_key, bound)
                    || bound.trait_.as_ref().is_some_and(|bound_trait| {
                        self.blanket_bound_holds(ctx, scope, subject, type_key, bound_trait, peel)
                    })
            });
            if bounds_satisfied && self.blanket_assoc_constraints_hold(subject, &blanket.bounds) {
                return true;
            }
        }

        false
    }

    /// Whether `subject` satisfies the associated-type constraints a blanket's
    /// bounds pin to its receiver param (`impl<T: Mul<Output = T>> Product for
    /// T`). Compared as `TypeId`s, so a generic argument counts.
    pub(super) fn blanket_assoc_constraints_hold(
        &self,
        subject: Option<TypeId>,
        bounds: &[BlanketBound],
    ) -> bool {
        bounds.iter().all(|bound| {
            if bound.pinned_to_receiver.is_empty() {
                return true;
            }
            let (Some(subject), Some(trait_)) = (subject, bound.decl()) else {
                return false;
            };
            bound.pinned_to_receiver.iter().all(|assoc| {
                self.type_table
                    .borrow_mut()
                    .resolve_trait_assoc_type_of_instance(subject, &trait_, assoc)
                    .is_none_or(|actual| actual == subject)
            })
        })
    }

    /// Whether `blanket`'s receiver bound is a reflection trait — the shape the
    /// stdlib derives structural traits through.
    fn is_reflect_bounded(&self, scope: &TypeLookup, blanket: &BlanketImpl) -> bool {
        blanket.bounds.iter().any(|bound| {
            self.classify_on_bound_trait(scope, &bound.name)
                .is_some_and(OnBoundTrait::is_reflect)
        })
    }

    /// The module declaring `def`, when `def` is a newtype. The kind with no
    /// members carries no member info to read a module off, so the declaration
    /// answers directly.
    fn newtype_declaring_module(&self, scope: &TypeLookup, def: DefId) -> Option<ModuleSource> {
        // A generic declaration is a newtype too, and it is recorded in its own
        // table: each instantiation resolves the base afresh, so there is no
        // single type to key it by.
        if scope.newtype_of(def).is_none() && scope.generic_newtype_of(def).is_none() {
            return None;
        }
        Some(self.type_table.borrow().defs().module(def).clone())
    }

    /// Whether `bound_name` is a synthesized reflection trait the subject is
    /// eligible for by kind. These have no impl blocks, so the name-based search
    /// misses them; a hit records the bound-driven synth request.
    ///
    /// One scope lookup keys every kind check below, so the four cannot each
    /// reach a different declaration.
    fn synthesized_reflect_bound_holds(
        &self,
        scope: &TypeLookup,
        type_name: &DeclName,
        bound_name: &str,
    ) -> bool {
        let Some(on_bound) = self.classify_on_bound_trait(scope, bound_name) else {
            return false;
        };
        let Some(def) = scope.declaration(type_name.as_decl_str()) else {
            return false;
        };
        let subject = match on_bound {
            // The root asks for a name, not for a shape, so whichever kind
            // answers answers for it.
            OnBoundTrait::Reflect => [
                OnBoundTrait::ReflectStruct,
                OnBoundTrait::ReflectVariant,
                OnBoundTrait::ReflectEnum,
                OnBoundTrait::ReflectFlags,
            ]
            .into_iter()
            .find_map(|kind| declaring_module_of_kind(scope, def, kind))
            .or_else(|| self.newtype_declaring_module(scope, def)),
            OnBoundTrait::ReflectStruct
            | OnBoundTrait::ReflectVariant
            | OnBoundTrait::ReflectEnum
            | OnBoundTrait::ReflectFlags => declaring_module_of_kind(scope, def, on_bound),
            OnBoundTrait::ReflectNewtype => self.newtype_declaring_module(scope, def),
            // A template shape names no declaration; its request is recorded
            // at the site that mints it.
            OnBoundTrait::ReflectTemplate => None,
            OnBoundTrait::Eq
            | OnBoundTrait::Ord
            | OnBoundTrait::Serialize
            | OnBoundTrait::Deserialize
            | OnBoundTrait::WireNumbered
            | OnBoundTrait::Default
            | OnBoundTrait::Ref
            | OnBoundTrait::RefMut
            | OnBoundTrait::Inspect => None,
        };
        let Some(module_source) = subject else {
            return false;
        };
        let Some(key) = self.synth_trait_key(on_bound) else {
            return false;
        };
        let head = {
            let tt = self.type_table.borrow();
            FqTypeName::declared(tt.defs(), def).head().clone()
        };
        self.type_table
            .borrow_mut()
            .record_bound_driven_synth_request(&head, &module_source, &key);
        true
    }
}

/// The module declaring `def`, when `def` is the kind `on_bound` names.
/// `None` for any other kind, and for a bound that is not a reflection kind.
fn declaring_module_of_kind(
    scope: &TypeLookup,
    def: DefId,
    on_bound: OnBoundTrait,
) -> Option<ModuleSource> {
    match on_bound {
        OnBoundTrait::ReflectStruct => scope.struct_fields_of(def).map(|i| i.module_source.clone()),
        OnBoundTrait::ReflectVariant => {
            scope.variant_cases_of(def).map(|i| i.module_source.clone())
        }
        OnBoundTrait::ReflectEnum => scope.enum_cases_of(def).map(|i| i.module_source.clone()),
        OnBoundTrait::ReflectFlags => scope.flags_members_of(def).map(|i| i.module_source.clone()),
        OnBoundTrait::Reflect
        | OnBoundTrait::ReflectNewtype
        | OnBoundTrait::ReflectTemplate
        | OnBoundTrait::Eq
        | OnBoundTrait::Ord
        | OnBoundTrait::Serialize
        | OnBoundTrait::Deserialize
        | OnBoundTrait::WireNumbered
        | OnBoundTrait::Default
        | OnBoundTrait::Ref
        | OnBoundTrait::RefMut
        | OnBoundTrait::Inspect => None,
    }
}

/// An associated type paired with the trait that *declares* it: a subtrait's
/// default body may name a supertrait's, and keying the projection to the
/// dispatched-through trait made `<T as Base>::Elem` and `Derived`'s two types.
type DeclaredAssocType = (DefId, ast::AssociatedTypeDecl);

impl<H: CompilerHost> Elaborator<'_, H> {
    /// The trait declaration a reference site names, from
    /// [`crate::resolve::Resolutions`] and so resolved in the writing module: an
    /// alias or a second module's same-named trait cannot displace it.
    ///
    /// `written` feeds the fallback only, for a site answering with something that
    /// is no trait at all — a same-named enum case in the prelude.
    pub(super) fn trait_decl_at(&self, site: AstId, written: &str) -> Option<DefId> {
        if let Some(def) = self.tysys.resolutions.declared(site)
            && self.tysys.trait_env.declares_trait(&def)
        {
            return Some(def);
        }
        self.decl_key_or_local(written)
    }

    /// The recorded signature of `method_name` on the trait `trait_name` names
    /// in this frame, with the associated-type declarations its body may name —
    /// the trait's own and every supertrait's, since `Self::Elem` in a
    /// `Derived` default body is `Base`'s.
    ///
    /// The header answers whether the method exists, the digest what it says.
    /// A method the header lists but the digest lacks is a decl-pass bug, so it
    /// panics rather than reading as "no such method".
    fn trait_method_of(
        &self,
        key: &DefId,
        method_name: &str,
    ) -> Option<(sig::MethodSig, Vec<DeclaredAssocType>)> {
        let header = self.tysys.trait_decl_header_of(key)?;
        if !header.methods.iter().any(|m| m.name == method_name) {
            return None;
        }
        let mut assoc_types: Vec<DeclaredAssocType> = header
            .assoc_types
            .iter()
            .map(|decl| (*key, decl.clone()))
            .collect();
        let inherited: Vec<DeclaredAssocType> = self
            .tysys
            .trait_env
            .supertrait_decls(key)
            .filter_map(|decl| Some((decl, self.tysys.trait_decl_header_of(&decl)?)))
            .flat_map(|(decl, super_header)| {
                super_header
                    .assoc_types
                    .iter()
                    .map(move |a| (decl, a.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for entry in inherited {
            if !assoc_types.iter().any(|(_, own)| own.name == entry.1.name) {
                assoc_types.push(entry);
            }
        }
        let sig = self
            .tysys
            .trait_sig_of(key)
            .and_then(|sig| sig.method(method_name))
            .expect("the decl pass records every trait method's signature")
            .sig
            .clone();
        Some((sig, assoc_types))
    }

    /// What `Self::X` means for a receiver reached through a trait bound, for
    /// each associated type `X` the trait declares.
    ///
    /// The declaration cannot say: `I: IntoIterator<Item = u8>` is written at
    /// the caller. Every name gets an answer, because the projection over the
    /// receiver carries the caller's bindings and instantiating the recorded
    /// one would not.
    fn trait_assoc_answers(
        &mut self,
        assoc_types: &[DeclaredAssocType],
        self_type_id: TypeId,
    ) -> Vec<(String, TypeId)> {
        let self_name = match self.tysys.type_table.borrow().get(self_type_id) {
            ResolvedType::TypeParam { name, .. } => name.clone(),
            _ => String::new(),
        };
        let mut answers = Vec::with_capacity(assoc_types.len());
        for (declaring, decl) in assoc_types {
            let known = self.frame_projection(self_type_id, &self_name, &decl.name);
            let answer = known.unwrap_or_else(|| {
                self.make_frame_projection_of_trait(
                    self_type_id,
                    &self_name,
                    *declaring,
                    &decl.name,
                )
            });
            answers.push((decl.name.clone(), answer));
        }
        answers
    }

    /// Find a method in the trait declarations the bound names give, read in
    /// elaborated form: `T: Ord` searches `Ord` and its supertraits. `Self` is
    /// substituted by the `TypeParam`'s type. More than one bound declaring the
    /// name is ambiguous — reported, then resolved to the first.
    ///
    /// `required_trait`: the trait a qualified call named, which alone may
    /// answer. Matched against the *elaborated* bounds, so a supertrait of a
    /// written bound qualifies.
    pub(super) fn find_method_in_trait_bounds(
        &mut self,
        bounds: &[ScopedBound],
        method_name: &str,
        self_type_id: TypeId,
        span: Span,
        required_trait: Option<&RequiredTrait>,
        args: ArgSource<'_, '_>,
    ) -> Option<(FqTraitName, MethodInfo)> {
        self.find_method_in_trait_bounds_with(
            bounds,
            &IndexMap::default(),
            method_name,
            self_type_id,
            span,
            required_trait,
            args,
        )
    }

    /// The space `elaborated`'s written types are read in: empty for a bound the
    /// frame wrote itself, and for an inherited one the chain from what `bounds`
    /// writes down to the trait that declared it.
    fn bound_space(&mut self, bounds: &[ScopedBound], elaborated: &ElaboratedBound) -> ParamSpace {
        let Some((root, via)) = elaborated.inherited.clone() else {
            return ParamSpace::new();
        };
        let root_args = self.trait_args_of_bound(bounds, root);
        self.inherited_space(root, &root_args, &via)
    }

    /// [`Self::bound_slots`] read in the space the candidate's bound was written in.
    fn bound_slots_in_space(
        &mut self,
        candidate: &BoundCandidate,
        self_type_id: TypeId,
    ) -> IndexMap<u32, TypeId> {
        let BoundCandidate {
            bound,
            space,
            decl,
            written_self,
        } = candidate;
        let (bound, decl, written_self) = (bound.clone(), *decl, *written_self);
        self.in_space(space, |e| {
            e.bound_slots(&bound, decl, self_type_id, written_self)
        })
    }

    /// One bound per trait declaration. Several bounds on one trait are one
    /// method at several instantiations, which the call's arguments choose.
    fn one_bound_per_trait(
        &mut self,
        candidates: Vec<BoundCandidate>,
        method_name: &str,
        self_type_id: TypeId,
        args: ArgSource<'_, '_>,
    ) -> Vec<BoundCandidate> {
        let mut groups: Vec<Vec<BoundCandidate>> = Vec::new();
        for candidate in candidates {
            match groups
                .iter_mut()
                .find(|group| group[0].decl == candidate.decl)
            {
                Some(group) => group.push(candidate),
                None => groups.push(vec![candidate]),
            }
        }
        let mut args = args;
        groups
            .into_iter()
            .map(|group| {
                self.select_instantiation(group, method_name, self_type_id, args.reborrow())
            })
            .collect()
    }

    /// The instantiation of one trait that the call's arguments admit. The
    /// bound writing arguments wins where nothing selects, a bare one beside it
    /// naming only the declared default.
    fn select_instantiation(
        &mut self,
        group: Vec<BoundCandidate>,
        method_name: &str,
        self_type_id: TypeId,
        args: ArgSource<'_, '_>,
    ) -> BoundCandidate {
        let written = group.iter().any(|c| !c.bound.type_args.is_empty());
        let mut args = args;
        if group.len() > 1 && written && !args.is_empty() {
            // Classification costs something, so it waits for an overload set,
            // as `select_trait_match` makes it wait (WEP 2026-07-31).
            let classes: Vec<ArgClass> = (0..args.len()).map(|i| args.class(self, i)).collect();
            let admitted: Vec<usize> = (0..group.len())
                .filter(|&i| {
                    let Some(params) = self.bound_param_types(&group[i], method_name, self_type_id)
                    else {
                        return false;
                    };
                    classes.len() <= params.len()
                        && classes
                            .iter()
                            .zip(params.iter())
                            .all(|(class, &param)| self.class_admits(param, class))
                })
                .collect();
            // One exact answer decides on its own. A parameter that is the
            // caller’s own type parameter is a bare slot to `class_admits`,
            // which admits every argument, so without this tier an exactly
            // matching instantiation never wins over a slot-shaped sibling.
            let exact: Vec<usize> = admitted
                .iter()
                .copied()
                .filter(|&i| self.exactly_admits(&group[i], method_name, self_type_id, &classes))
                .collect();
            if let [winner] = exact.as_slice() {
                let mut group = group;
                return group.swap_remove(*winner);
            }
            // Unique-or-nothing, as among impls: several admitted candidates
            // select none, and the written bound answers below.
            if let [winner] = admitted.as_slice() {
                let mut group = group;
                return group.swap_remove(*winner);
            }
        }
        let first_written = group
            .iter()
            .position(|c| !c.bound.type_args.is_empty())
            .unwrap_or(0);
        let mut group = group;
        group.swap_remove(first_written)
    }

    /// Whether every class names exactly what the candidate takes there, with
    /// no slot standing in. Argument passing’s one coercion still applies.
    fn exactly_admits(
        &mut self,
        candidate: &BoundCandidate,
        method_name: &str,
        self_type_id: TypeId,
        classes: &[ArgClass],
    ) -> bool {
        let Some(params) = self.bound_param_types(candidate, method_name, self_type_id) else {
            return false;
        };
        let tt = self.tysys.type_table.borrow();
        classes.len() == params.len()
            && classes.iter().zip(params.iter()).all(|(class, &param)| {
                matches!(class, ArgClass::Exact(arg) if param_takes(&tt, param, *arg))
            })
    }

    /// The value parameters `method_name` takes under `candidate`, for selection.
    fn bound_param_types(
        &mut self,
        candidate: &BoundCandidate,
        method_name: &str,
        self_type_id: TypeId,
    ) -> Option<Vec<TypeId>> {
        let decl = candidate.decl;
        let (sig, trait_assoc_types) = self.trait_method_of(&decl, method_name)?;
        let answers = self.trait_assoc_answers(&trait_assoc_types, self_type_id);
        let slots = self.bound_slots_in_space(candidate, self_type_id);
        let instantiated = sig.decl.instantiate_slots_with(
            &self.tysys.type_table,
            &slots,
            &SlotProjections::from_iter([(0, answers)]),
        );
        let first_value_param = sig.first_value_param().min(instantiated.param_types.len());
        Some(instantiated.param_types[first_value_param..].to_vec())
    }

    /// [`Self::find_method_in_trait_bounds`] for bounds that carry no reference
    /// site of their own.
    ///
    /// `known` maps a bound's own id to the declaration it means, answered
    /// where the bound was first read. An associated-type projection is the
    /// case: it outlives the trait declaration's frame, so it records the
    /// identities and hands them back here. Keyed by id rather than by name so
    /// two same-named traits stay two bounds.
    pub(super) fn find_method_in_trait_bounds_with(
        &mut self,
        bounds: &[ScopedBound],
        known: &IndexMap<AstId, FqTraitName>,
        method_name: &str,
        self_type_id: TypeId,
        span: Span,
        required_trait: Option<&RequiredTrait>,
        args: ArgSource<'_, '_>,
    ) -> Option<(FqTraitName, MethodInfo)> {
        let elaborated = self.elaborate_bounds_with(bounds, known);
        // Which trait each bound means is settled once, here: a bound reached
        // through a supertrait was written in the *declaring* module, so
        // resolving its spelling in this frame would miss an aliased one.
        let keyed: Vec<(ElaboratedBound, DefId)> = elaborated
            .iter()
            .filter_map(|b| {
                // A synthesised bound carries its referent, so it is read off
                // the bound rather than looked up by an id the walk never saw.
                let key = b
                    .bound
                    .resolved
                    .or_else(|| known.get(&b.bound.id).and_then(FqTraitName::canonical))
                    .or_else(|| self.trait_decl_at(b.bound.id, &b.bound.name));
                key.map(|key| (b.clone(), key))
            })
            .collect();
        // Stopping at the first hit would hide the ambiguity, so every bound is
        // scanned — by predicate, leaving only the winner to clone.
        // A qualified call names one bound, so the others are not competitors.
        // The filter runs *after* elaboration: `T: Derived` carries `Base`, so
        // `Base::tag(x)` names a supertrait the frame never wrote. Comparing
        // declarations, not spellings, keeps another module's same-named trait
        // from answering for the one the call named.
        let kept: Vec<(ElaboratedBound, DefId)> = keyed
            .into_iter()
            .filter(|(_, key)| {
                required_trait.is_none_or(|w| match w.decl {
                    // A binder or an unreached name declares no trait, so it
                    // competes with none — which is what the fabricated key
                    // amounted to.
                    Resolution::Def(def) => def == *key,
                    _ => false,
                }) && self
                    .tysys
                    .trait_method_header_of(key, method_name)
                    .is_some()
            })
            .collect();
        // The space each bound's written types are read in, settled once here:
        // every reader below resolves in it rather than re-spelling.
        let mut candidates = Vec::with_capacity(kept.len());
        for (elaborated, decl) in kept {
            let space = self.bound_space(bounds, &elaborated);
            candidates.push(BoundCandidate {
                bound: elaborated.bound,
                space,
                decl,
                written_self: elaborated.self_type,
            });
        }
        let mut candidates = self.one_bound_per_trait(candidates, method_name, self_type_id, args);
        // A reserved name answers only where no method of its kind does.
        let header = |c: &BoundCandidate| self.tysys.trait_method_header_of(&c.decl, method_name);
        let answers = |receiver: bool| {
            candidates
                .iter()
                .any(|c| header(c).is_some_and(|m| !m.is_reserved && m.has_receiver == receiver))
        };
        let answered = [answers(false), answers(true)];
        candidates.retain(|c| {
            header(c).is_none_or(|m| !m.is_reserved || !answered[usize::from(m.has_receiver)])
        });
        let resolved = candidates.first().and_then(|candidate| {
            self.trait_method_of(&candidate.decl, method_name)
                .map(|found| (candidate.clone(), found))
        });
        if candidates.len() > 1 {
            // Two candidates can share a spelling; reporting both as "Base"
            // names no escape from the collision.
            let ambiguous_spelling = |c: &BoundCandidate| {
                candidates
                    .iter()
                    .any(|other| other.bound.name == c.bound.name && other.decl != c.decl)
            };
            let traits = candidates
                .iter()
                .map(|c| {
                    if ambiguous_spelling(c) {
                        format!(
                            "{}::{}",
                            self.tysys.resolutions.defs().module(c.decl),
                            c.bound.name
                        )
                    } else {
                        c.bound.name.clone()
                    }
                })
                .collect();
            // Keep going with the first candidate: `None` reads to the caller
            // as "no such method", which it would then report as well.
            let _ = self.emit(TypeError::AmbiguousTraitMethod {
                method: method_name.to_string(),
                traits,
                span,
            });
        }
        let (candidate, (sig, trait_assoc_types)) = resolved?;
        let BoundCandidate {
            bound,
            space,
            decl,
            written_self,
        } = candidate;
        // The bound answers with the trait its own reference site resolves to,
        // not the spelling it wrote: an aliased bound (`T: G` for
        // `use { Greet as G }`) must reach the impl that defines the method.
        let fq_trait_name = known
            .get(&bound.id)
            .cloned()
            .unwrap_or_else(|| FqTraitName::declared(self.tysys.resolutions.defs(), decl));
        // The arguments the bound writes name the trait the way the impl that
        // answers it is named, so `T: Eq<String>` reaches `impl Eq<String>`.
        let fq_trait_name = self.tysys.trait_env.fq_trait_named_by_bound(
            fq_trait_name,
            &bound,
            &self.tysys.resolutions,
        );
        // An inherited bound wrote its arguments in the declaring trait's
        // parameter space, so a spelling read here means the wrong binder.
        let pick = args_to_resolve(&bound);
        let fq_trait_name = self.in_space(&space, |e| {
            e.trait_named_with_resolved_args(fq_trait_name, &bound, &pick)
        });

        let answers = self.trait_assoc_answers(&trait_assoc_types, self_type_id);
        let slots = self.in_space(&space, |e| {
            e.bound_slots(&bound, decl, self_type_id, written_self)
        });
        let fq_trait_name = self
            .tysys
            .trait_named_from_slots(fq_trait_name, &bound, &slots);
        let instantiated = sig.decl.instantiate_slots_with(
            &self.tysys.type_table,
            &slots,
            &SlotProjections::from_iter([(0, answers)]),
        );
        let first_value_param = sig.first_value_param().min(instantiated.param_types.len());

        Some((
            fq_trait_name,
            MethodInfo {
                // A bare bound dispatches on the parameter itself, off no
                // `impl` block.
                impl_type_bindings: Vec::new(),
                method_def: Some(sig.def),
                return_type: instantiated.return_type,
                self_kind: sig.self_kind,
                param_types: instantiated.param_types[first_value_param..].to_vec(),
                param_is_mut: Param::is_mut_flags(&sig.params),
                owner: MethodOwner::Receiver,
                cm_name: None,
                is_ref_impl: false,
                method_type_param_ids: sig.own_type_param_ids(),
                method_own_params: sig.own_params.clone(),
                impl_module: None,
                from_concrete_impl: false,
                param_defaults: Param::defaults(&sig.params),
                param_names: Param::names(&sig.params),
                consumes_self: sig.self_kind == ast::SelfKind::Value,
                inherent_visibility: None,
                defaults_module: sig.defaults_module.clone(),
            },
        ))
    }
}

impl TypeSystem {
    /// The header of `method_name` on the trait `key` names. The cheap form of
    /// [`Self::trait_method_of`], for counting candidates without cloning each
    /// one's declaration.
    fn trait_method_header_of(&self, key: &DefId, method_name: &str) -> Option<&ImplMethodHeader> {
        self.trait_decl_header_of(key)?
            .methods
            .iter()
            .find(|m| m.name == method_name)
    }
}

impl TypeSystem {
    /// The recorded signature of an already-identified trait.
    ///
    /// Every by-name form funnels through this one. Flattening a key back to
    /// its declared name and resolving that again is what broke an aliased
    /// head: the module imported `Alpha as Ay` and never `Alpha`, so the
    /// second resolution found nothing.
    pub(super) fn trait_sig_of(&self, key: &DefId) -> Option<&TraitSig> {
        if !self.trait_env.declares_trait(key) {
            return None;
        }
        self.signatures.trait_sig(*key)
    }
}

impl TypeSystem {
    /// The types a pack parameter's bound actually falls on.
    ///
    /// A pack is instantiated with the tuple that carries its elements, so the
    /// bound is checked element-wise. A non-tuple argument is a pack of one.
    pub(super) fn pack_elements(&self, type_arg: TypeId) -> Vec<TypeId> {
        match self.type_table.borrow().get(type_arg) {
            ResolvedType::GenericInstance { def, type_args }
                if TypeTable::is_tuple_type(self.type_table.borrow().def_name(*def)) =>
            {
                type_args.clone()
            }
            _ => vec![type_arg],
        }
    }
    /// Whether what the receiver substitutes for an `impl` block's parameters
    /// satisfies their bounds: `impl<T: Ord> List<T>` wants an `Ord` element.
    pub(super) fn check_impl_block_bounds(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_params: &[ast::GenericParam],
        impl_ty: &ast::Type,
        receiver: Option<TypeId>,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        // No type params with bounds → always OK
        if type_params.iter().all(|p| p.bounds.is_empty()) {
            return true;
        }

        // The bound's own site says which trait it names and what it writes for
        // that trait's parameters, so the check compares declarations rather
        // than the spelling the impl header happened to write.
        let bounds_map: IndexMap<&str, Vec<FqTraitName>> = type_params
            .iter()
            .filter(|p| !p.bounds.is_empty())
            .map(|p| {
                (
                    p.name.as_str(),
                    p.bounds
                        .iter()
                        .filter_map(|b| self.bound_written(b))
                        .collect(),
                )
            })
            .collect();

        // `impl<T: Bound> Trait for &T` writes no position, so `T` stands for
        // the receiver's pointee rather than for an argument of it.
        if let ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) = impl_ty
            && let ast::Type::Named(inner) = boxed.as_ref()
        {
            let Some(bounds) = bounds_map.get(inner.name.as_str()) else {
                return true;
            };
            let Some(pointee) = receiver.and_then(|id| self.pointee_of(id)) else {
                return true;
            };
            return self.bounds_hold(ctx, scope, pointee, bounds);
        }

        let Some(type_args) = type_args else {
            // An existence or bounds check that threaded no positions has
            // nothing to compare against.
            return true;
        };

        if let ast::Type::Generic(generic) = impl_ty {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg
                    && let Some(bounds) = bounds_map.get(named.name.as_str())
                    && let Some(&type_arg) = type_args.get(i)
                    && !self.bounds_hold(ctx, scope, type_arg, bounds)
                {
                    return false;
                }
            }
        } else if let ast::Type::Tuple(elements) = impl_ty {
            // Variadic tuple impl (`impl<..T: Trait> Trait for [..T]`, e.g.
            // `Eq`/`Ord` for tuples in core:prelude/tuple.wado): every entry
            // in `type_args` instantiates the same variadic parameter, so
            // each is checked against its bounds.
            for elem in elements {
                let ast::Type::TypePackSpread(name, _) = elem else {
                    continue;
                };
                let Some(bounds) = bounds_map.get(name.as_str()) else {
                    continue;
                };
                for &type_arg in type_args {
                    if !self.bounds_hold(ctx, scope, type_arg, bounds) {
                        return false;
                    }
                }
            }
        }

        true
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Check trait bounds on a generic function's type arguments.
    ///
    /// A type argument list is dense: an effect parameter and a `fn`-bound one
    /// hold no slot in it (see [`ast::GenericParam::is_real_type_param`], the
    /// filter inference and projection pair by). Zipping the declared list
    /// instead would shift every parameter after one of those onto the next
    /// argument's bounds.
    pub(super) fn check_function_type_arg_bounds(
        &mut self,
        callee: &CalleeRef,
        type_args: &[TypeId],
        span: Span,
    ) {
        let type_params: Vec<ast::GenericParam> = self
            .lookup_function_type_params(callee)
            .into_iter()
            .filter(ast::GenericParam::is_real_type_param)
            .collect();
        self.enforce_type_arg_bounds(&type_params, type_args, None, span);
    }

    /// Check the bounds on a generic type declaration's type arguments, for
    /// every `struct`, `variant` and generic newtype instantiation.
    pub(super) fn check_type_decl_arg_bounds(
        &mut self,
        def: DefId,
        type_args: &[TypeId],
        span: Span,
    ) {
        let Some(params) = self
            .type_lookup()
            .declared_generic_params(def)
            .map(<[ast::GenericParam]>::to_vec)
        else {
            return;
        };
        self.enforce_type_arg_bounds(&params, type_args, None, span);
    }

    /// The single enforcement of trait bounds on a generic decl's type args,
    /// shared by every generic-call kind so the rule cannot drift. Only a fully
    /// concrete arg is enforced, and `self_binding` is what a bound's
    /// `Self::Assoc` projects off where the call binds one.
    pub(super) fn enforce_type_arg_bounds(
        &mut self,
        params: &[ast::GenericParam],
        type_args: &[TypeId],
        self_binding: Option<SelfBinding>,
        span: Span,
    ) {
        let at_call = self.tysys.call_site_types(params, type_args);
        for (i, param) in params.iter().enumerate() {
            let Some(&type_arg) = type_args.get(i) else {
                continue;
            };
            if self.tysys.type_table.borrow().contains_type_param(type_arg) {
                // Also covers holes (reserved-index params), re-checked at finalize.
                continue;
            }
            // `..T: Foo` binds every element of the pack, not the tuple that
            // carries them: `f<..T: Foo>([1, "x"])` asks `i32: Foo` and
            // `String: Foo`, never `[i32, String]: Foo` — which would be a
            // question about a variadic impl of `Foo` for tuples.
            let subjects = if param.is_pack {
                self.tysys.pack_elements(type_arg)
            } else {
                vec![type_arg]
            };
            for bound in &param.real_bounds() {
                let written = self
                    .tysys
                    .bound_written(bound)
                    .map(|trait_| {
                        self.bound_trait_at_args(trait_, bound, params, type_args, self_binding)
                    })
                    .and_then(|trait_| asked_at(trait_, &at_call));
                for &subject in &subjects {
                    self.enforce_single_bound_args(
                        subject,
                        &bound.name,
                        written.as_ref(),
                        &param.name,
                        span,
                    );
                    self.enforce_assoc_type_bounds(subject, bound, self_binding, span);
                }
            }
            // A supertrait failure has the same one cause as the bound that
            // implied it, so it is asked but not reported — asking is what
            // drives the derivation that makes `T: Ord` alone satisfy `Eq`.
            for (bound, root, via) in self.inherited_bounds_of(&param.bounds) {
                // Which trait a direct bound names, not how it spells it: two
                // modules may call one name two traits. One that names no
                // declaration falls back to its spelling, as `merge_bound` does.
                let declared = bound
                    .resolved
                    .or_else(|| self.trait_decl_at(bound.id, &bound.name));
                let already_direct = |b: &ast::TraitBound| match declared {
                    Some(decl) => self.trait_decl_at(b.id, &b.name) == Some(decl),
                    None => b.name == bound.name,
                };
                if param.bounds.iter().any(already_direct) {
                    continue;
                }
                // The clause is written in the trait that declared it, which the
                // chain from the direct bound's own arguments reaches. Those
                // arguments are the declaration's, so they are read with the
                // call's own answers in scope — `U: Uses<P::Inner>` asks what
                // the argument for `P` binds `Inner` to.
                let site: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                let bounds = ScopedBound::pin_declared(param, self_binding);
                let root_args = self.with_type_params_bound(&site, type_args, |e| {
                    e.trait_args_of_bound(&bounds, root)
                });
                let space = self.inherited_space(root, &root_args, &via);
                let pick = args_to_resolve(&bound);
                let subjects = subjects.clone();
                self.in_space(&space, |e| {
                    let inherited = e
                        .tysys
                        .bound_written(&bound)
                        .map(|trait_| e.trait_named_with_resolved_args(trait_, &bound, &pick));
                    for subject in subjects {
                        if let Some(trait_) = inherited
                            .clone()
                            .and_then(|trait_| asked_at(trait_, &at_call))
                        {
                            e.check_and_register_bound(subject, &trait_);
                        }
                        e.enforce_assoc_type_bounds(subject, &bound, self_binding, span);
                    }
                });
            }
        }
    }

    /// What the bound on `bounds` naming `root` writes for `root`'s own
    /// parameters, resolved here. Empty where none of them names it.
    fn trait_args_of_bound(&mut self, bounds: &[ScopedBound], root: DefId) -> Vec<TypeId> {
        let Some(found) = bounds
            .iter()
            .find(|bound| self.trait_decl_at(bound.id, &bound.name) == Some(root))
            .cloned()
        else {
            return Vec::new();
        };
        self.in_bound_frame(&found, &ParamSpace::new(), |e| {
            found
                .type_args
                .iter()
                .map(|ty| e.resolve_type(ty))
                .collect()
        })
    }

    /// Whether one concrete type argument meets one trait bound — the primitive
    /// every enforcement path funnels through. Registers the associated types on
    /// success, raises `TraitBoundNotSatisfied` on failure.
    pub(super) fn enforce_single_bound_args(
        &mut self,
        type_arg: TypeId,
        trait_name: &str,
        trait_: Option<&FqTraitName>,
        param_name: &str,
        span: Span,
    ) -> bool {
        // A bound whose site names no declaration cannot be enforced against an
        // identity; the unresolved name is diagnosed where it was written.
        let Some(trait_) = trait_ else {
            return true;
        };
        if self.check_and_register_bound(type_arg, trait_) {
            return true;
        }
        let type_name = self.tysys.type_id_to_string(type_arg);
        let reason = self.tysys.trait_unimpl_reason_chain(
            &self.annotate_ctx,
            &self.type_lookup(),
            type_arg,
            trait_name,
        );
        let _ = self.emit(TypeError::TraitBoundNotSatisfied {
            type_name,
            trait_name: bound_as_written(trait_name, trait_),
            param_name: param_name.to_string(),
            reason,
            span,
            unmet: BoundUnmet(()),
        });
        false
    }

    /// Report that type parameter `param` carries no bound supplying
    /// `trait_name`, which is how an operator reaches one.
    pub(super) fn report_operator_bound_missing(
        &mut self,
        param: &str,
        trait_name: &str,
        span: Span,
    ) {
        let _ = self.emit(TypeError::TraitBoundNotSatisfied {
            type_name: param.to_string(),
            trait_name: trait_name.to_string(),
            param_name: param.to_string(),
            reason: Vec::new(),
            span,
            unmet: BoundUnmet(()),
        });
    }

    /// What a bound binds `decl`'s slots to: slot 0 is `Self`, and the trait's
    /// own parameters what it writes (`T: Eq<String>`), else their defaults.
    pub(super) fn bound_slots(
        &mut self,
        bound: &ast::TraitBound,
        decl: DefId,
        self_type_id: TypeId,
        written_self: BoundSelf,
    ) -> IndexMap<u32, TypeId> {
        let mut slots = IndexMap::from_iter([(0, self_type_id)]);
        let Some(trait_params) = self.tysys.trait_decl_type_params_of(&decl) else {
            return slots;
        };
        // Slot 0 is the trait's `Self`. An argument the bound wrote means
        // whatever wrote it; a defaulted one (`Eq<Rhs = Self>`) is written in
        // the trait's space and means the bounded type.
        let written: Vec<(u32, ast::Type, BoundSelf)> =
            trait_params_from_impl(&trait_params, &bound.type_args, None)
                .into_iter()
                .filter(|supplied| supplied.takes_a_slot)
                .filter_map(|supplied| match supplied.arg {
                    Some(ty) => Some((supplied.slot, ty.clone(), written_self)),
                    None => supplied
                        .param
                        .default
                        .as_ref()
                        .map(|ty| (supplied.slot, ty.clone(), BoundSelf::Bounded)),
                })
                .collect();
        for (slot, ty, scope) in written {
            let resolved =
                self.under_self_binding(scope.at(self_type_id, decl), |s| s.resolve_type(&ty));
            slots.insert(slot, resolved);
        }
        slots
    }

    /// Check a bound's associated-type constraints (`T: Collect<Item = i32>`)
    /// against the type argument. Runs after [`Self::enforce_single_bound_args`],
    /// which is what registers the argument's bindings.
    fn enforce_assoc_type_bounds(
        &mut self,
        type_arg: TypeId,
        bound: &ast::TraitBound,
        self_binding: Option<SelfBinding>,
        span: Span,
    ) {
        for constraint in &bound.assoc_types {
            // `Self` in a constraint means the receiver the call names — how
            // `collect<C: FromIterator<Elem = Self::Item>>` says what `C`
            // collects. With no receiver to bind it there is nothing to check
            // here; `enforce_impl_assoc_type_bounds` owns those.
            let mentions_self = constraint.ty.mentions("Self");
            let binding = self_binding.filter(|_| mentions_self);
            if (binding.is_none() && mentions_self) || mentions_type_pack(&constraint.ty) {
                continue;
            }
            // The bound's own site says which trait declares the constraint.
            let trait_key = self
                .tysys
                .fq_trait_name_at(bound.id, &bound.name)
                .canonical();
            let Some(actual) = trait_key.and_then(|key| {
                self.tysys.type_table.borrow().resolve_assoc_type_of_trait(
                    type_arg,
                    &key,
                    &constraint.name,
                )
            }) else {
                continue;
            };
            let expected = match binding {
                // Every `Self::Assoc` it spells must project before the
                // resolver runs, which reports what it cannot resolve: a bound
                // nothing answers goes unchecked rather than reported.
                Some(binding) if self.projects_off_self(&constraint.ty, binding) => {
                    self.with_self_binding(binding, |e| e.resolve_type(&constraint.ty))
                }
                Some(_) => continue,
                None => self.resolve_type(&constraint.ty),
            };
            let tt = self.tysys.type_table.borrow();
            if tt.contains_type_param(expected)
                || tt.contains_type_param(actual)
                || satisfies(&tt, expected, actual)
            {
                continue;
            }
            let (expected_name, actual_name) = (
                self.tysys.type_id_to_string(expected),
                self.tysys.type_id_to_string(actual),
            );
            drop(tt);
            let type_name = self.tysys.type_id_to_string(type_arg);
            let _ = self.emit(TypeError::AssocTypeBoundNotSatisfied {
                type_name,
                trait_name: bound.name.clone(),
                assoc_name: constraint.name.clone(),
                expected: expected_name,
                actual: actual_name,
                span,
            });
        }
    }

    /// Whether every `Self::Assoc` written anywhere in `ty` projects off
    /// `binding`. A `Self` head with arguments never does: nothing spells one.
    fn projects_off_self(&mut self, ty: &ast::Type, binding: SelfBinding) -> bool {
        match ty {
            ast::Type::NamespacedGeneric(ns) => {
                (ns.namespace != "Self" || self.project_off_self(binding, &ns.name).is_some())
                    && self.all_project_off_self(&ns.args, binding)
            }
            ast::Type::Generic(generic) => {
                generic.name != "Self" && self.all_project_off_self(&generic.args, binding)
            }
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
                self.projects_off_self(inner, binding)
            }
            ast::Type::Tuple(elements) => self.all_project_off_self(elements, binding),
            _ => true,
        }
    }

    fn all_project_off_self(&mut self, types: &[ast::Type], binding: SelfBinding) -> bool {
        types.iter().all(|ty| self.projects_off_self(ty, binding))
    }

    /// `Self::assoc` where `Self` is `binding`'s receiver. The trait that wrote
    /// the constraint qualifies the lookup, and the unqualified rule answers
    /// where it declared the name but a supertrait's impl registered it.
    fn project_off_self(&mut self, binding: SelfBinding, assoc: &str) -> Option<TypeId> {
        let mut table = self.tysys.type_table.borrow_mut();
        if let Some(trait_) = binding.declaring_trait
            && let Some(resolved) =
                table.resolve_trait_assoc_type_of_instance(binding.type_id, &trait_, assoc)
        {
            return Some(resolved);
        }
        table.resolve_assoc_type_of_instance(binding.type_id, assoc)
    }

    /// Whether `type_arg` satisfies `trait_name`, registering its associated
    /// types when it does. Asking is what records an on-demand derivation
    /// request, so callers that do not report the answer still ask.
    pub(super) fn check_and_register_bound(
        &mut self,
        type_arg: TypeId,
        trait_: &FqTraitName,
    ) -> bool {
        if !self.tysys.type_implements_trait(
            &self.annotate_ctx,
            &self.type_lookup(),
            type_arg,
            trait_,
        ) {
            return false;
        }
        let Some(decl) = trait_.canonical() else {
            return true;
        };
        self.register_assoc_types_for_concrete_type_and_trait(type_arg, decl);
        true
    }

    /// Register associated type resolutions for a concrete type instantiating a trait.
    /// For example, when `List<u8>` implements `IntoIterator`, registers:
    /// - (List<u8>, "Item") → u8
    /// - (List<u8>, "Iter") → `ListIter`<u8>
    ///
    /// This enables the monomorphizer to resolve `I::Iter` → `ListIter<u8>` when `I = List<u8>`.
    pub(super) fn register_assoc_types_for_concrete_type_and_trait(
        &mut self,
        concrete_type_id: TypeId,
        trait_: DefId,
    ) {
        // Get the base type name and concrete type args for impl block lookup.
        // For newtypes, follow the chain to the underlying type to find the trait impl,
        // but registration (below) still uses concrete_type_id so the monomorphizer can
        // resolve e.g. `MyBytes::Iter` when `MyBytes` is a newtype over `List<u8>`.
        let (type_name, concrete_type_args) = {
            let tt = self.tysys.type_table.borrow();
            let effective_id = tt.representation_head(concrete_type_id);
            let list_name = tt.compiler_struct_fq_name(CompilerItem::List);
            match tt.get(effective_id).clone() {
                ResolvedType::GenericInstance { type_args, .. } => {
                    (tt.fq_type_name(effective_id).head_only(), type_args)
                }
                // Primitives (`i32`, `f64`, `bool`, ...) can implement traits
                // with associated types just like structs. Without this arm,
                // a generic call like `parse_range::<i32>(...)` would skip
                // the `i32::Err = ParseIntError` registration and leave
                // `T::Err` unresolved at the caller's binding site.
                ResolvedType::Struct { .. } | ResolvedType::Primitive(_) => {
                    (tt.fq_type_name(effective_id), vec![])
                }
                ResolvedType::BuiltinArray(elem) => (list_name, vec![elem]),
                _ => return,
            }
        };

        // Collect matching impl block info (avoids borrow conflicts during resolution)
        struct ImplInfo {
            /// Each declared parameter and where the target names it, so the
            /// instantiation binds it by position. `None` where the target
            /// does not name it, leaving bounds with no binder to pair with.
            type_params: Vec<(ast::GenericParam, Option<u32>)>,
            assoc_types: Vec<ast::AssociatedTypeBinding>,
            /// The trait this block implements, as its own header names it —
            /// the key the registration must use.
            trait_key: DefId,
            /// The written trait reference and the impl's target, so the
            /// registration can name the instantiation the block implements.
            trait_type: ast::Type,
            target: ast::Type,
        }
        let trait_env = self.tysys.trait_env.clone();
        let impl_infos: Vec<ImplInfo> = {
            let mut result = vec![];
            {
                let entries = trait_env.entries_by_receiver_vec(&Receiver::Type(type_name));
                for entry in entries {
                    let Some(header) = trait_env.impl_headers.get(&entry) else {
                        continue;
                    };
                    if header.trait_def() == Some(trait_) && !header.associated_types.is_empty() {
                        // The receiver is keyed by head, so every impl on
                        // `List<_>` answers here, including ones implementing
                        // an instantiation this one contradicts.
                        if !self
                            .tysys
                            .inherent_impl_type_args_match(&header.ty, Some(&concrete_type_args))
                        {
                            continue;
                        }
                        let slots = ImplParamSlots::of(&header.ty, &header.type_params);
                        let type_params: Vec<(ast::GenericParam, Option<u32>)> = header
                            .type_params
                            .iter()
                            .map(|param| (param.clone(), slots.of_name(&param.name)))
                            .collect();
                        let Some(trait_key) = header
                            .fq_trait(&self.tysys.resolutions)
                            .and_then(|t| t.canonical())
                        else {
                            continue;
                        };
                        let Some(trait_type) = header.trait_ty().cloned() else {
                            continue;
                        };
                        result.push(ImplInfo {
                            type_params,
                            assoc_types: header.associated_types.clone(),
                            trait_key,
                            trait_type,
                            target: header.ty.clone(),
                        });
                    }
                }
            }
            result
        };

        for info in impl_infos {
            let mut scope = self.enter_inherited_type_param_scope();

            // `Self` in `type Output = Self;` is the type being registered for,
            // not whatever the enclosing frame was implementing — and the trait
            // it projects `Self::Assoc` off is the one this block implements.
            let implementing = SelfBinding {
                type_id: concrete_type_id,
                declaring_trait: Some(info.trait_key),
            };
            scope.set_self_binding(implementing);

            // `impl<T> IntoIterator for List<T>` registering for `List<u8>`
            // binds `T` to `u8`: the slot is where the target names it.
            for (param, slot) in &info.type_params {
                let bounds = ScopedBound::pin_declared(param, Some(implementing));
                let bound_to = slot.and_then(|slot| {
                    let &arg = concrete_type_args.get(slot as usize)?;
                    Some(BinderInScope::undeclared(slot, arg))
                });
                match bound_to {
                    Some(binder) => scope.bind_param(&param.name, binder, bounds),
                    None => scope.add_param_bounds(&param.name, bounds),
                }
            }

            // Resolve and register each associated type in this substituted context
            let trait_ref = scope.impl_trait_ref(&info.trait_type, &info.target, info.trait_key);
            for binding in &info.assoc_types {
                let resolved_id = scope.resolve_type(&binding.ty);
                if !scope
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_param(resolved_id)
                {
                    scope
                        .tysys
                        .type_table
                        .borrow_mut()
                        .register_assoc_type_resolution(
                            concrete_type_id,
                            trait_ref.clone(),
                            binding.name.clone(),
                            resolved_id,
                        );
                }
            }

            drop(scope);
        }

        // Also check blanket impls: `impl<I: Trait> OtherTrait for I`.
        // For example, `impl<I: Iterator> IntoIterator for I` applies to StrUtf8ByteIter.
        struct BlanketImplInfo {
            blanket_param_name: String,
            blanket_param_bounds: Vec<ast::TraitBound>,
            assoc_types: Vec<ast::AssociatedTypeBinding>,
            /// The trait this blanket implements, as its own header names it.
            trait_key: DefId,
        }
        let blanket_infos: Vec<BlanketImplInfo> = {
            let mut result = vec![];
            for blanket in trait_env.blanket_impls.get(&trait_).into_iter().flatten() {
                let Some(header) = trait_env.impl_headers.get(&blanket.def) else {
                    continue;
                };
                if header.associated_types.is_empty() {
                    continue;
                }
                let impl_type_name = get_type_name_static(&header.ty);
                let Some(blanket_param) = header
                    .type_params
                    .iter()
                    .find(|tp| tp.name == impl_type_name && !tp.bounds.is_empty())
                else {
                    continue;
                };
                // Check if the concrete type satisfies the blanket param's bounds
                let bounds_ok = blanket_param.bounds.iter().all(|bound| {
                    self.tysys.bound_written(bound).is_some_and(|trait_| {
                        self.tysys.type_implements_trait(
                            &self.annotate_ctx,
                            &self.type_lookup(),
                            concrete_type_id,
                            &trait_,
                        )
                    })
                });
                if bounds_ok {
                    let Some(trait_key) = header
                        .fq_trait(&self.tysys.resolutions)
                        .and_then(|t| t.canonical())
                    else {
                        continue;
                    };
                    result.push(BlanketImplInfo {
                        blanket_param_name: blanket_param.name.clone(),
                        blanket_param_bounds: blanket_param.bounds.clone(),
                        assoc_types: header.associated_types.clone(),
                        trait_key,
                    });
                }
            }
            result
        };

        for info in blanket_infos {
            let mut scope = self.enter_inherited_type_param_scope();

            // A blanket's target is its parameter, so the type registered for
            // is what both `Self` and that parameter stand for.
            let implementing = SelfBinding {
                type_id: concrete_type_id,
                declaring_trait: Some(info.trait_key),
            };
            scope.set_self_binding(implementing);
            scope.bind_param(
                &info.blanket_param_name,
                BinderInScope::undeclared(0, concrete_type_id),
                ScopedBound::pin_all(&info.blanket_param_bounds, Some(implementing)),
            );

            // Resolve and register each associated type
            let trait_key = info.trait_key;
            for binding in &info.assoc_types {
                let resolved_id = scope.resolve_type(&binding.ty);
                if !scope
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_param(resolved_id)
                {
                    scope
                        .tysys
                        .type_table
                        .borrow_mut()
                        .register_assoc_type_resolution(
                            concrete_type_id,
                            TraitRef::bare(trait_key),
                            binding.name.clone(),
                            resolved_id,
                        );
                }
            }

            drop(scope);
        }
    }

    /// Single entry point for resolving a trait method a binary operator
    /// dispatches to (Eq / Ord / Add / … / Shr), returning a fully-populated
    /// [`ResolvedTraitMethod`] with `rhs_type` already substituted so no caller
    /// can forget to wire it through. `struct_name` / `lookup_type_id` are the
    /// impl-lookup key — for a newtype, possibly the ultimate base.
    pub(super) fn resolve_trait_method_for_op(
        &mut self,
        struct_name: &str,
        lookup_type_id: TypeId,
        trait_: DefId,
        trait_name: &str,
        method_name: &str,
        is_type_param: bool,
        rhs: Option<&ArgClass>,
    ) -> Option<ResolvedTraitMethod> {
        // `Eq` and `Ord` fix their return types whatever a user impl writes, and
        // `find_arithmetic_trait_impl` would default `output_type` to the
        // receiver type absent a `type Output`.
        //
        // Retrying unselected is what leaves a lone `Eq<Self>` impl to
        // type-check the operand and report a mismatch as one.
        let auto_derive = self.tysys.auto_derive_by_trait(trait_name);
        let written = rhs
            .and_then(|rhs| {
                self.find_arithmetic_trait_impl(
                    struct_name,
                    lookup_type_id,
                    trait_,
                    method_name,
                    Some(rhs),
                )
            })
            .or_else(|| {
                self.find_arithmetic_trait_impl(
                    struct_name,
                    lookup_type_id,
                    trait_,
                    method_name,
                    None,
                )
            });
        let (info_trait_name, self_kind, param_types, return_type, impl_def) =
            if let Some(info) = written {
                let return_type = auto_derive.map_or(info.output_type, |(_, ty)| ty);
                let param_types = info.rhs_type.map(|t| vec![t]).unwrap_or_default();
                (
                    info.trait_name,
                    info.self_kind,
                    param_types,
                    return_type,
                    Some(info.impl_def),
                )
            } else if let Some((item, return_type)) = auto_derive
                && let Some(trait_) = self.tysys.compiler_trait(item)
                && self.tysys.type_implements_trait(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    lookup_type_id,
                    &trait_,
                )
            {
                let ref_self_ty = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::Ref(lookup_type_id));
                // Auto-derived: no `impl` block is written, so none is named.
                (
                    self.tysys.type_table.borrow().compiler_trait_fq(item),
                    ast::SelfKind::Ref,
                    vec![ref_self_ty],
                    return_type,
                    None,
                )
            } else {
                return None;
            };
        Some(ResolvedTraitMethod {
            // The block's own method where one is written; an auto-derived
            // match names no block and so no declaration.
            method_def: impl_def.and_then(|def| self.tysys.declared_method(def, method_name)),
            trait_name: info_trait_name,
            method_name: method_name.to_string(),
            impl_def,
            impl_name: struct_name.to_string(),
            impl_type_id: (!is_type_param).then_some(lookup_type_id),
            self_kind,
            return_type,
            param_types,
            is_type_param_receiver: is_type_param,
        })
    }

    /// Fallback for [`Self::find_trait_method_for_type`]: with no user-written
    /// impl of `trait_name::method_name` on an auto-derive-eligible type,
    /// synthesize a [`TraitMethodMatch`] with the receiver substituted into
    /// `Self`. Primitives are excluded, comparing via Wasm instructions. The
    /// method ↔ trait table is [`TypeSystem::auto_derive_by_method`].
    pub(super) fn try_auto_derived_method_match(
        &mut self,
        struct_name: &str,
        method_name: &str,
        receiver_type_id: TypeId,
    ) -> Option<TraitMethodMatch> {
        let (item, _, return_type) = self.tysys.auto_derive_by_method(method_name)?;
        let base_type_id = self.tysys.get_base_type(receiver_type_id);
        // A newtype has no derivation of its own: the one its representation
        // carries answers, and is inherited the way a written impl on the base
        // is, so the signature re-types back to the receiver.
        let derive_id = self
            .tysys
            .type_table
            .borrow()
            .representation_head(base_type_id);
        let inherited = (derive_id != base_type_id).then_some(derive_id);
        if !self.tysys.auto_derive_eligible_kind(derive_id) {
            return None;
        }
        let trait_ = self.tysys.compiler_trait(item)?;
        if !self.tysys.type_implements_trait(
            &self.annotate_ctx,
            &self.type_lookup(),
            derive_id,
            &trait_,
        ) {
            return None;
        }
        let ref_self_ty = self
            .tysys
            .type_table
            .borrow_mut()
            .intern(ResolvedType::Ref(derive_id));
        let method_info = MethodInfo {
            // Derived from the receiver's structure, off no `impl` block.
            impl_type_bindings: Vec::new(),
            method_def: None,
            return_type,
            self_kind: ast::SelfKind::Ref,
            param_types: vec![ref_self_ty],
            param_is_mut: vec![false],
            param_defaults: vec![None],
            param_names: vec!["other".to_string()],
            owner: inherited.map_or(MethodOwner::Receiver, MethodOwner::InheritedFrom),
            cm_name: None,
            is_ref_impl: false,
            method_type_param_ids: vec![],
            method_own_params: vec![],
            impl_module: None,
            from_concrete_impl: false,
            consumes_self: false,
            inherent_visibility: None,
            defaults_module: None,
        };
        // The receiver's declaration names the module the derived impl belongs
        // to; `auto_derive_eligible_kind` above already established it is one.
        let impl_module_source = self
            .tysys
            .type_table
            .borrow()
            .nominal_def(derive_id)
            .map_or_else(
                || self.declaring_module_of(struct_name),
                |def| self.tysys.resolutions.defs().module(def).clone(),
            );
        // The auto-derived trait is a compiler item, so it is named by the
        // declaration the registry holds, not by a spelling resolved here.
        let trait_fq = self.tysys.type_table.borrow().compiler_trait_fq(item);
        let trait_decl = self
            .tysys
            .compiler_trait_def(item)
            .expect("a compiler trait item names a declaration");
        Some(TraitMethodMatch {
            // Auto-derived `Eq` / `Ord` take no type arguments.
            trait_name: trait_fq,
            trait_decl,
            trait_args: vec![],
            method_info,
            impl_module_source,
            blanket_type_param: None,
            blanket_binder: None,
            blanket_bounds: None,
            impl_struct_name: match inherited {
                Some(id) => self.tysys.type_table.borrow().mangle_type_name(id),
                None => struct_name.to_string(),
            },
            impl_struct_fq: self.tysys.fq_receiver_head(derive_id),
            is_blanket_ref_impl: false,
            ref_impl_target: None,
        })
    }
}

impl TypeSystem {
    /// [`written_for`] over a call's type arguments. A parameter the call leaves
    /// parametric contributes nothing, so a bound mentioning it keeps its binder.
    fn call_site_types(
        &self,
        params: &[ast::GenericParam],
        type_args: &[TypeId],
    ) -> Vec<(FqTypeName, FqTypeName)> {
        let tt = self.type_table.borrow();
        params
            .iter()
            .zip(type_args)
            .filter(|(_, arg)| !tt.contains_type_param(**arg))
            .map(|(param, &arg)| (FqTypeName::binder(&param.name), tt.fq_type_name(arg)))
            .collect()
    }

    /// `fq` with each argument that names no type taken from the slot the bound
    /// fills: `Make<X::Item>` at `T: Constrained<Feed>` names what `Feed` binds
    /// `Item` to.
    fn trait_named_from_slots(
        &self,
        fq: FqTraitName,
        bound: &ast::TraitBound,
        slots: &IndexMap<u32, TypeId>,
    ) -> FqTraitName {
        if !bound.type_args.iter().any(names_no_type) {
            return fq;
        }
        let table = self.type_table.borrow();
        trait_named_by_position(fq, &table, |i| {
            bound
                .type_args
                .get(i)
                .filter(|ty| names_no_type(ty))
                .and_then(|_| slots.get(&(1 + i as u32)).copied())
        })
    }
}

/// A bound a call may dispatch through, with the trait it names and the space
/// its written types are read in.
#[derive(Clone)]
struct BoundCandidate {
    bound: ast::TraitBound,
    space: ParamSpace,
    decl: DefId,
    /// Whose `Self` the bound's written arguments mean.
    written_self: BoundSelf,
}

/// Whether a bound's argument names no type by its spelling, being written
/// against `Self`.
fn names_no_type(ty: &ast::Type) -> bool {
    ty.mentions("Self")
}

/// Which of `bound`'s arguments the reading frame resolves rather than reads as
/// the spelling it was written with: every one that names a type.
///
/// An argument written against `Self` names none, and `Self` at a reader is the
/// receiver rather than the declaring trait's own, so it stays for
/// [`Elaborator::trait_named_from_slots`].
fn args_to_resolve(bound: &ast::TraitBound) -> Vec<bool> {
    bound
        .type_args
        .iter()
        .map(|ty| !names_no_type(ty))
        .collect()
}

/// `fq` with the argument at each position `named` answers for replaced by the
/// type it names, the rest left as written.
fn trait_named_by_position(
    fq: FqTraitName,
    table: &TypeTable,
    named: impl Fn(usize) -> Option<TypeId>,
) -> FqTraitName {
    let args: Vec<FqTypeName> = fq
        .args()
        .iter()
        .enumerate()
        .map(|(i, written)| match named(i) {
            Some(id) => table.fq_type_name(id),
            None => written.clone(),
        })
        .collect();
    fq.with_args(args)
}

/// The bound as the source writes it, so a failure over an argument names the
/// argument: `Add<i32>` and not the `Add` the type does implement.
fn bound_as_written(name: &str, trait_: &FqTraitName) -> String {
    if trait_.args().is_empty() {
        return name.to_string();
    }
    let args: Vec<String> = trait_.args().iter().map(FqTypeName::to_display).collect();
    format!("{name}<{}>", args.join(", "))
}

/// Whether the compiler supplies `trait_name`'s operator for the primitive
/// spelled `prim_name`. `v128` is excluded with the non-numeric ones: its
/// arithmetic is lane-wise, and only the lane type's own impl knows the width.
pub(super) fn primitive_has_operator(prim_name: &str, op: CompilerItem) -> bool {
    let is_int = matches!(prim_name.as_bytes().first(), Some(b'i' | b'u'));
    match op {
        CompilerItem::Add
        | CompilerItem::Sub
        | CompilerItem::Mul
        | CompilerItem::Div
        | CompilerItem::Neg => is_int || matches!(prim_name, "f32" | "f64"),
        // `%` has no float lowering.
        CompilerItem::Rem => is_int,
        // Bit patterns. `bool` holds one bit, so `b & c` and `~b` are both
        // Wado expressions; a shift by a bit width it does not have is not.
        CompilerItem::BitAnd
        | CompilerItem::BitOr
        | CompilerItem::BitXor
        | CompilerItem::BitNot => is_int || prim_name == "bool",
        CompilerItem::Shl | CompilerItem::Shr => is_int,
        _ => false,
    }
}
