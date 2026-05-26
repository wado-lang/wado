//! Item-level resolution (structs, functions, methods, globals, variants, tests).

use std::cell::RefCell;

use crate::ast::{self, Function, GlobalDecl, SelfKind, Type};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::{
    CompilerItem, CompilerItemKind, RegisterError, Resolved, parse_compiler_item_attrs,
};
use crate::hashmap::IndexSet;
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName};
use crate::tir::{
    FunctionKind, TirBlock, TirEffect, TirEffectOp, TirFunction, TirGlobal, TirParam, TirResource,
    TirStruct, TirTest, TirVariantCase, TirVariantDecl, TypeId, TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::types::{FunctionContext, TypeError};

/// Extract the [`CompilerItem`] marker — if any — from a declaration's
/// `#[compiler_item("...")]` attributes, emitting a diagnostic for
/// each unrecognised name. Returns the first matched [`CompilerItem`]
/// (in attribute order); subsequent matches are silently dropped — a
/// declaration may carry at most one marker per the design contract.
pub(super) fn extract_compiler_item<H: CompilerHost>(
    attrs: &[crate::ast::Attribute],
    decl_span: Span,
    logger: &Logger<'_, H>,
) -> Option<CompilerItem> {
    let (items, unknown) = parse_compiler_item_attrs(attrs);
    for raw in unknown {
        let _ = logger.error(TypeError::CompilerItemAttr {
            message: format!("unknown compiler item `{raw}`"),
            span: decl_span,
        });
    }
    items.into_iter().next()
}

/// Push a [`RegisterError`] into the diagnostic stream. Duplicate
/// registrations are kept as errors because they always indicate a
/// stdlib bug (two declarations claiming the same anchor); kind
/// mismatches are reported by [`check_kind`] before reaching this
/// path, so the error surface here is small.
fn report_register_error<H: CompilerHost>(err: RegisterError, span: Span, logger: &Logger<'_, H>) {
    let _ = logger.error(TypeError::CompilerItemAttr {
        message: err.to_string(),
        span,
    });
}

/// Run the per-attribute validation that applies to every kind:
///
/// 1. The attribute is only meaningful inside `core::*` modules; reject
///    it elsewhere.
/// 2. The declared kind must match [`CompilerItem::expected_kind`];
///    otherwise emit a diagnostic and skip registration.
///
/// Returns `true` when registration should proceed.
fn check_compiler_item_placement<H: CompilerHost>(
    item: CompilerItem,
    actual_kind: CompilerItemKind,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) -> bool {
    if !module_source.is_core() {
        let _ = logger.error(TypeError::CompilerItemAttr {
            message: format!(
                "`#[compiler_item(\"{name}\")]` is only valid inside `core::*` modules",
                name = item.attr_name(),
            ),
            span,
        });
        return false;
    }
    if item.expected_kind() != actual_kind {
        let _ = logger.error(TypeError::CompilerItemAttr {
            message: format!(
                "`#[compiler_item(\"{name}\")]` expects a {expected}, but it is attached to a {actual}",
                name = item.attr_name(),
                expected = item.expected_kind(),
                actual = actual_kind,
            ),
            span,
        });
        return false;
    }
    true
}

/// Register a struct declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_struct_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(item, CompilerItemKind::Struct, module_source, span, logger) {
        return;
    }
    let resolved = Resolved::Struct {
        module_source: module_source.clone(),
        name: name.to_string(),
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

/// Register a variant declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_variant_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(item, CompilerItemKind::Variant, module_source, span, logger)
    {
        return;
    }
    let resolved = Resolved::Variant {
        module_source: module_source.clone(),
        name: name.to_string(),
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

/// Register an enum declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_enum_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(item, CompilerItemKind::Enum, module_source, span, logger) {
        return;
    }
    let resolved = Resolved::Enum {
        module_source: module_source.clone(),
        name: name.to_string(),
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

/// Register a trait declaration's `#[compiler_item(...)]` annotation, if any.
///
/// `methods` is the trait's full method list; the elaborator inspects it
/// to cache the single-method trait's primary method name into the
/// registry (see [`Resolved::Trait::method_name`]). For multi-method
/// traits the cache stays `None` and downstream consumers that need a
/// method name must reach for a dedicated method [`CompilerItem`].
pub(super) fn register_trait_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    name: &str,
    methods: &[crate::ast::Function],
    assoc_types: &[crate::ast::AssociatedTypeDecl],
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(item, CompilerItemKind::Trait, module_source, span, logger) {
        return;
    }
    // Single-method traits cache the method's name so the synthesiser
    // can construct `<Trait>::<method>` calls without hard-coding the
    // source-side spelling. Multi-method traits leave it unset.
    let method_name = if methods.len() == 1 {
        Some(methods[0].name.clone())
    } else {
        None
    };
    // For each associated type, capture both its source-side name and the
    // source-side names of all its trait bounds. The synthesiser identifies
    // assoc types by their bound (a `#[compiler_item("...")]`-registered
    // trait whose current spelling also comes from the registry), so both
    // ends stay rename-stable.
    let assoc_types = assoc_types
        .iter()
        .map(|a| crate::compiler_item::TraitAssocType {
            name: a.name.clone(),
            bound_names: a.bounds.iter().map(|b| b.name.clone()).collect(),
        })
        .collect();
    let resolved = Resolved::Trait {
        module_source: module_source.clone(),
        name: name.to_string(),
        method_name,
        assoc_types,
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

/// Register an impl-block method's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_method_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    method_name: &str,
    owner_type: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(item, CompilerItemKind::Method, module_source, span, logger) {
        return;
    }
    let resolved = Resolved::Method {
        module_source: module_source.clone(),
        owner_type: owner_type.to_string(),
        name: method_name.to_string(),
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

/// Register a single variant case's `#[compiler_item("...")]` annotation.
///
/// `parent_type` is the variant the case belongs to (e.g. `"Option"`).
/// `case_index` is the zero-based position of the case in its declared
/// order, which downstream consumers (pattern matching, variant
/// construction) need in addition to the case name.
pub(super) fn register_variant_case_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    parent_type: &str,
    case_name: &str,
    case_index: u32,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(
        item,
        CompilerItemKind::VariantCase,
        module_source,
        span,
        logger,
    ) {
        return;
    }
    let resolved = Resolved::VariantCase {
        module_source: module_source.clone(),
        parent_type: parent_type.to_string(),
        name: case_name.to_string(),
        case_index,
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

/// Register a single enum case's `#[compiler_item("...")]` annotation.
/// See [`register_variant_case_compiler_item`] for the shape — same
/// payload, different parent kind.
pub(super) fn register_enum_case_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    parent_type: &str,
    case_name: &str,
    case_index: u32,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(
        item,
        CompilerItemKind::EnumCase,
        module_source,
        span,
        logger,
    ) {
        return;
    }
    let resolved = Resolved::EnumCase {
        module_source: module_source.clone(),
        parent_type: parent_type.to_string(),
        name: case_name.to_string(),
        case_index,
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

/// Register a `pub type [..T];` declaration's `#[compiler_item("tuple")]` annotation.
pub(super) fn register_tuple_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, logger) else {
        return;
    };
    if !check_compiler_item_placement(
        item,
        CompilerItemKind::TupleFamily,
        module_source,
        span,
        logger,
    ) {
        return;
    }
    let resolved = Resolved::TupleFamily {
        module_source: module_source.clone(),
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, logger);
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Recursively check whether `type_id` mentions a `fn(...)` / `fn mut(...)`
    /// closure type. Used to reject closures crossing the Component Model
    /// boundary (export/import function signatures, CM-exposed record fields,
    /// variant payloads, etc.). Descends through refs, arrays, generic-arg
    /// containers, newtype unwrap, and (when the struct's field registry is
    /// in scope) named-struct field types.
    pub(super) fn type_contains_closure(&self, type_id: TypeId) -> bool {
        let type_table = self.type_table.borrow();
        let mut visited: IndexSet<TypeId> = IndexSet::default();
        self.type_contains_closure_inner(&type_table, type_id, &mut visited)
    }

    fn type_contains_closure_inner(
        &self,
        type_table: &crate::tir::TypeTable,
        type_id: TypeId,
        visited: &mut IndexSet<TypeId>,
    ) -> bool {
        if !visited.insert(type_id) {
            return false;
        }
        match type_table.get(type_id) {
            crate::tir::ResolvedType::Function { .. } => true,
            crate::tir::ResolvedType::Ref(t)
            | crate::tir::ResolvedType::MutRef(t)
            | crate::tir::ResolvedType::Reactive(t)
            | crate::tir::ResolvedType::BuiltinArray(t) => {
                self.type_contains_closure_inner(type_table, *t, visited)
            }
            crate::tir::ResolvedType::GenericInstance { type_args, .. }
            | crate::tir::ResolvedType::GenericResource { type_args, .. } => type_args
                .iter()
                .any(|t| self.type_contains_closure_inner(type_table, *t, visited)),
            crate::tir::ResolvedType::Newtype { base_type, .. } => {
                self.type_contains_closure_inner(type_table, *base_type, visited)
            }
            crate::tir::ResolvedType::Struct { name, .. } => {
                // Recurse into the struct's field types via the elaborator's
                // pre-built field registry. Self-recursive structs are
                // protected by `visited`.
                let field_types: Vec<TypeId> = self
                    .lookup_struct_fields(name)
                    .map(|info| info.fields.iter().map(|(_, ty, _)| *ty).collect())
                    .unwrap_or_default();
                field_types
                    .into_iter()
                    .any(|t| self.type_contains_closure_inner(type_table, t, visited))
            }
            crate::tir::ResolvedType::Variant { name, .. } => {
                // `ResolvedType::Variant` carries only the name; the per-case
                // payload types live in `all_variant_cases`. Look them up so
                // a variant case payload containing a closure type fails the
                // CM boundary check too.
                let payloads: Vec<TypeId> = self
                    .lookup_variant_case(name)
                    .map(|info| info.cases.iter().map(|c| c.payload).collect())
                    .unwrap_or_default();
                payloads
                    .into_iter()
                    .any(|t| self.type_contains_closure_inner(type_table, t, visited))
            }
            _ => false,
        }
    }

    pub(super) fn resolve_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        // Set up type parameters in scope before resolving fields. Use an
        // inherited scope so that any caller-provided `assoc_type_bindings` or
        // `self_type` remain visible — only `type_params` are replaced, matching
        // the original `mem::take(&mut self.trait_ctx.type_params)` semantics.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(&struct_decl.type_params, 0);

        // Resolve field defaults in the struct's module scope. A field default
        // is standalone (no self, no other fields in scope) and must be pure;
        // the purity check runs in `effect_check`.
        let mut field_ctx =
            FunctionContext::new(TypeTable::UNIT, format!("struct:{}", struct_decl.name));
        let mut fields = Vec::new();
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = scope.resolve_type(&field.ty);
            let serde_rename = field.attrs.iter().find_map(|a| {
                if a.name == "serde" {
                    a.kv_value("rename").map(str::to_string)
                } else {
                    None
                }
            });
            let default_expr = field.default.as_ref().map(|default_ast| {
                let resolved = scope.resolve_expr(default_ast, &mut field_ctx, Some(type_id));
                scope.typecheck(resolved.type_id, type_id, default_ast.span());
                Box::new(resolved)
            });
            // A field with a declared default is implicitly non-required at
            // deserialization time: the synthesized Deserialize uses the
            // default expression when the field is absent (WEP 2026-04-11).
            let serde_default = default_expr.is_some()
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

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = struct_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        drop(scope);

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
            monomorph_info: None, // Not from monomorphization
            fields,
            span: struct_decl.span,
            serde_rename_all,
        }
    }

    /// Resolve the `methods` list of an effect or resource declaration into
    /// TIR operations. Generic type parameters declared on the enclosing
    /// resource (e.g. `resource Stream<T>`) are brought into scope before
    /// resolving each method's params and return type so that `T` maps to a
    /// proper `TypeParam` rather than `UNKNOWN`.
    /// Lower a resource / effect declaration's method list to
    /// [`TirEffectOp`]s. Type-param scope is set up once at the start
    /// (so operation signatures can mention `T` for generic resources)
    /// and then each method's params + return are resolved.
    ///
    /// `resource_self`, when `Some((name, module))`, signals that this
    /// is a resource decl rather than an effect decl: methods declared
    /// with `&self`/`&mut self` shorthand get the receiver synthesised
    /// as a real `TirEffectOp` parameter at index 0 (with type
    /// `&Self`/`&mut Self`) so dispatch wrapper signatures match the
    /// post-cm-binding call shape `__cm_binding__<R>_<op>(self, args)`.
    /// For effects (where `resource_self == None`) any `self_kind` is
    /// silently dropped — effect declarations don't take receivers.
    fn resolve_effect_ops(
        &mut self,
        type_params: &[ast::GenericParam],
        methods: &[ast::InterfaceMethod],
        resource_self: Option<(&str, ModuleSource)>,
    ) -> Vec<TirEffectOp> {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(type_params, 0);

        // Construct the resource's `Self` type after type params are in
        // scope, so a generic resource's `GenericResource` instance can
        // reference its own `TypeParam`s (which gap-2 substitution then
        // specialises per impl-block instantiation). For non-generic
        // resources this is just a plain `Resource { name, module }`.
        let self_type: Option<TypeId> = resource_self.map(|(name, module)| {
            if type_params.iter().any(|p| !p.is_effect) {
                let type_arg_ids: Vec<TypeId> = type_params
                    .iter()
                    .filter(|p| !p.is_effect)
                    .map(|p| {
                        scope
                            .trait_ctx
                            .type_params
                            .get(&p.name)
                            .map(|(_, id)| *id)
                            .expect("type param registered by register_generic_params")
                    })
                    .collect();
                scope
                    .type_table
                    .borrow_mut()
                    .intern(crate::tir::ResolvedType::GenericResource {
                        name: name.to_string(),
                        module_source: module,
                        type_args: type_arg_ids,
                    })
            } else {
                scope
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
                    (SelfKind::None, _) => scope.resolve_type(&p.ty),
                    // `&self` / `&mut self` on a resource method:
                    // synthesise the receiver as a first regular
                    // parameter so the dispatch wrapper and the
                    // cm_binding adapter agree on signature shape
                    // `(self, ...)`. By-value self isn't representable
                    // in the AST (`SelfKind` has no `Self_` variant),
                    // so the match is exhaustive over the resource
                    // case.
                    (SelfKind::Ref, Some(self_t)) => scope.type_table.borrow_mut().make_ref(self_t),
                    (SelfKind::MutRef, Some(self_t)) => {
                        scope.type_table.borrow_mut().make_mut_ref(self_t)
                    }
                    // No `Self` in scope (effect decls) — drop the
                    // receiver as before; effect operations don't take
                    // receivers and the elaborator should already have
                    // diagnosed `&self` in an `effect` decl elsewhere.
                    _ => continue,
                };
                let name = if matches!(p.self_kind, SelfKind::None) {
                    p.name.clone()
                } else {
                    // The AST `&self`/`&mut self` shorthand has an
                    // empty name field; give it a real name so
                    // downstream phases that key by parameter name
                    // (WIR `param_names`, the closure synthesis's
                    // `Local { name }` builder, ...) round-trip
                    // unambiguously.
                    "self".to_string()
                };
                params.push(TirParam {
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
                .map(|ty| scope.resolve_type(ty))
                .unwrap_or(TypeTable::UNIT);
            // Extract `#[cm("...")]` attribute payload, if any, so the
            // dispatch synthesis can map raw resource call sites back
            // to the right per-monomorphisation wrapper. Mirrors the
            // elaborator's existing per-call extraction in
            // `lookup_resource_static_cm`: takes the bare attribute
            // string without splitting on `#`. None for effect ops
            // and for resource methods that lack the attribute.
            let cm_name = method
                .attrs
                .iter()
                .find_map(crate::ast::Attribute::cm_identifier);
            ops.push(TirEffectOp {
                name: method.name.clone(),
                params,
                return_type,
                span: method.span,
                cm_name,
            });
        }
        ops
    }

    pub(super) fn resolve_effect_decl(&mut self, decl: &ast::InterfaceDecl) -> TirEffect {
        let operations = self.resolve_effect_ops(&[], &decl.methods, None);
        TirEffect {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
        }
    }

    pub(super) fn resolve_resource_decl(&mut self, decl: &ast::ResourceDecl) -> TirResource {
        let module_source = self.current_module_source.clone();
        let operations = self.resolve_effect_ops(
            &decl.type_params,
            &decl.methods,
            Some((decl.name.as_str(), module_source)),
        );
        TirResource {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
        }
    }

    /// Resolve a global variable declaration
    pub(super) fn resolve_global(&mut self, global_decl: &GlobalDecl) -> Option<TirGlobal> {
        // Resolve the type
        let ty = self.resolve_type(&global_decl.ty);

        // Create a minimal function context for resolving the initializer expression
        // Global initialization has no locals, but we need the context for expression resolution
        // The function name is used for #function compile-time literal (empty for global init)
        let mut ctx = FunctionContext::new(ty, format!("global:{}", global_decl.name));

        // Resolve the initializer expression with expected type for type inference
        let initializer = self.resolve_expr(&global_decl.initializer, &mut ctx, Some(ty));

        // Type check: initializer type must match declared type.
        self.typecheck(initializer.type_id, ty, global_decl.initializer.span());

        Some(TirGlobal {
            name: global_decl.name.clone(),
            ty,
            initializer,
            mutable: global_decl.mutable,
            wado_mutable: global_decl.mutable,
            is_pub: global_decl.is_pub,
            module_source: self.current_module_source.clone(),
            span: global_decl.span,
            // Both flags are set by the lower phase: `is_nullable` for
            // any global whose Wasm slot needs to accept `ref.null`, and
            // `lazy_init` for globals whose initializer runs in
            // `__initialize_module` (i.e. codegen narrows reads after
            // init). Constant `null` initializers set only `is_nullable`.
            is_nullable: false,
            lazy_init: false,
            locals: ctx.locals.clone(),
        })
    }

    /// Resolve a variant declaration
    pub(super) fn resolve_variant_decl(
        &mut self,
        variant_decl: &ast::VariantDecl,
    ) -> TirVariantDecl {
        // Set up type parameters in scope before resolving field types. Use an
        // inherited scope so any caller-provided `assoc_type_bindings`/`self_type`
        // stay visible — only `type_params` are replaced, matching the original
        // `mem::take(&mut self.trait_ctx.type_params)` semantics.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.register_generic_params(&variant_decl.type_params, 0);

        // Resolve each case - each variant case has exactly one payload type
        let mut cases = Vec::new();
        for (index, case) in variant_decl.cases.iter().enumerate() {
            // Unit variants have `()` (unit type) payload
            let payload = if let Some(payload_ty) = &case.payload {
                scope.resolve_type(payload_ty)
            } else {
                TypeTable::UNIT
            };
            cases.push(TirVariantCase {
                name: case.name.clone(),
                index: index as u32,
                payload,
                span: case.span,
            });
        }

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = variant_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        drop(scope);

        register_variant_compiler_item(
            &self.type_table,
            &variant_decl.attrs,
            &variant_decl.name,
            &self.current_module_source,
            variant_decl.span,
            self.logger,
        );

        TirVariantDecl {
            name: variant_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: variant_decl.is_pub,
            type_params,
            cases,
            span: variant_decl.span,
        }
    }

    /// Extract custom wasm export name from `#[export_name("...")]` attribute.
    pub(super) fn extract_export_name(attrs: &[crate::ast::Attribute]) -> Option<String> {
        attrs
            .iter()
            .find(|a| a.name == "export_name")
            .and_then(|a| a.args.first())
            .map(|a| a.as_str().to_string())
    }

    /// Extract allocator tag from `#[allocator("...")]` attribute.
    fn extract_allocator_tag(attrs: &[crate::ast::Attribute]) -> Option<String> {
        attrs
            .iter()
            .find(|a| a.name == "allocator")
            .and_then(|a| a.args.first())
            .map(|a| a.as_str().to_string())
    }

    /// Extract `#[ambient]` marker from function attributes.
    /// Ambient functions bypass effect-check propagation to callers: they may
    /// declare effects for implementation purposes, but callers don't need to
    /// list those effects. Intended for low-level helpers like `log_stdout`
    /// that use an implicit ambient capability.
    pub(super) fn extract_is_ambient(attrs: &[crate::ast::Attribute]) -> bool {
        attrs.iter().any(|a| a.name == "ambient")
    }

    pub(super) fn extract_inline_hint(attrs: &[crate::ast::Attribute]) -> crate::tir::InlineHint {
        let Some(attr) = attrs.iter().find(|a| a.name == "inline") else {
            return crate::tir::InlineHint::Auto;
        };
        match attr.args.first().map(super::super::ast::AttrArg::as_str) {
            Some("always") => crate::tir::InlineHint::Always,
            Some("never") => crate::tir::InlineHint::Never,
            None => crate::tir::InlineHint::Hint,
            _ => crate::tir::InlineHint::Auto,
        }
    }

    /// Validate that stores declarations reference valid reference parameters.
    fn validate_stores(&self, stores: &[String], params: &[TirParam], span: crate::token::Span) {
        let tt = self.type_table.borrow();
        for store_name in stores {
            if let Some(param) = params.iter().find(|p| p.name == *store_name) {
                let resolved = tt.get(param.type_id);
                // Allow stores on: reference types (&T, &mut T) and type parameters (T may be &U)
                if !matches!(
                    resolved,
                    crate::tir::ResolvedType::Ref(_)
                        | crate::tir::ResolvedType::MutRef(_)
                        | crate::tir::ResolvedType::TypeParam { .. }
                ) {
                    let type_name = tt.type_name(param.type_id);
                    let _ = self.logger.error(TypeError::InvalidStores {
                        message: format!(
                            "stores[{store_name}]: parameter '{store_name}' has type '{type_name}', \
                             but only reference parameters (&T or &mut T) or type parameters can be stored"
                        ),
                        span,
                    });
                }
            } else {
                let _ = self.logger.error(TypeError::InvalidStores {
                    message: format!("stores[{store_name}]: no parameter named '{store_name}'"),
                    span,
                });
            }
        }
    }

    /// Populate the generic-function inference caches (type params,
    /// resolved param types, resolved return type) for `func` without
    /// resolving its body. Used as a pre-pass before body resolution so
    /// that same-module forward references to other generic functions
    /// (e.g. `outer<T>` defined before `inner<T>` in the same file) can
    /// run argument-derived type inference at the call site.
    ///
    /// Idempotent: may be called multiple times. Uses fresh `TypeId`s
    /// each time; subsequent overwrites inside `resolve_function` keep
    /// the cache consistent with the body's own `TypeId`s.
    pub(super) fn precompute_generic_function_cache(&mut self, func: &Function) {
        // Mirror `resolve_function`'s `has_real_type_params` guard exactly:
        // fn-bound params are realised eagerly and do not need
        // monomorphisation, so a function whose only non-effect params are
        // fn-bound has nothing to cache.
        let has_real_type_params = func
            .type_params
            .iter()
            .any(|p| !p.is_effect && !p.bounds.iter().any(|b| b.fn_signature.is_some()));
        if !has_real_type_params {
            return;
        }
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.trait_ctx.type_param_bounds.clear();
        // Install effect params before `register_generic_params` so eager
        // `<F: fn() with E>` bound resolution sees `E` as `EffectRef::Param`.
        let old_effect_params = std::mem::take(&mut scope.current_effect_params);
        let old_effect_param_decls = std::mem::take(&mut scope.current_effect_param_decls);
        let effect_params: Vec<&ast::GenericParam> =
            func.type_params.iter().filter(|p| p.is_effect).collect();
        scope.current_effect_params = effect_params.iter().map(|p| p.name.clone()).collect();
        scope.current_effect_param_decls = effect_params
            .iter()
            .map(|p| (p.name.clone(), p.id))
            .collect();
        scope.register_generic_params(&func.type_params, 0);
        scope.populate_generic_function_cache(func);
        scope.current_effect_params = old_effect_params;
        scope.current_effect_param_decls = old_effect_param_decls;
    }

    /// Populate the three generic-function inference caches
    /// (`generic_function_params`, `generic_function_resolved_param_types`,
    /// `generic_function_resolved_return_types`) for `func`, keyed by its
    /// name. Type parameters must already be registered in
    /// `self.trait_ctx.type_params` before calling this.
    ///
    /// Returns the resolved declared return type so callers that need it
    /// (e.g. `resolve_function` for `task_return_type`) can avoid resolving
    /// it a second time.
    fn populate_generic_function_cache(&mut self, func: &Function) -> TypeId {
        // Skip effect params (never real generics) and fn-bound params
        // (realised eagerly to their bound's function type by
        // `register_generic_params`, which does not consume a `TypeParam`
        // index slot for them). The remaining entries' positional order
        // matches the dense `TypeParam.index` space so the inference cache
        // and substitution map line up.
        let type_param_list: Vec<(String, TypeId)> = func
            .type_params
            .iter()
            .filter(|p| !p.is_effect)
            .filter(|p| !p.bounds.iter().any(|b| b.fn_signature.is_some()))
            .filter_map(|p| {
                self.trait_ctx
                    .type_params
                    .get(&p.name)
                    .map(|&(_, id)| (p.name.clone(), id))
            })
            .collect();
        let resolved_param_types: Vec<TypeId> = func
            .params
            .iter()
            .filter(|p| p.self_kind == SelfKind::None)
            .map(|p| self.resolve_type(&p.ty))
            .collect();
        let declared_return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);
        self.generic_function_params
            .insert(func.name.clone(), type_param_list);
        self.generic_function_resolved_param_types
            .insert(func.name.clone(), resolved_param_types);
        self.generic_function_resolved_return_types
            .insert(func.name.clone(), declared_return_type);
        declared_return_type
    }

    /// Emit a `MissingReturn` diagnostic when a declared non-Unit return
    /// type cannot be satisfied by the body. Skipped for `Unit`/`Never`
    /// declarations, for missing bodies (declared-only externals), and
    /// for bodies whose every control path provably exits before the end
    /// (`block_always_exits`) — either via an explicit `return`, a
    /// divergent statement like `panic("…")`, or a fully-diverging
    /// labeled block / `loop`.
    ///
    /// The previous heuristic accepted any nested `return` even when only
    /// one branch of an `if` carried one, which let partial-return
    /// bodies through and produced an invalid core Wasm module ("type
    /// mismatch: expected i32 but nothing on stack") for the
    /// fall-through path.
    fn validate_missing_return(&self, return_type: TypeId, body: Option<&TirBlock>, span: Span) {
        if return_type == TypeTable::UNIT || return_type == TypeTable::NEVER {
            return;
        }
        let Some(body) = body else {
            return;
        };
        if Self::block_always_exits(body) {
            return;
        }
        let _ = self.logger.error(TypeError::MissingReturn {
            return_type: self.type_table.borrow().type_name(return_type),
            span,
        });
    }

    /// Resolve a function
    pub(super) fn resolve_function(&mut self, func: &Function) -> Option<TirFunction> {
        // Set up type parameters in scope before resolving types. Use an
        // inherited scope so any caller-provided `assoc_type_bindings`/`self_type`
        // stay visible — only `type_params` and `type_param_bounds` are replaced,
        // matching the original `mem::take` semantics for those two fields.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        scope.trait_ctx.type_param_bounds.clear();

        // Set effect params in scope before `register_generic_params`. Eager
        // `<F: fn() with E>` bound resolution runs inside
        // `register_generic_params` and consults `current_effect_param_decls`
        // to recognise `E` as `EffectRef::Param` rather than re-resolving it
        // to a phantom `EffectRef::Concrete`.
        let old_effect_params = std::mem::take(&mut scope.current_effect_params);
        let old_effect_param_decls = std::mem::take(&mut scope.current_effect_param_decls);
        let effect_params: Vec<_> = func.type_params.iter().filter(|p| p.is_effect).collect();
        if effect_params.len() > 1 {
            let _ = scope.logger.error(TypeError::InvalidLiteral {
                message: "multiple effect parameters are not allowed; use a single effect parameter instead".to_string(),
                span: effect_params[1].span,
            });
        }
        scope.current_effect_params = effect_params.iter().map(|p| p.name.clone()).collect();
        scope.current_effect_param_decls = effect_params
            .iter()
            .map(|p| (p.name.clone(), p.id))
            .collect();

        scope.register_generic_params(&func.type_params, 0);

        // Populate the generic-function inference caches
        // (`generic_function_params`, `generic_function_resolved_param_types`,
        // `generic_function_resolved_return_types`). Populated before the
        // `function_return_types` update below because that map is shared
        // with non-generic callers and may be overwritten by external
        // registrations (trait methods, etc.) over time. The declared return
        // type is also used for `task_return_type` in async fns.
        // `<F: fn(...)>` bounds are eagerly realised to the bound's function
        // type and do not consume a `TypeParam` slot — they're not generic
        // parameters that need monomorphisation.
        let has_real_type_params = func
            .type_params
            .iter()
            .any(|p| !p.is_effect && !p.bounds.iter().any(|b| b.fn_signature.is_some()));
        let declared_return_type = if has_real_type_params {
            scope.populate_generic_function_cache(func)
        } else {
            func.return_type
                .as_ref()
                .map(|t| scope.resolve_type(t))
                .unwrap_or(TypeTable::UNIT)
        };

        // For async functions, the Wasm-level return type is unit (the result is delivered
        // via `task return`, not via the Wasm function return). The declared return type is
        // stored as `task_return_type` for validating `task return expr`.
        let return_type = if func.is_async {
            TypeTable::UNIT
        } else {
            declared_return_type
        };

        // Update the function_return_types with the resolved return type
        // (This replaces the potentially incorrect type from static resolution)
        scope
            .function_return_types
            .insert(func.name.clone(), return_type);

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(declared_return_type);
        }

        // Resolve parameters. Each default expression is resolved in the
        // callee's lexical scope, with only the earlier parameters in scope
        // (a default cannot reference its own parameter or any later one).
        // This gives defaults access to the definition module's private items
        // and earlier parameters without needing call-site substitution.
        //
        // `export fn` is rejected: the Component Model ABI requires every
        // parameter at the boundary, so defaults cannot divergently exist
        // only on the Wado side.
        // A function crosses the Component Model boundary when it is either
        // exported (`export fn ...`) or imported (declaration with no body
        // carrying `#[canonical(...)]` or `#[cm(...)]`). Closures may not
        // appear in either side's signature.
        let is_cm_import =
            func.body.is_none() && func.attrs.iter().any(|a| a.cm_boundary.is_some());
        let crosses_cm_boundary = func.is_export || is_cm_import;

        let mut params = Vec::new();
        for param in &func.params {
            let type_id = scope.resolve_type(&param.ty);
            // Closures cannot cross the Component Model boundary.
            if crosses_cm_boundary && scope.type_contains_closure(type_id) {
                let _ = scope.logger.error(TypeError::ClosureAtCmBoundary {
                    function: func.name.clone(),
                    position: format!("parameter '{}'", param.name),
                    span: param.span,
                });
            }
            let default_expr = param.default.as_ref().map(|default_ast| {
                if func.is_export {
                    let _ = scope.logger.error(TypeError::DefaultInExportFn {
                        function: func.name.clone(),
                        param: param.name.clone(),
                        span: default_ast.span(),
                    });
                }
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(type_id));
                scope.typecheck(resolved.type_id, type_id, default_ast.span());
                Box::new(resolved)
            });
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
            scope.record_local_symbol(
                param.id,
                &param.name,
                param.name_span,
                param.is_mut,
                type_id,
            );
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                default_expr,
                span: param.span,
            });
        }

        // Closures cannot cross the CM boundary in return position either.
        if crosses_cm_boundary && scope.type_contains_closure(return_type) {
            let _ = scope.logger.error(TypeError::ClosureAtCmBoundary {
                function: func.name.clone(),
                position: "return type".to_string(),
                span: func.span,
            });
        }

        // Validate stores declarations
        scope.validate_stores(&func.stores, &params, func.span);

        // Resolve body
        let body = func
            .body
            .as_ref()
            .map(|b| scope.resolve_block(b, &mut ctx, None));

        scope.validate_missing_return(return_type, body.as_ref(), func.span);

        // Convert AST type params to TIR type params (while type params
        // still in scope). `<F: fn(...)>` / `<F: fn mut(...)>` bounds are
        // realised eagerly by `register_generic_params` and do not consume
        // a `TypeParam` index slot — drop them from the TIR list so the
        // monomorphiser doesn't try to specialise on the closure's functor
        // type. The remaining params keep their dense `register_generic_params`
        // index, which matches both the `TypeParam(name, index)` entries in
        // the type table and the positional order of the inference cache.
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
                Some(crate::tir::TirTypeParam {
                    name: p.name.clone(),
                    is_effect: p.is_effect,
                    is_pack: p.is_pack,
                    bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                    default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                    index: idx,
                })
            })
            .collect();

        // Resolve effects while effect params are still in scope
        let effects = scope.resolve_effects(&func.effects, &func.effect_ids);

        // Restore effect params scope
        scope.current_effect_params = old_effect_params;
        scope.current_effect_param_decls = old_effect_param_decls;
        drop(scope);

        Some(TirFunction {
            module_source: ModuleSource::default(),
            name: func.name.clone(),
            is_pub: func.is_pub,
            is_export: func.is_export,
            is_async: func.is_async,
            type_params,
            impl_type_params: vec![], // Not a method, no impl type params
            monomorph_info: None,     // Not from monomorphization
            method_info: None,        // Not a method
            params,
            return_type,
            task_return_type: if func.is_async {
                Some(declared_return_type)
            } else {
                None
            },
            effects,
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: IndexSet::default(),
            // Scratch local fields - computed by lower phase
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: Self::extract_is_ambient(&func.attrs),
            inline_hint: Self::extract_inline_hint(&func.attrs),
            compiler_item: extract_compiler_item(&func.attrs, func.span, self.logger),
            export_name: Self::extract_export_name(&func.attrs),
            allocator_tag: Self::extract_allocator_tag(&func.attrs),
            kind: FunctionKind::Regular,

            return_abi: crate::tir::ReturnAbi::default(),
        })
    }

    /// Resolve a test declaration to a `TirFunction` and `TirTest`
    pub(super) fn resolve_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
        module_is_todo: bool,
    ) -> Option<(TirFunction, TirTest)> {
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

        // Generate function name: __test_{index} or __test_{name_snake_case}
        // For expect_trap tests: __test_trap_{index} or __test_trap_{index}_{name_snake_case}
        // For TODO tests:        __test_todo_{index} or __test_todo_{index}_{name_snake_case}
        // For custom timeout:    __test_tm{ms}_{index} or __test_trap_tm{ms}_{index}_{name}
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
                // Convert test name to snake_case for function name
                let snake_name: String = name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect::<String>()
                    .to_lowercase();
                format!("{prefix}_{test_index}_{snake_name}")
            }
            None => format!("{prefix}_{test_index}"),
        };

        // Create function context - tests have no parameters and return unit
        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());

        // Resolve the test body
        let body = self.resolve_block(&test_decl.body, &mut ctx, None);

        let tir_func = TirFunction {
            module_source: ModuleSource::default(),
            name: function_name.clone(),
            is_pub: false,    // Tests are not public
            is_export: false, // Tests are not world exports
            is_async: false,  // Tests are never async
            type_params: vec![],
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params: vec![], // Tests have no parameters
            return_type,
            task_return_type: None,
            effects: vec![], // Tests can have any effects (they're allowed to do I/O)
            stores: vec![],
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: crate::tir::InlineHint::Auto,
            compiler_item: None,
            export_name: None,
            allocator_tag: None,
            kind: FunctionKind::Regular,

            return_abi: crate::tir::ReturnAbi::default(),
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

    /// Resolve a method (function with &self parameter)
    pub(super) fn resolve_method(
        &mut self,
        func: &Function,
        struct_name: &str,
        impl_type: &Type,
        trait_name: Option<&str>,
        trait_type: Option<&Type>,
    ) -> Option<TirFunction> {
        // Use an inherited scope so the caller's `assoc_type_bindings` (set up
        // for the surrounding impl block) remain visible — `Self::Output` etc.
        // must still resolve while we're inside this method body. Type params
        // and bounds get rebuilt below to match the original `mem::take`
        // behavior.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.trait_ctx.type_params.clear();
        let mut type_param_list = Vec::new();

        // Bare base trait name (e.g. `"Stream"` for an `impl Stream<u8>`).
        // Distinct from `trait_name`, which is the full mangled form
        // (`"Stream<u8>"`) used to make per-instantiation method names
        // unique. Effect / resource / trait decl indices are keyed by the
        // canonical `(decl_module, base name)` pair, so we also resolve
        // the trait reference through the current module's import context
        // so dispatch synthesis can tell two same-named effects /
        // resources apart.
        let base_trait_name: Option<String> = trait_type.map(|t| scope.get_type_name(t));
        let base_trait_module: Option<ModuleSource> = base_trait_name
            .as_deref()
            .map(|n| scope.canonical_decl_key(n).0);

        // First, collect type params from impl block's generic type (e.g., impl Box<T>)
        // Also build impl_type_params for the TirFunction
        // For ref-type impls (e.g., impl Trait for &Container<T>), unwrap the reference first.
        let mut impl_type_params = Vec::new();
        let impl_type_inner = match impl_type {
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => inner.as_ref(),
            other => other,
        };
        if let ast::Type::Generic(generic) = impl_type_inner {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg {
                    let name = &named.name;
                    if !scope.trait_ctx.type_params.contains_key(name) {
                        let type_id = scope
                            .type_table
                            .borrow_mut()
                            .make_type_param(name.clone(), i as u32);
                        scope
                            .trait_ctx
                            .type_params
                            .insert(name.clone(), (i as u32, type_id));
                        // Store impl type param info for later monomorphization
                        impl_type_params.push(crate::tir::TirTypeParam {
                            name: name.clone(),
                            is_effect: false,
                            is_pack: false,
                            bounds: vec![],
                            default: None, // Impl type params don't have defaults
                            index: i as u32,
                        });
                    }
                }
            }
        } else if let ast::Type::Named(named) = impl_type {
            // Blanket impl case: `impl<I: Iterator> IntoIterator for I`
            // The impl type is a type parameter itself, registered by the caller,
            // now living in the saved (parent) scope.
            if let Some(&(idx, _)) = scope.saved().type_params.get(&named.name) {
                let type_id = scope
                    .type_table
                    .borrow_mut()
                    .make_type_param(named.name.clone(), idx);
                scope
                    .trait_ctx
                    .type_params
                    .insert(named.name.clone(), (idx, type_id));
                let bounds = scope
                    .saved()
                    .type_param_bounds
                    .get(&named.name)
                    .map(|bs| bs.iter().map(|b| b.name.clone()).collect())
                    .unwrap_or_default();
                impl_type_params.push(crate::tir::TirTypeParam {
                    name: named.name.clone(),
                    is_effect: false,
                    is_pack: false,
                    bounds,
                    default: None,
                    index: idx,
                });
            }
        } else if let ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) = impl_type {
            // Reference impl case: `impl<T: Bound> Trait for &T` / `impl<T: Bound> Trait for &mut T`
            // The inner type T is a type parameter registered by the caller.
            if let ast::Type::Named(named) = boxed.as_ref()
                && let Some(&(idx, _)) = scope.saved().type_params.get(&named.name)
            {
                let type_id = scope
                    .type_table
                    .borrow_mut()
                    .make_type_param(named.name.clone(), idx);
                scope
                    .trait_ctx
                    .type_params
                    .insert(named.name.clone(), (idx, type_id));
                let bounds = scope
                    .saved()
                    .type_param_bounds
                    .get(&named.name)
                    .map(|bs| bs.iter().map(|b| b.name.clone()).collect())
                    .unwrap_or_default();
                impl_type_params.push(crate::tir::TirTypeParam {
                    name: named.name.clone(),
                    is_effect: false,
                    is_pack: false,
                    bounds,
                    default: None,
                    index: idx,
                });
            }
        } else if let ast::Type::Tuple(elements) = impl_type {
            // Variadic tuple impl: `impl<..T: Trait> Trait for [..T]`
            // Extract type pack params from the tuple's TypePackSpread elements.
            for elem in elements {
                if let ast::Type::TypePackSpread(name, _) = elem
                    && let Some(&(idx, _)) = scope.saved().type_params.get(name)
                {
                    let type_id = scope
                        .type_table
                        .borrow_mut()
                        .make_type_pack(name.clone(), idx);
                    scope
                        .trait_ctx
                        .type_params
                        .insert(name.clone(), (idx, type_id));
                    let bounds = scope
                        .saved()
                        .type_param_bounds
                        .get(name)
                        .map(|bs| bs.iter().map(|b| b.name.clone()).collect())
                        .unwrap_or_default();
                    impl_type_params.push(crate::tir::TirTypeParam {
                        name: name.clone(),
                        is_effect: false,
                        is_pack: true,
                        bounds,
                        default: None,
                        index: idx,
                    });
                }
            }
        }

        // Populate bounds from the impl block's type_params
        // (inherited from outer scope - second-pass sets these up).
        // The caller sets up bounds BEFORE calling resolve_method, so the saved
        // scope contains the caller's bounds. We start from those and add
        // method-level bounds on top.
        let saved_bounds = scope.saved().type_param_bounds.clone();
        scope.trait_ctx.type_param_bounds = saved_bounds;

        // Bind the trait's own type parameters to the impl's concrete trait
        // args so that references like `T` inside a default method body resolve
        // to the impl's instantiation (e.g., `impl Maker<i32> for IntMaker`
        // binds the trait's `T` to `i32`). Impl type params were registered
        // above, so `Maker<Container<U>>` in `impl<U> Maker<Container<U>> for
        // Foo<U>` resolves correctly.
        if let Some(trait_t) = trait_type {
            scope.bind_trait_type_params_from_impl(trait_t);
        }

        // Set effect params in scope (for resolving effect names in function types)
        let old_effect_params = std::mem::take(&mut scope.current_effect_params);
        let old_effect_param_decls = std::mem::take(&mut scope.current_effect_param_decls);
        let effect_params: Vec<_> = func.type_params.iter().filter(|p| p.is_effect).collect();
        if effect_params.len() > 1 {
            let _ = scope.logger.error(TypeError::InvalidLiteral {
                message: "multiple effect parameters are not allowed; use a single effect parameter instead".to_string(),
                span: effect_params[1].span,
            });
        }
        scope.current_effect_params = effect_params.iter().map(|p| p.name.clone()).collect();
        scope.current_effect_param_decls = effect_params
            .iter()
            .map(|p| (p.name.clone(), p.id))
            .collect();

        // Then, collect method-level type params. Mirrors
        // `register_generic_params` in `trait_env.rs`: `<F: fn(...)>` /
        // `<F: fn mut(...)>` bounds are realised eagerly to the bound's
        // function type and do NOT consume a `TypeParam` index slot, so the
        // index space stays dense for real type params. Effect params have
        // their own channel (`current_effect_param_decls`, installed above).
        let offset = scope.trait_ctx.type_params.len();
        let mut next_idx = offset as u32;
        for param in &func.type_params {
            if param.is_effect {
                continue;
            }
            let idx = next_idx;
            let fn_bound_sig = if param.is_pack {
                None
            } else {
                param.bounds.iter().find_map(|b| b.fn_signature.as_ref())
            };
            let (type_id, consumed_index) = if param.is_pack {
                (
                    scope
                        .type_table
                        .borrow_mut()
                        .make_type_pack(param.name.clone(), idx),
                    true,
                )
            } else if let Some(sig) = fn_bound_sig {
                (scope.resolve_type(&ast::Type::Function(sig.clone())), false)
            } else {
                (
                    scope
                        .type_table
                        .borrow_mut()
                        .make_type_param(param.name.clone(), idx),
                    true,
                )
            };
            scope
                .trait_ctx
                .type_params
                .insert(param.name.clone(), (idx, type_id));
            // Only push *real* type params (TypeParam-ids) into the
            // inference cache list. Eagerly-resolved fn-bound params have a
            // concrete Function type and aren't generics anymore.
            if fn_bound_sig.is_none() {
                type_param_list.push((param.name.clone(), type_id));
            }
            // Record only "real" trait bounds — `fn`/`fn mut` bounds are
            // already realised in the parameter's type itself.
            let real_bounds: Vec<ast::TraitBound> = param
                .bounds
                .iter()
                .filter(|b| b.fn_signature.is_none())
                .cloned()
                .collect();
            if !real_bounds.is_empty() {
                scope
                    .trait_ctx
                    .type_param_bounds
                    .insert(param.name.clone(), real_bounds);
            }
            if consumed_index {
                next_idx += 1;
            }
        }

        // Set up Self type for the impl block
        // This allows `&Self` to resolve correctly in method parameters
        let old_self_type = scope.trait_ctx.self_type;
        scope.trait_ctx.self_type = Some(scope.resolve_type(impl_type));

        // Concrete `TypeId`s of the trait/resource type arguments at this
        // impl site (e.g. `[u8]` for `impl Stream<u8> for MockCM`). Resolved
        // here — after impl-block + method-level type params are in scope —
        // so that generic impls (`impl<T> Stream<T>`) round-trip the right
        // `TypeParam` TypeId. Used by the dispatch synthesis to key
        // per-monomorphisation infrastructure off `(base_trait, trait_type_args)`.
        let trait_type_args: Vec<TypeId> = match trait_type {
            Some(ast::Type::Generic(generic)) => generic
                .args
                .iter()
                .map(|arg| scope.resolve_type(arg))
                .collect(),
            _ => Vec::new(),
        };

        // Resolve return type
        let return_type = func
            .return_type
            .as_ref()
            .map(|t| scope.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        // Update the function_return_types with the resolved return type
        // (This replaces the potentially incorrect type from static resolution)
        let mangled_name = MethodName::format_local(struct_name, trait_name, &func.name);
        scope
            .function_return_types
            .insert(mangled_name.clone(), return_type);

        // Display name for #function: StructName::method_name
        let display_name = MethodName::format_local(struct_name, None, &func.name);
        let mut ctx = FunctionContext::new(return_type, display_name);
        // Mark this context as a handler method body when the surrounding
        // impl block targets an effect or resource declaration. `resume`
        // is only valid inside such bodies (see WEP 2026-04-11). Resources
        // share the handler-method semantics with effects: an
        // `impl Fields for CountingFields` method is a one-shot handler
        // body just like `impl Counter for BaseCounter`.
        //
        // Decl indices are keyed by the base trait name, so for generic
        // resources (`impl Stream<u8> for MockCM`) the bare-name form
        // (`Stream`) is what the lookup needs — `trait_name` itself is the
        // full mangled form (`Stream<u8>`) and would miss the index.
        if let Some(name) = base_trait_name.as_deref() {
            // `trait_type` was referenced by bare name in the surrounding
            // `impl <Trait> for <Type>` block; canonicalise it against the
            // current module's import context so two modules with same-
            // named effects / resources don't get a false negative here.
            let canonical_key = scope.canonical_decl_key(name);
            if scope
                .trait_env
                .effect_decl_index
                .contains_key(&canonical_key)
                || scope
                    .trait_env
                    .resource_decl_index
                    .contains_key(&canonical_key)
            {
                ctx.in_handler_method = true;
            }
        }

        // Resolve parameters (including &self). Defaults are resolved in the
        // method's lexical scope with earlier parameters already bound.
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = match param.self_kind {
                ast::SelfKind::Ref => {
                    // &self: wrap impl type in immutable reference
                    let inner_type = scope.resolve_type(impl_type);
                    scope.type_table.borrow_mut().make_ref(inner_type)
                }
                ast::SelfKind::MutRef => {
                    // &mut self: wrap impl type in mutable reference
                    let inner_type = scope.resolve_type(impl_type);
                    scope.type_table.borrow_mut().make_mut_ref(inner_type)
                }
                ast::SelfKind::None => {
                    // Regular parameter
                    scope.resolve_type(&param.ty)
                }
            };
            // Reject parameter defaults on trait-impl methods: defaults live
            // on the trait declaration only (WEP 2026-04-11).
            if trait_name.is_some()
                && let Some(default_ast) = &param.default
            {
                let _ = scope.logger.error(TypeError::DefaultInTraitImpl {
                    method: func.name.clone(),
                    param: param.name.clone(),
                    span: default_ast.span(),
                });
            }
            let default_expr = param.default.as_ref().map(|default_ast| {
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(type_id));
                scope.typecheck(resolved.type_id, type_id, default_ast.span());
                Box::new(resolved)
            });
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
            scope.record_local_symbol(
                param.id,
                &param.name,
                param.name_span,
                param.is_mut,
                type_id,
            );
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                default_expr,
                span: param.span,
            });
        }

        // Validate stores declarations
        scope.validate_stores(&func.stores, &params, func.span);

        // Resolve body
        let body = func
            .body
            .as_ref()
            .map(|b| scope.resolve_block(b, &mut ctx, None));

        scope.validate_missing_return(return_type, body.as_ref(), func.span);

        // Convert AST type params to TIR type params (while type params still
        // in scope). Mirror the free-function path in `resolve_function`:
        // `<F: fn(...)>` bounds are realised eagerly and dropped from the
        // generic list; the remaining real type params use dense indices so
        // the substitution map in `substitute_type_params` lines up.
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
                Some(crate::tir::TirTypeParam {
                    name: p.name.clone(),
                    is_effect: p.is_effect,
                    is_pack: p.is_pack,
                    bounds: p.bounds.iter().map(|b| b.name.clone()).collect(),
                    default: p.default.as_ref().map(|ty| scope.resolve_type(ty)),
                    index: idx,
                })
            })
            .collect();

        // Store resolved param types for generic methods (before restoring type params scope)
        // so TypeParams have the correct ids for later inference at call sites.
        let method_resolved_param_types: Vec<TypeId> = if func.type_params.is_empty() {
            vec![]
        } else {
            func.params
                .iter()
                .filter(|p| p.self_kind == SelfKind::None)
                .map(|p| scope.resolve_type(&p.ty))
                .collect()
        };

        // Resolve effects while effect params are still in scope
        let effects = scope.resolve_effects(&func.effects, &func.effect_ids);

        // Restore effect params and Self type. `trait_ctx` is auto-restored on
        // `drop(scope)`, which replaces everything set up above.
        scope.current_effect_params = old_effect_params;
        scope.current_effect_param_decls = old_effect_param_decls;
        scope.trait_ctx.self_type = old_self_type;
        drop(scope);

        // Store type parameters for generic methods (for call site substitution)
        if !func.type_params.is_empty() {
            self.generic_method_params
                .insert(mangled_name.clone(), type_param_list);
            self.generic_method_resolved_param_types
                .insert(mangled_name, method_resolved_param_types);
        }

        Some(TirFunction {
            module_source: ModuleSource::default(),
            name: func.name.clone(), // Will be mangled by caller
            is_pub: func.is_pub,
            is_export: false, // Methods are not world exports
            is_async: false,  // Methods are never async
            type_params,
            impl_type_params, // Type params from impl block (e.g., T from impl Counter<T>)
            monomorph_info: None, // Not from monomorphization
            method_info: Some(LocalMethodName {
                struct_name: struct_name.to_string(),
                base_struct_name: struct_name.to_string(),
                trait_name: trait_name.map(String::from),
                base_trait_name,
                base_trait_module,
                trait_type_args,
                method_name: func.name.clone(),
                method_type_args: vec![],
                is_type_param_receiver: false,
                is_ref_impl: false,
                cm_name: None,
            }),
            params,
            return_type,
            task_return_type: None,
            effects,
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: Self::extract_is_ambient(&func.attrs),
            inline_hint: Self::extract_inline_hint(&func.attrs),
            compiler_item: extract_compiler_item(&func.attrs, func.span, self.logger),
            export_name: None,
            allocator_tag: None,
            kind: FunctionKind::Regular,

            return_abi: crate::tir::ReturnAbi::default(),
        })
    }
}
