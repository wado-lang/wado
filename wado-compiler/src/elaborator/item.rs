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
use crate::name::{MethodName, global_name};
use crate::tir::{
    FunctionKind, TirEffect, TirEffectOp, TirFunction, TirParam, TirResource, TirStruct, TirTest,
    TirVariantDecl, TypeId, TypeTable, method_param_offset,
};
use crate::token::Span;

use super::Elaborator;
use super::scope::TypeParamScope;
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

/// Body-walk placeholder for a function / method / test. Stage 7-B: the
/// combined walk records the signature facts (`fn_param_types`,
/// `fn_return_types`, `decl_type_params`, `function_effects`,
/// `method_names`, …) and resolves the body for its side-effect fact
/// recording, but no longer assembles the function's TIR — reify is the
/// sole producer. The returned `TirFunction` flows only into the
/// discarded `tir_module` of `resolve_module`, so a minimal shell with
/// the right name + span is all callers need.
fn placeholder_function(name: String, span: Span) -> TirFunction {
    TirFunction {
        module_source: ModuleSource::default(),
        name,
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

/// Register a struct declaration's `#[compiler_item(...)]` annotation, if any.
pub(super) fn register_struct_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
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
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
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
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
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
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
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
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
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
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
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
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
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
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
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
        report_register_error(err, span, module_source, logger);
    }
}

/// Register a named definition-less type (`pub type Array<T>;`) carrying a
/// `#[compiler_item("...")]` annotation. Binds the builtin type's name and
/// owning module so the type resolver can map the name to its builtin
/// `ResolvedType`.
pub(super) fn register_builtin_type_compiler_item<H: CompilerHost>(
    type_table: &RefCell<TypeTable>,
    attrs: &[crate::ast::Attribute],
    name: &str,
    module_source: &ModuleSource,
    span: Span,
    logger: &Logger<'_, H>,
) {
    let Some(item) = extract_compiler_item(attrs, span, module_source, logger) else {
        return;
    };
    if !check_compiler_item_placement(
        item,
        CompilerItemKind::BuiltinType,
        module_source,
        span,
        logger,
    ) {
        return;
    }
    let resolved = Resolved::BuiltinType {
        module_source: module_source.clone(),
        name: name.to_string(),
    };
    if let Err(err) = type_table
        .borrow_mut()
        .compiler_items_mut()
        .register(item, resolved)
    {
        report_register_error(err, span, module_source, logger);
    }
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
    ) -> crate::tir::TirTypeParam {
        let type_id = {
            let mut table = self.tysys.type_table.borrow_mut();
            if is_pack {
                table.make_type_pack(name.to_string(), index)
            } else {
                table.make_type_param(name.to_string(), index)
            }
        };
        self.annotate_ctx
            .trait_ctx
            .type_params
            .insert(name.to_string(), (index, type_id));
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
        generic: &ast::GenericType,
        impl_declared_params: &[ast::GenericParam],
    ) -> Vec<crate::tir::TirTypeParam> {
        let mut params = Vec::new();
        for (index, arg) in generic.args.iter().enumerate() {
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
            params.push(self.bind_target_param(name, index as u32, false, vec![], None));
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
        let Some(&(index, _)) = saved.type_params.get(&named.name) else {
            return Vec::new();
        };
        let bounds = self.saved_param_bounds(&named.name);
        let mut params = vec![self.bind_target_param(&named.name, index, false, bounds, None)];
        params.extend(self.bind_projected_packs(&named.name, index, saved, impl_declared_params));
        params
    }

    /// A pack `F` bound only through `T`'s associated type
    /// (`impl<T: Trait<Assoc = [..F]>, ..F: …>`) is not caller-supplied;
    /// monomorphization projects it from `T::Assoc`.
    fn bind_projected_packs(
        &mut self,
        target_name: &str,
        target_index: u32,
        saved: &super::scope::TraitContext,
        impl_declared_params: &[ast::GenericParam],
    ) -> Vec<crate::tir::TirTypeParam> {
        let projections: Vec<(String, String)> = impl_declared_params
            .iter()
            .find(|p| p.name == target_name)
            .into_iter()
            .flat_map(|t_param| &t_param.bounds)
            .flat_map(|bound| &bound.assoc_types)
            .filter_map(|assoc| match &assoc.ty {
                ast::Type::Tuple(elems) => Some((elems, assoc.name.clone())),
                _ => None,
            })
            .flat_map(|(elems, assoc_name)| {
                elems.iter().filter_map(move |elem| match elem {
                    ast::Type::TypePackSpread(f_name, _) => {
                        Some((f_name.clone(), assoc_name.clone()))
                    }
                    _ => None,
                })
            })
            .collect();
        let mut params = Vec::new();
        for (pack_name, assoc_name) in projections {
            let Some(&(index, _)) = saved.type_params.get(&pack_name) else {
                continue;
            };
            let bounds = self.saved_param_bounds(&pack_name);
            params.push(self.bind_target_param(
                &pack_name,
                index,
                true,
                bounds,
                Some((target_index, assoc_name)),
            ));
        }
        params
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
        let Some(&(index, _)) = saved.type_params.get(&named.name) else {
            return Vec::new();
        };
        let bounds = self.saved_param_bounds(&named.name);
        vec![self.bind_target_param(&named.name, index, false, bounds, None)]
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
            let Some(&(index, _)) = saved.type_params.get(name) else {
                continue;
            };
            let bounds = self.saved_param_bounds(name);
            params.push(self.bind_target_param(name, index, true, bounds, None));
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
        let impl_type_params = if let ast::Type::Generic(generic) = impl_type_inner
            && !impl_is_concrete
        {
            self.bind_declared_target_params(generic, impl_declared_params)
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

        // In declaration order, and inserted into scope as we go, so a later
        // `type B = Self::A` sees the earlier binding.
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

        scope.sem.decls.impl_sigs.insert(
            impl_block.id,
            super::sig::ImplSig {
                target_type_args,
                trait_type_args,
                associated_types,
            },
        );
    }

    /// The type arguments a generic type reference writes, resolved in the
    /// current frame. A non-generic reference writes none.
    pub(super) fn resolve_written_type_args(&mut self, ty: &Type) -> Vec<TypeId> {
        let Type::Generic(generic) = ty else {
            return Vec::new();
        };
        generic
            .args
            .clone()
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
            self.annotate_ctx
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
    /// Resolve one method parameter's type. A receiver comes from the impl
    /// target — the parser desugars `self` / `&self` / `&mut self` into
    /// `Self`-based annotations — and anything else from its annotation.
    fn resolve_method_param_type(&mut self, param: &ast::Param, impl_type: &Type) -> TypeId {
        match param.self_kind {
            ast::SelfKind::Value => self.resolve_type(impl_type),
            ast::SelfKind::Ref => {
                let inner = self.resolve_type(impl_type);
                self.tysys.type_table.borrow_mut().make_ref(inner)
            }
            ast::SelfKind::MutRef => {
                let inner = self.resolve_type(impl_type);
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
    /// AST under the *caller's* perspective (WEP 2026-07-10).
    pub(super) fn record_impl_decls(&mut self, impl_block: &ast::ImplBlock) {
        let mut block = self.enter_inherited_type_param_scope();
        block.annotate_ctx.trait_ctx.type_params.clear();
        block.annotate_ctx.trait_ctx.type_param_bounds.clear();
        block.register_impl_block_params(impl_block);

        block.annotate_ctx.trait_ctx.assoc_type_bindings.clear();

        let impl_is_concrete = block.impl_is_concrete_instantiation(
            &impl_block.ty,
            &impl_block.type_params,
            &block.current_module_source.clone(),
        );

        // Recorded for a synthesis request too: it declares a target and a
        // trait reference like any other block, and a total digest lets
        // dispatch read one without a fallback. Its *methods* do not exist
        // yet, so only the loop below is skipped.
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
            let mut type_params: Vec<(String, TypeId)> = frame
                .impl_type_params
                .iter()
                .filter_map(|tp| {
                    frame_scope
                        .annotate_ctx
                        .trait_ctx
                        .type_params
                        .get(&tp.name)
                        .map(|&(_, id)| (tp.name.clone(), id))
                })
                .collect();
            let declaring_slot_count = type_params.len() as u32;
            type_params.extend(frame.method_type_params.iter().cloned());
            let self_kind = method
                .params
                .first()
                .map(|p| p.self_kind)
                .unwrap_or(ast::SelfKind::None);
            frame_scope.sem.decls.method_sigs.insert(
                method.id,
                MethodSig {
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
            crate::tir::ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                // Recurse into the struct's field types via the elaborator's
                // pre-built field registry. Self-recursive structs are
                // protected by `visited`.
                let field_types: Vec<TypeId> = self
                    .lookup_struct_fields_in(name, module_source)
                    .map(|info| info.fields.iter().map(|(_, ty, _)| *ty).collect())
                    .unwrap_or_default();
                field_types
                    .into_iter()
                    .any(|t| self.type_contains_closure_inner(type_table, t, visited))
            }
            crate::tir::ResolvedType::Variant {
                name,
                module_source,
            } => {
                // The per-case payload types live in `all_variant_cases`; look
                // them up so a variant case payload containing a closure type
                // fails the CM boundary check too.
                let payloads: Vec<TypeId> = self
                    .lookup_variant_case_in(name, module_source)
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
        // resolution we just did, with `loaded_modules` in scope, can.
        self.sem
            .types
            .struct_field_types
            .insert(struct_decl.id, struct_field_types);

        TirStruct {
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
    /// Operation signatures the decl pass recorded for `decl_id`.
    fn declared_effect_ops(&self, decl_id: ast::AstId) -> Vec<TirEffectOp> {
        self.sem
            .decls
            .effect_ops
            .get(&decl_id)
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

        // `Self` is a slot, not a known type: a trait declaration says
        // nothing about what implements it. Registering it as slot 0 is what
        // turns `Self::Assoc` in a signature into a projection over that
        // slot rather than an unresolved-type error.
        let self_slot = scope
            .tysys
            .type_table
            .borrow_mut()
            .make_type_param("Self".to_string(), 0);
        scope
            .annotate_ctx
            .trait_ctx
            .type_params
            .insert("Self".to_string(), (0, self_slot));
        // The slot is bounded by the trait being declared — that is what
        // makes `Self` usable where the trait is required, as in
        // `Iterator::map` returning `IterMap<Self, U>` against
        // `struct IterMap<I: Iterator, U>`. Without the bound `Self` is a
        // parameter that satisfies nothing and every such signature in the
        // declaration fails its own well-formedness check.
        scope.annotate_ctx.trait_ctx.type_param_bounds.insert(
            "Self".to_string(),
            vec![ast::TraitBound {
                name: trait_decl.name.clone(),
                assoc_types: Vec::new(),
                span: trait_decl.span,
                fn_signature: None,
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
                    .map(|&(_, id)| (tp.name.clone(), id))
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
            let method_slots: Vec<(String, TypeId)> = method
                .type_params
                .iter()
                .filter(|p| !p.is_effect)
                .filter_map(|tp| {
                    method_scope
                        .annotate_ctx
                        .trait_ctx
                        .type_params
                        .get(&tp.name)
                        .map(|&(_, id)| (tp.name.clone(), id))
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

            let mut type_params = decl_slots.clone();
            type_params.extend(method_slots);

            methods.insert(
                method.name.clone(),
                super::sig::TraitMethod {
                    sig: MethodSig {
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
        scope
            .sem
            .decls
            .trait_sigs
            .insert(trait_decl.id, super::sig::TraitSig { module, methods });
    }

    pub(super) fn resolve_effect_ops(
        &mut self,
        type_params: &[ast::GenericParam],
        methods: &[ast::InterfaceMethod],
        resource_self: Option<(&str, ModuleSource)>,
    ) -> Vec<TirEffectOp> {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
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
                            .annotate_ctx
                            .trait_ctx
                            .type_params
                            .get(&p.name)
                            .map(|(_, id)| *id)
                            .expect("type param registered by register_generic_params")
                    })
                    .collect();
                scope.tysys.type_table.borrow_mut().intern(
                    crate::tir::ResolvedType::GenericResource {
                        name: name.to_string(),
                        module_source: module,
                        type_args: type_arg_ids,
                    },
                )
            } else {
                scope
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_resource(name.to_string(), module)
            }
        });

        // The declaration's own slots, in the order `register_generic_params`
        // numbered them. Fn-bound parameters consume no slot, so only the
        // real ones are listed — the density [`DeclSig`] documents.
        let decl_slots: Vec<(String, TypeId)> = scope
            .annotate_ctx
            .trait_ctx
            .type_params
            .iter()
            .filter(|(_, (_, id))| {
                let table = scope.tysys.type_table.borrow();
                matches!(
                    table.get(*id),
                    crate::tir::ResolvedType::TypeParam { .. }
                        | crate::tir::ResolvedType::TypePack { .. }
                )
            })
            .map(|(name, (_, id))| (name.clone(), *id))
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

            // The same resolution, recorded as a method signature so
            // dispatch reads it instead of re-resolving the declaration.
            // An effect operation takes no receiver, so its `self_kind` is
            // `None` however the AST spelled it — the receiver parameter
            // above was dropped for exactly that reason.
            let self_kind = if self_type.is_some() {
                method
                    .params
                    .first()
                    .map_or(SelfKind::None, |p| p.self_kind)
            } else {
                SelfKind::None
            };
            scope.sem.decls.method_sigs.insert(
                method.id,
                MethodSig {
                    decl: DeclSig {
                        type_params: decl_slots.clone(),
                        param_types: params.iter().map(|p| p.type_id).collect(),
                        return_type: method.return_type.as_ref().map(|_| return_type),
                    },
                    self_kind,
                    params: sig_params,
                    // An operation declares no type parameters of its own,
                    // so every slot belongs to the declaration.
                    declaring_slot_count: decl_slots.len() as u32,
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
            });
        }
        ops
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
    /// expression types, so the combined walk builds no TIR here.
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
            &variant_decl.name,
            &self.current_module_source,
            variant_decl.span,
            self.logger,
        );

        TirVariantDecl {
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
            .map(|(name, &(_, id))| (name.clone(), id))
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
                    .map(|&(_, id)| (p.name.clone(), id))
            })
            .collect();
        let param_types: Vec<TypeId> = func
            .params
            .iter()
            .map(|p| scope.resolve_type(&p.ty))
            .collect();
        let return_type = func.return_type.as_ref().map(|t| scope.resolve_type(t));
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
        let sig = self
            .sem
            .decls
            .function_sigs
            .get(&func.name)
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
                let _ = scope.emit(TypeError::ClosureAtCmBoundary {
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
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(type_id));
                scope.typecheck(resolved, type_id, default_ast.span());
            }
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
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

        scope.validate_stores(&func.stores, &params, func.span);

        if let Some(b) = func.body.as_ref() {
            scope.resolve_block(b, &mut ctx, None);
        }

        scope.validate_missing_return_ast(return_type, func.body.as_ref(), func.span);

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

        // Stash the resolved `Vec<EffectRef>` for reify (Stage 5): reify
        // cannot reconstruct effect-param canonicalisation without
        // `trait_ctx.effect_params`, so the annotate phase records
        // the already-resolved list here keyed by the function's `AstId`.
        let func_key = func.id;
        scope.sem.types.function_effects.insert(func_key, effects);

        // Stage 5: an async function's wasm return type is erased to
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

    /// Whether an impl type argument is a concrete (already-declared) type
    /// rather than a free type parameter. `u8` / `MyStruct` are concrete; a
    /// bare `T` is not — and neither is a name that, although it matches a
    /// known type, was *declared as an impl type parameter* (e.g. the `i32`
    /// in `impl<i32> Trait for Wrapper<i32>`, which shadows the primitive).
    pub(super) fn is_concrete_type_arg(
        &self,
        arg: &ast::Type,
        impl_params: &[ast::GenericParam],
        impl_module: &ModuleSource,
    ) -> bool {
        match arg {
            ast::Type::Named(named) => {
                self.tysys.is_known_type_name_in(impl_module, &named.name)
                    && !impl_params.iter().any(|p| p.name == named.name)
            }
            ast::Type::Generic(generic) => generic
                .args
                .iter()
                .all(|a| self.is_concrete_type_arg(a, impl_params, impl_module)),
            ast::Type::Tuple(elems) => elems
                .iter()
                .all(|e| self.is_concrete_type_arg(e, impl_params, impl_module)),
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
                self.is_concrete_type_arg(inner, impl_params, impl_module)
            }
            _ => false,
        }
    }

    /// Whether `impl_block` is a concrete generic instantiation (`impl List<u8>`,
    /// `impl Tag for List<u8>`): its self type is a generic type all of whose
    /// arguments are concrete (no free type params, accounting for any
    /// impl-declared parameters). Methods on such an impl are per-instantiation
    /// concrete functions, named `List<u8>::method` and called directly.
    pub(super) fn impl_is_concrete_instantiation(
        &self,
        impl_ty: &ast::Type,
        impl_type_params: &[ast::GenericParam],
        impl_module: &ModuleSource,
    ) -> bool {
        let inner = match impl_ty {
            ast::Type::Reference(i) | ast::Type::MutReference(i) => i.as_ref(),
            other => other,
        };
        matches!(inner, ast::Type::Generic(g)
        if !g.args.is_empty()
            && g.args.iter().all(|a| {
                self.is_concrete_type_arg(a, impl_type_params, impl_module)
            }))
    }

    /// Resolve a method (function with &self parameter).
    ///
    /// `impl_is_concrete` is `true` when the surrounding impl is a fully
    /// concrete generic instantiation (`impl List<u8>`, all args concrete).
    /// Such impls define per-instantiation *concrete* methods. Unlike a
    /// partially-generic impl (`impl TreeMap<String, V>`, where `String` must
    /// keep its positional slot so `V`'s index stays aligned for
    /// monomorphization), a fully-concrete impl has no free param to align, so
    /// its args are NOT registered as impl type params: the method's `self` /
    /// param / return types resolve to the concrete instantiation (`&List<u8>`,
    /// not `&List<TypeParam>`), and reify emits a standalone concrete function
    /// named `List<u8>::method`.
    pub(super) fn resolve_method(
        &mut self,
        func: &Function,
        struct_name: &str,
        impl_type: &Type,
        trait_name: Option<&str>,
        trait_type: Option<&Type>,
        impl_is_concrete: bool,
        impl_declared_params: &[ast::GenericParam],
        recorded_sig: Option<&MethodSig>,
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

        let mangled_name = MethodName::format_local(struct_name, trait_name, &func.name);
        scope
            .sem
            .decls
            .function_return_types
            .insert(mangled_name.clone(), return_type);

        let display_name = MethodName::format_local(struct_name, None, &func.name);

        // Stage 5 / mangled-name slice: publish the mangled + display
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
            let canonical_key = scope.canonical_decl_key(name);
            let effect_decl = scope
                .tysys
                .trait_env
                .effect_decl_index
                .get(&canonical_key)
                .cloned();
            let resource_decl = scope
                .tysys
                .trait_env
                .resource_decl_index
                .get(&canonical_key)
                .cloned();
            if effect_decl.is_some() || resource_decl.is_some() {
                ctx.in_handler_method = true;
            }
            let (decl_ref, is_resource_effect) = match (effect_decl, resource_decl) {
                (Some(d), _) => (Some(d), false),
                (None, Some(d)) => (Some(d), true),
                (None, None) => (None, false),
            };
            let async_op = decl_ref.and_then(|(_, decl_id)| {
                scope
                    .tysys
                    .signatures
                    .resource_method_sig(decl_id, &func.name)
                    .filter(|op| op.is_async)
                    .map(|op| op.cm_name.is_some())
            });
            if let Some(cm_backed) = async_op
                && (is_resource_effect || !cm_backed)
            {
                let _ = scope
                    .logger
                    .error(TypeError::AsyncUserEffectHandlerUnsupported {
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
                .map(|param| scope.resolve_method_param_type(param, impl_type))
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
            if let Some(default_ast) = &param.default {
                let resolved = scope.resolve_expr(default_ast, &mut ctx, Some(type_id));
                scope.typecheck(resolved, type_id, default_ast.span());
            }
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
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

        // Stash the resolved `Vec<EffectRef>` for reify (Stage 5): reify
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

        // Stage 7-B: reify (`reify_method`) emits the method's `TirFunction`
        // from the recorded facts (`method_impl_type_params`,
        // `method_names`, `fn_param_types`, `fn_return_types`,
        // `decl_type_params`, `function_effects`, the impl facts, …) + the
        // AST. The combined walk's copy is discarded, so a minimal shell
        // is all `resolve_module` needs.
        Some(placeholder_function(func.name.clone(), func.span))
    }
}
