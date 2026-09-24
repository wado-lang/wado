//! Item-level resolution (structs, functions, methods, globals, variants, tests).

use std::cell::RefCell;

use crate::ast::{self, Function, GlobalDecl, SelfKind, Type};
use crate::attribute::{self, WIRE};
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
use super::scope::{BinderInScope, ScopedBound, TypeParamScope, param_decl};
use super::sig::{DeclSig, MethodSig};
use super::trait_query::SelfBinding;
use super::types::{FunctionContext, TypeError};
use crate::ast::{AssociatedTypeDecl, AstId, Attribute, GenericParam, Visibility};
use crate::compiler_item::TraitAssocType;
use crate::defs::{DefId, DefKind};
use crate::elaborator::method_lookup::{ImplParamSlots, impl_target_args, impl_target_head_args};
use crate::elaborator::sem::decls::FunctionSig;
use crate::elaborator::sem::types::MethodNames;
use crate::elaborator::sig;
use crate::elaborator::sig::{ImplSig, TraitMethod, TraitSig, own_params_of};
use crate::elaborator::trait_env::get_type_name_static;
use crate::name::{FqTraitName, test_function_name};
use crate::tir::{ResolvedType, StructDef, TirTypeParam};
use crate::{hashmap, tir};

/// Extract the [`CompilerItem`] marker — if any — from a declaration's
/// `#[compiler_item("...")]` attributes, emitting a diagnostic for
/// each unrecognised name. Returns the first matched [`CompilerItem`]
/// (in attribute order); subsequent matches are silently dropped — a
/// declaration may carry at most one marker per the design contract.
pub(super) fn extract_compiler_item<H: CompilerHost>(
    attrs: &[Attribute],
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
        visibility: Visibility::Private,
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
        retains: vec![],
        immediates: vec![],
        trap: None,
        linear_memory: None,
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
        inline_hint: tir::InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        declared_return_convention: None,
        kind: FunctionKind::Regular,
        return_abi: tir::ReturnAbi::default(),
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
    attrs: &[Attribute],
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
    attrs: &[Attribute],
    decl: AstId,
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
    attrs: &[Attribute],
    decl: AstId,
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
    attrs: &[Attribute],
    decl: AstId,
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
    attrs: &[Attribute],
    decl: AstId,
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
    attrs: &[Attribute],
    decl: AstId,
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
    attrs: &[Attribute],
    decl: AstId,
    name: &str,
    methods: &[ast::Function],
    assoc_types: &[AssociatedTypeDecl],
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
        .map(|a| TraitAssocType {
            name: a.name.clone(),
            bound_names: a.bounds.iter().map(|b| b.name.clone()).collect(),
        })
        .collect();
    let fq = type_table
        .borrow()
        .defs()
        .of_ast_id(decl)
        .map(|def| FqTraitName::declared(type_table.borrow().defs(), def));
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
    attrs: &[Attribute],
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
    attrs: &[Attribute],
    method_name: &str,
    owner_type: &str,
    owner_head: &FqTypeName,
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
    attrs: &[Attribute],
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
    attrs: &[Attribute],
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
    attrs: &[Attribute],
    decl: AstId,
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
    attrs: &[Attribute],
    decl: AstId,
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
    pub(super) impl_type_params: Vec<TirTypeParam>,
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
    ) -> TirTypeParam {
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
        TirTypeParam {
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
        slots: &ImplParamSlots,
    ) -> Vec<TirTypeParam> {
        let mut params = Vec::new();
        for arg in target_args {
            let ast::Type::Named(named) = arg else {
                continue;
            };
            let name = &named.name;
            if self.annotate_ctx.trait_ctx.type_params.contains_key(name) {
                continue;
            }
            if !self.tysys.is_impl_target_param(impl_declared_params, name) {
                // A name the block does not declare has to be a type the module
                // does; otherwise it names nothing at all.
                if !self.names_type_at(Some(named.id), name) {
                    let _ = self.emit(TypeError::UndeclaredImplTypeParam {
                        name: name.clone(),
                        span: named.span,
                    });
                }
                continue;
            }
            let Some(slot) = slots.of_name(name) else {
                continue;
            };
            params.push(self.bind_target_param(
                name,
                slot,
                false,
                vec![],
                None,
                param_decl(impl_declared_params, name),
            ));
        }
        params
    }

    /// `impl<I: Iterator> IntoIterator for I` — the target *is* a parameter,
    /// registered by the caller and now living in the saved frame.
    fn bind_blanket_target_param(
        &mut self,
        named: &ast::NamedType,
        impl_declared_params: &[ast::GenericParam],
        slots: &ImplParamSlots,
    ) -> Vec<TirTypeParam> {
        let Some(target_index) = slots.of_name(&named.name) else {
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
            let Some(index) = slots.of_name(&declared.name) else {
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
    ) -> hashmap::IndexMap<String, String> {
        let mut out = hashmap::IndexMap::default();
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
        impl_declared_params: &[ast::GenericParam],
        slots: &ImplParamSlots,
    ) -> Vec<TirTypeParam> {
        let ast::Type::Named(named) = inner else {
            return Vec::new();
        };
        let Some(index) = slots.of_name(&named.name) else {
            return Vec::new();
        };
        let bounds = self.saved_param_bounds(&named.name);
        let decl = param_decl(impl_declared_params, &named.name);
        vec![self.bind_target_param(&named.name, index, false, bounds, None, decl)]
    }

    /// `impl<..T: Trait> Trait for [..T]` — the target's spread elements are
    /// the packs.
    fn bind_tuple_pack_params(
        &mut self,
        elements: &[ast::Type],
        impl_declared_params: &[ast::GenericParam],
        slots: &ImplParamSlots,
    ) -> Vec<TirTypeParam> {
        let mut params = Vec::new();
        for element in elements {
            let ast::Type::TypePackSpread(name, _) = element else {
                continue;
            };
            let Some(index) = slots.of_name(name) else {
                continue;
            };
            let bounds = self.saved_param_bounds(name);
            let decl = param_decl(impl_declared_params, name);
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
    ) -> Vec<TirTypeParam> {
        let slots = ImplParamSlots::of(impl_type, impl_declared_params);
        let impl_type_inner = match impl_type {
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => inner.as_ref(),
            other => other,
        };
        // However the head is spelled: `Cell<T>` and `ns::Cell<T>` write one
        // target a namespace apart.
        let head_args = impl_target_head_args(impl_type_inner);
        let impl_type_params = if let Some(args) = head_args
            && !impl_is_concrete
        {
            self.bind_declared_target_params(args, impl_declared_params, &slots)
        } else {
            match impl_type {
                ast::Type::Named(named) => {
                    self.bind_blanket_target_param(named, impl_declared_params, &slots)
                }
                ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) => {
                    self.bind_ref_target_param(boxed.as_ref(), impl_declared_params, &slots)
                }
                ast::Type::Tuple(elements) => {
                    self.bind_tuple_pack_params(elements, impl_declared_params, &slots)
                }
                _ => Vec::new(),
            }
        };

        let saved_bounds = self.saved().type_param_bounds.clone();
        self.annotate_ctx.trait_ctx.type_param_bounds = saved_bounds;

        // Before the trait's parameters, since `impl<T> From<T> for ByAny`
        // spells both `T`: binding the trait's first claims the name for an
        // argument that does not resolve yet, and the block's own slot never
        // gets made.
        let mut impl_type_params = impl_type_params;
        for param in impl_declared_params
            .iter()
            .filter(|p| p.is_real_type_param())
        {
            if self
                .annotate_ctx
                .trait_ctx
                .type_params
                .contains_key(&param.name)
            {
                continue;
            }
            let slot = slots
                .of_name(&param.name)
                .expect("a real type parameter fills an impl slot");
            let bounds = self.saved_param_bounds(&param.name);
            impl_type_params.push(self.bind_target_param(
                &param.name,
                slot,
                param.is_pack,
                bounds,
                None,
                Some(param.id),
            ));
        }

        // After the impl's own parameters, which `Maker<Container<U>>` names,
        // and before the trait's, whose bounds pin the `Self` they mean.
        let implementing = self.impl_self_binding(impl_type, trait_type);
        self.set_self_binding(implementing);
        if let Some(trait_t) = trait_type {
            self.bind_trait_type_params_from_impl(trait_t, implementing);
        }
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

        let mut associated_types = hashmap::IndexMap::default();
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
            .and_then(|t| scope.tysys.resolutions.head_decl(t));
        let impl_def = scope.def_at(impl_block.id);
        scope.sem.decls.impl_sigs.insert(
            impl_def,
            ImplSig {
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
            if param.is_effect || named.iter().any(|n| n == &param.name) {
                continue;
            }
            let _ = self.emit(TypeError::UnconstrainedImplTypeParam {
                param_name: param.name.clone(),
                span: impl_block.span,
            });
        }
    }

    /// Require that an `impl` header's trait position names a trait, an
    /// `interface` or a resource, at the header's own reference site.
    fn check_impl_trait_resolves(&mut self, impl_block: &ast::ImplBlock, trait_type: &Type) {
        if self.impl_trait_decl(trait_type).is_some() {
            return;
        }
        let _ = self.emit(TypeError::UnknownTraitImpl {
            name: get_type_name_static(trait_type),
            span: impl_block.span,
        });
    }

    fn reject_second_effect_param(&mut self, type_params: &[ast::GenericParam]) {
        if let Some(second) = type_params.iter().filter(|p| p.is_effect).nth(1) {
            let _ = self.emit(TypeError::SecondEffectParam { span: second.span });
        }
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
        let Some(args) = impl_target_head_args(inner) else {
            return Vec::new();
        };
        args.iter().map(|arg| self.resolve_type(arg)).collect()
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

        self.reject_second_effect_param(&func.type_params);

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
            let (type_id, consumed_index) = match fn_bound_sig {
                Some(sig) => (self.resolve_type(&ast::Type::Function(sig.clone())), false),
                None => (
                    self.tysys.type_table.borrow_mut().make_declared_param(
                        param.name.clone(),
                        idx,
                        param.is_pack,
                    ),
                    true,
                ),
            };
            let bounds = ScopedBound::pin_declared(param, self.self_binding());
            self.bind_param(
                &param.name,
                BinderInScope::declared(idx, type_id, param.id),
                bounds,
            );
            // Only push *real* type params (TypeParam-ids) into the
            // inference cache list. Eagerly-resolved fn-bound params have a
            // concrete Function type and aren't generics anymore.
            if fn_bound_sig.is_none() {
                type_param_list.push((param.name.clone(), type_id));
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
impl<'a, H: CompilerHost> Elaborator<'a, H> {
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
        let mut subst = hashmap::IndexMap::default();
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

    /// Resolve a parameter's default and check it against the parameter type,
    /// with the signature's own type-parameter defaults applied:
    /// `fn event<T = NoFields>(fields: T = NoFields {})` promises that value
    /// only for the `T` a caller gets by default. A default naming the
    /// parameter itself (`fields: T = T::default()`) answers for every `T`, so
    /// it is checked against the bare one.
    fn check_param_default(
        &mut self,
        default_ast: &ast::Expr,
        type_id: TypeId,
        type_params: &[ast::GenericParam],
        ctx: &mut FunctionContext,
    ) {
        let defaulted = self.apply_type_param_defaults(type_params, type_id);
        let resolved = self.resolve_expr(default_ast, ctx, Some(defaulted));
        let expected = if resolved == type_id {
            type_id
        } else {
            defaulted
        };
        self.typecheck(resolved, expected, default_ast.span());
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
            frame_scope.reject_signature_annotations(&method.params, method.return_type.as_ref());
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
            let method_slot_base = method_param_offset(&frame.impl_type_params);
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
                        .map(|p| sig::Param {
                            name: p.name.clone(),
                            is_mut: p.is_mut,
                            default: p.default.clone(),
                        })
                        .collect(),
                    declaring_slot_count,
                    method_slot_base,
                    declaring_impl: Some(impl_def),
                    own_params: own_params_of(&method.type_params),
                    cm_name: method.attrs.iter().find_map(Attribute::cm_identifier),
                    is_async: method.is_async,
                    // A trait impl takes the trait's, once every module's
                    // declarations are assembled.
                    defaults_module: None,
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
        type_table: &TypeTable,
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
                if type_table.compiler_item_def(CompilerItem::Slice) == Some(*def) {
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
        type_table: &TypeTable,
        type_id: TypeId,
        visited: &mut IndexSet<TypeId>,
    ) -> bool {
        if !visited.insert(type_id) {
            return false;
        }
        match type_table.get(type_id) {
            ResolvedType::Function { .. } => true,
            ResolvedType::Ref(t)
            | ResolvedType::MutRef(t)
            | ResolvedType::Reactive(t)
            | ResolvedType::BuiltinArray(t) => {
                self.type_contains_closure_inner(type_table, *t, visited)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => type_args
                .iter()
                .any(|t| self.type_contains_closure_inner(type_table, *t, visited)),
            ResolvedType::Newtype { base_type, .. } => {
                self.type_contains_closure_inner(type_table, *base_type, visited)
            }
            ResolvedType::Struct { .. } => {
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
            ResolvedType::Variant { .. } => {
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

    /// One field's declared type, with what the declaration cannot mean
    /// reported and its default expression resolved against that type. A local
    /// struct resolves its fields through here too, so a default coerces the
    /// same way wherever the struct is written.
    pub(super) fn resolve_struct_field(
        &mut self,
        field: &ast::StructField,
        field_ctx: &mut FunctionContext,
    ) -> TypeId {
        let type_id = self.resolve_type(&field.ty);
        self.reject_written_annotation(&field.ty);
        if let Some(serde_default) = field
            .attrs
            .iter()
            .find(|a| a.name == WIRE && a.has_arg("default"))
        {
            let _ = self.emit(TypeError::WireDefaultAttr {
                field: field.name.clone(),
                span: serde_default.span,
            });
        }
        if let Some(default_ast) = &field.default {
            let resolved = self.resolve_expr(default_ast, field_ctx, Some(type_id));
            self.typecheck(resolved, type_id, default_ast.span());
        }
        type_id
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
            struct_field_types.push(scope.resolve_struct_field(field, &mut field_ctx));
        }

        let type_params: Vec<TirTypeParam> = struct_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| TirTypeParam {
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
            def: StructDef::Decl(
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

    /// The scope a `trait`'s methods resolve in: `Self` as slot 0, bounded by
    /// the trait itself, then the trait's own type parameters. Returned with
    /// the `Self` slot and the first slot a method's own parameters may take.
    fn enter_trait_scope(
        &mut self,
        trait_decl: &ast::TraitDecl,
    ) -> (TypeParamScope<'_, 'a, H>, TypeId, u32) {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();

        let self_slot = scope
            .tysys
            .type_table
            .borrow_mut()
            .make_type_param("Self".to_string(), 0);
        // Before the parameters, since each one's bounds pin the `Self` they
        // mean, and inside the declaration that is this trait.
        let declaring = SelfBinding {
            type_id: self_slot,
            declaring_trait: scope.tysys.resolutions.defs().of_ast_id(trait_decl.id),
        };
        scope.set_self_binding(declaring);
        scope.bind_param(
            "Self",
            BinderInScope::undeclared(0, self_slot),
            vec![ScopedBound::new(
                ast::TraitBound {
                    // The trait's own declaration node, which the resolution
                    // walk answers for; a fresh id nothing resolved would not.
                    id: trait_decl.id,
                    name: trait_decl.name.clone(),
                    type_args: Vec::new(),
                    assoc_types: Vec::new(),
                    span: trait_decl.span,
                    fn_signature: None,
                    resolved: None,
                },
                Some(declaring),
            )],
        );
        let next_slot = scope.register_generic_params(&trait_decl.type_params, 1);
        (scope, self_slot, next_slot)
    }

    /// [`Self::resolve_operation_param_defaults`] for a `trait`'s methods, whose
    /// scope binds `Self` as a slot rather than a concrete type.
    pub(super) fn resolve_trait_param_defaults(&mut self, trait_decl: &ast::TraitDecl) {
        if !declares_a_default(&trait_decl.methods) {
            return;
        }
        let (mut scope, _, next_slot) = self.enter_trait_scope(trait_decl);
        scope.sem.decls.clear_fn_local_items();
        for method in &trait_decl.methods {
            let mut method_scope = scope.enter_inherited_type_param_scope();
            method_scope.register_generic_params(&method.type_params, next_slot);
            // Both frames are in scope for a default, so both supply the type
            // argument a caller gets by default, in declaration-slot order.
            let type_params: Vec<ast::GenericParam> = trait_decl
                .type_params
                .iter()
                .chain(&method.type_params)
                .cloned()
                .collect();
            method_scope.check_param_defaults(method, &type_params);
        }
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
        let (mut scope, self_slot, next_slot) = self.enter_trait_scope(trait_decl);

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

        // The effect check reads a requirement's `with` off its sites; these
        // calls report and link them.
        if let ast::TraitHead::Fixed { effects, .. } = &trait_decl.head {
            scope.resolve_effects(effects);
        }
        let mut methods: hashmap::IndexMap<String, TraitMethod> = hashmap::IndexMap::default();
        for method in &trait_decl.methods {
            scope.reject_declaration_attrs_on_requirement(&trait_decl.name, method);
            let mut method_scope = scope.enter_inherited_type_param_scope();
            // The head's names were resolved above, once for every method.
            if !method.effects_inherited {
                method_scope.resolve_effects(&method.effects);
            }
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
            method_scope.reject_signature_annotations(&method.params, method.return_type.as_ref());

            let mut type_params = decl_slots.clone();
            type_params.extend(method_slots);

            let method_def = method_scope.def_at(method.id);
            methods.insert(
                method.name.clone(),
                TraitMethod {
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
                            .map(|p| sig::Param {
                                name: p.name.clone(),
                                is_mut: p.is_mut,
                                default: p.default.clone(),
                            })
                            .collect(),
                        declaring_slot_count: decl_slots.len() as u32,
                        // A declaration numbers its own slots densely from
                        // zero, so the count is also where the method's begin.
                        method_slot_base: decl_slots.len() as u32,
                        declaring_impl: None,
                        own_params: own_params_of(&method.type_params),
                        cm_name: method.attrs.iter().find_map(Attribute::cm_identifier),
                        is_async: method.is_async,
                        defaults_module: None,
                    },
                    default_body: method
                        .body
                        .as_ref()
                        .map(|_| std::rc::Rc::new(method.clone())),
                    is_reserved: method.unavailable_attr().is_some(),
                },
            );
        }

        let module = scope.current_module_source.clone();
        let trait_def = scope.def_at(trait_decl.id);
        scope
            .sem
            .decls
            .trait_sigs
            .insert(trait_def, TraitSig { module, methods });
    }

    /// The scope an `interface` / `resource` declaration's operations resolve
    /// in: its type parameters, and for a resource `Self` bound to the declaring
    /// resource. That `Self` type comes back too, because it is the receiver's.
    ///
    /// It is constructed after the type params are in scope, so a generic
    /// resource's `GenericResource` instance can reference its own `TypeParam`s
    /// (which gap-2 substitution then specialises per impl-block instantiation).
    /// A non-generic resource is a plain `Resource { def }`.
    fn enter_operation_scope(
        &mut self,
        type_params: &[ast::GenericParam],
        resource_self: Option<DefId>,
    ) -> (TypeParamScope<'_, 'a, H>, Option<TypeId>) {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.register_generic_params(type_params, 0);
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
                scope
                    .tysys
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::GenericResource {
                        def,
                        type_args: type_arg_ids,
                    })
            } else {
                scope.tysys.type_table.borrow_mut().make_resource(def)
            }
        });
        if let Some(type_id) = self_type {
            // A resource declaration, so `Self` is the resource and no trait
            // declares names off it.
            scope.set_self_binding(SelfBinding {
                type_id,
                declaring_trait: None,
            });
        }
        (scope, self_type)
    }

    /// Lower an effect or resource declaration's method list to [`TirEffectOp`]s.
    /// `resource_self` marks a resource decl, whose `&self` shorthand becomes a
    /// real parameter at index 0 to match `$cm_binding__<R>_<op>(self, args)`;
    /// an effect decl takes no receiver.
    pub(super) fn resolve_effect_ops(
        &mut self,
        type_params: &[ast::GenericParam],
        methods: &[ast::Function],
        resource_self: Option<DefId>,
    ) -> Vec<TirEffectOp> {
        let (mut scope, self_type) = self.enter_operation_scope(type_params, resource_self);

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
                    ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
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
                    sig_params.push(sig::Param {
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
            scope.reject_signature_annotations(&method.params, method.return_type.as_ref());
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
            // The bare `#[cm("...")]` payload, unsplit on `#`, recorded on the
            // signature so every call site reads the same one.
            let cm_name = method.attrs.iter().find_map(Attribute::cm_identifier);

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
                    // A declaration numbers its own slots densely from zero, so
                    // the count is also where the method's begin.
                    method_slot_base: decl_slots.len() as u32,
                    declaring_impl: None,
                    // An `interface` / `resource` operation declares no type
                    // parameters of its own.
                    own_params: Vec::new(),
                    cm_name: cm_name.clone(),
                    is_async: method.is_async,
                    defaults_module: None,
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
            let rejections: [(Option<Span>, &'static str); 5] = [
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
                    (!method.effects.is_empty()).then_some(method.span),
                    "cannot declare effects: an operation's effects are not required at its \
                     call sites, so a default implementation must be performable wherever it \
                     is dispatched — reach for an `#[ambient]` function",
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
            self.reject_declaration_attrs_on_requirement(owner, method);
        }
    }

    /// Reject the body-less declaration attributes on a method requirement, in
    /// a `trait` as in an `interface`. A requirement is dispatched to an impl,
    /// and that impl is what answers each of them.
    pub(super) fn reject_declaration_attrs_on_requirement(
        &mut self,
        owner: &str,
        method: &ast::Function,
    ) {
        for (attr, name, bodyless) in attribute::bodyless(&method.attrs) {
            let _ = self.emit(TypeError::AttributeOnRequirement {
                owner: owner.to_string(),
                operation: method.name.clone(),
                attribute: name,
                reason: bodyless.on_requirement,
                span: attr.span,
            });
        }
    }

    /// Check one method's parameter defaults against their parameter types, with
    /// the earlier parameters in scope and `type_params`' own defaults applied.
    fn check_param_defaults(&mut self, method: &ast::Function, type_params: &[ast::GenericParam]) {
        let mut ctx = FunctionContext::new(TypeTable::UNIT, method.name.clone());
        for param in &method.params {
            if param.self_kind != SelfKind::None {
                continue;
            }
            let type_id = self.resolve_type(&param.ty);
            if let Some(default_ast) = &param.default {
                self.check_param_default(default_ast, type_id, type_params, &mut ctx);
            }
            ctx.add_local_at(param.name.clone(), type_id, param.is_mut, None, param.span);
        }
    }

    /// Walk each operation's parameter defaults in the declaring scope, the way
    /// [`Self::resolve_function`] walks a free function's. A call site re-emits
    /// the default from the AST, and reads back what this walk records: the
    /// default's expression types, and the use→def edges that keep what it names
    /// alive.
    pub(super) fn resolve_operation_param_defaults(
        &mut self,
        type_params: &[ast::GenericParam],
        methods: &[ast::Function],
        resource_self: Option<DefId>,
    ) {
        if !declares_a_default(methods) {
            return;
        }
        let (mut scope, _) = self.enter_operation_scope(type_params, resource_self);
        // A default names no function-local item, and the table holds whatever
        // the last body walked left behind.
        scope.sem.decls.clear_fn_local_items();
        for method in methods {
            scope.check_param_defaults(method, type_params);
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

        let type_params: Vec<TirTypeParam> = variant_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| TirTypeParam {
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
            .any(GenericParam::is_real_type_param);
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
    pub(super) fn record_function_sig(&mut self, func: &Function) -> FunctionSig {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.annotate_ctx.trait_ctx.type_param_bounds.clear();
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
        scope.reject_signature_annotations(&func.params, func.return_type.as_ref());
        let effects = scope.resolve_effects(&func.effects);
        drop(scope);
        self.sem
            .decls
            .function_return_types
            .insert(func.name.clone(), return_type.unwrap_or(TypeTable::UNIT));
        FunctionSig {
            decl: DeclSig {
                type_params: real_type_params,
                param_types,
                return_type,
            },
            type_param_ids,
            params: func
                .params
                .iter()
                .map(|p| sig::Param {
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

        scope.reject_second_effect_param(&func.type_params);

        scope.register_generic_params(&func.type_params, 0);

        // Populate the generic-inference caches before the `function_return_types`
        // update below, which shares its map with non-generic callers and can be
        // overwritten by an external registration. A `<F: fn(…)>` bound is
        // eagerly realised to its function type and consumes no `TypeParam` slot,
        // being nothing monomorphisation needs to substitute.
        let has_real_type_params = func
            .type_params
            .iter()
            .any(GenericParam::is_real_type_param);
        let declared_return_type = if has_real_type_params {
            scope.populate_generic_function_cache(func)
        } else {
            func.return_type
                .as_ref()
                .map(|t| scope.resolve_type(t))
                .unwrap_or(TypeTable::UNIT)
        };

        let return_type = declared_return_type;

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
        let crosses_cm_boundary = func.is_export || func.is_cm_import();

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
                scope.check_param_default(default_ast, type_id, &func.type_params, &mut ctx);
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
            // `params` survives only to feed the recorded `fn_param_types`;
            // the TIR `default_expr` is not built.
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

        if let Some(b) = func.body.as_ref() {
            scope.resolve_block(b, &mut ctx, None);
        }

        scope.validate_missing_return_ast(return_type, func);
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
        let type_params: Vec<TirTypeParam> = func
            .type_params
            .iter()
            .filter_map(|p| {
                if !p.is_real_type_param() {
                    return None;
                }
                let idx = non_effect_non_fn_idx;
                non_effect_non_fn_idx += 1;
                Some(TirTypeParam {
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

        let effects = scope.resolve_effects(&func.effects);

        let func_key = func.id;
        scope.sem.types.function_effects.insert(func_key, effects);

        // Record what `task return` delivers, so reify can set
        // `task_return_type` for resource-store inference.
        if func.is_async {
            let task_key = func.id;
            scope
                .sem
                .types
                .function_task_returns
                .insert(task_key, declared_return_type);
        }

        drop(scope);

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
        let function_name = test_function_name(&meta, test_index, test_decl.name.as_deref());

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
        let Some(args) = impl_target_args(impl_ty) else {
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
        trait_name: Option<&FqTraitName>,
        trait_type: Option<&Type>,
        impl_is_concrete: bool,
        impl_declared_params: &[ast::GenericParam],
        recorded_sig: Option<&MethodSig>,
        impl_def: Option<DefId>,
    ) -> Option<TirFunction> {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.sem.decls.clear_fn_local_items();

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
            MethodNames {
                display: display_name.clone(),
                mangled: mangled_name.clone(),
            },
        );

        let mut ctx = FunctionContext::new(return_type, display_name);
        // `resume` is valid only in a handler method body (WEP 2026-04-11).
        if let Some(handled) = trait_name.and_then(FqTraitName::canonical)
            && let kind = scope.tysys.resolutions.defs().kind(handled)
            && kind.is_effect()
        {
            ctx.in_handler_method = true;
            let is_resource_effect = kind == DefKind::Resource;
            let async_op = scope
                .tysys
                .signatures
                .resource_method_sig(handled, &func.name)
                .filter(|op| op.is_async)
                .map(|op| op.cm_name.is_some());
            if let Some(cm_backed) = async_op
                && (is_resource_effect || !cm_backed)
            {
                let _ = scope.emit(TypeError::AsyncUserEffectHandlerUnsupported {
                    interface_name: scope.tysys.resolutions.defs().name(handled).to_string(),
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

        // Defaults are the trait's, value and type parameters alike (WEP
        // 2026-04-11), and the block restates the lists without one.
        // `recorded_sig` is `None` for a trait's own default-bodied method,
        // elaborated here once per implementing type: that one is the
        // declaration, so it may write both.
        if trait_name.is_some() && recorded_sig.is_some() {
            for type_param in &func.type_params {
                if let Some(default) = &type_param.default {
                    let _ = scope.emit(TypeError::TypeParamDefaultInTraitImpl {
                        method: func.name.clone(),
                        param: type_param.name.clone(),
                        span: default.span(),
                    });
                }
            }
            for param in &func.params {
                if let Some(default) = &param.default {
                    let _ = scope.emit(TypeError::DefaultInTraitImpl {
                        method: func.name.clone(),
                        param: param.name.clone(),
                        span: default.span(),
                    });
                }
            }
        }

        // Resolve parameters (including &self). Defaults are resolved in the
        // method's lexical scope with earlier parameters already bound.
        let mut params = Vec::new();
        for (param, &type_id) in func.params.iter().zip(param_types.iter()) {
            if param.self_kind == ast::SelfKind::Value {
                scope.check_self_by_value(type_id, param.span);
            }
            // Walked for its side-effect fact recording; the resolved TIR is
            // discarded (reify re-emits it from the AST).
            if let Some(default_ast) = &param.default {
                scope.check_param_default(default_ast, type_id, &func.type_params, &mut ctx);
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
            // `params` survives only to feed the recorded `fn_param_types`;
            // the TIR `default_expr` is not built.
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                is_mut_ref: false,
                span: param.span,
            });
        }

        if let Some(b) = func.body.as_ref() {
            scope.resolve_block(b, &mut ctx, None);
        }

        scope.validate_missing_return_ast(return_type, func);
        scope.validate_loop_jumps_ast(func.body.as_ref());

        // Convert AST type params to TIR type params (while type params still
        // in scope). Mirror the free-function path in `resolve_function`:
        // `<F: fn(...)>` bounds are realised eagerly and dropped from the
        // generic list; the remaining real type params use dense indices so
        // the substitution map in `substitute_type_params` lines up.
        let mut non_effect_non_fn_idx: u32 = 0;
        let type_params: Vec<TirTypeParam> = func
            .type_params
            .iter()
            .filter_map(|p| {
                if !p.is_real_type_param() {
                    return None;
                }
                let idx = non_effect_non_fn_idx;
                non_effect_non_fn_idx += 1;
                Some(TirTypeParam {
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

        let effects = scope.resolve_effects(&func.effects);

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

/// Whether any of `methods` gives a parameter a default.
fn declares_a_default(methods: &[ast::Function]) -> bool {
    methods
        .iter()
        .any(|m| m.params.iter().any(|p| p.default.is_some()))
}

/// Which declaration an operation belongs to. A resource's operations are
/// Component Model imports with a `&self` receiver; an effect's are neither.
#[derive(Clone, Copy)]
pub(super) enum OperationOwner {
    Interface,
    Resource,
}
