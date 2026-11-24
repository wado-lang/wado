//! The single source of truth for "can type A be used where type B is
//! expected?", in three layers of increasing host-awareness: the pure
//! [`check_assignable`], then [`TypeSystem::typecheck`] adding a
//! [`TypeMismatchPayload`], then [`Elaborator::typecheck`] emitting the
//! diagnostic. The last two each have a `_return` flavour where `UNIT` passes.

use crate::ast::{AstId, Expr};
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::tir::{
    EffectRef, ResolvedType, RowFlow, StructDef, TupleSlot, TypeId, TypeParamId, is_open_row,
};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::call::ArgSite;
use super::coercion::is_literal_arithmetic;
use super::infer_hole::{DeferredCheck, Obligation, says_nothing};
use super::types::TypeError;
use super::tysys::TypeSystem;

/// Result of checking whether `actual` is assignable to `expected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeCheckResult {
    /// Types are compatible (identical, or actual is a subtype/coercible).
    Compatible,
    /// Check was deferred because types involve unresolved generics,
    /// UNKNOWN, or other conditions where we can't decide yet.
    Deferred,
    /// Types are definitely incompatible.
    Incompatible,
}

/// Whether a value meeting its slot settles the variables either type holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Settles {
    Variables,
    /// A numeric literal's type is its default only until a later coercion
    /// retypes it, so it settles nothing.
    Nothing,
}

impl Settles {
    pub(super) fn of(value: &Expr) -> Self {
        if is_literal_arithmetic(value) {
            Self::Nothing
        } else {
            Self::Variables
        }
    }
}

/// Whether a value of `type_id` may stand where a condition is read: a `bool`,
/// a `!` that never yields one, or a type already diagnosed or yet unknown.
pub(super) fn admits_condition(type_id: TypeId) -> bool {
    matches!(
        type_id,
        TypeTable::NEVER | TypeTable::UNKNOWN | TypeTable::ERROR | TypeTable::BOOL
    )
}

/// Pure type compatibility check, emitting no errors. Rules apply in order:
/// identity; `NEVER` compatible but as a type argument behind a reference or in
/// a function type, and `UNKNOWN` / `ERROR` deferred; a pack-arity conflict incompatible; anything else genuinely
/// undecided deferred; reference variance (`&mut T` → `&T` only); newtypes and
/// flags distinct from their base and from each other, but for two instances of
/// one newtype; `Option`; structural comparison for function types, and
/// argument by argument for two instances of one generic struct, resource or
/// newtype; anything else incompatible.
pub(super) fn check_assignable(
    actual: TypeId,
    expected: TypeId,
    type_table: &TypeTable,
) -> TypeCheckResult {
    check_at(actual, expected, type_table, Position::Covariant, false)
}

/// Whether a value of `actual` holds an `Option<!>` where `expected` holds the
/// `Option<T>` it goes to, so it must be rebuilt as one (`synthesis::widen`).
pub(super) fn widens(actual: TypeId, expected: TypeId, type_table: &TypeTable) -> bool {
    type_table.holds_never_arg_by_value(actual)
        && !type_table.is_indefinite(expected)
        && actual != expected
        && check_assignable(actual, expected, type_table) == TypeCheckResult::Compatible
}

/// Whether the position admits a subtype: a value, a `return` and a `&T`
/// referent do; `&mut T`, a container element and a function type's parts do
/// not. See `docs/wep-2026-04-28-resource-inheritance.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Covariant,
    Invariant,
}

/// `fixed`: a reference names the value or a function computes it, so no
/// conversion can rebuild it. A value whose type argument is `!` is one of the
/// expected type only once rebuilt (`synthesis::widen`).
fn check_at(
    actual: TypeId,
    expected: TypeId,
    type_table: &TypeTable,
    position: Position,
    fixed: bool,
) -> TypeCheckResult {
    // Rule 1: Identity
    if actual == expected {
        return TypeCheckResult::Compatible;
    }

    // Rule 2: Bottom/Top/Error
    if actual == TypeTable::UNKNOWN
        || expected == TypeTable::UNKNOWN
        || actual == TypeTable::ERROR
        || expected == TypeTable::ERROR
    {
        return TypeCheckResult::Deferred;
    }
    if actual == TypeTable::NEVER {
        return TypeCheckResult::Compatible;
    }

    if pack_shape_conflict(actual, expected, type_table) {
        return TypeCheckResult::Incompatible;
    }

    // Defer only what is genuinely undecided: an inference variable awaiting its
    // solver, a pack awaiting expansion that the rule above did not settle, a
    // projection over one of those, and `unknown` / `error`. A rigid `TypeParam`
    // is opaque, not undecided — a use of a polymorphic signature instantiates
    // its slots into `InferVar`s first, so nothing but itself is ever assignable
    // to it. A value carrying no pack at all is opaque on the same terms where
    // the expected packs are in scope, which this layer cannot see;
    // `Elaborator::typecheck` adds that rule.
    if type_table.contains_undecided(actual) || type_table.contains_undecided(expected) {
        // A hole decides nothing where the types already part around it:
        // `Result<i32, ?E>` is no `i32`, and `Option<List<?T>>` no
        // `Option<String>`, whatever the hole becomes.
        if apart_around_holes(actual, expected, type_table) {
            return TypeCheckResult::Incompatible;
        }
        return TypeCheckResult::Deferred;
    }

    // Unwrap references for inner type comparison
    let (actual_inner, actual_is_ref) = unwrap_ref(actual, type_table);
    let (expected_inner, expected_is_ref) = unwrap_ref(expected, type_table);

    // Rule 4: Reference compatibility
    if actual_is_ref && !expected_is_ref {
        // &T -> non-ref: always incompatible (auto-deref is only for method calls)
        return TypeCheckResult::Incompatible;
    }
    if !actual_is_ref && expected_is_ref {
        // non-ref -> &T: always incompatible. The caller must take the
        // reference explicitly with `&` (or `&mut`); the type checker
        // never inserts an implicit ref at a call/assignment boundary.
        return TypeCheckResult::Incompatible;
    }
    // Both refs: check inner compatibility
    if actual_is_ref && expected_is_ref {
        let actual_is_mut = type_table.is_mut_ref(actual);
        let expected_is_mut = type_table.is_mut_ref(expected);
        // &mut T -> &T is ok; &T -> &mut T is not
        if !actual_is_mut && expected_is_mut {
            return TypeCheckResult::Incompatible;
        }
        if actual_inner != expected_inner {
            let inner_position = if expected_is_mut {
                Position::Invariant
            } else {
                position
            };
            return check_at(
                actual_inner,
                expected_inner,
                type_table,
                inner_position,
                true,
            );
        }
        return TypeCheckResult::Compatible;
    }

    // Two instances of one declaration, two arrays or two tuples, compare by
    // their arguments (Rule 8), whatever their slots.
    let same_declaration = same_constructor_args(actual_inner, expected_inner, type_table);

    // Rule 5: Newtype/flags distinctness, but for two instances of one newtype.
    let actual_is_newtype = matches!(
        type_table.get(actual_inner),
        ResolvedType::Newtype { .. } | ResolvedType::Flags { .. }
    );
    let expected_is_newtype = matches!(
        type_table.get(expected_inner),
        ResolvedType::Newtype { .. } | ResolvedType::Flags { .. }
    );
    if (actual_is_newtype || expected_is_newtype)
        && actual_inner != expected_inner
        && same_declaration.is_none()
    {
        return TypeCheckResult::Incompatible;
    }
    if let ResolvedType::Newtype { base_type, .. } = type_table.get(actual_inner)
        && *base_type == expected_inner
    {
        return TypeCheckResult::Incompatible;
    }
    if let ResolvedType::Newtype { base_type, .. } = type_table.get(expected_inner)
        && *base_type == actual_inner
    {
        return TypeCheckResult::Incompatible;
    }

    // Rule 6: Option compatibility
    let actual_option = type_table.as_option(actual);
    let expected_option = type_table.as_option(expected);
    match (actual_option, expected_option) {
        (Some(actual_t), Some(expected_t)) => {
            if actual_t == TypeTable::UNKNOWN || expected_t == TypeTable::UNKNOWN {
                return TypeCheckResult::Compatible;
            }
            if needs_rebuild(actual_t, expected_t, fixed) {
                return TypeCheckResult::Incompatible;
            }
            return check_at(actual_t, expected_t, type_table, Position::Invariant, fixed);
        }
        (Some(_), None) | (None, Some(_)) => {
            return TypeCheckResult::Incompatible;
        }
        (None, None) => {}
    }

    // Rule 7: Function types -- structural comparison of params + return type.
    // Sub-typing: `fn <: fn mut` -- a read-only closure is assignable to a
    // `fn mut`-typed slot, but not the reverse.
    if let ResolvedType::Function {
        is_mut: actual_is_mut,
        params: actual_params,
        return_type: actual_ret,
        effects: actual_effects,
    } = type_table.get(actual_inner)
        && let ResolvedType::Function {
            is_mut: expected_is_mut,
            params: expected_params,
            return_type: expected_ret,
            effects: expected_effects,
        } = type_table.get(expected_inner)
    {
        if *actual_is_mut && !*expected_is_mut {
            // `fn mut` cannot widen to `fn`.
            return TypeCheckResult::Incompatible;
        }
        // A call through the slot performs no more than its type says. Where a
        // value can also be written back through the slot, a narrower row would
        // admit a wider one, so there the rows match. An effect parameter or
        // variable is the effect check's to resolve.
        if !is_open_row(expected_effects) && !actual_effects.iter().any(EffectRef::is_var) {
            let covers = |wide: &[EffectRef], narrow: &[EffectRef]| {
                narrow.iter().all(|effect| wide.contains(effect))
            };
            let admitted = match position {
                Position::Covariant => covers(expected_effects, actual_effects),
                Position::Invariant => {
                    covers(expected_effects, actual_effects)
                        && covers(actual_effects, expected_effects)
                }
            };
            if !admitted {
                return TypeCheckResult::Incompatible;
            }
        }
        if actual_params.len() != expected_params.len() {
            return TypeCheckResult::Incompatible;
        }
        for (a, e) in actual_params.iter().zip(expected_params.iter()) {
            match check_at(*a, *e, type_table, Position::Invariant, true) {
                TypeCheckResult::Incompatible => return TypeCheckResult::Incompatible,
                TypeCheckResult::Deferred => return TypeCheckResult::Deferred,
                TypeCheckResult::Compatible => {}
            }
        }
        return check_at(
            *actual_ret,
            *expected_ret,
            type_table,
            Position::Invariant,
            true,
        );
    }
    // One is function, the other isn't -> incompatible
    if matches!(type_table.get(actual_inner), ResolvedType::Function { .. })
        || matches!(
            type_table.get(expected_inner),
            ResolvedType::Function { .. }
        )
    {
        return TypeCheckResult::Incompatible;
    }

    // Rule 8: Instances of one generic struct, resource or newtype, or two
    // tuples -- compare their type args, so an argument's effect row is the
    // effect check's to resolve.
    if let Some((actual_args, expected_args)) = same_declaration {
        return check_type_args(actual_args, expected_args, type_table, fixed);
    }

    // Rule 9: `resource Child extends Parent`, where the position admits it.
    if position == Position::Covariant
        && let ResolvedType::Resource {
            def: actual_def, ..
        } = type_table.get(actual_inner)
        && let ResolvedType::Resource {
            def: expected_def, ..
        } = type_table.get(expected_inner)
        && type_table.is_resource_subtype(*actual_def, *expected_def)
    {
        return TypeCheckResult::Compatible;
    }

    // Rule 10: General catch-all -- different concrete types
    if actual_inner != expected_inner {
        return TypeCheckResult::Incompatible;
    }

    TypeCheckResult::Compatible
}

/// Whether an actual `!` meets the variable it reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NeverMeets {
    Nothing,
    Variables,
}

/// What [`meetings`] reads beyond the two types.
#[derive(Clone, Copy)]
pub(super) struct MeetRules<'a> {
    pub(super) never: NeverMeets,
    /// A projection base both sides were written with. Two projections of one
    /// family meet argument by argument only over such a base: one a variable
    /// reached is no alias the source wrote.
    pub(super) written_base: &'a dyn Fn(TypeId) -> bool,
}

impl MeetRules<'_> {
    /// No `!` answers, and no projection meets another.
    pub(super) const PLAIN: MeetRules<'static> = MeetRules {
        never: NeverMeets::Nothing,
        written_base: &|_| false,
    };
}

/// Each pair a value of `actual` meeting a slot of `expected` equates, where
/// one side is an inference variable: the places [`check_at`] reaches by the
/// same rules before an undecided type stops it. A place where no coercion
/// can be chosen, since a head is still a variable, equates the two whole.
/// A `!`, `unknown` or `error` side equates nothing, which every check admits,
/// but for an actual `!` meeting a variable where `rules` asks for it.
pub(super) fn meetings(
    actual: TypeId,
    expected: TypeId,
    type_table: &TypeTable,
    rules: MeetRules<'_>,
    out: &mut Meetings,
) {
    if rules.never == NeverMeets::Variables
        && type_table.is_never(actual)
        && type_table.is_infer_var(expected)
    {
        out.pairs.push((actual, expected));
        return;
    }
    if actual == expected || says_nothing(actual) || says_nothing(expected) {
        return;
    }
    if type_table.is_infer_var(actual) || type_table.is_infer_var(expected) {
        out.pairs.push((actual, expected));
        return;
    }
    let pairwise = |actual: &[TypeId], expected: &[TypeId], out: &mut Meetings| {
        if actual.len() == expected.len() {
            for (&a, &e) in actual.iter().zip(expected) {
                meetings(a, e, type_table, rules, out);
            }
        }
    };
    let (actual_inner, actual_is_ref) = unwrap_ref(actual, type_table);
    let (expected_inner, expected_is_ref) = unwrap_ref(expected, type_table);
    if actual_is_ref || expected_is_ref {
        let into_mut = !type_table.is_mut_ref(actual) && type_table.is_mut_ref(expected);
        if actual_is_ref && expected_is_ref && !into_mut {
            meetings(actual_inner, expected_inner, type_table, rules, out);
        }
        return;
    }
    match (type_table.get(actual), type_table.get(expected)) {
        (ResolvedType::Tuple(a), ResolvedType::Tuple(e)) => {
            let around = fixed_around_packs(a, e, type_table);
            for (a, e) in around.fixed {
                meetings(a, e, type_table, rules, out);
            }
            if let Some(run) = around.run {
                out.packs.push(run);
            }
        }
        (
            ResolvedType::Function {
                params: a_params,
                return_type: a_ret,
                ..
            },
            ResolvedType::Function {
                params: e_params,
                return_type: e_ret,
                ..
            },
        ) => {
            if a_params.len() == e_params.len() {
                pairwise(a_params, e_params, out);
                meetings(*a_ret, *e_ret, type_table, rules, out);
            }
        }
        // A projection over a rigid base answers nothing yet, so two of one
        // family meet argument by argument, as Rust relates rigid aliases.
        (
            ResolvedType::AssocTypeProjection {
                param_id: a_base,
                assoc_name: a_name,
                args: a_args,
                owning_trait: a_trait,
                trait_args: a_trait_args,
                ..
            },
            ResolvedType::AssocTypeProjection {
                param_id: e_base,
                assoc_name: e_name,
                args: e_args,
                owning_trait: e_trait,
                trait_args: e_trait_args,
                ..
            },
        ) if a_base == e_base
            && (rules.written_base)(*e_base)
            && a_name == e_name
            && a_trait == e_trait =>
        {
            pairwise(a_args, e_args, out);
            if let (Some(a), Some(e)) = (a_trait_args, e_trait_args) {
                pairwise(a, e, out);
            }
        }
        _ => {
            if let Some((a_args, e_args)) = same_constructor_args(actual, expected, type_table) {
                pairwise(a_args, e_args, out);
            }
        }
    }
}

/// What [`meetings`] finds two types meet at.
#[derive(Default)]
pub(super) struct Meetings {
    /// Each `(actual, expected)` pair to equate.
    pub(super) pairs: Vec<(TypeId, TypeId)>,
    /// Each pack variable a tuple holds, with the run of elements it meets.
    pub(super) packs: Vec<PackRun>,
}

/// A pack variable, as the plain variable, and the elements the other tuple
/// holds where it stands, which answer it as one tuple.
pub(super) struct PackRun {
    pub(super) var: TypeId,
    pub(super) elems: Vec<TypeId>,
}

/// What two tuples meet at whatever their packs expand to.
struct AroundPacks {
    /// The elements at one position: all of them where neither holds a pack,
    /// else those ahead of the first pack and behind the last, counted from
    /// each end. Two tuples both holding packs place them alike or pair
    /// nothing.
    fixed: Vec<(TypeId, TypeId)>,
    /// A pack variable standing alone among the packs of its tuple, and what
    /// the other tuple holds in its place.
    run: Option<PackRun>,
}

fn fixed_around_packs(a: &[TypeId], e: &[TypeId], type_table: &TypeTable) -> AroundPacks {
    let packs_at = |t: &[TypeId]| -> Vec<usize> {
        t.iter()
            .enumerate()
            .filter(|&(_, &t)| type_table.is_pack_like(t))
            .map(|(i, _)| i)
            .collect()
    };
    let nothing = || AroundPacks {
        fixed: Vec::new(),
        run: None,
    };
    let (a_packs, e_packs) = (packs_at(a), packs_at(e));
    if a_packs.is_empty() && e_packs.is_empty() {
        if a.len() != e.len() {
            return nothing();
        }
        return AroundPacks {
            fixed: a.iter().copied().zip(e.iter().copied()).collect(),
            run: None,
        };
    }
    if !a_packs.is_empty() && !e_packs.is_empty() {
        if a.len() != e.len() || a_packs != e_packs {
            return nothing();
        }
        // A pack variable facing a pack is answered with it, a variable
        // facing a variable joins it.
        let mut fixed = Vec::new();
        let mut run = None;
        for (&a, &e) in a.iter().zip(e) {
            match (type_table.pack_var_list(a), type_table.pack_var_list(e)) {
                (Some(a_var), Some(e_var)) => fixed.push((a_var, e_var)),
                (Some(var), None) if a_packs.len() == 1 => {
                    run = Some(PackRun {
                        var,
                        elems: vec![e],
                    });
                }
                (None, Some(var)) if a_packs.len() == 1 => {
                    run = Some(PackRun {
                        var,
                        elems: vec![a],
                    });
                }
                (Some(_) | None, Some(_) | None) => {
                    if !type_table.is_pack_like(a) {
                        fixed.push((a, e));
                    }
                }
            }
        }
        return AroundPacks { fixed, run };
    }
    let (packed, packs, other) = if e_packs.is_empty() {
        (a, &a_packs, e)
    } else {
        (e, &e_packs, a)
    };
    let before = packs[0];
    let after = packed.len() - packs[packs.len() - 1] - 1;
    if other.len() < before + after {
        return nothing();
    }
    let ends = (0..before).chain(packed.len() - after..packed.len());
    let other_at = |i: usize| {
        if i < before {
            other[i]
        } else {
            other[other.len() - (packed.len() - i)]
        }
    };
    let oriented = |packed_side: TypeId, other_side: TypeId| {
        if e_packs.is_empty() {
            (packed_side, other_side)
        } else {
            (other_side, packed_side)
        }
    };
    let mut fixed: Vec<(TypeId, TypeId)> = ends.map(|i| oriented(packed[i], other_at(i))).collect();
    // One pack takes what the ends leave over; two divide it at a boundary
    // nothing here can find. A repeat's elements each meet the type it
    // repeats.
    let run = match packs[..] {
        [only] => type_table.pack_var_run(packed[only]).map(|(var, each)| {
            let elems = other[before..other.len() - after].to_vec();
            if let Some(each) = each {
                fixed.extend(elems.iter().map(|&elem| oriented(each, elem)));
            }
            PackRun { var, elems }
        }),
        [_, _, ..] => None,
        [] => unreachable!("one side holds a pack"),
    };
    AroundPacks { fixed, run }
}

/// Whether `actual` and `expected` part at some position: their heads, or
/// the heads of a pair of arguments one constructor, or one `fn` type, holds
/// in both. A tuple holding a pack, or a variable or error that may stand for
/// one, places nothing after it, so its elements are not paired by position.
fn apart_around_holes(actual: TypeId, expected: TypeId, type_table: &TypeTable) -> bool {
    if heads_apart(actual, expected, type_table) {
        return true;
    }
    let may_shift = |id: TypeId| {
        type_table.as_tuple(id).is_some_and(|elems| {
            elems.iter().any(|&e| {
                matches!(
                    type_table.get(e),
                    ResolvedType::TypePack { .. }
                        | ResolvedType::PackVar { .. }
                        | ResolvedType::InferVar(_)
                        | ResolvedType::Unknown
                        | ResolvedType::Error
                )
            })
        })
    };
    if may_shift(actual) || may_shift(expected) {
        return false;
    }
    let any_apart = |a_args: &[TypeId], e_args: &[TypeId]| {
        a_args.len() == e_args.len()
            && a_args
                .iter()
                .zip(e_args)
                .any(|(&a, &e)| apart_around_holes(a, e, type_table))
    };
    if let (
        ResolvedType::Function {
            params: a_params,
            return_type: a_ret,
            ..
        },
        ResolvedType::Function {
            params: e_params,
            return_type: e_ret,
            ..
        },
    ) = (type_table.get(actual), type_table.get(expected))
    {
        return a_params.len() != e_params.len()
            || any_apart(a_params, e_params)
            || apart_around_holes(*a_ret, *e_ret, type_table);
    }
    same_constructor_args(actual, expected, type_table)
        .is_some_and(|(a_args, e_args)| any_apart(a_args, e_args))
}

/// Whether `actual` and `expected` are apart at their outermost type, which no
/// rule here relates across: two declarations, two primitives, or two kinds of
/// type. A reference, a resource and `!` are not judged here, since a rule
/// relates one to another of a different outermost type, and a hole or a
/// parameter is no outermost type at all.
fn heads_apart(actual: TypeId, expected: TypeId, type_table: &TypeTable) -> bool {
    let head = |id: TypeId| match type_table.get(id) {
        ResolvedType::Primitive(_)
        | ResolvedType::Unit
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Function { .. }
        | ResolvedType::Tuple(_)
        | ResolvedType::BuiltinArray(_)
        | ResolvedType::Newtype { .. }
        | ResolvedType::Flags { .. } => Some(type_table.get(id)),
        _ => None,
    };
    let (Some(a), Some(e)) = (head(actual), head(expected)) else {
        return false;
    };
    match (a, e) {
        (ResolvedType::Primitive(a), ResolvedType::Primitive(e)) => a != e,
        (ResolvedType::Enum { def: a }, ResolvedType::Enum { def: e })
        | (ResolvedType::Variant { def: a, .. }, ResolvedType::Variant { def: e, .. })
        | (ResolvedType::Newtype { def: a, .. }, ResolvedType::Newtype { def: e, .. })
        | (ResolvedType::Flags { def: a }, ResolvedType::Flags { def: e }) => a != e,
        (
            ResolvedType::Struct {
                def: a,
                type_args: a_args,
            },
            ResolvedType::Struct {
                def: e,
                type_args: e_args,
            },
        ) if !a_args.is_empty() || !e_args.is_empty() => {
            a_args.is_empty() || e_args.is_empty() || a != e
        }
        (a, e) => std::mem::discriminant(a) != std::mem::discriminant(e),
    }
}

/// Whether a type argument `actual` of `!` meets an `expected` other than `!`
/// where the value cannot be rebuilt.
fn needs_rebuild(actual: TypeId, expected: TypeId, fixed: bool) -> bool {
    fixed && actual == TypeTable::NEVER && expected != TypeTable::NEVER
}

/// The arguments of `actual` and `expected` where both are one constructor
/// at their own arguments: two tuples, two arrays, or two instances of one
/// declaration.
fn same_constructor_args(
    actual: TypeId,
    expected: TypeId,
    type_table: &TypeTable,
) -> Option<(&[TypeId], &[TypeId])> {
    match (type_table.get(actual), type_table.get(expected)) {
        (ResolvedType::Tuple(a), ResolvedType::Tuple(e)) => Some((a.as_slice(), e.as_slice())),
        (ResolvedType::BuiltinArray(a), ResolvedType::BuiltinArray(e)) => {
            Some((std::slice::from_ref(a), std::slice::from_ref(e)))
        }
        _ => match (
            declaration_instance(actual, type_table),
            declaration_instance(expected, type_table),
        ) {
            (Some((a, a_args)), Some((e, e_args))) if a == e => Some((a_args, e_args)),
            _ => None,
        },
    }
}

/// The declaration `id` instantiates and its type arguments, where it is a
/// generic struct, resource or variant, or a newtype; a non-generic one has
/// none.
fn declaration_instance(id: TypeId, type_table: &TypeTable) -> Option<(DefId, &[TypeId])> {
    match type_table.get(id) {
        ResolvedType::Struct {
            def: StructDef::Decl(def),
            type_args,
        }
        | ResolvedType::Resource { def, type_args }
        | ResolvedType::Variant { def, type_args }
        | ResolvedType::Newtype { def, type_args, .. } => Some((*def, type_args.as_slice())),
        _ => None,
    }
}

/// Two instances of one generic declaration, compared argument by argument;
/// an argument is an element's type, so it admits no subtype.
fn check_type_args(
    actual: &[TypeId],
    expected: &[TypeId],
    type_table: &TypeTable,
    fixed: bool,
) -> TypeCheckResult {
    if actual.len() != expected.len() {
        return TypeCheckResult::Incompatible;
    }
    for (a, e) in actual.iter().zip(expected) {
        if needs_rebuild(*a, *e, fixed) {
            return TypeCheckResult::Incompatible;
        }
        match check_at(*a, *e, type_table, Position::Invariant, fixed) {
            TypeCheckResult::Compatible => {}
            verdict => return verdict,
        }
    }
    TypeCheckResult::Compatible
}

/// Whose frame a slot's type is written in, which says whose effect
/// parameters its rows name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SlotFrame {
    /// The enclosing function's: a binding, a `return`, a cast's target.
    Own,
    /// A callee's parameter, whose effect parameters the call resolves.
    Callee,
}

/// Two tuple types over the same packs are the same type only when their
/// layouts line up. Each pack stands for the same arity on both sides, so every
/// other element has a settled offset: `[..R, L]` is never `[..R]`, never
/// `[L, ..R]`, and `[..A, ..B]` is never `[..B, ..A]`. Decidable before the
/// packs expand, which is the only point where a body carrying one is checked.
fn pack_shape_conflict(actual: TypeId, expected: TypeId, type_table: &TypeTable) -> bool {
    let (actual_inner, _) = unwrap_ref(actual, type_table);
    let (expected_inner, _) = unwrap_ref(expected, type_table);
    let (Some(actual_layout), Some(expected_layout)) = (
        type_table.tuple_layout(actual_inner),
        type_table.tuple_layout(expected_inner),
    ) else {
        return false;
    };
    // Two tuples naming different packs are two declarations' slots, which a
    // call settles; only the same packs make them one type to compare.
    let packs = packs_named(&actual_layout);
    if packs.is_empty() || packs != packs_named(&expected_layout) {
        return false;
    }
    if actual_layout != expected_layout {
        return true;
    }
    // Equal layouts put every fixed element at the same offset. Invariant, as
    // rule 8 compares a generic instance's arguments; only a settled element
    // conflicts, an undecided one defers with everything else.
    let actual_elems = type_table.as_tuple(actual_inner).unwrap();
    let expected_elems = type_table.as_tuple(expected_inner).unwrap();
    actual_layout
        .iter()
        .enumerate()
        .filter(|(_, slot)| **slot == TupleSlot::Fixed)
        .any(|(i, _)| {
            check_at(
                actual_elems[i],
                expected_elems[i],
                type_table,
                Position::Invariant,
                false,
            ) == TypeCheckResult::Incompatible
        })
}

/// The packs a layout names, sorted and deduplicated, so two tuples naming the
/// same packs in different orders still compare as one type.
fn packs_named(layout: &[TupleSlot]) -> Vec<TypeParamId> {
    let mut packs: Vec<TypeParamId> = layout
        .iter()
        .filter_map(|slot| match slot {
            TupleSlot::Pack(pack) => Some(*pack),
            TupleSlot::Fixed => None,
        })
        .collect();
    packs.sort_unstable();
    packs.dedup();
    packs
}

/// Unwrap one layer of Ref/MutRef, returning (`inner_type`, `was_ref`).
fn unwrap_ref(type_id: TypeId, type_table: &TypeTable) -> (TypeId, bool) {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => (*inner, true),
        _ => (type_id, false),
    }
}

/// Pretty-printed payload carried when [`TypeSystem::typecheck`] rejects an
/// assignment.
///
/// The strings are materialised at the rejection site (while the
/// [`TypeTable`] is still borrowed) so the caller can drop the type
/// table before emitting the diagnostic. Both fields use the same
/// [`TypeTable::name`] formatting that the original
/// `TypeError::TypeMismatch` diagnostic used.
#[derive(Debug, Clone)]
pub(crate) struct TypeMismatchPayload {
    pub(crate) expected: String,
    pub(crate) found: String,
}

impl TypeSystem {
    /// Host-agnostic type-mismatch check. Returns `Ok(())` if `actual`
    /// is assignable to `expected` (or if the answer is deferred —
    /// unknown types, unresolved generics — which the body walk
    /// re-checks once the type is known); returns `Err` with the
    /// pretty-printed `expected` / `found` names on a definite reject.
    ///
    /// This is layer 2 of [the typecheck stack](self): callers that
    /// want a diagnostic emitted should go through
    /// [`Elaborator::typecheck`].
    pub(crate) fn typecheck(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> Result<(), TypeMismatchPayload> {
        let type_table = self.type_table.borrow();
        match check_assignable(actual, expected, &type_table) {
            TypeCheckResult::Incompatible => {
                let (expected, found) = type_table.names_for_mismatch(expected, actual);
                Err(TypeMismatchPayload { expected, found })
            }
            TypeCheckResult::Compatible | TypeCheckResult::Deferred => Ok(()),
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Check type mismatch and emit a [`TypeError::TypeMismatch`]
    /// diagnostic via [`Self::logger`] on rejection.
    ///
    /// Layer 3 wrapper over [`TypeSystem::typecheck`]: confines the
    /// `<H: CompilerHost>` plumbing to the `Elaborator` boundary so
    /// `TypeSystem` itself stays host-agnostic.
    pub(super) fn typecheck(&mut self, actual: TypeId, expected: TypeId, span: Span) {
        self.typecheck_in(actual, expected, span, SlotFrame::Own, Settles::Variables);
    }

    /// [`Self::typecheck`] on `value` meeting its slot, recording the rebuild
    /// an `Option<!>` it holds needs there.
    pub(super) fn typecheck_value(&mut self, value: &Expr, actual: TypeId, expected: TypeId) {
        let settles = Settles::of(value);
        self.typecheck_in(actual, expected, value.span(), SlotFrame::Own, settles);
        self.record_widening(value.id(), actual, expected);
    }

    /// Record that the value `value` meets a slot of `expected`, where its type
    /// holds or may yet hold an `Option<!>`. Reify rebuilds it as `expected`
    /// where the settled types say so ([`widens`]).
    pub(super) fn record_widening(&mut self, value: AstId, actual: TypeId, expected: TypeId) {
        let unsettled = {
            let table = self.tysys.type_table.borrow();
            table.contains_never_arg(actual) || table.contains_undecided(actual)
        };
        if unsettled {
            self.sem.types.widenings.insert(value, expected);
        }
    }

    /// [`Self::typecheck`] on a value bound to a callee's parameter, whose type
    /// is written in the callee's frame.
    pub(super) fn typecheck_into_callee(
        &mut self,
        actual: TypeId,
        expected: TypeId,
        span: Span,
        settles: Settles,
    ) {
        self.typecheck_in(actual, expected, span, SlotFrame::Callee, settles);
    }

    fn typecheck_in(
        &mut self,
        actual: TypeId,
        expected: TypeId,
        span: Span,
        frame: SlotFrame,
        settles: Settles,
    ) {
        self.typecheck_worded(actual, expected, span, frame, settles, move |payload| {
            TypeError::TypeMismatch {
                expected: payload.expected,
                found: payload.found,
                span,
            }
        });
    }

    /// [`Self::typecheck_value`] on a call argument, worded by
    /// [`ArgSite::mismatch`].
    pub(super) fn typecheck_arg(&mut self, actual: TypeId, expected: TypeId, site: ArgSite) {
        self.typecheck_worded(
            actual,
            expected,
            site.span(),
            SlotFrame::Callee,
            site.settles(),
            move |payload| site.mismatch(payload.expected, payload.found),
        );
        if let ArgSite::Written { value, .. } = site {
            self.record_widening(value, actual, expected);
        }
    }

    fn typecheck_worded(
        &mut self,
        actual: TypeId,
        expected: TypeId,
        span: Span,
        frame: SlotFrame,
        settles: Settles,
        mismatch: impl FnOnce(TypeMismatchPayload) -> TypeError + 'static,
    ) {
        let [actual, expected] = match settles {
            Settles::Variables => self.equated(actual, expected),
            Settles::Nothing => [actual, expected],
        };
        if let Err(payload) = self.tysys.typecheck(actual, expected) {
            let _ = self.emit(mismatch(payload));
            return;
        }
        self.defer_check(actual, expected, mismatch);
        self.record_row_flows(actual, expected, span, frame);
        self.reject_pack_layout_mismatch(actual, expected, span);
    }

    /// [`check_assignable`], recording what [`Self::record_row_flows`] says
    /// where the value is admitted.
    pub(super) fn assignable(
        &mut self,
        actual: TypeId,
        expected: TypeId,
        span: Span,
    ) -> TypeCheckResult {
        let [actual, expected] = self.equated(actual, expected);
        let result = check_assignable(actual, expected, &self.tysys.type_table.borrow());
        if result != TypeCheckResult::Incompatible {
            self.record_row_flows(actual, expected, span, SlotFrame::Own);
        }
        if result == TypeCheckResult::Deferred {
            self.defer_check(actual, expected, move |payload| TypeError::TypeMismatch {
                expected: payload.expected,
                found: payload.found,
                span,
            });
        }
        result
    }

    /// `actual` and `expected` once a value of the one meets a slot of the
    /// other: [`Self::equate`]d, and what that settled put in.
    fn equated(&mut self, actual: TypeId, expected: TypeId) -> [TypeId; 2] {
        self.equate(actual, expected);
        [
            self.apply_infer_holes(actual),
            self.apply_infer_holes(expected),
        ]
    }

    /// Ask the check of `actual` against `expected` again once the variables
    /// either holds settle, worded by `mismatch` if it fails then.
    fn defer_check(
        &self,
        actual: TypeId,
        expected: TypeId,
        mismatch: impl FnOnce(TypeMismatchPayload) -> TypeError + 'static,
    ) {
        if self.type_has_infer_hole(actual) || self.type_has_infer_hole(expected) {
            self.infer_holes.require(Obligation::Check(DeferredCheck {
                actual,
                expected,
                mismatch: Box::new(mismatch),
            }));
        }
    }

    /// Record each row of `actual` beside the row of `expected` it moves into,
    /// where either holds an effect variable. The type check admits a variable
    /// unbounded, since only the effect check knows what it stands for; the
    /// effect check bounds or widens it there. A callee's slot names the
    /// callee's own effect parameters, which the argument determines rather
    /// than meets, so those rows are left to the call's resolution.
    pub(super) fn record_row_flows(
        &self,
        actual: TypeId,
        expected: TypeId,
        span: Span,
        frame: SlotFrame,
    ) {
        let module = self.current_module_source.source_path();
        let mut table = self.tysys.type_table.borrow_mut();
        let mut flows = Vec::new();
        table.zip_rows(expected, actual, false, &mut |into, from| {
            let determined_by_it =
                frame == SlotFrame::Callee && into.iter().any(EffectRef::is_param);
            let open = from.iter().chain(into).any(EffectRef::is_var);
            if open && from != into && !determined_by_it {
                flows.push(RowFlow {
                    from: from.to_vec(),
                    into: into.to_vec(),
                    span,
                    module: module.clone(),
                });
            }
        });
        for flow in flows {
            table.record_row_flow(flow);
        }
    }

    /// Reject a value carrying no pack where one element of a pack, or a tuple
    /// of packs, this signature declares is expected. `pack_shape_conflict`
    /// compares two tuples naming the same packs; a value naming none is the
    /// case only a scope can settle.
    fn reject_pack_layout_mismatch(&self, actual: TypeId, expected: TypeId, span: Span) {
        let table = self.tysys.type_table.borrow();
        let carries_pack = |ty: TypeId| {
            table.contains_type_pack(ty)
                || matches!(
                    table.get(table.peel_refs(ty)),
                    ResolvedType::PackElement { .. }
                )
        };
        if !carries_pack(expected) || carries_pack(actual) || table.awaits_inference(actual) {
            return;
        }
        let (expected_referent, _) = unwrap_ref(expected, &table);
        let (actual_referent, _) = unwrap_ref(actual, &table);
        match table.get(expected_referent) {
            ResolvedType::TypePack { id: pack, .. } => {
                // Inside an expansion the bare pack name spells the step's element.
                if matches!(table.get(actual_referent), ResolvedType::TypeParam { id } if id == pack)
                {
                    return;
                }
            }
            ResolvedType::PackElement { .. } => {}
            _ => {
                let Some(layout) = table.tuple_layout(expected) else {
                    return;
                };
                if table.tuple_layout(actual) == Some(layout) {
                    return;
                }
            }
        }
        let scope = self.scope_type_param_ids();
        if !table
            .packs(expected)
            .into_iter()
            .all(|pack| scope.ids().contains(&pack))
        {
            return;
        }
        let (expected_name, found_name) = (table.name(expected), table.name(actual));
        drop(table);
        let _ = self.emit(TypeError::TypeMismatch {
            expected: expected_name,
            found: found_name,
            span,
        });
    }
}
