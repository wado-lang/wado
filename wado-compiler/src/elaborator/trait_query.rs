//! Trait query functions: checking trait implementations, bounds validation,
//! and associated type resolution.

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, Item, Type};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::module_source::ModuleSource;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::callee::CalleeRef;
use super::scope::Scope;
use super::types::{MethodInfo, ResolvedTraitMethod, TraitMethodMatch, TypeError, TypeLookup};
use super::tysys::TypeSystem;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OnBoundTrait {
    Eq,
    Ord,
    Serialize,
    Deserialize,
    Default,
    Reflect,
    Ref,
    RefMut,
    Inspect,
    InspectAlt,
    DisplayAlt,
}

impl OnBoundTrait {
    pub(super) fn is_serde(self) -> bool {
        matches!(self, Self::Serialize | Self::Deserialize)
    }

    /// Traits total over every type: the bound always holds and the body is
    /// generated eagerly. `DisplayAlt` is included so its bound holds before its
    /// fallback is synthesized (generation is separately gated on a `Display`
    /// existing). `Display` is excluded — a `T: Display` bound is checked against
    /// a real impl.
    pub(super) fn is_format(self) -> bool {
        matches!(self, Self::Inspect | Self::InspectAlt | Self::DisplayAlt)
    }

    pub(super) fn is_field_recursive(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ord | Self::Serialize | Self::Deserialize
        )
    }
}

#[derive(Clone, Copy)]
enum StructuralMember<'a> {
    Field(&'a str),
    Case(&'a str),
}

impl StructuralMember<'_> {
    fn describe(self) -> String {
        match self {
            Self::Field(name) => format!("field `{name}`"),
            Self::Case(name) => format!("variant `{name}`"),
        }
    }
}

/// Canonical identity of an associated-constant key: canonicalize the
/// `Type` prefix of a use-site `Type::CONST` spelling via
/// [`canonical_decl_key_with`]. `None` for keys with no `::` (never a
/// constant key). Shared by annotate and reify so both resolve a constant
/// to the same identity.
pub(super) fn canonical_assoc_const_key(
    key: &str,
    current_module_source: &ModuleSource,
    imports: &super::sem::ModuleImports,
    symbols: &crate::symbol::SymbolTable,
    trait_env: &super::trait_env::TraitEnv,
) -> Option<(ModuleSource, String)> {
    let (prefix, _) = key.split_once("::")?;
    let (type_module, canon_name) =
        canonical_decl_key_with(prefix, current_module_source, imports, symbols, trait_env);
    let canon_key = if canon_name == prefix {
        key.to_string()
    } else {
        format!("{}{}", canon_name, &key[prefix.len()..])
    };
    Some((type_module, canon_key))
}

/// Free-function form of [`Elaborator::canonical_decl_key`], callable from
/// any module that has the inputs in hand. Reify uses this for trait
/// default-method synthesis (it has no `Elaborator` instance but does carry
/// the same `imports` / `symbols` / `trait_env` / `current_module_source`
/// references).
pub(crate) fn canonical_decl_key_with(
    name: &str,
    current_module_source: &ModuleSource,
    imports: &super::sem::ModuleImports,
    symbols: &crate::symbol::SymbolTable,
    trait_env: &super::trait_env::TraitEnv,
) -> (ModuleSource, String) {
    if super::is_primitive_type_name(name) {
        return (ModuleSource::primitive(), name.to_string());
    }
    if let Some(src) = imports.imported_type_sources.get(name) {
        let original = imports
            .import_original_names
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string());
        let canonical = symbols
            .lookup_in_module(src, &original)
            .map(|sym| sym.module_source().clone())
            .unwrap_or_else(|| src.clone());
        return (canonical, original);
    }
    if let Some(src) = imports.effect_sources.get(name) {
        let canonical = symbols
            .lookup_in_module(src, name)
            .map(|sym| sym.module_source().clone())
            .unwrap_or_else(|| src.clone());
        return (canonical, name.to_string());
    }
    // A name defined in the current module resolves to it, ahead of the global
    // decl-index fallbacks below (which are by-name and can pick a same-named
    // declaration in another module). Without this a locally defined
    // `trait Visitor` lost to `core:serde::Visitor` (issue #1298).
    if symbols.is_defined_in_module(current_module_source, name) {
        return (current_module_source.clone(), name.to_string());
    }
    if let Some(sym) = symbols.lookup(current_module_source, name) {
        return (sym.module_source().clone(), name.to_string());
    }
    if let Some(key) = trait_env.find_trait_decl_key(name) {
        return key;
    }
    if let Some(key) = trait_env.find_effect_or_resource_decl_key(name) {
        return key;
    }
    if let Some(key) = trait_env.find_static_method_decl_key(name) {
        return key;
    }
    (current_module_source.clone(), name.to_string())
}

/// Free-function form of
/// [`Elaborator::find_trait_decl_methods_with_module`], callable from any
/// module that has the inputs in hand. Used by reify's
/// `reify_impl_default_methods` to enumerate the trait's default methods
/// for an impl block.
/// Resolve a trait declaration by name to its `TraitDecl` and owning module,
/// local-first (a local trait shadows a same-named one elsewhere, issue #1298).
fn find_trait_decl_with<'a>(
    trait_name: &str,
    current_module_source: &ModuleSource,
    current_module_items: &'a [ast::Item],
    imports: &super::sem::ModuleImports,
    symbols: &crate::symbol::SymbolTable,
    trait_env: &super::trait_env::TraitEnv,
    loaded_modules: &'a IndexMap<ModuleSource, ast::Module>,
) -> Option<(&'a ast::TraitDecl, ModuleSource)> {
    let canonical_key = canonical_decl_key_with(
        trait_name,
        current_module_source,
        imports,
        symbols,
        trait_env,
    );
    if let Some((module_src, item_id)) = trait_env.decl_index.get(&canonical_key)
        && let Some(module) = loaded_modules.get(module_src)
        && let Some(Item::Trait(trait_decl)) = module.item_by_id(*item_id)
    {
        return Some((trait_decl, module_src.clone()));
    }
    for item in current_module_items {
        if let Item::Trait(trait_decl) = item
            && trait_decl.name == trait_name
        {
            return Some((trait_decl, current_module_source.clone()));
        }
    }
    None
}

pub(crate) fn find_trait_decl_methods_with_module_with(
    trait_name: &str,
    current_module_source: &ModuleSource,
    current_module_items: &[ast::Item],
    imports: &super::sem::ModuleImports,
    symbols: &crate::symbol::SymbolTable,
    trait_env: &super::trait_env::TraitEnv,
    loaded_modules: &IndexMap<ModuleSource, ast::Module>,
) -> Option<(Vec<ast::Function>, ModuleSource)> {
    find_trait_decl_with(
        trait_name,
        current_module_source,
        current_module_items,
        imports,
        symbols,
        trait_env,
        loaded_modules,
    )
    .map(|(decl, module)| (decl.methods.clone(), module))
}

/// A trait declaration's associated-type declarations, resolved by name.
pub(crate) fn find_trait_decl_assoc_types_with(
    trait_name: &str,
    current_module_source: &ModuleSource,
    current_module_items: &[ast::Item],
    imports: &super::sem::ModuleImports,
    symbols: &crate::symbol::SymbolTable,
    trait_env: &super::trait_env::TraitEnv,
    loaded_modules: &IndexMap<ModuleSource, ast::Module>,
) -> Option<Vec<ast::AssociatedTypeDecl>> {
    find_trait_decl_with(
        trait_name,
        current_module_source,
        current_module_items,
        imports,
        symbols,
        trait_env,
        loaded_modules,
    )
    .map(|(decl, _)| decl.associated_types.clone())
}

impl TypeSystem {
    /// Build the mapping from an impl block's declared type-parameter names to
    /// the concrete type arguments at a use site, by position. Pure over the
    /// AST impl type and the concrete arg list — needs no type table.
    pub(crate) fn build_type_param_mapping(
        impl_ty: &Type,
        concrete_type_args: &[TypeId],
        declared_type_params: &IndexSet<String>,
    ) -> IndexMap<String, TypeId> {
        let mut mapping = IndexMap::default();

        // Extract type parameter names from impl_ty, tracking positions
        // Position tracking is needed to map type params to the correct concrete arg
        if let Type::Generic(g) = impl_ty {
            for (concrete_idx, arg) in g.args.iter().enumerate() {
                if let Type::Named(n) = arg {
                    let is_type_param = if declared_type_params.is_empty() {
                        true // legacy: treat all Named as type params
                    } else {
                        declared_type_params.contains(&n.name)
                    };
                    if is_type_param && let Some(&type_id) = concrete_type_args.get(concrete_idx) {
                        mapping.insert(n.name.clone(), type_id);
                    }
                }
            }
        }

        mapping
    }

    /// The traits the compiler auto-derives for eligible aggregate types
    /// (`struct` / `variant` / `enum` / generic instance) and exposes through
    /// method-call and operator dispatch, each paired with the method it
    /// declares. The single source for the auto-derive method ↔ trait mapping:
    /// the dispatch sites read it instead of hardcoding the `"eq"` / `"cmp"`
    /// strings and the per-trait return type. Adding a new auto-derived trait
    /// is one entry here (plus its synthesis in `synthesis::traits`).
    const AUTO_DERIVED_METHODS: &'static [(CompilerItem, &'static str)] =
        &[(CompilerItem::Eq, "eq"), (CompilerItem::Ord, "cmp")];

    /// The return type an auto-derived trait fixes, regardless of what any user
    /// impl writes (`Eq` → `bool`, `Ord` → `Ordering`).
    fn auto_derive_return_type(&self, item: CompilerItem) -> TypeId {
        match item {
            CompilerItem::Eq => TypeTable::BOOL,
            _ => self
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::Ordering),
        }
    }

    /// Resolve the auto-derived trait that declares `method_name`, returning its
    /// trait name and fixed return type, or `None` when no auto-derived trait
    /// declares that method.
    pub(super) fn auto_derive_by_method(&self, method_name: &str) -> Option<(String, TypeId)> {
        let item = Self::AUTO_DERIVED_METHODS
            .iter()
            .find(|(_, m)| *m == method_name)
            .map(|(it, _)| *it)?;
        let trait_name = self
            .type_table
            .borrow()
            .compiler_trait_name(item)
            .to_string();
        Some((trait_name, self.auto_derive_return_type(item)))
    }

    /// Mirror of [`Self::auto_derive_by_method`] keyed by trait name, for
    /// operator dispatch which already knows the trait. Returns the fixed
    /// return type, or `None` when `trait_name` is not an auto-derived trait.
    pub(super) fn auto_derive_by_trait(&self, trait_name: &str) -> Option<TypeId> {
        let item = Self::AUTO_DERIVED_METHODS.iter().find_map(|(item, _)| {
            let name = self
                .type_table
                .borrow()
                .compiler_trait_name(*item)
                .to_string();
            (name == trait_name).then_some(*item)
        })?;
        Some(self.auto_derive_return_type(item))
    }

    /// Check that concrete type args at non-type-parameter positions match the impl type.
    /// e.g., `impl KeyValueLiteral for TreeMap<String, V>` with `TreeMap<i32, String>` should fail
    /// because position 0 expects String but got i32.
    pub(crate) fn verify_impl_type_compatibility(
        &self,
        impl_ty: &Type,
        concrete_type_args: &[TypeId],
        declared_type_params: &IndexSet<String>,
    ) -> bool {
        if declared_type_params.is_empty() {
            return true; // No filtering available, assume compatible
        }
        let Type::Generic(g) = impl_ty else {
            return true;
        };
        let tt = self.type_table.borrow();
        for (i, arg) in g.args.iter().enumerate() {
            let Some(&concrete_id) = concrete_type_args.get(i) else {
                continue;
            };
            if !Self::impl_type_matches_concrete(arg, concrete_id, declared_type_params, &tt) {
                return false;
            }
        }
        true
    }

    /// Recursively check whether an impl type argument matches a concrete type ID.
    /// - `Type::Named` that is a declared type param → always matches (free type param)
    /// - `Type::Named` not in type params → concrete name must equal `type_table.type_name()`
    /// - `Type::Generic` → concrete must be a `GenericInstance` with same outer name; inner args checked recursively
    /// - Other types → not validated (return true)
    pub(crate) fn impl_type_matches_concrete(
        impl_ty: &Type,
        concrete_id: TypeId,
        declared_type_params: &IndexSet<String>,
        type_table: &TypeTable,
    ) -> bool {
        match impl_ty {
            Type::Named(n) => {
                if declared_type_params.contains(&n.name) {
                    true // free type param — matches anything
                } else {
                    type_table.type_name(concrete_id) == n.name
                }
            }
            Type::Generic(g) => {
                let resolved = type_table.get(concrete_id).clone();
                match resolved {
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        if name != g.name {
                            return false;
                        }
                        for (i, inner) in g.args.iter().enumerate() {
                            let Some(&inner_id) = type_args.get(i) else {
                                return false;
                            };
                            if !Self::impl_type_matches_concrete(
                                inner,
                                inner_id,
                                declared_type_params,
                                type_table,
                            ) {
                                return false;
                            }
                        }
                        true
                    }
                    _ => false,
                }
            }
            _ => true,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Find a trait declaration by name across all modules.
    /// Returns the trait's methods (cloned) if found.
    pub(super) fn find_trait_decl_methods(&self, trait_name: &str) -> Option<Vec<ast::Function>> {
        self.find_trait_decl_methods_with_module(trait_name)
            .map(|(methods, _)| methods)
    }

    /// Like [`Self::find_trait_decl_methods`] but also returns the module that
    /// owns the trait declaration, for callers that need to attribute a trait
    /// *default* method body to its declaring module (e.g. diagnostics).
    /// Fact keying needs no module qualifier: the body's nodes carry their
    /// own globally-unique `AstId`s.
    pub(super) fn find_trait_decl_methods_with_module(
        &self,
        trait_name: &str,
    ) -> Option<(Vec<ast::Function>, ModuleSource)> {
        find_trait_decl_methods_with_module_with(
            trait_name,
            &self.current_module_source,
            self.current_module_items,
            &self.sem.imports,
            self.symbols,
            &self.tysys.trait_env,
            self.loaded_modules,
        )
    }

    /// A trait declaration's associated-type declarations, with their bounds.
    pub(super) fn find_trait_decl_assoc_type_decls(
        &self,
        trait_name: &str,
    ) -> Option<Vec<ast::AssociatedTypeDecl>> {
        find_trait_decl_assoc_types_with(
            trait_name,
            &self.current_module_source,
            self.current_module_items,
            &self.sem.imports,
            self.symbols,
            &self.tysys.trait_env,
            self.loaded_modules,
        )
    }

    /// Enforce a trait's associated-type bounds (`type X: Bound`) against an
    /// impl's bindings (`type X = Concrete`), skipping still-parametric
    /// bindings. Only the bound's trait is checked, not its associated-type
    /// equality constraints (`Iterator<Item = Self::Item>`).
    pub(super) fn enforce_impl_assoc_type_bounds(&mut self, impl_block: &ast::ImplBlock) {
        let Some(trait_type) = &impl_block.trait_type else {
            return;
        };
        let trait_name = self.get_type_name(trait_type);
        let Some(decls) = self.find_trait_decl_assoc_type_decls(&trait_name) else {
            return;
        };
        for binding in &impl_block.associated_types {
            let Some(decl) = decls.iter().find(|d| d.name == binding.name) else {
                continue;
            };
            if decl.bounds.is_empty() {
                continue;
            }
            let type_id = self
                .annotate_ctx
                .trait_ctx
                .assoc_type_bindings
                .get(&binding.name)
                .copied()
                .unwrap_or_else(|| self.resolve_type(&binding.ty));
            if self.tysys.type_table.borrow().contains_type_param(type_id) {
                continue;
            }
            for bound in &decl.bounds {
                if bound.fn_signature.is_some() {
                    continue;
                }
                if !self.tysys.type_implements_trait(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    type_id,
                    &bound.name,
                ) {
                    let type_name = self.tysys.type_id_to_string(type_id);
                    let reason = self.tysys.trait_unimpl_reason_chain(
                        &self.annotate_ctx,
                        &self.type_lookup(),
                        type_id,
                        &bound.name,
                    );
                    let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                        type_name,
                        trait_name: bound.name.clone(),
                        param_name: binding.name.clone(),
                        reason,
                        span: binding.span,
                    });
                }
            }
        }
    }

    /// Find a trait declaration's type parameters (e.g., `<T, U>` in `trait Foo<T, U>`).
    pub(super) fn find_trait_decl_type_params(
        &self,
        trait_name: &str,
    ) -> Option<Vec<ast::GenericParam>> {
        // `canonical_decl_key` is local-first (issue #1298), so the type-param
        // list and the default-method bodies resolve to the same trait.
        let canonical_key = self.canonical_decl_key(trait_name);
        if let Some(loc) = self.tysys.trait_env.decl_index.get(&canonical_key)
            && let Some(header) = self.tysys.trait_env.trait_decl_headers.get(loc)
        {
            return Some(header.type_params.clone());
        }
        // Fallback: a trait declared in the current module whose canonical key
        // missed the decl index. (`trait_decl_headers` covers every loaded
        // module, so the current module is included.)
        for (key, header) in &self.tysys.trait_env.trait_decl_headers {
            if key.0 == self.current_module_source && header.name == trait_name {
                return Some(header.type_params.clone());
            }
        }
        None
    }
}

impl TypeSystem {
    /// Check if a type implements a specific trait (for trait bound checking)
    pub(super) fn type_implements_trait(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_name: &str,
    ) -> bool {
        let resolved = self.type_table.borrow().get(type_id).clone();

        // Recursion guard: if we're already checking this (type, trait) pair,
        // optimistically return true to break infinite recursion on recursive types.
        // This is sound because auto-derived Eq/Ord on recursive types is well-founded
        // (Wasm GC types are heap-allocated, so the comparison terminates on structural equality).
        // If any non-recursive field fails the trait check, it will be caught on that path.
        {
            let key = (type_id, trait_name.to_string());
            let stack = ctx.trait_check_stack.borrow();
            if stack.contains(&key) {
                return true;
            }
        }
        ctx.trait_check_stack
            .borrow_mut()
            .push((type_id, trait_name.to_string()));

        let result = self.type_implements_trait_inner(ctx, scope, &resolved, trait_name);

        ctx.trait_check_stack.borrow_mut().pop();

        result
    }

    /// Explain *why* `type_id` does not implement `trait_name` by walking the
    /// auto-derive / `on_bound` structure. Each returned entry is one step of a
    /// reason chain, deepest cause last; an empty result means no structural
    /// explanation is available (the type is itself a leaf — e.g. a function
    /// type — whose non-conformance the headline message already states).
    ///
    /// Only the `Eq` / `Ord` (`automatic` policy) and `Serialize` /
    /// `Deserialize` (`on_bound` policy) structural-conformance rules are
    /// explained, since those are the ones a diagnostic can usefully unfold.
    pub(super) fn trait_unimpl_reason_chain(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_name: &str,
    ) -> Vec<String> {
        let mut chain = Vec::new();
        self.collect_trait_unimpl_reason(ctx, scope, type_id, trait_name, &mut chain);
        chain
    }

    fn collect_trait_unimpl_reason(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_name: &str,
        chain: &mut Vec<String>,
    ) {
        // Bound the depth so a pathologically nested (or cyclic) type cannot
        // produce an unbounded chain.
        if chain.len() >= 8 {
            return;
        }
        let Some(tr) = self.classify_on_bound_trait(scope, trait_name) else {
            return;
        };
        if !tr.is_field_recursive() {
            return;
        }
        let resolved = self.type_table.borrow().get(type_id).clone();

        let mut failing: Option<(String, TypeId)> = None;
        self.walk_structural_derive_members(scope, &resolved, tr, &mut |member, member_tid| {
            if self.type_implements_trait(ctx, scope, member_tid, trait_name) {
                true
            } else {
                failing = Some((member.describe(), member_tid));
                false
            }
        });
        if let Some((label, member_tid)) = failing {
            let owner = self.type_id_to_string(type_id);
            let member_ty = self.type_id_to_string(member_tid);
            chain.push(format!(
                "`{owner}` does not implement `{trait_name}` because {label} of type `{member_ty}` does not implement `{trait_name}`"
            ));
            self.collect_trait_unimpl_reason(ctx, scope, member_tid, trait_name, chain);
        }
    }

    pub(super) fn classify_on_bound_trait(
        &self,
        scope: &TypeLookup,
        trait_name: &str,
    ) -> Option<OnBoundTrait> {
        let (on_bound, compiler_module) = {
            let tt = self.type_table.borrow();
            let items = tt.compiler_items();
            let of = |item: CompilerItem, on_bound: OnBoundTrait| {
                items.trait_module(item).map(|m| (on_bound, m.clone()))
            };
            if trait_name == items.trait_name(CompilerItem::Eq) {
                of(CompilerItem::Eq, OnBoundTrait::Eq)
            } else if trait_name == items.trait_name(CompilerItem::Ord) {
                of(CompilerItem::Ord, OnBoundTrait::Ord)
            } else if items.trait_name_opt(CompilerItem::Serialize) == Some(trait_name) {
                of(CompilerItem::Serialize, OnBoundTrait::Serialize)
            } else if items.trait_name_opt(CompilerItem::Deserialize) == Some(trait_name) {
                of(CompilerItem::Deserialize, OnBoundTrait::Deserialize)
            } else if trait_name == items.trait_name(CompilerItem::Default) {
                of(CompilerItem::Default, OnBoundTrait::Default)
            } else if trait_name == items.trait_name(CompilerItem::Reflect) {
                of(CompilerItem::Reflect, OnBoundTrait::Reflect)
            } else if trait_name == items.trait_name(CompilerItem::Ref) {
                of(CompilerItem::Ref, OnBoundTrait::Ref)
            } else if trait_name == items.trait_name(CompilerItem::RefMut) {
                of(CompilerItem::RefMut, OnBoundTrait::RefMut)
            } else if trait_name == items.trait_name(CompilerItem::Inspect) {
                of(CompilerItem::Inspect, OnBoundTrait::Inspect)
            } else if trait_name == items.trait_name(CompilerItem::InspectAlt) {
                of(CompilerItem::InspectAlt, OnBoundTrait::InspectAlt)
            } else if trait_name == items.trait_name(CompilerItem::DisplayAlt) {
                of(CompilerItem::DisplayAlt, OnBoundTrait::DisplayAlt)
            } else {
                None
            }
        }?;
        match self.scoped_trait_decl_module(scope, trait_name) {
            Some(module) => (*module == compiler_module).then_some(on_bound),
            None => Some(on_bound),
        }
    }

    /// `true` when `trait_name` resolves to the compiler's prelude `Display`
    /// trait in this scope (not a same-name user trait). `Display` is not an
    /// [`OnBoundTrait`] — it is never auto-derived except for plain enums — so
    /// its identity is checked here rather than through `classify_on_bound_trait`.
    pub(super) fn is_display_trait(&self, scope: &TypeLookup, trait_name: &str) -> bool {
        let compiler_module = {
            let tt = self.type_table.borrow();
            let items = tt.compiler_items();
            if trait_name != items.trait_name(CompilerItem::Display) {
                return false;
            }
            let Some(module) = items.trait_module(CompilerItem::Display) else {
                return false;
            };
            module.clone()
        };
        match self.scoped_trait_decl_module(scope, trait_name) {
            Some(module) => *module == compiler_module,
            None => true,
        }
    }

    /// The trait declaration `trait_name` binds to in scope (local, else an
    /// explicit import); `None` when it falls through to the ambient compiler
    /// trait. Lets a same-name user `trait` be distinguished from the compiler's.
    fn scoped_trait_decl_module<'a>(
        &self,
        scope: &TypeLookup<'a>,
        trait_name: &str,
    ) -> Option<&'a ModuleSource> {
        let trait_env = &self.trait_env;
        let declares = |module: &ModuleSource| {
            trait_env
                .decl_index
                .contains_key(&(module.clone(), trait_name.to_string()))
        };
        if declares(scope.current_module_source) {
            return Some(scope.current_module_source);
        }
        scope
            .imported_type_sources
            .get(trait_name)
            .filter(|src| declares(src))
    }

    fn walk_structural_derive_members(
        &self,
        scope: &TypeLookup,
        resolved: &ResolvedType,
        tr: OnBoundTrait,
        visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool,
    ) -> Option<bool> {
        let subst = |param_ids: &[TypeId], type_args: &[TypeId], tid: TypeId| -> TypeId {
            param_ids
                .iter()
                .position(|param| *param == tid)
                .and_then(|i| type_args.get(i).copied())
                .unwrap_or(tid)
        };
        let walk_struct = |info: &super::types::StructFieldInfo,
                           type_args: &[TypeId],
                           visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool|
         -> bool {
            info.fields.iter().all(|(fname, tid, _)| {
                let concrete = subst(&info.type_param_type_ids, type_args, *tid);
                visit(StructuralMember::Field(fname), concrete)
            })
        };
        let walk_variant = |info: &super::types::VariantInfo,
                            type_args: &[TypeId],
                            visit: &mut dyn FnMut(StructuralMember<'_>, TypeId) -> bool|
         -> bool {
            info.cases
                .iter()
                .filter(|c| c.payload != TypeTable::UNIT)
                .all(|c| {
                    let concrete = subst(&info.type_param_type_ids, type_args, c.payload);
                    visit(StructuralMember::Case(&c.name), concrete)
                })
        };
        match resolved {
            ResolvedType::Enum { .. } => Some(true),
            ResolvedType::Flags { .. } => {
                if tr.is_serde() {
                    Some(true)
                } else {
                    None
                }
            }
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                let info = scope.struct_fields_in(name, module_source)?;
                Some(walk_struct(info, &[], visit))
            }
            ResolvedType::Variant {
                name,
                module_source,
            } => {
                if tr == OnBoundTrait::Ord {
                    return None;
                }
                let info = scope.variant_case_in(name, module_source)?;
                Some(walk_variant(info, &[], visit))
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                if let Some(info) = scope.struct_fields_in(name, module_source) {
                    Some(walk_struct(info, type_args, visit))
                } else if tr != OnBoundTrait::Ord
                    && let Some(info) = scope.variant_case_in(name, module_source)
                {
                    Some(walk_variant(info, type_args, visit))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub(super) fn structurally_derivable_for_explicit_request(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        trait_name: &str,
    ) -> bool {
        let Some(tr) = self.classify_on_bound_trait(scope, trait_name) else {
            return false;
        };
        if tr.is_format() {
            return true;
        }
        if tr == OnBoundTrait::Default {
            return self.is_defaultable_struct(scope, type_id);
        }
        let resolved = self.type_table.borrow().get(type_id).clone();
        match &resolved {
            ResolvedType::Newtype { base_type, .. } => {
                self.type_implements_trait(ctx, scope, *base_type, trait_name)
            }
            ResolvedType::Flags { .. } => {
                self.type_implements_trait(ctx, scope, TypeTable::U32, trait_name)
            }
            nominal => {
                self.walk_structural_derive_members(scope, nominal, tr, &mut |_, member| {
                    matches!(
                        self.type_table.borrow().get(member),
                        ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
                    ) || self.type_implements_trait(ctx, scope, member, trait_name)
                }) == Some(true)
            }
        }
    }

    fn is_defaultable_struct(&self, scope: &TypeLookup, type_id: TypeId) -> bool {
        let name = {
            let tt = self.type_table.borrow();
            if !matches!(tt.get(type_id), ResolvedType::Struct { .. }) {
                return false;
            }
            tt.type_name(type_id)
        };
        self.auto_derive_default_struct_type(scope, &name).is_some()
    }

    /// The `Ref` marker's eligibility: whether a value of this type is a Wasm GC
    /// reference (WEP 2026-01-20). A `Newtype` follows its base type.
    pub(super) fn is_ref_identity(&self, resolved: &ResolvedType) -> bool {
        match resolved {
            ResolvedType::Primitive(p) => {
                matches!(p, PrimitiveType::I128 | PrimitiveType::U128)
            }
            ResolvedType::Struct { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::Function { .. }
            | ResolvedType::Ref(_)
            | ResolvedType::MutRef(_) => true,
            ResolvedType::Newtype { base_type, .. } => {
                let base = self.type_table.borrow().get(*base_type).clone();
                self.is_ref_identity(&base)
            }
            ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Reactive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Unknown
            | ResolvedType::Error
            | ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. } => false,
        }
    }

    /// The `RefMut` marker's eligibility: a `Ref` type whose value is mutated in
    /// place rather than replaced on assign (WEP 2026-01-20). `variant` and `fn`
    /// are `Ref` but boxed (replace-on-assign), so a `&mut` cannot write through
    /// them; every other `Ref` type qualifies. A `Newtype` follows its base.
    pub(super) fn is_ref_mut_identity(&self, scope: &TypeLookup, resolved: &ResolvedType) -> bool {
        match resolved {
            ResolvedType::Variant { .. } | ResolvedType::Function { .. } => false,
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => {
                if scope.variant_case_in(name, module_source).is_some() {
                    false
                } else {
                    self.is_ref_identity(resolved)
                }
            }
            ResolvedType::Newtype { base_type, .. } => {
                let base = self.type_table.borrow().get(*base_type).clone();
                self.is_ref_mut_identity(scope, &base)
            }
            _ => self.is_ref_identity(resolved),
        }
    }

    fn type_implements_trait_inner(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        resolved: &ResolvedType,
        trait_name: &str,
    ) -> bool {
        let on_bound = self.classify_on_bound_trait(scope, trait_name);

        if on_bound.is_some_and(OnBoundTrait::is_format) {
            return true;
        }

        // A plain `enum` auto-derives `Display` (the bare case name), so its
        // bound holds before `synthesize_traits` emits the body.
        if matches!(resolved, ResolvedType::Enum { .. }) && self.is_display_trait(scope, trait_name)
        {
            return true;
        }

        // A type parameter satisfies exactly the bounds declared on it. Concrete
        // types no longer reach here as parameters — a known non-declared type arg
        // (`String` in `impl for TreeMap<String, V>`) resolves as itself.
        if let ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } = resolved
        {
            return ctx
                .trait_ctx
                .type_param_bounds
                .get(name)
                .is_some_and(|bounds| bounds.iter().any(|b| b.name == trait_name));
        }

        if on_bound == Some(OnBoundTrait::Ref) {
            return self.is_ref_identity(resolved);
        }

        if on_bound == Some(OnBoundTrait::RefMut) {
            return self.is_ref_mut_identity(scope, resolved);
        }

        let is_eq = on_bound == Some(OnBoundTrait::Eq);
        let is_eq_or_ord = is_eq || on_bound == Some(OnBoundTrait::Ord);

        // Primitives have built-in implementations for certain traits
        if let ResolvedType::Primitive(prim) = &resolved {
            if is_eq_or_ord {
                return true;
            }
            // Numeric primitives implement arithmetic traits
            if matches!(trait_name, "Add" | "Sub" | "Mul" | "Div" | "Rem")
                && !matches!(prim, PrimitiveType::Bool | PrimitiveType::Char)
            {
                return true;
            }
            // For other traits, check the type name
            let type_name = format!("{prim:?}").to_lowercase();
            return self.find_trait_impl_for_type(ctx, scope, &type_name, trait_name);
        }

        if let Some(tr) = on_bound
            && tr.is_field_recursive()
            && let ResolvedType::Enum {
                name,
                module_source,
            }
            | ResolvedType::Struct {
                name,
                module_source,
                ..
            }
            | ResolvedType::Variant {
                name,
                module_source,
            }
            | ResolvedType::Flags {
                name,
                module_source,
            }
            | ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } = resolved
        {
            let serde_blocked =
                tr.is_serde() && self.has_real_trait_impl_for_type(ctx, scope, name, trait_name);
            if !serde_blocked
                && self.walk_structural_derive_members(scope, resolved, tr, &mut |_, member| {
                    self.type_implements_trait(ctx, scope, member, trait_name)
                }) == Some(true)
            {
                self.type_table
                    .borrow_mut()
                    .record_bound_driven_synth_request(name, module_source, trait_name);
                return true;
            }
        }

        if let ResolvedType::Struct {
            name,
            module_source,
            ..
        } = &resolved
            && on_bound == Some(OnBoundTrait::Default)
            && self.auto_derive_default_struct_type(scope, name).is_some()
        {
            self.type_table
                .borrow_mut()
                .record_bound_driven_synth_request(name, module_source, trait_name);
            return true;
        }

        // `Reflect` is synthesized for every struct: eligibility is "is a
        // struct", not a field-recursive check.
        if let ResolvedType::Struct {
            name,
            module_source,
            ..
        } = &resolved
            && on_bound == Some(OnBoundTrait::Reflect)
        {
            self.type_table
                .borrow_mut()
                .record_bound_driven_synth_request(name, module_source, trait_name);
            return true;
        }

        // Get the type name and type args for looking up implementations
        let (type_name, type_args) = match &resolved {
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. } => (name.clone(), None),
            // The raw GC array `Array<T>` carries its element as a single type
            // arg, so trait impls (`impl IntoIterator for Array<T>`) resolve
            // under the canonical name "Array".
            ResolvedType::BuiltinArray(elem) => {
                (TypeTable::ARRAY_TYPE_NAME.to_string(), Some(vec![*elem]))
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                if TypeTable::is_tuple_type(name) {
                    // Tuples implement a trait when all elements implement it
                    let elems = type_args.clone();
                    return elems
                        .iter()
                        .all(|e| self.type_implements_trait(ctx, scope, *e, trait_name));
                }
                (
                    name.clone(),
                    if type_args.is_empty() {
                        None
                    } else {
                        Some(type_args.clone())
                    },
                )
            }
            ResolvedType::Ref(inner) => {
                // References always implement Eq via ref.eq (identity comparison)
                if is_eq {
                    return true;
                }
                // Check for a specific impl Trait for &T first (e.g., impl Inspect for &T)
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args(
                    ctx,
                    scope,
                    "&",
                    trait_name,
                    Some(&[inner_id]),
                ) {
                    return true;
                }
                return self.type_implements_trait(ctx, scope, inner_id, trait_name);
            }
            ResolvedType::MutRef(inner) => {
                // Mutable references always implement Eq via ref.eq (identity comparison)
                if is_eq {
                    return true;
                }
                let inner_id = *inner;
                if self.find_trait_impl_for_type_with_args(
                    ctx,
                    scope,
                    "&mut",
                    trait_name,
                    Some(&[inner_id]),
                ) {
                    return true;
                }
                return self.type_implements_trait(ctx, scope, inner_id, trait_name);
            }
            ResolvedType::AssocTypeProjection { bounds, .. } => {
                // An associated type projection T::Assoc implements a trait if
                // the trait declaration for Assoc declares that bound.
                // e.g., I::Iter: Iterator when IntoIterator::Iter: Iterator
                return bounds.iter().any(|b| b == trait_name);
            }
            ResolvedType::Newtype {
                name, base_type, ..
            } => {
                // Check for a direct impl on the newtype first (e.g., impl Describe for Meters)
                if self.find_trait_impl_for_type(ctx, scope, name, trait_name) {
                    return true;
                }
                // Fall back to base type's trait implementation
                let base_id = *base_type;
                return self.type_implements_trait(ctx, scope, base_id, trait_name);
            }
            ResolvedType::Flags { name, .. } => {
                if self.find_trait_impl_for_type(ctx, scope, name, trait_name) {
                    return true;
                }
                return self.type_implements_trait(ctx, scope, TypeTable::U32, trait_name);
            }
            _ => return false,
        };

        self.find_trait_impl_for_type_with_args(
            ctx,
            scope,
            &type_name,
            trait_name,
            type_args.as_deref(),
        )
    }

    /// Helper to check if there's an impl block for a type implementing a trait
    pub(super) fn find_trait_impl_for_type(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_name: &str,
        trait_name: &str,
    ) -> bool {
        self.find_trait_impl_for_type_with_args(ctx, scope, type_name, trait_name, None)
    }

    pub(super) fn has_real_trait_impl_for_type(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_name: &str,
        trait_name: &str,
    ) -> bool {
        self.trait_env.has_any_methodful_impl(type_name, trait_name)
            || self.blanket_trait_impl_applies(ctx, scope, type_name, trait_name)
    }

    /// Check if there's a trait impl for a type, with optional type args for bounds checking.
    /// For `impl<T: Eq> Eq for List<T>`, when checking `List<Foo>`, passes `[Foo]` as `type_args`.
    pub(super) fn find_trait_impl_for_type_with_args(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_name: &str,
        trait_name: &str,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        let trait_env = self.trait_env.clone();
        if let Some(entries) = trait_env.impl_index.get(type_name) {
            for entry in entries {
                let (module_src, _) = entry;
                let Some(header) = trait_env.impl_headers.get(entry) else {
                    continue;
                };
                let Some(impl_trait_name) = &header.trait_name else {
                    continue;
                };
                if impl_trait_name == trait_name
                    && self.inherent_impl_type_args_match(
                        &header.ty,
                        &header.type_params,
                        type_args,
                        module_src,
                    )
                    && self.check_impl_block_bounds(
                        ctx,
                        scope,
                        &header.type_params,
                        &header.ty,
                        type_args,
                    )
                {
                    return true;
                }
            }
        }

        // The current module's trait impls are already covered by
        // `impl_index` above (the index is built from every loaded module,
        // including this one), so no separate current-module scan is needed.

        self.blanket_trait_impl_applies(ctx, scope, type_name, trait_name)
    }

    fn blanket_trait_impl_applies(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_name: &str,
        trait_name: &str,
    ) -> bool {
        let trait_env = self.trait_env.clone();
        for entry in &trait_env.blanket_impl_index {
            let Some(header) = trait_env.impl_headers.get(entry) else {
                continue;
            };
            let Some(impl_trait_name) = &header.trait_name else {
                continue;
            };
            if impl_trait_name == trait_name {
                let impl_type_name = super::trait_env::get_type_name_static(&header.ty);
                let matching_param = header
                    .type_params
                    .iter()
                    .find(|tp| tp.name == impl_type_name);
                if let Some(param) = matching_param {
                    let bounds_satisfied = param.bounds.iter().all(|bound| {
                        self.find_trait_impl_for_type(ctx, scope, type_name, &bound.name)
                    });
                    if bounds_satisfied {
                        return true;
                    }
                }
            }
        }

        false
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Compute `assoc_type_bindings` for an `AssocTypeProjection` by resolving `Self::X`
    /// references in the trait bound's associated type constraints.
    ///
    /// Example: `IntoIterator::Iter` has bound `Iterator<Item = Self::Item>`.
    /// With `I: IntoIterator<Item = u8>` and `self_type = I`, `Self::Item = I::Item = u8`.
    /// Result: `[("Item", u8_typeid)]`, stored in the `I::Iter` projection.
    ///
    /// This enables `I::Iter::Item` to resolve to `u8` when `Iterator::next` is called.
    fn compute_assoc_type_bindings_from_trait_bounds(
        &mut self,
        self_type_id: TypeId,
        self_type_param_name: Option<&str>,
        assoc_bounds: &[crate::ast::TraitBound],
    ) -> Vec<(String, TypeId)> {
        let mut bindings = Vec::new();
        let Some(type_param_name) = self_type_param_name else {
            // Also handle AssocTypeProjection self_type: propagate bindings from its bindings
            let resolved = self.tysys.type_table.borrow().get(self_type_id).clone();
            if let ResolvedType::AssocTypeProjection {
                assoc_type_bindings,
                ..
            } = resolved
            {
                // Reuse existing bindings from the source projection
                return assoc_type_bindings;
            }
            return bindings;
        };
        // For each bound, check its associated type constraints and resolve Self::X
        for bound in assoc_bounds {
            for assoc in &bound.assoc_types.clone() {
                if let crate::ast::Type::NamespacedGeneric(ns) = &assoc.ty
                    && ns.namespace == "Self"
                {
                    // Self::ns.name → type_param_name::ns.name
                    // Look in current_type_param_bounds[type_param_name] for direct binding
                    if let Some(param_bounds) = self
                        .annotate_ctx
                        .trait_ctx
                        .type_param_bounds
                        .get(type_param_name)
                        .cloned()
                    {
                        for pb in &param_bounds {
                            for ab in &pb.assoc_types {
                                if ab.name == ns.name {
                                    let resolved_ty = self.resolve_type(&ab.ty.clone());
                                    bindings.push((assoc.name.clone(), resolved_ty));
                                }
                            }
                        }
                    }
                }
            }
        }
        bindings
    }

    /// Find a method in the trait declarations given by the bound names.
    /// For example, if T: Ord, look up the "cmp" method in the Ord trait declaration.
    /// Returns (`trait_name`, `MethodInfo`) with the method's return type, `self_kind`, and `param_types`,
    /// where Self is substituted with the `TypeParam`'s type.
    pub(super) fn find_method_in_trait_bounds(
        &mut self,
        bounds: &[String],
        method_name: &str,
        self_type_id: TypeId,
    ) -> Option<(String, MethodInfo)> {
        // Collect trait declarations from all modules
        for trait_name in bounds {
            // Search all loaded modules for the trait declaration
            let mut found_trait_method: Option<(
                ast::Function,
                Vec<ast::AssociatedTypeDecl>,
                ModuleSource,
            )> = None;

            for (module_src, module) in self.loaded_modules {
                for item in &module.items {
                    if let Item::Trait(trait_decl) = item
                        && trait_decl.name == *trait_name
                    {
                        for method in &trait_decl.methods {
                            if method.name == method_name {
                                found_trait_method = Some((
                                    method.clone(),
                                    trait_decl.associated_types.clone(),
                                    module_src.clone(),
                                ));
                                break;
                            }
                        }
                    }
                }
                if found_trait_method.is_some() {
                    break;
                }
            }

            // Also check current module items
            if found_trait_method.is_none() {
                for item in self.current_module_items {
                    if let Item::Trait(trait_decl) = item
                        && trait_decl.name == *trait_name
                    {
                        for method in &trait_decl.methods {
                            if method.name == method_name {
                                found_trait_method = Some((
                                    method.clone(),
                                    trait_decl.associated_types.clone(),
                                    self.current_module_source.clone(),
                                ));
                                break;
                            }
                        }
                    }
                }
            }

            if let Some((method, trait_assoc_types, _module_source)) = found_trait_method {
                // Save the entire trait context; we'll modify self_type, assoc_type_bindings,
                // type_params, and type_param_bounds during this resolution scope.
                let mut scope = self.enter_inherited_type_param_scope();
                scope.annotate_ctx.trait_ctx.self_type = Some(self_type_id);
                scope.annotate_ctx.trait_ctx.assoc_type_bindings.clear();

                // Set up associated type bindings as projections so that
                // Self::AssocType resolves to AssocTypeProjection(self_type_id, "AssocType").
                // We also compute assoc_type_bindings to propagate concrete types through
                // associated type chains.
                // Determine the TypeParam name for Self, if self_type is a TypeParam.
                let self_type_param_name = {
                    let resolved = scope.tysys.type_table.borrow().get(self_type_id).clone();
                    if let ResolvedType::TypeParam { name, .. } = resolved {
                        Some(name)
                    } else {
                        None
                    }
                };
                for assoc_decl in &trait_assoc_types {
                    // Check if self_type has a direct assoc_type_binding for this name.
                    // This handles the case: self_type = I::Iter which has ("Item", u8_typeid).
                    let directly_bound = {
                        let resolved = scope.tysys.type_table.borrow().get(self_type_id).clone();
                        if let ResolvedType::AssocTypeProjection {
                            assoc_type_bindings,
                            ..
                        } = resolved
                        {
                            assoc_type_bindings
                                .iter()
                                .find(|(name, _)| *name == assoc_decl.name)
                                .map(|(_, type_id)| *type_id)
                        } else {
                            None
                        }
                    };
                    let projection = directly_bound.unwrap_or_else(|| {
                        let bound_names: Vec<String> =
                            assoc_decl.bounds.iter().map(|b| b.name.clone()).collect();
                        // Compute assoc_type_bindings by resolving Self::X references in the
                        // assoc type's bounds. e.g., Iterator<Item = Self::Item> with Self = I
                        // and I: IntoIterator<Item = u8> gives [("Item", u8)].
                        let atb = scope.compute_assoc_type_bindings_from_trait_bounds(
                            self_type_id,
                            self_type_param_name.as_deref(),
                            &assoc_decl.bounds,
                        );
                        scope
                            .tysys
                            .type_table
                            .borrow_mut()
                            .make_assoc_type_projection(
                                self_type_id,
                                assoc_decl.name.clone(),
                                bound_names,
                                atb,
                            )
                    });
                    scope
                        .annotate_ctx
                        .trait_ctx
                        .assoc_type_bindings
                        .insert(assoc_decl.name.clone(), projection);
                }

                // Register the method's own type parameters so that they resolve to
                // TypeParam{index: N} instead of UNKNOWN. This is needed for generic methods
                // like `fn next_element<T: Deserialize>(&mut self) -> Result<Option<T>, ...>`
                // where `T` must be a proper TypeParam to allow substitution at the call site.
                // We use index 0, 1, ... because find_method_in_trait_bounds is only called
                // for TypeParam/AssocTypeProjection receivers, where impl_offset = 0.
                let mut method_type_param_ids: Vec<TypeId> = Vec::new();
                for (index, param) in method.type_params.iter().enumerate() {
                    let type_id = scope
                        .tysys
                        .type_table
                        .borrow_mut()
                        .make_type_param(param.name.clone(), index as u32);
                    scope
                        .annotate_ctx
                        .trait_ctx
                        .type_params
                        .insert(param.name.clone(), (index as u32, type_id));
                    if !param.bounds.is_empty() {
                        scope
                            .annotate_ctx
                            .trait_ctx
                            .type_param_bounds
                            .insert(param.name.clone(), param.bounds.clone());
                    }
                    if !param.is_effect {
                        method_type_param_ids.push(type_id);
                    }
                }

                let return_type = method
                    .return_type
                    .as_ref()
                    .map(|t| scope.resolve_type(t))
                    .unwrap_or(TypeTable::UNIT);
                let self_kind = method
                    .params
                    .first()
                    .map(|p| p.self_kind)
                    .unwrap_or(ast::SelfKind::None);
                let param_types = scope.extract_param_types(&method.params);
                let param_is_mut: Vec<bool> = method
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.is_mut)
                    .collect();
                let param_defaults: Vec<Option<ast::Expr>> = method
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.default.clone())
                    .collect();
                let param_names: Vec<String> = method
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| p.name.clone())
                    .collect();

                drop(scope);

                return Some((
                    trait_name.clone(),
                    MethodInfo {
                        return_type,
                        self_kind,
                        param_types,
                        param_is_mut,
                        inherited_from_base: None,
                        cm_name: None,
                        is_ref_impl: false,
                        method_type_param_ids,
                        impl_module: None,
                        from_concrete_impl: false,
                        param_defaults,
                        param_names,
                        consumes_self: super::method_lookup::takes_self_by_value(&method.params),
                    },
                ));
            }
        }

        None
    }
}

impl TypeSystem {
    /// Check if an impl block's type parameter bounds are satisfied by the given type args.
    /// For `impl<T: Ord> List<T>`, checks that the concrete type substituted for T implements Ord.
    pub(super) fn check_impl_block_bounds(
        &self,
        ctx: &Scope,
        scope: &TypeLookup,
        type_params: &[ast::GenericParam],
        impl_ty: &ast::Type,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        // No type params with bounds → always OK
        if type_params.iter().all(|p| p.bounds.is_empty()) {
            return true;
        }

        let Some(type_args) = type_args else {
            // No type args to check (non-generic receiver) → skip bounds check
            return true;
        };

        // Build name → bounds map from impl block type params (trait names only)
        let bounds_map: IndexMap<&str, Vec<String>> = type_params
            .iter()
            .filter(|p| !p.bounds.is_empty())
            .map(|p| {
                (
                    p.name.as_str(),
                    p.bounds.iter().map(|b| b.name.clone()).collect(),
                )
            })
            .collect();

        // Match type params to receiver type args via generic type arg positions
        let inner_type_name: Option<&str> =
            if let ast::Type::Reference(boxed) | ast::Type::MutReference(boxed) = impl_ty {
                if let ast::Type::Named(inner) = boxed.as_ref() {
                    Some(&inner.name)
                } else {
                    None
                }
            } else {
                None
            };

        if let ast::Type::Generic(generic) = impl_ty {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg
                    && let Some(bounds) = bounds_map.get(named.name.as_str())
                    && let Some(&type_arg) = type_args.get(i)
                {
                    // A type-parameter arg satisfies a bound only if that bound is
                    // among its declared bounds (`type_implements_trait` reads
                    // `type_param_bounds`): an unbounded `T` must not match
                    // `impl<T: Ref> ... for C<T>`, else a container's generic
                    // `self.repr[i]` would bind the reference impl and box scalars.
                    for bound in bounds {
                        if !self.type_implements_trait(ctx, scope, type_arg, bound) {
                            return false;
                        }
                    }
                }
            }
        } else if let Some(inner_name) = inner_type_name {
            // Handle `impl<T: Bound> Trait for &T` / `impl<T: Bound> Trait for &mut T`:
            // type_args[0] is the inner type T.
            if let Some(bounds) = bounds_map.get(inner_name)
                && let Some(&type_arg) = type_args.first()
                && !matches!(
                    self.type_table.borrow().get(type_arg),
                    ResolvedType::TypeParam { .. }
                )
            {
                for bound in bounds {
                    if !self.type_implements_trait(ctx, scope, type_arg, bound) {
                        return false;
                    }
                }
            }
        } else if let ast::Type::Tuple(elements) = impl_ty {
            // Variadic tuple impl (`impl<..T: Trait> Trait for [..T]`, e.g.
            // `Eq`/`Ord` for tuples in core:prelude/tuple.wado): every entry
            // in `type_args` instantiates the same variadic parameter, so
            // each is checked against its bounds.
            for elem in elements {
                let ast::Type::TypePackSpread(name, _) = elem else {
                    continue;
                };
                let Some(bounds) = bounds_map.get(name.as_str()) else {
                    continue;
                };
                for &type_arg in type_args {
                    if matches!(
                        self.type_table.borrow().get(type_arg),
                        ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
                    ) {
                        continue;
                    }
                    for bound in bounds {
                        if !self.type_implements_trait(ctx, scope, type_arg, bound) {
                            return false;
                        }
                    }
                }
            }
        }

        true
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Check trait bounds on a generic function's type arguments.
    /// Looks up the function's type params and validates bounds against the provided type args.
    pub(super) fn check_function_type_arg_bounds(
        &mut self,
        callee: &CalleeRef,
        type_args: &[TypeId],
        span: Span,
    ) {
        let type_params = self.lookup_function_type_params(callee);
        self.enforce_type_arg_bounds(&type_params, type_args, span);
    }

    /// The single enforcement of trait bounds on a generic decl's type args,
    /// shared by every generic-call kind so the rule cannot drift. Enforces only
    /// fully concrete args: a still-parametric arg is forwarded from the caller
    /// (verified once concrete, since impl-level bounds are not in scope here),
    /// and `fn(...)`-bound params are realised eagerly elsewhere.
    pub(super) fn enforce_type_arg_bounds(
        &mut self,
        params: &[ast::GenericParam],
        type_args: &[TypeId],
        span: Span,
    ) {
        for (i, param) in params.iter().enumerate() {
            let Some(&type_arg) = type_args.get(i) else {
                continue;
            };
            if self.tysys.type_table.borrow().contains_type_param(type_arg) {
                // Also covers holes (reserved-index params), re-checked at finalize.
                continue;
            }
            for bound in &param.bounds {
                if bound.fn_signature.is_some() {
                    continue;
                }
                self.enforce_single_bound(type_arg, &bound.name, &param.name, span);
            }
        }
    }

    /// Check one concrete type argument against one trait bound — the primitive
    /// every bound-enforcement path funnels through. On success registers the
    /// associated types; on failure raises a clean `TraitBoundNotSatisfied`.
    pub(super) fn enforce_single_bound(
        &mut self,
        type_arg: TypeId,
        trait_name: &str,
        param_name: &str,
        span: Span,
    ) {
        if self.tysys.type_implements_trait(
            &self.annotate_ctx,
            &self.type_lookup(),
            type_arg,
            trait_name,
        ) {
            self.register_assoc_types_for_concrete_type_and_trait(type_arg, trait_name);
        } else {
            let type_name = self.tysys.type_id_to_string(type_arg);
            let reason = self.tysys.trait_unimpl_reason_chain(
                &self.annotate_ctx,
                &self.type_lookup(),
                type_arg,
                trait_name,
            );
            let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name: trait_name.to_string(),
                param_name: param_name.to_string(),
                reason,
                span,
            });
        }
    }

    /// Register associated type resolutions for a concrete type instantiating a trait.
    /// For example, when `List<u8>` implements `IntoIterator`, registers:
    /// - (List<u8>, "Item") → u8
    /// - (List<u8>, "Iter") → `ListIter`<u8>
    ///
    /// This enables the monomorphizer to resolve `I::Iter` → `ListIter<u8>` when `I = List<u8>`.
    pub(super) fn register_assoc_types_for_concrete_type_and_trait(
        &mut self,
        concrete_type_id: TypeId,
        trait_name: &str,
    ) {
        // Get the base type name and concrete type args for impl block lookup.
        // For newtypes, follow the chain to the underlying type to find the trait impl,
        // but registration (below) still uses concrete_type_id so the monomorphizer can
        // resolve e.g. `MyBytes::Iter` when `MyBytes` is a newtype over `List<u8>`.
        let (type_name, concrete_type_args) = {
            let tt = self.tysys.type_table.borrow();
            let effective_id = tt.get_ultimate_base_type(concrete_type_id);
            let list_name = tt
                .compiler_struct_name(crate::compiler_item::CompilerItem::List)
                .to_string();
            match tt.get(effective_id).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } => (name, type_args),
                ResolvedType::Struct { name, .. } => (name, vec![]),
                ResolvedType::BuiltinArray(elem) => (list_name, vec![elem]),
                // Primitives (`i32`, `f64`, `bool`, ...) can implement traits
                // with associated types just like structs. Without this arm,
                // a generic call like `parse_range::<i32>(...)` would skip
                // the `i32::Err = ParseIntError` registration and leave
                // `T::Err` unresolved at the caller's binding site.
                ResolvedType::Primitive(p) => (p.as_str().to_string(), vec![]),
                _ => return,
            }
        };

        // Collect matching impl block info (avoids borrow conflicts during resolution)
        struct ImplInfo {
            type_params: Vec<ast::GenericParam>,
            impl_ty_param_names: Vec<String>,
            assoc_types: Vec<ast::AssociatedTypeBinding>,
        }
        let trait_env = self.tysys.trait_env.clone();
        let impl_infos: Vec<ImplInfo> = {
            let mut result = vec![];
            if let Some(entries) = trait_env.impl_index.get(&type_name) {
                for entry in entries {
                    let Some(header) = trait_env.impl_headers.get(entry) else {
                        continue;
                    };
                    if header.trait_name.as_deref() == Some(trait_name)
                        && !header.associated_types.is_empty()
                    {
                        let impl_ty_param_names: Vec<String> = match &header.ty {
                            ast::Type::Generic(g) => g
                                .args
                                .iter()
                                .filter_map(|arg| {
                                    if let ast::Type::Named(named) = arg {
                                        Some(named.name.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                            _ => vec![],
                        };
                        result.push(ImplInfo {
                            type_params: header.type_params.clone(),
                            impl_ty_param_names,
                            assoc_types: header.associated_types.clone(),
                        });
                    }
                }
            }
            result
        };

        for info in impl_infos {
            let mut scope = self.enter_inherited_type_param_scope();

            // Bind impl type params to concrete type args.
            // For `impl<T> IntoIterator for List<T>` with List<u8>:
            // impl_ty_param_names = ["T"], concrete_type_args = [u8_typeid]
            // → set current_type_params["T"] = (0, u8_typeid)
            for (i, tp_name) in info.impl_ty_param_names.iter().enumerate() {
                if let Some(&concrete_arg) = concrete_type_args.get(i) {
                    scope
                        .annotate_ctx
                        .trait_ctx
                        .type_params
                        .insert(tp_name.clone(), (i as u32, concrete_arg));
                }
            }
            // Add bounds from type param declarations
            for param in &info.type_params {
                if !param.bounds.is_empty() {
                    scope
                        .annotate_ctx
                        .trait_ctx
                        .type_param_bounds
                        .entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.clone());
                }
            }

            // Resolve and register each associated type in this substituted context
            for binding in &info.assoc_types {
                let resolved_id = scope.resolve_type(&binding.ty);
                if !scope
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_param(resolved_id)
                {
                    scope
                        .tysys
                        .type_table
                        .borrow_mut()
                        .register_assoc_type_resolution(
                            concrete_type_id,
                            binding.name.clone(),
                            resolved_id,
                        );
                }
            }

            drop(scope);
        }

        // Also check blanket impls: `impl<I: Trait> OtherTrait for I`.
        // For example, `impl<I: Iterator> IntoIterator for I` applies to StrUtf8ByteIter.
        struct BlanketImplInfo {
            blanket_param_name: String,
            blanket_param_bounds: Vec<ast::TraitBound>,
            assoc_types: Vec<ast::AssociatedTypeBinding>,
        }
        let blanket_infos: Vec<BlanketImplInfo> = {
            let mut result = vec![];
            for entry in &trait_env.blanket_impl_index {
                let Some(header) = trait_env.impl_headers.get(entry) else {
                    continue;
                };
                if header.trait_name.as_deref() == Some(trait_name)
                    && !header.associated_types.is_empty()
                {
                    let impl_type_name = Self::get_type_name_static(&header.ty);
                    if let Some(blanket_param) = header
                        .type_params
                        .iter()
                        .find(|tp| tp.name == impl_type_name && !tp.bounds.is_empty())
                    {
                        // Check if the concrete type satisfies the blanket param's bounds
                        let bounds_ok = blanket_param.bounds.iter().all(|bound| {
                            self.tysys.type_implements_trait(
                                &self.annotate_ctx,
                                &self.type_lookup(),
                                concrete_type_id,
                                &bound.name,
                            )
                        });
                        if bounds_ok {
                            result.push(BlanketImplInfo {
                                blanket_param_name: blanket_param.name.clone(),
                                blanket_param_bounds: blanket_param.bounds.clone(),
                                assoc_types: header.associated_types.clone(),
                            });
                        }
                    }
                }
            }
            result
        };

        for info in blanket_infos {
            let mut scope = self.enter_inherited_type_param_scope();

            // Bind the blanket type param to the concrete type
            // For `impl<I: Iterator> IntoIterator for I` with StrUtf8ByteIter:
            // → set current_type_params["I"] = (0, StrUtf8ByteIter_typeid)
            scope
                .annotate_ctx
                .trait_ctx
                .type_params
                .insert(info.blanket_param_name.clone(), (0, concrete_type_id));
            scope
                .annotate_ctx
                .trait_ctx
                .type_param_bounds
                .insert(info.blanket_param_name.clone(), info.blanket_param_bounds);

            // Resolve and register each associated type
            for binding in &info.assoc_types {
                let resolved_id = scope.resolve_type(&binding.ty);
                if !scope
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_param(resolved_id)
                {
                    scope
                        .tysys
                        .type_table
                        .borrow_mut()
                        .register_assoc_type_resolution(
                            concrete_type_id,
                            binding.name.clone(),
                            resolved_id,
                        );
                }
            }

            drop(scope);
        }
    }

    /// Build a mapping from type parameter names to concrete type IDs.
    /// For `impl Trait for Container<T>` with concrete type `Container<i32>`,
    /// returns `{"T" -> i32's TypeId}`.
    ///
    /// When `declared_type_params` is non-empty, only names in that set are
    /// treated as type parameters. This prevents concrete types (e.g., `String` in
    /// `impl Trait for Map<String, V>`) from being incorrectly mapped.
    /// When empty, all `Named` types are assumed to be type parameters (legacy behavior).
    /// Single entry point for resolving a trait method that a binary operator
    /// dispatches to (Eq / Ord / Add / Sub / Mul / Div / Rem / `BitAnd` / `BitOr` /
    /// `BitXor` / Shl / Shr). Produces a fully-populated
    /// [`ResolvedTraitMethod`] with the rhs type already substituted, so
    /// callers need not reach into the underlying `find_*_trait_impl`
    /// family and cannot forget to wire `rhs_type` through the typecheck.
    ///
    /// `struct_name` / `lookup_type_id` are the name-and-id used for impl
    /// lookup (for newtypes this may be the ultimate base). `is_type_param`
    /// is true for `T: Trait` type-param receivers.
    pub(super) fn resolve_trait_method_for_op(
        &mut self,
        struct_name: &str,
        lookup_type_id: TypeId,
        trait_name: &str,
        method_name: &str,
        is_type_param: bool,
    ) -> Option<ResolvedTraitMethod> {
        // 1. User-written impl via the shared arithmetic-trait lookup.
        // 2. Auto-derive fallback (Eq / Ord only; other operator traits
        //    have no auto-derive rules).
        //
        // Eq / Ord trait decls fix their return types (`bool` and `Ordering`
        // respectively) regardless of what a user impl writes, so normalize
        // those here. `find_arithmetic_trait_impl` would otherwise default
        // `output_type` to the receiver type when no `type Output` is
        // declared. The auto-derive set and the fixed return types come from
        // `TypeSystem::auto_derive_by_trait` (the single source).
        let auto_derive = self.tysys.auto_derive_by_trait(trait_name);
        let (info_trait_name, self_kind, param_types, return_type) = if let Some(info) =
            self.find_arithmetic_trait_impl(struct_name, lookup_type_id, trait_name, method_name)
        {
            let return_type = auto_derive.unwrap_or(info.output_type);
            let param_types = info.rhs_type.map(|t| vec![t]).unwrap_or_default();
            (info.trait_name, info.self_kind, param_types, return_type)
        } else if let Some(return_type) = auto_derive
            && self.tysys.type_implements_trait(
                &self.annotate_ctx,
                &self.type_lookup(),
                lookup_type_id,
                trait_name,
            )
        {
            let ref_self_ty = self
                .tysys
                .type_table
                .borrow_mut()
                .intern(ResolvedType::Ref(lookup_type_id));
            (
                trait_name.to_string(),
                ast::SelfKind::Ref,
                vec![ref_self_ty],
                return_type,
            )
        } else {
            return None;
        };
        Some(ResolvedTraitMethod {
            trait_name: info_trait_name,
            method_name: method_name.to_string(),
            impl_name: struct_name.to_string(),
            self_kind,
            return_type,
            param_types,
            is_type_param_receiver: is_type_param,
        })
    }

    /// Fallback for [`Self::find_trait_method_for_type`]: when no user-written
    /// impl of `trait_name::method_name` exists for a type that is still
    /// auto-derive-eligible (per [`Self::type_implements_trait`]), synthesize
    /// a [`TraitMethodMatch`] whose [`MethodInfo`] has the receiver type
    /// fully substituted into `Self` positions. This is the single pathway
    /// that makes auto-derived methods discoverable via method-call
    /// resolution, mirroring what operator dispatch already got from the
    /// arithmetic-trait lookup's own auto-derive branch.
    ///
    /// Only method bodies actually produced by the trait-synthesis phase
    /// (`synthesis::traits`) are returned here; primitives are excluded
    /// because their equality/comparison lowers to Wasm instructions, not
    /// to a method body.
    ///
    /// The method ↔ trait mapping and the fixed return type come from
    /// [`TypeSystem::auto_derive_by_method`] — the single source for which
    /// traits the compiler auto-derives — so adding a new auto-derived trait
    /// touches that table, not this arm.
    ///
    /// The synthesized [`MethodInfo`] is still built with the fixed shape the
    /// auto-derived `Eq` / `Ord` declarations have (`self_kind: Ref`, a single
    /// `&Self` parameter named `"other"`, no defaults, no method type params).
    /// Should a future auto-derived trait need a different shape, read it from
    /// the trait's `ast::Function` (via `find_trait_decl_methods`) instead of
    /// this literal; the current two traits share this shape exactly.
    pub(super) fn try_auto_derived_method_match(
        &mut self,
        struct_name: &str,
        method_name: &str,
        receiver_type_id: TypeId,
    ) -> Option<TraitMethodMatch> {
        let (trait_name, return_type) = self.tysys.auto_derive_by_method(method_name)?;
        let base_type_id = self.tysys.get_base_type(receiver_type_id);
        if !self.tysys.auto_derive_eligible_kind(base_type_id) {
            return None;
        }
        if !self.tysys.type_implements_trait(
            &self.annotate_ctx,
            &self.type_lookup(),
            base_type_id,
            &trait_name,
        ) {
            return None;
        }
        let ref_self_ty = self
            .tysys
            .type_table
            .borrow_mut()
            .intern(ResolvedType::Ref(base_type_id));
        let method_info = MethodInfo {
            return_type,
            self_kind: ast::SelfKind::Ref,
            param_types: vec![ref_self_ty],
            param_is_mut: vec![false],
            param_defaults: vec![None],
            param_names: vec!["other".to_string()],
            inherited_from_base: None,
            cm_name: None,
            is_ref_impl: false,
            method_type_param_ids: vec![],
            impl_module: None,
            from_concrete_impl: false,
            consumes_self: false,
        };
        let impl_module_source = self.find_struct_module_source(struct_name);
        Some(TraitMethodMatch {
            trait_name,
            method_info,
            impl_module_source,
            blanket_type_param: None,
            impl_struct_name: struct_name.to_string(),
            is_blanket_ref_impl: false,
        })
    }
}
