//! Type substitution for monomorphization: `substitute_type` replaces type
//! params with concrete types, and `collect_instantiation_sites` queues the
//! struct instances the pool holds.

use crate::defs::DefId;
use crate::hashmap::IndexMap;
use crate::tir::{
    InstantiationKey, ResolvedType, StructDef, TirPattern, TirStruct, TypeArgMap, TypeId,
    TypeParamId,
};
use crate::type_table::TypeTable;

use super::state::Monomorphizer;

impl Monomorphizer {
    /// Queue each concrete instance of a struct in `generic_structs` that the
    /// pool holds, and return the held instances of generic variants, whose
    /// payloads the drain lays out.
    pub(super) fn collect_instantiation_sites(
        &mut self,
        type_table: &TypeTable,
        generic_structs: &IndexMap<DefId, TirStruct>,
    ) -> Vec<TypeId> {
        let tested = &mut self.structs.sites_tested;
        let untested = generic_structs
            .keys()
            .filter(|def| tested.insert(**def))
            .count();
        // A site naming no generic struct yet may name one a later run adds.
        let mut ids = if untested == 0 {
            Vec::new()
        } else {
            std::mem::take(&mut self.structs.unmatched)
        };
        ids.append(&mut self.pool.instances);
        let mut variants = Vec::new();
        for id in ids {
            let (def, type_args) = type_table
                .instance_parts(id)
                .expect("the pool keeps only instances for the drain");
            if self.pool.generic_variants.contains_key(&def) {
                variants.push(id);
                continue;
            }
            if !generic_structs.contains_key(&def) {
                self.structs.unmatched.push(id);
                continue;
            }
            let all_concrete = type_args
                .iter()
                .all(|&arg| !type_table.contains_type_param(arg));
            if all_concrete {
                let key = InstantiationKey {
                    def: Some(def),
                    name: type_table.decl_name(def).to_string(),
                    module_source: type_table.def_module(def).clone(),
                    impl_type_args: type_args.to_vec(),
                    method_type_args: vec![],
                    method_info: None,
                    template: None,
                };
                let mangled = self.instantiation_name(&key, type_table);
                self.try_queue_struct(key, mangled);
            }
        }
        variants
    }

    /// Substitute type parameters in a type with concrete types, delegating the
    /// leaf cases to [`TypeTable::substitute_type_params`] and handling
    /// containers inline. A struct named with no arguments resolves through
    /// the substitution map itself, to an instance already interned.
    pub fn substitute_type(
        &self,
        type_id: TypeId,
        substitution: &TypeArgMap,
        type_table: &mut TypeTable,
    ) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type(elem, substitution, type_table);
                type_table.intern(ResolvedType::BuiltinArray(new_elem))
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_mut_ref(new_inner)
            }
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
            } => {
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute_type(p, substitution, type_table))
                    .collect();
                let new_return_type = self.substitute_type(return_type, substitution, type_table);
                type_table.make_function_with_mut(is_mut, new_params, new_return_type, effects)
            }
            // Only a param-embedding base is re-mangled; a concrete-base newtype
            // keeps its identity so its trait impls resolve. See wado-lang/wado#1626.
            // The head stays the declaration; only the arguments move, which
            // is why nothing here re-derives a head by truncating a spelling.
            ResolvedType::Newtype { def, base_type, .. }
                if type_table.contains_type_param(base_type) =>
            {
                let new_base = self.substitute_type(base_type, substitution, type_table);
                if new_base == base_type {
                    return type_id;
                }
                let new_args = type_table.generic_type_args(new_base).unwrap_or_default();
                type_table.intern(ResolvedType::Newtype {
                    def,
                    type_args: new_args,
                    base_type: new_base,
                })
            }
            // A pack among the elements splices what it stands for.
            ResolvedType::Tuple(elems) => {
                let mut new_elems: Vec<TypeId> = Vec::new();
                for &e in &elems {
                    let ResolvedType::TypePack {
                        id: pack,
                        name: pack_name,
                        mapped_elem,
                    } = type_table.get(e).clone()
                    else {
                        new_elems.push(self.substitute_type(e, substitution, type_table));
                        continue;
                    };
                    // A splice position spells the whole pack, so inside a
                    // variadic for-of body it reads the pack's own tuple rather
                    // than the element the loop binds (see
                    // `pack_splice_bindings`).
                    let pack_type = self
                        .pack_splice_bindings
                        .borrow()
                        .get(&pack)
                        .copied()
                        .or_else(|| substitution.get(&pack).copied())
                        .unwrap_or_else(|| {
                            panic!("TypePack `..{pack_name}` ({pack:?}) not found in substitution map")
                        });
                    match mapped_elem {
                        // Mapped pack: substitute `elem` once per source
                        // element with the pack param bound to it —
                        // `[..Case<T, P>]` yields `Case<T, P_k>`, a
                        // pack-independent `..F::method()` repeats `R`.
                        Some(elem) => {
                            let pack_elems = type_table.elem_types_or_self(pack_type);
                            // The element binding must not reach a splice
                            // position nested in `elem`: one still spells the
                            // whole pack, so it reads the pack's tuple, not `pe`.
                            let displaced = self.bind_pack_splice(pack, pack_type);
                            for pe in pack_elems {
                                let mut elem_substitution = substitution.clone();
                                elem_substitution.insert(pack, pe);
                                new_elems.push(self.substitute_type(
                                    elem,
                                    &elem_substitution,
                                    type_table,
                                ));
                            }
                            self.restore_pack_splice(pack, displaced);
                        }
                        None => new_elems.extend(type_table.elem_types_or_self(pack_type)),
                    }
                }
                type_table.make_tuple(new_elems)
            }
            ResolvedType::Struct {
                def: StructDef::Decl(def),
                type_args,
            } if type_args.is_empty() => {
                if substitution.is_empty() {
                    return type_id;
                }
                let mut indexed_args: Vec<(TypeParamId, TypeId)> =
                    substitution.iter().map(|(&param, &tid)| (param, tid)).collect();
                indexed_args.sort_by_key(|(param, _)| param.index);
                let args: Vec<TypeId> = indexed_args.into_iter().map(|(_, tid)| tid).collect();
                type_table
                    .find_generic_instance(def, &args)
                    .unwrap_or(type_id)
            }
            ResolvedType::Variant { type_args, .. } | ResolvedType::Resource { type_args, .. }
                if type_args.is_empty() =>
            {
                type_id
            }
            ResolvedType::Struct {
                def: StructDef::Decl(def),
                type_args,
            }
            | ResolvedType::Variant { def, type_args }
            | ResolvedType::Resource { def, type_args } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute_type(arg, substitution, type_table))
                    .collect();
                type_table.make_generic_instance(def, new_args)
            }
            // Delegate leaf cases (TypeParam, TypePack,
            // AssocTypeProjection, and other non-composite types) to the
            // shared implementation.
            _ => type_table.substitute_type_params(type_id, substitution),
        }
    }

    /// Substitute type parameters in a pattern
    pub fn substitute_types_in_pattern(
        &self,
        pattern: &mut TirPattern,
        substitution: &TypeArgMap,
        type_table: &mut TypeTable,
    ) {
        match pattern {
            TirPattern::Wildcard | TirPattern::Literal(_) | TirPattern::Range { .. } => {}
            TirPattern::Binding { type_id, .. }
            | TirPattern::Narrow { type_id, .. }
            | TirPattern::PerInstance {
                scrutinee_type: type_id,
                ..
            } => {
                // Substitute the binding's type (e.g., type parameter T -> i32)
                *type_id = self.substitute_type(*type_id, substitution, type_table);
            }
            TirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.substitute_types_in_pattern(p, substitution, type_table);
                }
            }
            TirPattern::Variant {
                enum_type,
                bindings,
                payload_type,
                ..
            } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
                // Also substitute the payload type (e.g., type parameter U -> i32)
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
                for binding in bindings {
                    self.substitute_types_in_pattern(binding, substitution, type_table);
                }
            }
            TirPattern::Enum { enum_type, .. } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
            }
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);
                for field in fields {
                    self.substitute_types_in_pattern(&mut field.pattern, substitution, type_table);
                }
            }
            TirPattern::Or(alternatives) => {
                for p in alternatives {
                    self.substitute_types_in_pattern(p, substitution, type_table);
                }
            }
            TirPattern::ConstantValue { expr } => {
                expr.type_id = self.substitute_type(expr.type_id, substitution, type_table);
            }
        }
    }
}
