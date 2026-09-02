//! Trait query functions: checking trait implementations, bounds validation,
//! and associated type resolution.

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, Type};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::defs::DefId;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, Receiver, RefKind, TypeHead};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::callee::CalleeRef;
use super::scope::{BinderInScope, Scope, TraitCheckFrame};
use super::sig::TraitSig;
use super::trait_env::InheritedBound;
use super::types::{
    MethodInfo, MethodOwner, ResolvedTraitMethod, TraitMethodMatch, TypeError, TypeLookup,
};
use super::tysys::TypeSystem;

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
    Default,
    Reflect,
    ReflectStruct,
    ReflectVariant,
    ReflectEnum,
    ReflectFlags,
    ReflectNewtype,
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
            Self::Default => CompilerItem::Default,
            Self::Reflect => CompilerItem::Reflect,
            Self::ReflectStruct => CompilerItem::ReflectStruct,
            Self::ReflectVariant => CompilerItem::ReflectVariant,
            Self::ReflectEnum => CompilerItem::ReflectEnum,
            Self::ReflectFlags => CompilerItem::ReflectFlags,
            Self::ReflectNewtype => CompilerItem::ReflectNewtype,
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
            CompilerItem::Default => Self::Default,
            CompilerItem::Reflect => Self::Reflect,
            CompilerItem::ReflectStruct => Self::ReflectStruct,
            CompilerItem::ReflectVariant => Self::ReflectVariant,
            CompilerItem::ReflectEnum => Self::ReflectEnum,
            CompilerItem::ReflectFlags => Self::ReflectFlags,
            CompilerItem::ReflectNewtype => Self::ReflectNewtype,
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
    resolutions: &crate::resolve::Resolutions,
) -> Option<DefId> {
    let site = match qualifier? {
        ast::Type::Named(t) => t.id,
        ast::Type::Generic(t) => t.id,
        ast::Type::NamespacedGeneric(t) => t.id,
        _ => return None,
    };
    match resolutions.get(site) {
        crate::resolve::Resolution::Def(def) => Some(def),
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
    resolutions: &crate::resolve::Resolutions,
) -> Option<DefId> {
    let owner = ident.segments.len().checked_sub(2)?;
    match resolutions.get(ident.segments[owner].id) {
        crate::resolve::Resolution::Def(def) => Some(def),
        _ => None,
    }
}

/// Whether `ty` spells one of the declaration's own type packs
/// (`Trait<Assoc = [..P]>`). Such a binding names a parameter to project into,
/// not an expectation to enforce — and `..P` belongs to the declaration's
/// scope, so resolving it at a use site would not find it.
fn mentions_type_pack(ty: &ast::Type) -> bool {
    match ty {
        ast::Type::TypePackSpread(..) => true,
        ast::Type::NamespacedGeneric(ns) => ns.args.iter().any(mentions_type_pack),
        ast::Type::Generic(generic) => generic.args.iter().any(mentions_type_pack),
        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => mentions_type_pack(inner),
        ast::Type::Tuple(elements) => elements.iter().any(mentions_type_pack),
        _ => false,
    }
}

/// Whether an AST type is phrased against `Self` anywhere, and so only means
/// something where an implementing type is bound.
fn mentions_self(ty: &ast::Type) -> bool {
    match ty {
        ast::Type::Named(named) => named.name == "Self",
        ast::Type::NamespacedGeneric(ns) => {
            ns.namespace == "Self" || ns.args.iter().any(mentions_self)
        }
        ast::Type::Generic(generic) => {
            generic.name == "Self" || generic.args.iter().any(mentions_self)
        }
        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => mentions_self(inner),
        ast::Type::Tuple(elements) => elements.iter().any(mentions_self),
        _ => false,
    }
}

/// The recorded declaration facts of the trait `decl` declares, for a caller
/// holding the inputs rather than an `Elaborator` — reify's default-method
/// pass. Reads the digest the decl pass recorded, never the declaring module's
/// AST, and answers `None` for a declaration that is no trait.
pub(crate) fn trait_sig_of_with<'a>(
    decl: crate::defs::DefId,
    trait_env: &super::trait_env::TraitEnv,
    signatures: &'a super::sig::Signatures,
) -> Option<&'a super::sig::TraitSig> {
    if !trait_env.decl_index.contains(&decl) {
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
    ) -> Option<(crate::compiler_item::CompilerItem, String, TypeId)> {
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
    pub(super) fn auto_derive_by_trait(
        &self,
        trait_name: &str,
    ) -> Option<(crate::compiler_item::CompilerItem, TypeId)> {
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
    /// [`Self::trait_sig_of`] for a caller still holding a trait's spelling.
    pub(super) fn trait_sig_by_name(&self, trait_name: &str) -> Option<&TraitSig> {
        let decl = self.decl_key_or_local(trait_name)?;
        trait_sig_of_with(decl, &self.tysys.trait_env, &self.tysys.signatures)
    }

    /// The declaration header of the trait `trait_name` names in this frame.
    pub(super) fn trait_decl_header_in_frame(
        &self,
        trait_name: &str,
    ) -> Option<&super::trait_env::TraitDeclHeader> {
        self.trait_decl_header_of(&self.decl_key_or_local(trait_name)?)
    }

    /// The declaration header of a trait already identified.
    ///
    /// Every by-name form here funnels through this one, so a caller holding a
    /// site answers about the declaration that site resolved to rather than
    /// re-resolving the spelling in its own frame.
    pub(super) fn trait_decl_header_of(
        &self,
        key: &crate::defs::DefId,
    ) -> Option<&super::trait_env::TraitDeclHeader> {
        let loc = self.tysys.trait_env.decl_index.get(key)?;
        self.tysys.trait_env.trait_decl_headers.get(loc)
    }

    /// The trait's declaration of the associated type `assoc_name`, or `None`
    /// when it declares no such type.
    pub(super) fn trait_assoc_type_decl(
        &self,
        trait_name: &str,
        assoc_name: &str,
    ) -> Option<&ast::AssociatedTypeDecl> {
        self.trait_decl_header_in_frame(trait_name)?
            .assoc_types
            .iter()
            .find(|decl| decl.name == assoc_name)
    }

    /// Enforce a trait's associated-type bounds (`type X: Bound`) against an
    /// impl's bindings (`type X = Concrete`), skipping still-parametric
    /// bindings. Only the bound's trait is checked, not its associated-type
    /// equality constraints (`Iterator<Item = Self::Item>`).
    pub(super) fn enforce_impl_assoc_type_bounds(&mut self, impl_block: &ast::ImplBlock) {
        let Some(trait_type) = &impl_block.trait_type else {
            return;
        };
        let trait_name = self.get_type_name(trait_type);
        for binding in &impl_block.associated_types {
            // The bound carries its own reference site, so which `Ord` it
            // means is the answer the table already recorded for it — not the
            // spelling, which two modules can share.
            let bounds: Vec<(String, Option<DefId>)> = self
                .trait_assoc_type_decl(&trait_name, &binding.name)
                .into_iter()
                .flat_map(|decl| &decl.bounds)
                .filter(|bound| bound.fn_signature.is_none())
                .map(|bound| (bound.name.clone(), self.bound_trait_def(bound.id)))
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
            for (bound_name, bound_def) in &bounds {
                let Some(bound_def) = *bound_def else {
                    continue;
                };
                if !self.tysys.type_implements_trait(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    type_id,
                    bound_def,
                ) {
                    let type_name = self.tysys.type_id_to_string(type_id);
                    let reason = self.tysys.trait_unimpl_reason_chain(
                        &self.annotate_ctx,
                        &self.type_lookup(),
                        type_id,
                        bound_name,
                    );
                    let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                        type_name,
                        trait_name: bound_name.clone(),
                        param_name: binding.name.clone(),
                        reason,
                        span: binding.span,
                    });
                }
            }
        }
    }

    /// Enforce a trait's supertraits against `impl Trait for T`. The whole
    /// closure, not just the direct ones: a supertrait satisfied structurally
    /// has no impl block of its own to carry the rest of the chain.
    pub(super) fn enforce_impl_supertraits(&mut self, impl_block: &ast::ImplBlock) {
        let Some(trait_type) = &impl_block.trait_type else {
            return;
        };
        let trait_name = self.get_type_name(trait_type);
        // The header's own site says which trait it names, so an aliased
        // `impl B for T` enforces `Base`'s supertraits.
        let Some(trait_decl) = crate::resolve::head_site(trait_type)
            .and_then(|site| self.decl_key_at(site, &trait_name))
        else {
            return;
        };
        let supertraits: Vec<(String, Option<DefId>)> = self
            .tysys
            .trait_env
            .supertrait_closure(&trait_decl)
            .iter()
            .map(|b| (b.bound.name.clone(), self.bound_trait_def(b.bound.id)))
            .collect();
        if supertraits.is_empty() {
            return;
        }
        let self_type = self.resolve_type(&impl_block.ty);
        for (supertrait, supertrait_def) in supertraits {
            let Some(supertrait_def) = supertrait_def else {
                continue;
            };
            if self.tysys.type_implements_trait(
                &self.annotate_ctx,
                &self.type_lookup(),
                self_type,
                supertrait_def,
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

    /// Find a trait declaration's type parameters (e.g., `<T, U>` in `trait Foo<T, U>`).
    /// The declared type parameters of an already-identified trait.
    pub(super) fn trait_decl_type_params_of(
        &self,
        key: &crate::defs::DefId,
    ) -> Option<Vec<ast::GenericParam>> {
        let loc = self.tysys.trait_env.decl_index.get(key)?;
        self.tysys
            .trait_env
            .trait_decl_headers
            .get(loc)
            .map(|header| header.type_params.clone())
    }

    pub(super) fn find_trait_decl_type_params(
        &self,
        trait_name: &str,
    ) -> Option<Vec<ast::GenericParam>> {
        // `decl_key_or_local` is local-first (issue #1298), so the type-param
        // list and the default-method bodies resolve to the same trait.
        if let Some(params) = self
            .decl_key_or_local(trait_name)
            .and_then(|key| self.trait_decl_type_params_of(&key))
        {
            return Some(params);
        }
        // Only the current-module scan can add anything the key lookup did not:
        // a trait declared here whose canonical key missed the decl index.
        // (`trait_decl_headers` covers every loaded module, this one included.)
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
    /// The trait a compiler item names, as an identity.
    ///
    /// A compiler item is a declaration the compiler knows by construction, so
    /// a check phrased against one asks for *that* declaration — never for
    /// whatever a module's `Iterator` happens to be.
    pub(super) fn compiler_trait_def(&self, item: CompilerItem) -> Option<DefId> {
        let decl = self.type_table.borrow().compiler_items().trait_decl(item)?;
        self.resolutions.defs().of_ast_id(decl)
    }

    /// The declaration `type_id` is an instance of. A nominal type already knows
    /// its declaring node, so a caller holding a type has an identity without
    /// reading a `(name, module)` pair off it and resolving that again.
    /// The declaration `type_id` was *registered* under, so it declines for a head
    /// whose declaration never got a node — a `GenericResource` instantiation.
    /// For the head's declaration regardless, use [`crate::tir::TypeTable::nominal_def`].
    pub(crate) fn type_def(&self, type_id: TypeId) -> Option<DefId> {
        let decl = self.type_table.borrow().decl_of_type(type_id)?;
        self.resolutions.defs().of_ast_id(decl)
    }

    /// Whether `type_id` implements the trait `trait_` declares.
    ///
    /// The trait is an identity and nothing else. There is no name beside it to
    /// compare instead, and no way for a caller to decline to have one — which
    /// is what let 16 of this query's 30 call sites fall through to a spelling
    /// comparison that two modules' traits both satisfy.
    pub(super) fn type_implements_trait(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_: DefId,
    ) -> bool {
        let resolved = self.type_table.borrow().get(type_id).clone();

        if let Some(repeated) = Self::repeated_answer(ctx, type_id, trait_) {
            return repeated;
        }
        Self::open_question(ctx, type_id, trait_);
        let result = self.type_implements_trait_inner(ctx, scope, type_id, &resolved, trait_);
        ctx.trait_check_stack.borrow_mut().pop();

        self.check_solver_agreement(ctx, scope, type_id, trait_, result);
        result
    }

    /// The answer to a question already open on the stack, `None` for a new
    /// one: a repeat reached through a member is a recursive type and holds; one
    /// reached through bounds alone grounds nothing (WEP 2026-09-01).
    fn repeated_answer(ctx: &Scope, type_id: TypeId, trait_: DefId) -> Option<bool> {
        let member_edges = ctx.member_edges.get();
        ctx.trait_check_stack
            .borrow()
            .iter()
            .find(|f| f.type_id == type_id && f.trait_ == trait_)
            .map(|open| member_edges > open.member_edges)
    }

    /// Open a question on the stack; the caller pops it.
    fn open_question(ctx: &Scope, type_id: TypeId, trait_: DefId) {
        ctx.trait_check_stack.borrow_mut().push(TraitCheckFrame {
            type_id,
            trait_,
            member_edges: ctx.member_edges.get(),
        });
    }

    /// The differential of WEP 2026-09-01: in debug builds, the solver must
    /// answer an outermost bound question as this path did.
    fn check_solver_agreement(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_: DefId,
        expected: bool,
    ) {
        if !ctx.trait_check_stack.borrow().is_empty() {
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
            self.resolutions.defs().name(trait_),
            bridge.explain(self, ctx, scope, type_id, trait_),
        );
    }

    /// Whether `type_id` satisfies `trait_` at the type itself, without peeling
    /// a newtype — rank 2's question, where [`Self::type_implements_trait`]
    /// answers dispatch's (`docs/wep-2026-09-01-trait-resolution.md`).
    pub(super) fn type_implements_trait_here(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_: DefId,
    ) -> bool {
        let is_newtype = matches!(
            self.type_table.borrow().get(type_id),
            ResolvedType::Newtype { .. }
        );
        if !is_newtype {
            return self.type_implements_trait(ctx, scope, type_id, trait_);
        }
<<<<<<< HEAD
        if let Some(repeated) = Self::repeated_answer(ctx, type_id, trait_) {
            return repeated;
||||||| b32a52617
        // A repeat answers `false` where `type_implements_trait` answers `true`
        // on the same stack: that query descends into members, this one holds
        // the subject fixed, so a repeat here is a bound grounding nothing.
        {
            let stack = ctx.trait_check_stack.borrow();
            if stack.contains(&(type_id, trait_)) {
                return false;
            }
=======
        // The two facts a newtype owns rather than inherits: it has a name, and
        // it is a newtype (WEP 2026-06-13). Both are synthesized, so no impl
        // block exists for the index search below to find, and both hold at
        // depth 0 — which is what makes a `ReflectNewtype`-keyed blanket
        // outrank one the base satisfies.
        if matches!(
            self.on_bound_of(trait_),
            Some(OnBoundTrait::Reflect | OnBoundTrait::ReflectNewtype)
        ) {
            return true;
        }
        // A repeat answers `false` where `type_implements_trait` answers `true`
        // on the same stack: that query descends into members, this one holds
        // the subject fixed, so a repeat here is a bound grounding nothing.
        {
            let stack = ctx.trait_check_stack.borrow();
            if stack.contains(&(type_id, trait_)) {
                return false;
            }
>>>>>>> origin/main
        }
        Self::open_question(ctx, type_id, trait_);
        let receiver = self.type_table.borrow().impl_receiver_key(type_id);
        let result = self.find_trait_impl_for_subject(
            ctx,
            scope,
            Some(type_id),
            &receiver,
            trait_,
            NewtypePeel::Here,
        );
        ctx.trait_check_stack.borrow_mut().pop();
        result
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
        trait_: DefId,
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
        let Some(trait_) = self.compiler_trait_def(tr.compiler_item()) else {
            return;
        };
        if let StructuralConformance::Fails {
            member: label,
            type_id: member_tid,
        } = self.structural_conformance(ctx, scope, &resolved, tr, trait_)
        {
            let owner = self.type_id_to_string(type_id);
            let member_ty = self.type_id_to_string(member_tid);
            chain.push(format!(
                "`{owner}` does not implement `{trait_name}` because {label} of type `{member_ty}` does not implement `{trait_name}`"
            ));
            self.collect_trait_unimpl_reason(ctx, scope, member_tid, trait_name, chain);
        }
    }

    /// Whether a reflection written at `scope`'s module can enumerate `info`'s
    /// fields (WEP 2026-06-13, Visibility). *Every* field must be reachable: a
    /// declaration carries one synthesized impl with a fixed `members()`, so
    /// admitting the struct on one public field would expose its private ones.
    /// Eligibility is separate — [`Self::is_reflect_eligible`] sees every field.
    fn has_visible_fields(&self, scope: &TypeLookup, info: &super::types::StructFieldInfo) -> bool {
        info.fields_visible_from(scope.current_module_source)
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
            .is_some_and(|info| self.has_visible_fields(scope, info)),
            CompilerItem::ReflectVariant => match resolved {
                ResolvedType::Variant { def } | ResolvedType::GenericInstance { def, .. } => {
                    scope.variant_cases_of(*def).is_some()
                }
                _ => false,
            },
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
    pub(super) fn synth_trait_key(&self, on_bound: OnBoundTrait) -> Option<crate::defs::DefId> {
        self.type_table
            .borrow()
            .compiler_items()
            .trait_fq_opt(on_bound.compiler_item())
            .and_then(|t| t.canonical())
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
        self.trait_env.decl_index.contains(&key).then_some(key)
    }

    /// Whether holding the bound spelled `bound_name` in `scope` also gives
    /// `trait_` — the same declaration, or one of its supertraits.
    pub(super) fn bound_decl_implies(
        &self,
        scope: &TypeLookup,
        bound_name: &str,
        trait_: DefId,
    ) -> bool {
        if self.scoped_trait_decl_key(scope, bound_name) == Some(trait_) {
            return true;
        }
        self.supertraits_of(scope, bound_name)
            .iter()
            .any(|s| s.decl == trait_)
    }

    /// The transitive supertraits of `trait_name` as seen from `scope`.
    pub(super) fn supertraits_of(&self, scope: &TypeLookup, trait_name: &str) -> &[InheritedBound] {
        match self.scoped_trait_decl_key(scope, trait_name) {
            Some(key) => self.trait_env.supertrait_closure(&key),
            None => self.trait_env.supertrait_closure_named(trait_name),
        }
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
            .decl_index
            .contains(&def)
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
        let substitution = |param_ids: &[TypeId], type_args: &[TypeId]| {
            let table = self.type_table.borrow();
            param_ids
                .iter()
                .zip(type_args)
                .filter_map(|(&param, &arg)| match table.get(param) {
                    ResolvedType::TypeParam { index, .. } => Some((*index, arg)),
                    _ => None,
                })
                .collect::<IndexMap<u32, TypeId>>()
        };
        let at_instance = |substitution: &IndexMap<u32, TypeId>, tid: TypeId| {
            if substitution.is_empty() {
                tid
            } else {
                self.type_table
                    .borrow_mut()
                    .substitute_type_params(tid, substitution)
            }
        };
        let walk_struct = |info: &super::types::StructFieldInfo,
                           type_args: &[TypeId],
                           visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool|
         -> bool {
            let substitution = substitution(&info.type_param_type_ids, type_args);
            info.fields.iter().all(|(fname, tid, _)| {
                visit(
                    StructuralMember::Field(fname),
                    at_instance(&substitution, *tid),
                )
            })
        };
        let walk_variant = |info: &super::types::VariantInfo,
                            type_args: &[TypeId],
                            visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool|
         -> bool {
            let substitution = substitution(&info.type_param_type_ids, type_args);
            info.cases
                .iter()
                .filter(|c| c.payload != TypeTable::UNIT)
                .all(|c| {
                    visit(
                        StructuralMember::Case(&c.name),
                        at_instance(&substitution, c.payload),
                    )
                })
        };
        match resolved {
            ResolvedType::Enum { .. } => Some(true),
            // A bitmask has no members to recurse into, like a plain `enum`.
            ResolvedType::Flags { .. } => Some(true),
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
        let Some(trait_) = self.compiler_trait_def(tr.compiler_item()) else {
            return false;
        };
        if tr.is_total() {
            return true;
        }
        if tr == OnBoundTrait::Default {
            return self.is_defaultable_struct(scope, type_id);
        }
        let resolved = self.type_table.borrow().get(type_id).clone();
        match &resolved {
            ResolvedType::Newtype { base_type, .. } => {
                self.type_implements_trait(ctx, scope, *base_type, trait_)
            }
            ResolvedType::Flags { .. } => {
                self.type_implements_trait(ctx, scope, TypeTable::U32, trait_)
            }
            nominal => {
                self.walk_structural_derive_members(scope, nominal, tr, &mut |_, member| {
                    matches!(
                        self.type_table.borrow().get(member),
                        ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
                    ) || self.type_implements_trait(ctx, scope, member, trait_)
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

    /// The `Ref` marker's eligibility: whether a value of this type is a Wasm GC
    /// reference (WEP 2026-01-20). A `Newtype` follows its base type.
    pub(super) fn is_ref_identity(&self, resolved: &ResolvedType) -> bool {
        match resolved {
            ResolvedType::Primitive(p) => {
                matches!(p, PrimitiveType::I128 | PrimitiveType::U128)
            }
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
    pub(super) fn is_ref_mut_identity(&self, scope: &TypeLookup, resolved: &ResolvedType) -> bool {
        match resolved {
            ResolvedType::Variant { .. } | ResolvedType::Function { .. } => false,
            ResolvedType::GenericInstance { def, .. } => {
                if scope.variant_cases_of(*def).is_some() {
                    false
                } else {
                    self.is_ref_identity(resolved)
                }
            }
            ResolvedType::Newtype { base_type, .. } => {
                let base = self.type_table.borrow().get(*base_type).clone();
                self.is_ref_mut_identity(scope, &base)
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
                m.sig.self_kind == crate::ast::SelfKind::None
                    && self.receiverless_method_mentions_self(&m.sig)
            })
        })
    }

    /// Whether a receiverless method's signature names `Self` — in a parameter,
    /// the return type, or a bound on one of its own type parameters. Slot 0 of
    /// a trait method's frame is `Self`.
    fn receiverless_method_mentions_self(&self, sig: &super::sig::MethodSig) -> bool {
        let table = self.type_table.borrow();
        let in_types = sig
            .decl
            .param_types
            .iter()
            .chain(sig.decl.return_type.iter())
            .any(|t| table.contains_type_param_index(*t, 0));
        in_types
            || sig.own_params.iter().any(|p| {
                p.bounds
                    .iter()
                    .flat_map(|b| &b.assoc_types)
                    .any(|c| mentions_self(&c.ty))
            })
    }

    fn type_implements_trait_inner(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        resolved: &ResolvedType,
        trait_: DefId,
    ) -> bool {
        let on_bound = self.on_bound_of(trait_);

        if on_bound.is_some_and(OnBoundTrait::is_total) {
            return true;
        }

        // A plain `enum` auto-derives `Display` (the bare case name), so its
        // bound holds before `synthesize_traits` emits the body.
        if matches!(resolved, ResolvedType::Enum { .. }) && self.is_display_trait_of(trait_) {
            return true;
        }

        if let ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } = resolved
        {
            return ctx
                .trait_ctx
                .type_param_bounds
                .get(name)
                .is_some_and(|bounds| {
                    bounds
                        .iter()
                        .any(|b| self.bound_decl_implies(scope, &b.name, trait_))
                });
        }

        if on_bound == Some(OnBoundTrait::Ref) {
            return self.is_ref_identity(resolved);
        }

        if on_bound == Some(OnBoundTrait::RefMut) {
            return self.is_ref_mut_identity(scope, resolved);
        }

        let is_eq = on_bound == Some(OnBoundTrait::Eq);
        let is_eq_or_ord = is_eq || on_bound == Some(OnBoundTrait::Ord);

        // Primitives have built-in implementations for certain traits
        if let ResolvedType::Primitive(prim) = &resolved {
            if is_eq_or_ord {
                return true;
            }
            // Numeric primitives implement the operator traits the compiler
            // supplies — the items, so a user `trait Rem` gets none of it.
            if self
                .compiler_item_of_trait(trait_)
                .is_some_and(|op| primitive_has_operator(prim.as_str(), op))
            {
                return true;
            }
            return self.find_trait_impl_for_subject(
                ctx,
                scope,
                Some(type_id),
                &Receiver::Type(FqTypeName::builtin(prim.as_str())),
                trait_,
                NewtypePeel::Follow,
            );
        }

        // Read the head out before the block: a `borrow()` held in a
        // `let`-chain lives for the whole body, and the body borrows mutably
        // to record the synthesis request.
        let nominal = self.type_table.borrow().nominal_head(type_id);
        if let Some(tr) = on_bound
            && tr.is_field_recursive()
            && let Some((_, module_source)) = nominal
        {
            let receiver = self.type_table.borrow().impl_receiver_key(type_id);
            let serde_blocked = tr.is_serde()
                && self.has_real_trait_impl_for_type(ctx, scope, Some(type_id), &receiver, trait_);
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
            ResolvedType::BuiltinArray(elem) => (
                FqTypeName::builtin(TypeTable::ARRAY_TYPE_NAME),
                Some(vec![*elem]),
            ),
            ResolvedType::GenericInstance { type_args, .. } => (
                self.type_table.borrow().fq_base_type_name(type_id),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
            ),
            ResolvedType::Ref(inner) => {
                // References always implement Eq via ref.eq (identity comparison)
                if is_eq {
                    return true;
                }
                // Check for a specific impl Trait for &T first (e.g., impl Inspect for &T)
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args(
                    ctx,
                    scope,
                    Some(type_id),
                    &Receiver::Ref(RefKind::Shared),
                    trait_,
                    Some(&[inner_id]),
                    NewtypePeel::Follow,
                ) {
                    return true;
                }
                if self.ref_denies_bound(on_bound, trait_) {
                    return false;
                }
                return self.type_implements_trait(ctx, scope, inner_id, trait_);
            }
            ResolvedType::MutRef(inner) => {
                // Mutable references always implement Eq via ref.eq (identity comparison)
                if is_eq {
                    return true;
                }
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args(
                    ctx,
                    scope,
                    Some(type_id),
                    &Receiver::Ref(RefKind::Mut),
                    trait_,
                    Some(&[inner_id]),
                    NewtypePeel::Follow,
                ) {
                    return true;
                }
                if self.ref_denies_bound(on_bound, trait_) {
                    return false;
                }
                return self.type_implements_trait(ctx, scope, inner_id, trait_);
            }
            ResolvedType::AssocTypeProjection { bounds, .. } => {
                // An associated type projection T::Assoc implements a trait if
                // the trait declaration for Assoc declares that bound.
                // e.g., I::Iter: Iterator when IntoIterator::Iter: Iterator
                return bounds.iter().any(|b| b.canonical() == Some(trait_));
            }
            ResolvedType::Newtype { base_type, .. } => {
                // Check for a direct impl on the newtype first (e.g., impl Describe for Meters)
                let receiver = self.type_table.borrow().impl_receiver_key(type_id);
                if self.find_trait_impl_for_subject(
                    ctx,
                    scope,
                    Some(type_id),
                    &receiver,
                    trait_,
                    NewtypePeel::Follow,
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
                    trait_,
                    NewtypePeel::Follow,
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
            trait_,
            type_args.as_deref(),
            NewtypePeel::Follow,
        )
    }

    /// Whether an impl block makes `type_key` implement `trait_`. `subject` is
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
    ) -> bool {
        self.find_trait_impl_for_type_with_args(ctx, scope, subject, type_key, trait_, None, peel)
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

    /// Whether a bare bound on the header's trait selects it — see
    /// [`super::trait_env::header_answers_bare_bound`].
    fn header_answers_bare_bound(&self, header: &super::trait_env::ImplHeader) -> bool {
        let (Some(trait_type), Some(decl)) = (header.trait_type.as_ref(), header.trait_ref) else {
            return true;
        };
        let Some(decl_header) = self.trait_env.trait_decl_headers.get(&decl) else {
            return true;
        };
        super::trait_env::header_answers_bare_bound(
            trait_type,
            &header.ty,
            &decl_header.type_params,
            &self.resolutions,
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
                if header.trait_ref == Some(trait_)
                    && self.header_answers_bare_bound(header)
                    && self.inherent_impl_type_args_match(&header.ty, type_args)
                    && self.check_impl_block_bounds(
                        ctx,
                        scope,
                        &header.type_params,
                        &header.ty,
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
    fn primitive_satisfies_builtin_trait(
        &self,
        type_key: &Receiver,
        bound: &super::trait_env::BlanketBound,
    ) -> bool {
        let Receiver::Type(fq) = type_key else {
            return false;
        };
        let TypeHead::Builtin(name) = fq.head() else {
            return false;
        };
        if !PrimitiveType::is_primitive_name(name) {
            return false;
        }
        let Some(trait_) = bound.decl_ref else {
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
        bound_trait: DefId,
        peel: NewtypePeel,
    ) -> bool {
        match (peel, subject) {
            (NewtypePeel::Here, Some(id)) => {
                self.type_implements_trait_here(ctx, scope, id, bound_trait)
            }
            (NewtypePeel::Here, None) => {
                self.find_trait_impl_for_subject(ctx, scope, subject, type_key, bound_trait, peel)
            }
            // With a subject the general question is the whole answer: it
            // searches the impls too, under the recursion guard. Asking the
            // impls again beside it would repeat a refused question outside
            // the guard.
            (NewtypePeel::Follow, Some(id)) => {
                self.type_implements_trait(ctx, scope, id, bound_trait)
            }
            (NewtypePeel::Follow, None) => {
                self.find_trait_impl_for_subject(ctx, scope, subject, type_key, bound_trait, peel)
            }
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
                b.receiver == super::trait_env::BlanketReceiver::Value
                    && !matches!(type_key, Receiver::Ref(_))
            })
            .filter(|b| !(structural && self.is_reflect_bounded(scope, b)))
        {
            let bounds_satisfied = blanket.bounds.iter().all(|bound| {
                self.synthesized_reflect_bound_holds(scope, &type_key.decl_key(), &bound.name)
                    || self.primitive_satisfies_builtin_trait(type_key, bound)
                    || bound.decl_ref.is_some_and(|bound_trait| {
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
        bounds: &[super::trait_env::BlanketBound],
    ) -> bool {
        bounds.iter().all(|bound| {
            if bound.pinned_to_receiver.is_empty() {
                return true;
            }
            let (Some(subject), Some(trait_)) = (subject, bound.decl_ref) else {
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
    fn is_reflect_bounded(
        &self,
        scope: &TypeLookup,
        blanket: &super::trait_env::BlanketImpl,
    ) -> bool {
        blanket.bounds.iter().any(|bound| {
            self.classify_on_bound_trait(scope, &bound.name)
                .is_some_and(OnBoundTrait::is_reflect)
        })
    }

    /// The module declaring `def`, when `def` is a newtype. The kind with no
    /// members carries no member info to read a module off, so the declaration
    /// answers directly.
    fn newtype_declaring_module(
        &self,
        scope: &TypeLookup,
        def: crate::defs::DefId,
    ) -> Option<ModuleSource> {
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
        type_name: &crate::name::DeclName,
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
            OnBoundTrait::Eq
            | OnBoundTrait::Ord
            | OnBoundTrait::Serialize
            | OnBoundTrait::Deserialize
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
    def: crate::defs::DefId,
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
        | OnBoundTrait::Eq
        | OnBoundTrait::Ord
        | OnBoundTrait::Serialize
        | OnBoundTrait::Deserialize
        | OnBoundTrait::Default
        | OnBoundTrait::Ref
        | OnBoundTrait::RefMut
        | OnBoundTrait::Inspect => None,
    }
}

/// An associated type paired with the trait that *declares* it: a subtrait's
/// default body may name a supertrait's, and keying the projection to the
/// dispatched-through trait made `<T as Base>::Elem` and `Derived`'s two types.
type DeclaredAssocType = (crate::defs::DefId, ast::AssociatedTypeDecl);

impl<H: CompilerHost> Elaborator<'_, H> {
    /// The trait declaration a reference site names, from
    /// [`crate::resolve::Resolutions`] and so resolved in the writing module: an
    /// alias or a second module's same-named trait cannot displace it.
    ///
    /// `written` feeds the fallback only, for a site answering with something that
    /// is no trait at all — a same-named enum case in the prelude.
    pub(super) fn trait_decl_at(
        &self,
        site: crate::ast::AstId,
        written: &str,
    ) -> Option<crate::defs::DefId> {
        if let Some(def) = self.tysys.resolutions.declared(site)
            && self.tysys.trait_env.decl_index.contains(&def)
        {
            return Some(def);
        }
        self.decl_key_or_local(written)
    }

    /// Whether the trait declaration `decl` is in scope in the current frame:
    /// declared by the current module, or reachable from it under any name.
    /// Ties between same-named foreign declarations break on this.
    pub(super) fn trait_decl_in_scope(&self, decl: crate::defs::DefId) -> bool {
        self.tysys
            .resolutions
            .in_scope(&self.current_module_source, decl)
    }

    /// Whether the trait `key` names declares `method_name`. The cheap form of
    /// [`Self::trait_method_of`], for counting candidates without cloning each
    /// one's declaration.
    fn trait_declares_method_of(&self, key: &crate::defs::DefId, method_name: &str) -> bool {
        self.trait_decl_header_of(key)
            .is_some_and(|header| header.methods.iter().any(|m| m.name == method_name))
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
        key: &crate::defs::DefId,
        method_name: &str,
    ) -> Option<(super::sig::MethodSig, Vec<DeclaredAssocType>)> {
        let header = self.trait_decl_header_of(key)?;
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
            .supertrait_closure(key)
            .iter()
            .filter_map(|bound| Some((bound.decl, self.trait_decl_header_of(&bound.decl)?)))
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
            .trait_sig_of(key)
            .and_then(|sig| sig.method(method_name))
            .expect("the decl pass records every trait method's signature")
            .sig
            .clone();
        Some((sig, assoc_types))
    }

    /// The recorded declaration facts of an identified trait — the digest
    /// counterpart of [`Self::trait_decl_header_of`], answerable only once the
    /// decl pass has run.
    /// The recorded signature of an already-identified trait.
    ///
    /// Every by-name form funnels through this one. Flattening a key back to
    /// its declared name and resolving that again is what broke an aliased
    /// head: the module imported `Alpha as Ay` and never `Alpha`, so the
    /// second resolution found nothing.
    pub(super) fn trait_sig_of(&self, key: &crate::defs::DefId) -> Option<&super::sig::TraitSig> {
        if !self.tysys.trait_env.decl_index.contains(key) {
            return None;
        }
        self.tysys.signatures.trait_sig(*key)
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
                let bound_names: Vec<crate::name::FqTraitName> = decl
                    .bounds
                    .iter()
                    .map(|b| self.fq_trait_name_at(b.id, &b.name))
                    .collect();
                let bindings = self.frame_assoc_bindings(self_type_id, &self_name, &decl.bounds);
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_assoc_type_projection_of_trait(
                        self_type_id,
                        Some(*declaring),
                        decl.name.clone(),
                        bound_names,
                        bindings,
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
        bounds: &[ast::TraitBound],
        method_name: &str,
        self_type_id: TypeId,
        span: Span,
        required_trait: Option<&super::types::RequiredTrait>,
    ) -> Option<(crate::name::FqTraitName, MethodInfo)> {
        self.find_method_in_trait_bounds_with(
            bounds,
            &IndexMap::default(),
            method_name,
            self_type_id,
            span,
            required_trait,
        )
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
        bounds: &[ast::TraitBound],
        known: &IndexMap<crate::ast::AstId, crate::name::FqTraitName>,
        method_name: &str,
        self_type_id: TypeId,
        span: Span,
        required_trait: Option<&super::types::RequiredTrait>,
    ) -> Option<(crate::name::FqTraitName, MethodInfo)> {
        let bounds = self.elaborate_bounds_with(bounds, known);
        // Which trait each bound means is settled once, here: a bound reached
        // through a supertrait was written in the *declaring* module, so
        // resolving its spelling in this frame would miss an aliased one.
        let keyed: Vec<(ast::TraitBound, crate::defs::DefId)> = bounds
            .iter()
            .filter_map(|b| {
                // A synthesised bound carries its referent, so it is read off
                // the bound rather than looked up by an id the walk never saw.
                let key = b
                    .resolved
                    .or_else(|| {
                        known
                            .get(&b.id)
                            .and_then(crate::name::FqTraitName::canonical)
                    })
                    .or_else(|| self.trait_decl_at(b.id, &b.name));
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
        let candidates: Vec<(ast::TraitBound, crate::defs::DefId)> = keyed
            .into_iter()
            .filter(|(_, key)| {
                required_trait.is_none_or(|w| match w.decl {
                    // A binder or an unreached name declares no trait, so it
                    // competes with none — which is what the fabricated key
                    // amounted to.
                    crate::resolve::Resolution::Def(def) => def == *key,
                    _ => false,
                }) && self.trait_declares_method_of(key, method_name)
            })
            .collect();
        let resolved = candidates.first().and_then(|(bound, key)| {
            self.trait_method_of(key, method_name)
                .map(|found| (bound.clone(), *key, found))
        });
        if candidates.len() > 1 {
            // Two candidates can share a spelling; reporting both as "Base"
            // names no escape from the collision.
            let ambiguous_spelling = |bound: &ast::TraitBound, key: &crate::defs::DefId| {
                candidates
                    .iter()
                    .any(|(other, other_key)| other.name == bound.name && other_key != key)
            };
            let traits = candidates
                .iter()
                .map(|(bound, key)| {
                    if ambiguous_spelling(bound, key) {
                        format!(
                            "{}::{}",
                            self.tysys.resolutions.defs().module(*key),
                            bound.name
                        )
                    } else {
                        bound.name.clone()
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
        let (bound, decl, (sig, trait_assoc_types)) = resolved?;
        // The bound answers with the trait its own reference site resolves to,
        // not the spelling it wrote: an aliased bound (`T: G` for
        // `use { Greet as G }`) must reach the impl that defines the method.
        let fq_trait_name = known.get(&bound.id).cloned().unwrap_or_else(|| {
            crate::name::FqTraitName::declared(self.tysys.resolutions.defs(), decl)
        });

        let answers = self.trait_assoc_answers(&trait_assoc_types, self_type_id);
        let slots = self.bare_bound_slots(decl, self_type_id);
        let instantiated = sig.decl.instantiate_slots_with(
            &self.tysys.type_table,
            &slots,
            &crate::tir::SlotProjections::from_iter([(0, answers)]),
        );
        let first_value_param = sig.first_value_param().min(instantiated.param_types.len());

        Some((
            fq_trait_name,
            MethodInfo {
                method_def: Some(sig.def),
                return_type: instantiated.return_type,
                self_kind: sig.self_kind,
                param_types: instantiated.param_types[first_value_param..].to_vec(),
                param_is_mut: super::sig::Param::is_mut_flags(&sig.params),
                owner: MethodOwner::Receiver,
                cm_name: None,
                is_ref_impl: false,
                method_type_param_ids: sig.own_type_param_ids(),
                method_own_params: sig.own_params.clone(),
                impl_module: None,
                from_concrete_impl: false,
                param_defaults: sig.params.iter().map(|p| p.default.clone()).collect(),
                param_names: super::sig::Param::names(&sig.params),
                consumes_self: sig.self_kind == ast::SelfKind::Value,
                inherent_visibility: None,
            },
        ))
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
    /// Check if an impl block's type parameter bounds are satisfied by the given type args.
    /// For `impl<T: Ord> List<T>`, checks that the concrete type substituted for T implements Ord.
    pub(super) fn check_impl_block_bounds(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_params: &[ast::GenericParam],
        impl_ty: &ast::Type,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        // No type params with bounds → always OK
        if type_params.iter().all(|p| p.bounds.is_empty()) {
            return true;
        }

        let Some(type_args) = type_args else {
            // No type args to check (non-generic receiver) → skip bounds check
            return true;
        };

        // Build name → bounds map from impl block type params (trait names only)
        let bounds_map: IndexMap<&str, Vec<DefId>> = type_params
            .iter()
            .filter(|p| !p.bounds.is_empty())
            .map(|p| {
                (
                    p.name.as_str(),
                    // The bound's own site says which trait it names, so the
                    // check compares declarations rather than the spelling the
                    // impl header happened to write.
                    p.bounds
                        .iter()
                        .filter_map(|b| self.resolutions.declared(b.id))
                        .collect(),
                )
            })
            .collect();

        // Match type params to receiver type args via generic type arg positions
        let inner_type_name: Option<&str> =
            if let ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) = impl_ty {
                if let ast::Type::Named(inner) = boxed.as_ref() {
                    Some(&inner.name)
                } else {
                    None
                }
            } else {
                None
            };

        if let ast::Type::Generic(generic) = impl_ty {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg
                    && let Some(bounds) = bounds_map.get(named.name.as_str())
                    && let Some(&type_arg) = type_args.get(i)
                {
                    for &bound in bounds {
                        if !self.type_implements_trait(ctx, scope, type_arg, bound) {
                            return false;
                        }
                    }
                }
            }
        } else if let Some(inner_name) = inner_type_name {
            // Handle `impl<T: Bound> Trait for &T` / `impl<T: Bound> Trait for &mut T`:
            // type_args[0] is the inner type T.
            if let Some(bounds) = bounds_map.get(inner_name)
                && let Some(&type_arg) = type_args.first()
                && !matches!(
                    self.type_table.borrow().get(type_arg),
                    ResolvedType::TypeParam { .. }
                )
            {
                for &bound in bounds {
                    if !self.type_implements_trait(ctx, scope, type_arg, bound) {
                        return false;
                    }
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
                    if matches!(
                        self.type_table.borrow().get(type_arg),
                        ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
                    ) {
                        continue;
                    }
                    for &bound in bounds {
                        if !self.type_implements_trait(ctx, scope, type_arg, bound) {
                            return false;
                        }
                    }
                }
            }
        }

        true
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Check trait bounds on a generic function's type arguments.
    /// Looks up the function's type params and validates bounds against the provided type args.
    pub(super) fn check_function_type_arg_bounds(
        &mut self,
        callee: &CalleeRef,
        type_args: &[TypeId],
        span: Span,
    ) {
        let type_params = self.lookup_function_type_params(callee);
        self.enforce_type_arg_bounds(&type_params, type_args, span);
    }

    /// The single enforcement of trait bounds on a generic decl's type args,
    /// shared by every generic-call kind so the rule cannot drift. Enforces only
    /// fully concrete args: a still-parametric arg is forwarded from the caller
    /// (verified once concrete, since impl-level bounds are not in scope here),
    /// and `fn(...)`-bound params are realised eagerly elsewhere.
    pub(super) fn enforce_type_arg_bounds(
        &mut self,
        params: &[ast::GenericParam],
        type_args: &[TypeId],
        span: Span,
    ) {
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
            for bound in &param.bounds.clone() {
                if bound.fn_signature.is_some() {
                    continue;
                }
                for &subject in &subjects {
                    let bound_def = self.bound_trait_def(bound.id);
                    self.enforce_single_bound(subject, &bound.name, bound_def, &param.name, span);
                    self.enforce_assoc_type_bounds(subject, bound, span);
                }
            }
            // A supertrait failure has the same one cause as the bound that
            // implied it, so it is asked but not reported — asking is what
            // drives the derivation that makes `T: Ord` alone satisfy `Eq`.
            for bound in self.elaborate_bounds(&param.bounds) {
                if bound.fn_signature.is_some() || param.bounds.iter().any(|b| b.name == bound.name)
                {
                    continue;
                }
                for &subject in &subjects {
                    if let Some(trait_) = self.bound_trait_def(bound.id) {
                        self.check_and_register_bound(subject, trait_);
                    }
                    self.enforce_assoc_type_bounds(subject, &bound, span);
                }
            }
        }
    }

    /// Check one concrete type argument against one trait bound — the primitive
    /// every bound-enforcement path funnels through. On success registers the
    /// associated types; on failure raises a clean `TraitBoundNotSatisfied`.
    pub(super) fn enforce_single_bound(
        &mut self,
        type_arg: TypeId,
        trait_name: &str,
        trait_: Option<DefId>,
        param_name: &str,
        span: Span,
    ) {
        // A bound whose site names no declaration cannot be enforced against an
        // identity; the unresolved name is diagnosed where it was written.
        let Some(trait_) = trait_ else {
            return;
        };
        if !self.check_and_register_bound(type_arg, trait_) {
            let type_name = self.tysys.type_id_to_string(type_arg);
            let reason = self.tysys.trait_unimpl_reason_chain(
                &self.annotate_ctx,
                &self.type_lookup(),
                type_arg,
                trait_name,
            );
            let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name: trait_name.to_string(),
                param_name: param_name.to_string(),
                reason,
                span,
            });
        }
    }

    /// What a bare bound binds `decl`'s slots to: slot 0 is `Self`, and the
    /// trait's own parameters take their declared defaults, since a bound names
    /// none of them positionally (`T: Add<Output = T>` binds an assoc type).
    pub(super) fn bare_bound_slots(
        &mut self,
        decl: crate::defs::DefId,
        self_type_id: TypeId,
    ) -> IndexMap<u32, TypeId> {
        let mut slots = IndexMap::from_iter([(0, self_type_id)]);
        let Some(trait_params) = self.trait_decl_type_params_of(&decl) else {
            return slots;
        };
        let defaults: Vec<(u32, ast::Type)> = trait_params
            .iter()
            .filter(|p| p.is_real_type_param())
            .enumerate()
            .filter_map(|(i, p)| p.default.clone().map(|d| (1 + i as u32, d)))
            .collect();
        for (slot, default_ty) in defaults {
            let resolved = self.with_self_type(self_type_id, |s| s.resolve_type(&default_ty));
            slots.insert(slot, resolved);
        }
        slots
    }

    /// Check a bound's associated-type constraints (`T: Collect<Item = i32>`)
    /// against the type argument. Runs after [`Self::enforce_single_bound`],
    /// which is what registers the argument's bindings.
    fn enforce_assoc_type_bounds(&mut self, type_arg: TypeId, bound: &ast::TraitBound, span: Span) {
        for constraint in &bound.assoc_types {
            // `Self` means the implementing type, which this site has no
            // binding for; `enforce_impl_assoc_type_bounds` owns those.
            if mentions_self(&constraint.ty) || mentions_type_pack(&constraint.ty) {
                continue;
            }
            // The bound's own site says which trait declares the constraint.
            let trait_key = self.fq_trait_name_at(bound.id, &bound.name).canonical();
            let Some(actual) = trait_key.and_then(|key| {
                self.tysys.type_table.borrow().resolve_assoc_type_of_trait(
                    type_arg,
                    &key,
                    &constraint.name,
                )
            }) else {
                continue;
            };
            let expected = self.resolve_type(&constraint.ty);
            let tt = self.tysys.type_table.borrow();
            if tt.contains_type_param(expected)
                || tt.contains_type_param(actual)
                || expected == actual
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

    /// The trait a bound's own reference site names.
    pub(super) fn bound_trait_def(&self, site: crate::ast::AstId) -> Option<DefId> {
        self.tysys.resolutions.declared(site)
    }

    /// Whether `type_arg` satisfies `trait_name`, registering its associated
    /// types when it does. Asking is what records an on-demand derivation
    /// request, so callers that do not report the answer still ask.
    pub(super) fn check_and_register_bound(&mut self, type_arg: TypeId, trait_: DefId) -> bool {
        if !self.tysys.type_implements_trait(
            &self.annotate_ctx,
            &self.type_lookup(),
            type_arg,
            trait_,
        ) {
            return false;
        }
        self.register_assoc_types_for_concrete_type_and_trait(type_arg, trait_);
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
        trait_: crate::defs::DefId,
    ) {
        // Get the base type name and concrete type args for impl block lookup.
        // For newtypes, follow the chain to the underlying type to find the trait impl,
        // but registration (below) still uses concrete_type_id so the monomorphizer can
        // resolve e.g. `MyBytes::Iter` when `MyBytes` is a newtype over `List<u8>`.
        let (type_name, concrete_type_args) = {
            let tt = self.tysys.type_table.borrow();
            let effective_id = tt.representation_head(concrete_type_id);
            let list_name = tt.compiler_struct_fq_name(crate::compiler_item::CompilerItem::List);
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
            type_params: Vec<ast::GenericParam>,
            impl_ty_param_names: Vec<String>,
            assoc_types: Vec<ast::AssociatedTypeBinding>,
            /// The trait this block implements, as its own header names it —
            /// the key the registration must use.
            trait_key: crate::defs::DefId,
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
                    if header.trait_ref == Some(trait_) && !header.associated_types.is_empty() {
                        let impl_ty_param_names: Vec<String> = match &header.ty {
                            ast::Type::Generic(g) => g
                                .args
                                .iter()
                                .filter_map(|arg| {
                                    if let ast::Type::Named(named) = arg {
                                        Some(named.name.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                            _ => vec![],
                        };
                        let Some(trait_key) = header
                            .fq_trait(&self.tysys.resolutions)
                            .and_then(|t| t.canonical())
                        else {
                            continue;
                        };
                        let Some(trait_type) = header.trait_type.clone() else {
                            continue;
                        };
                        result.push(ImplInfo {
                            type_params: header.type_params.clone(),
                            impl_ty_param_names,
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
            // not whatever the enclosing frame was implementing.
            scope.annotate_ctx.trait_ctx.self_type = Some(concrete_type_id);

            // Bind impl type params to concrete type args.
            // For `impl<T> IntoIterator for List<T>` with List<u8>:
            // impl_ty_param_names = ["T"], concrete_type_args = [u8_typeid]
            // → set current_type_params["T"] = (0, u8_typeid)
            for (i, tp_name) in info.impl_ty_param_names.iter().enumerate() {
                if let Some(&concrete_arg) = concrete_type_args.get(i) {
                    scope.annotate_ctx.trait_ctx.type_params.insert(
                        tp_name.clone(),
                        BinderInScope::undeclared(i as u32, concrete_arg),
                    );
                }
            }
            // Add bounds from type param declarations
            for param in &info.type_params {
                if !param.bounds.is_empty() {
                    scope
                        .annotate_ctx
                        .trait_ctx
                        .type_param_bounds
                        .entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.clone());
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
            trait_key: crate::defs::DefId,
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
                let impl_type_name = super::trait_env::get_type_name_static(&header.ty);
                let Some(blanket_param) = header
                    .type_params
                    .iter()
                    .find(|tp| tp.name == impl_type_name && !tp.bounds.is_empty())
                else {
                    continue;
                };
                // Check if the concrete type satisfies the blanket param's bounds
                let bounds_ok = blanket_param.bounds.iter().all(|bound| {
                    self.bound_trait_def(bound.id).is_some_and(|trait_| {
                        self.tysys.type_implements_trait(
                            &self.annotate_ctx,
                            &self.type_lookup(),
                            concrete_type_id,
                            trait_,
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

            // Bind the blanket type param to the concrete type
            // For `impl<I: Iterator> IntoIterator for I` with StrUtf8ByteIter:
            // → set current_type_params["I"] = (0, StrUtf8ByteIter_typeid)
            scope.annotate_ctx.trait_ctx.self_type = Some(concrete_type_id);
            scope.annotate_ctx.trait_ctx.type_params.insert(
                info.blanket_param_name.clone(),
                BinderInScope::undeclared(0, concrete_type_id),
            );
            scope
                .annotate_ctx
                .trait_ctx
                .type_param_bounds
                .insert(info.blanket_param_name.clone(), info.blanket_param_bounds);

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
                            crate::tir::TraitRef::bare(trait_key),
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
    ) -> Option<ResolvedTraitMethod> {
        // A user-written impl first, then the Eq / Ord auto-derive fallback.
        // Both fix their return types (`bool`, `Ordering`) whatever a user impl
        // writes, so normalize here: `find_arithmetic_trait_impl` would default
        // `output_type` to the receiver type absent a `type Output`. The set and
        // the types come from `TypeSystem::auto_derive_by_trait`.
        let auto_derive = self.tysys.auto_derive_by_trait(trait_name);
        let (info_trait_name, self_kind, param_types, return_type, impl_def) = if let Some(info) =
            self.find_arithmetic_trait_impl(struct_name, lookup_type_id, trait_, method_name, None)
        {
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
            && let Some(trait_) = self.tysys.compiler_trait_def(item)
            && self.tysys.type_implements_trait(
                &self.annotate_ctx,
                &self.type_lookup(),
                lookup_type_id,
                trait_,
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
        if !self.tysys.auto_derive_eligible_kind(base_type_id) {
            return None;
        }
        let trait_ = self.tysys.compiler_trait_def(item)?;
        if !self.tysys.type_implements_trait(
            &self.annotate_ctx,
            &self.type_lookup(),
            base_type_id,
            trait_,
        ) {
            return None;
        }
        let ref_self_ty = self
            .tysys
            .type_table
            .borrow_mut()
            .intern(ResolvedType::Ref(base_type_id));
        let method_info = MethodInfo {
            method_def: None,
            return_type,
            self_kind: ast::SelfKind::Ref,
            param_types: vec![ref_self_ty],
            param_is_mut: vec![false],
            param_defaults: vec![None],
            param_names: vec!["other".to_string()],
            owner: MethodOwner::Receiver,
            cm_name: None,
            is_ref_impl: false,
            method_type_param_ids: vec![],
            method_own_params: vec![],
            impl_module: None,
            from_concrete_impl: false,
            consumes_self: false,
            inherent_visibility: None,
        };
        // The receiver's declaration names the module the derived impl belongs
        // to; `auto_derive_eligible_kind` above already established it is one.
        let impl_module_source = self
            .tysys
            .type_table
            .borrow()
            .nominal_def(base_type_id)
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
            bound_depth: 0,
            impl_struct_name: struct_name.to_string(),
            impl_struct_fq: self.tysys.fq_receiver_head(base_type_id),
            is_blanket_ref_impl: false,
            is_variadic_impl: false,
        })
    }
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
