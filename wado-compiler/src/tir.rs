//! Typed Intermediate Representation — the post-type-resolution form used for
//! lowering, optimization, and codegen. Every type is a `TypeId`, every variable
//! reference a local index, every call a resolved target, and all syntactic
//! sugar is already desugared.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use crate::builtin_facts::{BuiltinFacts, ParamShape, ReturnShape, SideEffect, Storage, TrapCheck};
use crate::call_args::CallArgs;
use crate::canonical::CmCallTarget;
use crate::compiler_item::CompilerItem;
use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{NamePolicy, RangeKind, RestClause, Visibility};
use crate::defs::{DefId, DefTable};
use crate::hashmap;
use crate::module_source::ModuleSource;
use crate::name::{DeclaredHead, FunctionId, LocalMethodName, checked_name, unchecked_twin_name};
use crate::primitive::PrimitiveType;
use crate::symbol_notation::render;
use crate::token::Span;
use crate::type_table::{AnonStructId, TypeKey, TypeTable};

/// `ReflectNewtype`'s only associated type (`type Base`): what the newtype
/// wraps. Sealed and compiler-defined, so its spelling is fixed rather than
/// registry-driven.
pub(crate) const REFLECT_NEWTYPE_BASE: &str = "Base";

/// Identifies the scope where a type parameter is defined
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeParamScope {
    /// Type parameter from struct/impl block (e.g., T in `impl Container<T>`)
    Impl,
    /// Type parameter from method signature (e.g., U in `fn transform<U>`)
    Method,
    /// Type parameter from free function (e.g., T in `fn identity<T>`)
    Function,
}

/// A resolved effect reference — a bare string in the AST. `Concrete` is
/// identified by its declaration, so `with Stdout` imported from `wasi:cli` and
/// from `core:cli` compare equal; `Param` by its name alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EffectRef {
    /// The effect (or resource) a name resolved to.
    Concrete(DeclaredHead),
    /// A `with`-clause name written in `module_source` that reaches no effect,
    /// already reported: there is no declaration to identify it by.
    Undeclared {
        name: String,
        module_source: ModuleSource,
    },
    /// An effect parameter from a generic effect declaration (e.g., `E` in `<effect E>`)
    Param { name: String },
    /// A row the elaborator cannot know yet, which the effect check solves: a
    /// closure literal's, or an effect parameter one call instantiates. Minted
    /// by [`TypeTable::mint_effect_var`]; `name` is the id's spelling.
    Var { id: u32, name: String },
}

impl EffectRef {
    pub fn name(&self) -> &str {
        match self {
            EffectRef::Concrete(head) => head.name(),
            EffectRef::Undeclared { name, .. }
            | EffectRef::Param { name }
            | EffectRef::Var { name, .. } => name,
        }
    }

    /// The module a concrete or undeclared effect is named in.
    pub fn module(&self) -> Option<&ModuleSource> {
        match self {
            EffectRef::Concrete(head) => Some(head.module()),
            EffectRef::Undeclared { module_source, .. } => Some(module_source),
            EffectRef::Param { .. } | EffectRef::Var { .. } => None,
        }
    }

    /// The name a message prints: in `MODULE#SYMBOL` notation when `qualified`,
    /// for a concrete effect that another of its name would otherwise print as.
    pub fn display_name(&self, qualified: bool) -> String {
        match self.module() {
            Some(module) if qualified => render(&module.to_string(), self.name()),
            _ => self.name().to_string(),
        }
    }

    pub fn is_param(&self) -> bool {
        matches!(self, EffectRef::Param { .. })
    }

    /// Whether this is a row still to be solved.
    pub fn is_var(&self) -> bool {
        matches!(self, EffectRef::Var { .. })
    }

    /// A `with`-clause name written in `module` that reaches no effect.
    pub fn unresolved(name: &str, module: &ModuleSource) -> Self {
        EffectRef::Undeclared {
            name: name.to_string(),
            module_source: module.clone(),
        }
    }
}

/// Identifies a type parameter with its scope and index
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeParamId {
    pub scope: TypeParamScope,
    pub index: u32,
}

impl TypeParamId {
    pub fn impl_param(index: u32) -> Self {
        Self {
            scope: TypeParamScope::Impl,
            index,
        }
    }

    pub fn method_param(index: u32) -> Self {
        Self {
            scope: TypeParamScope::Method,
            index,
        }
    }

    pub fn function_param(index: u32) -> Self {
        Self {
            scope: TypeParamScope::Function,
            index,
        }
    }
}

/// What a use site chose for a declaration's type parameters.
///
/// Keyed by the parameter itself. A slot carries its own index, and only the
/// declaration knows which index each of its parameters holds — a generic,
/// `&`-target, blanket or variadic-tuple impl numbers its slots differently,
/// and a partially concrete target leaves gaps no positional list can express.
/// Keying by position asks the caller to reconstruct that, which it cannot;
/// `impl<T, ..F> Emit for T` is enough to break the reconstruction.
/// `instantiate_call` takes the same view.
#[derive(Debug, Clone, Default)]
pub struct SubstitutionContext {
    substitutions: IndexMap<TypeId, TypeId>,
}

impl SubstitutionContext {
    pub fn new() -> Self {
        Self {
            substitutions: IndexMap::default(),
        }
    }

    /// Bind a declaration's type parameters to the arguments a use site chose.
    ///
    /// `params` are the parameters as the declaration holds them: a lookup
    /// reports them, a declaration record carries them. Nothing recomputes
    /// them. Parameters past the end of `args` stay unbound.
    pub fn bind(mut self, params: &[TypeId], args: &[TypeId]) -> Self {
        for (&param, &arg) in params.iter().zip(args.iter()) {
            self.substitutions.insert(param, arg);
        }
        self
    }

    /// Substitute type parameters in a type
    pub fn substitute(&self, type_id: TypeId, type_table: &mut TypeTable) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } => {
                self.substitutions.get(&type_id).copied().unwrap_or(type_id)
            }
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                args,
                owning_trait,
                trait_args,
                assoc_type_bindings,
            } => {
                let concrete_id = self.substitute(param_id, type_table);
                let new_args: Vec<TypeId> = args
                    .iter()
                    .map(|&a| self.substitute(a, type_table))
                    .collect();
                let new_trait_args: Option<Vec<TypeId>> = trait_args.as_ref().map(|given| {
                    given
                        .iter()
                        .map(|&a| self.substitute(a, type_table))
                        .collect()
                });
                if let Some(resolved) = type_table.answer_projection(
                    concrete_id,
                    &owning_trait,
                    new_trait_args.as_deref(),
                    &assoc_name,
                    &new_args,
                ) {
                    return resolved;
                }
                // The associated type cannot be resolved yet because the
                // projected parameter is still abstract (e.g. substituting
                // `I::Item` with `I -> I`, or `I -> some other type param`).
                // Preserve the projection over the substituted parameter
                // rather than collapsing to the bare parameter, so later
                // monomorphization can resolve `concrete::assoc_name`. What it
                // carries is written in the same frame, so it substitutes too.
                let new_bindings: Vec<(String, TypeId)> = assoc_type_bindings
                    .iter()
                    .map(|(name, bound)| (name.clone(), self.substitute(*bound, type_table)))
                    .collect();
                if concrete_id == param_id
                    && new_args == args
                    && new_trait_args == trait_args
                    && new_bindings == assoc_type_bindings
                {
                    type_id
                } else {
                    type_table.make_assoc_type_projection(
                        concrete_id,
                        owning_trait,
                        new_trait_args,
                        assoc_name,
                        new_args,
                        new_bindings,
                    )
                }
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute(elem, type_table);
                type_table.make_builtin_array(new_elem)
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.make_mut_ref(new_inner)
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let splices_packs = type_table.is_tuple_def(def);
                let mut new_args: Vec<TypeId> = Vec::new();
                for &arg in &type_args {
                    // A pack in a tuple stands for the elements it took, not
                    // for the tuple holding them: `[..T]` with `T = [i32,
                    // bool]` is `[i32, bool]`, never `[[i32, bool]]`.
                    let is_plain_pack = splices_packs
                        && matches!(
                            type_table.get(arg),
                            ResolvedType::TypePack {
                                mapped_elem: None,
                                ..
                            }
                        );
                    let substituted = self.substitute(arg, type_table);
                    if is_plain_pack
                        && substituted != arg
                        && let Some(elements) = type_table.as_tuple(substituted)
                    {
                        new_args.extend(elements);
                        continue;
                    }
                    new_args.push(substituted);
                }
                type_table.make_generic_instance(def, new_args)
            }
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
            } => {
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute(p, type_table))
                    .collect();
                let new_return = self.substitute(return_type, type_table);
                type_table.make_function_with_mut(is_mut, new_params, new_return, effects)
            }
            // Other types don't contain type parameters
            _ => type_id,
        }
    }

    /// Check if this context has any substitutions
    pub fn is_empty(&self) -> bool {
        self.substitutions.is_empty()
    }
}

/// Type identifier for resolved types in TIR.
/// This is a newtype wrapper to prevent misuse of raw integers as `TypeId`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TypeId(pub u32);

impl std::fmt::Display for TypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A set of heap object types, keyed by [`TypeTable::heap_object_key`]; `any`
/// stands for every type.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct ObjectTypes {
    any: bool,
    keys: IndexSet<TypeKey>,
}

impl ObjectTypes {
    /// The set of every type.
    pub fn everything() -> Self {
        Self {
            any: true,
            keys: IndexSet::default(),
        }
    }

    /// The set of `key` alone.
    pub fn one(key: TypeKey) -> Self {
        Self {
            any: false,
            keys: std::iter::once(key).collect(),
        }
    }

    /// Whether the set is every type.
    pub fn is_any(&self) -> bool {
        self.any
    }

    /// The keys of a set that is not [`Self::is_any`].
    pub fn keys(&self) -> impl Iterator<Item = TypeKey> + '_ {
        assert!(!self.any, "an `any` set has no key list");
        self.keys.iter().copied()
    }

    /// Whether `key` is in the set.
    pub fn contains(&self, key: TypeKey) -> bool {
        self.any || self.keys.contains(&key)
    }

    /// Whether the set has no type.
    pub fn is_empty(&self) -> bool {
        !self.any && self.keys.is_empty()
    }

    /// Add `key`.
    pub fn insert(&mut self, key: TypeKey) {
        if !self.any {
            self.keys.insert(key);
        }
    }

    /// Make the set every type.
    pub fn set_any(&mut self) {
        self.any = true;
        self.keys.clear();
    }

    /// `self ∪= other`.
    pub fn union(&mut self, other: &ObjectTypes) {
        if other.any {
            self.set_any();
        } else if !self.any {
            self.keys.extend(other.keys.iter().copied());
        }
    }

    /// Whether `self ∩ other` is non-empty.
    pub fn meets(&self, other: &ObjectTypes) -> bool {
        match (self.any, other.any) {
            (true, true) => true,
            (true, false) => !other.keys.is_empty(),
            (false, true) => !self.keys.is_empty(),
            (false, false) => self.keys.iter().any(|k| other.keys.contains(k)),
        }
    }

    /// `self ∪= a ∩ b`.
    pub fn union_meet(&mut self, a: &ObjectTypes, b: &ObjectTypes) {
        match (a.any, b.any) {
            (true, true) => {
                self.set_any();
            }
            (true, false) => {
                self.union(b);
            }
            (false, true) => {
                self.union(a);
            }
            (false, false) => {
                for &k in &a.keys {
                    if b.keys.contains(&k) {
                        self.insert(k);
                    }
                }
            }
        }
    }
}

/// Identity of an inference variable — see [`ResolvedType::InferVar`].
///
/// Minted per module by the elaborator, so two uses of the same polymorphic
/// signature get distinct variables. Unlike a [`ResolvedType::TypeParam`]
/// index, this is not positional: it names one unknown, not a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InferVarId(pub u32);

impl std::fmt::Display for InferVarId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "?{}", self.0)
    }
}

/// A struct shape the compiler minted rather than source declared.
///
/// A struct literal is identified by its fields, because two literals of the
/// same shape are one type. A closure environment is identified by the name
/// lowering assigns it, because two closures with identical captures are
/// still two environments.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum AnonShape {
    Fields(Vec<(String, TypeId)>),
    Synthetic(String),
    /// The type a tagged template literal denotes, identified by its static
    /// shape: two sites writing one template reach one type (WEP 2026-01-10).
    Template(TemplateShape),
}

/// One hole of a template shape: its expression's type and its specifier as
/// written.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TemplateHole {
    pub ty: TypeId,
    pub spec: Option<String>,
}

/// The static part of a tagged template literal: its holes. The literal
/// segments are no part of it: a value carries its own, so templates differing
/// only in their text share one type and one instantiation of the tag.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TemplateShape {
    pub holes: Vec<TemplateHole>,
}

impl TemplateShape {
    /// The struct field holding hole `k`.
    #[must_use]
    pub fn field_name(k: usize) -> String {
        format!("h{k}")
    }

    /// Segment `k`, cooked or raw, as `builtin::template_segment` numbers it.
    #[must_use]
    pub fn segment_key(k: usize, raw: bool) -> u32 {
        (2 * k + usize::from(raw)) as u32
    }

    /// The segment `key` numbers, and whether it is the raw form.
    #[must_use]
    pub fn segment_of(key: u32) -> (usize, bool) {
        ((key / 2) as usize, key % 2 == 1)
    }

    /// The field holding the segment `key` numbers. The segments follow the
    /// holes, so a hole's field index is its own.
    #[must_use]
    pub fn segment_field(&self, key: u32) -> (String, u32) {
        let name = match Self::segment_of(key) {
            (k, false) => format!("s{k}"),
            (k, true) => format!("r{k}"),
        };
        (name, self.holes.len() as u32 + key)
    }

    /// The segment keys, one cooked and one raw per segment, in field order.
    pub fn segment_keys(&self) -> std::ops::Range<u32> {
        0..Self::segment_key(self.holes.len() + 1, false)
    }

    #[must_use]
    pub fn hole_types(&self) -> Vec<TypeId> {
        self.holes.iter().map(|hole| hole.ty).collect()
    }
}

/// Hold `field`'s value, a hole of type `hole_ty`, as the shape's field does
/// ([`TypeTable::hole_field_type`]), and answer the local the `&` takes, which
/// the caller records as address-taken. Only a closed type decides it, so an
/// open shape's literal is held once substituted; holding twice is a no-op.
pub fn hold_hole(field: &mut TirStructField, hole_ty: TypeId, tt: &mut TypeTable) -> Option<u32> {
    let field_ty = tt.hole_field_type(hole_ty);
    // Monomorphization holds a substituted shape before its instances are
    // drained, while one generic instance may still be spelled both as a
    // `GenericInstance` and as its struct.
    let same = |tt: &TypeTable, a: TypeId, b: TypeId| {
        tt.type_key(a) == tt.type_key(b) || tt.instance_key(a) == tt.instance_key(b)
    };
    if same(tt, field_ty, hole_ty) || same(tt, field.value.type_id, field_ty) {
        return None;
    }
    assert!(
        same(tt, field.value.type_id, hole_ty),
        "a hole's value has the hole's type"
    );
    let taken = match &field.value.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        _ => None,
    };
    let span = field.value.span;
    let value = std::mem::replace(
        &mut field.value,
        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
    );
    field.value = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(value),
        },
        field_ty,
        span,
    );
    taken
}

/// `expr` dereferenced until `layers` references remain over its pointee,
/// folding `*&e` to `e`: how a receiver reaches the impl of its pointee through
/// however many references hold it.
pub fn deref_to_layers(mut expr: TirExpr, layers: usize, type_table: &TypeTable) -> TirExpr {
    while type_table.ref_depth(expr.type_id) > layers {
        let pointee = type_table
            .ref_pointee(expr.type_id)
            .expect("a type with a reference layer points at something");
        expr = match expr.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: taken,
            } => *taken,
            kind => {
                let span = expr.span;
                TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: Box::new(TirExpr { kind, ..expr }),
                    },
                    pointee,
                    span,
                )
            }
        };
    }
    expr
}

/// `expr` holding as many references over its pointee as `target` does: a layer
/// too many dereferenced, a missing one taken, as a receiver spelled as the
/// first argument of `Trait::method(recv)` is adjusted to its `self`.
pub fn to_layers_of(expr: TirExpr, target: TypeId, type_table: &TypeTable) -> TirExpr {
    let want = type_table.ref_depth(target);
    let have = type_table.ref_depth(expr.type_id);
    if have >= want {
        return deref_to_layers(expr, want, type_table);
    }
    let missing: Vec<TypeId> = std::iter::successors(Some(target), |&t| type_table.ref_pointee(t))
        .take(want - have)
        .collect();
    missing.into_iter().rev().fold(expr, |expr, layer| {
        let op = if matches!(type_table.get(layer), ResolvedType::MutRef(_)) {
            TirUnaryOp::MutRef
        } else {
            TirUnaryOp::Ref
        };
        let span = expr.span;
        TirExpr::new(
            TirExprKind::Unary {
                op,
                expr: Box::new(expr),
            },
            layer,
            span,
        )
    })
}

/// The reference the newtype `expr` holds wraps (`type H = &mut T`): `expr`'s
/// own references dereferenced away, then each newtype cast to its base.
pub fn wrapped_reference(expr: TirExpr, type_table: &TypeTable) -> TirExpr {
    let mut expr = deref_to_layers(expr, 0, type_table);
    while type_table.ref_pointee(expr.type_id).is_none() {
        let ResolvedType::Newtype { base_type, .. } = *type_table.get(expr.type_id) else {
            unreachable!("the caller passes a newtype over a reference");
        };
        let span = expr.span;
        expr = TirExpr::new(
            TirExprKind::Cast {
                expr: Box::new(expr),
                target_type: base_type,
            },
            base_type,
            span,
        );
    }
    expr
}

/// A written shape a substituted one is made from, and the substitution that
/// makes it, over every parameter the origin mentions: what instantiating the
/// origin's methods applies. Binding the two shapes again cannot recover it
/// where a field is a pack or a projection.
#[derive(Debug, Clone)]
pub struct ShapeSubstitution {
    pub origin: AnonStructId,
    pub params: IndexMap<u32, TypeId>,
}

/// What a struct type's head is.
///
/// A struct literal with no type name has no declaration and no node to
/// identify it by — two literals of the same shape intern to one type on
/// purpose — so the head says which of the two it is rather than carrying a
/// `DefId` that would have to be invented for the second.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StructDef {
    Decl(DefId),
    Anon(AnonStructId),
}

impl StructDef {
    /// The declaration this head names, or `None` for a shape that names none.
    #[must_use]
    pub fn decl(self) -> Option<DefId> {
        match self {
            Self::Decl(def) => Some(def),
            Self::Anon(_) => None,
        }
    }
}

/// One position in a tuple's layout: a pack standing for a run of positions,
/// or a single slot holding one type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TupleSlot {
    Fixed,
    Pack(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResolvedType {
    Primitive(PrimitiveType),
    Unit,
    Never,
    Struct {
        /// The declaration this was written from, or the shape it was built
        /// from. Never a spelling: a monomorphized struct used to store the
        /// rendered `TreeMap<String,i32>` and a `base_name` to recover the head
        /// from, which is what let its spelling pass as a declaration name
        /// wherever the two were matched together.
        def: StructDef,
        /// What this instantiation was made with; empty for a declaration
        /// written as such. The rendered spelling is derived from the two —
        /// see [`TypeTable::struct_rendered_name`].
        type_args: Vec<TypeId>,
    },
    Enum {
        def: DefId,
    },
    /// Resource type - opaque handle (i32) to a Component Model resource
    Resource {
        def: DefId,
    },
    Variant {
        def: DefId,
    },
    // `Option<T>` is a `GenericInstance`, not a variant here — see
    // `TypeTable::as_option`.
    //
    // TODO: represent `Option<T>` as `ref null T` when `T` is a non-nullable
    // reference, dropping the discriminant struct. `Option<Option<T>>` must not
    // take that route — its null would be ambiguous.
    Ref(TypeId),
    MutRef(TypeId),
    Function {
        /// `true` for `fn mut(...)` (closure type that may mutate its captures).
        /// `false` for `fn(...)` (read-only).
        is_mut: bool,
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<EffectRef>,
    },
    /// Type parameter (e.g., `T` in `struct Box<T>`) — a *rigid* variable.
    ///
    /// It stands for whatever a caller instantiates the binding item with, so
    /// inside that item it is opaque: nothing but itself is assignable to it.
    /// It appears only within the item that binds it; a *use* of a polymorphic
    /// signature instantiates these into [`ResolvedType::InferVar`]s.
    TypeParam {
        name: String,
        /// Index of the type parameter in the generic definition (0 for first param)
        index: u32,
    },
    /// Inference variable — a *flexible* variable standing for a type the
    /// solver has yet to determine. Minted when a use site instantiates a
    /// polymorphic signature: where a rigid parameter rejects anything but
    /// itself, a variable accepts and records.
    ///
    /// Reaches no recorded fact — `finalize_infer_holes` substitutes every
    /// one away. The intermediate types built on one stay interned, as every
    /// type ever considered does, so a pass enumerating
    /// [`TypeTable::all_types`] must select with [`TypeTable::is_concrete`].
    InferVar(InferVarId),
    /// Type pack parameter (`..T` in `fn foo<..T>(x: [..T])`), living inside
    /// tuples until substitution expands it. `mapped_elem` separates an identity
    /// pack (`None` — element `i` is `F_i`) from a mapped one (`Some(R)` —
    /// element `i` is `R[F := F_i]`, degenerating to `|F|` copies of `R` when
    /// `R` does not mention the pack).
    TypePack {
        name: String,
        index: u32,
        mapped_elem: Option<TypeId>,
    },
    /// One element of an identity pack: what `for let v of t` over `t: [..T]`
    /// binds `v` to. Apart from the pack because a tuple splices a pack it
    /// holds: `[v, 7]` is `[T_k, i32]`, where the pack would make it
    /// `[..T, i32]`. It carries the pack's bounds, and unrolling substitutes
    /// each element for it.
    ///
    /// Nested walks over one pack bind different elements, so the element is
    /// also the walk's: `x` and `v` in `for x of t { for v of t { … } }` have
    /// two types, and `pick::<A>(x, v)` does not unify them.
    PackElement {
        /// The identity [`ResolvedType::TypePack`] this is an element of.
        pack: TypeId,
        /// How many pack walks enclose the one binding this element: its
        /// substitution slot is `TypeTable::pack_walk_slot`.
        walk: u32,
    },
    /// A generic associated type's own parameter (`E` in `type Buf<E> =
    /// List<E>;`), standing in a binding for the argument a projection supplies.
    ///
    /// Apart from [`ResolvedType::TypeParam`] because the two are filled at
    /// different times: an impl's parameters by the receiver, this by each
    /// projection's arguments, which [`TypeTable::instantiate_family`] fills.
    /// So a binding holding only these is as concrete as its impl.
    ///
    /// Identified by its position in the declaration, never by the name a
    /// site writes it under: an impl may rename it, and the two are one.
    AssocParam {
        owning_trait: DefId,
        assoc_name: String,
        index: u32,
        /// The declaration's spelling, for messages.
        name: String,
    },
    /// An application of a generic declaration (`Box<i32>`, `Option<T>`,
    /// `Stream<u8>`). A struct's or a variant's is replaced by its
    /// monomorphized `Struct` / `Variant`; a resource's is the handle type
    /// itself and lasts to codegen ([`TypeTable::is_resource_def`]).
    GenericInstance {
        /// The generic declaration this instantiates.
        def: DefId,
        /// Concrete type arguments (e.g., [i32])
        type_args: Vec<TypeId>,
    },
    /// Associated type projection: `T::X` where T is a type parameter with a trait bound.
    /// During concrete instantiation this is resolved to the concrete associated type.
    AssocTypeProjection {
        /// The type-parameter `TypeId` (must be a `TypeParam` variant)
        param_id: TypeId,
        /// Name of the associated type (e.g., `"Value"` in `T::Value`)
        assoc_name: String,
        /// The associated type's own arguments (`i32` in `T::Buf<i32>`), empty
        /// for one that declares no parameters.
        args: Vec<TypeId>,
        /// The trait declaring `assoc_name`: `Self::Err` inside `trait FromStr`
        /// is `<Self as FromStr>::Err`.
        // Part of the identity, so a projection built under one module's
        // `FromStr` is never answered by another's.
        owning_trait: DefId,
        /// The arguments `owning_trait` is reached at, one per its type
        /// parameter: `<T as Holder<i32>>::Out` and `<T as Holder<String>>::Out`
        /// are two types. `None` where a bare bound (`T: Holder`) left a
        /// parameter with no default unwritten.
        ///
        /// What the declaration bounds the projection by is read off its
        /// [`AssocTypeSig`] at these, never stored: a projection is built while
        /// signatures are, and two associated types may each bound the other.
        trait_args: Option<Vec<TypeId>>,
        /// Resolved associated type bindings (e.g., [("Item", `u8_typeid`)] for `I::Iter`
        /// when I: `IntoIterator`<Item = u8> and `IntoIterator::Iter`: Iterator<Item = `Self::Item`>)
        assoc_type_bindings: Vec<(String, TypeId)>,
    },
    /// Raw GC array intrinsic (`Array<T>`)
    /// This is the underlying storage type for `String` and `List<T>` structs
    BuiltinArray(TypeId),
    /// Newtype: a distinct type wrapping a base type with the same representation.
    /// Created by `type T = U;` declarations.
    /// Newtypes are distinct from their base types but can be cast between them.
    Newtype {
        def: DefId,
        /// What this instantiation was made with; empty for a declaration
        /// written as such. The same head/arguments split `Struct` has — the
        /// stored name used to bake them into the head (`MyArray<i32>`), which
        /// is a fused spelling no `impl` header writes.
        type_args: Vec<TypeId>,
        /// The direct base type (may be another newtype for chained newtypes)
        base_type: TypeId,
    },
    /// Flags: a bitmask type over u32.
    /// Created by `flags F { A, B, C }` declarations.
    /// Distinct from Newtype so flags can be detected without name-based lookup.
    Flags {
        def: DefId,
    },
    Unknown,
    Error,
}

/// A trait at the arguments it was instantiated at (WEP 2026-08-12): `Combine`
/// and `Combine<Inch>` are two. `args` holds only what an impl wrote beyond the
/// declared defaults, so a bound — writing none — is always [`TraitRef::bare`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TraitRef {
    pub decl: DefId,
    pub args: Vec<TypeId>,
}

impl TraitRef {
    #[must_use]
    pub fn bare(decl: DefId) -> Self {
        Self {
            decl,
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn new(decl: DefId, args: Vec<TypeId>) -> Self {
        Self { decl, args }
    }
}

/// A trait's own frame, resolved once: slot 0 is `Self`, its type parameters
/// follow, and each declared default is written over the ones before it.
#[derive(Debug, Clone)]
pub struct TraitFrame {
    /// The frame's `Self`.
    pub self_param: TypeId,
    /// One per the trait's type parameter (every one but an effect) in
    /// declaration order: the positions a trait reference's arguments fill.
    /// `None` for one the frame binds to no type.
    pub params: Vec<Option<TypeId>>,
    /// The default each of [`Self::params`] declares, in this frame.
    pub defaults: Vec<Option<TypeId>>,
}

/// What a trait declares of one of its associated types, resolved once in the
/// trait's own frame: slot 0 is `Self`, the trait's parameters follow, and the
/// associated type's own parameters are [`ResolvedType::AssocParam`]s. A reader
/// instantiates it by substitution, never by reading the declaration's names in
/// a frame of its own.
#[derive(Debug, Clone)]
pub struct AssocTypeSig {
    /// The trait declaring the associated type.
    pub owning_trait: DefId,
    /// The associated type's name.
    pub assoc_name: String,
    /// The declaring trait's frame, which the rest is written in.
    pub frame: Arc<TraitFrame>,
    /// The associated type's own parameters, in declaration order.
    pub params: Vec<TypeId>,
    /// The bounds each of [`Self::params`] declares.
    pub param_bounds: Vec<Vec<TraitRef>>,
    /// The bounds the associated type declares on itself (`type A: Bound`).
    pub bounds: Vec<TraitRef>,
}

/// Every argument a projection names apart from its base: the associated
/// type's own and its trait's. A walk reading one reads both.
pub fn projection_arguments<'a>(
    args: &'a [TypeId],
    trait_args: &'a Option<Vec<TypeId>>,
) -> impl Iterator<Item = TypeId> + 'a {
    args.iter().chain(trait_args.iter().flatten()).copied()
}

/// The prelude struct a `kind` range literal builds.
pub fn range_item(kind: RangeKind) -> CompilerItem {
    match kind {
        RangeKind::Exclusive => CompilerItem::RangeExclusive,
        RangeKind::Inclusive => CompilerItem::RangeInclusive,
    }
}

/// `args` keyed by the parameter slot each one fills, which is what
/// [`TypeTable::substitute_type_params`] reads.
#[must_use]
pub fn positional_substitution(args: &[TypeId]) -> IndexMap<u32, TypeId> {
    args.iter()
        .enumerate()
        .map(|(slot, &arg)| (slot as u32, arg))
        .collect()
}

#[derive(Debug, Clone)]
pub struct TirExpr {
    pub kind: TirExprKind,
    pub type_id: TypeId,
    pub span: Span,
    /// What a pass decided about this node, written where it decided it while
    /// it held the body by reference. The node is its own identity: a span is
    /// none, since every synthesized node shares one, and a copy carries the
    /// decision along.
    marks: Cell<ExprMarks>,
}

/// What lowering's ownership walk decided about one expression.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExprMarks {
    /// An assignment, or a `List::index_assign` call, that writes into the
    /// storage its place holds rather than repointing the place: a holder of
    /// that storage may read it afterwards (WEP 2026-10-13).
    pub in_place: bool,
    /// A value read, a whole local's or a materialization out of an aggregate,
    /// that hands its storage over, so it needs no copy (WEP 2026-05-21).
    pub moves: bool,
    /// A by-value argument whose storage the call's evaluation may write before
    /// the callee is done with it: a confined parameter still copies it.
    pub written: bool,
}

impl TirExpr {
    pub fn new(kind: TirExprKind, type_id: TypeId, span: Span) -> Self {
        Self {
            kind,
            type_id,
            span,
            marks: Cell::default(),
        }
    }

    #[must_use]
    pub fn marks(&self) -> ExprMarks {
        self.marks.get()
    }

    pub fn mark(&self, set: impl FnOnce(&mut ExprMarks)) {
        let mut marks = self.marks.get();
        set(&mut marks);
        self.marks.set(marks);
    }

    /// The place `self` moves out of, or `self` where it moves nothing.
    pub fn unmoved(&self) -> &Self {
        match &self.kind {
            TirExprKind::Move { expr } => expr,
            _ => self,
        }
    }

    /// The value of the place `self` names, moved out of it.
    pub fn moved(self) -> Self {
        let (type_id, span) = (self.type_id, self.span);
        Self::new(
            TirExprKind::Move {
                expr: Box::new(self),
            },
            type_id,
            span,
        )
    }
}

#[derive(Debug, Clone)]
pub struct FunctionRef {
    pub module_source: ModuleSource,
    pub name: String,
    /// The declaration the call's producer selected, which a template call
    /// instantiates. `None` where the producer selected none.
    pub template: Option<TemplateId>,
    pub monomorph_info: Option<MonomorphInfo>,
    pub method_info: Option<LocalMethodName>,
}

impl FunctionRef {
    /// Create a `FunctionRef` by extracting metadata from a resolved `TirFunction`.
    pub fn from_resolved(func: &TirFunction, module_source: ModuleSource) -> Self {
        Self {
            module_source,
            name: func.name.clone(),
            template: func.template_id(),
            monomorph_info: func.monomorph_info.clone(),
            method_info: func.method_info.clone(),
        }
    }

    /// Point this call at its callee's `unchecked` twin.
    pub fn retarget_to_unchecked_twin(&mut self) {
        self.name = unchecked_twin_name(&self.name);
        if let Some(info) = &mut self.method_info {
            info.mark_unchecked();
        }
        if let Some(mono) = &mut self.monomorph_info {
            mono.generic_name = unchecked_twin_name(&mono.generic_name);
        }
        self.template = self.template.as_ref().map(TemplateId::unchecked_twin);
    }

    /// Point this call, aimed at an `unchecked` twin, back at the function it
    /// is the twin of.
    pub fn retarget_to_checked(&mut self) {
        self.name = checked_name(&self.name).to_string();
        if let Some(info) = &mut self.method_info {
            info.mark_checked();
        }
        if let Some(mono) = &mut self.monomorph_info {
            mono.generic_name = checked_name(&mono.generic_name).to_string();
        }
        self.template = self.template.as_ref().and_then(TemplateId::checked);
    }

    /// The function this reference denotes, keyed as NIR's
    /// `FunctionRef::function_id` keys it. Not by `method_info`: monomorphization
    /// points a call at an instance by its `name` alone, so the template's
    /// `method_info` stays on the call.
    pub fn function_id(&self) -> FunctionId {
        FunctionId::free(&self.module_source, &self.name)
    }

    pub fn intrinsic(&self) -> Option<&str> {
        DeclarationLookup::from(self).intrinsic()
    }

    /// Whether this is the `core:builtin` intrinsic `builtin`.
    pub fn is_builtin_named(&self, builtin: &str) -> bool {
        self.intrinsic() == Some(builtin)
    }

    /// Check if this function is monomorphized (instantiated from a generic)
    pub fn is_monomorphized(&self) -> bool {
        self.monomorph_info.is_some()
    }

    /// Check if this is a method (instance or static) as opposed to a free function.
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }
}

/// A function argument bundled with its parameter mutability metadata.
///
/// `is_mut` reflects whether the callee declares this parameter as `mut`.
/// It controls value-copy semantics at the call site in the WIR translation phase.
/// An empty `args` list or missing metadata defaults to conservative (copy).
#[derive(Debug, Clone)]
pub struct CallArg {
    pub expr: TirExpr,
    /// Whether the callee declares this parameter as `mut`.
    pub is_mut: bool,
}

impl CallArg {
    pub fn new(expr: TirExpr, is_mut: bool) -> Self {
        Self { expr, is_mut }
    }
}

impl TirExprKind {
    /// Build `recv.m(args)`: a [`TirExprKind::Call`] whose receiver heads the
    /// argument list, so `args[i]` maps to the callee's `params[i]`.
    ///
    /// Callers are expected to have typechecked `args` against the callee's
    /// declared parameter types before reaching here.
    ///
    /// The receiver's `is_mut` is left `false`; `lower` fills the real value in
    /// from the callee's `self` parameter, which is where every consumer of it
    /// lives.
    pub(crate) fn method_call(
        receiver: Box<TirExpr>,
        func: FunctionRef,
        type_args: Vec<TypeId>,
        args: Vec<CallArg>,
    ) -> Self {
        Self::Call {
            func: Box::new(func),
            type_args,
            args: CallArgs::method(CallArg::new(*receiver, false), args),
        }
    }

    /// An instance-method call viewed as receiver plus the arguments after it.
    /// `None` for a free call.
    ///
    /// The split is a view, not storage: the node keeps one argument list in the
    /// callee's parameter order, so a pass that treats every argument alike
    /// (traversal, substitution, type-arg rewriting) matches `Call` directly and
    /// never needs this.
    pub fn as_method_call(&self) -> Option<(&TirExpr, &FunctionRef, &[CallArg])> {
        let TirExprKind::Call { func, args, .. } = self else {
            return None;
        };
        let (Some(receiver), rest) = args.split() else {
            return None;
        };
        Some((&receiver.expr, func, rest))
    }

    /// `args[0]` of a call whose callee declares a receiver, whichever spelling
    /// wrote it. [`Self::as_method_call`] answers for dot syntax alone, and
    /// gates the lowering rules only that spelling may take.
    pub fn call_receiver(&self) -> Option<(&TirExpr, &FunctionRef)> {
        let TirExprKind::Call { func, args, .. } = self else {
            return None;
        };
        Some((&args.first()?.expr, func))
    }
}

#[derive(Debug, Clone)]
pub enum TirExprKind {
    IntLiteral {
        value: u64,
        repr: String,
    },
    FloatLiteral {
        value: f64,
        repr: String,
    },
    BoolLiteral(bool),
    CharLiteral(char),
    StringLiteral(String),
    /// Byte array literal from `#include_bytes`. Lowered to `List<u8>` via data segment.
    BytesLiteral(Vec<u8>),
    Null,
    Unit,

    Local {
        index: u32,
        name: String,
    },
    FuncRef {
        module_source: ModuleSource,
        name: String,
        /// Type arguments pinned for a generic function reference (either via
        /// turbofish `identity::<i32>` or inferred from an expected `fn(...)`
        /// type at the use site). Empty for non-generic function references.
        /// Consumed by the monomorphizer, which queues the corresponding
        /// instantiation and rewrites `name` to the mangled form.
        type_args: Vec<TypeId>,
        /// The function declaration referenced, as [`FunctionRef::template`].
        template: Option<TemplateId>,
    },
    /// Read a global variable
    GlobalVarGet {
        module_source: ModuleSource,
        name: String,
    },
    /// Write to a global variable
    GlobalVarSet {
        module_source: ModuleSource,
        name: String,
        value: Box<TirExpr>,
    },

    Binary {
        left: Box<TirExpr>,
        op: TirBinaryOp,
        right: Box<TirExpr>,
    },
    Unary {
        op: TirUnaryOp,
        expr: Box<TirExpr>,
    },
    Assign {
        target: Box<TirExpr>,
        value: Box<TirExpr>,
    },
    Cast {
        expr: Box<TirExpr>,
        target_type: TypeId,
    },
    /// The affine value of the place `expr` names, moved out of it: the place
    /// is empty afterwards (WEP: Affine Types). Reify marks the places
    /// `ownership::Modes::moves` names, and synthesis marks those it moves
    /// itself. Drop elaboration takes ownership here and nowhere else, then
    /// removes the node.
    Move {
        expr: Box<TirExpr>,
    },

    /// Every call: free function (`foo(args)`), static method
    /// (`Type::method(args)`), and instance method (`recv.method(args)`), which
    /// all map to the same `WirInstr::Call` and share identical semantics.
    Call {
        /// Boxed: unboxed, it dominates the enum by ~290 bytes (clippy's
        /// `large_enum_variant`).
        func: Box<FunctionRef>,
        /// Explicit type arguments for generic functions: `identity::<i32>(x)`
        type_args: Vec<TypeId>,
        /// Arguments in the callee's parameter order, `args[i]` for
        /// `params[i]` in every call shape. Only `TirExprKind::method_call`
        /// builds one with a receiver, so a trait-qualified (UFCS) call's
        /// already reference-typed first argument needs none of the treatment a
        /// receiver gets, notably `lower`'s never-value-copy-a-receiver rule.
        args: CallArgs<CallArg>,
    },
    /// Raw Component Model call to a lowered WASI import or a canonical built-in.
    ///
    /// Used inside synthesized CM binding functions to call the flat-ABI function
    /// directly, bypassing the normal effect call mechanism. Args are already lowered
    /// to flat CM types (i32, i64, f32, f64).
    CmRawCall {
        /// Which function this calls, by identity rather than by rendered name.
        target: CmCallTarget,
        /// Flat ABI arguments (already lowered to core Wasm types)
        args: Vec<TirExpr>,
    },

    FieldAccess {
        expr: Box<TirExpr>,
        field_index: u32,
        field_name: String,
    },
    Index {
        expr: Box<TirExpr>,
        index: Box<TirExpr>,
    },

    Block(TirBlock),
    If {
        condition: Box<TirExpr>,
        then_branch: TirBlock,
        else_branch: Option<TirBlock>,
    },
    Match {
        expr: Box<TirExpr>,
        arms: Vec<TirMatchArm>,
    },

    StructLiteral {
        struct_type: TypeId,
        struct_name: String,
        fields: Vec<TirStructField>,
    },
    TupleLiteral {
        elements: Vec<TirExpr>,
    },
    /// `Array<T>` of exactly `elements.len()` slots, the value a `[e0, e1, …]`
    /// literal denotes. Emitted by literal coercion, which then hands it to the
    /// target type's `From<Array<T>>` impl. Lowers to `ExprKind::ArrayLiteral`.
    ArrayLiteral {
        elements: Vec<TirExpr>,
    },

    /// Spread a tuple expression into an enclosing `TupleLiteral`.
    /// Created by the elaborator for `[..expr]` syntax. Expanded by monomorphization
    /// into individual `FieldAccess` elements once the concrete tuple arity is known.
    TupleSpread {
        expr: Box<TirExpr>,
    },

    /// Transpose a tuple-of-tuples: `[a, b].zip()` → `[[a.0, b.0], [a.1, b.1], ...]`.
    /// Created by the elaborator for the `.zip()` pseudo-method on tuples.
    /// Expanded during monomorphization once concrete tuple arities are known.
    TupleZip {
        expr: Box<TirExpr>,
    },

    /// Compile-time arity of a tuple whose type still contains a type pack
    /// (`[..T].len()`). For a fully concrete tuple the elaborator folds `.len()`
    /// to an integer literal immediately; when a `..T` pack is present the arity
    /// is unknown until monomorphization, so it is deferred here and expanded to
    /// an `IntLiteral` once the concrete arity is known. The `expr` is evaluated
    /// only for its type (tuples are value types with no side effects in `len`).
    TupleLen {
        expr: Box<TirExpr>,
    },

    /// Type pack expansion: `[..T::method()]` inside a `TupleLiteral`.
    /// Expands at monomorphization to one call per concrete type in the pack:
    /// `[T_0::method(), T_1::method(), ...]`.
    ///
    /// The `call_expr` is a resolved Call whose receiver/return type references
    /// the `TypePack`. During monomorphization, the pack type is substituted
    /// with each concrete element type to produce individual calls.
    TypePackExpansion {
        /// The call expression template (resolved with `TypePack` type)
        call_expr: Box<TirExpr>,
        /// The `TypePack` type ID (index into type table, pre-substitution)
        pack_type_id: TypeId,
        /// The tuple the pack stands for, on a node whose own site settled it:
        /// a parameter default spliced into a caller nothing instantiates.
        /// `None` where the enclosing function's instantiation settles it.
        settled_pack: Option<TypeId>,
    },

    /// Deferred `[for let v of tuple { expr }]` over a pack-typed tuple.
    ///
    /// The body is resolved once with the binding typed as the pack element;
    /// the monomorphizer unrolls it into a tuple literal once the pack is
    /// concrete. See `docs/wep-2026-03-14-variadic-type-parameters.md`.
    VariadicTupleComprehension {
        /// The source tuple (type contains `TypePack` before substitution)
        iterable: Box<TirExpr>,
        /// The element binding's name and local slot
        binding_name: String,
        binding_local: u32,
        /// Sub-bindings of a destructured binding (`[i, v]`), as
        /// field reads off the binding local
        destructure: Vec<TirStmt>,
        /// The per-element expression
        body: Box<TirExpr>,
        /// Unique ID for generating labels
        unique_id: u32,
        /// Whether the binding is the `[index, value]` pair of `.enumerate()`
        is_enumerate: bool,
        /// The `walk` of the `PackElement` the binding holds
        walk: u32,
    },

    /// Access to a captured variable inside a closure body
    Capture {
        /// Index into the closure's captures array
        index: u32,
        /// Variable name (for debugging)
        name: String,
    },

    Closure {
        params: Vec<(String, TypeId)>,
        body: Box<TirExpr>,
        captures: Vec<TirCapture>,
        /// Optional functor ID assigned during lowering.
        /// Used by monomorphize phase to look up the corresponding `ClosureFunctor`.
        functor_id: Option<u32>,
        /// Closure-scope address-taken locals, captured from the
        /// closure's resolution `FunctionContext`. The boxing pass uses
        /// this when descending into the closure body — the body's
        /// `Local { index: N }` references closure locals, not the
        /// parent function's, so the parent's set would mis-box. Empty
        /// for synthesised closures (e.g. effect-handler dispatch),
        /// which never take addresses.
        address_taken_locals: hashmap::IndexSet<u32>,
        /// Body-level let-bindings inside the closure, in declaration order,
        /// occupying `params.len()..` in its local-index namespace (the params
        /// themselves live in `params`). Captured at resolve time so pattern
        /// lowering can seed a closure-scoped allocator without re-walking the
        /// body. Empty for the synthetic closures `synthesis/` creates.
        body_locals: Vec<TirLocal>,
        /// Effects the closure type was annotated with at the use site (let
        /// annotation, function-typed parameter, etc.). `Some` only when the
        /// annotation provides a concrete effect set; the effect checker swaps
        /// to these when entering the body so e.g. `let f: fn() = ||{println}`
        /// rejects the Stdout leak. `None` means "unannotated, inherit outer
        /// effects" (preserves the original behaviour for free closures).
        declared_effects: Option<Vec<EffectRef>>,
    },

    /// Indirect call through a callable value (closure or funcref)
    IndirectCall {
        /// The callee expression (closure struct or funcref)
        callee: Box<TirExpr>,
        /// Arguments to pass to the callee
        args: Vec<TirExpr>,
    },

    /// Custom variant construction: `Shape::Circle(5.0)` or `MyVariant::Unit`
    VariantConstruct {
        /// The variant type (e.g., `ResolvedType::Variant` { name: "Shape", ... })
        variant_type: TypeId,
        /// The case index (0-based position in variant declaration)
        case_index: u32,
        /// The case name (for debugging/error messages)
        case_name: String,
        /// Payload value (None for unit variants constructed without explicit payload)
        payload: Option<Box<TirExpr>>,
    },

    /// Enum construction: `Color::Red`
    /// Enums have no payload, just a discriminant value.
    EnumConstruct {
        /// The enum type (e.g., `ResolvedType::Enum` { name: "Color", ... })
        enum_type: TypeId,
        /// The case index (0-based position in enum declaration)
        case_index: u32,
        /// The case name (for debugging/error messages)
        case_name: String,
    },

    /// Labeled block expression: `label: { ... }` that produces a value
    /// The value must be returned via `break label: expr;`
    LabeledBlock {
        label: String,
        block: TirBlock,
        /// The type of value this block produces (from break expressions)
        result_type: TypeId,
    },

    /// Get the discriminant (tag) of a variant value.
    /// Generated from match expressions on variants.
    /// Result type is i32.
    VariantTag {
        expr: Box<TirExpr>,
    },

    /// Test if a variant value is of a specific case.
    /// Generated from if-let patterns on custom variants.
    /// For unit variants: checks discriminator == `case_index`
    /// For payload variants: uses ref.test on the case type
    /// Result type is bool.
    VariantTest {
        expr: Box<TirExpr>,
        /// The case index to test for
        case_index: u32,
        /// The case name (for error messages)
        case_name: String,
    },

    /// Extract the payload from a variant value at a specific case index.
    /// Generated from match expressions on variants.
    VariantPayload {
        expr: Box<TirExpr>,
        /// The case index to extract payload from
        case_index: u32,
        /// The payload type for this case
        payload_type: TypeId,
    },

    /// A call argument converted to the `&ListView<T>` parameter it meets
    /// (the node's type) from a `ListArg<T>`. The synthesis phase
    /// (pre-monomorphize) expands it to `&ListArg::lowered(&value)`.
    ListViewOf {
        value: Box<TirExpr>,
    },

    /// Effect handler installation: `with E1 => h1, ... do { body }`.
    /// See `docs/wep-2026-04-11-effect-handler.md`.
    ///
    /// Each binding installs a handler for one effect for the duration of `body`.
    /// The block evaluates to `body`'s value; in MVP `result_type` is always Unit
    /// because do-block bodies are statement blocks, not expression blocks.
    WithHandler {
        bindings: Vec<TirHandlerBinding>,
        body: TirBlock,
        result_type: TypeId,
    },
}

/// One `Effect => handler` binding inside a `with ... do` block.
#[derive(Debug, Clone)]
pub struct TirHandlerBinding {
    /// The effect (or resource) being handled. The bundled `with &mut h do`
    /// form is expanded to one binding per implemented effect.
    pub effect: DeclaredHead,
    /// Concrete `TypeId`s of the trait / resource type arguments at this
    /// installation site (e.g. `[u8]` for `with Stream<u8> => &mut s do`,
    /// or as derived from the impl block in a bundled `with &mut s do`
    /// where `MockCM` implements `Stream<u8>`). Empty for non-generic
    /// effects / resources. The dispatch synthesis projects this together
    /// with `effect` into the `InstantiationKey` it uses to look up the
    /// per-monomorphisation dispatch infrastructure.
    pub trait_type_args: Vec<TypeId>,
    /// Handler value expression (e.g., `&mut mock`).
    pub handler: TirExpr,
    /// The concrete struct type (after deref) implementing the effect.
    /// Used by codegen to pick the correct `impl E for T` methods.
    pub handler_type: TypeId,
    pub span: Span,
    /// `Some(id)` marks a binding from a bundled `with &mut h do` expansion; all
    /// bindings from one clause share the id, so dispatch synthesis can allocate
    /// a single `$h_<bundle>` local that every per-effect closure captures —
    /// the handler is evaluated once and one effect's mutations are seen by the
    /// rest. `None` for an explicit `Effect => handler`. Unique per `WithHandler`.
    pub bundle_group: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TirBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    /// Whether two values differ bit for bit: a reference by identity, a number
    /// by its bits, so `-0.0` differs from `0.0`. Lowering mints it; no source
    /// operator spells it.
    NotIdentical,
}

impl TirBinaryOp {
    /// Whether this is `&&` or `||`, whose right operand may not run.
    #[must_use]
    pub fn is_short_circuit(self) -> bool {
        matches!(self, Self::And | Self::Or)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TirUnaryOp {
    Neg,
    Not,
    BitNot,
    Ref,
    MutRef,
    Deref,
}

#[derive(Debug, Clone)]
pub struct TirMatchArm {
    pub pattern: TirPattern,
    /// Optional guard expression (the condition after `&&`)
    pub guard: Option<TirExpr>,
    pub body: TirExpr,
    pub span: Span,
    /// Whether entering the arm consumes the scrutinee. Only the path a
    /// refutable `let` takes when its pattern fails to match does not: it
    /// leaves the scrutinee to its owner (WEP: Affine Types).
    pub consumes: bool,
}

#[derive(Debug, Clone)]
pub enum TirPattern {
    Wildcard,
    Binding {
        name: String,
        local_index: u32,
        type_id: TypeId,
    },
    Literal(TirLiteralPattern),
    Tuple(Vec<TirPattern>, /* has_rest */ bool),
    Variant {
        enum_type: TypeId,
        variant_name: String,
        /// Which case of `enum_type` this matches. Resolved where the pattern is
        /// elaborated, so no consumer re-derives it from `variant_name`.
        case_index: u32,
        bindings: Vec<TirPattern>,
        /// Payload type for the matched variant case (unit for no-payload cases)
        payload_type: TypeId,
    },
    /// Enum case pattern (enums are simple i32 discriminants with no payload)
    Enum {
        enum_type: TypeId,
        case_name: String,
        case_index: u32,
    },
    /// Struct destructuring pattern: `{ x, y }` or `Point { x, y }`
    Struct {
        struct_type: TypeId,
        fields: Vec<TirStructPatternField>,
        has_rest: bool,
    },
    /// Or pattern: matches if any alternative matches
    Or(Vec<TirPattern>),
    /// Constant value pattern: compares scrutinee against a constant expression
    /// (immutable global variable or associated constant like `i32::MAX`)
    ConstantValue {
        expr: Box<TirExpr>,
    },
    /// Range pattern: `0..<10` or `'a'..='z'`
    Range {
        start: i128,
        end: i128,
        inclusive: bool,
        is_unsigned: bool,
    },
    /// Holds the scrutinee in `local_index` at `type_id`, matching where `test` on
    /// it holds: a host type check, a constant's `Eq`, or a range compared by
    /// its bounds. `name` is what it binds.
    Narrow {
        name: Option<String>,
        local_index: u32,
        type_id: TypeId,
        test: Box<TirExpr>,
        /// The pattern as written, which a closure's source text shows in
        /// place of the test the compiler built.
        shown: String,
    },
    /// A literal or range pattern on a scrutinee whose type is still a type
    /// parameter. What it names depends on the instance, so monomorphization
    /// settles `scrutinee_type` and `instance_patterns` judges and lowers it.
    PerInstance {
        pattern: InstancePattern,
        scrutinee_type: TypeId,
        span: Span,
    },
}

/// What a [`TirPattern::PerInstance`] matches.
#[derive(Debug, Clone)]
pub enum InstancePattern {
    Literal(PatternLiteral),
    /// The one float a decimal literal or a float type's limit names.
    Float(FloatBound),
    /// Integer, `char` or float, as the scrutinee's type decides.
    Range {
        start: RangeBound,
        end: RangeBound,
        inclusive: bool,
    },
}

/// What a range-pattern bound names: an integer or `char` literal, which a
/// float reads by value, or a float, whose value turns on the float type that
/// reads it.
#[derive(Debug, Clone)]
pub enum RangeBound {
    Discrete(PatternLiteral),
    Float(FloatBound),
}

/// A float pattern or range bound, before a float type rounds it.
#[derive(Debug, Clone)]
pub struct FloatBound {
    /// What the bound is written as.
    pub kind: FloatBoundKind,
    /// How a diagnostic writes it: `-1.5`, `f64::INFINITY`.
    pub shown: String,
}

/// A float pattern or range bound as written: a literal or a float type's
/// limit.
#[derive(Debug, Clone)]
pub enum FloatBoundKind {
    /// The unsigned literal `digits`, negated where `negated`.
    Literal { digits: String, negated: bool },
    /// The limit `name` of the float type `owner`, as `f64::INFINITY`.
    Limit { owner: PrimitiveType, name: String },
}

/// The value a literal pattern or range bound names, before a scrutinee type
/// reads it.
#[derive(Debug, Clone)]
pub enum PatternLiteral {
    /// `shown` is how a diagnostic writes it: `-5`, `b'a'`, `i32::MAX`. A
    /// limit such as `i32::MAX` keeps its `owner` type; a literal has none and
    /// takes the scrutinee's.
    Int {
        magnitude: u128,
        negated: bool,
        owner: Option<PrimitiveType>,
        shown: String,
    },
    Char(char),
    Bool(bool),
}

impl PatternLiteral {
    /// The 128 bits a range bound compares by. An unsigned scrutinee reads
    /// them as a `u128`.
    pub fn bits(&self) -> i128 {
        match self {
            PatternLiteral::Int {
                magnitude, negated, ..
            } => {
                let bits = magnitude.cast_signed();
                if *negated { bits.wrapping_neg() } else { bits }
            }
            PatternLiteral::Char(c) => i128::from(u32::from(*c)),
            PatternLiteral::Bool(_) => unreachable!("a range bound is never a `bool`"),
        }
    }

    /// This literal as a pattern on a scrutinee, unsigned where `is_unsigned`.
    pub fn to_tir(&self, is_unsigned: bool) -> TirLiteralPattern {
        match self {
            PatternLiteral::Int { .. } if is_unsigned => {
                TirLiteralPattern::U128(self.bits().cast_unsigned())
            }
            PatternLiteral::Int { .. } => TirLiteralPattern::I128(self.bits()),
            PatternLiteral::Char(c) => TirLiteralPattern::Char(*c),
            PatternLiteral::Bool(b) => TirLiteralPattern::Bool(*b),
        }
    }
}

impl TirPattern {
    /// The local this pattern itself declares, with its type: a binding's, or
    /// the one a narrowing holds the scrutinee in. Sub-patterns are not asked.
    pub fn declared_local(&self) -> Option<(u32, TypeId)> {
        match self {
            TirPattern::Binding {
                local_index,
                type_id,
                ..
            }
            | TirPattern::Narrow {
                local_index,
                type_id,
                ..
            } => Some((*local_index, *type_id)),
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Tuple(..)
            | TirPattern::Variant { .. }
            | TirPattern::Enum { .. }
            | TirPattern::Struct { .. }
            | TirPattern::Or(_)
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. }
            | TirPattern::PerInstance { .. } => None,
        }
    }

    /// The index of the local [`Self::declared_local`] names, to renumber it.
    pub fn declared_local_index_mut(&mut self) -> Option<&mut u32> {
        match self {
            TirPattern::Binding { local_index, .. } | TirPattern::Narrow { local_index, .. } => {
                Some(local_index)
            }
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Tuple(..)
            | TirPattern::Variant { .. }
            | TirPattern::Enum { .. }
            | TirPattern::Struct { .. }
            | TirPattern::Or(_)
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. }
            | TirPattern::PerInstance { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TirStructPatternField {
    pub field_name: String,
    pub field_index: u32,
    pub pattern: TirPattern,
}

#[derive(Debug, Clone)]
pub enum TirLiteralPattern {
    /// Signed integer literal (covers i8, i16, i32, i64, i128)
    I128(i128),
    /// Unsigned integer literal (covers u8, u16, u32, u64, u128)
    U128(u128),
    Bool(bool),
    Char(char),
    String(String),
    Null,
}

#[derive(Debug, Clone)]
pub struct TirStructField {
    pub name: String,
    pub value: TirExpr,
    pub field_index: u32,
}

/// Where the frame constructing a closure finds a captured binding: a local it
/// owns, or a slot of its own environment when it is a closure itself. See
/// [WEP: Closure Implementation Internals](../../docs/wep-2026-01-25-closure-implementation-internals.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// A local of the constructing frame.
    Local(u32),
    /// The constructing frame's own capture slot.
    Capture(u32),
}

impl CaptureSource {
    /// The constructing frame's local index, or `None` when the binding is
    /// reached through that frame's own environment.
    pub fn local(self) -> Option<u32> {
        match self {
            Self::Local(index) => Some(index),
            Self::Capture(_) => None,
        }
    }

    /// Apply `f` to a local index, leaving a capture slot alone.
    pub fn map_local(self, f: impl FnOnce(u32) -> u32) -> Self {
        match self {
            Self::Local(index) => Self::Local(f(index)),
            Self::Capture(_) => self,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TirCapture {
    pub name: String,
    pub source: CaptureSource,
    pub type_id: TypeId,
}

/// The constructing frame's locals `captures` reads, in order. A slot read off
/// that frame's own environment names no local of it, so it yields nothing here.
pub fn capture_source_locals(captures: &[TirCapture]) -> impl Iterator<Item = u32> + '_ {
    captures.iter().filter_map(|c| c.source.local())
}

/// The environment entry a `TirExprKind::Capture` names. A body only indexes the
/// environment of the closure it belongs to, so `slot` is always one of these.
pub fn capture_at(captures: &[TirCapture], slot: u32, span: Span) -> &TirCapture {
    let Some(capture) = captures.get(slot as usize) else {
        unreachable!(
            "at {}: slot {slot} is read, but this closure's environment has {}",
            span.location(),
            captures.len()
        )
    };
    capture
}

#[derive(Debug, Clone)]
pub struct TirBlock {
    pub stmts: Vec<TirStmt>,
    pub span: Span,
}

impl TirBlock {
    pub fn new(stmts: Vec<TirStmt>, span: Span) -> Self {
        Self { stmts, span }
    }

    pub fn empty(span: Span) -> Self {
        Self {
            stmts: Vec::new(),
            span,
        }
    }

    /// The expression the block evaluates to, where its last statement is one.
    pub fn tail_expr(&self) -> Option<&TirExpr> {
        match &self.stmts.last()?.kind {
            TirStmtKind::Expr(expr) => Some(expr),
            _ => None,
        }
    }
}

/// `target = value;`
pub fn assign_stmt(target: TirExpr, value: TirExpr, span: Span) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value),
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    )
}

/// `receiver.zip()` as a tuple literal: one column per position, each reading
/// its cell out of every row. A `&`/`&mut` receiver transposes what it refers to.
pub fn transpose_tuple_expr(receiver: &TirExpr, span: Span, type_table: &mut TypeTable) -> TirExpr {
    assert!(
        matches!(receiver.kind, TirExprKind::Local { .. }),
        "every cell reads the receiver, so reify binds it before either expansion"
    );
    let tuple = type_table.peel_refs(receiver.type_id);
    let by_value = tuple == receiver.type_id;
    let transposed = type_table
        .transposed_tuple(tuple)
        .expect("method lookup admits `zip` only over rows that transpose");
    let rows: Vec<(TypeId, Vec<TypeId>)> = type_table
        .as_tuple(tuple)
        .expect("a transpose has rows")
        .into_iter()
        .map(|row| {
            let cells = type_table
                .as_tuple(row)
                .expect("a transposed row is a tuple");
            (row, cells)
        })
        .collect();
    let column_types = type_table
        .as_tuple(transposed)
        .expect("a transpose is a tuple");
    let columns: Vec<TirExpr> = column_types
        .into_iter()
        .enumerate()
        .map(|(col, column_type)| {
            let elements: Vec<TirExpr> = rows
                .iter()
                .enumerate()
                .map(|(row, (row_type, cells))| {
                    let row_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(receiver.clone()),
                            field_index: row as u32,
                            field_name: row.to_string(),
                        },
                        *row_type,
                        span,
                    );
                    let cell = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(row_access),
                            field_index: col as u32,
                            field_name: col.to_string(),
                        },
                        cells[col],
                        span,
                    );
                    // A receiver held by value hands each cell on, once.
                    if by_value { cell.moved() } else { cell }
                })
                .collect();
            TirExpr::new(TirExprKind::TupleLiteral { elements }, column_type, span)
        })
        .collect();
    TirExpr::new(
        TirExprKind::TupleLiteral { elements: columns },
        transposed,
        span,
    )
}

/// Value-yielding type of a block: the last statement decides, except that a
/// two-branch `If` needs its branches to agree (or one to be `Never`)
/// and falls back to `Unit`, and a diverging `Return` / `Break` / `Continue`
/// yields `Never`. The elaborator enforces the agreement rule while typing the
/// surrounding expression, so a mismatch here is already reported.
pub fn block_result_type(tt: &TypeTable, block: &TirBlock) -> TypeId {
    block
        .stmts
        .last()
        .and_then(|s| match &s.kind {
            TirStmtKind::Expr(e) => Some(e.type_id),
            TirStmtKind::If {
                then_block,
                else_block: Some(else_block),
                ..
            } => agree_branch_types(
                tt,
                block_result_type(tt, then_block),
                block_result_type(tt, else_block),
            ),
            TirStmtKind::Return { .. } | TirStmtKind::Break { .. } | TirStmtKind::Continue => {
                Some(TypeTable::NEVER)
            }
            _ => None,
        })
        .unwrap_or(TypeTable::UNIT)
}

/// Combine two branch result types under the elaborator's rule:
/// equal types agree; a `Never` branch defers to the other; two
/// `extends`-related resources agree on the ancestor; an outright
/// mismatch yields `None` so the caller falls back to `Unit`.
pub(crate) fn agree_branch_types(tt: &TypeTable, t: TypeId, e: TypeId) -> Option<TypeId> {
    if t == e {
        Some(t)
    } else if t == TypeTable::NEVER {
        Some(e)
    } else if e == TypeTable::NEVER {
        Some(t)
    } else {
        tt.resource_join(t, e)
    }
}

/// Whose storage a `let` binding holds, which decides whether it copies its
/// value and whether the value-copy analysis may hand that storage on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LetStorage {
    /// Its own: the value-copy analysis copies the value unless it proves the
    /// value owned.
    Planned,
    /// Its own, taken over from the value without a copy. The producer vouches
    /// that nothing reads that storage as another value afterwards.
    Taken,
    /// The value's, without a copy: the binding shares whatever storage the
    /// value names, and the value-copy analysis follows that share as it
    /// follows a planned binding's source.
    Aliased,
}

impl LetStorage {
    /// Whether the binding holds its value without copying it.
    pub fn skips_copy(self) -> bool {
        match self {
            LetStorage::Planned => false,
            LetStorage::Taken | LetStorage::Aliased => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TirStmt {
    pub kind: TirStmtKind,
    pub span: Span,
}

impl TirStmt {
    pub fn new(kind: TirStmtKind, span: Span) -> Self {
        Self { kind, span }
    }
}

#[derive(Debug, Clone)]
pub enum TirStmtKind {
    Let {
        name: String,
        local_index: u32,
        is_mut: bool,
        type_id: TypeId,
        value: TirExpr,
        /// Whose storage the binding holds.
        storage: LetStorage,
    },
    Expr(TirExpr),
    Return {
        value: Option<TirExpr>,
    },
    /// `task return expr;` — delivers the async task result without terminating the function.
    /// Eliminated by `synthesis::cm_binding` before lower/optimize phases.
    TaskReturn {
        value: TirExpr,
    },
    If {
        condition: TirExpr,
        then_block: TirBlock,
        else_block: Option<TirBlock>,
    },
    /// Canonical loop: `loop { ... }` - infinite loop exited via break
    Loop {
        body: TirBlock,
    },
    /// Break statement: `break;`, `break label;`, or `break label: expr;`
    Break {
        /// Optional label to break to (for labeled blocks)
        label: Option<String>,
        /// Optional value to return from the labeled block
        value: Option<TirExpr>,
    },
    Continue,
    /// Labeled block: `LABEL: { ... }` - creates a new scope with local bindings
    LabeledBlock {
        label: String,
        block: TirBlock,
    },
    /// Tuple destructuring let statement: `let [a, b] = tuple_expr;`
    LetDestructure {
        /// The pattern to bind (e.g., [a, b, c] or [x, [y, z]])
        pattern: TirPattern,
        /// The value expression (must be a tuple)
        value: TirExpr,
    },
    /// Deferred tuple for-of expansion for variadic type packs.
    ///
    /// Created when `for let v of iterable` where `iterable` has a tuple type containing
    /// `TypePack` elements. The monomorphizer expands this after type substitution resolves
    /// the `TypePack` to a concrete tuple.
    VariadicForOf {
        /// The tuple iterable expression (type contains `TypePack` before substitution)
        iterable: TirExpr,
        /// The loop variable name
        binding_name: String,
        /// Local index for the loop variable
        binding_local: u32,
        /// Whether the binding is mutable
        is_mut: bool,
        /// The body to execute for each element (resolved with TypePack-typed binding)
        body: TirBlock,
        /// Unique ID for generating labels
        unique_id: u32,
        /// When the iterable is `&[..T]` (a reference to a variadic tuple),
        /// each element is bound by reference (`&T_k`), matching the
        /// `for v of &list` refiter semantics. The binding is resolved with
        /// type `&TypePack`; expansion wraps each element field in `&`.
        by_ref: bool,
        /// When the iterable is `tuple.enumerate()`, the binding is the pair
        /// `[i32, T_k]`; expansion pairs each element with its index literal.
        is_enumerate: bool,
        /// The `walk` of the `PackElement` the binding holds
        walk: u32,
        /// Whether the element is affine, so each unrolled binding moves its
        /// field out of the tuple. Only reify knows the body's `affine`
        /// parameters.
        moves: bool,
    },
}

/// Generic type parameter in TIR (from AST `GenericParam`)
#[derive(Debug, Clone)]
pub struct TirTypeParam {
    pub name: String,
    /// Whether this is an effect parameter (`effect E`)
    pub is_effect: bool,
    /// Whether this is a type pack parameter (`..T`)
    pub is_pack: bool,
    pub bounds: Vec<String>,
    /// Default type if specified (e.g., `Effects = []`)
    pub default: Option<TypeId>,
    pub index: u32,
    /// Whether a blanket impl's bound projects it (`..F` in
    /// `impl<T: ReflectStruct<FieldTypes = [..F]>, ..F>`). The instance key
    /// carries the projection's answer at the parameter's own slot, a pack
    /// as one tuple (`blanket_impl_args`).
    pub projected: bool,
}

/// What a use site knows about the associated types projected from a slot,
/// beyond the type filling it: slot index → the answers it gives.
///
/// The companion of the slot substitution in
/// [`TypeTable::substitute_type_params_with`]. A declaration resolves
/// `Self::Item` in its own frame, where it can only be a projection; what it
/// stands for is written at the use site (`I: IntoIterator<Item = u8>`).
pub type SlotProjections = IndexMap<u32, Vec<ProjectionAnswer>>;

/// What a use site says one projection off a slot is.
#[derive(Debug, Clone)]
pub struct ProjectionAnswer {
    /// The trait declaring the associated type.
    pub owning_trait: DefId,
    /// The arguments the trait is reached at, where the bound answering wrote
    /// them: `H: Holder<i32, Out = A> + Holder<String, Out = B>` answers
    /// `Out` twice. `None` answers it at any.
    pub trait_args: Option<Vec<TypeId>>,
    /// The associated type's name.
    pub assoc_name: String,
    /// What it is at the use site.
    pub answer: TypeId,
}

impl ProjectionAnswer {
    /// `<_ as owning_trait>::assoc_name` is `answer`, at whatever arguments.
    #[must_use]
    pub fn at_any(owning_trait: DefId, assoc_name: String, answer: TypeId) -> Self {
        Self {
            owning_trait,
            trait_args: None,
            assoc_name,
            answer,
        }
    }
}

/// Effect parameters and variables, by name, and the row each stands for.
pub type EffectRows = IndexMap<String, Vec<EffectRef>>;

/// A function value with row `from` moved into a slot whose function type has
/// row `into`: what the value performs must be what the slot admits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RowFlow {
    pub from: Vec<EffectRef>,
    pub into: Vec<EffectRef>,
    pub span: Span,
    /// The source path of the module the move is written in.
    pub module: String,
}

/// `effects` with each variable `rows` solves replaced by its solution, and each
/// variable in that solution in turn, each effect once. `read` sees every
/// variable looked up; one with no solution stays as it is.
pub fn solve_row(
    effects: &[EffectRef],
    rows: &EffectRows,
    read: &mut impl FnMut(&str),
) -> Vec<EffectRef> {
    fn expand(
        effects: &[EffectRef],
        rows: &EffectRows,
        read: &mut impl FnMut(&str),
        expanded: &mut IndexSet<String>,
        out: &mut Vec<EffectRef>,
    ) {
        for effect in effects {
            if let EffectRef::Var { name, .. } = effect {
                read(name);
                if let Some(solution) = rows.get(name) {
                    if expanded.insert(name.clone()) {
                        expand(solution, rows, read, expanded, out);
                    }
                    continue;
                }
            }
            if !out.contains(effect) {
                out.push(effect.clone());
            }
        }
    }
    let mut out = Vec::new();
    expand(effects, rows, read, &mut IndexSet::default(), &mut out);
    out
}

/// Substitution-key base for method-level type params: past the highest
/// impl-param *index*, not the count. A concrete type in a receiver slot
/// (`String` in `impl<V> ... for TreeMap<String, V>`) is not a param, so a later
/// param keeps a sparse index the count would undershoot, colliding a method
/// param onto an impl slot. Elaboration and monomorphization both derive it here.
#[must_use]
pub fn method_param_offset(impl_type_params: &[TirTypeParam]) -> u32 {
    impl_type_params
        .iter()
        .map(|p| p.index + 1)
        .max()
        .unwrap_or(0)
}

/// Information about monomorphization origin for instantiated items
#[derive(Debug, Clone)]
pub struct MonomorphInfo {
    /// Original generic name: `"Box"` for `"Box<i32>"`, or
    /// `"BTreeNode<K,V>::insert"` for methods.
    pub generic_name: String,
    /// Impl-level type arguments (from the struct/type, e.g. `[i32]` for `List<i32>`)
    pub impl_type_args: Vec<TypeId>,
    /// Method-level type arguments (from the method's own generics, e.g. `[String]` for `.transform::<String>()`)
    pub method_type_args: Vec<TypeId>,
    /// Whether this originates from a blanket impl (e.g., `impl<I: Iterator> IntoIterator for I`)
    pub is_blanket: bool,
}

/// A generic function template's identity: what a call selected, and what the
/// monomorphizer instantiates, with no name in between.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TemplateId {
    /// A written declaration, and the impl block its body was emitted into —
    /// a trait's default body is emitted once per impl.
    Declared { def: DefId, block: Option<DefId> },
    /// The `unchecked` twin of the `Declared` template with these fields,
    /// which a call marked `unchecked` reaches.
    Unchecked { def: DefId, block: Option<DefId> },
    /// A body synthesis minted, which declares nothing: the name it gave it in
    /// its module.
    Synthesized { module: ModuleSource, name: String },
    /// An interface's operation. A call of one instantiates no body: dispatch
    /// routes it to a handler.
    Operation { def: DefId },
}

impl TemplateId {
    /// The declaration `def` as emitted into the impl block `block`.
    pub fn in_block(def: DefId, block: DefId) -> Self {
        Self::Declared {
            def,
            block: Some(block),
        }
    }

    /// The impl block a written declaration's body is emitted into, its
    /// `unchecked` twin's included.
    pub fn block(&self) -> Option<DefId> {
        match self {
            Self::Declared { block, .. } | Self::Unchecked { block, .. } => *block,
            Self::Synthesized { .. } | Self::Operation { .. } => None,
        }
    }

    /// The written declaration whose body answers, its `unchecked` twin's
    /// included; `None` for a synthesized body and for an operation.
    pub fn def(&self) -> Option<DefId> {
        match self {
            Self::Declared { def, .. } | Self::Unchecked { def, .. } => Some(*def),
            Self::Synthesized { .. } | Self::Operation { .. } => None,
        }
    }

    /// The operation a call of an interface reaches.
    pub fn operation(&self) -> Option<DefId> {
        match self {
            Self::Operation { def } => Some(*def),
            Self::Declared { .. } | Self::Unchecked { .. } | Self::Synthesized { .. } => None,
        }
    }

    /// The module the body is emitted into: its block's where it has one.
    pub fn home(&self, defs: &DefTable) -> ModuleSource {
        match self {
            Self::Declared { def, block } | Self::Unchecked { def, block } => {
                defs.module(block.unwrap_or(*def)).clone()
            }
            Self::Synthesized { module, .. } => module.clone(),
            Self::Operation { .. } => panic!("an operation emits no body: {self:?}"),
        }
    }

    /// The template of this one's `unchecked` twin.
    pub fn unchecked_twin(&self) -> Self {
        match self {
            Self::Declared { def, block } => Self::Unchecked {
                def: *def,
                block: *block,
            },
            Self::Unchecked { .. } | Self::Synthesized { .. } | Self::Operation { .. } => {
                panic!("only a declaration has an unchecked twin: {self:?}")
            }
        }
    }

    /// The declaration whose `unchecked` twin this one is, if it is one.
    pub fn checked(&self) -> Option<Self> {
        match self {
            Self::Unchecked { def, block } => Some(Self::Declared {
                def: *def,
                block: *block,
            }),
            Self::Declared { .. } | Self::Synthesized { .. } | Self::Operation { .. } => None,
        }
    }

    /// The template a call of `info` reaches, given that its method answers
    /// with this one: the `unchecked` twin where the call is marked.
    pub fn called_by(self, info: &LocalMethodName) -> Self {
        if info.is_unchecked() {
            self.unchecked_twin()
        } else {
            self
        }
    }

    /// The body derivation mints in `module` for the trait method `info`,
    /// named by its receiver's head alone.
    pub fn derived(module: ModuleSource, info: &LocalMethodName) -> Self {
        Self::Synthesized {
            module,
            name: LocalMethodName::new(
                info.fq_base_struct_name(),
                info.trait_name.clone(),
                info.method_name.clone(),
            )
            .to_mangled_name(),
        }
    }
}

/// The value a method call's receiver argument delivers, past the auto-`&` /
/// `&mut` the elaborator takes of it. Every question about the receiver is about
/// this value; the reference is only how the callee reaches it.
#[must_use]
pub fn receiver_value(receiver: &TirExpr) -> &TirExpr {
    match &receiver.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: value,
        } => value,
        _ => receiver,
    }
}

/// A `#[param]` compile-time parameter declared on a `global`.
///
/// Carried from the body walk (which validates the attribute shape) to the
/// param-resolution pass (which resolves an override and rewrites the
/// initializer). See `wep-2026-04-26-compile-time-params.md`.
#[derive(Debug, Clone)]
pub struct ParamSpec {
    /// Parameter name matched against `-D NAME=value`. Defaults to the
    /// global's identifier; overridden by `#[param(name = "...")]`.
    pub name: String,
    /// Environment variable read at compile time, from
    /// `#[param(from_env = "...")]`. Independent of `name`.
    pub from_env: Option<String>,
}

/// How a global's storage gets its value.
///
/// One choice rather than a flag beside an initializer field, so a placeholder
/// can never be read as the declared value — it is a plausible constant, and
/// mistaking it folds every read of `global A: i32 = 1 + 2` to zero.
///
/// See [Global Variables](../../docs/wep-2026-01-27-global-variables.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalInit<E> {
    /// The storage holds the declared value, as a constant expression.
    Direct(E),
    /// The storage starts at this placeholder; the module's initialization
    /// function assigns the declared value before anything else runs.
    Deferred(E),
}

impl<E> GlobalInit<E> {
    /// What the storage is initialized with: the declared value, or the
    /// placeholder standing in for it.
    pub fn slot_expr(&self) -> &E {
        match self {
            Self::Direct(e) | Self::Deferred(e) => e,
        }
    }

    pub fn slot_expr_mut(&mut self) -> &mut E {
        match self {
            Self::Direct(e) | Self::Deferred(e) => e,
        }
    }

    /// The declared value, or `None` when it is assigned elsewhere. Anything
    /// asking what a global holds goes through this.
    pub fn declared(&self) -> Option<&E> {
        match self {
            Self::Direct(e) => Some(e),
            Self::Deferred(_) => None,
        }
    }

    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred(_))
    }
}

/// Whether the Wasm slot can hold this value directly. Under-approximates: the
/// classifier on the lowered Wasm value promotes back what this defers.
#[must_use]
pub fn is_constant_initializer(expr: &TirExpr, type_table: &TypeTable) -> bool {
    match &expr.kind {
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Unit
        | TirExprKind::Null => true,
        TirExprKind::Cast { expr: inner, .. } => is_constant_initializer(inner, type_table),
        TirExprKind::Unary { op, expr: inner } => {
            matches!(op, TirUnaryOp::Neg) && is_constant_initializer(inner, type_table)
        }
        TirExprKind::Binary { op, left, right } => {
            matches!(op, TirBinaryOp::Add | TirBinaryOp::Sub | TirBinaryOp::Mul)
                && is_wasm_width_int(expr.type_id, type_table)
                && is_constant_initializer(left, type_table)
                && is_constant_initializer(right, type_table)
        }
        _ => false,
    }
}

/// An integer whose Wado width matches the Wasm operand it lowers to, so
/// wrapping needs no masking. Wasm folds `add` / `sub` / `mul` on these alone.
fn is_wasm_width_int(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Primitive(
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::I64 | PrimitiveType::U64
        )
    )
}

/// The placeholder a deferred global's slot holds until its initialization
/// function assigns the declared value.
#[must_use]
pub fn placeholder_value(type_id: TypeId, type_table: &TypeTable, span: Span) -> TirExpr {
    let kind = match type_table.get(type_id) {
        ResolvedType::Primitive(PrimitiveType::F32 | PrimitiveType::F64) => {
            TirExprKind::FloatLiteral {
                value: 0.0,
                repr: "0.0".to_string(),
            }
        }
        ResolvedType::Primitive(PrimitiveType::Bool) => TirExprKind::BoolLiteral(false),
        ResolvedType::Primitive(PrimitiveType::Char) => TirExprKind::CharLiteral('\0'),
        ResolvedType::Primitive(_) => TirExprKind::IntLiteral {
            value: 0,
            repr: "0".to_string(),
        },
        ResolvedType::Unit => TirExprKind::Unit,
        // String, List, a struct — every reference type starts null.
        _ => TirExprKind::Null,
    };
    TirExpr::new(kind, type_id, span)
}

/// A global initializer function's body: `{ return <value>; }`.
#[must_use]
pub fn initializer_body(value: TirExpr, span: Span) -> TirBlock {
    TirBlock {
        stmts: vec![TirStmt::new(
            TirStmtKind::Return { value: Some(value) },
            span,
        )],
        span,
    }
}

/// Global variable declaration in TIR
#[derive(Debug, Clone)]
pub struct TirGlobal {
    pub name: String,
    pub ty: TypeId,
    pub init: GlobalInit<TirExpr>,
    /// `Some` when the global carries a `#[param]` attribute. Drives the
    /// param-resolution pass; `None` for ordinary globals.
    pub param: Option<ParamSpec>,
    /// Whether the program may assign to this global — `global mut`. The Wasm
    /// slot's mutability is wider and derived when the module is built.
    pub wado_mutable: bool,
    pub visibility: Visibility,
    /// Module where this global is defined
    pub module_source: ModuleSource,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirFunction {
    pub name: String,
    /// The source declaration reify emitted this from. `None` for anything the
    /// compiler synthesizes, which declares nothing and so identifies nothing.
    pub def_id: Option<DefId>,
    /// Module this function belongs to. Set by the link phase when flattening
    /// per-module TIR into flat lists; before link, the `module_source` is
    /// carried implicitly by the parent `TirModule`.
    pub module_source: ModuleSource,
    pub visibility: Visibility,
    /// Whether this function is exported at the Component Model boundary (world export)
    pub is_export: bool,
    /// Whether this is an async function (`export async fn`).
    /// Async functions use `task return` instead of `return` to deliver results.
    pub is_async: bool,
    /// Generic type parameters (empty for non-generic functions)
    pub type_params: Vec<TirTypeParam>,
    /// Type parameters from the impl block (for methods on generic structs)
    /// e.g., for a method in `impl Counter<T>`, this contains T's info
    pub impl_type_params: Vec<TirTypeParam>,
    /// The `impl` block a method was emitted into, which tells apart two blocks'
    /// like-named templates. `None` for anything else, instances included.
    pub impl_origin: Option<DefId>,
    /// If this function was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    /// Parsed method info for methods (None for free functions)
    /// Contains `struct_name`, `trait_name`, and `method_name` extracted from the function name.
    pub method_info: Option<LocalMethodName>,
    pub params: Vec<TirParam>,
    pub return_type: TypeId,
    /// The result an `async fn` delivers through `task return`. `None` for a
    /// non-async fn and for a synthesized wrapper. The effect checker reads it
    /// to infer signature resources.
    pub task_return_type: Option<TypeId>,
    pub effects: Vec<EffectRef>,
    /// `#[immediate(...)]` — parameters lowered to a Wasm immediate, whose
    /// argument must still be a literal when codegen reads it.
    pub immediates: Vec<String>,
    /// What a bodyless declaration states about its calls with `#[storage]`
    /// and `#[side_effect]`. A function with a body states nothing: the body
    /// is read.
    pub declared: Option<BuiltinFacts<String>>,
    pub body: Option<TirBlock>,
    pub span: Span,
    pub local_count: u32,
    /// Per-local metadata — `name`, `type_id`, `is_mut` — indexed by Wasm
    /// local index. Entries `0..params.len()` shadow the corresponding
    /// `params[i]` (for uniform absolute indexing); body let-bindings and
    /// elaborator/optimizer-allocated temporaries occupy `params.len()..`.
    /// `local_count == locals.len()` post-resolve; passes that grow the
    /// local set must keep the two in sync.
    pub locals: Vec<TirLocal>,
    /// Local indices that have their address taken (&x or &mut x).
    /// For mutable primitives, these locals are stored in box structs.
    pub address_taken_locals: IndexSet<u32>,

    /// Whether this function is a synthesized CM binding (generated by `synthesis::cm_binding`).
    /// The inliner and effect checker both skip CM bindings because they are ABI bridges
    /// between Wado GC types and CM linear memory with special effect semantics.
    pub is_cm_binding: bool,

    /// Whether this function is a synthesised effect-dispatch wrapper
    /// (generated by `synthesis::effect_dispatch`). Effect-operation
    /// call-site rewriting must skip these — their fallback path
    /// directly calls the operation's `$cm_binding__*`, which would loop back
    /// through the wrapper if rewritten.
    pub is_dispatch_wrapper: bool,

    /// Whether this function is a synthesized CM *export* binding (world export wrapper).
    /// When true, the global initializer (`$initialize_modules`) is injected at the start
    /// of this function's body during lowering.
    pub is_cm_export: bool,

    /// Whether this function is marked `#[ambient]`. Ambient functions are implicitly
    /// available to callers without requiring matching `with` clauses — they still carry
    /// interface declarations for documentation / implementation purposes, but the effect
    /// checker does not propagate those requirements to callers.
    pub is_ambient: bool,

    /// Inline hint from `#[inline]`, `#[inline(always)]`, or `#[inline(never)]` attributes.
    pub inline_hint: InlineHint,

    /// The compiler-recognized stdlib role this function fills, if any.
    /// Set from `#[compiler_item("...")]` on the source declaration; see
    /// [`crate::compiler_item::CompilerItem`].
    pub compiler_item: Option<CompilerItem>,

    /// Custom wasm export name from `#[export_name("...")]` attribute.
    pub export_name: Option<String>,

    /// Allocator tag from `#[allocator("...")]` attribute (e.g., `"freelist"`, `"debug"`).
    pub allocator_tag: Option<String>,

    /// Categorizes the function for kind-specific optimizations. Most functions
    /// are `Regular`; synthesis passes set specialized kinds so the TIR
    /// optimizer can apply targeted transformations (e.g. freshness-based
    /// elision for `ValueCopy`).
    pub kind: FunctionKind,

    /// ABI for delivering the function's return value at WIR / Wasm level.
    /// Defaults to [`ReturnAbi::Single`]; an analysis pass sets
    /// [`ReturnAbi::MultiValue`] for tuple- or user-struct-returning
    /// functions whose every call site destructures the result via
    /// `FieldAccess` and whose body's returns produce a fresh
    /// `TupleLiteral` / `StructLiteral`. WIR build then emits a
    /// multi-value Wasm result signature (no heap struct round-trip).
    pub return_abi: ReturnAbi,
}

/// How a function delivers its return value at the Wasm level.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReturnAbi {
    /// Single Wasm return value. The function's TIR `return_type` is taken
    /// as-is; tuple / user-struct types lower to a heap struct ref.
    #[default]
    Single,
    /// Multi-value Wasm return: each tuple element / struct field becomes a
    /// separate Wasm result. Carries the per-element TIR type ids and field
    /// names for WIR-build's signature emission and call-site split-local
    /// generation. The function's TIR `return_type` is unchanged (it remains
    /// the tuple / struct type) — only the WIR-level ABI shifts.
    ///
    /// For tuple returns, `field_names` is `["0", "1", ...]` (matching the
    /// numeric field names tuple structs carry). For user-struct returns,
    /// `field_names` is the struct's fields in declaration order.
    MultiValue {
        /// TIR types of each result, in declaration order.
        result_types: Vec<TypeId>,
        /// Field names matching the source aggregate's declaration order.
        /// Used by WIR build to look up the right split local from a
        /// `FieldAccess` access on a multi-value-bound temp.
        field_names: Vec<String>,
    },
}

/// Semantic category of a `TirFunction`. Carries the type operand so the
/// optimizer can reason about the call without re-deriving it from the
/// signature.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FunctionKind {
    /// Ordinary user-defined or synthesized function.
    #[default]
    Regular,
    /// Synthesized `copy_value` function that deep-copies a value of
    /// `type_id`. Calls to such functions may be elided when the argument is
    /// provably fresh.
    ValueCopy { type_id: TypeId },
    /// The `unchecked` twin of the function `of` declares: its body, with the
    /// `contract` clauses checked only under `-f contract-checks`.
    UncheckedTwin { of: DefId },
    /// `F$contract_report{k}`: reports a failed `contract` clause, then traps.
    ContractReport,
    /// Auto-derived `fn(..)^Inspect::inspect` dispatch stub. The TIR body is
    /// `unreachable()` — enough to register the function and resolve calls to
    /// it — and WIR build supplies the real one, a `call_ref` through
    /// `CanonicalClosure_K`'s vtable slot. `(arity, return_type)` are
    /// structured so nobody parses the mangle.
    FnCanonicalDispatch { arity: usize, return_type: TypeId },
}

/// Inline hint for a function, extracted from `#[inline(...)]` attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InlineHint {
    /// No hint — the optimizer decides based on heuristics.
    #[default]
    Auto,
    /// `#[inline]` — suggest inlining (raises the threshold).
    Hint,
    /// `#[inline(always)]` — always inline regardless of size.
    Always,
    /// `#[inline(never)]` — never inline.
    Never,
}

/// Where a bodyless declaration's result comes from, as its `#[storage]`
/// implies. A body is read for the same fact, so this is the declared half of
/// one notion: the result is either a fresh place or a projection of
/// parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnConvention {
    /// Every returned value is freshly materialized, so a caller may consume
    /// it as a move.
    Owned,
    /// The result names a component of one of these parameters in place, so
    /// it lives as long as that argument's storage does.
    PartOf(Vec<usize>),
}

/// What a call keeps of one parameter past its return, and where that lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainSpec<Param> {
    /// The retained parameter.
    pub source: Param,
    /// What is retained is what `source` points to, not `source`.
    pub elements: bool,
    /// Where what is retained lands.
    pub into: RetainInto<Param>,
}

/// Where a retained reference lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainInto<Param> {
    /// The result, which `holds_args` or `copies_args` builds around it.
    Result,
    /// The `&mut` parameter `stores_args` stores into.
    Param(Param),
    /// Anywhere, the result included: `opaque` says nothing.
    Anywhere,
}

/// How a call reaches the one array element it is an accessor of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArrayElementAccess {
    /// It hands back the element's value.
    Read,
    /// It hands back a `&mut` to the element.
    Write,
}

/// A bodyless declaration's signature, by parameter position, as its facts
/// are read against it.
#[derive(Debug, Clone, Default)]
pub struct DeclarationShape {
    /// How many parameters it takes.
    pub arity: usize,
    /// Positions whose type can carry storage in some instantiation.
    pub storage_params: Vec<usize>,
    /// Positions taken through a reference, which contribute what they point
    /// to rather than themselves.
    pub reference_params: IndexSet<usize>,
    /// Positions taken by `&mut`.
    pub mut_params: IndexSet<usize>,
    /// `#[immediate(...)]` positions.
    pub immediate_params: IndexSet<usize>,
    /// It returns neither `()` nor `!`.
    pub returns_value: bool,
    /// It returns `&mut`.
    pub returns_mut_ref: bool,
    /// The declaration returns `!`, so every call ends in a trap.
    pub returns_never: bool,
}

impl DeclarationShape {
    /// The shape of a declaration taking `params` and returning `ret`.
    pub fn new(
        params: &[ParamShape<'_>],
        ret: ReturnShape,
        immediate_params: IndexSet<usize>,
    ) -> Self {
        fn positions<B: FromIterator<usize>>(
            params: &[ParamShape<'_>],
            keep: impl Fn(&ParamShape<'_>) -> bool,
        ) -> B {
            params
                .iter()
                .enumerate()
                .filter(|(_, p)| keep(p))
                .map(|(pos, _)| pos)
                .collect()
        }
        Self {
            arity: params.len(),
            storage_params: positions(params, |p| p.carries_storage),
            reference_params: positions(params, |p| p.is_reference),
            mut_params: positions(params, |p| p.is_mut_ref),
            immediate_params,
            returns_value: ret.returns_value,
            returns_mut_ref: ret.is_mut_ref,
            returns_never: ret.is_never,
        }
    }
}

/// What a bodyless declaration stated, by parameter position, and what that
/// implies for the questions its readers ask. Link snapshots it because
/// monomorphization drops the generic declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltinDeclaration {
    /// How many parameters the declaration takes.
    pub arity: usize,
    /// What `#[storage]` and `#[side_effect]` state.
    pub facts: BuiltinFacts<usize>,
    /// The convention the storage implies, `None` where it implies none.
    pub returns: Option<ReturnConvention>,
    /// What the call keeps beyond it, as the storage implies.
    pub retains: Vec<RetainSpec<usize>>,
    /// Positions the declaration takes through a reference, which hand it the
    /// argument's storage.
    pub reference_params: IndexSet<usize>,
    /// Positions the declaration takes by `&mut`, the only ones it can write
    /// the caller's storage through. Snapshot here because the bodyless
    /// declaration keeps no parameters past lowering.
    pub mut_params: IndexSet<usize>,
    /// The arrays `outside` names: the call reaches their elements and nothing
    /// they reach.
    pub ranged_params: IndexSet<usize>,
    /// `#[immediate(...)]` positions, lowered to a Wasm immediate. Codegen
    /// reads the argument's literal value, so nothing may rewrite it into a
    /// load.
    pub immediate_params: IndexSet<usize>,
    /// The array parameter whose one element the call reaches, and how.
    pub element_access: Option<(usize, ArrayElementAccess)>,
    /// Every call ends in a trap, as a `!` return states.
    pub never_returns: bool,
    /// No parameter's type can carry storage, so the call is handed nothing it
    /// could write a field of.
    pub handed_no_storage: bool,
}

impl BuiltinDeclaration {
    /// The declaration `facts` make of a signature shaped as `shape`.
    pub fn new(facts: BuiltinFacts<usize>, shape: DeclarationShape) -> Self {
        let returns = match facts.storage {
            Storage::Opaque => None,
            Storage::PartOfArgs => Some(ReturnConvention::PartOf(shape.storage_params.clone())),
            Storage::None
            | Storage::Fresh
            | Storage::HoldsArgs
            | Storage::CopiesArgs
            | Storage::StoresArgs => Some(ReturnConvention::Owned),
        };
        let retain = |source: usize, into: RetainInto<usize>| RetainSpec {
            source,
            elements: shape.reference_params.contains(&source),
            into,
        };
        let retains = match facts.storage {
            Storage::HoldsArgs | Storage::CopiesArgs => shape
                .storage_params
                .iter()
                .map(|&p| retain(p, RetainInto::Result))
                .collect(),
            Storage::StoresArgs => {
                let dst = *shape
                    .mut_params
                    .first()
                    .expect("validation gives `stores_args` one `&mut` parameter");
                shape
                    .storage_params
                    .iter()
                    .filter(|&&p| p != dst)
                    .map(|&p| retain(p, RetainInto::Param(dst)))
                    .collect()
            }
            Storage::Opaque => shape
                .storage_params
                .iter()
                .map(|&source| RetainSpec {
                    source,
                    elements: false,
                    into: RetainInto::Anywhere,
                })
                .collect(),
            Storage::None | Storage::Fresh | Storage::PartOfArgs => Vec::new(),
        };
        let element_access = match facts
            .trap_checks()
            .iter()
            .filter(|check| matches!(check, TrapCheck::Outside { .. }))
            .collect::<Vec<_>>()[..]
        {
            [
                TrapCheck::Outside {
                    array,
                    at: Some(_),
                    count: None,
                },
            ] if shape.returns_value => {
                let access = if shape.returns_mut_ref {
                    ArrayElementAccess::Write
                } else {
                    ArrayElementAccess::Read
                };
                Some((*array, access))
            }
            _ => None,
        };
        let ranged_params = facts.ranged_arrays().copied().collect();
        let handed_no_storage = shape.storage_params.is_empty();
        Self {
            arity: shape.arity,
            facts,
            returns,
            retains,
            reference_params: shape.reference_params,
            mut_params: shape.mut_params,
            ranged_params,
            immediate_params: shape.immediate_params,
            element_access,
            never_returns: shape.returns_never,
            handed_no_storage,
        }
    }

    /// Whether the call returns new storage: `fresh`, `holds_args` or
    /// `copies_args`.
    pub fn allocates(&self) -> bool {
        self.facts.storage.returns_new_storage()
    }

    /// Whether the call may store where neither its result nor a `&mut`
    /// argument shows, itself or through the tasks that run while it suspends.
    /// Handed nothing that carries storage, a call that does not suspend
    /// reaches no memory to store into, whatever else its effects touch.
    pub fn stores_unseen(&self) -> bool {
        self.facts.suspend
            || !self.handed_no_storage
                && (self.facts.storage == Storage::Opaque
                    || matches!(self.facts.side_effect, SideEffect::Opaque))
    }

    /// Whether the call writes nothing itself: it hands back part of an
    /// argument, and its one `&mut` parameter, if any, is the array whose
    /// element it hands back, and it does not suspend. A write through the
    /// result is its user's.
    pub fn only_aliases(&self) -> bool {
        matches!(self.returns, Some(ReturnConvention::PartOf(_)))
            && !self.facts.is_opaque()
            && !self.facts.suspend
            && self
                .mut_params
                .iter()
                .all(|&p| self.element_access == Some((p, ArrayElementAccess::Write)))
    }

    /// Whether the call writes no struct field. A call that suspends may let
    /// another task write any field. Otherwise it reaches only what it is
    /// handed: nothing that carries storage, or, where its storage and effects
    /// are stated, arrays it ranges over through its `&mut` parameters.
    pub fn writes_no_field(&self) -> bool {
        !self.facts.suspend
            && (self.handed_no_storage
                || self.facts.storage != Storage::Opaque
                    && !self.facts.is_opaque()
                    && self.mut_params.is_subset(&self.ranged_params))
    }

    /// Whether nothing outside the program can tell the call ran: it writes no
    /// field, and its side effect is stated. An `opaque` one may be handed no
    /// storage and still complete a task (`task_return`) or write a stream.
    pub fn is_unobservable(&self) -> bool {
        !self.facts.is_opaque() && self.writes_no_field()
    }
}

/// All a declaration lookup reads of a call. TIR and NIR each carry their own
/// `FunctionRef`, so the key is spelled out rather than taken from either.
#[derive(Clone, Copy)]
pub struct DeclarationLookup<'a> {
    pub module_source: &'a ModuleSource,
    pub name: &'a str,
    /// The generic declaration a monomorphized instance came from.
    pub generic_name: Option<&'a str>,
}

impl<'a> DeclarationLookup<'a> {
    /// The `core:builtin` intrinsic this is, plain or monomorphized. A function
    /// declared anywhere else may share the name, a wasm-asset export included.
    pub fn intrinsic(self) -> Option<&'a str> {
        self.module_source
            .is_core_builtin()
            .then(|| self.generic_name.unwrap_or(self.name))
    }
}

impl<'a> From<&'a TirFunction> for DeclarationLookup<'a> {
    fn from(func: &'a TirFunction) -> Self {
        Self {
            module_source: &func.module_source,
            name: &func.name,
            generic_name: func
                .monomorph_info
                .as_ref()
                .map(|m| m.generic_name.as_str()),
        }
    }
}

impl<'a> From<&'a FunctionRef> for DeclarationLookup<'a> {
    fn from(func: &'a FunctionRef) -> Self {
        Self {
            module_source: &func.module_source,
            name: &func.name,
            generic_name: func
                .monomorph_info
                .as_ref()
                .map(|m| m.generic_name.as_str()),
        }
    }
}

/// One value per body-less declaration, found from a call to it.
#[derive(Debug, Clone)]
pub struct DeclarationTable<V>(IndexMap<ModuleSource, IndexMap<String, V>>);

impl<V> Default for DeclarationTable<V> {
    fn default() -> Self {
        Self(IndexMap::default())
    }
}

impl<V> DeclarationTable<V> {
    /// A table over `declarations`, keyed by module and declared name.
    pub fn new(declarations: IndexMap<(ModuleSource, String), V>) -> Self {
        let mut by_module: IndexMap<ModuleSource, IndexMap<String, V>> = IndexMap::default();
        for ((module, name), value) in declarations {
            by_module.entry(module).or_default().insert(name, value);
        }
        Self(by_module)
    }

    /// The value for the declaration `call` resolves to, or `None` where there
    /// is none. Keyed by the generic name a monomorphized instance came from,
    /// which is the name the declaration was snapshot under. It allocates
    /// nothing, since every call a pass visits asks.
    pub fn get<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> Option<&V> {
        let call = call.into();
        let module = self.0.get(call.module_source)?;
        call.generic_name
            .and_then(|generic| module.get(generic))
            .or_else(|| module.get(call.name))
    }

    /// The same declarations, each mapped to `f` of its value.
    pub fn map<W>(&self, mut f: impl FnMut(&V) -> W) -> DeclarationTable<W> {
        DeclarationTable(
            self.0
                .iter()
                .map(|(module, names)| {
                    let names = names.iter().map(|(n, v)| (n.clone(), f(v))).collect();
                    (module.clone(), names)
                })
                .collect(),
        )
    }
}

/// What each body-less declaration stated about storage, resolved from a call.
/// Link snapshots these before monomorphization drops the generic declarations,
/// and both the lowering plan and the NIR optimizer read them.
pub type BuiltinDeclarations = DeclarationTable<BuiltinDeclaration>;

impl DeclarationTable<BuiltinDeclaration> {
    /// Whether `call` names a body-less declaration whose storage implies a
    /// return convention — the calls that answer from a declaration rather
    /// than from the fixpoint.
    pub fn declares<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> bool {
        self.get(call.into()).is_some_and(|d| d.returns.is_some())
    }

    /// Whether a call to the body-less `call` may leave for the host: nothing
    /// declares it, or it declares `#[side_effect(opaque)]`.
    pub fn leaves_for_host<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> bool {
        self.get(call.into())
            .is_none_or(|d| matches!(d.facts.side_effect, SideEffect::Opaque))
    }

    /// Whether another task may run before a call to the body-less `call`
    /// returns, and read or write anything held elsewhere: nothing declares
    /// it, as for a Component Model import, or it declares `suspend`.
    pub fn may_suspend<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> bool {
        self.get(call.into()).is_none_or(|d| d.facts.suspend)
    }

    /// Whether the call leaves the argument *object* at `pos` where the caller
    /// put it: it neither writes through it (`&mut`) nor keeps it past the
    /// return. It says nothing about what that object holds — a call keeping
    /// the elements a reference points to still answers `true` here, so a
    /// caller asking about reachable storage must read the retain specs itself.
    ///
    /// A call with no snapshot answers `false`: nothing here says what it does.
    pub fn reads_param<'a>(&self, call: impl Into<DeclarationLookup<'a>>, pos: usize) -> bool {
        self.get(call.into()).is_some_and(|d| {
            !d.mut_params.contains(&pos)
                && !d.retains.iter().any(|r| r.source == pos && !r.elements)
        })
    }

    /// The positions the call lowers to a Wasm immediate, where codegen reads
    /// the argument's literal. An optimizer that would replace one with
    /// anything else — a global read, a local — must leave it alone.
    ///
    /// Read from the snapshot rather than from the callee's parameters, which a
    /// bodyless declaration does not keep past lowering.
    pub fn immediate_params<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> IndexSet<usize> {
        self.get(call.into())
            .map(|d| d.immediate_params.clone())
            .unwrap_or_default()
    }

    /// The array parameter whose one element `call` reaches, and how.
    pub fn element_access<'a>(
        &self,
        call: impl Into<DeclarationLookup<'a>>,
    ) -> Option<(usize, ArrayElementAccess)> {
        self.get(call.into())?.element_access
    }

    /// The positions `call` takes by `&mut`, `None` where nothing was snapshot.
    pub fn mut_params<'a>(
        &self,
        call: impl Into<DeclarationLookup<'a>>,
    ) -> Option<&IndexSet<usize>> {
        self.get(call.into()).map(|d| &d.mut_params)
    }

    /// Whether what `call` hands back is never part of an argument.
    pub fn returns_owned<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> bool {
        self.get(call.into())
            .is_some_and(|d| d.returns == Some(ReturnConvention::Owned))
    }

    /// Whether `call` writes no struct field.
    pub fn writes_no_field<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> bool {
        self.get(call.into())
            .is_some_and(BuiltinDeclaration::writes_no_field)
    }

    /// Whether `call` writes nothing itself, only handing back part of an
    /// argument.
    pub fn only_aliases<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> bool {
        self.get(call.into())
            .is_some_and(BuiltinDeclaration::only_aliases)
    }

    /// Whether `call` returns new storage.
    pub fn allocates<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> bool {
        self.get(call.into())
            .is_some_and(BuiltinDeclaration::allocates)
    }

    /// The parameters a declaration's result may be a component of, for a
    /// call whose storage is `part_of_args`.
    pub fn part_of_params<'a>(&self, call: impl Into<DeclarationLookup<'a>>) -> Option<&[usize]> {
        match &self.get(call.into())?.returns {
            Some(ReturnConvention::PartOf(params)) => Some(params),
            Some(ReturnConvention::Owned) | None => None,
        }
    }

    /// The parameters a declaration keeps beyond the call.
    pub fn retained_params<'a>(
        &self,
        call: impl Into<DeclarationLookup<'a>>,
    ) -> impl Iterator<Item = usize> + '_ {
        self.retain_specs(call).map(|r| r.source)
    }

    /// Everything a declaration keeps beyond the call, destinations included.
    pub fn retain_specs<'a>(
        &self,
        call: impl Into<DeclarationLookup<'a>>,
    ) -> impl Iterator<Item = &RetainSpec<usize>> + '_ {
        self.get(call.into())
            .into_iter()
            .flat_map(|d| d.retains.iter())
    }
}

/// A body's local frame. Taken and given whole, so a caller moving a body
/// between functions cannot carry the locals and leave what describes them.
#[derive(Debug, Clone, Default)]
pub struct LocalFrame {
    pub locals: Vec<TirLocal>,
    /// Indices of the locals something takes the address of (`&x` / `&mut x`),
    /// which codegen boxes so a write through the reference is seen.
    pub address_taken: IndexSet<u32>,
}

impl LocalFrame {
    /// Append `other`, renumbering its locals to follow this frame's.
    pub fn absorb(&mut self, other: LocalFrame, shift: impl FnOnce(u32)) {
        let count = u32::try_from(other.locals.len()).expect("local count fits in u32");
        assert!(
            other.address_taken.iter().all(|&i| i < count),
            "an address-taken index names a local of its own frame"
        );
        let offset = u32::try_from(self.locals.len()).expect("local count fits in u32");
        if offset > 0 && !other.locals.is_empty() {
            shift(offset);
        }
        self.locals.extend(other.locals);
        self.address_taken
            .extend(other.address_taken.into_iter().map(|i| i + offset));
    }
}

impl TirFunction {
    /// Where an attribute's named parameter sits. The body walk drops a clause
    /// naming no parameter, so one reaching here names a parameter of this
    /// very declaration.
    fn param_position(&self, name: &str) -> usize {
        self.params
            .iter()
            .position(|p| p.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "`{}` names `{name}`, which it takes no parameter for",
                    self.name
                )
            })
    }

    /// This declaration's `#[immediate(...)]` parameters by position.
    pub fn immediates_by_position(&self) -> impl Iterator<Item = usize> + '_ {
        self.immediates
            .iter()
            .map(move |name| self.param_position(name))
    }

    /// This declaration's `#[storage]` and `#[side_effect]` by parameter
    /// position.
    pub fn declared_by_position(&self) -> Option<BuiltinFacts<usize>> {
        self.declared
            .as_ref()
            .map(|facts| facts.map(|name| self.param_position(name)))
    }

    /// Take the body's frame, leaving an empty one. The counterpart of
    /// [`Self::set_frame`]: a caller moving a body elsewhere takes what
    /// describes its locals with it.
    pub fn take_frame(&mut self) -> LocalFrame {
        self.local_count = 0;
        LocalFrame {
            locals: std::mem::take(&mut self.locals),
            address_taken: std::mem::take(&mut self.address_taken_locals),
        }
    }

    /// Give the body a whole new frame. The three fields describing one are
    /// replaced together, so a caller swapping a body cannot leave one of them
    /// describing the body it replaced.
    pub fn set_frame(&mut self, frame: LocalFrame) {
        let LocalFrame {
            locals,
            address_taken,
        } = frame;
        self.local_count = u32::try_from(locals.len()).expect("local count fits in u32");
        self.locals = locals;
        self.address_taken_locals = address_taken;
    }

    /// A parameterless function the compiler mints for itself: no declaration,
    /// no generics, no effects.
    #[must_use]
    pub fn synthesized(
        module_source: ModuleSource,
        name: String,
        return_type: TypeId,
        body: TirBlock,
        frame: LocalFrame,
        span: Span,
    ) -> Self {
        let LocalFrame {
            locals,
            address_taken,
        } = frame;
        Self {
            module_source,
            def_id: None,
            is_async: false,
            name,
            visibility: Visibility::Public,
            is_export: false,
            type_params: Vec::new(),
            impl_type_params: Vec::new(),
            impl_origin: None,
            monomorph_info: None,
            method_info: None,
            params: Vec::new(),
            return_type,
            task_return_type: None,
            effects: Vec::new(),
            immediates: Vec::new(),
            declared: None,
            body: Some(body),
            span,
            local_count: u32::try_from(locals.len()).expect("local count fits in u32"),
            locals,
            address_taken_locals: address_taken,
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: InlineHint::Auto,
            compiler_item: None,
            export_name: None,
            allocator_tag: None,
            kind: FunctionKind::Regular,
            return_abi: ReturnAbi::default(),
        }
    }

    /// Returns true if this is a method (belongs to a struct)
    #[inline]
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }

    /// Whether the first parameter is the `self` receiver.
    #[inline]
    pub fn takes_self(&self) -> bool {
        self.params.first().is_some_and(TirParam::is_self)
    }

    /// Returns true if this is the synthesized `$call` method on a
    /// `$Closure_N` functor struct. See
    /// [`LocalMethodName::is_closure_call`] for the rationale.
    #[inline]
    pub fn is_closure_call(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(LocalMethodName::is_closure_call)
    }

    /// Returns true if this function has type params that need monomorphization
    /// (excludes effect params, which are erased at compile time).
    #[inline]
    pub fn has_real_type_params(&self) -> bool {
        self.type_params.iter().any(|p| !p.is_effect)
    }

    /// Whether monomorphization instantiates this function rather than
    /// emitting it as written.
    pub fn is_template(&self) -> bool {
        self.has_real_type_params() || !self.impl_type_params.is_empty()
    }

    /// The identity a call of this function records: its declaration and block,
    /// or the name synthesis gave it. `None` for an instance.
    pub fn template_id(&self) -> Option<TemplateId> {
        if self.monomorph_info.is_some() {
            return None;
        }
        if let FunctionKind::UncheckedTwin { of } = self.kind {
            return Some(TemplateId::Unchecked {
                def: of,
                block: self.impl_origin,
            });
        }
        Some(match self.def_id {
            Some(def) => TemplateId::Declared {
                def,
                block: self.impl_origin,
            },
            None => TemplateId::Synthesized {
                module: self.module_source.clone(),
                name: self.name.clone(),
            },
        })
    }

    /// Returns the copied type if this is a synthesized value-copy function.
    #[inline]
    pub fn value_copy_type(&self) -> Option<TypeId> {
        match self.kind {
            FunctionKind::ValueCopy { type_id } => Some(type_id),
            _ => None,
        }
    }

    /// Dispatch coordinates of an auto-derived `fn(..)^Inspect` stub, which WIR
    /// build turns into the indirect-call body.
    #[inline]
    pub fn fn_canonical_dispatch(&self) -> Option<(usize, TypeId)> {
        match self.kind {
            FunctionKind::FnCanonicalDispatch { arity, return_type } => Some((arity, return_type)),
            _ => None,
        }
    }

    /// Returns true if this function was synthesized as a value-copy helper.
    #[inline]
    pub fn is_value_copy(&self) -> bool {
        matches!(self.kind, FunctionKind::ValueCopy { .. })
    }
}

/// A resolved local slot in a function, global initializer, or closure scope.
/// `FunctionContext::locals` is the single source of truth — every parameter,
/// `let`, destructure binding and elaborator temporary — and is projected onto
/// `TirFunction::locals` (keyed by Wasm local index) and onto
/// `Closure { body_locals }`, whose params stay in `params` instead.
#[derive(Debug, Clone)]
pub struct TirLocal {
    /// Source-level name of the binding (or a synthesised `$name` for
    /// elaborator-generated temporaries that have no surface syntax).
    pub name: String,
    pub type_id: TypeId,
    pub is_mut: bool,
    /// Where the binding was written. Default for a synthesised slot, which
    /// no diagnostic can point at.
    pub span: Span,
}

impl TirLocal {
    /// Build a `TirLocal` for a synthesised slot whose name follows the
    /// `$local_N` convention used by `wir_build` when no source-level
    /// name is available.
    pub fn synth(index: u32, type_id: TypeId, is_mut: bool) -> Self {
        Self {
            name: format!("$local_{index}"),
            type_id,
            is_mut,
            span: Span::default(),
        }
    }
}

/// The local table of one frame: a function's, or a closure body's, whose
/// parameters are numbered ahead of its body locals.
pub enum FrameLocals<'a> {
    Function {
        locals: &'a mut Vec<TirLocal>,
        local_count: &'a mut u32,
    },
    Closure {
        params: &'a [(String, TypeId)],
        body_locals: &'a mut Vec<TirLocal>,
    },
}

impl FrameLocals<'_> {
    /// `None` past the table, which a synthesized frame may leave empty: its
    /// slots cannot be told apart from a fresh one, so it gets no proxy.
    pub fn local(&self, index: u32) -> Option<(&str, TypeId)> {
        let index = index as usize;
        match self {
            Self::Function { locals, .. } => locals
                .get(index)
                .map(|local| (local.name.as_str(), local.type_id)),
            Self::Closure {
                params,
                body_locals,
            } => match params.get(index) {
                Some((name, type_id)) => Some((name, *type_id)),
                None => body_locals
                    .get(index - params.len())
                    .map(|local| (local.name.as_str(), local.type_id)),
            },
        }
    }

    /// The locals the frame declares, by index; a closure's parameters, which no
    /// loop of the body they head can declare, are left out.
    pub fn declarations(&self) -> impl Iterator<Item = (u32, &TirLocal)> {
        let (first, declared) = match self {
            Self::Function { locals, .. } => (0, locals.as_slice()),
            Self::Closure {
                params,
                body_locals,
            } => (params.len(), body_locals.as_slice()),
        };
        declared
            .iter()
            .enumerate()
            .map(move |(at, local)| (u32::try_from(first + at).unwrap(), local))
    }

    /// The index the next `alloc` returns.
    pub fn next_index(&self) -> u32 {
        let next = match self {
            Self::Function { locals, .. } => locals.len(),
            Self::Closure {
                params,
                body_locals,
            } => params.len() + body_locals.len(),
        };
        u32::try_from(next).unwrap()
    }

    pub fn alloc(&mut self, name: String, type_id: TypeId) -> u32 {
        let index = self.next_index();
        let local = TirLocal {
            name,
            type_id,
            is_mut: false,
            span: Span::default(),
        };
        match self {
            Self::Function {
                locals,
                local_count,
            } => {
                assert_eq!(**local_count as usize, locals.len());
                locals.push(local);
                **local_count += 1;
            }
            Self::Closure { body_locals, .. } => body_locals.push(local),
        }
        index
    }
}

#[derive(Debug, Clone)]
pub struct TirParam {
    pub name: String,
    pub type_id: TypeId,
    pub local_index: u32,
    pub is_mut: bool,
    /// The parameter is a `&mut T` (captured here before the boxing
    /// plan rewrites `&mut T` and `&T` to the same `Box<T>` type, erasing the
    /// distinction). A `&T` cannot be written through, so only a `&mut`
    /// parameter can mutate the caller's argument storage.
    ///
    /// `lower::plan` fills it, so anything running before that — `link` among
    /// them — reads `false` here whatever the type says, and must ask the type.
    pub is_mut_ref: bool,
    pub span: Span,
}

impl TirParam {
    /// Whether this is the `self` receiver.
    #[must_use]
    pub fn is_self(&self) -> bool {
        self.name == "self"
    }
}

#[derive(Debug, Clone)]
pub struct TirStruct {
    /// The struct type this reifies, as its head plus what it was
    /// instantiated with — the same pair `ResolvedType::Struct` carries, so a
    /// consumer keys on it instead of re-rendering a spelling to match one
    /// built elsewhere.
    pub def: StructDef,
    pub type_args: Vec<TypeId>,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: Visibility,
    /// Generic type parameters (empty for non-generic structs)
    pub type_params: Vec<TirTypeParam>,
    /// If this struct was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub fields: Vec<TirField>,
    pub span: Span,
    /// `#[wire(name_policy = "...")]` — naming strategy for all fields.
    pub wire_name_policy: Option<NamePolicy>,
}

impl TirStruct {
    /// The struct of anonymous shape `shape`, holding `fields`.
    pub fn anonymous(
        shape: AnonStructId,
        name: String,
        module_source: ModuleSource,
        fields: &[(String, TypeId)],
        span: Span,
    ) -> Self {
        Self {
            def: StructDef::Anon(shape),
            type_args: Vec::new(),
            name,
            module_source,
            visibility: Visibility::Private,
            type_params: Vec::new(),
            monomorph_info: None,
            fields: fields
                .iter()
                .enumerate()
                .map(|(i, (name, ty))| {
                    TirField::plain(name.clone(), Visibility::Public, *ty, i as u32, span)
                })
                .collect(),
            span,
            wire_name_policy: None,
        }
    }
}

/// Each reified struct's fields, keyed as `ResolvedType::Struct` names it.
pub type StructFields = IndexMap<(StructDef, Vec<TypeId>), Vec<TirField>>;

#[must_use]
pub fn struct_fields(structs: &[TirStruct]) -> StructFields {
    structs
        .iter()
        .map(|s| ((s.def, s.type_args.clone()), s.fields.clone()))
        .collect()
}

#[derive(Debug, Clone)]
pub struct TirField {
    pub name: String,
    pub visibility: Visibility,
    pub type_id: TypeId,
    pub index: u32,
    pub span: Span,
    /// `#[secret]` — field not shown in debug inspect output.
    pub is_secret: bool,
    /// `#[wire(name = "name")]` — custom serialization name for this field.
    pub wire_name_override: Option<String>,
    /// `#[wire(default)]` — use default value when field is missing during deserialization.
    pub serde_default: bool,
    /// `#[wire(positional)]` — field is resolved by position, not by name.
    /// Format-agnostic ordinal hint: synthesized `FieldSchema::lookup` omits it
    /// (never matched by name) and `positional_at` enumerates it. Name-only and
    /// sequence-only formats ignore it; `core:args` binds it to a bare token.
    pub serde_positional: bool,
    /// Resolved default expression for `struct S { x: T = expr }`.
    /// Inserted by the elaborator when the field is omitted in a struct literal.
    pub default_expr: Option<Box<TirExpr>>,
}

impl TirField {
    /// A field the compiler adds itself: no attributes and no default.
    pub fn plain(
        name: String,
        visibility: Visibility,
        type_id: TypeId,
        index: u32,
        span: Span,
    ) -> Self {
        Self {
            name,
            visibility,
            type_id,
            index,
            span,
            is_secret: false,
            wire_name_override: None,
            serde_default: false,
            serde_positional: false,
            default_expr: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TirEnum {
    /// The declaration this was reified from.
    pub def: DefId,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: Visibility,
    /// Generic type parameters (empty for non-generic enums)
    pub type_params: Vec<TirTypeParam>,
    /// If this enum was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub cases: Vec<TirEnumCase>,
    pub span: Span,
    /// `#[wire(name_policy = "...")]` — naming strategy for all cases.
    pub wire_name_policy: Option<NamePolicy>,
}

/// A case in a TIR enum.
/// Unlike `TirVariantCase`, enum cases have no payload.
#[derive(Debug, Clone)]
pub struct TirEnumCase {
    pub name: String,
    pub index: u32,
    pub span: Span,
    /// `#[wire(name = "...")]` — custom serialized name for this case.
    pub wire_name_override: Option<String>,
}

/// A flags type declaration (bitmask type, like WIT flags)
/// e.g., `flags PathFlags { SymlinkFollow }`
/// Represented as `ResolvedType::Flags`; each member is a bitmask value (1 << index).
#[derive(Debug, Clone)]
pub struct TirFlags {
    /// The declaration this was reified from.
    pub def: DefId,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: Visibility,
    /// The newtype `TypeId` (base type is u32)
    pub type_id: TypeId,
    pub members: Vec<TirFlagsMember>,
    pub span: Span,
    pub wire_name_policy: Option<NamePolicy>,
}

/// A member of a flags type
#[derive(Debug, Clone)]
pub struct TirFlagsMember {
    pub name: String,
    /// Bitmask value: `1 << index`
    pub bitmask: u32,
    pub span: Span,
}

/// A variant type declaration (tagged union, distinct from enum)
/// e.g., `variant Shape { Circle(f64), Rectangle(f64, f64), Point }`
#[derive(Debug, Clone)]
pub struct TirVariantDecl {
    /// The declaration this was reified from. Case indices are looked up
    /// through it, so a same-named variant in another module cannot answer.
    pub def: DefId,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: Visibility,
    /// Generic type parameters (e.g., `T` in `variant Option<T>`)
    pub type_params: Vec<TirTypeParam>,
    /// Cases of the variant (e.g., Some, None for Option)
    pub cases: Vec<TirVariantCase>,
    pub span: Span,
    /// `#[wire(name_policy = "...")]` — naming strategy for all cases.
    pub wire_name_policy: Option<NamePolicy>,
    /// `#[wire(tag = "...")]` — the key an internally tagged case is written
    /// under.
    pub wire_tag: Option<String>,
}

/// A case in a variant declaration
/// e.g., `Circle(f64)` or `Point`
///
/// Each variant case has exactly one payload type:
/// - Unit variants: `None` → payload is `()` (unit type)
/// - Scalar payloads: `Some(T)` → payload is `T`
/// - Tuple payloads: `Rectangle([f64, f64])` → payload is `[f64, f64]`
#[derive(Debug, Clone)]
pub struct TirVariantCase {
    pub name: String,
    /// Case index (0-based)
    pub index: u32,
    /// Payload type for this case. Unit variants have `()` (unit type) payload.
    pub payload: TypeId,
    pub span: Span,
    /// `#[wire(name = "...")]` — custom serialized name for this case.
    pub wire_name_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TirNewtype {
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: Visibility,
    pub def: DefId,
    /// Empty for `type N = T`; the declaration's parameters for `type N<T> = …`,
    /// which is then one declaration over many types.
    pub type_params: Vec<TirTypeParam>,
    /// `None` for a generic declaration: it names no single type, since each
    /// instantiation is its own. Its impls are synthesized over the declaration
    /// and instantiated per use, the way a generic struct's are.
    pub type_id: Option<TypeId>,
    pub span: Span,
}

/// Test declaration metadata
/// The actual test code is stored as a `TirFunction` in the functions list.
#[derive(Debug, Clone)]
pub struct TirTest {
    /// The original test name from source (None if unnamed)
    pub name: Option<String>,
    /// Generated function name (e.g., "$`test_0`", "$`test_trap_0`", or "$`test_todo_0`")
    pub function_name: String,
    /// Source line number for unnamed test identification
    pub line: usize,
    pub span: Span,
    /// Whether this test is expected to trap (from `#[expect_trap]` attribute)
    pub expect_trap: bool,
    /// Whether this test is a TODO placeholder (from `#[TODO]` attribute).
    /// Like `expect_trap`, the test passes when the body traps, but the runner emits
    /// a distinct message when the body unexpectedly passes, reminding the developer
    /// to remove the `#[TODO]` attribute.
    pub is_todo: bool,
    /// Per-test timeout in milliseconds (from `#[timeout_ms(N)]` attribute).
    /// `None` means use the default timeout (1 second).
    pub timeout_ms: Option<u64>,
    pub is_synopsis: bool,
    pub is_workload: bool,
}

#[derive(Debug, Clone)]
pub struct TirEffect {
    /// The declaration this was reified from.
    pub head: DeclaredHead,
    pub visibility: Visibility,
    pub operations: Vec<TirEffectOp>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirEffectOp {
    pub def: DefId,
    pub name: String,
    pub params: Vec<TirParam>,
    pub return_type: TypeId,
    pub span: Span,
    /// The `#[cm("...")]` payload on the operation, `None` where it carries
    /// none.
    pub cm_name: Option<String>,
    pub is_async: bool,
    /// Set when the declaration gave the operation a body: what it does when
    /// dispatched with no handler installed. The dispatch wrapper calls
    /// [`crate::name::effect_default_impl_name`] in that case instead of
    /// trapping.
    pub has_default: bool,
}

/// Resource declaration captured in TIR for effect propagation.
///
/// Resources are effects in Wado's effect system: every operation on a
/// resource type requires the resource to be in scope. The `operations`
/// list mirrors `TirEffect` so the propagation closure builder can treat
/// effects and resources uniformly.
#[derive(Debug, Clone)]
pub struct TirResource {
    /// The declaration this was reified from.
    pub head: DeclaredHead,
    pub visibility: Visibility,
    pub operations: Vec<TirEffectOp>,
    pub type_params: Vec<TirTypeParam>,
    pub span: Span,
}

/// Trait declaration
#[derive(Debug, Clone)]
pub struct TirTrait {
    pub name: String,
    pub visibility: Visibility,
    pub type_params: Vec<TirTypeParam>,
    pub methods: Vec<TirTraitMethod>,
    pub span: Span,
}

/// A method signature in a trait
#[derive(Debug, Clone)]
pub struct TirTraitMethod {
    pub name: String,
    pub params: Vec<TirParam>,
    pub return_type: TypeId,
    pub has_default_body: bool,
    pub span: Span,
}

/// Which compiler-synthesizable trait an `impl Trait for Type;` request names.
///
/// The set is closed: the elaborator classifies the requested trait at the
/// syntax boundary and rejects anything else with a diagnostic, so downstream
/// synthesis never needs to re-parse a trait-name string. A `From` marker
/// never reaches here: `marker` writes it out as an ordinary impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SynthTrait {
    Serialize,
    Deserialize,
}

/// An `impl` block as declared — its identity, not its methods, which live in
/// [`TirModule::functions`] linked back by [`TirFunction::method_info`]. The
/// record exists for a block whose only content is a rest clause
/// (`impl Log for Passthrough { ..forward }`), which produces no methods at
/// all. Consumed by the effect-dispatch synthesis; nothing past it sees an
/// impl block.
#[derive(Debug, Clone)]
pub struct TirImpl {
    /// The trait this block implements; `None` for an inherent impl. Matches
    /// `LocalMethodName::base_trait_head`, so a block and its methods agree on
    /// which declaration they target.
    pub trait_canonical: Option<DeclaredHead>,
    /// Trait / resource type arguments at the impl site (`impl Stream<u8>` →
    /// `[u8]`). Matches `LocalMethodName::trait_type_args`.
    pub trait_type_args: Vec<TypeId>,
    /// The target type's name, derived exactly as `reify_method` derives it
    /// for this block's methods, so a block and its methods produce the same
    /// key in the effect-dispatch handler index.
    pub struct_name: String,
    /// `..trap` / `..forward`, when the block ends with a rest clause.
    pub rest: Option<RestClause>,
    pub span: Span,
}

/// `impl Trait for Type;` — request the compiler to synthesize the trait implementation.
#[derive(Debug, Clone)]
pub struct SynthesisRequest {
    pub trait_ref: SynthTrait,
    pub target_type_name: String,
    pub target_type_id: TypeId,
    /// Type parameters: `(name, index, type_id)`
    pub type_params: Vec<(String, u32, TypeId)>,
    pub span: Span,
}

/// Metadata about a closure for optimization (especially inlining).
///
/// This is populated by the lower phase and used by the optimizer to inline
/// closure calls when the closure is known at compile time.
#[derive(Debug, Clone)]
pub struct ClosureFunctor {
    pub module_source: ModuleSource,
    /// Unique closure ID (matches the order closures are visited in the module)
    pub id: u32,
    /// Name of the generated functor struct (e.g., `$Closure_0`)
    pub struct_name: String,
    /// Type ID of the generated functor struct (bare struct type for definitions)
    pub struct_type_id: TypeId,
    /// Type ID of reference to functor struct (for expression/local types)
    /// Functors are reference types, so variables holding them have this type.
    pub ref_type_id: TypeId,
    /// The `$call` method for this closure (with body transformed:
    /// Capture nodes become `FieldAccess` on self)
    pub call_method: Rc<RefCell<TirFunction>>,
    /// The per-functor `$Closure_N^Inspect::inspect` impl.
    pub inspect_method: Rc<RefCell<TirFunction>>,
    /// Captures from the original closure
    pub captures: Vec<TirCapture>,
    /// Canonical user-declared `(name, type)` pairs of the closure literal,
    /// captured at functor creation and never mutated. `register_closure_wrappers`
    /// derives the wrapper's external signature
    /// (`fn(env, ..canonical_user_params) -> canonical_return`) from this
    /// snapshot, so a later DAE shrink of `$call` cannot desynchronise it.
    pub canonical_user_params: Vec<(String, TypeId)>,
    /// Canonical return type of the closure literal. Same role as
    /// `canonical_user_params` — drives the wrapper external signature.
    pub canonical_return: TypeId,
}

/// Tracks a requested instantiation of a generic item.
/// `name`, `module_source`, `impl_type_args`, and `method_type_args` are used for equality/hashing.
/// `method_info` names an instance but never decides one: it is left out of
/// both, so read a declaration's own `method_info` for anything else.
#[derive(Debug, Clone)]
pub struct InstantiationKey {
    /// The generic declaration being instantiated, where the site holds one.
    ///
    /// `name` cannot stand in for it: two sibling functions may each declare a
    /// `struct Box<T>` in one module, and a `(name, module)` lookup answers
    /// with whichever was declared first — collapsing two distinct types onto
    /// one. `None` for a function or enum instantiation, which key by name.
    pub def: Option<DefId>,
    /// Name of the generic item (struct, function, or enum)
    pub name: String,
    /// Module where the generic item is defined.
    /// Distinguishes same-named generics from different modules.
    pub module_source: ModuleSource,
    /// Impl-level type arguments (from the struct/type)
    pub impl_type_args: Vec<TypeId>,
    /// Method-level type arguments (from the method's own generics)
    pub method_type_args: Vec<TypeId>,
    /// Method info for method instantiations (None for struct/enum instantiations)
    /// Not included in equality/hash - used only for name formatting
    pub method_info: Option<LocalMethodName>,
    /// The function template instantiated, which is what the instance is
    /// made from. `None` for a struct or enum.
    pub template: Option<TemplateId>,
}

impl PartialEq for InstantiationKey {
    fn eq(&self, other: &Self) -> bool {
        self.def == other.def
            && self.template == other.template
            && self.name == other.name
            && self.module_source == other.module_source
            && self.impl_type_args == other.impl_type_args
            && self.method_type_args == other.method_type_args
    }
}

impl Eq for InstantiationKey {}

impl std::hash::Hash for InstantiationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.def.hash(state);
        self.template.hash(state);
        self.name.hash(state);
        self.module_source.hash(state);
        self.impl_type_args.hash(state);
        self.method_type_args.hash(state);
    }
}

#[derive(Debug, Clone)]
pub struct TirModule {
    pub module_source: ModuleSource,
    /// Shared type table across all modules (enables cross-module type references)
    pub type_table: Rc<RefCell<TypeTable>>,
    pub functions: Vec<Rc<RefCell<TirFunction>>>,
    pub structs: Vec<TirStruct>,
    pub enums: Vec<TirEnum>,
    /// Flags type declarations (bitmask types, newtypes over u32)
    pub flags: Vec<TirFlags>,
    /// Custom variant declarations (tagged unions with payloads)
    pub variants: Vec<TirVariantDecl>,
    pub newtypes: Vec<TirNewtype>,
    pub effects: Vec<TirEffect>,
    pub resources: Vec<TirResource>,
    pub traits: Vec<TirTrait>,
    /// `impl` blocks as declared (identity + rest clause); their methods are
    /// in `functions`.
    pub impls: Vec<TirImpl>,
    /// `impl Trait for Type;` — synthesis requests (populated by elaborator, consumed by synthesis)
    pub synthesis_requests: Vec<SynthesisRequest>,
    /// Test declarations with their metadata
    pub tests: Vec<TirTest>,
    /// Global variable declarations
    pub globals: Vec<TirGlobal>,
    pub data_section: Option<String>,
    /// `#![wasm_module("name")]` — items in this module compile to a separate Wasm core module.
    pub wasm_module: Option<String>,
}

impl TirModule {
    pub fn new(module_source: ModuleSource) -> Self {
        Self {
            module_source,
            type_table: Rc::new(RefCell::new(TypeTable::new())),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            flags: Vec::new(),
            variants: Vec::new(),
            newtypes: Vec::new(),
            effects: Vec::new(),
            resources: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
        }
    }

    pub fn with_type_table(
        module_source: ModuleSource,
        type_table: Rc<RefCell<TypeTable>>,
    ) -> Self {
        Self {
            module_source,
            type_table,
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            flags: Vec::new(),
            variants: Vec::new(),
            newtypes: Vec::new(),
            effects: Vec::new(),
            resources: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
        }
    }

    pub fn with_data_section(mut self, data_section: Option<String>) -> Self {
        self.data_section = data_section;
        self
    }

    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }

    pub fn add_function(&mut self, func: TirFunction) -> Rc<RefCell<TirFunction>> {
        let func_rc = Rc::new(RefCell::new(func));
        self.functions.push(Rc::clone(&func_rc));
        func_rc
    }

    pub fn add_struct(&mut self, s: TirStruct) {
        self.structs.push(s);
    }

    pub fn add_enum(&mut self, e: TirEnum) {
        self.enums.push(e);
    }

    pub fn add_flags(&mut self, f: TirFlags) {
        self.flags.push(f);
    }

    pub fn add_newtype(&mut self, newtype: TirNewtype) {
        self.newtypes.push(newtype);
    }

    pub fn add_effect(&mut self, effect: TirEffect) {
        self.effects.push(effect);
    }

    pub fn add_resource(&mut self, resource: TirResource) {
        self.resources.push(resource);
    }

    pub fn add_trait(&mut self, trait_decl: TirTrait) {
        self.traits.push(trait_decl);
    }

    pub fn add_impl(&mut self, impl_block: TirImpl) {
        self.impls.push(impl_block);
    }

    pub fn find_function(&self, name: &str) -> Option<Rc<RefCell<TirFunction>>> {
        self.functions
            .iter()
            .find(|f| f.borrow().name == name)
            .cloned()
    }

    pub fn find_struct(&self, name: &str) -> Option<&TirStruct> {
        self.structs.iter().find(|s| s.name == name)
    }

    pub fn find_enum(&self, name: &str) -> Option<&TirEnum> {
        self.enums.iter().find(|e| e.name == name)
    }
}

#[derive(Debug)]
pub struct TirProgram {
    pub main_module: TirModule,
    pub dependencies: Vec<TirModule>,
    pub type_table: TypeTable,
}

impl TirProgram {
    pub fn new(main_module: TirModule) -> Self {
        Self {
            type_table: TypeTable::new(),
            main_module,
            dependencies: Vec::new(),
        }
    }
}
