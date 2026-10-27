//! What each type name a Component Model binding writes names: the declaration
//! the resolver answers it with, never its spelling. The registry reads a
//! binding's signatures as AST, so this is how it asks whether a `Type` is a
//! `List` or a `ListView`.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::ast::{AstId, GenericType, Item, Module, NamedType, Type};
use crate::compiler_item::{CompilerItem, parse_compiler_item_attrs};
use crate::component_model::for_each_signed_type;
use crate::hashmap::IndexMap;
use crate::resolve::Resolutions;
use crate::token::Span;

#[derive(Debug, Clone, Default)]
pub struct CmTypeDecls {
    sites: Sites,
    /// The compiler item each declaration is, with the name it declares.
    items: IndexMap<AstId, (CompilerItem, String)>,
    /// The base of each newtype among them (`ByteList`'s `List<u8>`), which no
    /// CM interface declares.
    newtype_bases: IndexMap<AstId, Type>,
}

/// The declaration each type site resolves to. Interior-mutable because
/// synthesis mints sites over a registry shared as `Arc`, and `Sync` because
/// the bundled bindings' table is a process-wide `OnceLock`.
#[derive(Debug, Default)]
struct Sites(RwLock<IndexMap<AstId, AstId>>);

impl Clone for Sites {
    fn clone(&self) -> Self {
        Self(RwLock::new(self.read().clone()))
    }
}

impl Sites {
    fn read(&self) -> RwLockReadGuard<'_, IndexMap<AstId, AstId>> {
        self.0.read().expect("type-site table not poisoned")
    }

    fn write(&self) -> RwLockWriteGuard<'_, IndexMap<AstId, AstId>> {
        self.0.write().expect("type-site table not poisoned")
    }

    fn get(&self, site: AstId) -> Option<AstId> {
        self.read().get(&site).copied()
    }
}

impl CmTypeDecls {
    /// Record the compiler item every declaration in `modules` is, and what each
    /// type the items of `signing` sign resolves to.
    pub fn bind<'m>(
        &mut self,
        modules: impl IntoIterator<Item = &'m Module>,
        signing: impl IntoIterator<Item = &'m Module>,
        resolutions: &Resolutions,
    ) {
        let defs = resolutions.defs();
        for item in modules.into_iter().flat_map(|module| &module.items) {
            let (items, _) = parse_compiler_item_attrs(item.attrs());
            let ([compiler_item], Some(def)) = (items.as_slice(), defs.of_ast_id(item.id())) else {
                continue;
            };
            if !defs.kind(def).is_type() {
                continue;
            }
            self.items
                .insert(item.id(), (*compiler_item, defs.name(def).to_string()));
            if let Item::Newtype(alias) = item {
                self.bind_sites(&alias.ty, resolutions);
                self.newtype_bases.insert(item.id(), alias.ty.clone());
            }
        }
        for item in signing.into_iter().flat_map(|module| &module.items) {
            for_each_signed_type(item, &mut |ty| self.bind_sites(ty, resolutions));
        }
    }

    fn bind_sites(&self, ty: &Type, resolutions: &Resolutions) {
        let defs = resolutions.defs();
        let mut sites = self.sites.write();
        ty.for_each(&mut |ty| {
            if let Some(site) = ty.id()
                && let Some(def) = resolutions.declared_if_walked(site)
            {
                sites.insert(site, defs.ast_id(def));
            }
        });
    }

    /// Whether the site `site` was answered.
    #[cfg(test)]
    #[must_use]
    pub fn binds(&self, site: AstId) -> bool {
        self.sites.get(site).is_some()
    }

    /// The compiler item the type site `site` names, if it names one.
    #[must_use]
    pub fn item_at(&self, site: AstId) -> Option<CompilerItem> {
        let decl = self.sites.get(site)?;
        self.items.get(&decl).map(|(item, _)| *item)
    }

    /// Whether the type site `site` names `item`.
    #[must_use]
    pub fn is(&self, site: AstId, item: CompilerItem) -> bool {
        self.item_at(site) == Some(item)
    }

    /// The compiler item `ty` names, if it names one.
    #[must_use]
    pub fn item_of(&self, ty: &Type) -> Option<CompilerItem> {
        self.item_at(ty.id()?)
    }

    /// Whether `ty` names `item`.
    #[must_use]
    pub fn names(&self, ty: &Type, item: CompilerItem) -> bool {
        self.item_of(ty) == Some(item)
    }

    /// The base of the compiler-item newtype `named` names.
    #[must_use]
    pub fn newtype_base(&self, named: &NamedType) -> Option<&Type> {
        self.newtype_bases.get(&self.sites.get(named.id)?)
    }

    /// A type naming `item`'s declaration over `args`, as a site of its own.
    pub fn item_type(&self, item: CompilerItem, args: Vec<Type>, span: Span) -> Type {
        let (decl, name) = self
            .items
            .iter()
            .find(|(_, (declared, _))| *declared == item)
            .map(|(decl, (_, name))| (*decl, name.clone()))
            .unwrap_or_else(|| panic!("`{item}` is declared where the registry reads"));
        let id = AstId::fresh();
        self.sites.write().insert(id, decl);
        if args.is_empty() {
            Type::Named(NamedType::new(id, name, span))
        } else {
            Type::Generic(GenericType {
                id,
                name,
                args,
                span,
            })
        }
    }
}
