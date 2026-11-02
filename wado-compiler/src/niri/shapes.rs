//! What writing an aggregate back as a literal needs and a [`Value`] does not
//! carry: a struct's field names and types, and a variant case's index and
//! payload type. Reading a literal into a value drops both.
//!
//! [`Value`]: crate::const_eval::Value

use crate::defs::DefId;
use crate::hashmap::IndexMap;
use crate::nir::{StructInstance, struct_instance};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId};
use crate::type_table::{TypeKey, TypeTable};

/// A struct field as a literal spells it.
pub struct FieldShape {
    pub name: String,
    pub type_id: TypeId,
}

/// A variant case as a `VariantConstruct` spells it.
pub struct CaseShape {
    pub index: u32,
    pub payload: TypeId,
}

/// A variant type as its declaration and argument types.
type VariantInstance = (DefId, Vec<TypeKey>);

fn variant_instance(type_id: TypeId, type_table: &TypeTable) -> Option<VariantInstance> {
    match type_table.get(type_id) {
        ResolvedType::Variant { def } => Some((*def, Vec::new())),
        ResolvedType::GenericInstance { def, type_args } => {
            Some((*def, type_table.type_keys(type_args)))
        }
        _ => None,
    }
}

/// Declaration shapes by type, built once per pass. An absent entry costs the
/// fold that would have written that type, never correctness.
#[derive(Default)]
pub struct AggregateShapes {
    /// Fields in `field_index` order.
    structs: IndexMap<StructInstance, Vec<FieldShape>>,
    /// Cases in declaration order, payloads at the instance's arguments.
    cases: IndexMap<VariantInstance, Vec<(String, CaseShape)>>,
}

impl AggregateShapes {
    /// Index `project`'s concrete struct declarations, and every variant type
    /// the type table interns. Substituting a generic variant's payloads
    /// interns them, so this borrows the type table for writing.
    #[must_use]
    pub fn of(project: &NirPackage) -> Self {
        let type_table = project.type_table.borrow();
        let mut structs = IndexMap::default();
        for decl in &project.structs {
            let Some(instance) = decl.instance(&type_table) else {
                continue;
            };
            structs.insert(
                instance,
                decl.fields_in_order()
                    .iter()
                    .map(|f| FieldShape {
                        name: f.name.clone(),
                        type_id: f.type_id,
                    })
                    .collect(),
            );
        }
        let mut instances: IndexMap<VariantInstance, Vec<TypeId>> = IndexMap::default();
        for (_, ty) in type_table.all_types() {
            let (def, type_args) = match ty {
                ResolvedType::Variant { def } => (*def, &[][..]),
                ResolvedType::GenericInstance { def, type_args } => (*def, type_args.as_slice()),
                _ => continue,
            };
            if project.find_variant(def).is_some() {
                instances
                    .entry((def, type_table.type_keys(type_args)))
                    .or_insert_with(|| type_args.to_vec());
            }
        }
        drop(type_table);
        let cases = instances
            .into_iter()
            .map(|(instance, type_args)| {
                let def = instance.0;
                let payloads = project
                    .variant_cases(def, &type_args)
                    .expect("only declared variants are collected");
                let declared = &project.find_variant(def).expect("declared").cases;
                let shapes = payloads
                    .into_iter()
                    .zip(declared)
                    .map(|((name, payload), case)| {
                        let index = case.index;
                        (name, CaseShape { index, payload })
                    })
                    .collect();
                (instance, shapes)
            })
            .collect();
        Self { structs, cases }
    }

    /// `type_id`'s fields in `field_index` order.
    #[must_use]
    pub fn fields(&self, type_id: TypeId, type_table: &TypeTable) -> Option<&[FieldShape]> {
        self.structs
            .get(&struct_instance(type_id, type_table)?)
            .map(Vec::as_slice)
    }

    /// The case at `case_index` in `type_id`, with the name that spells it.
    #[must_use]
    pub fn case_at(
        &self,
        type_id: TypeId,
        case_index: u32,
        type_table: &TypeTable,
    ) -> Option<(&str, &CaseShape)> {
        let cases = self.cases.get(&variant_instance(type_id, type_table)?)?;
        let (name, shape) = cases.get(case_index as usize)?;
        assert_eq!(
            shape.index, case_index,
            "variant case shapes are collected in declaration order"
        );
        Some((name.as_str(), shape))
    }
}
