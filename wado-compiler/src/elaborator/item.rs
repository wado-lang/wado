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
use crate::name::{FqTypeName, MethodName, global_name};
use crate::tir::{
    FunctionKind, TirEffect, TirEffectOp, TirFunction, TirParam, TirResource, TirStruct, TirTest,
    TirVariantDecl, TypeId, TypeTable, method_param_offset,
};
use crate::token::Span;

use super::Elaborator;
use super::scope::{BinderInScope, TypeParamScope};
use super::sig::{DeclSig, MethodSig};
use super::types::{FunctionContext, TypeError};

/// Extract the [`CompilerItem`] marker — if any — from a declaration's
/// `#[compiler_item("...")]` attributes, emitting a diagnostic for
/// each unrecognised name. Returns the first matched [`CompilerItem`]
/// (in attribute order); subsequent matches are silently dropped — a
/// declaration may carry at most one marker per the design contract.
pub(super) fn extract_compiler_item<H: CompilerHost>(
    attrs: &[crate::ast::Attribute],
    decl_span: Span,
    module_source: &ModuleSource,
    logger: &Logger<'_, H>,
) -> Option<CompilerItem> {
    let (items, unknown) = parse_compiler_item_attrs(attrs);
    for raw in unknown {
        let _ = logger.error_in(
            module_source,
            TypeError::CompilerItemAttr {
                message: format!("unknown compiler item `{raw}`"),
                span: decl_span,
            },
        );
    }
    items.into_iter().next()
}

/// Body-walk placeholder for a function / method / test. The
/// body walk records the signature facts (`fn_param_types`,
/// `fn_return_types`, `decl_type_params`, `function_effects`,
/// `method_names`, …) and resolves the body for its side-effect fact
/// recording, but no longer assembles the function's TIR — reify is the
/// sole producer. No caller reads the returned `TirFunction`, so a minimal
/// shell with the right name + span satisfies the signature.
fn placeholder_function(name: String, span: Span) -> TirFunction {
    TirFunction {
        module_source: ModuleSource::default(),
        name,
        def_id: None,
        visibility: crate::ast::Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params: vec![],
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: None,
        span,
        local_count: 0,
        locals: vec![],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        benign_effects: Vec::new(),
        inline_hint: crate::tir::InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
        return_abi: crate::tir::ReturnAbi::default(),
    }
}

/// Push a [`RegisterError`] into the diagnostic stream. Duplicate
/// registrations are kept as errors because they always indicate a
/// stdlib bug (two declarations claiming the same anchor); kind
/// mismatches are reported by [`check_kind`] before reaching this
/// path, so the error surface here is small.
fn report_register_error<H: CompilerHost>(
    err: RegisterError,
    span: Span,
    module_source: &ModuleSource,
    logger: &Logger<'_, H>,
) {
    let _ = logger.error_in(
        module_source,
        TypeError::CompilerItemAttr {
            message: err.to_string(),
            span,
        },
    );
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
        let _ = logger.error_in(
            module_source,
            TypeError::CompilerItemAttr {
                message: format!(
                    "`#[compiler_item(\"{name}\")]` is only valid inside `core::*` modules",
                    name = item.attr_name(),
                ),
                span,
            },
        );
        return false;
    }
    if item.expected_kind() != actual_kind {
        let _ = logger.error_in(
            module_source,
            TypeError::CompilerItemAttr {
                message: format!(
                    "`#[compiler_item(\"{name}\")]` expects a {expected}, but it is attached to a {actual}",
                    name = item.attr_name(),
                    expected = item.expected_kind(),
                    actual = actual_kind,
                ),
                span,
            },
        );
        return false;
    }
    true
}

/// The `#[compiler_item(...)]` this declaration carries, or `None` when it
/// carries none or names an item that may not sit on a `kind` declaration.
fn compiler_item_on<H: CompilerHost>(
    attrs: &[crate::ast::Attribute],
    kind: CompilerItemKind,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) -> Option<CompilerItem> {
    let item = extract_compiler_item(attrs, span, module_source, logger)?;
    check_compiler_item_placement(item, kind, module_source, span, logger).then_some(item)
}

/// Bind `item` to the declaration `resolved` names, reporting a clash.
fn bind_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    item: CompilerItem,
    resolved: Resolved,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, module_source, logger);
    }
}

/// Register a struct declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_struct_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    decl: crate::ast::AstId,
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(attrs, CompilerItemKind::Struct, module_source, span, logger)
    else {
        return;
    };
    let resolved = Resolved::Struct {
        module_source: module_source.clone(),
        name: name.to_string(),
        decl,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register a variant declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_variant_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    decl: crate::ast::AstId,
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::Variant,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::Variant {
        module_source: module_source.clone(),
        name: name.to_string(),
        decl,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register an enum declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_enum_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    decl: crate::ast::AstId,
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(attrs, CompilerItemKind::Enum, module_source, span, logger)
    else {
        return;
    };
    let resolved = Resolved::Enum {
        module_source: module_source.clone(),
        name: name.to_string(),
        decl,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register a `resource` declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_resource_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    decl: crate::ast::AstId,
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::Resource,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::Resource {
        module_source: module_source.clone(),
        name: name.to_string(),
        decl,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register a `type X = Y;` declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_newtype_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    decl: crate::ast::AstId,
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::Newtype,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::Newtype {
        module_source: module_source.clone(),
        name: name.to_string(),
        decl,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
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
    decl: crate::ast::AstId,
    name: &str,
    methods: &[crate::ast::Function],
    assoc_types: &[crate::ast::AssociatedTypeDecl],
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(attrs, CompilerItemKind::Trait, module_source, span, logger)
    else {
        return;
    };
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
    let fq = type_table
        .borrow()
        .defs()
        .of_ast_id(decl)
        .map(|def| crate::name::FqTraitName::declared(type_table.borrow().defs(), def));
    let resolved = Resolved::Trait {
        module_source: module_source.clone(),
        name: name.to_string(),
        decl,
        fq,
        method_name,
        assoc_types,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register a free function's `#[compiler_item(...)]` annotation, if any.
/// A CM ABI helper the binding synthesis calls by name lives here: it has no
/// receiver, so the method form cannot carry it.
pub(super) fn register_function_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::Function,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::Function {
        module_source: module_source.clone(),
        name: name.to_string(),
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register an impl-block method's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_method_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    method_name: &str,
    owner_type: &str,
    owner_head: &crate::name::FqTypeName,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(attrs, CompilerItemKind::Method, module_source, span, logger)
    else {
        return;
    };
    let resolved = Resolved::Method {
        module_source: module_source.clone(),
        owner_type: owner_type.to_string(),
        owner_head: Some(owner_head.clone()),
        name: method_name.to_string(),
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
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
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::VariantCase,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::VariantCase {
        module_source: module_source.clone(),
        parent_type: parent_type.to_string(),
        name: case_name.to_string(),
        case_index,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
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
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::EnumCase,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::EnumCase {
        module_source: module_source.clone(),
        parent_type: parent_type.to_string(),
        name: case_name.to_string(),
        case_index,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register a `pub type [..T];` declaration's `#[compiler_item("tuple")]` annotation.
pub(super) fn register_tuple_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    decl: crate::ast::AstId,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::TupleFamily,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::TupleFamily {
        module_source: module_source.clone(),
        decl,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Register a named definition-less type (`pub type Array<T>;`) carrying a
/// `#[compiler_item("...")]` annotation. Binds the builtin type's name and
/// owning module so the type resolver can map the name to its builtin
/// `ResolvedType`.
pub(super) fn register_builtin_type_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    decl: crate::ast::AstId,
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = compiler_item_on(
        attrs,
        CompilerItemKind::BuiltinType,
        module_source,
        span,
        logger,
    ) else {
        return;
    };
    let resolved = Resolved::BuiltinType {
        module_source: module_source.clone(),
        name: name.to_string(),
        decl,
    };
    bind_compiler_item(type_table, item, resolved, module_source, span, logger);
}

/// Everything an impl method's signature resolves against: the impl's
/// `TypeParam` scheme in its positional slots, the method's own params
/// numbered after them, and `Self` bound to the impl target.
///
/// Built in one place so the decl pass (which records the canonical
/// signature) and the body walk (which resolves the method against it)
/// cannot disagree about slot numbering.
pub(super) struct MethodFrame {
    pub(super) impl_type_params: Vec<crate::tir::TirTypeParam>,
    /// The method's own slots, in index order, starting after the impl's.
    pub(super) method_type_params: Vec<(String, TypeId)>,
}

impl<H: CompilerHost> TypeParamScope<'_, '_, H> {
    /// Register the impl's and the method's type parameters into this
    /// scope and bind `Self`, yielding the frame the method's parameter
    /// and return types resolve in.
    /// Intern one of the impl target's parameters, put it in this frame's
    /// scope, and describe it for the TIR.
    fn bind_target_param(
        &mut self,
        name: &str,
        index: u32,
        is_pack: bool,
        bounds: Vec<String>,
        projected_from: Option<(u32, String)>,
        decl: Option<ast::AstId>,
    ) -> crate::tir::TirTypeParam {
        let type_id = {
            let mut table = self.tysys.type_table.borrow_mut();
            if is_pack {
                table.make_type_pack(name.to_string(), index)
            } else {
                table.make_type_param(name.to_string(), index)
            }
        };
        self.annotate_ctx.trait_ctx.type_params.insert(
            name.to_string(),
            BinderInScope {
                index,
                type_id,
                decl,
            },
        );
        crate::tir::TirTypeParam {
            name: name.to_string(),
            is_effect: false,
            is_pack,
            bounds,
            default: None,
            index,
            projected_from,
        }
    }

    fn saved_param_bounds(&self, name: &str) -> Vec<String> {
        self.saved()
            .type_param_bounds
            .get(name)
            .map(|bounds| bounds.iter().map(|b| b.name.clone()).collect())
            .unwrap_or_default()
    }

    /// `impl<T> Trait for Foo<T>` — the target's arguments name the impl's
    /// own parameters, numbered by argument position. A concrete argument
    /// (`String` in `Foo<String, T>`) is not one, so it leaves its index
    /// unused rather than shifting the rest.
    fn bind_declared_target_params(
        &mut self,
        target_args: &[Type],
        impl_declared_params: &[ast::GenericParam],
    ) -> Vec<crate::tir::TirTypeParam> {
        let mut params = Vec::new();
        for (index, arg) in target_args.iter().enumerate() {
            let ast::Type::Named(named) = arg else {
                continue;
            };
            let name = &named.name;
            if self.annotate_ctx.trait_ctx.type_params.contains_key(name)
                || !self.tysys.is_impl_target_param(
                    &self.current_module_source,
                    impl_declared_params,
                    name,
                )
            {
                continue;
            }
            params.push(self.bind_target_param(
                name,
                index as u32,
                false,
                vec![],
                None,
                super::scope::param_decl(impl_declared_params, name),
            ));
        }
        params
    }

    /// `impl<I: Iterator> IntoIterator for I` — the target *is* a parameter,
    /// registered by the caller and now living in the saved frame.
    fn bind_blanket_target_param(
        &mut self,
        named: &ast::NamedType,
        saved: &super::scope::TraitContext,
        impl_declared_params: &[ast::GenericParam],
    ) -> Vec<crate::tir::TirTypeParam> {
        let Some(&BinderInScope {
            index: target_index,
            ..
        }) = saved.type_params.get(&named.name)
        else {
            return Vec::new();
        };
        // Declaration order, not "receiver then projections": the impl's type
        // arguments are consumed by position, so a parameter written before
        // the receiver must be bound before it.
        let projected = self.blanket_projections(&named.name, impl_declared_params);
        let mut params = Vec::new();
        for declared in impl_declared_params {
            if !declared.is_real_type_param() {
                continue;
            }
            let Some(&BinderInScope { index, .. }) = saved.type_params.get(&declared.name) else {
                continue;
            };
            let bounds = self.saved_param_bounds(&declared.name);
            if declared.name == named.name {
                params.push(self.bind_target_param(
                    &declared.name,
                    index,
                    false,
                    bounds,
                    None,
                    Some(declared.id),
                ));
                continue;
            }
            // A parameter the receiver's bound determines. One neither the
            // target nor a bound names is rejected at the impl, so anything
            // left here is projectable.
            if let Some(assoc_name) = projected.get(&declared.name) {
                params.push(self.bind_target_param(
                    &declared.name,
                    index,
                    declared.is_pack,
                    bounds,
                    Some((target_index, assoc_name.clone())),
                    Some(declared.id),
                ));
            }
        }
        params
    }

    /// Which associated type of the receiver's bound determines each of the
    /// impl's other parameters — `..F` from `Assoc = [..F]`, `A` from
    /// `Assoc = A`. Monomorphization projects them from the concrete receiver.
    fn blanket_projections(
        &self,
        target_name: &str,
        impl_declared_params: &[ast::GenericParam],
    ) -> crate::hashmap::IndexMap<String, String> {
        let mut out = crate::hashmap::IndexMap::default();
        for assoc in impl_declared_params
            .iter()
            .filter(|p| p.name == target_name)
            .flat_map(|p| &p.bounds)
            .flat_map(|bound| &bound.assoc_types)
        {
            let mut named = Vec::new();
            assoc.ty.mentioned_names(&mut named);
            for n in named {
                out.entry(n).or_insert_with(|| assoc.name.clone());
            }
        }
        out
    }

    /// `impl<T: Bound> Trait for &T` / `&mut T` — the inner type is a
    /// parameter the caller registered.
    fn bind_ref_target_param(
        &mut self,
        inner: &ast::Type,
        saved: &super::scope::TraitContext,
    ) -> Vec<crate::tir::TirTypeParam> {
        let ast::Type::Named(named) = inner else {
            return Vec::new();
        };
        let Some(&BinderInScope { index, decl, .. }) = saved.type_params.get(&named.name) else {
            return Vec::new();
        };
        let bounds = self.saved_param_bounds(&named.name);
        vec![self.bind_target_param(&named.name, index, false, bounds, None, decl)]
    }

    /// `impl<..T: Trait> Trait for [..T]` — the target's spread elements are
    /// the packs.
    fn bind_tuple_pack_params(
        &mut self,
        elements: &[ast::Type],
        saved: &super::scope::TraitContext,
    ) -> Vec<crate::tir::TirTypeParam> {
        let mut params = Vec::new();
        for element in elements {
            let ast::Type::TypePackSpread(name, _) = element else {
                continue;
            };
            let Some(&BinderInScope { index, decl, .. }) = saved.type_params.get(name) else {
                continue;
            };
            let bounds = self.saved_param_bounds(name);
            params.push(self.bind_target_param(name, index, true, bounds, None, decl));
        }
        params
    }

    /// Enter an `impl` block's own frame: its target type parameters bound
    /// into the positional slots the block is abstract over, the enclosing
    /// bounds restored, the trait reference's parameters bound to the impl's
    /// arguments, and `Self` set to the impl target.
    ///
    /// The one definition of an impl's slot numbering. A method frame is this
    /// plus the method's own parameters, numbered past these slots.
    pub(super) fn enter_impl_frame(
        &mut self,
        impl_type: &Type,
        trait_type: Option<&Type>,
        impl_is_concrete: bool,
        impl_declared_params: &[ast::GenericParam],
    ) -> Vec<crate::tir::TirTypeParam> {
        let saved = &self.saved().clone();
        let impl_type_inner = match impl_type {
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => inner.as_ref(),
            other => other,
        };
        // However the head is spelled: `Cell<T>` and `ns::Cell<T>` write one
        // target a namespace apart.
        let head_args = super::method_lookup::impl_target_head_args(impl_type_inner);
        let impl_type_params = if let Some(args) = head_args
            && !impl_is_concrete
        {
            self.bind_declared_target_params(args, impl_declared_params)
        } else {
            match impl_type {
                ast::Type::Named(named) => {
                    self.bind_blanket_target_param(named, saved, impl_declared_params)
                }
                ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) => {
                    self.bind_ref_target_param(boxed.as_ref(), saved)
                }
                ast::Type::Tuple(elements) => self.bind_tuple_pack_params(elements, saved),
                _ => Vec::new(),
            }
        };

        let saved_bounds = saved.type_param_bounds.clone();
        self.annotate_ctx.trait_ctx.type_param_bounds = saved_bounds;

        // Bind the trait's own type parameters to the impl's concrete trait
        // args so that references like `T` inside a default method body resolve
        // to the impl's instantiation (e.g., `impl Maker<i32> for IntMaker`
        // binds the trait's `T` to `i32`). Impl type params were registered
        // above, so `Maker<Container<U>>` in `impl<U> Maker<Container<U>> for
        // Foo<U>` resolves correctly.
        if let Some(trait_t) = trait_type {
            self.bind_trait_type_params_from_impl(trait_t);
        }

        let resolved_self_type = self.resolve_type(impl_type);
        self.annotate_ctx.trait_ctx.self_type = Some(resolved_self_type);
        // The trait this block implements qualifies `Self::Assoc` inside the
        // signatures of the defaults it inherits, where `Self` is concrete and
        // carries no bound to read the declaring trait off.
        self.annotate_ctx.trait_ctx.self_trait = trait_type.and_then(|t| {
            let name = self.get_type_name(t);
            self.trait_decl_at(t.id()?, &name)
        });
        impl_type_params
    }

    /// Resolve and record the `impl` block's own declaration facts — its
    /// target and trait type arguments and its `type X = …;` bindings — in
    /// the block's frame.
    ///
    /// Numbered against the same slots the block's method signatures are, so
    /// a use site substitutes both through one alignment
    /// ([`super::sig::ImplSig::slots`]).
    fn record_impl_sig(&mut self, impl_block: &ast::ImplBlock, impl_is_concrete: bool) {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.enter_impl_frame(
            &impl_block.ty,
            impl_block.trait_type.as_ref(),
            impl_is_concrete,
            &impl_block.type_params,
        );
        scope.annotate_ctx.trait_ctx.assoc_type_bindings.clear();

        let target_type_args = scope.resolve_written_type_args(&impl_block.ty);
        let trait_type_args = impl_block
            .trait_type
            .as_ref()
            .map(|t| scope.resolve_written_type_args(t))
            .unwrap_or_default();

        // The one place the impl's trait is resolved. Everything downstream
        // keys off the written name, so a name that resolves to nothing indexes
        // an impl of a trait that does not exist — it matches no query and no
        // phase objects. Resolving it here, in the frame, is what makes the
        // impl's own declaration facts complete.
        if let Some(trait_type) = &impl_block.trait_type {
            scope.check_impl_trait_resolves(impl_block, trait_type);
        }
        scope.check_impl_params_constrained(impl_block);

        let mut associated_types = crate::hashmap::IndexMap::default();
        for binding in &impl_block.associated_types {
            let type_id = scope.resolve_type(&binding.ty);
            scope
                .annotate_ctx
                .trait_ctx
                .assoc_type_bindings
                .insert(binding.name.clone(), type_id);
            associated_types.insert(binding.name.clone(), type_id);
        }

        // The block's name-level facts. Answered here because this is the only
        // phase standing in the block's own frame.
        let target_fq = scope.impl_receiver_name(impl_block);
        // The header's own site answers: `check_impl_trait_resolves` rejects a
        // header whose trait reaches no declaration, so a well-formed block has
        // an identity here and an erroneous one contributes none.
        let trait_decl = impl_block
            .trait_type
            .as_ref()
            .and_then(crate::resolve::head_site)
            .and_then(|site| scope.tysys.resolutions.declared(site));
        let self_type = scope
            .annotate_ctx
            .trait_ctx
            .self_type
            .expect("entering an impl frame binds Self to the target");

        let impl_def = scope.def_at(impl_block.id);
        scope.sem.decls.impl_sigs.insert(
            impl_def,
            super::sig::ImplSig {
                self_type,
                target_type_args,
                trait_type_args,
                associated_types,
                target_fq,
                trait_decl,
            },
        );
    }

    /// Require the impl's target and trait reference to name, between them, every
    /// type parameter it declares — a use site determines them from the receiver
    /// and trait arguments alone, so one they never mention has no value to be
    /// given (Rust's E0207). A bound's arguments count as mentions, its subject
    /// does not; an effect parameter is bound by the handler.
    fn check_impl_params_constrained(&mut self, impl_block: &ast::ImplBlock) {
        let mut named: Vec<String> = Vec::new();
        impl_block.ty.mentioned_names(&mut named);
        if let Some(trait_type) = &impl_block.trait_type {
            trait_type.mentioned_names(&mut named);
        }
        for binding in &impl_block.associated_types {
            binding.ty.mentioned_names(&mut named);
        }
        for param in &impl_block.type_params {
            for bound in &param.bounds {
                for assoc in &bound.assoc_types {
                    assoc.ty.mentioned_names(&mut named);
                }
                if let Some(sig) = &bound.fn_signature {
                    for p in &sig.params {
                        p.mentioned_names(&mut named);
                    }
                    sig.return_type.mentioned_names(&mut named);
                }
            }
        }
        for param in &impl_block.type_params {
            if param.is_effect
                || named.iter().any(|n| n == &param.name)
                || self
                    .tysys
                    .is_known_type_name_in(&self.current_module_source, &param.name)
            {
                continue;
            }
            let _ = self.emit(TypeError::UnconstrainedImplTypeParam {
                param_name: param.name.clone(),
                span: impl_block.span,
            });
        }
    }

    /// Require that the name an `impl` implements is declared — as a trait, an
    /// effect, or a resource, the latter two installing handlers through the same
    /// syntax. Nothing else resolves it: every downstream index keys off the
    /// written string, so an `impl` of an undeclared name registers happily,
    /// matches no query, and reaches the back end unmentioned.
    ///
    /// The header's own reference site answers, and only it. A global by-name
    /// scan would let `impl Deserialize for T;` compile in a module that never
    /// named `Deserialize`, and the header would carry no identity — leaving
    /// dispatch comparing spellings two modules can share.
    fn check_impl_trait_resolves(&mut self, impl_block: &ast::ImplBlock, trait_type: &Type) {
        let implementable = crate::resolve::head_site(trait_type)
            .and_then(|site| self.tysys.resolutions.declared(site))
            .is_some_and(|def| {
                matches!(
                    self.tysys.resolutions.defs().kind(def),
                    crate::defs::DefKind::Trait
                        | crate::defs::DefKind::Effect
                        | crate::defs::DefKind::Resource
                )
            });
        if implementable {
            return;
        }
        let _ = self.emit(TypeError::UnknownTraitImpl {
            name: super::trait_env::get_type_name_static(trait_type),
            span: impl_block.span,
        });
    }

    /// The type arguments a head writes, resolved in the current frame — the
    /// reading the impl frame binds through, so a block's recorded arguments
    /// and its bound parameters share one set of positions.
    pub(super) fn resolve_written_type_args(&mut self, ty: &Type) -> Vec<TypeId> {
        // Peeled as the frame peels it.
        let inner = match ty {
            Type::Reference(i) | Type::MutReference(i) => i.as_ref(),
            other => other,
        };
        let Some(args) = super::method_lookup::impl_target_head_args(inner) else {
            return Vec::new();
        };
        args.to_vec()
            .iter()
            .map(|arg| self.resolve_type(arg))
            .collect()
    }

    pub(super) fn enter_impl_method_frame(
        &mut self,
        func: &Function,
        impl_type: &Type,
        trait_type: Option<&Type>,
        impl_is_concrete: bool,
        impl_declared_params: &[ast::GenericParam],
    ) -> MethodFrame {
        let mut type_param_list = Vec::new();
        let impl_type_params = self.enter_impl_frame(
            impl_type,
            trait_type,
            impl_is_concrete,
            impl_declared_params,
        );

        let effect_params: Vec<_> = func.type_params.iter().filter(|p| p.is_effect).collect();
        if effect_params.len() > 1 {
            let _ = self.emit(TypeError::InvalidLiteral {
                message: "multiple effect parameters are not allowed; use a single effect parameter instead".to_string(),
                span: effect_params[1].span,
            });
        }
        self.annotate_ctx
            .trait_ctx
            .install_effect_params(&func.type_params);

        let offset = method_param_offset(&impl_type_params) as usize;
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
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_pack(param.name.clone(), idx),
                    true,
                )
            } else if let Some(sig) = fn_bound_sig {
                (self.resolve_type(&ast::Type::Function(sig.clone())), false)
            } else {
                (
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_param(param.name.clone(), idx),
                    true,
                )
            };
            self.annotate_ctx.trait_ctx.type_params.insert(
                param.name.clone(),
                BinderInScope::declared(idx, type_id, param.id),
            );
            // Only push *real* type params (TypeParam-ids) into the
            // inference cache list. Eagerly-resolved fn-bound params have a
            // concrete Function type and aren't generics anymore.
            if fn_bound_sig.is_none() {
                type_param_list.push((param.name.clone(), type_id));
            }
            // Record only "real" trait bounds — `fn`/`fn mut` bounds are
            // already realised in the parameter's type itself.
            let real_bounds = param.real_bounds();
            if !real_bounds.is_empty() {
                self.annotate_ctx
                    .trait_ctx
                    .type_param_bounds
                    .insert(param.name.clone(), real_bounds);
            }
            if consumed_index {
                next_idx += 1;
            }
        }

        MethodFrame {
            impl_type_params,
            method_type_params: type_param_list,
        }
    }
}
impl<H: CompilerHost> Elaborator<'_, H> {
    /// Substitute a signature's own defaulted type parameters into `ty`.
    ///
    /// A parameter without a default is left alone: it is opaque, and the
    /// caller chooses it.
    fn apply_type_param_defaults(
        &mut self,
        type_params: &[ast::GenericParam],
        ty: TypeId,
    ) -> TypeId {
        let defaulted: Vec<(String, ast::Type)> = type_params
            .iter()
            .filter(|p| p.is_real_type_param())
            .filter_map(|p| p.default.as_ref().map(|d| (p.name.clone(), d.clone())))
            .collect();
        if defaulted.is_empty() {
            return ty;
        }
        let mut subst = crate::hashmap::IndexMap::default();
        for (name, default_ty) in defaulted {
            // The index the declaration gave the parameter, not its position
            // among the signature's own: a method's slots follow the impl's.
            let &BinderInScope { index, .. } = self
                .annotate_ctx
                .trait_ctx
                .type_params
                .get(&name)
                .expect("a signature's own type parameters are in scope for its defaults");
            let resolved = self.resolve_type(&default_ty);
            subst.insert(index, resolved);
        }
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(ty, &subst)
    }

    /// Resolve one method parameter's type. A receiver comes from the impl
    /// target — the parser desugars `self` / `&self` / `&mut self` into
    /// `Self`-based annotations — and anything else from its annotation.
    fn resolve_method_param_type(&mut self, param: &ast::Param) -> TypeId {
        // The receiver takes the `Self` the impl frame already fixed, not a
        // re-resolution of the written target. By the time a parameter is
        // typed the method's own type parameters are in scope, and they are
        // keyed by name: `fn map<U>` inside `impl<I, U> Iterator for
        // IterMap<I, U>` shadows the impl's `U`, so re-resolving `IterMap<I,
        // U>` here would answer with the method's slot and give the receiver a
        // type no caller can produce.
        let self_type = || {
            self.annotate_ctx
                .trait_ctx
                .self_type
                .expect("impl frame entered before typing a receiver")
        };
        match param.self_kind {
            ast::SelfKind::Value => self_type(),
            ast::SelfKind::Ref => {
                let inner = self_type();
                self.tysys.type_table.borrow_mut().make_ref(inner)
            }
            ast::SelfKind::MutRef => {
                let inner = self_type();
                self.tysys.type_table.borrow_mut().make_mut_ref(inner)
            }
            ast::SelfKind::None => self.resolve_type(&param.ty),
        }
    }

    /// A by-value `self` transfers ownership, so it is legal on a resource
    /// and on a concrete aggregate that transitively owns one (the consuming
    /// method hands that resource off). A plain value type must borrow with
    /// `&self`. Generic aggregates resolve to `GenericInstance` rather than a
    /// concrete value type, so they are already permitted.
    fn check_self_by_value(&mut self, self_ty: TypeId, span: Span) {
        let (is_resource, is_concrete_value, type_name) = {
            let tt = self.tysys.type_table.borrow();
            use crate::tir::ResolvedType;
            let resolved = tt.get(self_ty);
            (
                matches!(
                    resolved,
                    ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. }
                ),
                matches!(
                    resolved,
                    ResolvedType::Struct { .. }
                        | ResolvedType::Enum { .. }
                        | ResolvedType::Variant { .. }
                ),
                tt.type_name(self_ty),
            )
        };
        if is_concrete_value && !is_resource && !self.tysys.carries_resource(self_ty) {
            let _ = self.emit(TypeError::SelfByValueOnNonResource { type_name, span });
        }
    }

    /// Resolve and record the canonical signature of every method in
    /// `impl_block`, in the impl's own frame.
    ///
    /// The decl pass runs this for every impl block so a dispatch query
    /// instantiates a recorded signature instead of re-resolving the method
    /// AST under the *caller's* perspective (WEP 2026-05-26).
    pub(super) fn record_impl_decls(&mut self, impl_block: &ast::ImplBlock) {
        let impl_def = self.def_at(impl_block.id);
        let mut block = self.enter_inherited_type_param_scope();
        block.annotate_ctx.trait_ctx.type_params.clear();
        block.annotate_ctx.trait_ctx.type_param_bounds.clear();
        block.register_impl_block_params(impl_block);

        block.annotate_ctx.trait_ctx.assoc_type_bindings.clear();

        let impl_is_concrete = block.impl_is_concrete_instantiation(&impl_block.ty);

        block.record_impl_sig(impl_block, impl_is_concrete);
        if impl_block.is_synthesize_request {
            return;
        }

        for method in &impl_block.methods {
            let mut frame_scope = block.enter_inherited_type_param_scope();
            frame_scope.annotate_ctx.trait_ctx.type_params.clear();
            let frame = frame_scope.enter_impl_method_frame(
                method,
                &impl_block.ty,
                impl_block.trait_type.as_ref(),
                impl_is_concrete,
                &impl_block.type_params,
            );
            // In this frame, not one scope out: a signature naming
            // `Self::Item` is numbered by these slots.
            frame_scope
                .annotate_ctx
                .trait_ctx
                .assoc_type_bindings
                .clear();
            for binding in &impl_block.associated_types {
                let type_id = frame_scope.resolve_type(&binding.ty);
                frame_scope
                    .annotate_ctx
                    .trait_ctx
                    .assoc_type_bindings
                    .insert(binding.name.clone(), type_id);
            }

            let param_types: Vec<TypeId> = method
                .params
                .iter()
                .map(|p| frame_scope.resolve_type(&p.ty))
                .collect();
            let return_type = method
                .return_type
                .as_ref()
                .map(|t| frame_scope.resolve_type(t));
            for param in &method.params {
                frame_scope.reject_unresolved_annotation(&param.ty);
            }
            if let Some(ty) = method.return_type.as_ref() {
                frame_scope.reject_unresolved_annotation(ty);
            }
            let mut type_params: Vec<(String, TypeId)> = frame
                .impl_type_params
                .iter()
                .filter_map(|tp| {
                    frame_scope
                        .annotate_ctx
                        .trait_ctx
                        .type_params
                        .get(&tp.name)
                        .map(|b| (tp.name.clone(), b.type_id))
                })
                .collect();
            let declaring_slot_count = type_params.len() as u32;
            type_params.extend(frame.method_type_params.iter().cloned());
            let self_kind = method
                .params
                .first()
                .map(|p| p.self_kind)
                .unwrap_or(ast::SelfKind::None);
            let method_def = frame_scope.def_at(method.id);
            frame_scope.sem.decls.method_sigs.insert(
                method_def,
                MethodSig {
                    def: method_def,
                    decl: DeclSig {
                        type_params,
                        param_types,
                        return_type,
                    },
                    self_kind,
                    params: method
                        .params
                        .iter()
                        .filter(|p| p.self_kind == ast::SelfKind::None)
                        .map(|p| super::sig::Param {
                            name: p.name.clone(),
                            is_mut: p.is_mut,
                            default: p.default.clone(),
                        })
                        .collect(),
                    declaring_slot_count,
                    declaring_impl: Some(impl_def),
                    own_params: super::sig::own_params_of(&method.type_params),
                    cm_name: method
                        .attrs
                        .iter()
                        .find_map(crate::ast::Attribute::cm_identifier),
                    is_async: method.is_async,
                },
            );
        }
    }

    /// Recursively check whether `type_id` mentions a `fn(...)` / `fn mut(...)`
    /// closure type. Used to reject closures crossing the Component Model
    /// boundary (export/import function signatures, CM-exposed record fields,
    /// variant payloads, etc.). Descends through refs, arrays, generic-arg
    /// containers, newtype unwrap, and (when the struct's field registry is
    /// in scope) named-struct field types.
    pub(super) fn type_contains_closure(&self, type_id: TypeId) -> bool {
        let type_table = self.tysys.type_table.borrow();
        let mut visited: IndexSet<TypeId> = IndexSet::default();
        self.type_contains_closure_inner(&type_table, type_id, &mut visited)
    }

    /// Whether `type_id` is, or contains anywhere within it, a `Slice<T>` — a
    /// reference view, which has no Component Model representation. A nested
    /// one degrades just as loudly as a top-level one, so the search reaches
    /// tuple members, struct fields, variant payloads, and type arguments.
    pub(super) fn type_contains_slice_view(&self, type_id: TypeId) -> bool {
        let tt = self.tysys.type_table.borrow();
        let mut visited: IndexSet<TypeId> = IndexSet::default();
        self.type_contains_slice_view_inner(&tt, type_id, &mut visited)
    }

    fn type_contains_slice_view_inner(
        &self,
        type_table: &crate::tir::TypeTable,
        type_id: TypeId,
        visited: &mut IndexSet<TypeId>,
    ) -> bool {
        use crate::tir::ResolvedType;
        if !visited.insert(type_id) {
            return false;
        }
        let base = type_table.representation_head(type_id);
        if base != type_id && self.type_contains_slice_view_inner(type_table, base, visited) {
            return true;
        }
        match type_table.get(base) {
            ResolvedType::Ref(t) | ResolvedType::MutRef(t) | ResolvedType::Reactive(t) => {
                self.type_contains_slice_view_inner(type_table, *t, visited)
            }
            ResolvedType::BuiltinArray(t) => {
                self.type_contains_slice_view_inner(type_table, *t, visited)
            }
            ResolvedType::Newtype { base_type, .. } => {
                self.type_contains_slice_view_inner(type_table, *base_type, visited)
            }
            ResolvedType::GenericInstance { def, type_args } => {
                if type_table.compiler_item_def(crate::compiler_item::CompilerItem::Slice)
                    == Some(*def)
                {
                    return true;
                }
                type_args
                    .iter()
                    .any(|t| self.type_contains_slice_view_inner(type_table, *t, visited))
            }
            ResolvedType::GenericResource { type_args, .. } => type_args
                .iter()
                .any(|t| self.type_contains_slice_view_inner(type_table, *t, visited)),
            ResolvedType::Struct { .. } => {
                let field_types: Vec<TypeId> = self
                    .struct_fields_of_type(base)
                    .map(|info| info.fields.iter().map(|(_, ty, _)| *ty).collect())
                    .unwrap_or_default();
                field_types
                    .into_iter()
                    .any(|t| self.type_contains_slice_view_inner(type_table, t, visited))
            }
            ResolvedType::Variant { .. } => {
                let payloads: Vec<TypeId> = self
                    .variant_of_type(base)
                    .map(|info| info.cases.iter().map(|c| c.payload).collect())
                    .unwrap_or_default();
                payloads
                    .into_iter()
                    .any(|t| self.type_contains_slice_view_inner(type_table, t, visited))
            }
            _ => false,
        }
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
            crate::tir::ResolvedType::Struct { .. } => {
                // Recurse into the struct's field types via the elaborator's
                // pre-built field registry. Self-recursive structs are
                // protected by `visited`.
                let field_types: Vec<TypeId> = self
                    .struct_fields_of_type(type_id)
                    .map(|info| info.fields.iter().map(|(_, ty, _)| *ty).collect())
                    .unwrap_or_default();
                field_types
                    .into_iter()
                    .any(|t| self.type_contains_closure_inner(type_table, t, visited))
            }
            crate::tir::ResolvedType::Variant { .. } => {
                // The per-case payload types live in `all_variant_cases`; look
                // them up so a variant case payload containing a closure type
                // fails the CM boundary check too.
                let payloads: Vec<TypeId> = self
                    .variant_of_type(type_id)
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
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.register_generic_params(&struct_decl.type_params, 0);

        // A field default is standalone — no self, no sibling fields in
        // scope — and must be pure; `effect_check` enforces that.
        let mut field_ctx =
            FunctionContext::new(TypeTable::UNIT, format!("struct:{}", struct_decl.name));
        let mut struct_field_types: Vec<TypeId> = Vec::with_capacity(struct_decl.fields.len());
        for field in &struct_decl.fields {
            let type_id = scope.resolve_type(&field.ty);
            scope.reject_unresolved_annotation(&field.ty);
            if let Some(serde_default) = field
                .attrs
                .iter()
                .find(|a| a.name == "wire" && a.has_arg("default"))
            {
                let _ = scope.emit(TypeError::WireDefaultAttr {
                    field: field.name.clone(),
                    span: serde_default.span,
                });
            }
            if let Some(default_ast) = &field.default {
                let resolved = scope.resolve_expr(default_ast, &mut field_ctx, Some(type_id));
                scope.typecheck(resolved, type_id, default_ast.span());
            }
            struct_field_types.push(type_id);
        }

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
                projected_from: None,
            })
            .collect();

        drop(scope);

        self.sem
            .types
            .decl_type_params
            .insert(struct_decl.id, type_params);

        // Record per-field resolved types for reify to read instead of
        // re-resolving them off the static decl pass + UNKNOWN-fallback.
        // The static pass cannot follow `pub use` re-export chains; the
        // resolution we just did, with import scopes in place, can.
        self.sem
            .types
            .struct_field_types
            .insert(struct_decl.id, struct_field_types);

        TirStruct {
            def: crate::tir::StructDef::Decl(
                self.tysys
                    .resolutions
                    .defs()
                    .of_ast_id(struct_decl.id)
                    .expect("a `struct` declaration is declared"),
            ),
            type_args: Vec::new(),
            name: struct_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: struct_decl.visibility,
            type_params: vec![],
            monomorph_info: None,
            fields: vec![],
            span: struct_decl.span,
            wire_name_policy: None,
        }
    }

    /// Operation signatures the decl pass recorded for the declaration at
    /// `decl_id`.
    fn declared_effect_ops(&self, decl_id: ast::AstId) -> Vec<TirEffectOp> {
        let decl = self.def_at(decl_id);
        self.sem
            .decls
            .effect_ops
            .get(&decl)
            .cloned()
            .expect("the decl pass records every interface / resource declaration's operations")
    }

    /// Resolve a `trait` declaration in its own frame and record it.
    ///
    /// `Self` takes slot 0 and the trait's own type parameters follow, so a
    /// method signature naming `Self`, `Self::Assoc` or the trait's `T` is
    /// abstract over exactly those slots. An `impl` reads a method back by
    /// filling slot 0 with its target and the rest with its trait arguments
    /// — the same instantiation every other declaration uses, instead of
    /// re-resolving the trait's method AST in the impl's perspective.
    pub(super) fn resolve_trait_decl(&mut self, trait_decl: &ast::TraitDecl) {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.annotate_ctx.trait_ctx.assoc_type_bindings.clear();

        let self_slot = scope
            .tysys
            .type_table
            .borrow_mut()
            .make_type_param("Self".to_string(), 0);
        scope
            .annotate_ctx
            .trait_ctx
            .type_params
            .insert("Self".to_string(), BinderInScope::undeclared(0, self_slot));
        scope.annotate_ctx.trait_ctx.type_param_bounds.insert(
            "Self".to_string(),
            vec![ast::TraitBound {
                // The trait's own declaration node, which the resolution walk
                // answers for: `Self` here is bounded by this trait, and a
                // fresh id would be a reference site nothing resolved.
                id: trait_decl.id,
                name: trait_decl.name.clone(),
                assoc_types: Vec::new(),
                span: trait_decl.span,
                fn_signature: None,
                resolved: None,
            }],
        );
        scope.annotate_ctx.trait_ctx.self_type = Some(self_slot);
        let next_slot = scope.register_generic_params(&trait_decl.type_params, 1);

        let decl_slots: Vec<(String, TypeId)> = std::iter::once(("Self".to_string(), self_slot))
            .chain(trait_decl.type_params.iter().filter_map(|tp| {
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_params
                    .get(&tp.name)
                    .map(|b| (tp.name.clone(), b.type_id))
            }))
            .collect();

        let mut methods: crate::hashmap::IndexMap<String, super::sig::TraitMethod> =
            crate::hashmap::IndexMap::default();
        for method in &trait_decl.methods {
            let mut method_scope = scope.enter_inherited_type_param_scope();
            method_scope
                .annotate_ctx
                .trait_ctx
                .install_effect_params(&method.type_params);
            method_scope.register_generic_params(&method.type_params, next_slot);
            // Only slot-consuming parameters. A `fn`-bound one registers as
            // its bound's function type, so admitting it here put a
            // `Function` where a slot belongs — and made a trait's signature
            // count its parameters differently from an impl's, which counts
            // them by the same rule below.
            let method_slots: Vec<(String, TypeId)> = method
                .type_params
                .iter()
                .filter(|p| p.is_real_type_param())
                .filter_map(|tp| {
                    method_scope
                        .annotate_ctx
                        .trait_ctx
                        .type_params
                        .get(&tp.name)
                        .map(|b| (tp.name.clone(), b.type_id))
                })
                .collect();

            let param_types: Vec<TypeId> = method
                .params
                .iter()
                .map(|p| method_scope.resolve_type(&p.ty))
                .collect();
            let return_type = method
                .return_type
                .as_ref()
                .map(|t| method_scope.resolve_type(t));
            for param in &method.params {
                method_scope.reject_unresolved_annotation(&param.ty);
            }
            if let Some(ty) = method.return_type.as_ref() {
                method_scope.reject_unresolved_annotation(ty);
            }

            let mut type_params = decl_slots.clone();
            type_params.extend(method_slots);

            let method_def = method_scope.def_at(method.id);
            methods.insert(
                method.name.clone(),
                super::sig::TraitMethod {
                    sig: MethodSig {
                        def: method_def,
                        decl: DeclSig {
                            type_params,
                            param_types,
                            return_type,
                        },
                        self_kind: method
                            .params
                            .first()
                            .map(|p| p.self_kind)
                            .unwrap_or(SelfKind::None),
                        params: method
                            .params
                            .iter()
                            .filter(|p| p.self_kind == SelfKind::None)
                            .map(|p| super::sig::Param {
                                name: p.name.clone(),
                                is_mut: p.is_mut,
                                default: p.default.clone(),
                            })
                            .collect(),
                        declaring_slot_count: decl_slots.len() as u32,
                        declaring_impl: None,
                        own_params: super::sig::own_params_of(&method.type_params),
                        cm_name: method
                            .attrs
                            .iter()
                            .find_map(crate::ast::Attribute::cm_identifier),
                        is_async: method.is_async,
                    },
                    default_body: method
                        .body
                        .as_ref()
                        .map(|_| std::rc::Rc::new(method.clone())),
                },
            );
        }

        let module = scope.current_module_source.clone();
        let trait_def = scope.def_at(trait_decl.id);
        scope
            .sem
            .decls
            .trait_sigs
            .insert(trait_def, super::sig::TraitSig { module, methods });
    }

    /// Lower an effect or resource declaration's method list to [`TirEffectOp`]s,
    /// with the enclosing type-param scope set up first so a generic resource's
    /// signatures can mention `T`. `resource_self` marks a resource decl, whose
    /// `&self` shorthand becomes a real parameter at index 0 to match
    /// `__cm_binding__<R>_<op>(self, args)`; an effect decl takes no receiver.
    pub(super) fn resolve_effect_ops(
        &mut self,
        type_params: &[ast::GenericParam],
        methods: &[ast::Function],
        resource_self: Option<crate::defs::DefId>,
    ) -> Vec<TirEffectOp> {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.register_generic_params(type_params, 0);

        // Construct the resource's `Self` type after type params are in
        // scope, so a generic resource's `GenericResource` instance can
        // reference its own `TypeParam`s (which gap-2 substitution then
        // specialises per impl-block instantiation). For non-generic
        // resources this is just a plain `Resource { def }`.
        let self_type: Option<TypeId> = resource_self.map(|def| {
            if type_params.iter().any(|p| !p.is_effect) {
                let type_arg_ids: Vec<TypeId> = type_params
                    .iter()
                    .filter(|p| !p.is_effect)
                    .map(|p| {
                        scope
                            .annotate_ctx
                            .trait_ctx
                            .type_params
                            .get(&p.name)
                            .map(|b| b.type_id)
                            .expect("type param registered by register_generic_params")
                    })
                    .collect();
                scope.tysys.type_table.borrow_mut().intern(
                    crate::tir::ResolvedType::GenericResource {
                        def,
                        type_args: type_arg_ids,
                    },
                )
            } else {
                scope.tysys.type_table.borrow_mut().make_resource(def)
            }
        });
        // `Self` in a resource method names the declaring resource.
        if self_type.is_some() {
            scope.annotate_ctx.trait_ctx.self_type = self_type;
        }

        let decl_slots: Vec<(String, TypeId)> = scope
            .annotate_ctx
            .trait_ctx
            .type_params
            .iter()
            .filter(|(_, b)| {
                let id = &b.type_id;
                let table = scope.tysys.type_table.borrow();
                matches!(
                    table.get(*id),
                    crate::tir::ResolvedType::TypeParam { .. }
                        | crate::tir::ResolvedType::TypePack { .. }
                )
            })
            .map(|(name, b)| (name.clone(), b.type_id))
            .collect();

        let mut ops = Vec::with_capacity(methods.len());
        for method in methods {
            let mut params = Vec::with_capacity(method.params.len());
            let mut sig_params = Vec::with_capacity(method.params.len());
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
                    (SelfKind::Ref, Some(self_t)) => {
                        scope.tysys.type_table.borrow_mut().make_ref(self_t)
                    }
                    (SelfKind::MutRef, Some(self_t)) => {
                        scope.tysys.type_table.borrow_mut().make_mut_ref(self_t)
                    }
                    // By-value `self` (consuming): the receiver is the resource
                    // itself, transferred.
                    (SelfKind::Value, Some(self_t)) => self_t,
                    // No `Self` in scope (effect decls) — drop the
                    // receiver as before; effect operations don't take
                    // receivers and the elaborator should already have
                    // diagnosed `&self` in an `effect` decl elsewhere.
                    _ => continue,
                };
                let name = if matches!(p.self_kind, SelfKind::None) {
                    sig_params.push(super::sig::Param {
                        name: p.name.clone(),
                        is_mut: p.is_mut,
                        default: p.default.clone(),
                    });
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
                    is_mut_ref: false,
                    span: p.span,
                });
                next_local += 1;
            }
            let return_type = method
                .return_type
                .as_ref()
                .map(|ty| scope.resolve_type(ty))
                .unwrap_or(TypeTable::UNIT);
            for param in &method.params {
                scope.reject_unresolved_annotation(&param.ty);
            }
            if let Some(ty) = method.return_type.as_ref() {
                scope.reject_unresolved_annotation(ty);
            }
            if method.is_async
                && scope
                    .tysys
                    .type_table
                    .borrow()
                    .as_async_call(return_type)
                    .is_none()
            {
                let _ = scope.emit(TypeError::AsyncOpMustReturnAsyncCall {
                    op_name: method.name.clone(),
                    span: method.span,
                });
            }
            // The `#[cm("...")]` payload, so dispatch synthesis maps a raw
            // resource call site back to its per-monomorphisation wrapper. The
            // bare attribute string, unsplit on `#`; `None` for an effect op
            // and for a resource method without the attribute. Recorded on the
            // signature, which is where every call site reads it.
            let cm_name = method
                .attrs
                .iter()
                .find_map(crate::ast::Attribute::cm_identifier);

            let self_kind = if self_type.is_some() {
                method
                    .params
                    .first()
                    .map_or(SelfKind::None, |p| p.self_kind)
            } else {
                SelfKind::None
            };
            let method_def = scope.def_at(method.id);
            scope.sem.decls.method_sigs.insert(
                method_def,
                MethodSig {
                    def: method_def,
                    decl: DeclSig {
                        type_params: decl_slots.clone(),
                        param_types: params.iter().map(|p| p.type_id).collect(),
                        return_type: method.return_type.as_ref().map(|_| return_type),
                    },
                    self_kind,
                    params: sig_params,
                    declaring_slot_count: decl_slots.len() as u32,
                    declaring_impl: None,
                    // An `interface` / `resource` operation declares no type
                    // parameters of its own.
                    own_params: Vec::new(),
                    cm_name: cm_name.clone(),
                    is_async: method.is_async,
                },
            );

            ops.push(TirEffectOp {
                name: method.name.clone(),
                params,
                return_type,
                span: method.span,
                cm_name,
                is_async: method.is_async,
                has_default: method.body.is_some(),
            });
        }
        ops
    }

    /// Reject what an operation declares that dispatch cannot honour, so a
    /// declaration says only what the language delivers. Each row pairs the
    /// offending shape with the message explaining why it cannot work.
    pub(super) fn reject_unsupported_operation_clauses(
        &mut self,
        owner: &str,
        methods: &[ast::Function],
        kind: OperationOwner,
    ) {
        let is_resource = matches!(kind, OperationOwner::Resource);
        for method in methods {
            // A resource operation is a CM import in every case; an effect
            // operation only when it carries the attribute.
            let cm_backed = is_resource || method.attrs.iter().any(|a| a.cm_boundary.is_some());
            // A resource method's `&self` becomes the CM adapter's first
            // parameter; an effect operation is called as `E::op(args)`.
            let receiver = (!is_resource)
                .then(|| {
                    method
                        .params
                        .iter()
                        .find(|p| p.self_kind != ast::SelfKind::None)
                })
                .flatten();
            let rejections: [(Option<Span>, &'static str); 7] = [
                (
                    method.body.as_ref().filter(|_| cm_backed).map(|b| b.span),
                    "cannot carry a default implementation: a Component Model import backs it, \
                     so it has no no-handler case for a default to serve",
                ),
                (
                    (method.is_async && method.body.is_some()).then_some(method.span),
                    "cannot carry a default implementation: an async operation's call site is \
                     typed as an `AsyncCall`, which a plain body does not produce",
                ),
                (
                    receiver.map(|p| p.span),
                    "cannot take a `self` receiver: an operation is called as \
                     `Effect::op(args)`, with no receiver to bind it to",
                ),
                (
                    method
                        .params
                        .iter()
                        .find(|p| p.default.is_some())
                        .map(|p| p.span),
                    "cannot give a parameter a default: an operation's call site is a dispatch \
                     wrapper, which takes the arguments as declared",
                ),
                (
                    (!method.effects.is_empty()).then_some(method.span),
                    "cannot declare effects: an operation's effects are not required at its \
                     call sites, so a default implementation must be performable wherever it \
                     is dispatched — reach for an `#[ambient]` function",
                ),
                (
                    (!method.stores.is_empty()).then_some(method.span),
                    "cannot declare `stores`: nothing checks the clause on an operation, so it \
                     would constrain call sites on a promise the handler never makes",
                ),
                (
                    method
                        .type_params
                        .iter()
                        .any(ast::GenericParam::is_real_type_param)
                        .then_some(method.span),
                    "cannot declare type parameters: dispatch holds one slot per operation, \
                     not one per instantiation",
                ),
            ];
            for (span, detail) in rejections {
                let Some(span) = span else {
                    continue;
                };
                let _ = self.emit(TypeError::OperationClauseNotAllowed {
                    owner: owner.to_string(),
                    operation: method.name.clone(),
                    detail,
                    span,
                });
            }
        }
    }

    pub(super) fn resolve_effect_decl(&mut self, decl: &ast::InterfaceDecl) -> TirEffect {
        let operations = self.declared_effect_ops(decl.id);
        self.sem
            .types
            .effect_ops
            .insert(decl.id, operations.clone());
        TirEffect {
            name: decl.name.clone(),
            visibility: decl.visibility,
            operations,
            span: decl.span,
        }
    }

    pub(super) fn resolve_resource_decl(&mut self, decl: &ast::ResourceDecl) -> TirResource {
        let operations = self.declared_effect_ops(decl.id);
        self.sem
            .types
            .effect_ops
            .insert(decl.id, operations.clone());
        TirResource {
            def: self
                .tysys
                .resolutions
                .defs()
                .of_ast_id(decl.id)
                .expect("a `resource` declaration is declared"),
            name: decl.name.clone(),
            visibility: decl.visibility,
            operations,
            is_generic: !decl.type_params.is_empty(),
            span: decl.span,
        }
    }

    /// Resolve a global variable declaration for its fact-recording side
    /// effects. Reify (`reify_global`) is the sole producer of the `TirGlobal`,
    /// re-emitting the initializer from the AST + recorded per-`AstId`
    /// expression types, so the body walk builds no TIR here.
    pub(super) fn resolve_global(&mut self, global_decl: &GlobalDecl) {
        let ty = self.resolve_type(&global_decl.ty);

        // Global initialization has no locals; the context only carries the
        // `#function` label. Reify must reproduce it byte-for-byte so the
        // per-`AstId` expression types line up, so both route through
        // `global_name`.
        let mut ctx = FunctionContext::new(
            ty,
            global_name(&self.current_module_source, &global_decl.name),
        );

        let initializer_type = self.resolve_expr(&global_decl.initializer, &mut ctx, Some(ty));

        self.typecheck(initializer_type, ty, global_decl.initializer.span());
    }

    /// Resolve a variant declaration
    pub(super) fn resolve_variant_decl(
        &mut self,
        variant_decl: &ast::VariantDecl,
    ) -> TirVariantDecl {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.register_generic_params(&variant_decl.type_params, 0);

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
                projected_from: None,
            })
            .collect();

        drop(scope);

        self.sem
            .types
            .decl_type_params
            .insert(variant_decl.id, type_params);

        register_variant_compiler_item(
            &self.tysys.type_table,
            &variant_decl.attrs,
            variant_decl.id,
            &variant_decl.name,
            &self.current_module_source,
            variant_decl.span,
            self.logger,
        );

        TirVariantDecl {
            def: self
                .tysys
                .resolutions
                .defs()
                .of_ast_id(variant_decl.id)
                .expect("a `variant` declaration is declared"),
            name: variant_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: variant_decl.visibility,
            type_params: vec![],
            cases: vec![],
            span: variant_decl.span,
            wire_name_policy: None,
        }
    }

    /// Validate that stores declarations reference valid reference parameters.
    fn validate_stores(&self, stores: &[String], params: &[TirParam], span: crate::token::Span) {
        let tt = self.tysys.type_table.borrow();
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
                    let _ = self.emit(TypeError::InvalidStores {
                        message: format!(
                            "stores[{store_name}]: parameter '{store_name}' has type '{type_name}', \
                             but only reference parameters (&T or &mut T) or type parameters can be stored"
                        ),
                        span,
                    });
                }
            } else {
                let _ = self.emit(TypeError::InvalidStores {
                    message: format!("stores[{store_name}]: no parameter named '{store_name}'"),
                    span,
                });
            }
        }
    }

    /// Populate `func`'s generic-inference caches without resolving its body, so
    /// a same-module forward reference — `outer<T>` written before `inner<T>` —
    /// can still run argument-derived inference at the call site. Idempotent, and
    /// mints fresh `TypeId`s each time; `resolve_function`'s later overwrite is
    /// what keeps the cache consistent with the body's own ids.
    pub(super) fn precompute_generic_function_cache(&mut self, func: &Function) {
        // Mirrors `resolve_function`'s guard: fn-bound params are realised
        // eagerly, so a function whose only non-effect params are fn-bound
        // has nothing to cache.
        let has_real_type_params = func
            .type_params
            .iter()
            .any(super::super::ast::GenericParam::is_real_type_param);
        if !has_real_type_params {
            return;
        }
        self.populate_generic_function_cache(func);
    }

    /// Resolve `func`'s canonical signature (see
    /// [`super::sem::decls::FunctionSig`]) and record its declared return
    /// type on `function_return_types`. The one signature resolution per
    /// function in the decl pass — the body walk re-resolves only to
    /// record per-node facts.
    pub(super) fn record_function_sig(
        &mut self,
        func: &Function,
    ) -> super::sem::decls::FunctionSig {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.annotate_ctx.trait_ctx.type_param_bounds.clear();
        scope
            .annotate_ctx
            .trait_ctx
            .install_effect_params(&func.type_params);
        scope.register_generic_params(&func.type_params, 0);
        let type_param_ids: Vec<(String, TypeId)> = scope
            .annotate_ctx
            .trait_ctx
            .type_params
            .iter()
            .map(|(name, b)| (name.clone(), b.type_id))
            .collect();
        let real_type_params: Vec<(String, TypeId)> = func
            .type_params
            .iter()
            .filter(|p| p.is_real_type_param())
            .filter_map(|p| {
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_params
                    .get(&p.name)
                    .map(|b| (p.name.clone(), b.type_id))
            })
            .collect();
        let param_types: Vec<TypeId> = func
            .params
            .iter()
            .map(|p| scope.resolve_type(&p.ty))
            .collect();
        let return_type = func.return_type.as_ref().map(|t| scope.resolve_type(t));
        // The frame still holds this function's type parameters, so they are
        // not mistaken for unknown names.
        for param in &func.params {
            scope.reject_unresolved_annotation(&param.ty);
        }
        if let Some(ty) = func.return_type.as_ref() {
            scope.reject_unresolved_annotation(ty);
        }
        let effects = scope.resolve_effects(&func.effects, &func.effect_ids);
        drop(scope);
        self.sem
            .decls
            .function_return_types
            .insert(func.name.clone(), return_type.unwrap_or(TypeTable::UNIT));
        super::sem::decls::FunctionSig {
            decl: super::sig::DeclSig {
                type_params: real_type_params,
                param_types,
                return_type,
            },
            type_param_ids,
            params: func
                .params
                .iter()
                .map(|p| super::sig::Param {
                    name: p.name.clone(),
                    is_mut: p.is_mut,
                    default: p.default.clone(),
                })
                .collect(),
            effects,
        }
    }

    /// Populate the three generic-function inference caches for `func`
    /// from its recorded [`super::sem::decls::FunctionSig`] — no
    /// re-resolution. Returns the declared return type for callers that
    /// need it (`resolve_function`'s `task_return_type`).
    fn populate_generic_function_cache(&mut self, func: &Function) -> TypeId {
        let def = self.def_at(func.id);
        let sig = self
            .sem
            .decls
            .function_sigs
            .get(&def)
            .expect("decl pass records every free function's canonical signature");
        let type_param_list = sig.decl.type_params.clone();
        let resolved_param_types = sig.decl.param_types.clone();
        let declared_return_type = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        self.sem
            .decls
            .generic_function_params
            .insert(func.name.clone(), type_param_list);
        self.sem
            .decls
            .generic_function_resolved_param_types
            .insert(func.name.clone(), resolved_param_types);
        self.sem
            .decls
            .generic_function_resolved_return_types
            .insert(func.name.clone(), declared_return_type);
        declared_return_type
    }

    /// Resolve a function
    pub(super) fn resolve_function(&mut self, func: &Function) -> Option<TirFunction> {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.annotate_ctx.trait_ctx.type_param_bounds.clear();
        scope.sem.decls.clear_fn_local_items();

        // Set effect params in scope before `register_generic_params`. Eager
        // `<F: fn() with E>` bound resolution runs inside
        // `register_generic_params` and consults `trait_ctx.effect_params`
        // to recognise `E` as `EffectRef::Param` rather than re-resolving it
        // to a phantom `EffectRef::Concrete`.
        let effect_params: Vec<_> = func.type_params.iter().filter(|p| p.is_effect).collect();
        if effect_params.len() > 1 {
            let _ = scope.emit(TypeError::InvalidLiteral {
                message: "multiple effect parameters are not allowed; use a single effect parameter instead".to_string(),
                span: effect_params[1].span,
            });
        }
        scope
            .annotate_ctx
            .trait_ctx
            .install_effect_params(&func.type_params);

        scope.register_generic_params(&func.type_params, 0);

        // Populate the generic-inference caches before the `function_return_types`
        // update below, which shares its map with non-generic callers and can be
        // overwritten by an external registration. A `<F: fn(…)>` bound is
        // eagerly realised to its function type and consumes no `TypeParam` slot,
        // being nothing monomorphisation needs to substitute.
        let has_real_type_params = func
            .type_params
            .iter()
            .any(super::super::ast::GenericParam::is_real_type_param);
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

        scope
            .sem
            .decls
            .function_return_types
            .insert(func.name.clone(), return_type);

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(declared_return_type);
        }

        // Resolve parameters. A default expression resolves in the callee's
        // lexical scope with only the earlier parameters visible, so it reaches
        // the definition module's private items. An `export fn` takes no
        // parameter default, and nothing crossing the Component Model boundary
        // takes a closure: the ABI represents neither.
        let is_cm_import =
            func.body.is_none() && func.attrs.iter().any(|a| a.cm_boundary.is_some());
        let crosses_cm_boundary = func.is_export || is_cm_import;

        let mut params = Vec::new();
        for param in &func.params {
            let type_id = scope.resolve_type(&param.ty);
            // Closures cannot cross the Component Model boundary.
            if crosses_cm_boundary && scope.type_contains_closure(type_id) {
                let _ = scope.emit(TypeError::ClosureAtCmBoundary {
                    function: func.name.clone(),
                    position: format!("parameter '{}'", param.name),
                    span: param.span,
                });
            }
            // A slice has no CM representation. Rejecting it here keeps the
            // static "an `export` appears in WIT" guarantee: reaching
            // `wit_emit` instead drops the component-type section wholesale.
            if crosses_cm_boundary && scope.type_contains_slice_view(type_id) {
                let _ = scope.emit(TypeError::SliceAtCmBoundary {
                    function: func.name.clone(),
                    position: format!("parameter '{}'", param.name),
                    span: param.span,
                });
            }
            // Walked for the recorded expression types only; reify
            // re-emits the default from the AST.
            if let Some(default_ast) = &param.default {
                if func.is_export {
                    let _ = scope.emit(TypeError::DefaultInExportFn {
                        function: func.name.clone(),
                        param: param.name.clone(),
                        span: default_ast.span(),
                    });
                }
                // A default is checked against the parameter type with the
                // signature's own type-parameter defaults applied. `fn
                // event<T = NoFields>(fields: T = NoFields {})` promises the
                // value only for the `T` the caller gets by default; against a
                // bare `T` — opaque, standing for whatever a caller picks —
                // nothing concrete could ever satisfy it.
                let expected = scope.apply_type_param_defaults(&func.type_params, type_id);
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(expected));
                scope.typecheck(resolved, expected, default_ast.span());
            }
            let index = ctx.add_local_at(
                param.name.clone(),
                type_id,
                param.is_mut,
                Some(param.id),
                param.span,
            );
            scope.record_local_symbol(
                param.id,
                &param.name,
                param.name_span,
                param.is_mut,
                type_id,
            );
            // `params` survives only to feed the recorded `fn_param_types`
            // and `validate_stores`; the TIR `default_expr` is not built.
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                is_mut_ref: false,
                span: param.span,
            });
        }

        // Closures cannot cross the CM boundary in return position either.
        if crosses_cm_boundary && scope.type_contains_closure(return_type) {
            let _ = scope.emit(TypeError::ClosureAtCmBoundary {
                function: func.name.clone(),
                position: "return type".to_string(),
                span: func.span,
            });
        }
        if crosses_cm_boundary && scope.type_contains_slice_view(declared_return_type) {
            let _ = scope.emit(TypeError::SliceAtCmBoundary {
                function: func.name.clone(),
                position: "return type".to_string(),
                span: func.span,
            });
        }

        scope.validate_stores(&func.stores, &params, func.span);

        if let Some(b) = func.body.as_ref() {
            scope.resolve_block(b, &mut ctx, None);
        }

        scope.validate_missing_return_ast(return_type, func.body.as_ref(), func.span);
        scope.validate_loop_jumps_ast(func.body.as_ref());

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
                if p.has_fn_bound() {
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
                    projected_from: None,
                })
            })
            .collect();

        let effects = scope.resolve_effects(&func.effects, &func.effect_ids);

        // Stash the resolved `Vec<EffectRef>` for reify: reify
        // cannot reconstruct effect-param canonicalisation without
        // `trait_ctx.effect_params`, so the annotate phase records
        // the already-resolved list here keyed by the function's `AstId`.
        let func_key = func.id;
        scope.sem.types.function_effects.insert(func_key, effects);

        // An async function's wasm return type is erased to
        // `()`; record the declared (pre-erasure) return type so reify
        // can set `task_return_type` for resource-store inference.
        if func.is_async {
            let task_key = func.id;
            scope
                .sem
                .types
                .function_task_returns
                .insert(task_key, declared_return_type);
        }

        drop(scope);

        // The return type recorded here is post-async-erasure.
        let sig_key = func.id;
        self.sem
            .types
            .fn_param_types
            .insert(sig_key, params.iter().map(|p| p.type_id).collect());
        self.sem.types.fn_return_types.insert(sig_key, return_type);
        self.sem.types.decl_type_params.insert(sig_key, type_params);

        Some(placeholder_function(func.name.clone(), func.span))
    }

    /// Resolve a test declaration to a `TirFunction` and `TirTest`
    pub(super) fn resolve_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
        module_is_todo: bool,
    ) -> Option<(TirFunction, TirTest)> {
        let meta = test_decl.metadata(module_is_todo);
        let ast::TestMetadata {
            expect_trap,
            is_todo,
            timeout_ms,
            is_synopsis,
        } = meta;
        let function_name =
            crate::name::test_function_name(&meta, test_index, test_decl.name.as_deref());

        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());

        self.sem.decls.clear_fn_local_items();

        // Recorded under `function_name` so `#function` literals match
        // what reify emits.
        self.resolve_block(&test_decl.body, &mut ctx, None);

        let tir_test = TirTest {
            name: test_decl.name.clone(),
            function_name: function_name.clone(),
            line: test_decl.span.line,
            span: test_decl.span,
            expect_trap,
            is_todo,
            timeout_ms,
            is_synopsis,
        };

        Some((
            placeholder_function(function_name, test_decl.span),
            tir_test,
        ))
    }

    /// Whether `impl_block` is a concrete generic instantiation (`impl List<u8>`,
    /// `impl Tag for [i32, i32]`) — a generic self type, tuples included, whose
    /// every argument is concrete. Its methods are per-instantiation functions
    /// named `List<u8>::method` and called directly. The tuple arm carries
    /// coherence Rule 1: the variadic template is skipped for that arity.
    ///
    /// "Concrete" is [`super::TypeSystem::impl_arg_pins_a_position`] and
    /// nothing else: this names the method, matching decides which receivers
    /// reach that name, and a second answer mints one name from two functions.
    pub(super) fn impl_is_concrete_instantiation(&self, impl_ty: &ast::Type) -> bool {
        let Some(args) = super::method_lookup::impl_target_args(impl_ty) else {
            return false;
        };
        !args.is_empty() && args.iter().all(|a| self.tysys.impl_arg_pins_a_position(a))
    }

    /// Resolve a method. Under `impl_is_concrete` the surrounding impl is a fully
    /// concrete instantiation, so its arguments are *not* registered as impl type
    /// params — there is no free parameter to keep aligned, unlike
    /// `impl TreeMap<String, V>`. The signature then resolves to `&List<u8>` and
    /// reify emits a standalone `List<u8>::method`.
    pub(super) fn resolve_method(
        &mut self,
        func: &Function,
        struct_name: &str,
        impl_type: &Type,
        trait_name: Option<&crate::name::FqTraitName>,
        trait_type: Option<&Type>,
        impl_is_concrete: bool,
        impl_declared_params: &[ast::GenericParam],
        recorded_sig: Option<&MethodSig>,
        impl_def: Option<crate::defs::DefId>,
    ) -> Option<TirFunction> {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.sem.decls.clear_fn_local_items();

        // Bare base trait name (e.g. `"Stream"` for an `impl Stream<u8>`).
        // Distinct from `trait_name`, which is the full mangled form
        // (`"Stream<u8>"`) used to make per-instantiation method names
        // unique. Effect / resource / trait decl indices are keyed by the
        // canonical `(decl_module, base name)` pair, so we also resolve
        // the trait reference through the current module's import context
        // so dispatch synthesis can tell two same-named effects /
        // resources apart.
        let base_trait_name: Option<String> = trait_type.map(|t| scope.get_type_name(t));

        let frame = scope.enter_impl_method_frame(
            func,
            impl_type,
            trait_type,
            impl_is_concrete,
            impl_declared_params,
        );
        let impl_type_params = frame.impl_type_params;
        let type_param_list = frame.method_type_params;

        // Keyed by the method's globally-unique `AstId`; per-impl
        // `ModuleSemantics` snapshots disambiguate one trait default body
        // synthesised into several impls.
        let method_key = func.id;
        scope
            .sem
            .types
            .method_impl_type_params
            .insert(method_key, impl_type_params);

        // A method the impl block declares has its canonical signature from
        // the decl pass, resolved in this same frame; reading it back is what
        // keeps the two passes from drifting. A trait *default* method being
        // synthesised into this impl has no such entry: its signature is
        // canonical per impl, not per declaration, so it resolves here until
        // the trait-decl digest (S6) gives it a per-impl key.
        let return_type = match recorded_sig {
            Some(sig) => sig.decl.return_type.unwrap_or(TypeTable::UNIT),
            None => func
                .return_type
                .as_ref()
                .map(|t| scope.resolve_type(t))
                .unwrap_or(TypeTable::UNIT),
        };

        // The receiver is named by the module that declares it — the written
        // name alone is not an identity. `display_name` below stays bare: it is
        // what diagnostics show, not what the registry keys on.
        let qualified_struct_name = scope.qualified_receiver_name_owned(struct_name, impl_def);
        let mangled_name = MethodName::format_local(&qualified_struct_name, trait_name, &func.name);
        scope
            .sem
            .decls
            .function_return_types
            .insert(mangled_name.clone(), return_type);

        // Diagnostics show the written name, not the registry key.
        let display_name =
            MethodName::format_local(&FqTypeName::binder(struct_name), None, &func.name);

        // Publish the mangled + display
        // names for reify to read straight off `MethodNames` instead of
        // running `format_local` itself against the impl facts.
        let method_names_key = func.id;
        scope.sem.types.method_names.insert(
            method_names_key,
            super::sem::types::MethodNames {
                display: display_name.clone(),
                mangled: mangled_name.clone(),
            },
        );

        let mut ctx = FunctionContext::new(return_type, display_name);
        // Mark this context as a handler method body when the surrounding
        // impl block targets an effect or resource declaration. `resume`
        // is only valid inside such bodies (see WEP 2026-04-11). Resources
        // share the handler-method semantics with effects: an
        // `impl Fields for CountingFields` method is a one-shot handler
        // body just like `impl Counter for BaseCounter`.
        //
        if let Some(name) = base_trait_name.as_deref() {
            let canonical_key = scope.decl_key_or_local(name);
            let declares = |index: &crate::hashmap::IndexSet<crate::defs::DefId>| {
                canonical_key.filter(|key| index.contains(key))
            };
            let effect_decl = declares(&scope.tysys.trait_env.effect_decl_index);
            let resource_decl = declares(&scope.tysys.trait_env.resource_decl_index);
            if effect_decl.is_some() || resource_decl.is_some() {
                ctx.in_handler_method = true;
            }
            let (decl_ref, is_resource_effect) = match (effect_decl, resource_decl) {
                (Some(d), _) => (Some(d), false),
                (None, Some(d)) => (Some(d), true),
                (None, None) => (None, false),
            };
            let async_op = decl_ref.and_then(|decl| {
                scope
                    .tysys
                    .signatures
                    .resource_method_sig(decl, &func.name)
                    .filter(|op| op.is_async)
                    .map(|op| op.cm_name.is_some())
            });
            if let Some(cm_backed) = async_op
                && (is_resource_effect || !cm_backed)
            {
                let _ = scope.emit(TypeError::AsyncUserEffectHandlerUnsupported {
                    interface_name: name.to_string(),
                    op_name: func.name.clone(),
                    span: func.span,
                });
            }
        }

        // Parameter types come from the decl pass for a method the impl block
        // declares; a synthesised trait default resolves its own (S5a).
        let param_types: Vec<TypeId> = match recorded_sig {
            Some(sig) => sig.decl.param_types.clone(),
            None => func
                .params
                .iter()
                .map(|param| scope.resolve_method_param_type(param))
                .collect(),
        };

        // Resolve parameters (including &self). Defaults are resolved in the
        // method's lexical scope with earlier parameters already bound.
        let mut params = Vec::new();
        for (param, &type_id) in func.params.iter().zip(param_types.iter()) {
            if param.self_kind == ast::SelfKind::Value {
                scope.check_self_by_value(type_id, param.span);
            }
            // Reject parameter defaults on trait-impl methods: defaults live
            // on the trait declaration only (WEP 2026-04-11).
            if trait_name.is_some()
                && let Some(default_ast) = &param.default
            {
                let _ = scope.emit(TypeError::DefaultInTraitImpl {
                    method: func.name.clone(),
                    param: param.name.clone(),
                    span: default_ast.span(),
                });
            }
            // Walk the default for its side-effect fact recording; the
            // resolved TIR is discarded (reify re-emits it from the AST).
            // Checked against the parameter type with the method's own
            // type-parameter defaults applied, as the free-function path does.
            if let Some(default_ast) = &param.default {
                let expected = scope.apply_type_param_defaults(&func.type_params, type_id);
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(expected));
                scope.typecheck(resolved, expected, default_ast.span());
            }
            let index = ctx.add_local_at(
                param.name.clone(),
                type_id,
                param.is_mut,
                Some(param.id),
                param.span,
            );
            scope.record_local_symbol(
                param.id,
                &param.name,
                param.name_span,
                param.is_mut,
                type_id,
            );
            // `params` survives only to feed the recorded `fn_param_types`
            // and `validate_stores`; the TIR `default_expr` is not built.
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                is_mut_ref: false,
                span: param.span,
            });
        }

        scope.validate_stores(&func.stores, &params, func.span);

        if let Some(b) = func.body.as_ref() {
            scope.resolve_block(b, &mut ctx, None);
        }

        scope.validate_missing_return_ast(return_type, func.body.as_ref(), func.span);
        scope.validate_loop_jumps_ast(func.body.as_ref());

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
                if p.has_fn_bound() {
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
                    projected_from: None,
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

        let effects = scope.resolve_effects(&func.effects, &func.effect_ids);

        // Stash the resolved `Vec<EffectRef>` for reify: reify
        // cannot reconstruct effect-param canonicalisation without
        // `trait_ctx.effect_params`, so the annotate phase records
        // the already-resolved list here keyed by the method's `AstId`.
        let method_key = func.id;
        scope.sem.types.function_effects.insert(method_key, effects);

        drop(scope);

        // Record the resolved param/return types for reify to read back
        // (single source of truth = this path); `params` is in `func.params`
        // order including the receiver.
        let sig_key = func.id;
        self.sem
            .types
            .fn_param_types
            .insert(sig_key, params.iter().map(|p| p.type_id).collect());
        self.sem.types.fn_return_types.insert(sig_key, return_type);
        // Record the method-level TIR type params (with defaults resolved while
        // the type-param scope was still alive, above) for reify to read back
        // rather than re-projecting them after its scope is torn down.
        self.sem.types.decl_type_params.insert(func.id, type_params);

        // Store type parameters for generic methods (for call site substitution)
        if !func.type_params.is_empty() {
            self.sem
                .decls
                .generic_method_params
                .insert(mangled_name.clone(), type_param_list);
            self.sem
                .decls
                .generic_method_resolved_param_types
                .insert(mangled_name, method_resolved_param_types);
        }

        // Reify (`reify_method`) emits the method's `TirFunction`
        // from the recorded facts (`method_impl_type_params`,
        // `method_names`, `fn_param_types`, `fn_return_types`,
        // `decl_type_params`, `function_effects`, the impl facts, …) + the
        // AST. No caller reads this return value, so a minimal shell
        // satisfies the signature.
        Some(placeholder_function(func.name.clone(), func.span))
    }
}

/// Which declaration an operation belongs to. A resource's operations are
/// Component Model imports with a `&self` receiver; an effect's are neither.
#[derive(Clone, Copy)]
pub(super) enum OperationOwner {
    Interface,
    Resource,
}
