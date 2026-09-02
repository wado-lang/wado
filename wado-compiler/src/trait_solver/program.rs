//! The value trait resolution answers questions about.
//!
//! Nothing here names a compiler type. An id is a plain index, so a test writes
//! the program it needs rather than compiling source to obtain one.

use crate::hashmap::IndexMap;

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

/// A type as selection reads it.
///
/// Two impls whose targets are equal here are two impls of one `(Trait, Type)`
/// pair: a parameter is its position, so `impl<T> Tag for Box_<T>` and
/// `impl<U> Tag for Box_<U>` are the same target, while
/// `impl Tag for Box_<i32>` is a different one.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
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
}

/// One of an impl's type parameters.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct ParamDef {
    pub bounds: Vec<TraitDeclId>,
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
    /// Whether this is `impl Tr for T;` — a request for the derived body and a
    /// conformance check on it, rather than a body of its own
    /// (WEP 2026-06-25). It provides nothing, so it is not one of the impls a
    /// pair may have only one of.
    pub is_derivation_request: bool,
}

/// A module, as the asking side of a question.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ModuleId(pub u32);

/// A trait declaration, reduced to what the rules read.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TraitDef {
    /// The traits an implementor must also implement, so a bound naming this
    /// one answers for them too.
    pub supertraits: Vec<TraitDeclId>,
}

/// A body the answer depends on: "this type satisfies `Eq`" is also "emit `Eq`
/// for this type". An answer that arrives without its requests loses the body.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DerivationRequest {
    pub ty: SolverType,
    pub trait_: TraitDeclId,
}

/// One way a bound holds that is a property of the type rather than a search:
/// a primitive's built-in traits, a plain `enum`'s `Display`, a reference
/// identity, a structural derivation over the members, a declaration's own
/// reflection kind. The lowering resolves those and states them here; the
/// solver's own work is the impls and the blanket recursion over them.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Fact {
    /// Where the fact holds. `None` — everywhere. `Some(modules)` — only when
    /// asked from one of them, which is how a `Reflect*` bound's visibility gate
    /// arrives: naming a type is not enumerating it.
    pub visible_from: Option<Vec<ModuleId>>,
    pub requests: Vec<DerivationRequest>,
}

/// What the solver is asked about.
///
/// Each module's imported declarations join it as `candidates` lands — see
/// "How the order is guaranteed" in
/// `docs/wep-2026-09-01-trait-resolution.md`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Program {
    /// Insertion order is the order every answer is reported in, so a caller
    /// that builds it deterministically gets deterministic diagnostics.
    pub impls: IndexMap<ImplId, ImplDef>,
    pub traits: IndexMap<TraitDeclId, TraitDef>,
    /// The non-recursive ways a bound holds, keyed by the pair they answer.
    pub facts: IndexMap<(SolverType, TraitDeclId), Fact>,
}

/// The bounds in force where a question was asked.
///
/// A generic body's `T: Tr` holds because its own signature says so, not because
/// any impl exists, so no question is a function of the program alone. Indexed
/// by the position [`SolverType::Param`] carries.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Env {
    pub param_bounds: Vec<Vec<TraitDeclId>>,
}

impl Program {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one impl. Panics on a repeated id: an id names one impl block, and a
    /// caller that mints two for one block has already lost the mapping back.
    pub fn add_impl(&mut self, id: ImplId, def: ImplDef) {
        assert!(
            self.impls.insert(id, def).is_none(),
            "{id:?} was added twice"
        );
    }

    /// The traits a bound on `trait_` answers for: itself and its supertraits,
    /// transitively. A cycle among supertraits is rejected where the traits are
    /// declared; the walk refuses to hang on one regardless.
    pub(super) fn bound_reaches(&self, bound: TraitDeclId, wanted: TraitDeclId) -> bool {
        let mut stack = vec![bound];
        let mut seen: Vec<TraitDeclId> = Vec::new();
        while let Some(next) = stack.pop() {
            if next == wanted {
                return true;
            }
            if seen.contains(&next) {
                continue;
            }
            seen.push(next);
            if let Some(def) = self.traits.get(&next) {
                stack.extend(def.supertraits.iter().copied());
            }
        }
        false
    }
}
