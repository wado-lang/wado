//! What writing an aggregate back as a literal needs and a [`Value`] does not
//! carry: a struct's field names and types, and a variant case's index and
//! payload type. Reading a literal into a value drops both.
//!
//! [`Value`]: crate::const_eval::Value

use crate::hashmap::IndexMap;
use crate::nir_package::NirPackage;
use crate::tir::{TypeId, TypeTable};

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

/// Declaration shapes by type, built once per pass. An absent entry costs the
/// fold that would have written that type, never correctness.
#[derive(Default)]
pub struct AggregateShapes {
    /// Fields in `field_index` order, which is how a struct literal lowers.
    structs: IndexMap<TypeId, Vec<FieldShape>>,
    cases: IndexMap<TypeId, IndexMap<String, CaseShape>>,
}

impl AggregateShapes {
    /// Index `project`'s struct and variant declarations by the type each one
    /// reifies. A declaration whose type is not interned is skipped: nothing
    /// names it, so nothing can ask for its shape.
    #[must_use]
    pub fn of(project: &NirPackage, type_table: &TypeTable) -> Self {
        let mut structs = IndexMap::default();
        for decl in &project.structs {
            let Some(type_id) = type_table.find_struct_by_name(&decl.name, &decl.module_source)
            else {
                continue;
            };
            let mut fields: Vec<&crate::nir::NirField> = decl.fields.iter().collect();
            fields.sort_by_key(|f| f.index);
            if fields.iter().enumerate().any(|(k, f)| f.index != k as u32) {
                continue;
            }
            structs.insert(
                type_id,
                fields
                    .into_iter()
                    .map(|f| FieldShape {
                        name: f.name.clone(),
                        type_id: f.type_id,
                    })
                    .collect(),
            );
        }
        let mut cases = IndexMap::default();
        for decl in &project.variants {
            let Some(type_id) = type_table.find_decl_type_by_name(&decl.name, &decl.module_source)
            else {
                continue;
            };
            cases.insert(
                type_id,
                decl.cases
                    .iter()
                    .map(|c| {
                        (
                            c.name.clone(),
                            CaseShape {
                                index: c.index,
                                payload: c.payload,
                            },
                        )
                    })
                    .collect(),
            );
        }
        Self { structs, cases }
    }

    /// `type_id`'s fields in `field_index` order.
    #[must_use]
    pub fn fields(&self, type_id: TypeId) -> Option<&[FieldShape]> {
        self.structs.get(&type_id).map(Vec::as_slice)
    }

    /// The case `case_name` names in `type_id`.
    #[must_use]
    pub fn case(&self, type_id: TypeId, case_name: &str) -> Option<&CaseShape> {
        self.cases.get(&type_id)?.get(case_name)
    }
}
