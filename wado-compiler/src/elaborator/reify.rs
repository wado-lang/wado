//! Reify — AST + [`super::sem::ModuleSemantics`] → [`crate::tir::TirModule`].
//!
//! Introduced by Stage 5 of [`wep-2026-05-26-elaborator-rearchitecture.md`].
//! The reify pass is the mechanical half of the annotate/reify split:
//! every TIR-shaping decision is already recorded on `ModuleSemantics`
//! during `annotate_bodies`; this walker reads those annotations and emits
//! the corresponding TIR nodes. It never re-runs type inference, name
//! resolution, or method dispatch.
//!
//! # Surface
//!
//! `reify_module(module, tysys, sem, …) → TirModule` mirrors
//! [`super::Elaborator::resolve_module`]'s per-Item dispatch shape. Each
//! `Item::*` arm calls a `reify_*` helper; decl-only items (`Enum`,
//! `Flags`, `Newtype`, `Variant`, `Effect`, `Resource`, `Struct`) read
//! decl-interned types from `TypeSystem.all_*` and produce TIR without
//! consulting `TypeAnnotations`; function / impl-method / test / global
//! bodies build a fresh [`super::types::FunctionContext`] and walk the
//! AST via `reify_block` / `reify_stmt` / `reify_expr` / `reify_pattern`.
//!
//! # What reify reads
//!
//! - `ModuleSemantics.types.expression_types[id]` → `TirExpr::type_id`
//! - `ModuleSemantics.types.method_dispatch[id]` → dispatch target +
//!   `self_kind` + `is_ref_impl`
//! - `ModuleSemantics.types.coercions[id]` → coercion wrapper to emit
//!   around the raw expression
//! - `ModuleSemantics.types.desugars[id]` → which expansion path to take
//!   (assert / matches / for-of / while / compound-assign / comparison
//!   chain / IndexMut method call / newtype-from collapse)
//! - `ModuleSemantics.types.generic_instantiations[id]` → `type_args`
//!   for call / struct / variant constructions
//! - `ModuleSemantics.types.closure_captures[id]` → closure capture list
//!   and `__ref_*` materialisation
//! - `ModuleSemantics.types.assert_captures[id]` → assert slot map
//! - `ModuleSemantics.types.for_of_iterator[id]` → for-of iterator
//!   dispatch target
//! - `ModuleSemantics.bindings.references` / `bindings.local_symbols` →
//!   identifier resolution
//! - `ModuleSemantics.imports.*` → name lookups
//! - `ModuleSemantics.decls.*` → function return types, generic
//!   parameter tables, anonymous-struct registry
//!
//! # What reify mutates
//!
//! Reify takes `&mut TypeSystem` because monomorphic struct / variant
//! instances reached for the first time at reify time must intern through
//! the shared [`crate::tir::TypeTable`]. Reify does **not** mutate
//! `TraitEnv` or any impl tables — those are read-only inputs.
//!
//! # `FunctionContext` walk-order invariant
//!
//! Reify maintains its own [`super::types::FunctionContext`] per function
//! body. Local indices, capture indices, and synthetic-local counters
//! (`next_assert_id`, `next_loop_id`, the `__ref_*` ordering) must match
//! what `annotate_bodies` produced. The contract is documented at
//! [`wep-2026-05-26-elaborator-rearchitecture.md`] §`Design notes (Stage
//! 5)` →`Gap 7: per-function local-frame walk-order invariant`. The unit
//! test contract: for every function `f`, the `Vec<TirLocal>` annotate
//! emitted equals the `Vec<TirLocal>` reify emits.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{self, Item, Module};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::symbol::SymbolTable;
use crate::tir::{
    self as tir, TirBlock, TirEnum, TirEnumCase, TirExpr, TirFlags, TirFlagsMember, TirFunction,
    TirGlobal, TirModule, TirNewtype, TirPattern, TirStmt, TirStruct, TirTest, TirVariantDecl,
    TypeId,
};

use super::sem::ModuleSemantics;
use super::types::{FunctionContext, TypeLookup};
use super::tysys::TypeSystem;

/// Per-module reify pass. One instance per loaded module the batch driver
/// emits TIR for.
///
/// Construction is via [`Reify::new`]; the driver hands in the shared
/// `TypeSystem` (cloned by shallow Rc/Arc copy), the read-only
/// `ModuleSemantics` for this module, and the per-compile context the
/// elaborator already threads through.
///
/// `#[allow(dead_code)]` on the struct + every method is intentional
/// until [`crate::elaborator::orchestration`] wires the
/// `annotate_bodies → reify_modules` pipeline split (the second half of
/// Stage 5). The skeleton lands first so the membership contract is
/// reviewable; the call site flips later in the same WEP migration.
#[allow(dead_code)]
pub(crate) struct Reify<'a, H: CompilerHost> {
    /// Pipeline-wide type knowledge. `&mut` only because reify may
    /// intern new monomorphic instances; the trait/impl tables are
    /// treated as read-only per the WEP `Reify surface` contract.
    pub(crate) tysys: TypeSystem,
    /// Per-module semantic facts produced by `annotate_bodies`. Read
    /// only — reify never mutates the recorded decisions.
    pub(crate) sem: &'a ModuleSemantics,
    /// Symbol table from analyzer (cross-module).
    #[allow(dead_code)]
    pub(crate) symbols: &'a SymbolTable,
    /// All loaded modules. Used by cross-module lookups (e.g. resolving
    /// the AST of a function referenced by a `FunctionRef`).
    #[allow(dead_code)]
    pub(crate) loaded_modules: &'a IndexMap<ModuleSource, Module>,
    /// Diagnostics logger.
    pub(crate) logger: &'a Logger<'a, H>,
    /// Source module currently being reified.
    pub(crate) current_module_source: ModuleSource,
    /// Items of the current module, set before per-Item dispatch.
    pub(crate) current_module_items: &'a [Item],
    /// `ModuleSource` interner. Shared with annotate so cross-pass
    /// references resolve to the same `ModuleSource` identity.
    #[allow(dead_code)]
    pub(crate) interner: Rc<RefCell<ModuleSourceInterner>>,
    /// Kiln invocation redirects. Consulted by `use`-resolution paths
    /// the same way annotate consults them.
    #[allow(dead_code)]
    pub(crate) invocations: Rc<crate::kiln::InvocationIndex>,
    /// Entry module, used for cross-module import dedup.
    #[allow(dead_code)]
    pub(crate) entry_module_source: ModuleSource,
}

#[allow(dead_code)]
impl<'a, H: CompilerHost> Reify<'a, H> {
    /// Build a [`TypeLookup`] view over the current module's import
    /// context and the shared `all_*` tables. Used by `reify_*` helpers
    /// that need to resolve AST `Type` nodes (e.g. type-param defaults,
    /// resource method param/return types) the same way the elaborator
    /// did during annotate — but without the elaborator's
    /// `record_type_name_reference` side-effect (use→def edges were
    /// already recorded by annotate and live on
    /// [`ModuleSemantics::bindings`]).
    fn type_lookup(&self) -> TypeLookup<'_> {
        TypeLookup {
            current_module_source: &self.current_module_source,
            imported_type_sources: &self.sem.imports.imported_type_sources,
            import_original_names: &self.sem.imports.import_original_names,
            all_newtypes: &self.tysys.all_newtypes,
            all_struct_fields: &self.tysys.all_struct_fields,
            all_variant_cases: &self.tysys.all_variant_cases,
            all_enum_cases: &self.tysys.all_enum_cases,
            all_flags_cases: &self.tysys.all_flags_cases,
            all_resource_types: &self.tysys.all_resource_types,
            all_generic_newtypes: &self.tysys.all_generic_newtypes,
            local_struct_fields: &self.sem.decls.local_struct_fields,
            local_newtypes: &self.sem.decls.local_newtypes,
            local_enum_cases: &self.sem.decls.local_enum_cases,
            local_flags_cases: &self.sem.decls.local_flags_cases,
            local_generic_newtypes: &self.sem.decls.local_generic_newtypes,
            local_variant_cases: &self.sem.decls.local_variant_cases,
        }
    }

    /// Resolve an AST [`ast::Type`] to a [`TypeId`] without recording
    /// any use→def edge. Reify uses this for type-level resolutions
    /// (type-param defaults, resource method params, …) — annotate
    /// already recorded the edges during its body walk.
    ///
    /// Delegates to the existing
    /// [`super::Elaborator::resolve_type_static`] helper, which is
    /// host-agnostic and operates over the [`TypeLookup`] view above.
    fn resolve_type(&mut self, ty: &ast::Type) -> TypeId {
        let lookup = self.type_lookup();
        super::Elaborator::<H>::resolve_type_static(
            ty,
            &mut self.tysys.type_table.borrow_mut(),
            &lookup,
        )
    }

    /// Like [`Self::resolve_type`] but with an explicit type-parameter
    /// scope so `T`/`U` in a generic decl's method signature resolve to
    /// the right `TypeParam` slot. Used by `reify_effect_decl` /
    /// `reify_resource_decl` for method params and return types, and by
    /// `reify_variant_decl` for the (unused-here) payload path.
    fn resolve_type_in_scope(&mut self, ty: &ast::Type, type_params: &[String]) -> TypeId {
        let lookup = self.type_lookup();
        super::Elaborator::<H>::resolve_type_static_with_params(
            ty,
            &mut self.tysys.type_table.borrow_mut(),
            &lookup,
            type_params,
        )
    }

    /// Reify one module: emit a [`TirModule`] from the AST + the
    /// `ModuleSemantics` `annotate_bodies` populated.
    ///
    /// The flow mirrors [`super::Elaborator::resolve_module`] item by
    /// item; only the per-Item dispatch shape is reproduced here, the
    /// body of each branch is delegated to a `reify_*` helper.
    pub(crate) fn reify_module(
        &mut self,
        module: &'a Module,
        module_source: ModuleSource,
    ) -> Result<TirModule, Bail> {
        self.current_module_source = module_source.clone();
        self.current_module_items = &module.items;

        let mut tir_module = TirModule::new(module_source);

        for item in &module.items {
            match item {
                Item::Function(func) => {
                    if let Some(tir_func) = self.reify_function(func) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Struct(struct_decl) => {
                    tir_module.add_struct(self.reify_struct(struct_decl));
                }
                Item::Impl(impl_block) => {
                    for tir_func in self.reify_impl(impl_block) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Trait(_) => {
                    // Trait declarations don't lower to TIR; the elaborator
                    // already registered the signature on `TraitEnv`.
                }
                Item::Variant(variant_decl) => {
                    tir_module
                        .variants
                        .push(self.reify_variant_decl(variant_decl));
                }
                Item::Test(test_decl) => {
                    let test_index = tir_module.tests.len();
                    let module_is_todo = module.has_todo();
                    if let Some((tir_func, tir_test)) =
                        self.reify_test_decl(test_decl, test_index, module_is_todo)
                    {
                        tir_module.add_function(tir_func);
                        tir_module.tests.push(tir_test);
                    }
                }
                Item::Global(global_decl) => {
                    if let Some(tir_global) = self.reify_global(global_decl) {
                        tir_module.globals.push(tir_global);
                    }
                }
                Item::Enum(enum_decl) => {
                    tir_module.add_enum(self.reify_enum(enum_decl));
                }
                Item::Flags(flags_decl) => {
                    if let Some(tir_flags) = self.reify_flags(flags_decl) {
                        tir_module.add_flags(tir_flags);
                    }
                }
                Item::Newtype(newtype_decl) => {
                    if let Some(tir_newtype) = self.reify_newtype(newtype_decl) {
                        tir_module.add_newtype(tir_newtype);
                    }
                }
                Item::Interface(effect_decl) => {
                    tir_module.add_effect(self.reify_effect_decl(effect_decl));
                }
                Item::Resource(resource_decl) => {
                    tir_module.add_resource(self.reify_resource_decl(resource_decl));
                }
                _ => {}
            }
        }

        // Share the type table via Rc::clone so downstream phases see
        // the same arena reify just interned into.
        tir_module.type_table = Rc::clone(&self.tysys.type_table);

        if let Some(data) = module.data_section() {
            tir_module = tir_module.with_data_section(Some(data.to_string()));
        }

        // Anonymous structs synthesised during body resolution by
        // `annotate_bodies` live on `sem.decls.pending_anonymous_structs`.
        // Reify clones them into the emitted module rather than draining,
        // because `sem` is `&` here.
        for anon_struct in &self.sem.decls.pending_anonymous_structs {
            tir_module.add_struct(anon_struct.clone());
        }

        tir_module.wasm_module = module.wasm_module().map(String::from);

        self.logger.ok_or_bail(tir_module)
    }

    // ─────────────────────────────────────────────────────────────────
    // Decl-only items: read from `tysys.all_*` and produce TIR without
    // consulting `TypeAnnotations`.
    // ─────────────────────────────────────────────────────────────────

    /// Reify an `enum E { … }` declaration. Pure projection from the
    /// AST shape; cases keep their declared index.
    fn reify_enum(&self, enum_decl: &ast::EnumDecl) -> TirEnum {
        TirEnum {
            name: enum_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: enum_decl.is_pub,
            type_params: Vec::new(),
            monomorph_info: None,
            cases: enum_decl
                .cases
                .iter()
                .enumerate()
                .map(|(i, case)| TirEnumCase {
                    name: case.name.clone(),
                    index: i as u32,
                    span: case.span,
                })
                .collect(),
            span: enum_decl.span,
        }
    }

    /// Reify a `flags F { … }` declaration. The `TypeId` is the one
    /// `annotate_decls` interned via `make_flags`; reify reads it from
    /// `tysys.all_flags_cases`.
    fn reify_flags(&self, flags_decl: &ast::FlagsDecl) -> Option<TirFlags> {
        let info = self
            .tysys
            .all_flags_cases
            .get(&self.current_module_source)?
            .get(&flags_decl.name)?;
        Some(TirFlags {
            name: flags_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: flags_decl.is_pub,
            type_id: info.type_id,
            members: flags_decl
                .flags
                .iter()
                .enumerate()
                .map(|(i, m)| TirFlagsMember {
                    name: m.name.clone(),
                    bitmask: 1u32 << i,
                    span: m.span,
                })
                .collect(),
            span: flags_decl.span,
        })
    }

    /// Reify a `newtype N = T;` declaration (concrete only). Generic
    /// newtypes are instantiated on demand by `make_newtype_instance`;
    /// the concrete decl shape is what lands in TIR.
    fn reify_newtype(&self, newtype_decl: &ast::Newtype) -> Option<TirNewtype> {
        if !newtype_decl.type_params.is_empty() {
            // Generic newtypes have no concrete TIR decl emitted at the
            // module level; they materialise per-instantiation.
            return None;
        }
        let type_id = *self
            .tysys
            .all_newtypes
            .get(&self.current_module_source)?
            .get(&newtype_decl.name)?;
        Some(TirNewtype {
            name: newtype_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: newtype_decl.is_pub,
            type_id,
            span: newtype_decl.span,
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with sub-declaration walks but no function bodies.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a `struct S { … }`. Field types come from
    /// `tysys.all_struct_fields`; field-default expressions and
    /// type-param defaults are read from `ModuleSemantics`.
    fn reify_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        // Field types are decl-resolved during annotate and live in
        // `tysys.all_struct_fields`. Read them directly rather than
        // re-running `resolve_type` over `field.ty`.
        let field_info = self
            .tysys
            .all_struct_fields
            .get(&self.current_module_source)
            .and_then(|m| m.get(&struct_decl.name));

        let mut fields = Vec::with_capacity(struct_decl.fields.len());
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = field_info
                .and_then(|info| info.fields.get(index).map(|(_, t, _)| *t))
                .unwrap_or(crate::tir::TypeTable::UNKNOWN);

            let serde_rename = field.attrs.iter().find_map(|a| {
                if a.name == "serde" {
                    a.kv_value("rename").map(str::to_string)
                } else {
                    None
                }
            });

            // Stage 5 follow-up: field defaults are AST `Expr`s that
            // need a full body-walk reify (a per-struct
            // `FunctionContext` keyed `struct:<name>`, just as
            // `Elaborator::resolve_struct` builds at item.rs:461). When
            // `reify_expr` lands, replace the `None` below with
            // `field.default.as_ref().map(|e| Box::new(self.reify_expr(e, …)))`.
            let default_expr: Option<Box<TirExpr>> =
                if field.default.is_some() { None } else { None };

            let serde_default = field.default.is_some()
                || field
                    .attrs
                    .iter()
                    .any(|a| a.name == "serde" && a.has_arg("default"));

            fields.push(crate::tir::TirField {
                name: field.name.clone(),
                is_pub: field.is_pub,
                type_id,
                index: index as u32,
                span: field.span,
                is_hidden: field.attrs.iter().any(|a| a.name == "hidden"),
                serde_rename,
                serde_default,
                default_expr,
            });
        }

        // Type-param defaults are AST `Type`s — resolve via the
        // import-aware [`TypeLookup`] view (no use→def re-recording).
        // The base type-param `TypeId`s themselves were interned at
        // annotate time and are cached on `field_info.type_param_type_ids`,
        // but `TirTypeParam` only needs the `default: Option<TypeId>`,
        // so resolve_type each default directly.
        let type_params: Vec<crate::tir::TirTypeParam> = struct_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        let serde_rename_all = struct_decl.attrs.iter().find_map(|a| {
            if a.name == "serde" {
                a.kv_value("rename_all").map(str::to_string)
            } else {
                None
            }
        });

        TirStruct {
            name: struct_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: struct_decl.is_pub,
            type_params,
            monomorph_info: None,
            fields,
            span: struct_decl.span,
            serde_rename_all,
        }
    }

    /// Reify a `variant V<T> { … }` declaration. Cases' payload types
    /// come from `tysys.all_variant_cases`; the type-param table is
    /// projected from the AST.
    fn reify_variant_decl(&mut self, variant_decl: &ast::VariantDecl) -> TirVariantDecl {
        let case_info = self
            .tysys
            .all_variant_cases
            .get(&self.current_module_source)
            .and_then(|m| m.get(&variant_decl.name));

        let cases: Vec<tir::TirVariantCase> = variant_decl
            .cases
            .iter()
            .enumerate()
            .map(|(index, case)| {
                let payload = case_info
                    .and_then(|info| info.cases.get(index).map(|c| c.payload))
                    .unwrap_or(crate::tir::TypeTable::UNIT);
                tir::TirVariantCase {
                    name: case.name.clone(),
                    index: index as u32,
                    payload,
                    span: case.span,
                }
            })
            .collect();

        let type_params: Vec<crate::tir::TirTypeParam> = variant_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        TirVariantDecl {
            name: variant_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: variant_decl.is_pub,
            type_params,
            cases,
            span: variant_decl.span,
        }
    }

    /// Reify an `interface E { … }` declaration. Effects have no
    /// `Self` type — `&self` / `&mut self` on an effect method is a
    /// surface error annotate already diagnosed.
    fn reify_effect_decl(&mut self, decl: &ast::InterfaceDecl) -> tir::TirEffect {
        let operations = self.reify_effect_ops(&[], &decl.methods, None);
        tir::TirEffect {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
        }
    }

    /// Reify a `resource R<T> { … }` declaration. Resource methods take
    /// a synthesised `self` parameter (`&Self` or `&mut Self`) at index
    /// 0; for generic resources `Self = GenericResource<…>` with the
    /// decl's own `TypeParam`s as type args.
    fn reify_resource_decl(&mut self, decl: &ast::ResourceDecl) -> tir::TirResource {
        let module_source = self.current_module_source.clone();
        let operations = self.reify_effect_ops(
            &decl.type_params,
            &decl.methods,
            Some((decl.name.as_str(), module_source)),
        );
        tir::TirResource {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
        }
    }

    /// Translation of `Elaborator::resolve_effect_ops` (item.rs:554–672).
    /// Decl-level method-list resolution — no body walk, no
    /// `TypeAnnotations` consumption. Type-param scope is established
    /// per call; the optional `resource_self` adds a synthesised
    /// receiver parameter for resource methods.
    fn reify_effect_ops(
        &mut self,
        type_params: &[ast::GenericParam],
        methods: &[ast::InterfaceMethod],
        resource_self: Option<(&str, ModuleSource)>,
    ) -> Vec<tir::TirEffectOp> {
        use crate::ast::SelfKind;

        let param_names: Vec<String> = type_params.iter().map(|p| p.name.clone()).collect();

        // Construct the resource's `Self` type after the param-name list
        // is known so a generic resource's `GenericResource` instance
        // references its own `TypeParam`s.
        let self_type: Option<TypeId> = resource_self.map(|(name, module)| {
            if type_params.iter().any(|p| !p.is_effect) {
                let type_arg_ids: Vec<TypeId> = type_params
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| !p.is_effect)
                    .map(|(i, p)| {
                        self.tysys
                            .type_table
                            .borrow_mut()
                            .make_type_param(p.name.clone(), i as u32)
                    })
                    .collect();
                self.tysys.type_table.borrow_mut().intern(
                    crate::tir::ResolvedType::GenericResource {
                        name: name.to_string(),
                        module_source: module,
                        type_args: type_arg_ids,
                    },
                )
            } else {
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_resource(name.to_string(), module)
            }
        });

        let mut ops = Vec::with_capacity(methods.len());
        for method in methods {
            let mut params = Vec::with_capacity(method.params.len());
            let mut next_local: u32 = 0;
            for p in &method.params {
                let type_id = match (p.self_kind, self_type) {
                    (SelfKind::None, _) => self.resolve_type_in_scope(&p.ty, &param_names),
                    (SelfKind::Ref, Some(self_t)) => {
                        self.tysys.type_table.borrow_mut().make_ref(self_t)
                    }
                    (SelfKind::MutRef, Some(self_t)) => {
                        self.tysys.type_table.borrow_mut().make_mut_ref(self_t)
                    }
                    _ => continue,
                };
                let name = if matches!(p.self_kind, SelfKind::None) {
                    p.name.clone()
                } else {
                    "self".to_string()
                };
                params.push(tir::TirParam {
                    name,
                    type_id,
                    local_index: next_local,
                    is_mut: p.is_mut,
                    default_expr: None,
                    span: p.span,
                });
                next_local += 1;
            }
            let return_type = method
                .return_type
                .as_ref()
                .map(|ty| self.resolve_type_in_scope(ty, &param_names))
                .unwrap_or(crate::tir::TypeTable::UNIT);
            let cm_name = method
                .attrs
                .iter()
                .find_map(crate::ast::Attribute::cm_identifier);
            ops.push(tir::TirEffectOp {
                name: method.name.clone(),
                params,
                return_type,
                span: method.span,
                cm_name,
            });
        }
        ops
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with function bodies — the bulk of reify_*.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a free function. Builds a fresh `FunctionContext`, walks
    /// params + body, and assembles `TirFunction`. All inference
    /// decisions are read from `sem.types`; reify only emits TIR.
    #[allow(unused_variables)]
    fn reify_function(&mut self, func: &ast::Function) -> Option<TirFunction> {
        // TODO(stage-5-bodies): mirror `Elaborator::resolve_function`.
        // Construct a `FunctionContext` with the function's return
        // type (read from `sem.decls.function_return_types[&func.name]`).
        // Add params via `ctx.add_local` in declaration order — this
        // pins the `FunctionContext::locals` walk-order invariant.
        // Walk the body via `reify_block`; emit `TirFunction`.
        todo!("reify_function: pending body-walk reify")
    }

    /// Reify every method (regular + synthesised default) on an `impl`
    /// block. Returns the resulting `TirFunction`s in the same order
    /// `Elaborator::resolve_module` emits them.
    #[allow(unused_variables)]
    fn reify_impl(&mut self, impl_block: &ast::ImplBlock) -> Vec<TirFunction> {
        // TODO(stage-5-bodies): mirror the `Item::Impl` arm of
        // `Elaborator::resolve_module` (elaborator.rs:1022–1238). Includes
        // synthesis-request handling, associated-type binding setup,
        // explicit + inferred type-param registration, regular-method
        // resolution, and the default-method synthesis pass.
        todo!("reify_impl: pending body-walk reify")
    }

    /// Reify a `test "…" { … }` block. Returns the synthesised
    /// `TirFunction` plus the `TirTest` metadata.
    #[allow(unused_variables)]
    fn reify_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
        module_is_todo: bool,
    ) -> Option<(TirFunction, TirTest)> {
        // TODO(stage-5-bodies): mirror `Elaborator::resolve_test_decl`
        // (item.rs:1233+).
        todo!("reify_test_decl: pending body-walk reify")
    }

    /// Reify a `global g: T = expr;` declaration.
    #[allow(unused_variables)]
    fn reify_global(&mut self, global_decl: &ast::GlobalDecl) -> Option<TirGlobal> {
        // TODO(stage-5-bodies): mirror `Elaborator::resolve_global`
        // (item.rs:700–733). Single-expression initializer in a minimal
        // `FunctionContext`; the type was already resolved by annotate
        // and is on `sem.decls.current_module_globals`.
        todo!("reify_global: pending body-walk reify")
    }

    // ─────────────────────────────────────────────────────────────────
    // Body walks: expressions, statements, blocks, patterns.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a block expression — walks each statement in order so
    /// `FunctionContext::locals` matches what annotate produced.
    #[allow(unused_variables)]
    pub(super) fn reify_block(
        &mut self,
        block: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirBlock {
        // TODO(stage-5-bodies): walk `block.stmts` via `reify_stmt`;
        // handle the trailing expression's type via
        // `sem.types.expression_types[trailing.id()]`.
        todo!("reify_block: pending body-walk reify")
    }

    /// Reify a statement. Dispatches on `Stmt::*`; `Let` adds a local
    /// (preserving walk-order), `For` / `While` / `Assert` consult
    /// `sem.types.desugars` to pick the right expansion path.
    #[allow(unused_variables)]
    pub(super) fn reify_stmt(
        &mut self,
        stmt: &ast::Stmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        // TODO(stage-5-bodies): mirror `Elaborator::resolve_stmt`. The
        // for-of / while / if-let / assert / compound-assign branches
        // consult `sem.types.desugars[stmt.id()]` to pick the recorded
        // expansion path; the iterator path reads
        // `sem.types.for_of_iterator`.
        todo!("reify_stmt: pending body-walk reify")
    }

    /// Reify an expression. Reads `sem.types.expression_types` for the
    /// type, `sem.types.coercions` for any coercion wrap,
    /// `sem.types.method_dispatch` for method calls,
    /// `sem.types.desugars` for desugar expansions, and
    /// `sem.types.generic_instantiations` for generic call /
    /// struct-literal / variant-ctor type args.
    #[allow(unused_variables)]
    pub(super) fn reify_expr(
        &mut self,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // TODO(stage-5-bodies): the longest method in this file. Mirrors
        // `Elaborator::resolve_expr`'s `Expr::*` dispatch but reads each
        // decision from `sem.types` instead of recomputing.
        //
        // For each arm:
        // - Look up `sem.types.expression_types[expr.id()]` for the
        //   resolved type.
        // - If `sem.types.coercions[expr.id()]` is Some, the underlying
        //   expression is built first and then wrapped per the recorded
        //   `CoercionKind`.
        // - If `sem.types.desugars[expr.id()]` is Some, follow the
        //   recorded desugar path (NewtypeFromCollapse / IndexMutMethodCall
        //   / Matches / ComparisonChain / …).
        // - Method calls read `sem.types.method_dispatch[expr.id()]` for
        //   the dispatch target and feed receiver adjustment from
        //   `self_kind` + `is_ref_impl`.
        // - Closures read `sem.types.closure_captures[expr.id()]` for the
        //   capture list.
        // - Generic call / struct / variant sites read
        //   `sem.types.generic_instantiations[expr.id()]` for type_args.
        todo!("reify_expr: pending body-walk reify")
    }

    /// Reify a pattern in a `let`, `match`, `if let`, or `while let`.
    /// Binding patterns add locals to `ctx` in the same order annotate
    /// did (per the walk-order invariant).
    #[allow(unused_variables)]
    pub(super) fn reify_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        // TODO(stage-5-bodies): mirror `Elaborator::resolve_if_pattern_inner`
        // and the pattern paths inside `resolve_match_expr`.
        todo!("reify_pattern: pending body-walk reify")
    }
}
