//! Transient annotate-walk scope and its RAII guards.
//!
//! Rule: scope state is mutated only through the guards and `with_*`
//! helpers in this file — every entry has exactly one panic-safe restore
//! path (WEP 2026-05-26).

use std::borrow::Borrow;
use std::cell::RefCell;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{TraitRef, TypeId};
use crate::type_table::TypeTable;

use super::coercion::PendingLiterals;
use super::sem::decls::PendingBounds;
use super::trait_env::{InheritedBound, ViaClause};
use super::trait_query::SelfBinding;
use super::type_resolution::ParamSpace;
use super::types::TypeError;
use super::util;
use super::{AbstractSelections, Elaborator};
use crate::defs::{DefId, DefTable};
use crate::name::{FqTraitName, FqTypeName, repeat_pack_name};
use crate::resolve::{Resolution, Resolutions};
use crate::token::Span;

/// A type parameter as the declaration `owner` writes it: a method's `U` and
/// its impl's `U` are two parameters, whatever a frame calls them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParamRef {
    pub(super) owner: Option<DefId>,
    /// Where a parameter no item owns is written: a generic associated type's
    /// own, which only its family declares.
    pub(super) family_decl: Option<ast::AstId>,
    pub(super) name: String,
}

impl ParamRef {
    pub(super) fn of(owner: DefId, name: &str) -> Self {
        Self {
            owner: Some(owner),
            family_decl: None,
            name: name.to_string(),
        }
    }

    /// The parameter `param` declares.
    pub(super) fn declared(param: &ast::GenericParam, resolutions: &Resolutions) -> Self {
        Self::of_binder(param.id, &param.name, resolutions)
    }

    /// The parameter a reference site written `name` resolves to, if it
    /// resolves to one.
    pub(super) fn at_site(site: ast::AstId, name: &str, resolutions: &Resolutions) -> Option<Self> {
        match resolutions.get(site) {
            Resolution::Binder(binder) => Some(Self::of_binder(binder, name, resolutions)),
            Resolution::Def(_) | Resolution::Projection(_) | Resolution::Unresolved => None,
        }
    }

    fn of_binder(binder: ast::AstId, name: &str, resolutions: &Resolutions) -> Self {
        let owner = resolutions.binder_owner(binder);
        Self {
            owner,
            family_decl: owner.is_none().then_some(binder),
            name: name.to_string(),
        }
    }

    /// `params` paired with `args`, one for one.
    pub(super) fn space(
        params: &[ast::GenericParam],
        args: &[TypeId],
        resolutions: &Resolutions,
    ) -> ParamSpace {
        params
            .iter()
            .map(|param| Self::declared(param, resolutions))
            .zip(args.iter().copied())
            .collect()
    }

    /// The parameter as a type name spells it.
    pub(super) fn binder(&self, defs: &DefTable) -> FqTypeName {
        match self.owner {
            Some(owner) => FqTypeName::binder_of(defs, owner, &self.name),
            None => FqTypeName::binder(&self.name),
        }
    }
}

/// A name bound in a type-parameter scope: its slot, the type it stands for,
/// and the parameter it stands in for.
#[derive(Clone, Copy, Debug)]
pub(super) struct BinderInScope {
    /// Slot in the enclosing item's parameter list.
    pub(super) index: u32,
    pub(super) type_id: TypeId,
    /// Where the parameter is written, which tells a method's parameter from
    /// the impl receiver's binder of the same letter. `None` for a name no
    /// parameter declares here: `Self`, or one bound to a site's argument for
    /// an item's parameter, which [`Self::owner`] names instead.
    pub(super) decl: Option<ast::AstId>,
    /// The declaration whose parameter a binding to a site's argument stands
    /// in for.
    pub(super) owner: Option<DefId>,
    /// Declared `affine T`: an argument for it may be move-only.
    pub(super) is_affine: bool,
}

impl BinderInScope {
    /// A binder the source declares, named by its own node.
    pub(super) fn declared(index: u32, type_id: TypeId, param: &ast::GenericParam) -> Self {
        Self {
            index,
            type_id,
            decl: Some(param.id),
            owner: None,
            is_affine: param.is_affine,
        }
    }

    /// The parameter `param` bound to a site's argument.
    pub(super) fn standing_for(index: u32, type_id: TypeId, param: &ParamRef) -> Self {
        Self {
            owner: param.owner,
            decl: param.family_decl,
            ..Self::undeclared(index, type_id)
        }
    }

    /// A binder `param` declares where one does, else an undeclared one.
    pub(super) fn of(index: u32, type_id: TypeId, param: Option<&ast::GenericParam>) -> Self {
        match param {
            Some(param) => Self::declared(index, type_id, param),
            None => Self::undeclared(index, type_id),
        }
    }

    /// What tells this binder from another standing for the same type: the
    /// parameter it stands in for, and its slot, since two parameters of one
    /// owner bound to a site's arguments name no declaration apart.
    pub(super) fn key(&self) -> BinderKey {
        BinderKey {
            type_id: self.type_id,
            index: self.index,
            decl: self.decl,
            owner: self.owner,
        }
    }

    /// Whether `other` is this same parameter bound again.
    fn stands_for_same(&self, other: &Self) -> bool {
        self.type_id == other.type_id && self.decl == other.decl && self.owner == other.owner
    }

    /// A name in scope that no parameter declares — see [`Self::decl`].
    pub(super) fn undeclared(index: u32, type_id: TypeId) -> Self {
        Self {
            index,
            type_id,
            decl: None,
            owner: None,
            is_affine: false,
        }
    }
}

/// A binder's identity: see [`BinderInScope::key`]. Two parameters standing
/// for one type are two binders, each bounded by what it declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BinderKey {
    type_id: TypeId,
    index: u32,
    decl: Option<ast::AstId>,
    owner: Option<DefId>,
}

/// Whose bounds answer for a type: the binder a written name reaches, or the
/// type itself, which answers to every bound in force on it.
#[derive(Clone, Copy, Debug)]
pub(super) enum BoundsOf {
    Binder(BinderKey),
    Type(TypeId),
}

impl BoundsOf {
    /// The type the bounds hold of.
    pub(super) fn base(self) -> TypeId {
        match self {
            Self::Binder(key) => key.type_id,
            Self::Type(ty) => ty,
        }
    }
}

/// One binder of a frame: see [`TraitContext::binders`].
pub(super) struct FrameBinder<'a> {
    pub(super) type_id: TypeId,
    pub(super) is_affine: bool,
    pub(super) bounds: Vec<&'a ScopedBound>,
}

/// A bound a frame carries, with the chain it came through when it was reached
/// through a supertrait clause rather than written here.
///
/// `inherited` names the trait the written bound resolved to and the clauses
/// from there down to the one that declared `bound`. `bound`'s own types are
/// written in that last trait's parameter space, so a reader resolves them
/// through the chain instead of reading them here (WEP-2026-08-12).
#[derive(Clone)]
pub(super) struct ElaboratedBound {
    pub(super) bound: ast::TraitBound,
    pub(super) inherited: Option<(DefId, Vec<ViaClause>)>,
    /// Whose `Self` this bound's written types mean.
    pub(super) self_type: BoundSelf,
    /// The [`ScopedBound::space`] of the bound this was reached from.
    pub(super) space: ParamSpace,
}

/// Whose `Self` a bound's written types mean. The two cases are not the same
/// type, so a reader that supplies one for the other resolves a projection off
/// the wrong receiver (#2112).
#[derive(Clone, Copy, Debug)]
pub(super) enum BoundSelf {
    /// The frame that wrote the bound; `None` where that frame binds no `Self`.
    Frame(Option<SelfBinding>),
    /// The type being bounded. A supertrait clause and a declared parameter
    /// default are both written in the trait's own space, where `Self` is
    /// whichever type the bound is standing on.
    Bounded,
}

impl BoundSelf {
    /// The `Self` to resolve under, for a bound standing on `bounded` that
    /// `declaring` wrote.
    pub(super) fn at(self, bounded: TypeId, declaring: DefId) -> Option<SelfBinding> {
        match self {
            Self::Frame(binding) => binding,
            Self::Bounded => Some(SelfBinding {
                type_id: bounded,
                declaring_trait: Some(declaring),
            }),
        }
    }
}

/// What `Self::X` names in a frame: the bindings resolved so far, and the
/// impl whose bindings are being resolved, so one naming a sibling written
/// after it (`type Ints = Self::Buf<i32>;` before `type Buf<E>`) resolves that
/// one first. One value, so a frame leaving them behind leaves both.
#[derive(Clone, Default)]
pub(super) struct AssocBindings {
    pub(super) resolved: IndexMap<String, TypeId>,
    pub(super) pending: Option<Rc<ImplBindings>>,
}

impl AssocBindings {
    /// Forget every binding, resolved or pending.
    pub(super) fn clear(&mut self) {
        self.resolved.clear();
        self.pending = None;
    }
}

/// An impl's associated-type bindings and where they are read; see
/// [`AssocBindings::pending`].
pub(super) struct ImplBindings {
    /// The impl block they are written in.
    pub(super) impl_id: ast::AstId,
    pub(super) site: Option<(DefId, FamilySite)>,
    /// The impl's frame, its bindings left out, which each binding is
    /// resolved in.
    pub(super) frame: TraitContext,
    pub(super) bindings: Vec<ast::AssociatedTypeBinding>,
    /// What each is bound to once resolved, shared by every scope the
    /// resolution nests, which each restore their own frame on leaving.
    pub(super) resolved: RefCell<IndexMap<String, TypeId>>,
}

/// Where a generic associated type's declaration is read: the type standing
/// for the trait's `Self`, and the arguments its parameters take there, one
/// per type parameter — `None` where a bare bound left one unwritten.
#[derive(Clone, Debug)]
pub(super) struct FamilySite {
    pub(super) base: TypeId,
    pub(super) trait_args: Option<Vec<TypeId>>,
}

/// A bound together with what `Self` means where it was written, which is not
/// the frame of whatever later reads it (#2112).
#[derive(Clone, Debug)]
pub(super) struct ScopedBound {
    pub(super) bound: ast::TraitBound,
    /// What `Self` meant where the bound was written. `None` in a frame that
    /// binds none, where a `Self`-rooted spelling is rejected at the declaration.
    pub(super) self_binding: Option<SelfBinding>,
    /// Names the bound's written types read that the frame reading it does
    /// not bind: a generic associated type's parameters, at the arguments a
    /// projection gives them. Bound for the bound's own types alone, so a
    /// frame's parameter of the same name is untouched elsewhere.
    pub(super) space: ParamSpace,
}

impl ScopedBound {
    pub(super) fn new(bound: ast::TraitBound, self_binding: Option<SelfBinding>) -> Self {
        Self {
            bound,
            self_binding,
            space: ParamSpace::new(),
        }
    }

    /// This bound with its written types read in `space`.
    pub(super) fn in_space(self, space: ParamSpace) -> Self {
        Self { space, ..self }
    }

    /// What `param` declares, pinned to one frame's `Self`. An `fn` bound is left
    /// out: it is realised in the parameter's own type, so nothing reaches it
    /// through a trait name, which is the only way a binder's bounds are read.
    pub(super) fn pin_declared(
        param: &ast::GenericParam,
        self_binding: Option<SelfBinding>,
    ) -> Vec<Self> {
        Self::pin_all(&param.real_bounds(), self_binding)
    }

    /// Pin every bound in `bounds` to one frame's `Self`.
    pub(super) fn pin_all(
        bounds: &[ast::TraitBound],
        self_binding: Option<SelfBinding>,
    ) -> Vec<Self> {
        bounds
            .iter()
            .cloned()
            .map(|bound| Self::new(bound, self_binding))
            .collect()
    }

    /// Whose `Self` this bound's written types mean. A bound written at a frame
    /// means that frame, never the type it is standing on.
    pub(super) fn scope(&self) -> BoundSelf {
        BoundSelf::Frame(self.self_binding)
    }
}

impl Borrow<ast::TraitBound> for ScopedBound {
    fn borrow(&self) -> &ast::TraitBound {
        &self.bound
    }
}

impl Deref for ScopedBound {
    type Target = ast::TraitBound;

    fn deref(&self) -> &ast::TraitBound {
        &self.bound
    }
}

/// A trait declaration's own parameter, as the site supplying its argument
/// sees it.
pub(super) struct TraitParamFromImpl<'p, 'a, A> {
    pub(super) param: &'p ast::GenericParam,
    /// What the site wrote at this parameter's argument position, `None` where
    /// it wrote none. What a missing argument means is the caller's: a default
    /// to expand, or a parameter to leave alone.
    pub(super) arg: Option<&'a A>,
    /// The slot the parameter occupies in the trait's own numbering, counted
    /// from 1 since slot 0 is the trait's `Self`.
    pub(super) slot: u32,
    /// Whether the parameter occupies `slot` at all. An `fn`-bound one is
    /// realised in its own type, so it takes an argument position and no slot.
    pub(super) takes_a_slot: bool,
    pub(super) bounds: Vec<ScopedBound>,
}

/// Each parameter of a trait declaration with the argument a site wrote for
/// it, both its numberings, and its bounds pinned to `implementing`.
pub(super) fn trait_params_from_impl<'p, 'a, A>(
    params: &'p [ast::GenericParam],
    args: &'a [A],
    implementing: Option<SelfBinding>,
) -> Vec<TraitParamFromImpl<'p, 'a, A>> {
    let mut slot = 1;
    params
        .iter()
        .filter(|param| param.fills_impl_slot())
        .enumerate()
        .map(|(at, param)| {
            let takes_a_slot = param.is_real_type_param();
            let this = TraitParamFromImpl {
                param,
                arg: args.get(at),
                slot,
                takes_a_slot,
                bounds: ScopedBound::pin_declared(param, implementing),
            };
            slot += u32::from(takes_a_slot);
            this
        })
        .collect()
}

/// The node in `params` that declares `name`, when one does. The caller picks
/// the list, since only it knows which item bound the name (#1932).
pub(super) fn param_decl<'p>(
    params: &'p [ast::GenericParam],
    name: &str,
) -> Option<&'p ast::GenericParam> {
    params.iter().find(|p| p.name == name)
}

/// Mutable trait resolution context scoped to the current resolution site.
///
/// Groups all state that changes when entering/leaving generic scopes
/// (impl blocks, trait method lookups, etc). Use
/// [`Elaborator::enter_inherited_type_param_scope`] to mutate this safely
/// with RAII restore on drop.
#[derive(Clone, Default)]
pub(super) struct TraitContext {
    /// Type parameters currently in scope. Set when resolving generic structs,
    /// functions, or impl blocks.
    pub(super) type_params: IndexMap<String, BinderInScope>,
    /// The binders a method's parameter of the same name shadowed. No name
    /// reaches one, but the body still holds values of it, through `self`.
    pub(super) shadowed: Vec<(String, BinderInScope)>,
    /// Trait bounds in force, by the type they hold of, each with the binder
    /// that brought it and the `Self` its written types mean. Used for
    /// resolving trait methods on type params (e.g., `T.cmp()` when T: Ord).
    /// A bound is a fact about the type, not the name: a method's `T`
    /// shadowing its impl's leaves values of the impl's `T`, which still
    /// answer to its bounds.
    bounds: IndexMap<TypeId, Vec<(BinderKey, ScopedBound)>>,
    /// Associated type bindings in scope (`Self::Name` → resolved type).
    /// Set when resolving trait implementations.
    pub(super) assoc_type_bindings: AssocBindings,
    /// The bounds each generic associated type's parameter in scope carries,
    /// by its identity, instantiated where the frame stands.
    pub(super) assoc_param_bounds: IndexMap<TypeId, Vec<FqTraitName>>,
    /// Current `Self` type in scope (the type being implemented in an impl block).
    pub(super) self_type: Option<TypeId>,
    /// The trait `Self` is being elaborated against — the trait an `impl` block
    /// names. Qualifies `Self::Assoc` when `Self` is a concrete type, where
    /// there is no `Self` bound to read the declaring trait off.
    pub(super) self_trait: Option<DefId>,
    /// Set while the body under walk is a trait's default body, written
    /// against the trait's `Self` rather than this concrete one: what each
    /// of its calls selected through a declared bound, by the call's node.
    /// See [`Elaborator::abstract_selections`].
    pub(super) abstract_selections: Option<Rc<AbstractSelections>>,
}

impl TraitContext {
    /// The binders in scope declared `affine T`, which a body treats as
    /// move-only. A binder is its type, not its name: a method's `T` is not
    /// the impl's `T` it shadows.
    pub(super) fn affine_params(&self) -> Vec<TypeId> {
        self.binders()
            .into_iter()
            .filter(|binder| binder.is_affine)
            .map(|binder| binder.type_id)
            .collect()
    }

    /// Every type the frame's binders stand for, a shadowed binder's included,
    /// each once with the bounds every binder standing for it carries. A
    /// callee's parameter bound to the caller's `K` under the letter `K`
    /// stands for that same type, not a second one shadowing it.
    pub(super) fn binders(&self) -> Vec<FrameBinder<'_>> {
        let mut binders: IndexMap<TypeId, FrameBinder<'_>> = IndexMap::default();
        for binder in self.every_binder() {
            let held = binders
                .entry(binder.type_id)
                .or_insert_with(|| FrameBinder {
                    type_id: binder.type_id,
                    is_affine: false,
                    bounds: self.type_bounds(binder.type_id).collect(),
                });
            held.is_affine |= binder.is_affine;
        }
        binders.into_values().collect()
    }

    /// Every binder the frame holds, named or shadowed.
    fn every_binder(&self) -> impl Iterator<Item = &BinderInScope> {
        self.type_params
            .values()
            .chain(self.shadowed.iter().map(|(_, binder)| binder))
    }

    /// The binder whose type is `type_id`, shadowed or not.
    pub(super) fn binder(&self, type_id: TypeId) -> Option<&BinderInScope> {
        self.every_binder().find(|binder| binder.type_id == type_id)
    }

    /// Every bound in force on `ty`, a written bound two binders brought held
    /// once.
    fn type_bounds(&self, ty: TypeId) -> impl Iterator<Item = &ScopedBound> {
        let held = self.bounds.get(&ty).map_or(&[][..], Vec::as_slice);
        held.iter()
            .enumerate()
            .filter(|(i, (_, bound))| {
                !held[..*i]
                    .iter()
                    .any(|(_, earlier)| earlier.bound.id == bound.bound.id)
            })
            .map(|(_, (_, bound))| bound)
    }

    /// What the binder `key` names declares: the bounds it brought, or, for a
    /// binder that brought none, those of the type it names, bounded where it
    /// is declared.
    pub(super) fn binder_bounds(&self, key: BinderKey) -> Vec<ScopedBound> {
        let held = self.bounds.get(&key.type_id).map_or(&[][..], Vec::as_slice);
        let own: Vec<ScopedBound> = held
            .iter()
            .filter(|(by, _)| *by == key)
            .map(|(_, bound)| bound.clone())
            .collect();
        if own.is_empty() {
            return self.type_bounds(key.type_id).cloned().collect();
        }
        own
    }

    /// The bounds the binder the written name `name` reaches carries.
    pub(super) fn bounds_named(&self, name: &str) -> Vec<ScopedBound> {
        self.type_params
            .get(name)
            .map_or_else(Vec::new, |binder| self.binder_bounds(binder.key()))
    }

    /// The bounds `of` names.
    pub(super) fn bounds(&self, table: &TypeTable, of: BoundsOf) -> Vec<ScopedBound> {
        match of {
            BoundsOf::Binder(key) => self.binder_bounds(key),
            BoundsOf::Type(ty) => self
                .type_bounds(self.bounded_type(table, ty))
                .cloned()
                .collect(),
        }
    }

    /// The type `ty`'s bounds are held under: `ty`, or the pack a binder in
    /// the frame stands for whose element placeholder `ty` is, since a pack's
    /// bound holds of each member.
    pub(super) fn bounded_type(&self, table: &TypeTable, ty: TypeId) -> TypeId {
        if self.bounds.contains_key(&ty) {
            return ty;
        }
        let Some(param) = util::bound_param(table, ty) else {
            return ty;
        };
        let pack = self
            .every_binder()
            .map(|binder| binder.type_id)
            .find(|&binder| table.type_param_id(binder) == Some(param));
        pack.unwrap_or(ty)
    }

    /// Whether any binder in scope carries a bound.
    pub(super) fn has_bounds(&self) -> bool {
        self.every_binder()
            .any(|binder| self.bounds.contains_key(&binder.type_id))
    }

    /// Put `bounds` in force on `binder`'s type, brought by `binder`. A
    /// written bound it already brought is read again where this frame reads
    /// it, so it takes the new reading rather than holding twice.
    pub(super) fn add_bounds_on(&mut self, binder: &BinderInScope, bounds: Vec<ScopedBound>) {
        if bounds.is_empty() {
            return;
        }
        let key = binder.key();
        let held = self.bounds.entry(key.type_id).or_default();
        for bound in bounds {
            match held
                .iter_mut()
                .find(|(by, other)| *by == key && other.bound.id == bound.bound.id)
            {
                Some((_, other)) => *other = bound,
                None => held.push((key, bound)),
            }
        }
    }

    /// Put `bounds` in force on the binder `name` reaches.
    pub(super) fn add_bounds(&mut self, name: &str, bounds: Vec<ScopedBound>) {
        let binder = self.type_params[name];
        self.add_bounds_on(&binder, bounds);
    }

    /// Drop every bound, for a frame naming its own parameters.
    pub(super) fn clear_bounds(&mut self) {
        self.bounds.clear();
    }

    /// The binder standing for the parameter the reference `site`, written
    /// `name`, resolves to: the one declared there, or one bound to a site's
    /// argument for it. Shadowed or not, the latest frame binding it wins, and
    /// a site resolving to no parameter has none.
    pub(super) fn binder_at(
        &self,
        resolutions: &Resolutions,
        site: ast::AstId,
        name: &str,
    ) -> Option<&BinderInScope> {
        let Resolution::Binder(decl) = resolutions.get(site) else {
            return None;
        };
        self.binder_declared_at(resolutions, decl, name)
    }

    /// The binder the projection at `site`, written `name::Assoc`, projects
    /// through, as [`Self::binder_at`] finds one.
    pub(super) fn projection_base_at(
        &self,
        resolutions: &Resolutions,
        site: ast::AstId,
        name: &str,
    ) -> Option<&BinderInScope> {
        let Resolution::Projection(decl) = resolutions.get(site) else {
            return None;
        };
        self.binder_declared_at(resolutions, decl, name)
    }

    /// The binder the name written at `site` reaches, whether the site names
    /// it or projects through it.
    pub(super) fn binder_reached_at(
        &self,
        resolutions: &Resolutions,
        site: ast::AstId,
        name: &str,
    ) -> Option<&BinderInScope> {
        match resolutions.get(site) {
            Resolution::Binder(decl) | Resolution::Projection(decl) => {
                self.binder_declared_at(resolutions, decl, name)
            }
            Resolution::Def(_) | Resolution::Unresolved => None,
        }
    }

    /// The binder of the pack a signature minted for the `[..element]` written
    /// at `site`, where `element` names no pack.
    pub(super) fn repeat_pack(
        &self,
        resolutions: &Resolutions,
        site: ast::AstId,
        element: &str,
    ) -> Option<&BinderInScope> {
        let pack = resolutions.repeat_pack(site)?;
        self.binder_declared_at(resolutions, pack, &repeat_pack_name(element))
    }

    /// The binder standing for `param`, as [`Self::binder_at`] finds one.
    pub(super) fn binder_of(
        &self,
        resolutions: &Resolutions,
        param: &ast::GenericParam,
    ) -> Option<&BinderInScope> {
        self.binder_declared_at(resolutions, param.id, &param.name)
    }

    fn binder_declared_at(
        &self,
        resolutions: &Resolutions,
        decl: ast::AstId,
        name: &str,
    ) -> Option<&BinderInScope> {
        let owner = resolutions.binder_owner(decl);
        let shadowed = self
            .shadowed
            .iter()
            .rev()
            .filter(|(shadowed, _)| shadowed == name)
            .map(|(_, binder)| binder);
        self.type_params
            .get(name)
            .into_iter()
            .chain(shadowed)
            .find(|binder| {
                binder.decl == Some(decl) || (binder.owner.is_some() && binder.owner == owner)
            })
    }

    /// Bind `name` to `binder`, keeping the binder it shadows.
    ///
    /// A binder of the type the name already reaches is a second parameter
    /// standing for that type: a callee's `K` bound to the caller's `K`, or
    /// two `fn`-bound parameters of one signature. Both stay reachable by
    /// identity, each with its own bounds.
    pub(super) fn bind(&mut self, name: &str, binder: BinderInScope) {
        let Some(held) = self.type_params.insert(name.to_string(), binder) else {
            return;
        };
        if held.stands_for_same(&binder) {
            return;
        }
        self.shadowed.push((name.to_string(), held));
    }

    /// Unbind every type parameter, for a frame naming its own. The bounds
    /// stay with their binders, so binding one again restores what it carries.
    pub(super) fn clear_binders(&mut self) {
        self.type_params.clear();
        self.shadowed.clear();
    }
}

/// Everything [`Elaborator::set_self_binding`] installs, so a scoped install
/// takes and restores what `Self` means as one.
pub(super) struct SelfFrame {
    assoc_type_bindings: AssocBindings,
    self_type: Option<TypeId>,
    self_trait: Option<DefId>,
    abstract_selections: Option<Rc<AbstractSelections>>,
}

/// One open `type_implements_trait` question.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct TraitCheckFrame {
    pub(super) type_id: TypeId,
    pub(super) trait_: DefId,
    /// The arguments the asking bound wrote. Part of the key: the same trait
    /// at two instantiations is two questions.
    pub(super) wanted: Vec<FqTypeName>,
}

/// Per-function annotate-time scope, bundled so queries take one `&Scope`.
/// Every field is walk-local, so none of it belongs on the shared `TypeSystem`.
#[derive(Default)]
pub(super) struct Scope {
    pub(super) trait_ctx: TraitContext,
    /// The `(type, trait)` questions open above the current one.
    pub(super) trait_check_stack: RefCell<Vec<TraitCheckFrame>>,
    /// The module that wrote the AST being resolved, while that is not this
    /// one — a default expression taken at a site in another module. Names in
    /// it resolve in their author's module, so this replaces the walk's own
    /// frame rather than being tried alongside it (WEP 2026-04-11).
    pub(super) resolving_home: Option<ModuleSource>,
    /// While set, use→def edges are dropped: a speculative walk choosing
    /// among overloads leaves no trace, and the real walk records them.
    pub(super) suppress_reference_recording: bool,
    /// Set while a trait's default body is walked as its author wrote it: each
    /// method call a declared bound alone answers records its selection, the
    /// table [`TraitContext::abstract_selections`] later reads.
    pub(super) abstract_selections: Option<AbstractSelections>,
    /// The `(base, assoc)` pairs whose binding is being resolved right now.
    /// Two assoc types bounded through each other have no fixpoint.
    pub(super) assoc_binding_stack: IndexSet<(TypeId, String)>,
    /// The associated types whose signature is being built right now, since
    /// two may each bound the other.
    pub(super) assoc_sig_stack: IndexSet<(DefId, String)>,
    /// The impl bindings being resolved right now, since two may name each
    /// other.
    pub(super) impl_binding_stack: IndexSet<(ast::AstId, String)>,
    /// The binders whose bound closure is being built right now, since
    /// `T: Uses<T::Item>` asks for it again while it is built.
    pub(super) bound_closure_stack: IndexSet<TypeId>,
    /// The numeric literals the arguments being resolved hold at one of their
    /// call's open variables, one entry per call nested in another's argument,
    /// innermost last. See [`PendingLiterals`].
    pub(super) pending_literals: Vec<PendingLiterals>,
    /// Set while what answers the bounds the types resolved meanwhile owe is
    /// not yet in place. See [`Elaborator::holding_bounds`].
    pub(super) held_bounds: Option<Vec<PendingBounds>>,
}

impl Scope {
    /// Take what `Self` means here, leaving the frame bound to nothing.
    fn take_self_frame(&mut self) -> SelfFrame {
        SelfFrame {
            assoc_type_bindings: std::mem::take(&mut self.trait_ctx.assoc_type_bindings),
            self_type: self.trait_ctx.self_type.take(),
            self_trait: self.trait_ctx.self_trait.take(),
            abstract_selections: self.trait_ctx.abstract_selections.take(),
        }
    }

    fn restore_self_frame(&mut self, frame: SelfFrame) {
        self.trait_ctx.assoc_type_bindings = frame.assoc_type_bindings;
        self.trait_ctx.self_type = frame.self_type;
        self.trait_ctx.self_trait = frame.self_trait;
        self.trait_ctx.abstract_selections = frame.abstract_selections;
    }
}

/// RAII guard restoring `Elaborator::trait_ctx` on drop, panic-safe. Derefs to
/// the `Elaborator`, so it reads as a transparent handle inside the scope.
/// Entered through [`Elaborator::enter_inherited_type_param_scope`], which keeps
/// the parent's `trait_ctx` in place; a caller wanting a clean slate for one
/// field clears it on `scope.annotate_ctx.trait_ctx` after entering.
pub(super) struct TypeParamScope<'r, 'a, H: CompilerHost> {
    elaborator: &'r mut Elaborator<'a, H>,
    saved: TraitContext,
}

impl<'a, H: CompilerHost> Deref for TypeParamScope<'_, 'a, H> {
    type Target = Elaborator<'a, H>;
    fn deref(&self) -> &Elaborator<'a, H> {
        self.elaborator
    }
}

impl<'a, H: CompilerHost> DerefMut for TypeParamScope<'_, 'a, H> {
    fn deref_mut(&mut self) -> &mut Elaborator<'a, H> {
        self.elaborator
    }
}

impl<H: CompilerHost> TypeParamScope<'_, '_, H> {
    /// Access the saved (parent) `TraitContext`. Useful when setting up an
    /// inner scope for an impl block whose impl type refers to one of the
    /// parent's type params (blanket impl / `&T` impl / variadic impl).
    pub(super) fn saved(&self) -> &TraitContext {
        &self.saved
    }
}

impl<H: CompilerHost> Drop for TypeParamScope<'_, '_, H> {
    fn drop(&mut self) {
        self.elaborator.annotate_ctx.trait_ctx = std::mem::take(&mut self.saved);
    }
}

impl<'a, H: CompilerHost> Elaborator<'a, H> {
    /// Enter an inherited type-param scope: the current `trait_ctx` is cloned
    /// into the saved slot but left in place, so the inner work registers
    /// additional type params on top of the parent's. A caller wanting a clean
    /// slate clears the specific fields it resets on `scope.annotate_ctx
    /// .trait_ctx` after entering; everything else stays inherited.
    pub(super) fn enter_inherited_type_param_scope(&mut self) -> TypeParamScope<'_, 'a, H> {
        let saved = self.annotate_ctx.trait_ctx.clone();
        TypeParamScope {
            elaborator: self,
            saved,
        }
    }

    /// A scope binding the `params` declaration `owner` declares and no type
    /// parameter the caller brought: a declaration names its parameters itself.
    pub(super) fn enter_decl_params_scope(
        &mut self,
        owner: DefId,
        params: &[ast::GenericParam],
    ) -> TypeParamScope<'_, 'a, H> {
        let mut scope = self.enter_inherited_type_param_scope();
        let ctx = &mut scope.annotate_ctx.trait_ctx;
        ctx.clear_binders();
        ctx.clear_bounds();
        scope.register_generic_params(owner, params, 0);
        scope
    }

    /// A scope binding `impl_block`'s parameters and `Self` and nothing the
    /// caller brought: an impl block names its parameters in `impl<...>`.
    pub(super) fn enter_impl_params_scope(
        &mut self,
        impl_block: &ast::ImplBlock,
    ) -> TypeParamScope<'_, 'a, H> {
        let mut scope = self.enter_inherited_type_param_scope();
        let ctx = &mut scope.annotate_ctx.trait_ctx;
        ctx.clear_binders();
        ctx.clear_bounds();
        ctx.assoc_type_bindings.clear();
        scope.register_impl_block_params(impl_block);
        scope
    }

    /// [`Self::enter_impl_params_scope`] with the block's associated types.
    pub(super) fn enter_impl_scope(
        &mut self,
        impl_block: &ast::ImplBlock,
    ) -> TypeParamScope<'_, 'a, H> {
        let mut scope = self.enter_impl_params_scope(impl_block);
        if impl_block.trait_type.is_some() && !impl_block.is_synthesize_request {
            let trait_name = scope.impl_block_trait_name(impl_block);
            scope.register_impl_assoc_types(impl_block, trait_name.as_ref());
        }
        scope
    }

    /// Run `body` with `space` bound and no other type parameter in scope, for
    /// resolving a type the declaration wrote against its arguments.
    ///
    /// Nothing in `struct Marked<M: Mark = Zero>` means the `Zero` a caller
    /// happens to declare. A parameter past the settled arguments stays
    /// unbound, so a default reaches only the parameters to its left.
    pub(super) fn with_type_param_args<R>(
        &mut self,
        space: &[(ParamRef, TypeId)],
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let binders: IndexMap<String, BinderInScope> = space
            .iter()
            .enumerate()
            .map(|(slot, (param, type_id))| {
                (
                    param.name.clone(),
                    BinderInScope::standing_for(slot as u32, *type_id, param),
                )
            })
            .collect();
        let mut scope = self.enter_inherited_type_param_scope();
        let ctx = &mut scope.annotate_ctx.trait_ctx;
        ctx.clear_binders();
        ctx.type_params = binders;
        body(&mut scope)
    }

    /// [`Self::with_type_param_args`] layered on this frame instead of replacing
    /// it, for resolving a declaration's type at a site's arguments: `Self` and
    /// the enclosing parameters still answer, and a repeated name is shadowed.
    pub(super) fn with_type_params_bound<R>(
        &mut self,
        space: &[(ParamRef, TypeId)],
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let mut scope = self.enter_inherited_type_param_scope();
        let base = scope.annotate_ctx.trait_ctx.type_params.len() as u32;
        for (slot, (param, type_id)) in space.iter().enumerate() {
            let binder = BinderInScope::standing_for(base + slot as u32, *type_id, param);
            scope.annotate_ctx.trait_ctx.bind(&param.name, binder);
        }
        body(&mut scope)
    }

    /// Run `body` collecting into `collection` the numeric literals it meets
    /// at one of the collection's variables, and answer them with its result.
    pub(super) fn collecting_pending_literals<R>(
        &mut self,
        collection: PendingLiterals,
        body: impl FnOnce(&mut Self) -> R,
    ) -> (R, PendingLiterals) {
        let depth = self.annotate_ctx.pending_literals.len();
        self.annotate_ctx.pending_literals.push(collection);
        let result = body(self);
        let pending = self
            .annotate_ctx
            .pending_literals
            .pop()
            .expect("a walk pushes into the collection, never takes it");
        assert_eq!(
            self.annotate_ctx.pending_literals.len(),
            depth,
            "a nested collection is popped by the call that pushed it"
        );
        (result, pending)
    }

    /// Run `body` with use→def reference recording suppressed. See
    /// [`Scope::suppress_reference_recording`].
    pub(super) fn with_reference_recording_suppressed<R>(
        &mut self,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        util::replaced(
            self,
            |e| &mut e.annotate_ctx.suppress_reference_recording,
            true,
            body,
        )
        .0
    }

    /// Run `body` with `key` on the walk `stack` selects, or answer `None`
    /// where it already is: a question asked again inside itself has no answer.
    pub(super) fn unless_on_walk<K: Eq + Hash + Clone, R>(
        &mut self,
        stack: fn(&mut Scope) -> &mut IndexSet<K>,
        key: K,
        body: impl FnOnce(&mut Self) -> Option<R>,
    ) -> Option<R> {
        struct Pop<'r, 'a, H: CompilerHost, K: Eq + Hash> {
            elaborator: &'r mut Elaborator<'a, H>,
            stack: fn(&mut Scope) -> &mut IndexSet<K>,
            key: K,
        }
        impl<H: CompilerHost, K: Eq + Hash> Drop for Pop<'_, '_, H, K> {
            fn drop(&mut self) {
                (self.stack)(&mut self.elaborator.annotate_ctx).shift_remove(&self.key);
            }
        }
        if !stack(&mut self.annotate_ctx).insert(key.clone()) {
            return None;
        }
        let guard = Pop {
            elaborator: self,
            stack,
            key,
        };
        body(guard.elaborator)
    }

    /// Make `Self` stand for `binding` for the rest of this scope, every part
    /// of it at once: installing one part is the frame that leaks (#2112).
    pub(super) fn set_self_binding(&mut self, binding: SelfBinding) {
        self.annotate_ctx.trait_ctx.assoc_type_bindings.clear();
        self.annotate_ctx.trait_ctx.self_trait = binding.declaring_trait;
        self.annotate_ctx.trait_ctx.self_type = Some(binding.type_id);
        self.annotate_ctx.trait_ctx.abstract_selections = None;
    }

    /// [`Self::set_self_binding`] for the duration of `body`, restoring the
    /// frame it replaced on return (panic-safe).
    pub(super) fn with_self_binding<R>(
        &mut self,
        binding: SelfBinding,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        struct Restore<'r, 'a, H: CompilerHost> {
            elaborator: &'r mut Elaborator<'a, H>,
            saved: Option<SelfFrame>,
        }
        impl<H: CompilerHost> Drop for Restore<'_, '_, H> {
            fn drop(&mut self) {
                let saved = self.saved.take().expect("saved self frame present");
                self.elaborator.annotate_ctx.restore_self_frame(saved);
            }
        }
        let saved = self.annotate_ctx.take_self_frame();
        self.set_self_binding(binding);
        let guard = Restore {
            elaborator: self,
            saved: Some(saved),
        };
        body(guard.elaborator)
    }

    /// [`Self::with_self_binding`] where there is a binding, and `body` as it
    /// stands where there is none.
    pub(super) fn under_self_binding<R>(
        &mut self,
        binding: Option<SelfBinding>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        match binding {
            Some(binding) => self.with_self_binding(binding, body),
            None => body(self),
        }
    }

    /// Run `body` with [`Scope::resolving_home`] replaced by `module`. Unlike
    /// [`Self::with_self_binding`], `None` here is a value: it returns the walk
    /// to its own module.
    pub(super) fn with_resolving_home<R>(
        &mut self,
        module: Option<ModuleSource>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        util::replaced(self, |e| &mut e.annotate_ctx.resolving_home, module, body).0
    }

    /// The supertraits `bounds` carry, each with the trait it was reached from
    /// and the chain that reaches it. For a caller that reads the inherited
    /// bound's own types, which are written in the declaring trait's space.
    pub(super) fn inherited_bounds_of(
        &self,
        bounds: &[ast::TraitBound],
    ) -> Vec<(InheritedBound, DefId)> {
        bounds
            .iter()
            .filter(|bound| bound.names_a_trait())
            .filter_map(|bound| self.trait_decl_of(bound))
            .flat_map(|root| {
                self.supertraits_of(root)
                    .iter()
                    .map(move |inherited| (inherited.clone(), root))
            })
            .collect()
    }

    /// `bounds` expanded to include every bound's supertraits, so a declared
    /// `T: Ord` also demands `Eq`. One declaration stays one bound however
    /// spelled, so an alias never competes with its original.
    pub(super) fn elaborate_bounds(&self, bounds: &[ScopedBound]) -> Vec<ElaboratedBound> {
        // Each entry carries the declaration it merged on, so a bound that has
        // none — a `fn(..)` bound — cannot shift the ones after it.
        let mut out: Vec<(ElaboratedBound, Option<DefId>)> = Vec::with_capacity(bounds.len());
        for scoped in bounds {
            let bound = &scoped.bound;
            self.merge_bound(&mut out, bound, None, scoped.scope(), &scoped.space);
            if !bound.names_a_trait() {
                continue;
            }
            let Some(root) = self.trait_decl_of(bound) else {
                continue;
            };
            for inherited in self.supertraits_of(root) {
                // A supertrait clause is written in the declaring trait's own
                // space, not in the frame that wrote the bound reaching it.
                self.merge_bound(
                    &mut out,
                    &inherited.bound,
                    Some((root, inherited.via.clone())),
                    BoundSelf::Bounded,
                    &scoped.space,
                );
            }
        }
        out.into_iter().map(|(bound, _)| bound).collect()
    }

    /// Add `bound` unless the list already holds its declaration, in which case
    /// the merged bound carries what either spelling wrote — `T: Iterator +
    /// Iterator<Item = i32>` is one bound, and it carries the associated type.
    ///
    /// A `fn(..)` bound names no trait, so it has no declaration to merge on
    /// and falls back to merging on its own site: two bounds written at two
    /// sites stay two bounds, and only a bound repeated at one site merges.
    fn merge_bound(
        &self,
        out: &mut Vec<(ElaboratedBound, Option<DefId>)>,
        bound: &ast::TraitBound,
        inherited: Option<(DefId, Vec<ViaClause>)>,
        self_type: BoundSelf,
        space: &ParamSpace,
    ) {
        let entry = || ElaboratedBound {
            bound: bound.clone(),
            inherited: inherited.clone(),
            self_type,
            space: space.clone(),
        };
        if !bound.names_a_trait() {
            if !out.iter().any(|(b, _)| b.bound.id == bound.id) {
                out.push((entry(), None));
            }
            return;
        }
        let decl = self.trait_decl_of(bound);
        // A bound that names no declaration falls back to its spelling, so an
        // erroring program still reports one bound rather than one per mention.
        // Only bounds that both write nothing are one bound. A written argument
        // names an instantiation, and a bare bound names the declared default,
        // so `Pick + Pick<String>` and `Eq<String> + Eq<StrSlice>` each ask for
        // two impls. A repeat costs a second diagnostic and nothing else.
        let mergeable = |b: &ast::TraitBound| b.type_args.is_empty() && bound.type_args.is_empty();
        let duplicate = match decl {
            Some(decl) => out
                .iter_mut()
                .find(|(b, d)| *d == Some(decl) && mergeable(&b.bound)),
            None => out.iter_mut().find(|(b, d)| {
                d.is_none() && b.bound.written_name() == bound.written_name() && mergeable(&b.bound)
            }),
        };
        if let Some((existing, _)) = duplicate {
            if existing.bound.assoc_types.is_empty() {
                existing.bound.assoc_types.clone_from(&bound.assoc_types);
            }
            return;
        }
        out.push((entry(), decl));
    }

    /// The transitive supertraits of trait `decl`.
    fn supertraits_of(&self, decl: DefId) -> &[InheritedBound] {
        self.tysys.trait_env.supertrait_closure(&decl)
    }

    /// The type-parameter ids the enclosing generic scope owns: a slot bound to
    /// one of them is the caller forwarding its generics, not inference giving
    /// up. A `..P` pack owns its element placeholder `TypeParam { P, index }`
    /// too — the spelling `make_mapped_type_pack` gives a pack element.
    pub(super) fn scope_type_param_ids(&self) -> Vec<TypeId> {
        let mut tt = self.tysys.type_table.borrow_mut();
        self.annotate_ctx
            .trait_ctx
            .binders()
            .into_iter()
            .flat_map(|binder| {
                let elem = tt
                    .is_type_pack(binder.type_id)
                    .then(|| tt.pack_placeholder(binder.type_id));
                std::iter::once(binder.type_id).chain(elem)
            })
            .collect()
    }

    /// Bind each of `params` written `<F: fn(...)>` that the frame does not
    /// bind yet to that function type, as [`Self::register_generic_params`]
    /// does: such a parameter is the signature, filling no slot, so its index
    /// is never read. An impl's placed nowhere in its target is bound by
    /// nothing else.
    pub(super) fn bind_fn_bound_params(&mut self, params: &[ast::GenericParam]) {
        for param in params.iter().filter(|p| !p.is_pack && !p.is_effect) {
            let index = self.annotate_ctx.trait_ctx.type_params.len() as u32;
            let Some(sig) = param.bounds.iter().find_map(|b| b.fn_signature.as_ref()) else {
                continue;
            };
            if self
                .annotate_ctx
                .trait_ctx
                .type_params
                .contains_key(&param.name)
            {
                continue;
            }
            let type_id = self.resolve_type(&ast::Type::Function(sig.clone()));
            self.bind_param(
                &param.name,
                BinderInScope::declared(index, type_id, param),
                Vec::new(),
            );
        }
    }

    /// Register the generic parameters `owner` declares as `TypeParam` /
    /// `TypePack` ids in the current `trait_ctx`, starting from `offset`. Skips
    /// effect params. Returns the next free index (i.e. `offset +
    /// non_effect_count`).
    ///
    /// Trait bounds attached to each parameter are recorded with its binder so
    /// trait-method lookups on the parameter work.
    pub(super) fn register_generic_params(
        &mut self,
        owner: DefId,
        params: &[ast::GenericParam],
        offset: u32,
    ) -> u32 {
        let mut idx = offset;
        for tp in params.iter().filter(|p| !p.is_effect) {
            // `<F: fn(...)>` binds the parameter directly to the bound's function
            // type: the bound is surface syntax for "F is exactly this
            // signature". Such params take no slot, so a use site's arguments,
            // which fill only the parameters binding a type, line up with them.
            let fn_bound_sig = if tp.is_pack {
                None
            } else {
                tp.bounds.iter().find_map(|b| b.fn_signature.as_ref())
            };
            let (type_id, consumed_index) = match fn_bound_sig {
                Some(sig) => (self.resolve_type(&ast::Type::Function(sig.clone())), false),
                None => (
                    self.tysys.type_table.borrow_mut().make_declared_param(
                        owner,
                        tp.name.clone(),
                        idx,
                        tp.is_pack,
                    ),
                    true,
                ),
            };
            let bounds = self.scoped_bounds(tp);
            self.bind_param(&tp.name, BinderInScope::declared(idx, type_id, tp), bounds);
            if consumed_index {
                idx += 1;
            }
        }
        idx
    }

    /// What `Self` means at this frame: the type it stands for and the trait
    /// that declares the names projected off it. `None` in a frame binding none.
    pub(super) fn self_binding(&self) -> Option<SelfBinding> {
        Some(self.self_binding_on(self.annotate_ctx.trait_ctx.self_type?))
    }

    /// This frame's `Self`, standing on `type_id` instead. A method's
    /// declaration projects `Self::Assoc` off its receiver, under the trait the
    /// frame implements.
    pub(super) fn self_binding_on(&self, type_id: TypeId) -> SelfBinding {
        SelfBinding {
            type_id,
            declaring_trait: self.annotate_ctx.trait_ctx.self_trait,
        }
    }

    /// What `Self` means inside an `impl` block: its target, under the trait
    /// the block implements.
    pub(super) fn impl_self_binding(
        &mut self,
        impl_type: &ast::Type,
        trait_type: Option<&ast::Type>,
    ) -> SelfBinding {
        SelfBinding {
            type_id: self.resolve_type(impl_type),
            declaring_trait: trait_type.and_then(|t| self.impl_trait_decl(t)),
        }
    }

    /// Bind `name` to `binder` and to what it is bounded by, in one step.
    ///
    /// The two are one fact: a binder without its bounds dispatches on nothing,
    /// and bounds without a binder are a constraint no name reaches. The bounds
    /// stay with the binder, so a method parameter shadowing an impl's
    /// dispatches on its own traits alone, and values of the impl's still
    /// answer to the impl's.
    pub(super) fn bind_param(
        &mut self,
        name: &str,
        binder: BinderInScope,
        bounds: Vec<ScopedBound>,
    ) {
        let ctx = &mut self.annotate_ctx.trait_ctx;
        ctx.bind(name, binder);
        ctx.add_bounds_on(&binder, bounds);
    }

    /// What `param` declares, pinned to the `Self` this frame binds — what the
    /// bounds' written types mean, whatever frame later reads them.
    pub(super) fn scoped_bounds(&mut self, param: &ast::GenericParam) -> Vec<ScopedBound> {
        let self_binding = self.self_binding();
        if self_binding.is_none() {
            // Every bound the parameter declares, not the trait-named ones
            // below: an `fn` bound writes types rooted at `Self` too.
            self.reject_self_in_bounds(&param.name, &param.bounds);
        }
        ScopedBound::pin_declared(param, self_binding)
    }

    /// The parameters of `owning_trait`'s associated type `assoc`, as the
    /// `ResolvedType::AssocParam`s a binding is written over.
    pub(super) fn family_params(&mut self, owning_trait: DefId, assoc: &str) -> Vec<TypeId> {
        let names: Vec<String> = self
            .tysys
            .trait_env
            .assoc_type_decl(&owning_trait, assoc)
            .map(|decl| decl.type_params.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default();
        self.tysys
            .type_table
            .borrow_mut()
            .family_params(owning_trait, assoc, &names)
    }

    /// Run `body` with the parameters of `owning_trait`'s associated type
    /// `assoc` in scope over this frame, under the names `written` gives them.
    /// Each is the family's own parameter at its position, so a declaration
    /// and an impl renaming it reach one type. At a `site`, each also carries
    /// the bounds the declaration gives it there, by its identity.
    pub(super) fn with_assoc_params<R>(
        &mut self,
        owning_trait: DefId,
        assoc: &str,
        written: &[ast::GenericParam],
        site: Option<&FamilySite>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if written.is_empty() {
            return body(self);
        }
        let params = self.family_params(owning_trait, assoc);
        let mut scope = self.enter_inherited_type_param_scope();
        let base = scope.annotate_ctx.trait_ctx.type_params.len() as u32;
        // An impl writing more parameters than the trait declares is reported
        // where the two are compared, and the excess binds nothing.
        for (index, (param, &type_id)) in written.iter().zip(&params).enumerate() {
            let bounds = scope.scoped_bounds(param);
            scope.bind_param(
                &param.name,
                BinderInScope::declared(base + index as u32, type_id, param),
                bounds,
            );
        }
        if let Some(site) = site {
            let declared = scope.param_bounds_at(owning_trait, assoc, site, &params);
            for (&param, refs) in params.iter().zip(&declared) {
                scope.give_assoc_param_bounds(param, refs);
            }
        }
        body(&mut scope)
    }

    /// Have the family parameter `param` carry `refs` in this frame, by its
    /// identity.
    pub(super) fn give_assoc_param_bounds(&mut self, param: TypeId, refs: &[TraitRef]) {
        let named: Vec<FqTraitName> = {
            let table = RefCell::borrow(&self.tysys.type_table);
            refs.iter().map(|r| table.trait_ref_name(r)).collect()
        };
        self.annotate_ctx
            .trait_ctx
            .assoc_param_bounds
            .insert(param, named);
    }

    /// The bounds `owning_trait`'s associated type `assoc` declares on each of
    /// its parameters, at `site` and at the arguments `args`.
    pub(super) fn param_bounds_at(
        &mut self,
        owning_trait: DefId,
        assoc: &str,
        site: &FamilySite,
        args: &[TypeId],
    ) -> Vec<Vec<TraitRef>> {
        // An impl may bind a name its trait does not declare, which is
        // reported where the two are compared. A family named in its own
        // declaration's bounds is asked while its signature is built, and the
        // parameter it binds is its own.
        if self
            .tysys
            .trait_env
            .assoc_type_decl(&owning_trait, assoc)
            .is_none()
        {
            return Vec::new();
        }
        let Some(sig) = self.assoc_type_sig(owning_trait, assoc) else {
            return Vec::new();
        };
        let mut table = self.tysys.type_table.borrow_mut();
        sig.param_bounds
            .iter()
            .map(|refs| {
                table.instantiate_trait_refs(
                    &sig,
                    refs,
                    site.base,
                    site.trait_args.as_deref(),
                    args,
                )
            })
            .collect()
    }

    /// The bounds `owning_trait`'s associated type `assoc` declares on itself
    /// (`type A: Bound`), at `site` and at the arguments `args`. Asked of an
    /// impl, never from inside the signature's own build.
    pub(super) fn bounds_at(
        &mut self,
        owning_trait: DefId,
        assoc: &str,
        site: &FamilySite,
        args: &[TypeId],
    ) -> Vec<TraitRef> {
        let sig = self.assoc_type_sig(owning_trait, assoc).unwrap_or_else(|| {
            panic!("associated type `{assoc}` of {owning_trait:?} has no signature")
        });
        let mut table = self.tysys.type_table.borrow_mut();
        table.instantiate_trait_refs(
            &sig,
            &sig.bounds,
            site.base,
            site.trait_args.as_deref(),
            args,
        )
    }

    /// What an impl binds `binding` to, its own parameters left open as the
    /// family's. `impl_site` is the trait the impl implements and where.
    pub(super) fn resolve_assoc_binding(
        &mut self,
        impl_site: Option<&(DefId, FamilySite)>,
        binding: &ast::AssociatedTypeBinding,
    ) -> TypeId {
        let Some((owning_trait, site)) = impl_site else {
            return self.resolve_type(&binding.ty);
        };
        self.with_assoc_params(
            *owning_trait,
            &binding.name,
            &binding.type_params,
            Some(site),
            |scope| scope.resolve_type(&binding.ty),
        )
    }

    /// Bind each of `impl_block`'s associated types in this frame, as `Self::X`
    /// then reads them, and answer them in the order written. One may name a
    /// sibling written after it, which [`Self::impl_binding`] resolves first.
    pub(super) fn bind_impl_assoc_types(
        &mut self,
        impl_block: &ast::ImplBlock,
    ) -> Vec<(String, TypeId)> {
        let site = self.impl_family_site(impl_block);
        let mut frame = self.annotate_ctx.trait_ctx.clone();
        frame.assoc_type_bindings = AssocBindings::default();
        self.annotate_ctx.trait_ctx.assoc_type_bindings = AssocBindings {
            resolved: IndexMap::default(),
            pending: Some(Rc::new(ImplBindings {
                impl_id: impl_block.id,
                site,
                frame,
                bindings: impl_block.associated_types.clone(),
                resolved: RefCell::default(),
            })),
        };
        impl_block
            .associated_types
            .iter()
            .map(|binding| {
                let type_id = self
                    .impl_binding(&binding.name)
                    .expect("a written binding is resolved, cycle or not");
                (binding.name.clone(), type_id)
            })
            .collect()
    }

    /// What the impl being bound binds `name` to, resolving it now where it is
    /// written but not yet resolved. `None` where the impl binds no `name`, or
    /// it is being resolved already: two bindings naming each other have no
    /// answer, and the inner naming is reported as naming nothing.
    pub(super) fn impl_binding(&mut self, name: &str) -> Option<TypeId> {
        let bindings = &self.annotate_ctx.trait_ctx.assoc_type_bindings;
        if let Some(&type_id) = bindings.resolved.get(name) {
            return Some(type_id);
        }
        let pending = Rc::clone(bindings.pending.as_ref()?);
        let binding = pending.bindings.iter().find(|b| b.name == name)?;
        let known = pending.resolved.borrow().get(name).copied();
        let type_id = if let Some(type_id) = known {
            type_id
        } else {
            // In the impl's frame, whoever names it: an asking binding's own
            // family parameters are not this one's.
            let type_id = self.unless_on_walk(
                |scope| &mut scope.impl_binding_stack,
                (pending.impl_id, name.to_string()),
                |e| {
                    let mut impl_frame = e.enter_inherited_type_param_scope();
                    impl_frame.annotate_ctx.trait_ctx = pending.frame.clone();
                    impl_frame.annotate_ctx.trait_ctx.assoc_type_bindings = AssocBindings {
                        resolved: IndexMap::default(),
                        pending: Some(Rc::clone(&pending)),
                    };
                    Some(impl_frame.resolve_assoc_binding(pending.site.as_ref(), binding))
                },
            )?;
            pending
                .resolved
                .borrow_mut()
                .insert(name.to_string(), type_id);
            type_id
        };
        self.annotate_ctx
            .trait_ctx
            .assoc_type_bindings
            .resolved
            .insert(name.to_string(), type_id);
        Some(type_id)
    }

    /// Reject a bound writing `Self` where the frame binds none. `Self::Assoc`
    /// on a free function's parameter would go unchecked rather than mean what
    /// the parameter's own name already says.
    fn reject_self_in_bounds(&mut self, param: &str, bounds: &[ast::TraitBound]) {
        let written: Vec<Span> = bounds
            .iter()
            .filter(|bound| bound.writes_self())
            .map(|bound| bound.span)
            .collect();
        for span in written {
            let _ = self.emit(TypeError::SelfInUnboundedBound {
                param: param.to_string(),
                span,
            });
        }
    }

    /// Bind a trait's declared type parameters to the impl's concrete trait
    /// arguments: `trait Foo<T, U>` against `Foo<i32, String>` registers
    /// `T → i32` and `U → String` with their bounds. Impl-level type params must
    /// already be registered, the trait args being able to name them
    /// (`impl<X> Foo<Container<X>>`). Existing entries are left untouched.
    ///
    /// `implementing` is what the trait declared these bounds' `Self` to mean.
    /// The caller passes it rather than leaving this to read ambient state it
    /// may not have set yet.
    pub(super) fn bind_trait_type_params_from_impl(
        &mut self,
        trait_type: &ast::Type,
        implementing: SelfBinding,
    ) {
        let Some(trait_decl_type_params) = implementing
            .declaring_trait
            .and_then(|trait_| self.tysys.trait_decl_type_params_of(&trait_))
        else {
            return;
        };
        let trait_args: &[ast::Type] = match trait_type {
            ast::Type::Generic(g) => &g.args,
            _ => &[],
        };
        let supplied =
            trait_params_from_impl(&trait_decl_type_params, trait_args, Some(implementing));
        for TraitParamFromImpl {
            param, arg, bounds, ..
        } in supplied
        {
            let Some(arg) = arg else {
                continue;
            };
            let resolved_arg = self.resolve_type(arg);
            let ctx = &mut self.annotate_ctx.trait_ctx;
            let binder = BinderInScope::declared(ctx.type_params.len() as u32, resolved_arg, param);
            // The impl's own parameter keeps the name, but the trait's mark
            // and bounds still reach what it binds.
            if ctx.type_params.contains_key(&param.name) {
                ctx.shadowed.push((param.name.clone(), binder));
                ctx.add_bounds_on(&binder, bounds);
                continue;
            }
            self.bind_param(&param.name, binder, bounds);
        }
    }
}
