//! The value trait resolution answers questions about. An id is a plain index,
//! so a test writes the program it needs rather than compiling source.

use crate::hashmap::{IndexMap, IndexSet};

/// A type declaration — a struct, variant, enum, flags, newtype, resource, or
/// builtin.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TypeDeclId(pub u32);

/// A trait declaration. Two same-named traits from different modules are two
/// ids, which is what keeps them distinct everywhere the order runs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TraitDeclId(pub u32);

/// An `impl` block.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ImplId(pub u32);

/// A method name, interned across the program: two traits declaring `describe`
/// share one id, which is what makes their collision one question.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MethodId(pub u32);

/// A type as selection reads it. A parameter is its position, so two targets
/// equal here are one `(Trait, Type)` pair.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SolverType {
    /// A declared type at its type arguments: `Point`, `Box_<i32>`.
    Decl(TypeDeclId, Vec<SolverType>),
    /// One of the impl's own type parameters, by position.
    Param(u32),
    /// One of the impl's own variadic type packs, by position — the `[..T]` an
    /// impl target may be.
    Pack(u32),
    Ref {
        is_mut: bool,
        inner: Box<SolverType>,
    },
    Tuple(Vec<SolverType>),
    /// An associated type of a rigid parameter, `X::Item` under `X: Holder`:
    /// a type of its own, satisfying the bounds the trait declares on it
    /// ([`TraitDef::assoc_bounds`]). A projection on a concrete type is
    /// normalized to the impl's binding before it reaches the solver.
    Projection {
        base: Box<SolverType>,
        trait_: TraitDeclId,
        assoc: AssocId,
        /// The associated type's own arguments (`i32` in `S::Buf<i32>`),
        /// empty for one that declares no parameters.
        args: Vec<SolverType>,
    },
    /// A function type's row holding an effect parameter: it stands for
    /// whatever row meets it.
    AnyRow,
}

/// An associated type of a trait: `Mul::Output`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct AssocId(pub u32);

/// What a bound says an associated type must be: the `Output = T` in
/// `impl<T: Mul<Output = T>> Product for T`, spelled with the impl's parameters.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pin {
    pub trait_: TraitDeclId,
    pub assoc: AssocId,
    pub ty: SolverType,
}

impl SolverType {
    /// Whether a [`Param`](Self::Param) or [`Pack`](Self::Pack) in `self`
    /// satisfies `pred`.
    #[must_use]
    pub fn mentions(&self, pred: &dyn Fn(&Self) -> bool) -> bool {
        match self {
            Self::Param(_) | Self::Pack(_) => pred(self),
            Self::AnyRow => false,
            Self::Decl(_, inner) | Self::Tuple(inner) => inner.iter().any(|t| t.mentions(pred)),
            Self::Ref { inner, .. } => inner.mentions(pred),
            Self::Projection { base, args, .. } => {
                base.mentions(pred) || args.iter().any(|t| t.mentions(pred))
            }
        }
    }

    /// Whether `self` or a type within it instantiates a declaration
    /// satisfying `pred`.
    #[must_use]
    pub fn mentions_decl(&self, pred: &dyn Fn(TypeDeclId) -> bool) -> bool {
        match self {
            Self::Param(_) | Self::Pack(_) | Self::AnyRow => false,
            Self::Decl(id, inner) => pred(*id) || inner.iter().any(|t| t.mentions_decl(pred)),
            Self::Tuple(inner) => inner.iter().any(|t| t.mentions_decl(pred)),
            Self::Ref { inner, .. } => inner.mentions_decl(pred),
            Self::Projection { base, args, .. } => {
                base.mentions_decl(pred) || args.iter().any(|t| t.mentions_decl(pred))
            }
        }
    }

    /// Whether `self` mentions the parameter or pack at `index`.
    #[must_use]
    pub fn mentions_param(&self, index: u32) -> bool {
        self.mentions(&|p| matches!(p, Self::Param(i) | Self::Pack(i) if *i == index))
    }

    /// Whether `self` mentions the parameter or pack at `index` other than
    /// behind a reference.
    #[must_use]
    pub fn owns_param(&self, index: u32) -> bool {
        match self {
            Self::Ref { .. } => false,
            Self::Decl(_, inner) | Self::Tuple(inner) => inner.iter().any(|t| t.owns_param(index)),
            other => other.mentions_param(index),
        }
    }

    /// `self` with each parameter and pack replaced by `arg` at its position;
    /// `None` where `arg` has none for one.
    #[must_use]
    pub fn map_params(&self, arg: &dyn Fn(u32) -> Option<Self>) -> Option<Self> {
        let each = |inner: &[Self]| -> Option<Vec<Self>> {
            inner.iter().map(|t| t.map_params(arg)).collect()
        };
        Some(match self {
            Self::Param(index) | Self::Pack(index) => arg(*index)?,
            Self::AnyRow => Self::AnyRow,
            Self::Decl(head, inner) => Self::Decl(*head, each(inner)?),
            // A pack spread in a tuple splices its elements in: `[..T]` at
            // `T = [i32, bool]` is `[i32, bool]`, not `[[i32, bool]]`.
            Self::Tuple(inner) => {
                let mut elems = Vec::with_capacity(inner.len());
                for elem in inner {
                    match (elem, elem.map_params(arg)?) {
                        (Self::Pack(_), Self::Tuple(spliced)) => elems.extend(spliced),
                        (_, mapped) => elems.push(mapped),
                    }
                }
                Self::Tuple(elems)
            }
            Self::Ref { is_mut, inner } => Self::Ref {
                is_mut: *is_mut,
                inner: Box::new(inner.map_params(arg)?),
            },
            Self::Projection {
                base,
                trait_,
                assoc,
                args,
            } => Self::Projection {
                base: Box::new(base.map_params(arg)?),
                trait_: *trait_,
                assoc: *assoc,
                args: each(args)?,
            },
        })
    }
}

/// A `Trait<Args>` written on an impl's type parameter or on a supertrait
/// clause.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ParamBound {
    pub trait_: TraitDeclId,
    /// What the bound writes for the trait's own parameters; empty asks for the
    /// declared defaults.
    pub args: Vec<SolverType>,
}

impl ParamBound {
    #[must_use]
    pub fn bare(trait_: TraitDeclId) -> Self {
        Self {
            trait_,
            args: Vec::new(),
        }
    }
}

/// One of an impl's type parameters.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct ParamDef {
    pub bounds: Vec<ParamBound>,
    pub pins: Vec<Pin>,
    /// Admits only a [`Program::copyable`] argument: the parameter is not
    /// declared `affine`. A kind, not a bound, so it ranks no impl above
    /// another.
    pub copyable: bool,
}

impl ParamDef {
    #[must_use]
    pub fn bounded(bounds: Vec<TraitDeclId>) -> Self {
        Self {
            bounds: bounds.into_iter().map(ParamBound::bare).collect(),
            ..Self::default()
        }
    }
}

/// An `impl` block, reduced to what the rules read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImplDef {
    /// The trait it implements; `None` for an inherent impl.
    pub trait_: Option<TraitDeclId>,
    /// The trait's own type arguments — `[i32]` for `impl Conv<i32> for X`.
    pub trait_args: Vec<SolverType>,
    pub target: SolverType,
    /// The impl's type parameters, in declaration order. [`SolverType::Param`]
    /// and [`SolverType::Pack`] index into this.
    pub params: Vec<ParamDef>,
    pub origin: ImplOrigin,
}

/// Where an impl came from, which decides whether an answer through it owes a
/// body, and which impl answers an instance two of them reach.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImplOrigin {
    /// Written in source, or supplied by the compiler for a primitive. Provides
    /// its own body.
    Written,
    /// Generated by [`derive`](super::derive) for a declaration whose members
    /// satisfy the trait. Answering through it owes the derived body. Yields
    /// to any other impl reaching the instance.
    Derived,
    /// `impl Tr for T;`, a demand for the derived body (WEP 2026-06-25).
    /// Answering through it owes the body; providing none, it duplicates no pair.
    Marker,
    /// Stated by [`pair_comparisons`](super::pair_comparisons) beside a
    /// written impl of the other comparison trait, at its target and bounds:
    /// the `Eq` a written `cmp` gives. Owes the body as `Derived` does, and
    /// yields only to a written impl or a marker.
    Paired,
}

impl ImplOrigin {
    /// The origins of the impls that `self` yields to where both reach an
    /// instance.
    #[must_use]
    pub fn yields_to(self) -> &'static [Self] {
        match self {
            Self::Written | Self::Marker => &[],
            Self::Paired => &[Self::Written, Self::Marker],
            Self::Derived => &[Self::Written, Self::Marker, Self::Paired],
        }
    }
}

/// A module, as the asking side of a question.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ModuleId(pub u32);

/// Who asks a question: code in a module, or an instance of a generic body.
/// Only a `Reflect*` fact's visibility gate tells them apart. The use site that
/// settled an instance's parameters proved its bounds where it stands
/// (spec-reflection.md § Visibility), so the instance sees every member.
/// Asking again from the module declaring the body refused an impl that use
/// site had chosen (`reflect_derivation_nested_private_fields.wado`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asker {
    Module(ModuleId),
    Instance,
}

/// The default a trait's type parameter declares — the `Self` in
/// `trait Mul<Rhs = Self>`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ArgDefault {
    /// `= Self`: the implementing type.
    SelfType,
    Type(SolverType),
    /// A default the lowering cannot spell. Nothing restates it, so an impl
    /// writing that argument answers no bare bound.
    Opaque,
}

impl ArgDefault {
    /// The default at an impl whose target is `target`; `None` for one the
    /// lowering cannot spell.
    #[must_use]
    pub fn at(&self, target: &SolverType) -> Option<SolverType> {
        match self {
            Self::SelfType => Some(target.clone()),
            Self::Type(ty) => Some(ty.clone()),
            Self::Opaque => None,
        }
    }
}

/// How a reference answers a bound its pointee holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RefRule {
    /// `&T` holds the bound when `T` does, by auto-deref at the call.
    #[default]
    Inherits,
    /// `&T` holds it of itself: the `Ref` / `RefMut` markers.
    Always,
    /// `&T` never inherits it: `Ord`, or a trait whose receiverless method
    /// names `Self`.
    Never,
}

/// A trait declaration, reduced to what the rules read.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TraitDef {
    /// The traits its clause names, which an implementor must also implement,
    /// so a bound naming this one answers for them too. Each carries what the
    /// clause writes for that trait's own parameters (`trait AsStrSlice:
    /// Eq<String>`), in this trait's space: [`SolverType::Param`] is this
    /// trait's parameter at that position (`trait Gauge<X>: Measure<X>`).
    pub supertraits: Vec<ParamBound>,
    /// Holds of every type before any body exists — `Inspect`. The unbounded
    /// blanket that would say so is rejected, so the trait says it itself.
    pub holds_for_all: bool,
    /// Derived over a declaration's members, so a body derived or asked for
    /// by a marker calls it on each member. Set by [`super::derive`].
    pub structural: bool,
    pub on_ref: RefRule,
    /// Per type parameter, its declared default, if any. A bound spells no
    /// arguments (WEP 2026-07-31), so it asks for the trait at its defaults.
    pub arg_defaults: Vec<Option<ArgDefault>>,
    /// The position of its parameter pack, which a bound writes as the flat
    /// run of arguments past the parameters around it.
    pub pack: Option<usize>,
    /// The methods it declares, which is what a call site matches on. A
    /// supertrait's methods are not among them: an implementor writes a
    /// separate impl for each trait.
    pub methods: Vec<MethodId>,
    /// The names it declares `#[unavailable]`. A call reaches one only where no
    /// method answers, so it can say why rather than make the call ambiguous.
    pub reserved: Vec<MethodId>,
    /// The bounds it declares on each associated type (`type Item: Display`),
    /// which a [`SolverType::Projection`] of it satisfies.
    pub assoc_bounds: IndexMap<AssocId, Vec<TraitDeclId>>,
}

/// What a module may name. A trait's methods are candidates only where its
/// declaration is in scope, so a module absent from
/// [`Program::scopes`](Program) reaches no trait at all.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ModuleScope {
    /// Declared in the module, imported by name or by alias, re-exported to it
    /// through `pub use`, or one of the prelude's. Where an impl was written
    /// does not enter: impls stay globally visible.
    pub traits_in_scope: Vec<TraitDeclId>,
}

/// A type declaration, reduced to what `holds` reads of it at a query. Its
/// members arrive at `derive` as a [`Declaration`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypeDef {
    /// For a newtype, the base it inherits impls from, spelled with the
    /// newtype's own parameters by position.
    pub newtype_base: Option<SolverType>,
}

/// What a declaration looks like to `derive`: the members a structural trait
/// is derived over. A newtype is not a declaration here, since it inherits.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Declaration {
    pub id: TypeDeclId,
    /// How many type parameters the declaration takes; a member may mention
    /// them as [`SolverType::Param`] by position.
    pub params: u32,
    /// Whether the last parameter is a pack, which a member mentions as
    /// [`SolverType::Pack`]: a shape's members are its arguments, one tuple
    /// of them.
    pub variadic: bool,
    /// Every field of a struct, or every payload of a variant.
    pub members: Vec<SolverType>,
    pub module: ModuleId,
}

impl Declaration {
    /// The members at the instance whose arguments are `args`: a pack member
    /// is each element of the tuple its argument is.
    #[must_use]
    pub fn members_at(&self, args: &[SolverType]) -> Vec<SolverType> {
        assert_eq!(
            args.len(),
            self.params as usize,
            "an instance argues every parameter"
        );
        let arg = |index: u32| Some(args[index as usize].clone());
        self.members
            .iter()
            .flat_map(|member| match (member, member.map_params(&arg)) {
                (SolverType::Pack(_), Some(SolverType::Tuple(elems))) => elems,
                (_, Some(at)) => vec![at],
                (_, None) => unreachable!("every parameter is argued"),
            })
            .collect()
    }
}

/// A body the answer depends on: "this type satisfies `Eq`" is also "emit `Eq`
/// for this type". An answer that arrives without its requests loses the body.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DerivationRequest {
    pub ty: SolverType,
    pub trait_: TraitDeclId,
    /// Whether the body is the structural one, which calls `trait_` on each
    /// member and so owes each member's body in turn ([`super::owed`]).
    pub structural: bool,
}

/// A declaration's own reflection kind, stated of the declaration and answering
/// for every instance of it. Its answer owes the body, as a derived impl's does.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Fact {
    /// The modules the fact holds when asked from; `None` is everywhere. A
    /// `Reflect*` bound's visibility gate arrives this way.
    pub visible_from: Option<Vec<ModuleId>>,
    /// The associated types the kind binds, spelled with the declaration's own
    /// parameters: a struct's `FieldTypes`, what a pin like
    /// `ReflectStruct<FieldTypes = [..F]>` reads.
    pub assoc: Vec<(AssocId, SolverType)>,
}

/// What the solver is asked about (`docs/wep-2026-09-01-trait-resolution.md`,
/// "The solver's input").
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Program {
    /// Insertion order is the order every answer is reported in, so a caller
    /// that builds it deterministically gets deterministic diagnostics.
    impls: IndexMap<ImplId, ImplDef>,
    /// The trait impls in `impls` order: by trait, by trait and the
    /// declaration the target names (`None` for any other target), and by that
    /// declaration alone. A question names its trait and its type, and a scan
    /// of every impl per question is quadratic in a whole package.
    impls_by_trait: IndexMap<TraitDeclId, Vec<ImplId>>,
    impls_by_target: IndexMap<(TraitDeclId, Option<TypeDeclId>), Vec<ImplId>>,
    trait_impls_by_head: IndexMap<Option<TypeDeclId>, Vec<ImplId>>,
    pub traits: IndexMap<TraitDeclId, TraitDef>,
    pub types: IndexMap<TypeDeclId, TypeDef>,
    /// The non-recursive ways a bound holds, keyed by the declaration and the
    /// trait they answer for.
    pub facts: IndexMap<(TypeDeclId, TraitDeclId), Fact>,
    /// Each impl's `type X = …;` bindings, spelled with the impl's own
    /// parameters. What a [`Pin`] is checked against. A family's binding
    /// (`type Buf<E> = List<E>;`) spells its own parameters past the impl's:
    /// `E` is [`SolverType::Param`] at the impl's parameter count.
    pub assoc_bindings: IndexMap<ImplId, Vec<(AssocId, SolverType)>>,
    /// The methods each impl block's body declares. One its trait does not
    /// declare — a helper the bodies call on `self` — is a candidate through
    /// that impl alone.
    pub impl_methods: IndexMap<ImplId, Vec<MethodId>>,
    /// What each module may name, which gates the candidates of a call made
    /// there.
    pub scopes: IndexMap<ModuleId, ModuleScope>,
    /// The declaration a tuple instantiates. An impl spells a tuple `[..T]`, so
    /// both sides lower to [`SolverType::Tuple`] rather than to a `Decl`, and a
    /// fact stated of that declaration needs this to reach an instance.
    pub tuple: Option<TypeDeclId>,
    /// Each declaration's members, which a structural body owes the bodies of.
    pub declarations: IndexMap<TypeDeclId, Declaration>,
    /// `Eq` and `Ord`, the pair the comparison table relates. Set by
    /// [`super::pair_comparisons`].
    pub comparisons: Option<(TraitDeclId, TraitDeclId)>,
    /// `Hash`, whose derived body feeds the members a derived `Eq` compares:
    /// a written `Eq` or `Ord` reaching a type withholds the derived one, as
    /// it may equate values whose members differ.
    pub hash: Option<TraitDeclId>,
    /// The associated types a [`Fact`] binds to a declaration's members. A
    /// pin on one descends into the type, which the cycle rule admits.
    pub member_channels: IndexSet<AssocId>,
    /// Every impl written again after its first copy. Coherence reports each
    /// where it is written, and no candidate set holds one, so a copy never
    /// ties with its original. Set by [`super::mark_duplicates`].
    pub duplicates: IndexSet<ImplId>,
    /// Impls that answer at their own level alone, never through a newtype's
    /// or a `flags` type's base: the stated `From` impls. Inheriting
    /// `From<Meters> for i64` would make every newtype of `i64` convert from
    /// `Meters`.
    pub not_inherited: IndexSet<ImplId>,
    /// The marker a type holds when no value of it is affine, which a
    /// [`ParamDef::copyable`] parameter asks of its argument.
    pub copyable: Option<TraitDeclId>,
    /// The head `!` lowers under. No value of it exists, so it holds every
    /// trait: no body it would owe can run.
    pub never: Option<TypeDeclId>,
}

/// The ids whose target could match `ty`, in the order they were added: what
/// `by_head` files under no declaration and under `ty`'s, or `all` for an open
/// row.
fn matching<'p, A: Iterator<Item = ImplId>>(
    ty: &SolverType,
    all: impl FnOnce() -> A,
    by_head: impl Fn(Option<TypeDeclId>) -> Option<&'p Vec<ImplId>>,
) -> Matching<'p, A> {
    if matches!(ty, SolverType::AnyRow) {
        return Matching::All(all());
    }
    let ids = |head| by_head(head).map_or(&[][..], Vec::as_slice);
    let named = target_head(ty).map_or(&[][..], |head| ids(Some(head)));
    Matching::Heads(ids(None), named)
}

/// What [`matching`] answers: every id, or two buckets merged in id order.
enum Matching<'p, A> {
    All(A),
    Heads(&'p [ImplId], &'p [ImplId]),
}

impl<A: Iterator<Item = ImplId>> Iterator for Matching<'_, A> {
    type Item = ImplId;

    fn next(&mut self) -> Option<ImplId> {
        let (a, b) = match self {
            Self::All(all) => return all.next(),
            Self::Heads(a, b) => (a, b),
        };
        let side = match (a.first(), b.first()) {
            (Some(x), Some(y)) if y < x => b,
            (Some(_), _) => a,
            (None, _) => b,
        };
        let (&first, rest) = side.split_first()?;
        *side = rest;
        Some(first)
    }
}

fn target_head(ty: &SolverType) -> Option<TypeDeclId> {
    match ty {
        SolverType::Decl(head, _) => Some(*head),
        _ => None,
    }
}

/// The bounds in force where a question was asked: a generic body's `T: Tr`
/// holds by its signature, not by any impl. Indexed by [`SolverType::Param`]
/// and [`SolverType::Pack`], a pack's bound holding of each element.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Env {
    /// Each parameter's bounds, whole: a bound carries the arguments it writes,
    /// which decide whether it answers a question at those arguments.
    pub param_bounds: Vec<Vec<ParamBound>>,
}

impl Program {
    /// The id the next [`Self::push_impl`] gives: every id from it on is an
    /// impl added after this call.
    #[must_use]
    pub fn next_impl_id(&self) -> ImplId {
        ImplId(self.impls.last().map_or(0, |(last, _)| last.0 + 1))
    }

    /// Add one impl at the id after the last.
    pub fn push_impl(&mut self, def: ImplDef) -> ImplId {
        let id = self.next_impl_id();
        let declared = |p: &SolverType| match p {
            SolverType::Param(index) | SolverType::Pack(index) => {
                (*index as usize) < def.params.len()
            }
            SolverType::Decl(..)
            | SolverType::Tuple(_)
            | SolverType::Ref { .. }
            | SolverType::Projection { .. }
            | SolverType::AnyRow => true,
        };
        let undeclared = |ty: &SolverType| ty.mentions(&|p| !declared(p));
        assert!(
            !undeclared(&def.target)
                && !def.trait_args.iter().any(undeclared)
                && !def
                    .params
                    .iter()
                    .flat_map(|p| &p.pins)
                    .any(|pin| undeclared(&pin.ty)),
            "{id:?} mentions a parameter it does not declare: {def:?}"
        );
        if let Some(trait_) = def.trait_ {
            self.impls_by_trait.entry(trait_).or_default().push(id);
            let head = target_head(&def.target);
            self.impls_by_target
                .entry((trait_, head))
                .or_default()
                .push(id);
            self.trait_impls_by_head.entry(head).or_default().push(id);
        }
        assert!(self.impls.insert(id, def).is_none());
        id
    }

    /// Every impl, in the order they were added.
    #[must_use]
    pub fn impls(&self) -> &IndexMap<ImplId, ImplDef> {
        &self.impls
    }

    /// Put `def` in place of the impl at `id`.
    pub fn replace_impl(&mut self, id: ImplId, def: ImplDef) {
        let old = &mut self.impls[&id];
        assert!(
            old.trait_ == def.trait_ && target_head(&old.target) == target_head(&def.target),
            "a replacement keeps the trait and the target head the indices file it under"
        );
        *old = def;
    }

    /// Take out the impls at `ids`, each index scanned once whatever their count.
    pub fn remove_impls(&mut self, ids: &IndexSet<ImplId>) {
        let mut keys = IndexSet::<(TraitDeclId, _)>::default();
        for id in ids {
            let def = &self.impls[id];
            if let Some(trait_) = def.trait_ {
                keys.insert((trait_, target_head(&def.target)));
            }
        }
        self.impls.retain(|id, _| !ids.contains(id));
        let kept = |other: &ImplId| !ids.contains(other);
        for &(trait_, head) in &keys {
            self.impls_by_trait[&trait_].retain(kept);
            self.impls_by_target[&(trait_, head)].retain(kept);
            self.trait_impls_by_head[&head].retain(kept);
        }
    }

    /// The impls of `trait_`, in the order they were added.
    pub fn impls_of(&self, trait_: TraitDeclId) -> impl Iterator<Item = (ImplId, &ImplDef)> {
        self.impls_by_trait
            .get(&trait_)
            .into_iter()
            .flatten()
            .map(|&id| (id, &self.impls[&id]))
    }

    /// The impls of `trait_` whose target could match `ty`, in the order they
    /// were added: a target naming a declaration matches only that one, or an
    /// open row.
    pub fn impls_for(
        &self,
        trait_: TraitDeclId,
        ty: &SolverType,
    ) -> impl Iterator<Item = (ImplId, &ImplDef)> {
        let ids = matching(
            ty,
            || self.impls_of(trait_).map(|(id, _)| id),
            |head| self.impls_by_target.get(&(trait_, head)),
        );
        ids.map(|id| (id, &self.impls[&id]))
    }

    /// The trait impls whose target could match `ty`, of any trait, in the
    /// order they were added.
    pub fn trait_impls_for(&self, ty: &SolverType) -> impl Iterator<Item = (ImplId, &ImplDef)> {
        let ids = matching(
            ty,
            || {
                let trait_impls = self.impls.iter().filter(|(_, def)| def.trait_.is_some());
                trait_impls.map(|(&id, _)| id)
            },
            |head| self.trait_impls_by_head.get(&head),
        );
        ids.map(|id| (id, &self.impls[&id]))
    }

    /// The default `def`'s trait declares for its argument at `index`, at
    /// `def`'s target; `None` where it declares none or cannot spell it.
    pub(super) fn default_arg(&self, def: &ImplDef, index: usize) -> Option<SolverType> {
        self.arg_at(def.trait_?, None, index, &def.target)
    }

    /// `trait_`'s argument at `index` in a bound on `subject` writing `written`
    /// there: the written one, else the declared default. `None` where neither
    /// spells one.
    pub(super) fn arg_at(
        &self,
        trait_: TraitDeclId,
        written: Option<&SolverType>,
        index: usize,
        subject: &SolverType,
    ) -> Option<SolverType> {
        if let Some(arg) = written {
            return Some(arg.clone());
        }
        self.traits
            .get(&trait_)?
            .arg_defaults
            .get(index)?
            .as_ref()?
            .at(subject)
    }

    /// What `args`, written for `trait_`, say for each of its parameters, as
    /// [`args_per_param`] reads them. With no trait declaring a pack, each
    /// argument is its own parameter's.
    pub(super) fn per_param(
        &self,
        trait_: Option<TraitDeclId>,
        args: &[SolverType],
    ) -> Vec<Option<SolverType>> {
        match trait_.and_then(|trait_| self.traits.get(&trait_)) {
            Some(def) => args_per_param(args, def.arg_defaults.len(), def.pack, SolverType::Tuple),
            None => args.iter().cloned().map(Some).collect(),
        }
    }

    /// Whether a bound on `subject` naming `bound` answers for `wanted`: itself
    /// or a supertrait, transitively.
    pub(super) fn bound_reaches(
        &self,
        bound: TraitDeclId,
        wanted: TraitDeclId,
        subject: &SolverType,
    ) -> bool {
        !self
            .args_reaching(&ParamBound::bare(bound), wanted, subject)
            .is_empty()
    }

    /// Every argument list a bound on `subject` writing `bound` reaches
    /// `wanted` with — its own arguments where it names `wanted`, and each
    /// clause that reaches it. Two edges to one trait writing different
    /// arguments are two answers, so the walk carries on past the first rather
    /// than deciding on it.
    pub(super) fn args_reaching(
        &self,
        bound: &ParamBound,
        wanted: TraitDeclId,
        subject: &SolverType,
    ) -> Vec<Vec<SolverType>> {
        let mut reaching = Vec::new();
        let mut stack = vec![bound.clone()];
        // An edge is its trait and what it writes, so a second instantiation of
        // one trait is walked rather than taken for a revisit.
        let mut seen: Vec<ParamBound> = Vec::new();
        while let Some(next) = stack.pop() {
            if seen.contains(&next) {
                continue;
            }
            if next.trait_ == wanted {
                reaching.push(next.args.clone());
            }
            if let Some(def) = self.traits.get(&next.trait_) {
                let args = self.per_param(Some(next.trait_), &next.args);
                stack.extend(
                    def.supertraits
                        .iter()
                        .filter_map(|clause| self.clause_at(clause, next.trait_, &args, subject)),
                );
            }
            seen.push(next);
        }
        reaching
    }

    /// `clause`, written in `sub`'s space, at `args`, what a bound on `subject`
    /// writes for each of `sub`'s parameters. `None` where the clause names a
    /// parameter that bound neither writes nor defaults: the edge then answers
    /// nothing rather than a guess.
    pub(super) fn clause_at(
        &self,
        clause: &ParamBound,
        sub: TraitDeclId,
        args: &[Option<SolverType>],
        subject: &SolverType,
    ) -> Option<ParamBound> {
        let arg = |i: u32| {
            let i = i as usize;
            self.arg_at(sub, args.get(i).and_then(Option::as_ref), i, subject)
        };
        Some(ParamBound {
            trait_: clause.trait_,
            args: clause
                .args
                .iter()
                .map(|ty| ty.map_params(&arg))
                .collect::<Option<_>>()?,
        })
    }
}

/// What the `written` arguments of a use say for each of a declaration's
/// `params` parameters. A pack at `pack` takes the run of every argument past
/// the parameters around it, made one by `tuple`, and an empty run where the
/// use stops short of it. `None` at a position nothing is written for, which
/// its default answers.
pub fn args_per_param<T: Clone>(
    written: &[T],
    params: usize,
    pack: Option<usize>,
    tuple: impl FnOnce(Vec<T>) -> T,
) -> Vec<Option<T>> {
    let Some(pack) = pack else {
        return written.iter().cloned().map(Some).collect();
    };
    let absorbed = (written.len() + 1).saturating_sub(params);
    let run = written
        .get(pack..pack + absorbed)
        .unwrap_or_default()
        .to_vec();
    (0..pack)
        .map(|i| written.get(i).cloned())
        .chain(std::iter::once(Some(tuple(run))))
        .chain((pack + 1..params).map(|i| written.get(i + absorbed - 1).cloned()))
        .collect()
}
