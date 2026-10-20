//! Whether a value of one type can reach the storage a value of another owns,
//! read off the types alone: through a reference to a type that storage holds,
//! or, for a result that is not fresh, by holding such a type by value.
//!
//! A call whose retention puts a reference argument into its result answers
//! for every argument it might return. `rmsnorm(g, &x) -> List<&mut Value>`
//! returns a fresh list of references to values. A `Value` owns a
//! `List<&mut Value>` of its own, but no reference to one, so the result
//! reaches none of `x`'s storage whatever its body does.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{ResolvedType, StructFields, TirExpr, TirExprKind, TypeId};
use crate::tir_visitor::TirRefVisitor;
use crate::type_table::{TypeKey, TypeTable};

/// A `fn` type's parameter and return types. A `fn` value passes where a
/// `fn mut`, or one with other effects, is expected, so only the shape says
/// which closures a value of the type can be.
type FnShape = (Vec<TypeKey>, TypeKey);

/// The types the closures of each shape capture: what a `fn` value of that
/// shape can hold.
#[derive(Default)]
pub struct ClosureCaptures(IndexMap<FnShape, Vec<TypeId>>);

impl ClosureCaptures {
    #[must_use]
    pub fn collect(flat: &FlatPackage) -> Self {
        struct Scan<'a> {
            type_table: &'a TypeTable,
            captures: IndexMap<FnShape, Vec<TypeId>>,
        }
        impl TirRefVisitor for Scan<'_> {
            fn visit_expr(&mut self, expr: &TirExpr) {
                if let TirExprKind::Closure { captures, .. } = &expr.kind {
                    let shape = fn_shape(expr.type_id, self.type_table)
                        .expect("a closure is of a `fn` type");
                    let types = self.captures.entry(shape).or_default();
                    for capture in captures {
                        if !types.contains(&capture.type_id) {
                            types.push(capture.type_id);
                        }
                    }
                }
                self.walk_expr(expr);
            }
        }
        let type_table = flat.type_table.borrow();
        let mut scan = Scan {
            type_table: &type_table,
            captures: IndexMap::default(),
        };
        for func in &flat.functions {
            if let Some(body) = &func.borrow().body {
                scan.visit_block(body);
            }
        }
        ClosureCaptures(scan.captures)
    }

    /// The types a `fn` value of type `ty` can hold.
    #[must_use]
    pub fn of(&self, ty: TypeId, type_table: &TypeTable) -> &[TypeId] {
        fn_shape(ty, type_table)
            .and_then(|shape| self.0.get(&shape))
            .map_or(&[], Vec::as_slice)
    }
}

/// The shape of `ty`, past any newtype over it, `None` for a type that is no
/// `fn`.
fn fn_shape(ty: TypeId, type_table: &TypeTable) -> Option<FnShape> {
    match type_table.get(ty) {
        ResolvedType::Newtype { base_type, .. } => fn_shape(*base_type, type_table),
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => Some((
            type_table.type_keys(params),
            type_table.type_key(*return_type),
        )),
        _ => None,
    }
}

/// One kind of storage: a value of a type, or the backing array of elements of
/// one, which a `List` holds without naming its type.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Storage {
    Of(TypeKey),
    ArrayOf(TypeKey),
}

pub struct StorageReach<'a> {
    type_table: &'a TypeTable,
    fields: &'a StructFields,
    captures: &'a ClosureCaptures,
}

impl<'a> StorageReach<'a> {
    #[must_use]
    pub fn new(
        type_table: &'a TypeTable,
        fields: &'a StructFields,
        captures: &'a ClosureCaptures,
    ) -> Self {
        Self {
            type_table,
            fields,
            captures,
        }
    }

    /// Whether a `holder` can reach storage a `target` value owns. A fresh
    /// holder (`by_value` false) owns all of its own storage, so only a
    /// reference it carries can lead into the target's. A reference target
    /// stands for the storage at the end of its chain: a callee handed
    /// `&&List` (a `&self` on `&List`) derefs down to the list.
    #[must_use]
    pub fn reaches(&self, holder: TypeId, target: TypeId, by_value: bool) -> bool {
        let mut target = target;
        while let Members::Reference(referent) = self.members(target) {
            target = referent;
        }
        self.storage(target, false)
            .is_none_or(|owned| self.walk(holder, &owned, by_value, &mut IndexSet::default()))
    }

    /// Whether a `holder` can reach storage a `target` value leads to, through
    /// the references it carries as well as what it owns: an iterator's `next`
    /// hands back an element of the list the iterator references.
    #[must_use]
    pub fn reaches_through(&self, holder: TypeId, target: TypeId, by_value: bool) -> bool {
        self.storage(target, true)
            .is_none_or(|led_to| self.walk(holder, &led_to, by_value, &mut IndexSet::default()))
    }

    /// The storage a `ty` value owns: itself, and what it holds by value, and
    /// with `through_refs` what its references lead to as well. `None` where a
    /// type cannot be read.
    fn storage(&self, ty: TypeId, through_refs: bool) -> Option<IndexSet<Storage>> {
        let mut storage = IndexSet::default();
        self.collect(ty, through_refs, &mut storage)
            .then_some(storage)
    }

    fn collect(&self, ty: TypeId, through_refs: bool, out: &mut IndexSet<Storage>) -> bool {
        match self.members(ty) {
            Members::Reference(referent) => !through_refs || self.collect(referent, true, out),
            Members::Unknown => false,
            Members::Held(members) => {
                let storage = self.storage_of(ty);
                if storage.first().is_some_and(|s| out.contains(s)) {
                    return true;
                }
                out.extend(storage);
                members
                    .into_iter()
                    .all(|m| self.collect(m, through_refs, out))
            }
        }
    }

    fn walk(
        &self,
        ty: TypeId,
        owned: &IndexSet<Storage>,
        by_value: bool,
        open: &mut IndexSet<(TypeKey, bool)>,
    ) -> bool {
        if !open.insert((self.type_table.type_key(ty), by_value)) {
            return false;
        }
        match self.members(ty) {
            // A reference may point at a target object of its referent's kind,
            // and leads on through the references that object carries. What the
            // object owns by value is its own, not the target's: one storage
            // has one owner, and a sharing the walk made it is the alias chains'
            // to answer, as for a fresh holder.
            Members::Reference(referent) => {
                self.storage_of(referent).iter().any(|s| owned.contains(s))
                    || self.walk(referent, owned, false, open)
            }
            Members::Unknown => true,
            Members::Held(members) => {
                (by_value && self.storage_of(ty).iter().any(|s| owned.contains(s)))
                    || members
                        .into_iter()
                        .any(|m| self.walk(m, owned, by_value, open))
            }
        }
    }

    /// The storage a value of `ty` is, by itself: none for a scalar, which
    /// copies on every read.
    fn storage_of(&self, ty: TypeId) -> Vec<Storage> {
        let tt = self.type_table;
        if matches!(
            tt.get(ty),
            ResolvedType::Primitive(_)
                | ResolvedType::Unit
                | ResolvedType::Never
                | ResolvedType::Enum { .. }
                | ResolvedType::Flags { .. }
                | ResolvedType::Resource { .. }
                | ResolvedType::GenericResource { .. }
        ) {
            return Vec::new();
        }
        let mut out = vec![Storage::Of(tt.type_key(ty))];
        if let Some(element) = self.array_element(ty) {
            out.push(Storage::ArrayOf(tt.type_key(element)));
        }
        out
    }

    /// The element of the backing array `ty` is or holds: a builtin array, or a
    /// `List`.
    fn array_element(&self, ty: TypeId) -> Option<TypeId> {
        match self.type_table.get(ty) {
            ResolvedType::BuiltinArray(inner) => Some(*inner),
            _ => self.type_table.list_element(ty),
        }
    }

    fn members(&self, ty: TypeId) -> Members {
        let tt = self.type_table;
        if let Some(payload) = tt.box_payload_of(ty) {
            return Members::Reference(payload);
        }
        if let Some(element) = tt.list_element(ty) {
            return Members::Held(vec![element]);
        }
        match tt.get(ty) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Members::Reference(*inner),
            ResolvedType::BuiltinArray(inner) => Members::Held(vec![*inner]),
            ResolvedType::Newtype { base_type, .. } => Members::Held(vec![*base_type]),
            ResolvedType::Struct { def, type_args } => {
                match self.fields.get(&(*def, type_args.clone())) {
                    Some(fields) => Members::Held(fields.iter().map(|f| f.type_id).collect()),
                    None => Members::Unknown,
                }
            }
            ResolvedType::GenericInstance { def, type_args } => {
                match tt.variant_template_cases(*def) {
                    Some(cases) => Members::Held(
                        cases
                            .iter()
                            .map(|&(_, _, payload)| match tt.get(payload) {
                                ResolvedType::TypeParam { index, .. } => type_args[*index as usize],
                                _ => payload,
                            })
                            .collect(),
                    ),
                    None => Members::Held(type_args.clone()),
                }
            }
            ResolvedType::Variant { def } => match tt.variant_template_cases(*def) {
                Some(cases) => {
                    Members::Held(cases.iter().map(|&(_, _, payload)| payload).collect())
                }
                None => Members::Unknown,
            },
            // A closure holds what it captured, which its type does not name:
            // whatever any closure of its shape captured.
            ResolvedType::Function { .. } => Members::Held(self.captures.of(ty, tt).to_vec()),
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. } => Members::Held(Vec::new()),
            ResolvedType::TypeParam { .. }
            | ResolvedType::AssocParam { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::Unknown
            | ResolvedType::Error => Members::Unknown,
        }
    }
}

enum Members {
    /// A reference, to this referent.
    Reference(TypeId),
    /// Held by value.
    Held(Vec<TypeId>),
    /// Nothing this walk can read.
    Unknown,
}
