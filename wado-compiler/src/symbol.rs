//! Symbol table for name resolution
//!
//! The symbol table tracks all definitions (functions, types, effects, etc.)
//! and their metadata. Each symbol is keyed by `(ModuleSource, AstId)` — the
//! stable coordinate of the AST node that introduced it. This lets downstream
//! phases and LSP queries translate between source positions, AST nodes, and
//! semantic information without a parallel integer ID space.

use crate::hashmap::IndexMap;

use crate::ast::{AstId, CmImport};
use crate::module_source::ModuleSource;
use crate::token::Span;

/// Canonical identity of a symbol.
///
/// The pair `(module, ast_id)` uniquely identifies the AST node that declared
/// the symbol. Effect / resource methods registered under qualified names
/// (e.g., `"Stdout::write"`) use their own method `AstId`, not the parent's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolKey {
    pub module: ModuleSource,
    pub ast_id: AstId,
}

impl SymbolKey {
    pub fn new(module: ModuleSource, ast_id: AstId) -> Self {
        Self { module, ast_id }
    }
}

/// The kind of symbol and its associated data
#[derive(Debug, Clone)]
pub enum SymbolKind {
    /// A function definition
    Function(FunctionSymbol),
    /// An effect definition
    Effect(EffectSymbol),
    /// A struct definition
    Struct(StructSymbol),
    /// An enum definition
    Enum(EnumSymbol),
    /// A flags definition (bitset)
    Flags(FlagsSymbol),
    /// A variant definition (tagged union with payloads)
    Variant(VariantSymbol),
    /// A trait definition
    Trait(TraitSymbol),
    /// A newtype
    Newtype(NewtypeSymbol),
    /// A local variable
    Variable(VariableSymbol),
    /// A resource definition
    Resource(ResourceSymbol),
    /// A world definition
    World(WorldSymbol),
    /// A global variable
    Global(GlobalSymbol),
}

/// Function symbol data
#[derive(Debug, Clone)]
pub struct FunctionSymbol {
    /// Parameter names and types (types are optional in Wado)
    pub params: Vec<String>,
    /// Return type name (if specified)
    pub return_type: Option<String>,
    /// Effect requirements
    pub effects: Vec<String>,
    /// Whether this is a builtin function (e.g., println from `core::cli`)
    pub is_builtin: bool,
    /// CM import metadata if this function maps to a CM function
    pub cm_import: Option<CmImport>,
}

/// Effect symbol data
#[derive(Debug, Clone)]
pub struct EffectSymbol {
    /// Method names defined in this effect
    pub methods: Vec<String>,
    /// CM import metadata if this effect maps to a CM interface
    pub cm_import: Option<CmImport>,
}

/// Struct symbol data
#[derive(Debug, Clone)]
pub struct StructSymbol {
    /// Field names
    pub fields: Vec<String>,
}

/// Enum symbol data
#[derive(Debug, Clone)]
pub struct EnumSymbol {
    /// Case names
    pub cases: Vec<String>,
}

/// Flags symbol data (bitset type)
#[derive(Debug, Clone)]
pub struct FlagsSymbol {
    /// Member names
    pub members: Vec<String>,
}

/// Variant symbol data (tagged union with payloads)
#[derive(Debug, Clone)]
pub struct VariantSymbol {
    /// Case names (e.g., "Some", "None" for Option)
    pub cases: Vec<String>,
}

/// Trait symbol data
#[derive(Debug, Clone)]
pub struct TraitSymbol {
    /// Method names defined in this trait
    pub methods: Vec<String>,
    /// Type parameter names
    pub type_params: Vec<String>,
}

/// Newtype symbol data
#[derive(Debug, Clone)]
pub struct NewtypeSymbol {
    /// The aliased type name
    pub aliased_type: String,
}

/// Variable symbol data
#[derive(Debug, Clone)]
pub struct VariableSymbol {
    /// Whether the variable is mutable
    pub is_mut: bool,
    /// Whether the variable is reactive
    pub is_reactive: bool,
}

/// Global variable symbol data
#[derive(Debug, Clone)]
pub struct GlobalSymbol {
    /// Whether the global is mutable
    pub is_mut: bool,
}

/// Resource symbol data
#[derive(Debug, Clone)]
pub struct ResourceSymbol {
    /// Method names defined on this resource
    pub methods: Vec<String>,
    /// CM import metadata if this resource maps to a CM resource
    pub cm_import: Option<CmImport>,
}

/// World symbol data
#[derive(Debug, Clone)]
pub struct WorldSymbol {
    /// Imported interfaces (bare interface refs)
    pub imports: Vec<WorldImportSymbol>,
    /// Exported items (interface or function form)
    pub exports: Vec<WorldExportSymbol>,
}

/// An imported interface in a world
#[derive(Debug, Clone)]
pub struct WorldImportSymbol {
    pub interface_name: String,
}

/// An exported item in a world
#[derive(Debug, Clone)]
pub enum WorldExportSymbol {
    /// `export Foo;` — references a `pub interface Foo` declaration.
    Interface { interface_name: String },
    /// `export [async] fn name(...) -> ret;` — direct freestanding-function export.
    Function {
        name: String,
        is_async: bool,
        params: Vec<String>,
        return_type: Option<String>,
    },
}

/// A symbol in the symbol table
#[derive(Debug, Clone)]
pub struct Symbol {
    /// Symbol name
    pub name: String,
    /// Symbol kind and data
    pub kind: SymbolKind,
    /// Canonical identity: `(module, ast_id)` of the declaring AST node
    pub defined_at: SymbolKey,
    /// Source location (if available)
    pub span: Option<Span>,
}

impl Symbol {
    /// Module where this symbol is defined.
    pub fn module_source(&self) -> &ModuleSource {
        &self.defined_at.module
    }

    /// Check if this is a builtin function
    pub fn is_builtin_function(&self) -> bool {
        matches!(&self.kind, SymbolKind::Function(f) if f.is_builtin)
    }

    /// Get the CM import metadata if this symbol has one
    pub fn cm_import(&self) -> Option<&CmImport> {
        match &self.kind {
            SymbolKind::Function(f) => f.cm_import.as_ref(),
            SymbolKind::Effect(e) => e.cm_import.as_ref(),
            SymbolKind::Resource(r) => r.cm_import.as_ref(),
            _ => None,
        }
    }
}

/// Target of a re-export (`pub use`)
///
/// Represents where a re-exported symbol originally comes from.
#[derive(Debug, Clone)]
pub struct ReExportTarget {
    /// Module where the symbol is originally defined (or re-exported from)
    pub source_module: ModuleSource,
    /// Original name in the source module
    pub source_name: String,
}

/// The symbol table
///
/// Tracks all symbols organized by module. Symbols are keyed by
/// `(ModuleSource, AstId)` via [`SymbolKey`]; the name indices (`modules`,
/// `imports`, `reexports`) map from textual names to that canonical identity.
#[derive(Debug, Default, Clone)]
pub struct SymbolTable {
    /// All symbols keyed by their canonical identity.
    symbols: IndexMap<SymbolKey, Symbol>,
    /// Module → symbol name → canonical key
    modules: IndexMap<ModuleSource, IndexMap<String, SymbolKey>>,
    /// Re-exports: module → exported name → re-export target
    reexports: IndexMap<ModuleSource, IndexMap<String, ReExportTarget>>,
    /// Imported symbols in the current module (name → canonical key)
    imports: IndexMap<String, SymbolKey>,
}

impl SymbolTable {
    /// Create a new empty symbol table
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether a symbol with `name` is already defined directly in
    /// `module_source`.  Unlike [`Self::lookup_in_module`], this does **not**
    /// follow re-export chains — it only inspects symbols registered via
    /// [`Self::define`] for this exact module.  Used by the analyzer to flag
    /// duplicate top-level definitions.
    pub fn is_defined_in_module(&self, module_source: &ModuleSource, name: &str) -> bool {
        self.modules
            .get(module_source)
            .is_some_and(|module| module.contains_key(name))
    }

    /// Return the span of an existing direct definition of `name` in
    /// `module_source`, if any.  Mirrors [`Self::is_defined_in_module`] but
    /// hands back the declaration site for error diagnostics.
    pub fn defined_span_in_module(&self, module_source: &ModuleSource, name: &str) -> Option<Span> {
        self.modules
            .get(module_source)
            .and_then(|module| module.get(name))
            .and_then(|key| self.symbols.get(key))
            .and_then(|sym| sym.span)
    }

    /// Define a symbol in a module.
    ///
    /// The `(module_source, ast_id)` pair uniquely identifies the declaring
    /// AST node. Returns the assigned [`SymbolKey`].
    pub fn define(
        &mut self,
        module_source: &ModuleSource,
        ast_id: AstId,
        name: &str,
        kind: SymbolKind,
        span: Option<Span>,
    ) -> SymbolKey {
        let key = SymbolKey {
            module: module_source.clone(),
            ast_id,
        };
        let symbol = Symbol {
            name: name.to_string(),
            kind,
            defined_at: key.clone(),
            span,
        };
        self.symbols.insert(key.clone(), symbol);

        let module = self.modules.entry(module_source.clone()).or_default();
        module.insert(name.to_string(), key.clone());

        key
    }

    /// Register an imported symbol in the current module
    ///
    /// This makes the symbol accessible by its short name (without module prefix).
    pub fn register_import(&mut self, name: &str, key: SymbolKey) {
        self.imports.insert(name.to_string(), key);
    }

    /// Clear all registered imports (when moving to a new module)
    pub fn clear_imports(&mut self) {
        self.imports.clear();
    }

    /// Register a re-export in a module
    ///
    /// This records that `export_name` in `module_source` is a re-export of
    /// `source_name` from `source_module`.
    pub fn register_reexport(
        &mut self,
        module_source: &ModuleSource,
        export_name: &str,
        source_module: &ModuleSource,
        source_name: &str,
    ) {
        let module_reexports = self.reexports.entry(module_source.clone()).or_default();
        module_reexports.insert(
            export_name.to_string(),
            ReExportTarget {
                source_module: source_module.clone(),
                source_name: source_name.to_string(),
            },
        );
    }

    /// Check if a name is re-exported from a module
    pub fn get_reexport(
        &self,
        module_source: &ModuleSource,
        name: &str,
    ) -> Option<&ReExportTarget> {
        self.reexports
            .get(module_source)
            .and_then(|reexports| reexports.get(name))
    }

    /// Get all struct import aliases
    ///
    /// Returns tuples of (`alias_name`, `module_source`, `original_struct_name`) for imports where:
    /// - The alias name differs from the original name
    /// - The imported symbol is a struct
    pub fn get_struct_aliases(&self) -> Vec<(String, ModuleSource, String)> {
        let mut aliases = Vec::new();
        for (alias_name, key) in &self.imports {
            if let Some(symbol) = self.symbols.get(key)
                && matches!(symbol.kind, SymbolKind::Struct(_))
                && alias_name != &symbol.name
            {
                aliases.push((
                    alias_name.clone(),
                    symbol.defined_at.module.clone(),
                    symbol.name.clone(),
                ));
            }
        }
        aliases
    }

    /// Look up a symbol by name in the current import context.
    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        self.imports.get(name).and_then(|key| self.symbols.get(key))
    }

    /// Look up a symbol in a specific module
    ///
    /// This resolves re-exports transparently, following re-export chains
    /// to find the original symbol definition.
    pub fn lookup_in_module(&self, module_source: &ModuleSource, name: &str) -> Option<&Symbol> {
        self.lookup_in_module_with_visited(module_source, name, &mut Vec::new())
    }

    /// Internal lookup that tracks visited modules to detect cycles
    fn lookup_in_module_with_visited(
        &self,
        module_source: &ModuleSource,
        name: &str,
        visited: &mut Vec<(ModuleSource, String)>,
    ) -> Option<&Symbol> {
        let visit_key = (module_source.clone(), name.to_string());
        if visited.contains(&visit_key) {
            return None;
        }
        visited.push(visit_key);

        if let Some(symbol) = self
            .modules
            .get(module_source)
            .and_then(|module| module.get(name))
            .and_then(|key| self.symbols.get(key))
        {
            return Some(symbol);
        }

        if let Some(reexport) = self.get_reexport(module_source, name) {
            return self.lookup_in_module_with_visited(
                &reexport.source_module,
                &reexport.source_name,
                visited,
            );
        }

        None
    }

    /// Check if a type name is re-exported by `core:prelude`.
    ///
    /// Only types explicitly `pub use`d from `core:prelude` are considered
    /// prelude types. Internal types in prelude sub-modules that are not
    /// re-exported (e.g. `UnpackResult` in fpfmt.wado) are not checked.
    pub fn is_prelude_type(&self, name: &str) -> bool {
        let prelude = ModuleSource::prelude();
        if let Some(symbol) = self.lookup_in_module(&prelude, name) {
            return matches!(
                symbol.kind,
                SymbolKind::Struct(_)
                    | SymbolKind::Enum(_)
                    | SymbolKind::Variant(_)
                    | SymbolKind::Flags(_)
                    | SymbolKind::Newtype(_)
                    | SymbolKind::Resource(_)
            );
        }
        false
    }

    /// Get a symbol by its canonical key.
    pub fn get(&self, key: &SymbolKey) -> Option<&Symbol> {
        self.symbols.get(key)
    }

    /// Check if a symbol exists in a module
    pub fn exists_in_module(&self, module_source: &ModuleSource, name: &str) -> bool {
        self.modules
            .get(module_source)
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Get all symbols in a module
    pub fn get_module_symbols(&self, module_source: &ModuleSource) -> Vec<&Symbol> {
        self.modules
            .get(module_source)
            .map(|module| {
                module
                    .values()
                    .filter_map(|key| self.symbols.get(key))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Iterate over all symbols in insertion order.
    pub fn all_symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_source::ModuleSourceInterner;

    #[test]
    fn test_define_and_lookup() {
        let mut table = SymbolTable::new();

        let core_cli = ModuleSource::cli();
        let key = table.define(
            &core_cli,
            AstId::fresh(),
            "println",
            SymbolKind::Function(FunctionSymbol {
                params: vec!["message".to_string()],
                return_type: None,
                effects: vec!["Stdout".to_string()],
                is_builtin: true,
                cm_import: None,
            }),
            None,
        );

        let symbol = table.lookup_in_module(&core_cli, "println");
        assert!(symbol.is_some());
        assert_eq!(symbol.unwrap().defined_at, key);
        assert_eq!(symbol.unwrap().name, "println");
    }

    #[test]
    fn test_import_lookup() {
        let mut table = SymbolTable::new();

        let core_cli = ModuleSource::cli();
        let key = table.define(
            &core_cli,
            AstId::fresh(),
            "println",
            SymbolKind::Function(FunctionSymbol {
                params: vec![],
                return_type: None,
                effects: vec![],
                is_builtin: true,
                cm_import: None,
            }),
            None,
        );

        table.register_import("println", key);

        let symbol = table.lookup("println");
        assert!(symbol.is_some());
        assert_eq!(symbol.unwrap().name, "println");
    }

    #[test]
    fn test_struct_aliases() {
        let mut table = SymbolTable::new();

        let mut interner = ModuleSourceInterner::new();
        let geometry = interner.local("./geometry.wado");

        let key = table.define(
            &geometry,
            AstId::fresh(),
            "Point",
            SymbolKind::Struct(StructSymbol {
                fields: vec!["x".to_string(), "y".to_string()],
            }),
            None,
        );

        table.register_import("OtherPoint", key);

        let aliases = table.get_struct_aliases();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].0, "OtherPoint");
        assert_eq!(aliases[0].1, geometry);
        assert_eq!(aliases[0].2, "Point");
    }

    #[test]
    fn test_struct_aliases_same_name() {
        let mut table = SymbolTable::new();

        let mut interner = ModuleSourceInterner::new();
        let geometry = interner.local("./geometry.wado");

        let key = table.define(
            &geometry,
            AstId::fresh(),
            "Point",
            SymbolKind::Struct(StructSymbol {
                fields: vec!["x".to_string(), "y".to_string()],
            }),
            None,
        );

        table.register_import("Point", key);

        let aliases = table.get_struct_aliases();
        assert_eq!(aliases.len(), 0);
    }
}
