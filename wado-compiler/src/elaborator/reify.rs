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
    /// Construct a per-module `Reify` for the orchestration driver.
    /// The `tysys` clone is the shallow Rc/Arc copy
    /// [`TypeSystem`] supports; per-module state (`sem`,
    /// `current_module_*`) is borrowed from
    /// [`crate::elaborator::orchestration::AnnotateState`] for the
    /// duration of the reify walk.
    ///
    /// `current_module_source` / `current_module_items` are
    /// placeholders at construction time — the driver overwrites them
    /// inside [`Self::reify_module`]. Keeping them on the struct
    /// matches the elaborator's shape (avoids threading them through
    /// every method signature) and keeps the walk-order invariant
    /// (Gap 7) tractable.
    pub(crate) fn new(
        tysys: TypeSystem,
        sem: &'a ModuleSemantics,
        symbols: &'a SymbolTable,
        loaded_modules: &'a IndexMap<ModuleSource, Module>,
        logger: &'a Logger<'a, H>,
        entry_module_source: ModuleSource,
        interner: Rc<RefCell<ModuleSourceInterner>>,
        invocations: Rc<crate::kiln::InvocationIndex>,
    ) -> Self {
        Self {
            tysys,
            sem,
            symbols,
            loaded_modules,
            logger,
            current_module_source: ModuleSource::entry_point_uninitialized(),
            current_module_items: &[],
            interner,
            invocations,
            entry_module_source,
        }
    }

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
            let default_expr = param
                .default
                .as_ref()
                .map(|default_ast| Box::new(self.reify_expr(default_ast, &mut ctx, Some(type_id))));
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
            task_return_type: if func.is_async {
                Some(return_type)
            } else {
                None
            },
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
    /// `TirFunction` plus the `TirTest` metadata. Mirrors
    /// `Elaborator::resolve_test_decl` (item.rs:1233+): the function
    /// name encodes test_index + attributes (`expect_trap`, `TODO`,
    /// `timeout_ms`); the body reifies into a unit-returning
    /// no-parameter function.
    fn reify_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
        module_is_todo: bool,
    ) -> Option<(TirFunction, TirTest)> {
        use crate::tir::{FunctionKind, InlineHint, ReturnAbi, TypeTable};

        let expect_trap = test_decl.attributes.iter().any(|a| a.name == "expect_trap");
        let is_todo = module_is_todo || test_decl.attributes.iter().any(|a| a.name == "TODO");
        let timeout_ms = test_decl.attributes.iter().find_map(|a| {
            if a.name == "timeout_ms" {
                a.args
                    .first()
                    .and_then(|arg| arg.as_str().parse::<u64>().ok())
            } else {
                None
            }
        });

        let prefix = match (is_todo, expect_trap, timeout_ms) {
            (true, _, Some(ms)) => format!("__test_todo_tm{ms}"),
            (true, _, None) => "__test_todo".to_string(),
            (_, true, Some(ms)) => format!("__test_trap_tm{ms}"),
            (_, true, None) => "__test_trap".to_string(),
            (_, _, Some(ms)) => format!("__test_tm{ms}"),
            (_, _, None) => "__test".to_string(),
        };
        let function_name = match &test_decl.name {
            Some(name) => {
                let snake_name: String = name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect::<String>()
                    .to_lowercase();
                format!("{prefix}_{test_index}_{snake_name}")
            }
            None => format!("{prefix}_{test_index}"),
        };

        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());
        let body = self.reify_block(&test_decl.body, &mut ctx, None);

        let tir_func = TirFunction {
            module_source: ModuleSource::default(),
            name: function_name.clone(),
            is_pub: false,
            is_export: false,
            is_async: false,
            type_params: vec![],
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params: vec![],
            return_type,
            task_return_type: None,
            effects: vec![],
            stores: vec![],
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
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
        };

        let tir_test = TirTest {
            name: test_decl.name.clone(),
            function_name,
            line: test_decl.span.line,
            span: test_decl.span,
            expect_trap,
            is_todo,
            timeout_ms,
        };

        Some((tir_func, tir_test))
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
                vec![TirStmt::new(
                    TirStmtKind::TaskReturn { value },
                    tr_stmt.span,
                )]
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
            ast::Stmt::If(if_stmt) => self.reify_if_stmt(if_stmt, ctx),
            ast::Stmt::Match(match_expr) => {
                // Stmt-position match — `Elaborator::resolve_stmt`
                // pins `expected_type = Some(Unit)` and records the
                // result type explicitly (stmt.rs ≈84–105). Reify
                // mirrors: reify the expression at Unit, then wrap as
                // an `Expr` stmt. The reified expression's
                // `type_id` will already be `Unit` (annotate's
                // `expression_types` records the stmt-position type),
                // so the WIR builder drops each arm body's value.
                let tir = self.reify_match_expr(
                    match_expr,
                    ctx,
                    Some(crate::tir::TypeTable::UNIT),
                    crate::tir::TypeTable::UNIT,
                );
                vec![TirStmt::new(TirStmtKind::Expr(tir), match_expr.span)]
            }
            ast::Stmt::Loop(loop_stmt) => {
                // `loop { … }` — direct lowering. The
                // `for_continue_labels` save/restore mirrors
                // `Elaborator::resolve_loop` (stmt.rs:2092–2101).
                let saved = std::mem::take(&mut ctx.for_continue_labels);
                let body = self.reify_block(&loop_stmt.body, ctx, None);
                ctx.for_continue_labels = saved;
                vec![TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)]
            }
            ast::Stmt::LabeledBlock(labeled_block) => {
                // `LABEL: { … }` stmt — mirrors
                // `Elaborator::resolve_labeled_block` (stmt.rs:116+).
                // Push the label onto `active_labels` so a nested
                // `break LABEL` lowers against this frame, walk the
                // inner block, pop. The block result is dropped at
                // stmt position, so no `expected_type` propagates.
                ctx.active_labels.push(labeled_block.label.clone());
                let block = self.reify_block(&labeled_block.block, ctx, None);
                ctx.active_labels.pop();
                vec![TirStmt::new(
                    TirStmtKind::LabeledBlock {
                        label: labeled_block.label.clone(),
                        block,
                    },
                    labeled_block.span,
                )]
            }
            ast::Stmt::While(w) => self.reify_while(w, ctx),
            ast::Stmt::For(f) => self.reify_for(f, ctx),
            ast::Stmt::Assert(assert_stmt) => self.reify_assert(assert_stmt, ctx),
            ast::Stmt::ForOf(_) => {
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
        // Uninitialised `let x: T;` — the parser guarantees `ty`
        // is present. The WIR builder zero-initialises the slot;
        // reify emits a Unit placeholder as the `value` and the
        // `type_id` field carries the user-declared type. Refutable
        // patterns in this position are rejected at annotate; the
        // recovery path emits an Expr-Unit placeholder to mirror.
        let Some(ast_value) = let_stmt.value.as_ref() else {
            use crate::tir::{TirExprKind, TirStmtKind, TypeTable};
            let type_id = let_stmt
                .ty
                .as_ref()
                .map(|t| self.resolve_type(t))
                .unwrap_or(TypeTable::UNKNOWN);
            return match &let_stmt.pattern {
                ast::Pattern::Ident { id, name, span: _ }
                | ast::Pattern::MutIdent { id, name, span: _ } => {
                    let is_mut = let_stmt.is_mut
                        || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                    let local_index = ctx.add_local(name.clone(), type_id, is_mut, Some(*id));
                    let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, let_stmt.span);
                    TirStmt::new(
                        TirStmtKind::Let {
                            name: name.clone(),
                            local_index,
                            is_mut,
                            is_reactive: let_stmt.is_reactive,
                            type_id,
                            value: placeholder,
                            skip_value_copy: false,
                        },
                        let_stmt.span,
                    )
                }
                _ => TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, let_stmt.span)),
                    let_stmt.span,
                ),
            };
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
            ast::Pattern::Tuple(_, _)
            | ast::Pattern::Struct { .. }
            | ast::Pattern::Variant { .. } => {
                // Destructuring `let [a, b] = …;` / `let Point { x, y }
                // = …;` / `let Some(x) = …;`. The TIR uses
                // `TirStmtKind::LetDestructure` rather than `Let`. The
                // shared `reify_pattern` adds the sub-pattern bindings
                // to `ctx`; the value's recorded type drives the
                // pattern's per-binding type lookups.
                let pattern = self.reify_pattern(&let_stmt.pattern, type_id, ctx);
                TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Literal(_) | ast::Pattern::Or(_) | ast::Pattern::Range { .. } => {
                let _ = type_id;
                let _ = TypeTable::UNKNOWN;
                // `let 42 = expr;` etc. are refutable patterns and the
                // elaborator rejects them at annotate time (only
                // irrefutable patterns are valid in `let`). Hitting
                // this branch means annotate let a refutable pattern
                // through — surface the invariant violation here.
                panic!(
                    "reify_let: refutable pattern {:?} in let binding (annotate should have rejected)",
                    let_stmt.pattern
                )
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
                TirExpr::new(TirExprKind::TupleLiteral { elements }, recorded_type, span)
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
            ast::Expr::MethodCall(method_call) => {
                self.reify_method_call(method_call, ctx, recorded_type)
            }
            ast::Expr::Binary(binary) => self.reify_binary(binary, ctx, recorded_type),
            ast::Expr::Call(call) => self.reify_call(call, ctx, recorded_type),
            ast::Expr::Match(match_expr) => {
                self.reify_match_expr(match_expr, ctx, expected_type, recorded_type)
            }
            ast::Expr::StructLiteral(struct_lit) => {
                self.reify_struct_literal(struct_lit, ctx, recorded_type)
            }
            ast::Expr::Range(range) => self.reify_range(range, ctx, recorded_type),
            ast::Expr::TemplateString(template) => {
                self.reify_template_string(template, ctx, recorded_type)
            }
            ast::Expr::Matches(m) => self.reify_matches(m, ctx),
            ast::Expr::CompoundAssign(compound) => {
                self.reify_compound_assign(compound, ctx, recorded_type)
            }
            ast::Expr::TryOp(qm) => self.reify_question_mark(qm, ctx, recorded_type),
            ast::Expr::Closure(closure) => self.reify_closure(closure, ctx, recorded_type, expected_type),
            ast::Expr::Index(index) => self.reify_index(index, ctx, recorded_type),
            ast::Expr::ComparisonChain(chain) => self.reify_comparison_chain(chain, ctx),
            ast::Expr::Resume(resume) => {
                // `resume value` inside a handler method. Reify the
                // value with the function's return type as expected
                // (matches `Elaborator::resolve_resume` at
                // handlers.rs:445), then emit `TirExprKind::Resume`.
                let expected = if ctx.in_handler_method {
                    Some(ctx.return_type)
                } else {
                    None
                };
                let value = self.reify_expr(&resume.value, ctx, expected);
                TirExpr::new(
                    TirExprKind::Resume {
                        value: Box::new(value),
                    },
                    crate::tir::TypeTable::UNIT,
                    span,
                )
            }
            ast::Expr::LabeledBlock(lb) => {
                // Match `Elaborator::resolve_expr`'s `LabeledBlock`
                // arm (expr.rs:234–305): push a `LabeledBlockTarget`
                // so any `break label: expr` inside lowers via this
                // frame, walk the inner block, pop the frame, emit
                // `TirExprKind::LabeledBlock`. The result type is the
                // recorded `expression_types[lb.id]`; annotate already
                // unified break types into it.
                use crate::elaborator::types::LabeledBlockTarget;
                ctx.labeled_block_targets.push(LabeledBlockTarget {
                    label: lb.label.clone(),
                    break_types: Vec::new(),
                    expected_type,
                });
                ctx.active_labels.push(lb.label.clone());
                let tir_block = self.reify_block(&lb.block, ctx, expected_type);
                ctx.active_labels.pop();
                let _target = ctx.labeled_block_targets.pop();
                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: lb.label.clone(),
                        block: tir_block,
                        result_type: recorded_type,
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::Spread(_, _) => {
                // `Spread` is only valid inside a tuple literal; the
                // elaborator panics if it sees one at top level.
                // Mirror the panic — annotate would have already
                // diagnosed a stray spread.
                panic!("reify_expr: bare Spread is invalid outside TupleLiteral")
            }
            ast::Expr::If(if_expr) => {
                self.reify_if_expr(if_expr, ctx, expected_type, recorded_type)
            }
            ast::Expr::Assign(assign) => {
                // `target = value` — both sides walked recursively; the
                // expression's type is `Unit` (assignment is a stmt-shape
                // expression in Wado, mirroring Rust).
                let target = self.reify_expr(&assign.target, ctx, None);
                let value = self.reify_expr(&assign.value, ctx, Some(target.type_id));
                TirExpr::new(
                    TirExprKind::Assign {
                        target: Box::new(target),
                        value: Box::new(value),
                    },
                    recorded_type,
                    span,
                )
            }
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
            ast::Expr::StaticMethodCall(_)
            | ast::Expr::WithHandler(_) => {
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

    /// Reify a `while cond { body }` statement. Mirrors
    /// `Elaborator::resolve_while`'s `Condition::Expr` arm
    /// (stmt.rs:2982+): the loop lowers into
    /// `Loop { if !cond { break; } body }`, which is the desugar
    /// `DesugarKind::While` tags. `for_continue_labels` is saved /
    /// restored around the body walk so naked `continue` inside
    /// `while` targets this loop (not an enclosing C-style `for`).
    fn reify_while(&mut self, w: &ast::WhileStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable};

        let span = w.span;
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);

        let stmts = match &w.condition {
            ast::Condition::Expr(cond_expr) => {
                let cond_span = cond_expr.span();
                let cond_tir = self.reify_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let neg_cond = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(cond_tir),
                    },
                    TypeTable::BOOL,
                    cond_span,
                );
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let if_break = TirStmt::new(
                    TirStmtKind::If {
                        condition: neg_cond,
                        then_block: TirBlock::new(vec![break_stmt], span),
                        else_block: None,
                    },
                    span,
                );
                let body_block = self.reify_block(&w.body, ctx, None);
                let mut stmts = Vec::with_capacity(1 + body_block.stmts.len());
                stmts.push(if_break);
                stmts.extend(body_block.stmts);
                stmts
            }
            ast::Condition::LetChain { .. } => {
                // TODO(stage-5-bodies): `while let PAT = … { … }`
                // shares the let-chain expansion with `if let` (see
                // `reify_if_expr`'s LetChain branch). When that
                // helper lands it should also drive this arm.
                todo!("reify_while: LetChain pending body-walk reify")
            }
        };

        ctx.for_continue_labels = saved_continue;
        vec![TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(stmts, span),
            },
            span,
        )]
    }

    /// Reify an `assert cond[, msg];` statement. The full power-
    /// assert template (slot extraction into `__vK = …;` + the
    /// captured-source message) requires re-running the
    /// [`super::assert::CaptureScanner`] against the condition AST
    /// in parallel with reify's walk; Gap 5's `assert_captures`
    /// recording carries the slot↔AstId map for that replay.
    ///
    /// Stage 5 lands the simplified shape that matches Wado's
    /// runtime semantics: `if !cond { panic("assertion failed at
    /// <file>:<line>") }`. The power-assert message reconstruction
    /// is staged as a follow-up — the recording is in place; the
    /// playback is the remaining work.
    fn reify_assert(
        &mut self,
        assert_stmt: &ast::AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::{
            CallArg, FunctionRef, TirBlock, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable,
        };

        let span = assert_stmt.span;
        let cond_tir = self.reify_expr(&assert_stmt.condition, ctx, Some(TypeTable::BOOL));
        let neg_cond = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(cond_tir),
            },
            TypeTable::BOOL,
            span,
        );

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        let panic_msg = TirExpr::new(
            TirExprKind::StringLiteral(format!(
                "Assertion failed in {} at {}:{}",
                ctx.function_name,
                self.current_module_source,
                span.line,
            )),
            string_type,
            span,
        );

        let panic_module_source = self.interner.borrow_mut().core("internal");
        let panic_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: panic_module_source,
                    name: "panic".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: Vec::new(),
                args: vec![CallArg::new(panic_msg, false)],
            },
            TypeTable::NEVER,
            span,
        );

        let then_block = TirBlock::new(
            vec![TirStmt::new(TirStmtKind::Expr(panic_call), span)],
            span,
        );
        let if_stmt = TirStmt::new(
            TirStmtKind::If {
                condition: neg_cond,
                then_block,
                else_block: None,
            },
            span,
        );

        // Wrap in `__assert_N:` LabeledBlock so the synthetic
        // counter on `FunctionContext` advances in lockstep with
        // annotate's allocation (Gap 7 walk-order invariant).
        let assert_serial = ctx.next_assert_id;
        ctx.next_assert_id += 1;
        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: format!("__assert_{assert_serial}"),
                block: TirBlock::new(vec![if_stmt], span),
            },
            span,
        )]
    }

    /// Reify a C-style `for init; cond; update { body }` loop into
    /// the shape `Elaborator::resolve_for` produces (stmt.rs:3095+).
    /// Implements the `Condition::Expr` arm; `Condition::LetChain`
    /// shares the let-chain expansion with `if let` / `while let`
    /// and routes through the same pending `todo!`.
    fn reify_for(&mut self, f: &ast::ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable};

        let span = f.span;
        let loop_id = ctx.next_loop_id;
        ctx.next_loop_id += 1;
        let body_label = format!("__for_{loop_id}_body");

        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);
        ctx.enter_scope();

        let mut outer_stmts: Vec<TirStmt> = Vec::new();
        if let Some(init) = &f.init {
            outer_stmts.extend(self.reify_stmt(init, ctx));
        }

        let iter_stmts: Vec<TirStmt> = match &f.condition {
            None => {
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![labeled_body];
                s.extend(self.reify_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(ast::Condition::Expr(cond_expr)) => {
                let cond_span = cond_expr.span();
                let cond_tir = self.reify_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let neg_cond = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(cond_tir),
                    },
                    TypeTable::BOOL,
                    cond_span,
                );
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let if_break = TirStmt::new(
                    TirStmtKind::If {
                        condition: neg_cond,
                        then_block: TirBlock::new(vec![break_stmt], span),
                        else_block: None,
                    },
                    span,
                );
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![if_break, labeled_body];
                s.extend(self.reify_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(ast::Condition::LetChain { .. }) => {
                // TODO(stage-5-bodies): for-let-chain mirrors the
                // `if let` / `while let` chain expansion. Lands when
                // `reify_let_chain_stmts` lands.
                todo!("reify_for: LetChain condition pending body-walk reify")
            }
        };

        outer_stmts.push(TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(iter_stmts, span),
            },
            span,
        ));

        ctx.exit_scope();
        ctx.for_continue_labels = saved_continue;
        outer_stmts
    }

    /// Reify the for-loop body wrapped in `__for_N_body:` so naked
    /// `continue` lowers as `break __for_N_body` (letting the
    /// `update` expression run before the next iteration). Mirrors
    /// `Elaborator::resolve_for_labeled_body` (stmt.rs:3280+).
    fn reify_for_labeled_body(
        &mut self,
        body_label: &str,
        body: &ast::Block,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        use crate::tir::TirStmtKind;
        ctx.for_continue_labels.push(body_label.to_string());
        ctx.active_labels.push(body_label.to_string());
        let body_block = self.reify_block(body, ctx, None);
        ctx.active_labels.pop();
        ctx.for_continue_labels.pop();
        TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: body_label.to_string(),
                block: body_block,
            },
            body.span,
        )
    }

    /// Reify the for-loop's optional `update` expression as a single
    /// stmt-list (empty when absent). Mirrors
    /// `Elaborator::resolve_for_update` (stmt.rs:3302+).
    fn reify_for_update(
        &mut self,
        update: Option<&ast::Expr>,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        update
            .map(|u| {
                let tir = self.reify_expr(u, ctx, None);
                vec![TirStmt::new(crate::tir::TirStmtKind::Expr(tir), u.span())]
            })
            .unwrap_or_default()
    }

    /// Reify a stmt-position `if cond { … } else { … }`. Stmt
    /// position never carries an `expected_type` from the surrounding
    /// block (the elaborator switches to `…_with_expected` only on a
    /// trailing position; reify follows suit by passing `None` to the
    /// branches). `Condition::LetChain` mirrors the expression-level
    /// `IfLetChain` desugar; the chain expansion lives behind the
    /// same `todo!` as `reify_if_expr`.
    fn reify_if_stmt(&mut self, if_stmt: &ast::IfStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::TirStmtKind;
        match &if_stmt.condition {
            ast::Condition::Expr(cond_expr) => {
                let condition = self.reify_expr(cond_expr, ctx, Some(crate::tir::TypeTable::BOOL));
                let then_block = self.reify_block(&if_stmt.then_block, ctx, None);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, None));
                vec![TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    },
                    if_stmt.span,
                )]
            }
            ast::Condition::LetChain { .. } => {
                // TODO(stage-5-bodies): mirror
                // `Elaborator::resolve_if_stmt`'s `Condition::LetChain`
                // arm (stmt.rs ≈1014). The expansion shape is the
                // same as the expression-position chain — share with
                // `reify_if_expr`'s LetChain branch when it lands.
                todo!("reify_if_stmt: LetChain pending body-walk reify")
            }
        }
    }

    /// Reify an `if cond { … } else { … }` expression. The `cond`
    /// shape is restricted to `Condition::Expr` here; `Condition::LetChain`
    /// dispatches through the `IfLetChain` desugar (`sem.types.desugars`
    /// records the tag at annotate time — `Elaborator::resolve_if_expr`
    /// at expr.rs:1860). The chain expansion needs `let`-binding +
    /// branch-merging logic that mirrors `resolve_let_chain_stmts` and
    /// is deferred to a follow-up.
    fn reify_if_expr(
        &mut self,
        if_expr: &ast::IfExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        recorded_type: TypeId,
    ) -> TirExpr {
        let cond_expr = match &if_expr.condition {
            ast::Condition::Expr(e) => e,
            ast::Condition::LetChain { .. } => {
                // TODO(stage-5-bodies): mirror
                // `Elaborator::resolve_if_expr`'s `Condition::LetChain`
                // arm (expr.rs ≈1867–1973). The expansion produces a
                // `Block` of nested `IfLet` stmts that fall through to
                // the `else_block`; reify reads the
                // `DesugarKind::IfLetChain` tag the elaborator already
                // placed on `if_expr.id` and replays the same shape.
                todo!("reify_if_expr: LetChain pending body-walk reify")
            }
        };
        let condition = self.reify_expr(cond_expr, ctx, Some(crate::tir::TypeTable::BOOL));
        let then_branch = self.reify_block(&if_expr.then_block, ctx, expected_type);
        let else_branch = if_expr
            .else_block
            .as_ref()
            .map(|b| self.reify_block(b, ctx, expected_type));
        TirExpr::new(
            crate::tir::TirExprKind::If {
                condition: Box::new(condition),
                then_branch,
                else_branch,
            },
            recorded_type,
            if_expr.span,
        )
    }

    /// Reify a binary expression. When the elaborator dispatched the
    /// operator to a trait method (Gap 11), the
    /// `sem.types.operator_dispatch[binary.id]` entry carries the
    /// `(FunctionRef, self_kind, arg_ref_wraps, return_type)` reify
    /// needs to emit the same `TirExprKind::MethodCall` shape. Absence
    /// of an entry means the elaborator emitted a native
    /// `TirExprKind::Binary`; reify mirrors with the 1:1 op mapping.
    fn reify_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, ResolvedType, TirExprKind, TirUnaryOp};

        let left = self.reify_expr(&binary.left, ctx, None);
        let right = self.reify_expr(&binary.right, ctx, None);

        if let Some(dispatch) = self.sem.types.operator_dispatch.get(&binary.id).cloned() {
            // Operator-trait dispatch path. Reuse the shared receiver
            // adjuster (statically; no Elaborator needed) and the
            // shared arg-wrap helper to produce TIR identical to what
            // `build_trait_op_method_call_on_resolved` emitted.
            let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                left,
                dispatch.self_kind,
                /* is_ref_impl */ false,
                binary.span,
                &self.tysys.type_table,
            );
            let args = vec![right];
            let call_args: Vec<CallArg> = args
                .into_iter()
                .zip(dispatch.arg_ref_wraps.iter().copied())
                .map(|(arg, wrap)| {
                    let arg_expr = if wrap {
                        let arg_ref_type = self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(arg.type_id));
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(arg),
                            },
                            arg_ref_type,
                            binary.span,
                        )
                    } else {
                        arg
                    };
                    CallArg::new(arg_expr, false)
                })
                .collect();
            return super::Elaborator::<H>::build_tir_method_call(
                receiver,
                dispatch.function_ref,
                vec![],
                call_args,
                dispatch.return_type,
                binary.span,
            );
        }

        // Native binary op — primitive path. The op mapping is 1:1
        // with the AST. Stage 5 follow-up: ref-equality
        // (`RefEq` / `RefNotEq`) is synthesised by the elaborator
        // after type analysis; until that decision is recorded, reify
        // emits the source-level op verbatim. The Cap on this is the
        // `==` / `!=` path on ref types; other ops on refs would
        // already be diagnosed by annotate.
        TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(left),
                op: ast_binary_op_to_tir(binary.op),
                right: Box::new(right),
            },
            recorded_type,
            binary.span,
        )
    }

    /// Reify a template string `"…{expr}…"`. Mirrors
    /// `Elaborator::resolve_template_string` (template.rs:16+):
    /// - Constant fast path: no interpolations → concatenate the
    ///   string parts at reify time and emit a `StringLiteral`.
    /// - Single-`String`-typed interpolation with no format spec →
    ///   forward the resolved expression unchanged.
    /// - General case: build a `Vec<TirTemplatePart>` where each
    ///   `Interpolation` part carries an optional
    ///   [`crate::tir::TemplateFormatSpec`] parsed by
    ///   [`super::template::parse_format_spec`].
    fn reify_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
        _recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirTemplatePart};

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        let span = template.span;

        let has_interpolation = template
            .parts
            .iter()
            .any(|p| matches!(p, ast::TemplatePart::Interpolation { .. }));

        if !has_interpolation {
            let mut combined = String::new();
            for part in &template.parts {
                if let ast::TemplatePart::String(s) = part {
                    let unescaped = super::util::unescape_template_string(s).unwrap_or_default();
                    combined.push_str(&unescaped);
                }
            }
            return TirExpr::new(TirExprKind::StringLiteral(combined), string_type, span);
        }

        if template.parts.len() == 1
            && let ast::TemplatePart::Interpolation { expr, format: None } = &template.parts[0]
        {
            let resolved = self.reify_expr(expr, ctx, None);
            if resolved.type_id == string_type {
                return resolved;
            }
        }

        let mut parts = Vec::new();
        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    if !s.is_empty() {
                        let unescaped =
                            super::util::unescape_template_string(s).unwrap_or_default();
                        if !unescaped.is_empty() {
                            parts.push(TirTemplatePart::Literal(unescaped));
                        }
                    }
                }
                ast::TemplatePart::Interpolation { expr, format } => {
                    let resolved = self.reify_expr(expr, ctx, None);
                    let format_spec = format
                        .as_ref()
                        .map(|f| super::template::parse_format_spec(&f.spec));
                    parts.push(TirTemplatePart::Interpolation {
                        expr: Box::new(resolved),
                        format_spec,
                    });
                }
            }
        }

        TirExpr::new(TirExprKind::TemplateString { parts }, string_type, span)
    }

    /// Reify a `a..<b` / `a..=b` range expression. The elaborator
    /// lowers ranges into the prelude's `RangeExclusive` /
    /// `RangeInclusive` struct literals (expr.rs:4397+); reify
    /// produces the same shape by reading the element type from
    /// the reified `start` expression and interning the
    /// `GenericInstance` via `make_generic_instance`.
    fn reify_range(
        &mut self,
        range: &ast::RangeExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::ast::RangeKind;
        use crate::tir::{TirExprKind, TirStructField, TypeTable};

        // Resolve both operands first; the element type comes from
        // `start` (annotate has unified start/end to the same type, so
        // either operand's type works).
        let start = self.reify_expr(&range.start, ctx, None);
        let end_expected = Some(start.type_id);
        let end = self.reify_expr(&range.end, ctx, end_expected);
        let element_type = start.type_id;

        // The recorded `expression_types[range.id]` carries the
        // assembled `GenericInstance` type, but the elaborator's
        // construction is purely from the prelude's compiler-item
        // registry — reproduce here so the same `module_source` lands
        // even if a future inference change made the recorded type
        // less specific.
        let (struct_name, module_source) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            match range.kind {
                RangeKind::Exclusive => (
                    "RangeExclusive".to_string(),
                    items
                        .struct_module(crate::compiler_item::CompilerItem::RangeExclusive)
                        .cloned()
                        .unwrap_or_else(crate::module_source::ModuleSource::range),
                ),
                RangeKind::Inclusive => (
                    "RangeInclusive".to_string(),
                    items
                        .struct_module(crate::compiler_item::CompilerItem::RangeInclusive)
                        .cloned()
                        .unwrap_or_else(crate::module_source::ModuleSource::range),
                ),
            }
        };

        let struct_type = self.tysys.type_table.borrow_mut().make_generic_instance(
            struct_name.clone(),
            module_source,
            vec![element_type],
        );

        let mut fields = vec![
            TirStructField {
                name: "start".to_string(),
                value: start,
                field_index: 0,
            },
            TirStructField {
                name: "end".to_string(),
                value: end,
                field_index: 1,
            },
        ];
        if matches!(range.kind, RangeKind::Inclusive) {
            fields.push(TirStructField {
                name: "exhausted".to_string(),
                value: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, range.span),
                field_index: 2,
            });
        }

        let arg_names = vec![self.tysys.type_table.borrow().type_name(element_type)];
        let mangled_name = crate::name::mangle_generic_name(&struct_name, &arg_names);

        // Honour the recorded result type if present (annotate may
        // have unified with a more specific `RangeInclusive<i32>` etc.
        // already on `recorded_type`); reify trusts it as the final
        // expression type.
        let _ = recorded_type;

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_name,
                fields,
            },
            struct_type,
            range.span,
        )
    }

    /// Reify a named `StructLiteralExpr`. Field types come from
    /// `tysys.all_struct_fields`; the instance type + type_args for
    /// generic structs come from Gap 1's
    /// `sem.types.generic_instantiations[id]` record. Anonymous
    /// struct literals (`{ x: 1, y: 2 }` with no leading type name)
    /// flow through a different elaborator helper and are deferred to
    /// a follow-up.
    fn reify_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStructField};

        let Some(struct_name) = struct_lit.name.clone() else {
            // TODO(stage-5-bodies): anonymous struct literal —
            // `Elaborator::resolve_anonymous_struct_literal` synthesises a
            // struct based on the expected type's shape. Reify needs to
            // read the synthesised entry from
            // `sem.decls.pending_anonymous_structs` keyed off the
            // recorded type.
            todo!("reify_struct_literal: anonymous struct literal pending body-walk reify")
        };

        // Field positional info from the decl-interned struct.
        let lookup = self.type_lookup();
        let info = lookup.struct_fields(&struct_name);
        let field_names_to_index: crate::hashmap::IndexMap<String, (u32, TypeId)> = info
            .map(|info| {
                info.fields
                    .iter()
                    .enumerate()
                    .map(|(i, (n, t, _is_pub))| (n.clone(), (i as u32, *t)))
                    .collect()
            })
            .unwrap_or_default();

        // Instance type for generic structs is recorded by Gap 1; for
        // non-generic structs Gap 1's recording is skipped and we use
        // the bare struct type from `recorded_type`.
        let (struct_type, generic_args): (TypeId, Vec<TypeId>) = self
            .sem
            .types
            .generic_instantiations
            .get(&struct_lit.id)
            .map(|gi| (gi.instance_type, gi.type_args.clone()))
            .unwrap_or((recorded_type, Vec::new()));

        let mangled_struct_name = if generic_args.is_empty() {
            struct_name.clone()
        } else {
            let arg_names: Vec<String> = generic_args
                .iter()
                .map(|&t| self.tysys.type_table.borrow().type_name(t))
                .collect();
            crate::name::mangle_generic_name(&struct_name, &arg_names)
        };

        // Reify each AST field. Field order in the TIR follows the AST
        // (source order); the elaborator-side `field_index` lookup
        // pins the positional slot used by codegen / WIR field
        // accesses.
        let fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .map(|f| {
                let (field_index, expected_field_ty) = field_names_to_index
                    .get(&f.name)
                    .copied()
                    .unwrap_or((0, crate::tir::TypeTable::UNKNOWN));
                let value = self.reify_expr(&f.value, ctx, Some(expected_field_ty));
                TirStructField {
                    name: f.name.clone(),
                    value,
                    field_index,
                }
            })
            .collect();

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_struct_name,
                fields,
            },
            struct_type,
            struct_lit.span,
        )
    }

    /// Reify a compound assignment `x += y` / `x -= y` / etc. The
    /// elaborator desugars to `x = x op y` and routes through
    /// `assign_to_target`, which handles complex lvalues
    /// (`a[i] += x` etc.). Reify handles the common case: target is
    /// any expression that produces a writeable place; the desugared
    /// shape is `Assign { target, value: Binary { left: target, op,
    /// right: value } }`. The shared `reify_binary` path picks the
    /// native vs operator-trait dispatch for the inner op via Gap 11.
    fn reify_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::ast::CompoundAssignOp;
        use crate::tir::{TirExprKind, TypeTable};

        let op = match compound.op {
            CompoundAssignOp::Add => crate::tir::TirBinaryOp::Add,
            CompoundAssignOp::Sub => crate::tir::TirBinaryOp::Sub,
            CompoundAssignOp::Mul => crate::tir::TirBinaryOp::Mul,
            CompoundAssignOp::Div => crate::tir::TirBinaryOp::Div,
            CompoundAssignOp::Mod => crate::tir::TirBinaryOp::Mod,
            CompoundAssignOp::BitAnd => crate::tir::TirBinaryOp::BitAnd,
            CompoundAssignOp::BitOr => crate::tir::TirBinaryOp::BitOr,
            CompoundAssignOp::BitXor => crate::tir::TirBinaryOp::BitXor,
            CompoundAssignOp::Shl => crate::tir::TirBinaryOp::Shl,
            CompoundAssignOp::Shr => crate::tir::TirBinaryOp::Shr,
        };

        // The target appears twice in the desugared shape (as the
        // read for the binary op, and as the assignment target). The
        // elaborator side reifies it once and emits the same node
        // twice (it's pure for the lvalue shapes the elaborator
        // accepts); reify mirrors by walking the AST twice. For
        // simple-local lvalues both walks observe the same
        // `expression_types` entry so the type stays consistent.
        // Complex lvalues (`a[i] += x`) leave a follow-up: the
        // elaborator's `assign_to_target` synthesises an
        // `IndexMut::index_mut(idx)` extra call, and reify needs the
        // matching `IndexMutMethodCall` desugar tag on the target.
        let read = self.reify_expr(&compound.target, ctx, None);
        let rhs = self.reify_expr(&compound.value, ctx, Some(read.type_id));
        let combined_type = read.type_id;
        let combined = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(read),
                op,
                right: Box::new(rhs),
            },
            combined_type,
            compound.span,
        );
        // Re-walk the target for the assignment side. For the simple
        // local / global / field-access cases this reproduces the same
        // TIR shape; complex IndexMut targets would need the dedicated
        // index-mut rewrite path (Gap 3) — `reify_assign_to_target` is
        // the follow-up that ports that branch.
        let target_for_assign = self.reify_expr(&compound.target, ctx, None);
        let _ = recorded_type;
        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target_for_assign),
                value: Box::new(combined),
            },
            TypeTable::UNIT,
            compound.span,
        )
    }

    /// Reify the `?` postfix operator. The elaborator desugars
    /// `expr?` based on the operand's type:
    /// - `Option<T>`: `match expr { Some(v) => v, None => return null }`
    /// - `Result<T, E>`: `match expr { Ok(v) => v, Err(e) =>
    ///   return Err(From::from(e)) }`
    ///
    /// The annotate-side validation (operand is `Option` / `Result`,
    /// function return type is compatible) has already fired, so
    /// reify trusts the recorded shape and produces the matching
    /// `Match` TIR.
    fn reify_question_mark(
        &mut self,
        qm: &ast::TryOpExpr,
        ctx: &mut FunctionContext,
        _recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{ResolvedType, TirExprKind, TypeTable};

        let inner = self.reify_expr(&qm.expr, ctx, None);
        let inner_type = inner.type_id;

        let (is_option, is_result) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.as_option(inner_type).is_some(),
                matches!(
                    tt.get(inner_type),
                    ResolvedType::GenericInstance { name, .. } if name == "Result"
                ),
            )
        };

        if is_option {
            self.reify_question_mark_option(inner, ctx, qm.span)
        } else if is_result {
            self.reify_question_mark_result(inner, ctx, qm.span)
        } else {
            // Annotate already diagnosed; produce a Unit-typed
            // placeholder of `ERROR` so downstream phases see the
            // same shape annotate's recovery path produced.
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span)
        }
    }

    /// `Option<T>`'s `?`-op desugar — mirrors
    /// `Elaborator::resolve_question_mark_option` (expr.rs:4000+).
    fn reify_question_mark_option(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind, TypeTable};

        let inner_type = inner.type_id;
        let (some_type, some_name, none_name) = {
            let tt = self.tysys.type_table.borrow();
            let some_type = tt.as_option(inner_type).unwrap();
            let items = tt.compiler_items();
            (
                some_type,
                items
                    .variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
                    .to_string(),
                items
                    .variant_case_name(crate::compiler_item::CompilerItem::OptionNone)
                    .to_string(),
            )
        };

        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), some_type, false, None);

        let some_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: some_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: some_type,
                }],
                payload_type: some_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                some_type,
                span,
            ),
            span,
        };

        let none_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: none_name,
                bindings: vec![],
                payload_type: TypeTable::UNIT,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(TirExpr::new(TirExprKind::Null, inner_type, span)),
                        },
                        span,
                    )],
                    span,
                )),
                TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![some_arm, none_arm],
            },
            some_type,
            span,
        )
    }

    /// `Result<T, E>`'s `?`-op desugar — mirrors
    /// `Elaborator::resolve_question_mark_result` (expr.rs:4087+).
    /// The `From::from` synthesis path on mismatched error types is
    /// staged for a Stage 5 follow-up; the same-error-type case
    /// (most common in fixtures) lands here.
    fn reify_question_mark_result(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{ResolvedType, TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind};

        let inner_type = inner.type_id;
        let return_type = ctx.return_type;

        let (ok_type, inner_err_type) = match self.tysys.type_table.borrow().get(inner_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => panic!("reify_question_mark_result: ? operand must be Result<T, E>"),
        };
        let outer_err_type = match self.tysys.type_table.borrow().get(return_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                type_args[1]
            }
            _ => panic!("reify_question_mark_result: ? return type must be Result<U, F>"),
        };

        if inner_err_type != outer_err_type {
            // TODO(stage-5-bodies): `From::from(e)` synthesis path.
            // The elaborator's `resolve_from_call` builds a synthetic
            // static-method call to `<OuterErr>::from(<InnerErr>_val)`;
            // reify needs that synthesis recorded as a generic-call
            // annotation so it can emit the same `TirExprKind::Call`
            // shape without re-running dispatch.
            todo!("reify_question_mark_result: From::from error conversion pending")
        }

        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), ok_type, false, None);
        let e_local = ctx.add_local("__qm_e".to_string(), inner_err_type, false, None);

        let (ok_name, err_name, err_index) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            let (_, _, ok_n, _ok_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
            let (_, _, err_n, err_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
            (ok_n.to_string(), err_n.to_string(), err_i)
        };

        let ok_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: ok_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: ok_type,
                }],
                payload_type: ok_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                ok_type,
                span,
            ),
            span,
        };

        let e_expr = TirExpr::new(
            TirExprKind::Local {
                index: e_local,
                name: "__qm_e".to_string(),
            },
            inner_err_type,
            span,
        );
        let err_variant = TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: return_type,
                case_index: err_index,
                case_name: err_name.clone(),
                payload: Some(Box::new(e_expr)),
            },
            return_type,
            span,
        );

        let err_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: err_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_e".to_string(),
                    local_index: e_local,
                    type_id: inner_err_type,
                }],
                payload_type: inner_err_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(err_variant),
                        },
                        span,
                    )],
                    span,
                )),
                crate::tir::TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![ok_arm, err_arm],
            },
            ok_type,
            span,
        )
    }

    /// Reify a comparison chain `a < b < c …`. Mirrors
    /// `Elaborator::desugar_comparison_chain` (operators.rs:1313+):
    /// each middle term `m_k` binds to a `__m{k}` local so it is
    /// not re-evaluated, and the chain reduces to
    /// `(a < m_0) && (m_0 < m_1) && … && (m_{n-1} < tail)` wrapped
    /// in a block that holds the `__mK` bindings.
    ///
    /// Native primitive comparisons emit `TirExprKind::Binary`
    /// directly; non-primitive operands route through trait
    /// dispatch in the elaborator. The trait-dispatch path inside
    /// the chain is staged for a Stage 5 follow-up: the synthesised
    /// inner comparisons have no source AST id, so Gap 11's
    /// `operator_dispatch` record (keyed by AstId) doesn't catch
    /// them. The primitive-only path covers the common fixture
    /// shape (`x < y && y < z`); non-primitive chains fall to the
    /// recovery shape (native Binary on whatever types the operands
    /// land at, plus the elaborator's `RequiresTrait` diagnostic
    /// will have already fired on the annotate side).
    fn reify_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirBinaryOp, TirBlock, TirExprKind, TirStmtKind, TypeTable};

        if chain.comparisons.is_empty() {
            // Degenerate parse — annotate emits `chain.first` as-is.
            return self.reify_expr(&chain.first, ctx, None);
        }

        if chain.comparisons.len() == 1 {
            let cmp = &chain.comparisons[0];
            let left = self.reify_expr(&chain.first, ctx, None);
            let right = self.reify_expr(&cmp.right, ctx, Some(left.type_id));
            let recorded_type = self
                .sem
                .types
                .expression_types
                .get(&chain.id)
                .copied()
                .unwrap_or(TypeTable::BOOL);
            return TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(left),
                    op: ast_binary_op_to_tir(cmp.op),
                    right: Box::new(right),
                },
                recorded_type,
                cmp.op_span,
            );
        }

        ctx.enter_scope();
        let mut stmts: Vec<TirStmt> = Vec::new();

        let cmp0 = &chain.comparisons[0];
        let first_tir = self.reify_expr(&chain.first, ctx, None);
        let right0_tir = self.reify_expr(&cmp0.right, ctx, Some(first_tir.type_id));

        // Bind first middle to `__m0`.
        let m0_type = right0_tir.type_id;
        let m0_name = "__m0".to_string();
        let m0_index = ctx.add_local(m0_name.clone(), m0_type, false, None);
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: m0_name.clone(),
                local_index: m0_index,
                is_mut: false,
                is_reactive: false,
                type_id: m0_type,
                value: right0_tir,
                skip_value_copy: false,
            },
            chain.span,
        ));
        let m0_ref = TirExpr::new(
            TirExprKind::Local {
                index: m0_index,
                name: m0_name,
            },
            m0_type,
            chain.span,
        );

        let mut acc_tir = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(first_tir),
                op: ast_binary_op_to_tir(cmp0.op),
                right: Box::new(m0_ref.clone()),
            },
            TypeTable::BOOL,
            cmp0.op_span,
        );
        let mut prev_tir = m0_ref;

        let last_idx = chain.comparisons.len() - 1;
        for idx in 1..chain.comparisons.len() {
            let cmp = &chain.comparisons[idx];
            let raw_right = self.reify_expr(&cmp.right, ctx, Some(prev_tir.type_id));
            let right_tir = if idx == last_idx {
                raw_right
            } else {
                let m_type = raw_right.type_id;
                let m_name = format!("__m{idx}");
                let m_index = ctx.add_local(m_name.clone(), m_type, false, None);
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: m_name.clone(),
                        local_index: m_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: m_type,
                        value: raw_right,
                        skip_value_copy: false,
                    },
                    chain.span,
                ));
                TirExpr::new(
                    TirExprKind::Local {
                        index: m_index,
                        name: m_name,
                    },
                    m_type,
                    chain.span,
                )
            };
            let next_prev = right_tir.clone();
            let cmp_tir = TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(prev_tir),
                    op: ast_binary_op_to_tir(cmp.op),
                    right: Box::new(right_tir),
                },
                TypeTable::BOOL,
                cmp.op_span,
            );
            acc_tir = TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(acc_tir),
                    op: TirBinaryOp::And,
                    right: Box::new(cmp_tir),
                },
                TypeTable::BOOL,
                chain.span,
            );
            prev_tir = next_prev;
        }

        ctx.exit_scope();

        stmts.push(TirStmt::new(TirStmtKind::Expr(acc_tir), chain.span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, chain.span)),
            TypeTable::BOOL,
            chain.span,
        )
    }

    /// Reify an `expr[idx]` index expression.
    ///
    /// Three shapes per `Elaborator::resolve_index` (expr.rs:1659+):
    /// - Tuple constant index → `TirExprKind::FieldAccess` with the
    ///   constant index as `field_index` / `field_name`.
    /// - `Index` trait dispatch → `*receiver.index(idx)` wrapped in
    ///   `Unary { Deref }`; the `operator_dispatch[index.id]` record
    ///   (Gap 11) carries the dispatch target. The `Ref(Output)`
    ///   `return_type` on the record is the signal that the outer
    ///   `Deref` wrap is needed.
    /// - `IndexValue` trait dispatch → `receiver.index_value(idx)`
    ///   returns the value by copy; no outer wrap.
    fn reify_index(
        &mut self,
        index: &ast::IndexExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, ResolvedType, TirExprKind, TirUnaryOp, TypeTable};

        let receiver = self.reify_expr(&index.expr, ctx, None);

        // Tuple constant-index path: detect via the receiver's
        // resolved type + the index being a constant integer
        // literal. Matches `Elaborator::resolve_index`'s tuple
        // branch (expr.rs:1674+).
        let tuple_elems: Option<Vec<TypeId>> = {
            let tt = self.tysys.type_table.borrow();
            let base = receiver.type_id;
            let unwrapped = match tt.get(base) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                _ => base,
            };
            tt.as_tuple(unwrapped)
        };
        if let Some(elems) = tuple_elems
            && let ast::Expr::Literal(lit) = &index.index
            && let ast::Literal::Number(repr) = &lit.value
            && let Ok(idx) = repr.parse::<usize>()
            && idx < elems.len()
        {
            return TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(receiver),
                    field_index: idx as u32,
                    field_name: idx.to_string(),
                },
                elems[idx],
                index.span,
            );
        }

        // Operator-dispatch path: Gap 11's `operator_dispatch[index.id]`
        // carries the resolved Index / IndexValue trait method. The
        // `return_type` on the record signals whether the outer
        // `Deref` wrap applies (Index returns `&Output`; IndexValue
        // returns `Output`).
        if let Some(dispatch) = self.sem.types.operator_dispatch.get(&index.id).cloned() {
            let adjusted_receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                receiver,
                dispatch.self_kind,
                false,
                index.span,
                &self.tysys.type_table,
            );
            let idx_expr = self.reify_expr(&index.index, ctx, None);
            let method_call = super::Elaborator::<H>::build_tir_method_call(
                adjusted_receiver,
                dispatch.function_ref,
                vec![],
                vec![CallArg::new(idx_expr, false)],
                dispatch.return_type,
                index.span,
            );
            // `Index` trait returns `&Output`, so the outer wrap is
            // a `Deref`. Detect via the recorded `return_type`'s
            // `Ref` shape — IndexValue's record has the raw output
            // type and skips the wrap.
            let needs_deref = matches!(
                self.tysys.type_table.borrow().get(dispatch.return_type),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_),
            );
            if needs_deref {
                return TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: Box::new(method_call),
                    },
                    recorded_type,
                    index.span,
                );
            }
            return method_call;
        }

        // No dispatch recorded → the elaborator emitted a recovery
        // shape (annotate would have diagnosed missing trait impl).
        // Match the recovery output with a Unit placeholder typed
        // as ERROR.
        let _ = recorded_type;
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, index.span)
    }

    /// Reify a closure expression. The Gap 4 record
    /// (`sem.types.closure_captures[closure.id]`) carries the
    /// capture-analysis result annotate computed:
    /// - `mut_captures`: outer mut-locals to materialise as
    ///   `let __ref_v = &mut v;` before the closure body opens, in
    ///   declaration order.
    /// - `captures`: the closure's final capture list (name +
    ///   outer-index + type + mut flag).
    /// - `is_mutating`: drives the `fn mut(...)` vs `fn(...)` tag on
    ///   the closure type.
    ///
    /// Mirror `Elaborator::resolve_closure` (closure.rs:127+) step
    /// by step so the walk-order invariant (Gap 7) lands the
    /// same locals at the same indices.
    fn reify_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        use crate::tir::{
            ResolvedType, TirBlock, TirCapture, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable,
        };

        let span = closure.span;

        let cap_info = self
            .sem
            .types
            .closure_captures
            .get(&closure.id)
            .cloned()
            .unwrap_or_else(|| super::sem::types::ClosureCaptureInfo {
                mut_captures: Vec::new(),
                captures: Vec::new(),
                is_mutating: false,
            });

        // Step 1 (replay): materialise outer-scope `__ref_v` locals
        // for each mut-capture in the recorded order; emit the
        // matching `let __ref_v = &mut v;` TIR; register
        // `deref_overrides` so the closure body's references to
        // captured mut-locals dereference the proxy.
        let mut ref_stmts: Vec<TirStmt> = Vec::new();
        let mut deref_overrides: crate::hashmap::IndexMap<String, (String, TypeId)> =
            crate::hashmap::IndexMap::default();
        for mc in &cap_info.mut_captures {
            ctx.add_local(mc.ref_name.clone(), mc.ref_type, false, None);
            ctx.address_taken_locals.insert(mc.outer_index);
            ref_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: mc.ref_name.clone(),
                    local_index: ctx.next_local - 1,
                    is_mut: false,
                    is_reactive: false,
                    type_id: mc.ref_type,
                    value: TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: mc.outer_index,
                                    name: mc.var_name.clone(),
                                },
                                mc.inner_type,
                                span,
                            )),
                        },
                        mc.ref_type,
                        span,
                    ),
                    skip_value_copy: false,
                },
                span,
            ));
            deref_overrides.insert(mc.var_name.clone(), (mc.ref_name.clone(), mc.inner_type));
        }

        // Step 2: open the closure context with the deref overrides.
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.tysys.type_table);
        closure_ctx.deref_overrides = deref_overrides;

        // Step 3: add closure parameters. Param types come from the
        // AST (resolved via the outer scope's type-param view); if
        // the expected fn type is available, prefer its param types
        // for cases where the closure has no explicit annotation.
        let expected_fn_params: Option<Vec<TypeId>> = expected_type.and_then(|t| {
            match self.tysys.type_table.borrow().get(t) {
                ResolvedType::Function { params, .. } => Some(params.clone()),
                _ => None,
            }
        });
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let type_id = if let Some(ty) = &p.ty {
                    self.resolve_type(ty)
                } else if let Some(ref fn_params) = expected_fn_params {
                    fn_params.get(i).copied().unwrap_or(TypeTable::UNKNOWN)
                } else {
                    TypeTable::UNKNOWN
                };
                closure_ctx.add_local(p.name.clone(), type_id, p.is_mut, Some(p.id));
                (p.name.clone(), type_id)
            })
            .collect();

        // Step 4: reify the body in the closure scope.
        let body_expected = expected_type.and_then(|t| match self.tysys.type_table.borrow().get(t) {
            ResolvedType::Function { return_type, .. } => Some(*return_type),
            _ => None,
        });
        let body = self.reify_expr(&closure.body, &mut closure_ctx, body_expected);

        // Step 5: assemble the capture list from the recorded entries.
        let captures: Vec<TirCapture> = cap_info
            .captures
            .iter()
            .map(|c| TirCapture {
                name: c.name.clone(),
                outer_index: c.outer_index,
                type_id: c.type_id,
                is_mut: c.is_mut,
            })
            .collect();

        // Step 6: determine the return type. Prefer the body's
        // resolved type (which annotate already unified) over the
        // recorded `recorded_type` for the inner — `recorded_type`
        // is the closure expression's function type, not the return
        // type.
        let return_type = body.type_id;

        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.tysys.type_table.borrow_mut().make_function_with_mut(
            cap_info.is_mutating,
            param_types,
            return_type,
            Vec::new(),
            Vec::new(),
        );

        let mut all_locals = closure_ctx.locals;
        let body_locals = if params.len() <= all_locals.len() {
            all_locals.split_off(params.len())
        } else {
            Vec::new()
        };
        let address_taken_locals = closure_ctx.address_taken_locals;

        let declared_effects = expected_type.and_then(|t| match self.tysys.type_table.borrow().get(t) {
            ResolvedType::Function { effects, .. } if !effects.is_empty() => Some(effects.clone()),
            _ => None,
        });

        let closure_tir = TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
                functor_id: None,
                address_taken_locals,
                body_locals,
                declared_effects,
            },
            func_type,
            span,
        );

        // Step 7: wrap in a Block when ref_stmts materialised any
        // outer-scope `__ref_v` bindings.
        if ref_stmts.is_empty() {
            let _ = recorded_type;
            return closure_tir;
        }

        let mut stmts = ref_stmts;
        stmts.push(TirStmt::new(TirStmtKind::Expr(closure_tir), span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, span)),
            func_type,
            span,
        )
    }

    /// Reify a `matches!`-style expression: `scrutinee matches { PAT
    /// [if guard] }`. Desugars (tagged `DesugarKind::Matches` at
    /// annotate time) into a two-arm match: pattern → true, wildcard
    /// → false. Mirror `Elaborator::desugar_matches_expr`
    /// (matches.rs:25+).
    fn reify_matches(&mut self, m: &ast::MatchesExpr, ctx: &mut FunctionContext) -> TirExpr {
        use crate::tir::{TirExprKind, TirMatchArm, TirPattern, TypeTable};

        let scrutinee = self.reify_expr(&m.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        ctx.enter_scope();
        let pattern_tir = self.reify_pattern(&m.pattern, scrutinee_type, ctx);
        let arm_body = match &m.guard {
            Some(guard) => self.reify_expr(guard, ctx, Some(TypeTable::BOOL)),
            None => TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, m.span),
        };
        ctx.exit_scope();

        let arms = vec![
            TirMatchArm {
                pattern: pattern_tir,
                guard: None,
                body: arm_body,
                span: m.span,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, m.span),
                span: m.span,
            },
        ];

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            TypeTable::BOOL,
            m.span,
        )
    }

    /// Reify a `MatchExpr`. The scrutinee is walked; each arm enters
    /// its own scope, reifies the pattern (which adds bindings to
    /// `ctx`), reifies the optional guard at `Bool`, and reifies the
    /// body at the match's `expected_type`. The result `TypeId` is
    /// the recorded type — annotate already unified arm body types
    /// into it.
    fn reify_match_expr(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirMatchArm, TypeTable};

        let scrutinee = self.reify_expr(&match_expr.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| {
                ctx.enter_scope();
                let pattern = self.reify_pattern(&arm.pattern, scrutinee_type, ctx);
                let guard = arm
                    .guard
                    .as_ref()
                    .map(|g| self.reify_expr(g, ctx, Some(TypeTable::BOOL)));
                let body = self.reify_expr(&arm.body, ctx, expected_type);
                ctx.exit_scope();
                TirMatchArm {
                    pattern,
                    guard,
                    body,
                    span: arm.span,
                }
            })
            .collect();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            recorded_type,
            match_expr.span,
        )
    }

    /// Reify a `CallExpr`. Stage 5 covers the common shapes: bare-ident
    /// callees that resolve to a current-module or imported free
    /// function (`TirExprKind::Call`), and qualified-ident
    /// variant-constructor calls (`Some(x)`, `Result::Ok(v)`)
    /// emitted as `TirExprKind::VariantConstruct` with a payload.
    /// Closure-call, indirect-callee, static-method, qualified-enum,
    /// and qualified-flags shapes route through `todo!` until each
    /// branch is ported — `Elaborator::resolve_call` (call.rs:200+)
    /// is the source they mirror.
    fn reify_call(
        &mut self,
        call: &ast::CallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, TirExprKind};

        let span = call.span;

        // Variant-ctor call shape: `Variant::Case(payload)`. Detected
        // via the callee being a qualified ident whose prefix names a
        // variant decl with a matching case. Generic variants pin
        // their `instance_type` on `sem.types.generic_instantiations`
        // via Gap 1; non-generic ones use the bare `Variant` type.
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some(pos) = ident.name.find("::")
        {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];
            if !suffix.contains("::") {
                let lookup = self.type_lookup();
                if let Some(variant_info) = lookup.variant_case(prefix).cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == suffix)
                        .map(|(i, c)| (i, c.clone()))
                {
                    let variant_type = self
                        .sem
                        .types
                        .generic_instantiations
                        .get(&call.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    let payload = call.args.first().map(|arg_expr| {
                        Box::new(self.reify_expr(arg_expr, ctx, Some(case_data.payload)))
                    });
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload,
                        },
                        variant_type,
                        span,
                    );
                }
            }
        }

        // Free-function call: bare-ident callee that names a current-
        // module or imported function.
        if let ast::Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
        {
            let (callee_module, callee_name) = if self
                .sem
                .decls
                .function_return_types
                .contains_key(&ident.name)
            {
                (self.current_module_source.clone(), ident.name.clone())
            } else if let Some(import_src) = self
                .sem
                .imports
                .imported_type_sources
                .get(&ident.name)
                .cloned()
            {
                let original_name = self
                    .sem
                    .imports
                    .import_original_names
                    .get(&ident.name)
                    .cloned()
                    .unwrap_or_else(|| ident.name.clone());
                (import_src, original_name)
            } else {
                // The callee neither matches a local function nor an
                // import — it might be a closure-call, static method,
                // or a misresolved name. The full dispatch lives in
                // `Elaborator::resolve_call`; route to `todo!` until
                // the remaining branches port.
                todo!(
                    "reify_call: callee `{}` not in current_module / imports — \
                     closure / static-method / namespace-import dispatch pending",
                    ident.name
                );
            };

            // Type args: explicit turbofish on the call expression,
            // else the inference recorded by Gap 1.
            let type_args: Vec<TypeId> = if !call.type_args.is_empty() {
                call.type_args
                    .iter()
                    .map(|ty| self.resolve_type(ty))
                    .collect()
            } else {
                self.sem
                    .types
                    .generic_instantiations
                    .get(&call.id)
                    .map(|gi| gi.type_args.clone())
                    .unwrap_or_default()
            };

            // TODO(stage-5-bodies): callee param types drive literal
            // re-coercion + `is_mut` per-arg. Until those records
            // land, reify resolves each arg with `expected = None`
            // and emits `is_mut = false`; matches the elaborator's
            // output for the no-coercion / no-mut common case.
            let args: Vec<CallArg> = call
                .args
                .iter()
                .map(|a| {
                    let arg = self.reify_expr(a, ctx, None);
                    CallArg::new(arg, false)
                })
                .collect();

            return TirExpr::new(
                TirExprKind::Call {
                    func: crate::tir::FunctionRef {
                        module_source: callee_module,
                        name: callee_name,
                        monomorph_info: None,
                        method_info: None,
                    },
                    type_args,
                    args,
                },
                recorded_type,
                span,
            );
        }

        // TODO(stage-5-bodies): closure-call (callee is a local with
        // fn type), indirect call (non-ident callee), and
        // qualified-callee static-method / enum-ctor /
        // flags-constructor (`none()`, `all()`) shapes all live behind
        // this `todo!`. Each branch in `Elaborator::resolve_call`
        // (call.rs:200+) maps to its own reify arm; the dispatcher
        // shape carries through unchanged so partial ports compose.
        todo!("reify_call: non-free-function callee pending body-walk reify")
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
        let adjusted_receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
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
    fn lookup_struct_field_index(&self, receiver_type: TypeId, field_name: &str) -> (u32, String) {
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
    /// enum / variant ctors) read [`super::sem::ModuleDecls`] to pick
    /// the right TIR shape; full coverage of every kind is staged.
    fn reify_ident(
        &mut self,
        ident: &ast::IdentExpr,
        recorded_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::TirExprKind;

        // 1. Local binding (parameter, let, closure param) — walk-order
        //    invariant guarantees the same index annotate produced.
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

        // 2. Current-module global.
        if self
            .sem
            .decls
            .current_module_globals
            .contains_key(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                recorded_type,
                ident.span,
            );
        }

        // 3. Imported global.
        if let Some((src, original_name, _ty, _is_mut)) =
            self.sem.decls.imported_globals.get(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: src.clone(),
                    name: original_name.clone(),
                },
                recorded_type,
                ident.span,
            );
        }

        // 4. Associated constant (e.g. `f64::PI`, `i32::MAX`). The
        //    elaborator inlines these to the resolved expression at
        //    every use site; reify reproduces the same inlining by
        //    re-reifying the constant's `Expr` from
        //    `sem.decls.associated_constants`. The constant's body is
        //    independent of the call site's scope (a pure literal /
        //    static expression in practice), so reify uses the
        //    surrounding `ctx` directly — matches the elaborator's
        //    `resolve_expr(&const_expr, ctx, …)` (expr.rs:594–605).
        if let Some((const_ty, const_expr)) = self
            .sem
            .decls
            .associated_constants
            .get(&ident.name)
            .cloned()
        {
            let type_id = self.resolve_type(&const_ty);
            let resolved = self.reify_expr(&const_expr, ctx, Some(type_id));
            return TirExpr::new(resolved.kind, type_id, ident.span);
        }

        // 5. Free function reference — the ident names a function in
        //    the current module or imported via a `use` declaration.
        //    Emit `TirExprKind::FuncRef` with the recorded
        //    instantiation's type_args when present.
        if self
            .sem
            .decls
            .function_return_types
            .contains_key(&ident.name)
        {
            let type_args = self
                .sem
                .types
                .generic_instantiations
                .get(&ident.id)
                .map(|gi| gi.type_args.clone())
                .unwrap_or_default();
            return TirExpr::new(
                TirExprKind::FuncRef {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                    type_args,
                },
                recorded_type,
                ident.span,
            );
        }
        if let Some(import_src) = self
            .sem
            .imports
            .imported_type_sources
            .get(&ident.name)
            .cloned()
        {
            let original_name = self
                .sem
                .imports
                .import_original_names
                .get(&ident.name)
                .cloned()
                .unwrap_or_else(|| ident.name.clone());
            // Imports through `use` collapse types + functions into the
            // same `imported_type_sources` map; the type / variant /
            // enum / flags / resource cases were already handled
            // above and would have returned. Anything left here is a
            // function import.
            let type_args = self
                .sem
                .types
                .generic_instantiations
                .get(&ident.id)
                .map(|gi| gi.type_args.clone())
                .unwrap_or_default();
            return TirExpr::new(
                TirExprKind::FuncRef {
                    module_source: import_src,
                    name: original_name,
                    type_args,
                },
                recorded_type,
                ident.span,
            );
        }

        // 6. Qualified case path `Type::Case`. Variant / enum / flags
        //    are checked in the same priority order as
        //    `Elaborator::resolve_ident` (expr.rs:607+). The
        //    namespace-import form `ns::Type::Case` (two `::`
        //    separators) is handled by a dedicated branch in the
        //    elaborator that resolves the namespace alias first;
        //    that path stays a `todo!` until the dispatcher gets it.
        if let Some(pos) = ident.name.find("::") {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];

            // Two-segment qualified path is "Type::Case". Anything with
            // a further `::` is `ns::Type::Case` (namespace path) —
            // defer to a later branch.
            if !suffix.contains("::") {
                let lookup = self.type_lookup();

                // Variant case.
                if let Some(variant_info) = lookup.variant_case(prefix).cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == suffix)
                        .map(|(i, c)| (i, c.clone()))
                {
                    // Generic variants record the instance type +
                    // type_args via Gap 1 — read them. Non-generic
                    // variants leave no record and the bare
                    // `recorded_type` already names the right
                    // `Variant` TypeId.
                    let variant_type = self
                        .sem
                        .types
                        .generic_instantiations
                        .get(&ident.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload: None,
                        },
                        variant_type,
                        ident.span,
                    );
                }

                // Enum case.
                if let Some(enum_info) = lookup.enum_case(prefix).cloned()
                    && let Some(case_data) = enum_info.find_case(suffix).cloned()
                {
                    let enum_type = self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .make_enum(enum_info.name.clone(), enum_info.module_source);
                    return TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type,
                            case_index: case_data.index,
                            case_name: case_data.name,
                        },
                        enum_type,
                        ident.span,
                    );
                }

                // Flags member.
                if let Some(flags_info) = lookup.flags_case(prefix).cloned()
                    && let Some(member) = flags_info
                        .members
                        .iter()
                        .find(|m| m.name == suffix)
                        .cloned()
                {
                    return TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: u64::from(member.bitmask),
                            repr: member.bitmask.to_string(),
                        },
                        flags_info.type_id,
                        ident.span,
                    );
                }
            }
        }

        // 7. Namespace-imported path `ns::Type::Case` (and variants
        //    with type args). The `ns::` prefix maps via
        //    `sem.imports.namespace_imports` to the namespace's
        //    source module; reify then resolves against the
        //    namespace's `tysys.all_*` tables (rather than the
        //    current module's).
        if let Some(double_colon) = ident.name.find("::") {
            let ns_prefix = &ident.name[..double_colon];
            let rest = &ident.name[double_colon + 2..];
            if let Some(ns_source) = self.sem.imports.namespace_imports.get(ns_prefix).cloned()
                && let Some(inner_double_colon) = rest.find("::")
            {
                let type_name = &rest[..inner_double_colon];
                let case_name = &rest[inner_double_colon + 2..];

                // Variant case in the namespace's module.
                if let Some(variant_info) = self
                    .tysys
                    .all_variant_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == case_name)
                        .map(|(i, c)| (i, c.clone()))
                {
                    let variant_type = self
                        .sem
                        .types
                        .generic_instantiations
                        .get(&ident.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or_else(|| {
                            self.tysys.type_table.borrow_mut().make_variant(
                                variant_info.name.clone(),
                                variant_info.module_source.clone(),
                            )
                        });
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload: None,
                        },
                        variant_type,
                        ident.span,
                    );
                }

                // Enum case in the namespace's module.
                if let Some(enum_info) = self
                    .tysys
                    .all_enum_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned()
                    && let Some(case_data) = enum_info.find_case(case_name).cloned()
                {
                    let enum_type = self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .make_enum(enum_info.name.clone(), enum_info.module_source);
                    return TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type,
                            case_index: case_data.index,
                            case_name: case_data.name,
                        },
                        enum_type,
                        ident.span,
                    );
                }
            }
        }

        // No remaining recognised ident kind — the elaborator would
        // have diagnosed an unknown identifier at annotate time.
        // Match the elaborator's recovery shape so reify doesn't
        // panic on a known-bad input.
        let _ = recorded_type;
        TirExpr::new(
            TirExprKind::Unit,
            crate::tir::TypeTable::ERROR,
            ident.span,
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
            ast::Literal::LocationFunction => TirExprKind::StringLiteral(ctx.function_name.clone()),
            ast::Literal::LocationFile => {
                // `#file` — current module source as a string.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                return TirExpr::new(
                    TirExprKind::StringLiteral(self.current_module_source.to_string()),
                    string_type,
                    lit.span,
                );
            }
            ast::Literal::LocationLine => {
                // `#line` — 1-indexed line number; matches the
                // elaborator's `I32` typing.
                let line = lit.span.line as u64;
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    crate::tir::TypeTable::I32,
                    lit.span,
                );
            }
            ast::Literal::DataSection => {
                // `#data` — the loaded module's `__DATA__` section.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                let data = self
                    .loaded_modules
                    .get(&self.current_module_source)
                    .and_then(|m| m.data_section())
                    .map(str::to_owned)
                    .unwrap_or_default();
                return TirExpr::new(
                    TirExprKind::StringLiteral(data),
                    string_type,
                    lit.span,
                );
            }
            ast::Literal::IncludeStr(raw_path) => {
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let value = self
                    .tysys
                    .included_files
                    .get(&key)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .map(str::to_owned)
                    .unwrap_or_default();
                return TirExpr::new(
                    TirExprKind::StringLiteral(value),
                    string_type,
                    lit.span,
                );
            }
            ast::Literal::IncludeBytes(raw_path) => {
                let array_u8_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_array(crate::tir::TypeTable::U8);
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let bytes = self
                    .tysys
                    .included_files
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                return TirExpr::new(
                    TirExprKind::BytesLiteral(bytes),
                    array_u8_type,
                    lit.span,
                );
            }
        };
        TirExpr::new(kind, recorded_type, lit.span)
    }

    /// Reify a pattern in a `let`, `match`, `if let`, or `while let`.
    /// Binding patterns add locals to `ctx` in the same order annotate
    /// did (per the walk-order invariant). The variant binding order
    /// mirrors `Elaborator::resolve_if_pattern_inner`'s recursion.
    pub(super) fn reify_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Wildcard => TirPattern::Wildcard,
            ast::Pattern::Ident { id, name, span: _ } => {
                let local_index = ctx.add_local(
                    name.clone(),
                    scrutinee_type,
                    /* is_mut */ false,
                    Some(*id),
                );
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id: scrutinee_type,
                }
            }
            ast::Pattern::MutIdent { id, name, span: _ } => {
                let local_index = ctx.add_local(
                    name.clone(),
                    scrutinee_type,
                    /* is_mut */ true,
                    Some(*id),
                );
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id: scrutinee_type,
                }
            }
            ast::Pattern::Literal(lit) => {
                let lit_pat = ast_literal_to_pattern(lit);
                TirPattern::Literal(lit_pat)
            }
            ast::Pattern::Tuple(elements, has_rest) => {
                // Tuple patterns destructure into the scrutinee's
                // element types. The elaborator already validated
                // arity; reify reads `tysys.type_table.as_tuple` to
                // get the per-element types, falling back to
                // UNKNOWN-typed inner walks for type-pack scrutinees.
                let elem_types: Vec<TypeId> = self
                    .tysys
                    .type_table
                    .borrow()
                    .as_tuple(scrutinee_type)
                    .unwrap_or_default();
                let sub_patterns: Vec<TirPattern> = elements
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let elem_ty = elem_types
                            .get(i)
                            .copied()
                            .unwrap_or(crate::tir::TypeTable::UNKNOWN);
                        self.reify_pattern(p, elem_ty, ctx)
                    })
                    .collect();
                TirPattern::Tuple(sub_patterns, *has_rest)
            }
            ast::Pattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                // Variant patterns appear in `match Some(x) { Some(v) => …
                // }` etc. The case's payload type lives on
                // `tysys.all_variant_cases`; reify reads it to give
                // sub-patterns the right scrutinee type. The
                // `variant_name` strips a `Variant::` prefix when
                // present (the AST keeps the qualified form).
                let case_name = variant_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(variant_name)
                    .to_string();

                // Resolve the variant decl + case payload.
                let (payload_type, _payload_decl_module) = {
                    use crate::tir::ResolvedType;
                    let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
                    let (decl_name, type_args) = match resolved {
                        ResolvedType::Variant {
                            name,
                            module_source,
                        } => (name, (Vec::<TypeId>::new(), module_source)),
                        ResolvedType::GenericInstance {
                            name,
                            module_source,
                            type_args,
                        } => (name, (type_args, module_source)),
                        _ => (
                            String::new(),
                            (Vec::new(), self.current_module_source.clone()),
                        ),
                    };
                    let payload =
                        self.get_variant_case_payload_type(&decl_name, &case_name, &type_args.0);
                    (payload, type_args.1)
                };

                let sub_patterns: Vec<TirPattern> = bindings
                    .iter()
                    .map(|p| self.reify_pattern(p, payload_type, ctx))
                    .collect();
                TirPattern::Variant {
                    enum_type: scrutinee_type,
                    variant_name: case_name,
                    bindings: sub_patterns,
                    payload_type,
                }
            }
            ast::Pattern::Or(alternatives) => {
                // Or patterns match any alternative. Each alternative
                // reifies against the same scrutinee type. Annotate
                // already validated that alternatives bind the same
                // set of names; reify trusts the validated shape and
                // only forwards the recursion.
                let sub: Vec<TirPattern> = alternatives
                    .iter()
                    .map(|p| self.reify_pattern(p, scrutinee_type, ctx))
                    .collect();
                TirPattern::Or(sub)
            }
            ast::Pattern::Range {
                start, end, kind, ..
            } => {
                use crate::ast::RangeKind;
                use crate::tir::{PrimitiveType, ResolvedType};
                let inclusive = matches!(kind, RangeKind::Inclusive);
                let start_val = pattern_endpoint_to_i128(start);
                let end_val = pattern_endpoint_to_i128(end);
                let is_unsigned = matches!(
                    self.tysys.type_table.borrow().get(scrutinee_type),
                    ResolvedType::Primitive(
                        PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::U64
                            | PrimitiveType::U128,
                    )
                );
                TirPattern::Range {
                    start: start_val,
                    end: end_val,
                    inclusive,
                    is_unsigned,
                }
            }
            ast::Pattern::Struct {
                type_name,
                fields,
                has_rest,
                ..
            } => self.reify_struct_pattern(
                type_name.as_deref(),
                fields,
                *has_rest,
                scrutinee_type,
                ctx,
            ),
        }
    }

    /// Reify a struct destructuring pattern `Point { x, y }` or
    /// `{ x, y }` (anonymous). The struct's field-name → index map
    /// comes from `tysys.all_struct_fields`; sub-patterns recurse
    /// against the declared field type. Mirrors
    /// `Elaborator::resolve_struct_pattern`'s shape; shorthand
    /// `{ x }` (== `{ x: x }`) is encoded by the AST having the
    /// sub-pattern be an `Ident { name: x }` either way.
    fn reify_struct_pattern(
        &mut self,
        type_name: Option<&str>,
        fields: &[ast::StructPatternField],
        has_rest: bool,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        use crate::tir::{ResolvedType, TirStructPatternField};

        // Determine the struct name: explicit `Type::Pattern` wins;
        // otherwise read from the scrutinee.
        let scrutinee_struct_name = match self.tysys.type_table.borrow().get(scrutinee_type).clone()
        {
            ResolvedType::Struct { name, .. } => name,
            ResolvedType::GenericInstance { name, .. } => name,
            _ => String::new(),
        };
        let lookup_name = type_name.unwrap_or(&scrutinee_struct_name);

        // Decl-interned struct info for field-name → (index, type)
        // lookup. Falls back to UNKNOWN-typed sub-patterns for
        // anonymous / unresolved scrutinees (matches annotate's
        // recovery shape).
        let field_info: crate::hashmap::IndexMap<String, (u32, TypeId)> = {
            let lookup = self.type_lookup();
            lookup
                .struct_fields(lookup_name)
                .map(|info| {
                    info.fields
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t, _))| (n.clone(), (i as u32, *t)))
                        .collect()
                })
                .unwrap_or_default()
        };

        let tir_fields: Vec<TirStructPatternField> = fields
            .iter()
            .map(|f| {
                let (field_index, field_ty) = field_info
                    .get(&f.field_name)
                    .copied()
                    .unwrap_or((0, crate::tir::TypeTable::UNKNOWN));
                let pattern = self.reify_pattern(&f.pattern, field_ty, ctx);
                TirStructPatternField {
                    field_name: f.field_name.clone(),
                    field_index,
                    pattern,
                }
            })
            .collect();

        TirPattern::Struct {
            struct_type: scrutinee_type,
            fields: tir_fields,
            has_rest,
        }
    }

    /// Look up a variant case's payload type, substituted with the
    /// scrutinee's type args. Reify-side mirror of the elaborator's
    /// `Elaborator::get_variant_case_payload_type`; the lookup walks
    /// `tysys.all_variant_cases` and the local-module override map
    /// via [`TypeLookup`], so the same `TypeId` annotate produced
    /// lands on the reified pattern.
    fn get_variant_case_payload_type(
        &self,
        variant_name: &str,
        case_name: &str,
        type_args: &[TypeId],
    ) -> TypeId {
        let lookup = self.type_lookup();
        let Some(variant_info) = lookup.variant_case(variant_name) else {
            return crate::tir::TypeTable::UNKNOWN;
        };
        let Some(case_data) = variant_info.cases.iter().find(|c| c.name == case_name) else {
            return crate::tir::TypeTable::UNKNOWN;
        };
        if type_args.is_empty() {
            return case_data.payload;
        }
        // Substitute decl type params with concrete `type_args` in the
        // payload type.
        let param_map: crate::hashmap::IndexMap<TypeId, TypeId> = variant_info
            .type_param_type_ids
            .iter()
            .zip(type_args.iter())
            .map(|(&p, &t)| (p, t))
            .collect();
        let _ = param_map;
        // Stage 5 follow-up: full substitution needs the elaborator's
        // `substitute_type_params_by_map`; until that helper is
        // factored to a free function, fall back to the raw payload
        // for non-generic cases (most variants in current fixtures)
        // and accept the same `UNKNOWN` slot as
        // `Elaborator::get_variant_case_payload_type` would for the
        // un-substitutable path.
        case_data.payload
    }
}

/// Decode a range-pattern endpoint (`a..<b` / `a..=b`) into its
/// `i128` value. The endpoint syntactic form is itself a `Pattern`:
/// either a `Literal(Number)` or a `Literal(Char)` (for char-range
/// patterns). Char endpoints lower to their codepoint as `i128`;
/// numeric endpoints reuse the same hex / oct / bin recogniser as
/// `ast_literal_to_pattern`'s integer decode. Non-literal endpoints
/// are a parser-elaborator invariant violation — annotate has already
/// diagnosed them — so reify panics with a labelled tripwire.
fn pattern_endpoint_to_i128(endpoint: &ast::Pattern) -> i128 {
    match endpoint {
        ast::Pattern::Literal(ast::Literal::Number(repr)) => {
            if let Some(stripped) = repr.strip_prefix("0x") {
                i128::from_str_radix(stripped, 16).unwrap_or(0)
            } else if let Some(stripped) = repr.strip_prefix("0o") {
                i128::from_str_radix(stripped, 8).unwrap_or(0)
            } else if let Some(stripped) = repr.strip_prefix("0b") {
                i128::from_str_radix(stripped, 2).unwrap_or(0)
            } else {
                repr.parse::<i128>().unwrap_or(0)
            }
        }
        ast::Pattern::Literal(ast::Literal::Char(s)) => {
            let inner = s.trim_start_matches('\'').trim_end_matches('\'');
            i128::from(inner.chars().next().unwrap_or('\0') as u32)
        }
        _ => panic!(
            "pattern_endpoint_to_i128: non-literal range endpoint {endpoint:?} \
             (annotate should have diagnosed)"
        ),
    }
}

/// Map an AST [`ast::Literal`] in pattern position to its
/// [`crate::tir::TirLiteralPattern`] counterpart. Number literals
/// decode into `I128` (parsed via the same hex / oct / bin prefix
/// recogniser used by `reify_literal`), with negative sources kept
/// as their parsed numeric value. The `Null` / `Unit` literals never
/// appear in pattern position in the surface grammar — they panic
/// here to surface a parser-elaborator invariant violation early.
fn ast_literal_to_pattern(lit: &ast::Literal) -> crate::tir::TirLiteralPattern {
    use crate::tir::TirLiteralPattern;
    match lit {
        ast::Literal::Number(repr) => {
            // Mirror `reify_literal`'s numeric decode: prefer
            // hex/oct/bin radix, else decimal. Pattern position
            // never sees float literals (the elaborator rejects
            // them earlier), so decode as integer.
            let value: i128 = if let Some(stripped) = repr.strip_prefix("0x") {
                i128::from_str_radix(stripped, 16).unwrap_or(0)
            } else if let Some(stripped) = repr.strip_prefix("0o") {
                i128::from_str_radix(stripped, 8).unwrap_or(0)
            } else if let Some(stripped) = repr.strip_prefix("0b") {
                i128::from_str_radix(stripped, 2).unwrap_or(0)
            } else {
                repr.parse::<i128>().unwrap_or(0)
            };
            TirLiteralPattern::I128(value)
        }
        ast::Literal::String(s) => TirLiteralPattern::String(s.clone()),
        ast::Literal::Char(s) => {
            let inner = s.trim_start_matches('\'').trim_end_matches('\'');
            TirLiteralPattern::Char(inner.chars().next().unwrap_or('\0'))
        }
        ast::Literal::Bool(b) => TirLiteralPattern::Bool(*b),
        ast::Literal::Null => TirLiteralPattern::Null,
        // Unit / Location / Include literals don't appear as pattern
        // literals in the surface grammar — the parser rejects them
        // earlier. Falling here would be a parser-elaborator
        // invariant violation; panic with a labelled tripwire.
        ast::Literal::Unit
        | ast::Literal::LocationFile
        | ast::Literal::LocationLine
        | ast::Literal::LocationFunction
        | ast::Literal::DataSection
        | ast::Literal::IncludeStr(_)
        | ast::Literal::IncludeBytes(_) => {
            panic!("ast_literal_to_pattern: literal kind {lit:?} not valid in pattern position")
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
