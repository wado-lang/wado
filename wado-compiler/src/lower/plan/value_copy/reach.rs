//! Whether a value of one type can reach the storage a value of another owns,
//! read off the types alone: through a reference to a type that storage holds,
//! or, for a result that is not fresh, by holding such a type by value.
//!
//! A call whose retention puts a borrowed argument into its result answers
//! for every argument it might return. `rmsnorm(g, &x) -> List<&mut Value>`
//! returns a fresh list of references to values, and no `&List<…>` or
//! `&Array<…>`, so it holds none of `x`'s storage whatever its body does.

use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{ResolvedType, StructDef, TirField, TypeId, TypeKey, TypeTable};

/// One kind of storage: a value of a type, or the backing array of elements of
/// one, which a `List` holds without naming its type.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Storage {
    Of(TypeKey),
    ArrayOf(TypeKey),
}

pub struct StorageReach<'a> {
    type_table: &'a TypeTable,
    fields: &'a IndexMap<(StructDef, Vec<TypeId>), Vec<TirField>>,
}

impl<'a> StorageReach<'a> {
    #[must_use]
    pub fn new(
        type_table: &'a TypeTable,
        fields: &'a IndexMap<(StructDef, Vec<TypeId>), Vec<TirField>>,
    ) -> Self {
        Self { type_table, fields }
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
        let Some(owned) = self.owned(target) else {
            return true;
        };
        self.walk(holder, &owned, by_value, &mut IndexSet::default())
    }

    /// The storage a `ty` value owns: itself, and what it holds by value, not
    /// past a reference. `None` where a type cannot be read.
    fn owned(&self, ty: TypeId) -> Option<IndexSet<Storage>> {
        let mut owned = IndexSet::default();
        self.collect_owned(ty, &mut owned).then_some(owned)
    }

    fn collect_owned(&self, ty: TypeId, owned: &mut IndexSet<Storage>) -> bool {
        match self.members(ty) {
            Members::Reference(_) => true,
            Members::Unknown => false,
            Members::Held(members) => {
                let storage = self.storage_of(ty);
                if storage.first().is_some_and(|s| owned.contains(s)) {
                    return true;
                }
                owned.extend(storage);
                members.into_iter().all(|m| self.collect_owned(m, owned))
            }
        }
    }

    fn walk(
        &self,
        ty: TypeId,
        owned: &IndexSet<Storage>,
        by_value: bool,
        open: &mut IndexSet<TypeKey>,
    ) -> bool {
        let key = self.type_table.type_key(ty);
        if !open.insert(key) {
            return false;
        }
        match self.members(ty) {
            // A reference to anything holding the target's storage reaches it,
            // and so does one this walk cannot read.
            Members::Reference(referent) => self
                .owned(referent)
                .is_none_or(|held| held.iter().any(|s| owned.contains(s))),
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
                | ResolvedType::Function { .. }
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
            ResolvedType::Function { .. }
            | ResolvedType::Primitive(_)
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
