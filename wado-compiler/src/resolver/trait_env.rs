//! Global trait knowledge base: trait declarations, impl blocks, and blanket impls.
//!
//! `TraitEnv` is built once before resolution begins and is immutable thereafter.
//! It provides O(1) lookup of trait implementations by type name and trait name,
//! replacing linear scans across all modules.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::ast::{self, Item, Module, Type};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::name::ModuleSource;
use crate::tir::{TypeId, TypeTable};

use super::Resolver;
use super::types::TypeError;

/// Pre-built index: type name → list of (`ModuleSource`, item index) for trait impl blocks.
/// Built once from all loaded modules to avoid O(all items) scans per method call.
pub(super) type TraitImplIndex = IndexMap<String, Vec<(ModuleSource, usize)>>;

/// Pre-built index: trait name → (`ModuleSource`, item index) for trait declarations.
pub(super) type TraitDeclIndex = IndexMap<String, (ModuleSource, usize)>;

/// Pre-built list of blanket trait impls: `impl<T: Trait> OtherTrait for T`.
/// These are impl blocks where the impl type is a free type parameter with trait bounds.
/// Stored separately because they can't be indexed by concrete type name.
pub(super) type BlanketTraitImplIndex = Vec<(ModuleSource, usize)>;

/// Pre-built index of static methods (no `self` parameter) from impl blocks.
/// Key: `(type_name, method_name)` → `(ModuleSource, item_index, method_index)`.
/// Enables O(1) lookup of static methods instead of scanning all modules.
pub(super) type StaticMethodIndex = IndexMap<String, Vec<(String, ModuleSource, usize, usize)>>;

/// Pre-built index of static methods from resource declarations.
/// Key: `type_name` → `[(method_name, ModuleSource, item_index, method_index)]`.
pub(super) type ResourceStaticMethodIndex =
    IndexMap<String, Vec<(String, ModuleSource, usize, usize)>>;

/// Immutable global knowledge base for trait resolution.
///
/// Contains pre-built indices for fast lookup of trait implementations,
/// trait declarations, and blanket impls. Built once before resolution
/// begins and shared (via `Arc`) across all module resolvers.
pub(crate) struct TraitEnv {
    /// Type name → impl blocks that implement traits for that type.
    pub(super) impl_index: TraitImplIndex,
    /// Trait name → trait declaration location.
    pub(super) decl_index: TraitDeclIndex,
    /// Blanket impls (`impl<T: Bound> Trait for T`), checked as fallback.
    pub(super) blanket_impl_index: BlanketTraitImplIndex,
    /// `type_name` → `[(method_name, ModuleSource, item_idx, method_idx)]` for static methods.
    pub(super) static_method_index: StaticMethodIndex,
    /// `type_name` → `[(method_name, ModuleSource, item_idx, method_idx)]` for resource static methods.
    pub(super) resource_static_method_index: ResourceStaticMethodIndex,
}

impl TraitEnv {
    /// Build trait indices from all loaded modules.
    ///
    /// Called once in `resolve_all_modules` before per-module resolution begins.
    /// The indices enable O(1) trait lookup by type/trait name instead of scanning all modules.
    /// Also performs orphan rule checking for impl blocks in local (user) modules.
    pub(super) fn build(modules: &IndexMap<ModuleSource, Module>) -> (Arc<Self>, Vec<TypeError>) {
        let mut impl_index: TraitImplIndex = IndexMap::default();
        let mut decl_index: TraitDeclIndex = IndexMap::default();
        let mut blanket_impl_index: BlanketTraitImplIndex = Vec::new();
        // type name → module source, for orphan rule "is this type local?" checks
        let mut type_decl_index: IndexMap<String, ModuleSource> = IndexMap::default();

        let mut static_method_index: StaticMethodIndex = IndexMap::default();
        let mut resource_static_method_index: ResourceStaticMethodIndex = IndexMap::default();

        for (module_source, module) in modules {
            for (item_idx, item) in module.items.iter().enumerate() {
                match item {
                    Item::Impl(impl_block) if impl_block.trait_type.is_some() => {
                        let type_name = get_type_name_static(&impl_block.ty);
                        // Detect blanket impls: impl_ty is a type parameter from type_params
                        let is_blanket = impl_block
                            .type_params
                            .iter()
                            .any(|tp| tp.name == type_name && !tp.bounds.is_empty());
                        if is_blanket {
                            blanket_impl_index.push((module_source.clone(), item_idx));
                        }
                        impl_index
                            .entry(type_name)
                            .or_default()
                            .push((module_source.clone(), item_idx));
                    }
                    Item::Impl(impl_block) => {
                        // Non-trait impl block: index static methods
                        let type_name = get_type_name_static(&impl_block.ty);
                        for (method_idx, method) in impl_block.methods.iter().enumerate() {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if !has_self {
                                static_method_index
                                    .entry(type_name.clone())
                                    .or_default()
                                    .push((
                                        method.name.clone(),
                                        module_source.clone(),
                                        item_idx,
                                        method_idx,
                                    ));
                            }
                        }
                    }
                    Item::Trait(trait_decl) => {
                        decl_index
                            .entry(trait_decl.name.clone())
                            .or_insert((module_source.clone(), item_idx));
                    }
                    Item::Resource(resource) => {
                        // Index static methods from resource declarations
                        for (method_idx, method) in resource.methods.iter().enumerate() {
                            let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name)
                            });
                            if !has_self {
                                resource_static_method_index
                                    .entry(resource.name.clone())
                                    .or_default()
                                    .push((
                                        method.name.clone(),
                                        module_source.clone(),
                                        item_idx,
                                        method_idx,
                                    ));
                            }
                        }
                    }
                    Item::Struct(s) => {
                        type_decl_index
                            .entry(s.name.clone())
                            .or_insert_with(|| module_source.clone());
                    }
                    Item::Variant(v) => {
                        type_decl_index
                            .entry(v.name.clone())
                            .or_insert_with(|| module_source.clone());
                    }
                    Item::Enum(e) => {
                        type_decl_index
                            .entry(e.name.clone())
                            .or_insert_with(|| module_source.clone());
                    }
                    Item::Flags(f) => {
                        type_decl_index
                            .entry(f.name.clone())
                            .or_insert_with(|| module_source.clone());
                    }
                    Item::Newtype(n) => {
                        type_decl_index
                            .entry(n.name.clone())
                            .or_insert_with(|| module_source.clone());
                    }
                    Item::TupleTypeDecl(_) => {
                        type_decl_index
                            .entry(TypeTable::TUPLE_TYPE_NAME.to_string())
                            .or_insert_with(|| module_source.clone());
                    }
                    _ => {}
                }
            }
        }

        // Also index static methods from trait impl blocks (they have trait_type.is_some())
        for (module_source, module) in modules {
            for (item_idx, item) in module.items.iter().enumerate() {
                if let Item::Impl(impl_block) = item
                    && impl_block.trait_type.is_some()
                {
                    let type_name = get_type_name_static(&impl_block.ty);
                    for (method_idx, method) in impl_block.methods.iter().enumerate() {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if !has_self {
                            static_method_index
                                .entry(type_name.clone())
                                .or_default()
                                .push((
                                    method.name.clone(),
                                    module_source.clone(),
                                    item_idx,
                                    method_idx,
                                ));
                        }
                    }
                }
            }
        }

        let violations = check_all_orphan_rules(modules, &decl_index, &type_decl_index);

        (
            Arc::new(Self {
                impl_index,
                decl_index,
                blanket_impl_index,
                static_method_index,
                resource_static_method_index,
            }),
            violations,
        )
    }
}

/// Returns `true` if the module source is a user-local module (part of the current package).
fn is_user_local(ms: &ModuleSource) -> bool {
    matches!(
        ms,
        ModuleSource::Local { .. } | ModuleSource::EntryPoint { .. }
    )
}

/// Returns `true` if the named type is a local (user-defined) type.
/// Primitive types (i32, bool, char, etc.) are not in the index and return `false`.
fn is_local_type_name(name: &str, type_decl_index: &IndexMap<String, ModuleSource>) -> bool {
    type_decl_index.get(name).is_some_and(is_user_local)
}

/// Returns `true` if the named trait is a local (user-defined) trait.
fn is_local_trait_name(name: &str, decl_index: &TraitDeclIndex) -> bool {
    decl_index
        .get(name)
        .is_some_and(|(ms, _)| is_user_local(ms))
}

/// Describes the orphan-rule "classification" of a position in the impl sequence.
enum PositionKind {
    /// The outermost type constructor is a user-local type.
    LocalType,
    /// The position is a bare uncovered type parameter.
    UncoveredTypeParam,
    /// The outermost type constructor is a foreign (non-local) type.
    ForeignType,
}

/// Classify the outermost type constructor of an AST type relative to the orphan rule.
///
/// RFC 2451 sequence rule: walk `[self_type, trait_arg1, ...]` left-to-right.
/// - `LocalType` at position i, with no `UncoveredTypeParam` seen before i → **allowed**.
/// - `UncoveredTypeParam` before any `LocalType` → **forbidden**.
///
/// References (`&T`, `&mut T`) are *fundamental* and are looked through.
fn classify_position(
    ty: &Type,
    type_params: &[String],
    type_decl_index: &IndexMap<String, ModuleSource>,
) -> PositionKind {
    match ty {
        // Fundamental: look through references
        Type::Reference(inner) | Type::MutReference(inner) => {
            classify_position(inner, type_params, type_decl_index)
        }
        Type::Named(named) => {
            if type_params.contains(&named.name) {
                PositionKind::UncoveredTypeParam
            } else if is_local_type_name(&named.name, type_decl_index) {
                PositionKind::LocalType
            } else {
                PositionKind::ForeignType
            }
        }
        Type::Generic(generic) => {
            if type_params.contains(&generic.name) {
                // Generic<T> where generic itself is a type param: uncovered
                PositionKind::UncoveredTypeParam
            } else if is_local_type_name(&generic.name, type_decl_index) {
                // LocalType<...>: the head is local → this position is local
                PositionKind::LocalType
            } else {
                PositionKind::ForeignType
            }
        }
        // Tuples are local if the current crate owns them (via `pub type [..T];`)
        Type::Tuple(_) => {
            if type_decl_index.contains_key(TypeTable::TUPLE_TYPE_NAME) {
                PositionKind::LocalType
            } else {
                PositionKind::ForeignType
            }
        }
        Type::Function(_) | Type::NamespacedGeneric(_) | Type::TypePackSpread(..) => {
            PositionKind::ForeignType
        }
    }
}

/// Check the RFC 2451 orphan rule for a single impl block that has a foreign trait.
///
/// Sequence: `[self_type, trait_arg1, trait_arg2, ...]`.
/// Valid if there exists a position with `LocalType` and no `UncoveredTypeParam` before it.
fn check_orphan_rfc2451(
    impl_block: &ast::ImplBlock,
    type_decl_index: &IndexMap<String, ModuleSource>,
) -> bool {
    let type_params: Vec<String> = impl_block
        .type_params
        .iter()
        .map(|p| p.name.clone())
        .collect();

    // Build the sequence: self type first, then trait type arguments
    let trait_args: &[Type] = match impl_block.trait_type.as_ref() {
        Some(Type::Generic(g)) => &g.args,
        _ => &[],
    };

    let mut seen_uncovered_before_local = false;

    // Position 0: self type
    match classify_position(&impl_block.ty, &type_params, type_decl_index) {
        PositionKind::LocalType => return true,
        PositionKind::UncoveredTypeParam => seen_uncovered_before_local = true,
        PositionKind::ForeignType => {}
    }

    // Positions 1+: trait type arguments
    for trait_arg in trait_args {
        match classify_position(trait_arg, &type_params, type_decl_index) {
            PositionKind::LocalType => {
                if !seen_uncovered_before_local {
                    return true;
                }
                // Uncovered param was seen before this local type → still violated
                return false;
            }
            PositionKind::UncoveredTypeParam => {
                seen_uncovered_before_local = true;
            }
            PositionKind::ForeignType => {}
        }
    }

    false
}

/// Check orphan rules for all trait impl blocks across all modules.
/// Only impl blocks in local (user) modules are checked.
fn check_all_orphan_rules(
    modules: &IndexMap<ModuleSource, Module>,
    decl_index: &TraitDeclIndex,
    type_decl_index: &IndexMap<String, ModuleSource>,
) -> Vec<TypeError> {
    let mut violations = Vec::new();

    for (module_source, module) in modules {
        if !is_user_local(module_source) {
            continue;
        }

        for item in &module.items {
            let Item::Impl(impl_block) = item else {
                continue;
            };
            let Some(trait_type) = &impl_block.trait_type else {
                continue; // inherent impl, no orphan rule
            };

            let trait_name = get_type_name_static(trait_type);

            // If the trait is local, always allowed
            if is_local_trait_name(&trait_name, decl_index) {
                continue;
            }

            // Foreign trait: apply RFC 2451 sequence check
            if !check_orphan_rfc2451(impl_block, type_decl_index) {
                let self_type_name = get_type_name_static(&impl_block.ty);
                violations.push(TypeError::OrphanViolation {
                    trait_name,
                    self_type_name,
                    span: impl_block.span,
                });
            }
        }
    }

    violations
}

/// Mutable trait resolution context scoped to the current resolution site.
///
/// Groups all state that changes when entering/leaving generic scopes
/// (impl blocks, trait method lookups, etc). Use [`Resolver::enter_fresh_type_param_scope`]
/// or [`Resolver::enter_inherited_type_param_scope`] to mutate this safely with RAII
/// restore on drop.
#[derive(Clone, Default)]
pub(super) struct TraitContext {
    /// Type parameters currently in scope (name → (index, `TypeId`)).
    /// Set when resolving generic structs, functions, or impl blocks.
    pub(super) type_params: IndexMap<String, (u32, TypeId)>,
    /// Trait bounds on type parameters in scope (name → full bounds with assoc types).
    /// Used for resolving trait methods on type params (e.g., `T.cmp()` when T: Ord).
    pub(super) type_param_bounds: IndexMap<String, Vec<ast::TraitBound>>,
    /// Associated type bindings in scope (`Self::Name` → resolved type).
    /// Set when resolving trait implementations.
    pub(super) assoc_type_bindings: IndexMap<String, TypeId>,
    /// Current `Self` type in scope (the type being implemented in an impl block).
    pub(super) self_type: Option<TypeId>,
}

/// RAII guard that restores `Resolver::trait_ctx` to its saved value on drop.
///
/// Implements `Deref<Target = Resolver>` so it can be used as a transparent
/// resolver handle inside the scope. Restoration is panic-safe: even if the
/// scope body panics, drop still runs and the parent context is reinstated.
///
/// Use [`Resolver::enter_inherited_type_param_scope`] to enter a new scope.
/// It preserves the current `trait_ctx` so the child scope can register new
/// entries on top of the parent's. Callers that want a clean slate for a
/// specific field (matching the legacy `mem::take` pattern) should clear that
/// field on `scope.trait_ctx` after entering.
pub(super) struct TypeParamScope<'r, 'a, H: CompilerHost> {
    resolver: &'r mut Resolver<'a, H>,
    saved: TraitContext,
}

impl<'a, H: CompilerHost> Deref for TypeParamScope<'_, 'a, H> {
    type Target = Resolver<'a, H>;
    fn deref(&self) -> &Resolver<'a, H> {
        self.resolver
    }
}

impl<'a, H: CompilerHost> DerefMut for TypeParamScope<'_, 'a, H> {
    fn deref_mut(&mut self) -> &mut Resolver<'a, H> {
        self.resolver
    }
}

impl<H: CompilerHost> TypeParamScope<'_, '_, H> {
    /// Access the saved (parent) `TraitContext`. Useful when setting up an
    /// inner scope for an impl block whose impl type refers to one of the
    /// parent's type params (blanket impl / `&T` impl / variadic impl).
    pub(super) fn saved(&self) -> &TraitContext {
        &self.saved
    }
}

impl<H: CompilerHost> Drop for TypeParamScope<'_, '_, H> {
    fn drop(&mut self) {
        self.resolver.trait_ctx = std::mem::take(&mut self.saved);
    }
}

impl<'a, H: CompilerHost> Resolver<'a, H> {
    /// Enter an inherited type-param scope. The current `trait_ctx` is cloned
    /// into the saved slot, but left in place so the inner work can register
    /// additional type params on top of what the parent already had. The
    /// original context is restored when the returned guard is dropped.
    ///
    /// Callers that want a clean slate (matching the legacy
    /// `mem::take(&mut self.trait_ctx.type_params)` pattern) should clear the
    /// specific fields they want to reset on `scope.trait_ctx` after entering
    /// the scope — only the fields they touch need to be cleared, all others
    /// are inherited from the parent scope.
    pub(super) fn enter_inherited_type_param_scope(&mut self) -> TypeParamScope<'_, 'a, H> {
        let saved = self.trait_ctx.clone();
        TypeParamScope {
            resolver: self,
            saved,
        }
    }

    /// Register a list of generic parameters as `TypeParam` / `TypePack` ids
    /// in the current `trait_ctx`, starting from `offset`. Skips effect params.
    /// Returns the next free index (i.e. `offset + non_effect_count`).
    ///
    /// Trait bounds attached to each parameter are also recorded in
    /// `type_param_bounds` so trait-method lookups on the parameter work.
    pub(super) fn register_generic_params(
        &mut self,
        params: &[ast::GenericParam],
        offset: u32,
    ) -> u32 {
        let mut idx = offset;
        for tp in params.iter().filter(|p| !p.is_effect) {
            let type_id = if tp.is_pack {
                self.type_table
                    .borrow_mut()
                    .make_type_pack(tp.name.clone(), idx)
            } else {
                self.type_table
                    .borrow_mut()
                    .make_type_param(tp.name.clone(), idx)
            };
            self.trait_ctx
                .type_params
                .insert(tp.name.clone(), (idx, type_id));
            if !tp.bounds.is_empty() {
                self.trait_ctx
                    .type_param_bounds
                    .insert(tp.name.clone(), tp.bounds.clone());
            }
            idx += 1;
        }
        idx
    }
}

/// Extract a type name from an AST type without needing a Resolver instance.
fn get_type_name_static(ty: &ast::Type) -> String {
    match ty {
        ast::Type::Named(named) => named.name.clone(),
        ast::Type::Generic(generic) => generic.name.clone(),
        ast::Type::Reference(_) => "&".to_string(),
        ast::Type::MutReference(_) => "&mut".to_string(),
        ast::Type::Tuple(_) => TypeTable::TUPLE_TYPE_NAME.to_string(),
        _ => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{GenericParam, GenericType, ImplBlock, NamedType};
    use crate::token::Span;

    fn dummy_span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 1,
        }
    }

    fn named(name: &str) -> Type {
        Type::Named(NamedType {
            id: crate::ast::AstId(0),
            name: name.to_string(),
            span: dummy_span(),
        })
    }

    fn generic(name: &str, args: Vec<Type>) -> Type {
        Type::Generic(GenericType {
            id: crate::ast::AstId(0),
            name: name.to_string(),
            args,
            span: dummy_span(),
        })
    }

    fn ref_type(inner: Type) -> Type {
        Type::Reference(Box::new(inner))
    }

    fn mut_ref_type(inner: Type) -> Type {
        Type::MutReference(Box::new(inner))
    }

    fn type_param(name: &str) -> GenericParam {
        GenericParam {
            id: crate::ast::AstId(0),
            name: name.to_string(),
            name_span: dummy_span(),
            is_effect: false,
            is_pack: false,
            bounds: vec![],
            default: None,
            span: dummy_span(),
        }
    }

    fn make_type_decl_index(local_names: &[&str]) -> IndexMap<String, ModuleSource> {
        let mut m = IndexMap::default();
        for &name in local_names {
            m.insert(
                name.to_string(),
                ModuleSource::EntryPoint {
                    filename: "test.wado".to_string(),
                },
            );
        }
        m
    }

    fn impl_block(type_params: Vec<GenericParam>, trait_type: Type, self_type: Type) -> ImplBlock {
        ImplBlock {
            id: crate::ast::AstId(0),
            type_params,
            trait_type: Some(trait_type),
            ty: self_type,
            associated_types: vec![],
            constants: vec![],
            methods: vec![],
            is_synthesize_request: false,
            span: dummy_span(),
        }
    }

    // --- is_user_local ---

    #[test]
    fn test_is_user_local_entry_point() {
        assert!(is_user_local(&ModuleSource::EntryPoint {
            filename: "main.wado".to_string()
        }));
    }

    #[test]
    fn test_is_user_local_local_path() {
        assert!(is_user_local(&ModuleSource::Local {
            path: "./lib.wado".to_string()
        }));
    }

    #[test]
    fn test_is_user_local_core_is_foreign() {
        assert!(!is_user_local(&ModuleSource::Core {
            name: "prelude".to_string()
        }));
    }

    #[test]
    fn test_is_user_local_wasi_is_foreign() {
        assert!(!is_user_local(&ModuleSource::Wasi {
            interface: "cli".to_string()
        }));
    }

    #[test]
    fn test_is_user_local_remote_is_foreign() {
        assert!(!is_user_local(&ModuleSource::Remote {
            url: "https://example.com/lib.wado".to_string()
        }));
    }

    // --- classify_position ---

    #[test]
    fn test_classify_local_named_type() {
        let tdx = make_type_decl_index(&["MyError"]);
        assert!(matches!(
            classify_position(&named("MyError"), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_foreign_named_type() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&named("String"), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_primitive_is_foreign() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&named("i32"), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_uncovered_type_param() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&named("T"), &["T".to_string()], &tdx),
            PositionKind::UncoveredTypeParam
        ));
    }

    #[test]
    fn test_classify_local_generic_head_is_local() {
        // LocalType<T> — head is local regardless of args
        let tdx = make_type_decl_index(&["LocalType"]);
        let ty = generic("LocalType", vec![named("T")]);
        assert!(matches!(
            classify_position(&ty, &["T".to_string()], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_foreign_generic_is_foreign() {
        // Array<T> — head Array is foreign
        let tdx = make_type_decl_index(&[]);
        let ty = generic("Array", vec![named("T")]);
        assert!(matches!(
            classify_position(&ty, &["T".to_string()], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_reference_to_local_is_local() {
        // &LocalType — fundamental: look through &
        let tdx = make_type_decl_index(&["MyStruct"]);
        assert!(matches!(
            classify_position(&ref_type(named("MyStruct")), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_mut_reference_to_local_is_local() {
        // &mut LocalType — fundamental: look through &mut
        let tdx = make_type_decl_index(&["MyStruct"]);
        assert!(matches!(
            classify_position(&mut_ref_type(named("MyStruct")), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_reference_to_foreign_is_foreign() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&ref_type(named("String")), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_tuple_is_foreign() {
        // Tuple types have no single named head → foreign
        let tdx = make_type_decl_index(&["MyStruct"]);
        let ty = Type::Tuple(vec![named("MyStruct"), named("i32")]);
        assert!(matches!(
            classify_position(&ty, &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    // --- check_orphan_rfc2451 ---

    #[test]
    fn test_rfc2451_local_self_type_allowed() {
        // impl ForeignTrait for LocalType → T0 is local → allowed
        let tdx = make_type_decl_index(&["MyStruct"]);
        let ib = impl_block(vec![], named("ForeignTrait"), named("MyStruct"));
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_both_foreign_forbidden() {
        // impl Eq for String → both foreign
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(vec![], named("Eq"), named("String"));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_local_in_trait_arg_allowed() {
        // impl From<MyError> for String → T0=String(foreign), T1=MyError(local) → allowed
        let tdx = make_type_decl_index(&["MyError"]);
        let ib = impl_block(
            vec![],
            generic("From", vec![named("MyError")]),
            named("String"),
        );
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_uncovered_type_param_forbidden() {
        // impl<T> Eq for T → T0=T(uncovered) → forbidden
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(vec![type_param("T")], named("Eq"), named("T"));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_uncovered_param_before_local_in_trait_arg_forbidden() {
        // impl<T> From<T> for String → T0=String(foreign), T1=T(uncovered) → forbidden
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(
            vec![type_param("T")],
            generic("From", vec![named("T")]),
            named("String"),
        );
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_local_type_as_generic_head_in_trait_arg() {
        // impl<T> From<LocalType<T>> for ForeignType → T0=ForeignType, T1=LocalType<T>(local head) → allowed
        let tdx = make_type_decl_index(&["LocalType"]);
        let trait_ty = generic("From", vec![generic("LocalType", vec![named("T")])]);
        let ib = impl_block(vec![type_param("T")], trait_ty, named("ForeignType"));
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_foreign_generic_head_in_trait_arg_forbidden() {
        // impl<T> From<Array<T>> for ForeignType → T0=ForeignType, T1=Array<T>(foreign head) → forbidden
        let tdx = make_type_decl_index(&[]);
        let trait_ty = generic("From", vec![generic("Array", vec![named("T")])]);
        let ib = impl_block(vec![type_param("T")], trait_ty, named("ForeignType"));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_ref_to_local_as_self_type() {
        // impl ForeignTrait for &LocalType → fundamental, look through & → allowed
        let tdx = make_type_decl_index(&["MyStruct"]);
        let ib = impl_block(vec![], named("ForeignTrait"), ref_type(named("MyStruct")));
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_ref_to_foreign_as_self_type_forbidden() {
        // impl ForeignTrait for &String → &String is foreign → forbidden
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(vec![], named("ForeignTrait"), ref_type(named("String")));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_local_self_before_uncovered_param_in_trait_arg() {
        // impl<T> From<T> for LocalType → T0=LocalType(local!) → allowed before reaching T1=T
        let tdx = make_type_decl_index(&["LocalType"]);
        let ib = impl_block(
            vec![type_param("T")],
            generic("From", vec![named("T")]),
            named("LocalType"),
        );
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }
}
