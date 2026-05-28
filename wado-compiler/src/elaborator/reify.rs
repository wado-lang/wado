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
    ///
    /// Stage 5 staging: the function shape is implemented (return type
    /// from `sem.decls.function_return_types`, parameters added in
    /// declaration order to pin the walk-order invariant, body
    /// delegated to [`Self::reify_block`]). Generic-bound checking,
    /// effect-param scoping, the `<F: fn(…)>` bound realisation pass,
    /// and `extract_*` attribute helpers are left to a follow-up — they
    /// are pure projections that don't depend on the body walk, but
    /// would duplicate `Elaborator` helpers and balloon this file.
    fn reify_function(&mut self, func: &ast::Function) -> Option<TirFunction> {
        let return_type = self
            .sem
            .decls
            .function_return_types
            .get(&func.name)
            .copied()
            .unwrap_or(crate::tir::TypeTable::UNIT);

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(return_type);
        }

        let type_param_names: Vec<String> = func
            .type_params
            .iter()
            .filter(|p| !p.is_effect)
            .map(|p| p.name.clone())
            .collect();

        let mut params = Vec::with_capacity(func.params.len());
        for param in &func.params {
            let type_id = self.resolve_type_in_scope(&param.ty, &type_param_names);
            let default_expr = param.default.as_ref().map(|default_ast| {
                Box::new(self.reify_expr(default_ast, &mut ctx, Some(type_id)))
            });
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
            params.push(tir::TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                default_expr,
                span: param.span,
            });
        }

        let body = func
            .body
            .as_ref()
            .map(|b| self.reify_block(b, &mut ctx, None));

        let mut non_effect_non_fn_idx: u32 = 0;
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .filter_map(|p| {
                if p.is_effect {
                    return None;
                }
                if p.bounds.iter().any(|b| b.fn_signature.is_some()) {
                    return None;
                }
                let idx = non_effect_non_fn_idx;
                non_effect_non_fn_idx += 1;
                let default = p.default.as_ref().map(|ty| self.resolve_type(ty));
                Some(crate::tir::TirTypeParam {
                    name: p.name.clone(),
                    is_effect: p.is_effect,
                    is_pack: p.is_pack,
                    bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                    default,
                    index: idx,
                })
            })
            .collect();

        Some(TirFunction {
            module_source: ModuleSource::default(),
            name: func.name.clone(),
            is_pub: func.is_pub,
            is_export: func.is_export,
            is_async: func.is_async,
            type_params,
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params,
            return_type,
            task_return_type: if func.is_async { Some(return_type) } else { None },
            effects: vec![],
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: tir::InlineHint::Auto,
            compiler_item: None,
            export_name: None,
            allocator_tag: None,
            kind: tir::FunctionKind::Regular,
            return_abi: tir::ReturnAbi::Single,
        })
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

    /// Reify a `global g: T = expr;` declaration. The declared type
    /// was already resolved by annotate_decls and lives on
    /// `sem.decls.current_module_globals`; reify reads it back and
    /// walks the initializer through a minimal `FunctionContext`.
    /// `is_nullable` / `lazy_init` are populated by the lower phase
    /// (kept `false` here, matching `Elaborator::resolve_global`).
    fn reify_global(&mut self, global_decl: &ast::GlobalDecl) -> Option<TirGlobal> {
        let ty = self
            .sem
            .decls
            .current_module_globals
            .get(&global_decl.name)
            .map(|(t, _)| *t)
            .unwrap_or_else(|| self.resolve_type(&global_decl.ty));

        let mut ctx = FunctionContext::new(ty, format!("global:{}", global_decl.name));
        let initializer = self.reify_expr(&global_decl.initializer, &mut ctx, Some(ty));

        Some(TirGlobal {
            name: global_decl.name.clone(),
            ty,
            initializer,
            mutable: global_decl.mutable,
            wado_mutable: global_decl.mutable,
            is_pub: global_decl.is_pub,
            module_source: self.current_module_source.clone(),
            span: global_decl.span,
            is_nullable: false,
            lazy_init: false,
            locals: ctx.locals.clone(),
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Body walks: expressions, statements, blocks, patterns.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a block expression — walks each statement in order so
    /// `FunctionContext::locals` matches what annotate produced.
    pub(super) fn reify_block(
        &mut self,
        block: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirBlock {
        ctx.enter_scope();
        let len = block.stmts.len();
        let mut stmts = Vec::new();
        for (i, s) in block.stmts.iter().enumerate() {
            // Propagate expected type to the last expression-form
            // statement for coercion, mirroring
            // `Elaborator::resolve_block` (stmt.rs:31–69).
            if expected_type.is_some() && i == len - 1 {
                if let ast::Stmt::Expr(expr_stmt) = s {
                    let expr = self.reify_expr(&expr_stmt.expr, ctx, expected_type);
                    stmts.push(TirStmt::new(
                        crate::tir::TirStmtKind::Expr(expr),
                        expr_stmt.span,
                    ));
                    continue;
                }
            }
            stmts.extend(self.reify_stmt(s, ctx));
        }
        ctx.exit_scope();
        TirBlock::new(stmts, block.span)
    }

    /// Reify a statement. Dispatches on `Stmt::*`; `Let` adds a local
    /// (preserving walk-order), `For` / `While` / `Assert` consult
    /// `sem.types.desugars` to pick the right expansion path.
    pub(super) fn reify_stmt(
        &mut self,
        stmt: &ast::Stmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::TirStmtKind;
        match stmt {
            ast::Stmt::Expr(expr_stmt) => {
                let expr = self.reify_expr(&expr_stmt.expr, ctx, None);
                vec![TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span)]
            }
            ast::Stmt::Return(ret_stmt) => {
                let value = ret_stmt
                    .value
                    .as_ref()
                    .map(|e| self.reify_expr(e, ctx, Some(ctx.return_type)));
                vec![TirStmt::new(TirStmtKind::Return { value }, ret_stmt.span)]
            }
            ast::Stmt::TaskReturn(tr_stmt) => {
                let expected = ctx.task_return_type;
                let value = self.reify_expr(&tr_stmt.value, ctx, expected);
                vec![TirStmt::new(TirStmtKind::TaskReturn { value }, tr_stmt.span)]
            }
            ast::Stmt::Break(break_stmt) => vec![TirStmt::new(
                TirStmtKind::Break {
                    label: break_stmt.label.clone(),
                    value: break_stmt
                        .value
                        .as_ref()
                        .map(|e| self.reify_expr(e, ctx, None)),
                },
                break_stmt.span,
            )],
            ast::Stmt::Continue(continue_stmt) => {
                vec![TirStmt::new(TirStmtKind::Continue, continue_stmt.span)]
            }
            ast::Stmt::Let(let_stmt) => vec![self.reify_let(let_stmt, ctx)],
            ast::Stmt::If(_)
            | ast::Stmt::While(_)
            | ast::Stmt::For(_)
            | ast::Stmt::ForOf(_)
            | ast::Stmt::Loop(_)
            | ast::Stmt::Match(_)
            | ast::Stmt::Assert(_)
            | ast::Stmt::LabeledBlock(_) => {
                // TODO(stage-5-bodies): mirror the corresponding
                // `Elaborator::resolve_*` branches. `For` / `While` /
                // `Assert` reads `sem.types.desugars[stmt.id()]` to
                // pick the recorded expansion path; the iterator path
                // additionally reads `sem.types.for_of_iterator`.
                todo!("reify_stmt: {:?} variant pending body-walk reify", stmt)
            }
        }
    }

    /// Reify `let pat[: T] = expr;`. Currently handles the common
    /// `Ident` / `MutIdent` / `Wildcard` patterns; destructuring
    /// (`Tuple` / `Struct` / `Variant`) defers to the dispatcher's
    /// `todo!`.
    fn reify_let(&mut self, let_stmt: &ast::LetStmt, ctx: &mut FunctionContext) -> TirStmt {
        use crate::tir::{TirStmtKind, TypeTable};
        // Uninitialised `let x: T;` is rare and routes to its own
        // helper in the elaborator; reify follows suit.
        let Some(ast_value) = let_stmt.value.as_ref() else {
            todo!("reify_let: uninitialised `let x: T;` pending")
        };

        let annotated_type = let_stmt.ty.as_ref().map(|t| self.resolve_type(t));
        let value = self.reify_expr(ast_value, ctx, annotated_type);
        let type_id = annotated_type.unwrap_or(value.type_id);

        match &let_stmt.pattern {
            ast::Pattern::Ident { id, name, span: _ } => {
                let local_index = ctx.add_local(name.clone(), type_id, false, Some(*id));
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: false,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::MutIdent { id, name, span: _ } => {
                let local_index = ctx.add_local(name.clone(), type_id, true, Some(*id));
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: true,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Wildcard => {
                // `let _ = expr;` discards. Lower as an Expr stmt.
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
            _ => {
                let _ = type_id;
                let _ = TypeTable::UNKNOWN;
                // TODO(stage-5-bodies): tuple / struct / variant
                // destructuring (`resolve_let` calls
                // `resolve_destructure_pattern`).
                todo!("reify_let: destructuring pattern pending body-walk reify")
            }
        }
    }

    /// Reify an expression. Reads `sem.types.expression_types` for the
    /// type, `sem.types.coercions` for any coercion wrap,
    /// `sem.types.method_dispatch` for method calls,
    /// `sem.types.desugars` for desugar expansions, and
    /// `sem.types.generic_instantiations` for generic call /
    /// struct-literal / variant-ctor type args.
    pub(super) fn reify_expr(
        &mut self,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};

        // The expression's recorded type is the source of truth for
        // `TirExpr::type_id`. Falls back to `expected_type` (or
        // `UNKNOWN` when neither is available) for AST shapes that
        // evaporated during annotate (e.g. a stmt-position match
        // whose recorder fires only at the stmt level).
        let recorded_type = self
            .sem
            .types
            .expression_types
            .get(&expr.id())
            .copied()
            .or(expected_type)
            .unwrap_or(TypeTable::UNKNOWN);
        let span = expr.span();

        match expr {
            ast::Expr::Literal(lit) => self.reify_literal(lit, recorded_type, ctx),
            ast::Expr::Block(block) => {
                let block_tir = self.reify_block(block, ctx, expected_type);
                TirExpr::new(TirExprKind::Block(block_tir), recorded_type, span)
            }
            ast::Expr::Ident(ident) => self.reify_ident(ident, recorded_type, ctx),
            ast::Expr::TupleLiteral(tuple_lit) => {
                // Element-by-element walk; the recorded type is the
                // tuple `TypeId` annotate produced (potentially after
                // coercion to a sequence type — that path goes through
                // `coercions[id]` and is handled by the wrapper above
                // once it lands).
                let elements: Vec<TirExpr> = tuple_lit
                    .elements
                    .iter()
                    .map(|e| self.reify_expr(e, ctx, None))
                    .collect();
                TirExpr::new(
                    TirExprKind::TupleLiteral { elements },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::Cast(cast) => {
                // `expr as Ty` — emit `Cast` with the recorded target
                // type. Numeric vs newtype-cast handling is downstream;
                // reify just produces the shape.
                let inner = self.reify_expr(&cast.expr, ctx, None);
                let target_type = self.resolve_type(&cast.target_type);
                TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(inner),
                        target_type,
                    },
                    target_type,
                    span,
                )
            }
            ast::Expr::Unary(unary) => {
                let op = ast_unary_op_to_tir(unary.op);
                let inner = self.reify_expr(&unary.expr, ctx, None);
                TirExpr::new(
                    TirExprKind::Unary {
                        op,
                        expr: Box::new(inner),
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::MethodCall(method_call) => self.reify_method_call(method_call, ctx, recorded_type),
            ast::Expr::FieldAccess(field_access) => {
                // The `field_index` and `field_name` on `FieldAccess`
                // TIR are positional; the elaborator looks them up from
                // the receiver's struct decl. Reify reads the same
                // info from `tysys.all_struct_fields` keyed by the
                // receiver's resolved struct name.
                let inner = self.reify_expr(&field_access.expr, ctx, None);
                let (field_index, field_name) =
                    self.lookup_struct_field_index(inner.type_id, &field_access.field);
                TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(inner),
                        field_index,
                        field_name,
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::Binary(_)
            | ast::Expr::Assign(_)
            | ast::Expr::CompoundAssign(_)
            | ast::Expr::ComparisonChain(_)
            | ast::Expr::Call(_)
            | ast::Expr::StaticMethodCall(_)
            | ast::Expr::Index(_)
            | ast::Expr::If(_)
            | ast::Expr::Match(_)
            | ast::Expr::Matches(_)
            | ast::Expr::Closure(_)
            | ast::Expr::TemplateString(_)
            | ast::Expr::StructLiteral(_)
            | ast::Expr::LabeledBlock(_)
            | ast::Expr::TryOp(_)
            | ast::Expr::Spread(_, _)
            | ast::Expr::Range(_)
            | ast::Expr::WithHandler(_)
            | ast::Expr::Resume(_) => {
                // TODO(stage-5-bodies): mirror the corresponding
                // `Elaborator::resolve_expr` arm. Each arm consults
                // `sem.types.{expression_types, coercions,
                // method_dispatch, desugars, generic_instantiations,
                // closure_captures, assert_captures, for_of_iterator}`
                // as documented at the call site.
                todo!("reify_expr: {:?} variant pending body-walk reify", expr)
            }
        }
    }

    /// Reify a `MethodCallExpr`. This is the cleanest Stage 5 path:
    /// every decision — the resolved `FunctionRef`, the receiver-
    /// adjustment kind, the ref-impl flag, the expression's final type
    /// — is already on `sem.types` (`method_dispatch` from Stage 4,
    /// `expression_types` from Stage 4, `is_ref_impl` from Stage 5
    /// Gap 2). Reify reads all four and emits the same TIR shape
    /// `Elaborator::resolve_method_call_with` produced, sharing the
    /// receiver-adjustment helper
    /// [`super::Elaborator::adjust_receiver_for_self_kind_static`].
    ///
    /// The `IndexMutMethodCall` desugar (Gap 3) routes through here
    /// too: when `sem.types.desugars[id] == IndexMutMethodCall`, the
    /// receiver is an `IndexExpr` and reify must materialise the
    /// `__index_mut_val` local before dispatching the method. That
    /// branch is documented inline and currently `todo!`'d.
    fn reify_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        // IndexMut rewrite gets first crack — when the elaborator
        // tagged this call as `IndexMutMethodCall`, the receiver is an
        // index expression that needs `__index_mut_val` synthesis.
        if matches!(
            self.sem.types.desugars.get(&method_call.id),
            Some(super::sem::types::DesugarKind::IndexMutMethodCall)
        ) {
            // TODO(stage-5-bodies): mirror
            // `Elaborator::try_resolve_index_mut_method_call`
            // (method_lookup.rs:3390–3500). Synthesise
            // `let __index_mut_val = container.index_mut(idx);` (the
            // existing local-frame walk-order invariant makes
            // `__index_mut_val`'s index reproducible), then dispatch
            // the method on it. `sem.types.method_dispatch[id]` already
            // carries the *outer* method's dispatch target.
            todo!("reify_method_call: IndexMut rewrite pending body-walk reify");
        }

        // Dispatch decision (Stage 4 + Gap 2 record).
        let dispatch = self
            .sem
            .types
            .method_dispatch
            .get(&method_call.id)
            .cloned()
            .unwrap_or_else(|| {
                // Method lookup failed during annotate (error-recovery
                // path). Reify produces a placeholder `Unit` of `ERROR`
                // type so downstream phases see the same shape annotate
                // would have built; the actual diagnostic was already
                // emitted by the elaborator.
                panic!(
                    "reify_method_call: dispatch annotation missing for `{}` — \
                     annotate should have recorded or short-circuited via desugar",
                    method_call.method
                )
            });

        // Reify receiver and adjust per the dispatch contract. Stage 5
        // shares the adjuster with the elaborator so the same TIR shape
        // (Unary{Ref}/Unary{MutRef}/Deref wrapping) lands.
        let raw_receiver = self.reify_expr(&method_call.receiver, ctx, None);
        let adjusted_receiver =
            super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                raw_receiver,
                dispatch.self_kind,
                dispatch.is_ref_impl,
                method_call.span,
                &self.tysys.type_table,
            );

        // Explicit method-level type args resolved against the current
        // type-param scope. Inferred type args (the generic-instantiation
        // path) live on `sem.types.generic_instantiations` but the
        // elaborator's `FunctionRef.method_info.method_type_args` already
        // carries the mangled form — reify trusts the recorded
        // `FunctionRef` and only resolves the syntactic type args here.
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // TODO(stage-5-bodies): callee parameter types drive literal
        // re-coercion + `is_mut` flagging for each argument. Until the
        // recording for them lands, reify resolves each argument with
        // `expected = None` and sets `is_mut = false`. This matches the
        // shape of the elaborator's output for arguments that needed
        // no coercion / no mut binding, which covers the bulk of real
        // call sites; the missing cases are flagged in the WEP's
        // Stage 5 follow-up list.
        let args: Vec<crate::tir::CallArg> = method_call
            .args
            .iter()
            .map(|a| {
                let arg_tir = self.reify_expr(a, ctx, None);
                crate::tir::CallArg::new(arg_tir, false)
            })
            .collect();

        super::Elaborator::<H>::build_tir_method_call(
            adjusted_receiver,
            dispatch.function_ref,
            type_args,
            args,
            recorded_type,
            method_call.span,
        )
    }

    /// Resolve a struct field name to its `(index, name)` pair via
    /// the resolved struct type. Tuple-struct projections (`t.0`)
    /// resolve through the tuple element index. Returns `(0, name)` on
    /// lookup failure so reify doesn't panic on a type the dispatch
    /// hasn't ported yet — the produced TIR is wrong, but downstream
    /// validation flags it loudly.
    fn lookup_struct_field_index(
        &self,
        receiver_type: TypeId,
        field_name: &str,
    ) -> (u32, String) {
        use crate::tir::ResolvedType;
        let resolved = self.tysys.type_table.borrow().get(receiver_type).clone();
        let struct_name = match resolved {
            ResolvedType::Struct { name, .. } => name,
            ResolvedType::GenericInstance { name, .. } => name,
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                let inner_resolved = self.tysys.type_table.borrow().get(inner).clone();
                match inner_resolved {
                    ResolvedType::Struct { name, .. } => name,
                    ResolvedType::GenericInstance { name, .. } => name,
                    _ => return (0, field_name.to_string()),
                }
            }
            _ => return (0, field_name.to_string()),
        };

        let lookup = self.type_lookup();
        if let Some(info) = lookup.struct_fields(&struct_name)
            && let Some((idx, (n, _, _))) = info
                .fields
                .iter()
                .enumerate()
                .find(|(_, (n, _, _))| n == field_name)
        {
            return (idx as u32, n.clone());
        }
        (0, field_name.to_string())
    }

    /// Reify a bare identifier reference. Local lookup goes through
    /// the per-function context (`FunctionContext::lookup`, walk-order
    /// invariant — Gap 7). Non-local idents (globals, function refs,
    /// enum / variant ctors) need the use→def edge in
    /// `sem.bindings.references` to pick the right TIR shape; that
    /// dispatch is the body-walk pending work.
    fn reify_ident(
        &mut self,
        ident: &ast::IdentExpr,
        recorded_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::TirExprKind;

        if let Some(local) = ctx.lookup(&ident.name) {
            return TirExpr::new(
                TirExprKind::Local {
                    index: local.index,
                    name: ident.name.clone(),
                },
                recorded_type,
                ident.span,
            );
        }

        // TODO(stage-5-bodies): non-local idents need to dispatch on
        // `sem.bindings.references[ident.id]`:
        //   - Global (`current_module_globals` / `imported_globals`)
        //     → `GlobalVarGet`
        //   - Free function → `FuncRef` (with type_args from
        //     `sem.types.generic_instantiations[ident.id]` and
        //     turbofish `ident.type_args` resolved via
        //     `resolve_type_in_scope`)
        //   - Enum / payload-less variant ctor → `EnumConstruct` /
        //     `VariantConstruct` (with type_args from
        //     `sem.types.generic_instantiations[ident.id]`)
        //   - Flags member → `IntLiteral` with the bitmask value
        //   - Associated constant → inlined via
        //     `sem.decls.associated_constants`
        //
        // The full surface matches `Elaborator::resolve_ident`
        // (expr.rs ≈300–800); panic with a labelled todo until the
        // dispatch is ported.
        let _ = recorded_type;
        todo!(
            "reify_ident: non-local `{}` pending body-walk reify",
            ident.name
        )
    }

    /// Reify a literal expression into its TIR shape. The recorded
    /// `TypeId` from `sem.types.expression_types` carries the final
    /// numeric type (e.g. an `i32` literal coerced to `i64` is recorded
    /// as `i64`), so this helper does not re-run literal-type defaulting.
    fn reify_literal(
        &mut self,
        lit: &ast::LiteralExpr,
        recorded_type: TypeId,
        ctx: &FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};
        let kind = match &lit.value {
            ast::Literal::Number(repr) => {
                // The recorded type tells us whether to emit an Int or
                // a Float TIR literal. Parsing the digits is done here
                // (same logic as `Elaborator::resolve_numeric_literal`
                // at expr.rs ≈648–765, sans the type-defaulting tree).
                if recorded_type == TypeTable::F32 || recorded_type == TypeTable::F64 {
                    let value: f64 = repr.parse().unwrap_or(0.0);
                    TirExprKind::FloatLiteral {
                        value,
                        repr: repr.clone(),
                    }
                } else {
                    let value: u64 = if let Some(stripped) = repr.strip_prefix("0x") {
                        u64::from_str_radix(stripped, 16).unwrap_or(0)
                    } else if let Some(stripped) = repr.strip_prefix("0o") {
                        u64::from_str_radix(stripped, 8).unwrap_or(0)
                    } else if let Some(stripped) = repr.strip_prefix("0b") {
                        u64::from_str_radix(stripped, 2).unwrap_or(0)
                    } else {
                        repr.parse::<u64>().unwrap_or(0)
                    };
                    TirExprKind::IntLiteral {
                        value,
                        repr: repr.clone(),
                    }
                }
            }
            ast::Literal::String(s) => TirExprKind::StringLiteral(s.clone()),
            ast::Literal::Char(s) => {
                // The Char literal is the raw source text (e.g. "'a'").
                // Strip the quotes and decode escapes the same way the
                // elaborator does. Stage 5 follow-up: share the decoder
                // with `Elaborator::resolve_char_literal` instead of
                // re-implementing here.
                let inner = s.trim_start_matches('\'').trim_end_matches('\'');
                let ch = inner.chars().next().unwrap_or('\0');
                TirExprKind::CharLiteral(ch)
            }
            ast::Literal::Bool(b) => TirExprKind::BoolLiteral(*b),
            ast::Literal::Null => TirExprKind::Null,
            ast::Literal::Unit => TirExprKind::Unit,
            ast::Literal::LocationFunction => {
                TirExprKind::StringLiteral(ctx.function_name.clone())
            }
            ast::Literal::LocationFile
            | ast::Literal::LocationLine
            | ast::Literal::DataSection
            | ast::Literal::IncludeStr(_)
            | ast::Literal::IncludeBytes(_) => {
                // TODO(stage-5-bodies): mirror the host-driven branches
                // of `Elaborator::resolve_literal`. `#file`/`#line`
                // need the logger's current file context;
                // `#include_str("path")` and `#include_bytes("path")`
                // additionally need the resolved bytes from
                // `tysys.included_files`. `#data` reads from
                // `Module::data_section`.
                todo!("reify_literal: location / include / data-section pending")
            }
        };
        TirExpr::new(kind, recorded_type, lit.span)
    }

    /// Reify a pattern in a `let`, `match`, `if let`, or `while let`.
    /// Binding patterns add locals to `ctx` in the same order annotate
    /// did (per the walk-order invariant).
    #[allow(unused_variables)] // kept until pattern dispatch is fleshed out
    pub(super) fn reify_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Wildcard => TirPattern::Wildcard,
            _ => {
                // TODO(stage-5-bodies): mirror
                // `Elaborator::resolve_if_pattern_inner`'s remaining
                // arms (Ident, Tuple, Struct, Variant, Enum, Literal,
                // Range, Or, Binding).
                todo!("reify_pattern: variant pending body-walk reify")
            }
        }
    }
}

/// Map an AST [`ast::UnaryOp`] to its TIR counterpart. The two enums
/// are 1:1; this helper exists so the dispatch table doesn't repeat
/// the mapping at every Unary arm.
fn ast_unary_op_to_tir(op: ast::UnaryOp) -> crate::tir::TirUnaryOp {
    use crate::tir::TirUnaryOp;
    match op {
        ast::UnaryOp::Neg => TirUnaryOp::Neg,
        ast::UnaryOp::Not => TirUnaryOp::Not,
        ast::UnaryOp::BitNot => TirUnaryOp::BitNot,
        ast::UnaryOp::Ref => TirUnaryOp::Ref,
        ast::UnaryOp::MutRef => TirUnaryOp::MutRef,
        ast::UnaryOp::Deref => TirUnaryOp::Deref,
    }
}

/// Map an AST [`ast::BinaryOp`] to its TIR counterpart. The mapping is
/// 1:1 for the source-level ops; TIR adds `RefEq` / `RefNotEq` as
/// internal variants that the elaborator only synthesises after
/// coercion analysis, so reify never produces them from this helper.
#[allow(dead_code)] // used by `Binary` arm once it lands
fn ast_binary_op_to_tir(op: ast::BinaryOp) -> crate::tir::TirBinaryOp {
    use crate::tir::TirBinaryOp;
    match op {
        ast::BinaryOp::Add => TirBinaryOp::Add,
        ast::BinaryOp::Sub => TirBinaryOp::Sub,
        ast::BinaryOp::Mul => TirBinaryOp::Mul,
        ast::BinaryOp::Div => TirBinaryOp::Div,
        ast::BinaryOp::Mod => TirBinaryOp::Mod,
        ast::BinaryOp::Eq => TirBinaryOp::Eq,
        ast::BinaryOp::NotEq => TirBinaryOp::NotEq,
        ast::BinaryOp::Lt => TirBinaryOp::Lt,
        ast::BinaryOp::LtEq => TirBinaryOp::LtEq,
        ast::BinaryOp::Gt => TirBinaryOp::Gt,
        ast::BinaryOp::GtEq => TirBinaryOp::GtEq,
        ast::BinaryOp::And => TirBinaryOp::And,
        ast::BinaryOp::Or => TirBinaryOp::Or,
        ast::BinaryOp::BitAnd => TirBinaryOp::BitAnd,
        ast::BinaryOp::BitOr => TirBinaryOp::BitOr,
        ast::BinaryOp::BitXor => TirBinaryOp::BitXor,
        ast::BinaryOp::Shl => TirBinaryOp::Shl,
        ast::BinaryOp::Shr => TirBinaryOp::Shr,
    }
}
