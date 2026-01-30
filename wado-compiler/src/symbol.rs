//! Symbol table for name resolution
//!
//! The symbol table tracks all definitions (functions, types, effects, etc.)
//! and their metadata. It supports module namespacing and scoped lookups.

use std::collections::HashMap;
use std::ops::Index;

use crate::ast::WasiImport;
use crate::token::Span;

/// Unique identifier for a symbol in the table.
/// This is a newtype wrapper to prevent misuse of raw integers as `SymbolId`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SymbolId(pub usize);

/// A vector of symbols that can be indexed by `SymbolId`.
#[derive(Debug, Default, Clone)]
struct SymbolVec(Vec<Symbol>);

impl SymbolVec {
    fn push(&mut self, symbol: Symbol) {
        self.0.push(symbol);
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn get(&self, id: SymbolId) -> Option<&Symbol> {
        self.0.get(id.0)
    }

    fn as_slice(&self) -> &[Symbol] {
        &self.0
    }
}

impl Index<SymbolId> for SymbolVec {
    type Output = Symbol;

    fn index(&self, id: SymbolId) -> &Self::Output {
        &self.0[id.0]
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
    /// A type alias
    TypeAlias(TypeAliasSymbol),
    /// A local variable
    Variable(VariableSymbol),
    /// A resource definition
    Resource(ResourceSymbol),
    /// A world definition
    World(WorldSymbol),
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
    /// WASI import metadata if this function maps to a WASI function
    pub wasi_import: Option<WasiImport>,
}

/// Effect symbol data
#[derive(Debug, Clone)]
pub struct EffectSymbol {
    /// Method names defined in this effect
    pub methods: Vec<String>,
    /// WASI import metadata if this effect maps to a WASI interface
    pub wasi_import: Option<WasiImport>,
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

/// Type alias symbol data
#[derive(Debug, Clone)]
pub struct TypeAliasSymbol {
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

/// Resource symbol data
#[derive(Debug, Clone)]
pub struct ResourceSymbol {
    /// Method names defined on this resource
    pub methods: Vec<String>,
    /// WASI import metadata if this resource maps to a WASI resource
    pub wasi_import: Option<WasiImport>,
}

/// World symbol data
#[derive(Debug, Clone)]
pub struct WorldSymbol {
    /// Imported effects and their functions
    pub imports: Vec<WorldImportSymbol>,
    /// Exported functions
    pub exports: Vec<WorldExportSymbol>,
}

/// An imported effect in a world
#[derive(Debug, Clone)]
pub struct WorldImportSymbol {
    /// The effect name
    pub effect_name: String,
    /// The imported function names
    pub functions: Vec<String>,
}

/// An exported function in a world
#[derive(Debug, Clone)]
pub struct WorldExportSymbol {
    /// The function name
    pub name: String,
    /// Whether this is an async function
    pub is_async: bool,
    /// Parameter names
    pub params: Vec<String>,
    /// Return type (if specified)
    pub return_type: Option<String>,
}

/// A symbol in the symbol table
#[derive(Debug, Clone)]
pub struct Symbol {
    /// Unique identifier
    pub id: SymbolId,
    /// Symbol name
    pub name: String,
    /// Symbol kind and data
    pub kind: SymbolKind,
    /// Module path where this symbol is defined (e.g., ["core", "cli"])
    pub module_path: Vec<String>,
    /// Source location (if available)
    pub span: Option<Span>,
}

impl Symbol {
    /// Check if this is a builtin function
    pub fn is_builtin_function(&self) -> bool {
        matches!(&self.kind, SymbolKind::Function(f) if f.is_builtin)
    }

    /// Get the WASI import metadata if this symbol has one
    pub fn wasi_import(&self) -> Option<&WasiImport> {
        match &self.kind {
            SymbolKind::Function(f) => f.wasi_import.as_ref(),
            SymbolKind::Effect(e) => e.wasi_import.as_ref(),
            SymbolKind::Resource(r) => r.wasi_import.as_ref(),
            _ => None,
        }
    }
}

/// Target of a re-export (`pub use`)
///
/// Represents where a re-exported symbol originally comes from.
#[derive(Debug, Clone)]
pub struct ReExportTarget {
    /// Module path where the symbol is originally defined (or re-exported from)
    pub source_module: Vec<String>,
    /// Original name in the source module
    pub source_name: String,
}

/// The symbol table
///
/// Tracks all symbols organized by module and supports scoped lookups
/// for local variables within function bodies.
#[derive(Debug, Default, Clone)]
pub struct SymbolTable {
    /// All symbols in the table
    symbols: SymbolVec,
    /// Module path → symbol name → symbol id
    modules: HashMap<Vec<String>, HashMap<String, SymbolId>>,
    /// Re-exports: module path → exported name → re-export target
    reexports: HashMap<Vec<String>, HashMap<String, ReExportTarget>>,
    /// Imported symbols in the current module (name → symbol id)
    imports: HashMap<String, SymbolId>,
    /// Current scope stack for local variables (innermost scope last)
    scopes: Vec<HashMap<String, SymbolId>>,
}

impl SymbolTable {
    /// Create a new empty symbol table
    pub fn new() -> Self {
        Self::default()
    }

    /// Define a symbol in a module
    ///
    /// # Arguments
    /// * `name` - Symbol name
    /// * `kind` - Symbol kind and data
    /// * `module_path` - Module where symbol is defined
    /// * `span` - Source location (optional)
    ///
    /// # Returns
    /// The assigned symbol ID
    pub fn define(
        &mut self,
        name: &str,
        kind: SymbolKind,
        module_path: &[String],
        span: Option<Span>,
    ) -> SymbolId {
        let id = SymbolId(self.symbols.len());
        let symbol = Symbol {
            id,
            name: name.to_string(),
            kind,
            module_path: module_path.to_vec(),
            span,
        };
        self.symbols.push(symbol);

        // Register in module map
        let module = self.modules.entry(module_path.to_vec()).or_default();
        module.insert(name.to_string(), id);

        id
    }

    /// Register an imported symbol in the current module
    ///
    /// This makes the symbol accessible by its short name (without module prefix).
    pub fn register_import(&mut self, name: &str, symbol_id: SymbolId) {
        self.imports.insert(name.to_string(), symbol_id);
    }

    /// Clear all registered imports (when moving to a new module)
    pub fn clear_imports(&mut self) {
        self.imports.clear();
    }

    /// Register a re-export in a module
    ///
    /// This records that `export_name` in `module_path` is a re-export of
    /// `source_name` from `source_module`.
    ///
    /// # Arguments
    /// * `module_path` - Module where the re-export is declared
    /// * `export_name` - Name under which the symbol is re-exported
    /// * `source_module` - Module from which the symbol is imported
    /// * `source_name` - Original name in the source module
    pub fn register_reexport(
        &mut self,
        module_path: &[String],
        export_name: &str,
        source_module: &[String],
        source_name: &str,
    ) {
        let module_reexports = self.reexports.entry(module_path.to_vec()).or_default();
        module_reexports.insert(
            export_name.to_string(),
            ReExportTarget {
                source_module: source_module.to_vec(),
                source_name: source_name.to_string(),
            },
        );
    }

    /// Check if a name is re-exported from a module
    pub fn get_reexport(&self, module_path: &[String], name: &str) -> Option<&ReExportTarget> {
        self.reexports
            .get(module_path)
            .and_then(|reexports| reexports.get(name))
    }

    /// Get all struct import aliases
    ///
    /// Returns tuples of (`alias_name`, `module_path`, `original_struct_name`) for imports where:
    /// - The alias name differs from the original name
    /// - The imported symbol is a struct
    ///
    /// The `module_path` can be used to construct qualified names for collision handling.
    pub fn get_struct_aliases(&self) -> Vec<(String, Vec<String>, String)> {
        let mut aliases = Vec::new();
        for (alias_name, &symbol_id) in &self.imports {
            if let Some(symbol) = self.symbols.get(symbol_id)
                && matches!(symbol.kind, SymbolKind::Struct(_))
                && alias_name != &symbol.name
            {
                aliases.push((
                    alias_name.clone(),
                    symbol.module_path.clone(),
                    symbol.name.clone(),
                ));
            }
        }
        aliases
    }

    /// Look up a symbol by name in the current context
    ///
    /// Search order:
    /// 1. Local scopes (innermost first)
    /// 2. Imported symbols
    /// 3. Current module's symbols
    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        // Check local scopes (innermost first)
        for scope in self.scopes.iter().rev() {
            if let Some(&id) = scope.get(name) {
                return Some(&self.symbols[id]);
            }
        }

        // Check imports
        if let Some(&id) = self.imports.get(name) {
            return Some(&self.symbols[id]);
        }

        None
    }

    /// Look up a symbol in a specific module
    ///
    /// This resolves re-exports transparently, following re-export chains
    /// to find the original symbol definition.
    pub fn lookup_in_module(&self, module_path: &[String], name: &str) -> Option<&Symbol> {
        self.lookup_in_module_with_visited(module_path, name, &mut Vec::new())
    }

    /// Internal lookup that tracks visited modules to detect cycles
    fn lookup_in_module_with_visited(
        &self,
        module_path: &[String],
        name: &str,
        visited: &mut Vec<(Vec<String>, String)>,
    ) -> Option<&Symbol> {
        // Check for cycles
        let key = (module_path.to_vec(), name.to_string());
        if visited.contains(&key) {
            return None; // Cycle detected
        }
        visited.push(key);

        // First, try direct lookup in the module
        if let Some(symbol) = self
            .modules
            .get(module_path)
            .and_then(|module| module.get(name))
            .map(|&id| &self.symbols[id])
        {
            return Some(symbol);
        }

        // If not found directly, check re-exports
        if let Some(reexport) = self.get_reexport(module_path, name) {
            return self.lookup_in_module_with_visited(
                &reexport.source_module,
                &reexport.source_name,
                visited,
            );
        }

        None
    }

    /// Get a symbol by its ID
    pub fn get(&self, id: SymbolId) -> Option<&Symbol> {
        self.symbols.get(id)
    }

    /// Enter a new local scope
    pub fn enter_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Exit the current local scope
    pub fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Define a local variable in the current scope
    ///
    /// # Panics
    /// Panics if no scope is active (call `enter_scope` first)
    pub fn define_local(&mut self, name: &str, kind: SymbolKind, span: Option<Span>) -> SymbolId {
        let id = SymbolId(self.symbols.len());
        let symbol = Symbol {
            id,
            name: name.to_string(),
            kind,
            module_path: vec![], // Locals don't have module paths
            span,
        };
        self.symbols.push(symbol);

        // Add to current scope
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), id);
        } else {
            panic!("define_local called with no active scope");
        }

        id
    }

    /// Check if a symbol exists in a module
    pub fn exists_in_module(&self, module_path: &[String], name: &str) -> bool {
        self.modules
            .get(module_path)
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Get all symbols in a module
    pub fn get_module_symbols(&self, module_path: &[String]) -> Vec<&Symbol> {
        self.modules
            .get(module_path)
            .map(|module| module.values().map(|&id| &self.symbols[id]).collect())
            .unwrap_or_default()
    }

    /// Get all symbols
    pub fn all_symbols(&self) -> &[Symbol] {
        self.symbols.as_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_define_and_lookup() {
        let mut table = SymbolTable::new();

        let id = table.define(
            "println",
            SymbolKind::Function(FunctionSymbol {
                params: vec!["message".to_string()],
                return_type: None,
                effects: vec!["Stdout".to_string()],
                is_builtin: true,
                wasi_import: None,
            }),
            &["core".to_string(), "cli".to_string()],
            None,
        );

        let symbol = table.lookup_in_module(&["core".to_string(), "cli".to_string()], "println");
        assert!(symbol.is_some());
        assert_eq!(symbol.unwrap().id, id);
        assert_eq!(symbol.unwrap().name, "println");
    }

    #[test]
    fn test_import_lookup() {
        let mut table = SymbolTable::new();

        let id = table.define(
            "println",
            SymbolKind::Function(FunctionSymbol {
                params: vec![],
                return_type: None,
                effects: vec![],
                is_builtin: true,
                wasi_import: None,
            }),
            &["core".to_string(), "cli".to_string()],
            None,
        );

        // Import the symbol
        table.register_import("println", id);

        // Now it should be found via lookup
        let symbol = table.lookup("println");
        assert!(symbol.is_some());
        assert_eq!(symbol.unwrap().name, "println");
    }

    #[test]
    fn test_scoped_locals() {
        let mut table = SymbolTable::new();

        table.enter_scope();

        let id = table.define_local(
            "x",
            SymbolKind::Variable(VariableSymbol {
                is_mut: true,
                is_reactive: false,
            }),
            None,
        );

        let symbol = table.lookup("x");
        assert!(symbol.is_some());
        assert_eq!(symbol.unwrap().id, id);

        table.exit_scope();

        // After exiting scope, x should not be found
        let symbol = table.lookup("x");
        assert!(symbol.is_none());
    }

    #[test]
    fn test_nested_scopes() {
        let mut table = SymbolTable::new();

        table.enter_scope();
        table.define_local(
            "x",
            SymbolKind::Variable(VariableSymbol {
                is_mut: false,
                is_reactive: false,
            }),
            None,
        );

        table.enter_scope();
        // Shadow x in inner scope
        let inner_id = table.define_local(
            "x",
            SymbolKind::Variable(VariableSymbol {
                is_mut: true,
                is_reactive: false,
            }),
            None,
        );

        // Should find inner x
        let symbol = table.lookup("x").unwrap();
        assert_eq!(symbol.id, inner_id);
        assert!(matches!(&symbol.kind, SymbolKind::Variable(v) if v.is_mut));

        table.exit_scope();

        // Should find outer x now
        let symbol = table.lookup("x").unwrap();
        assert!(matches!(&symbol.kind, SymbolKind::Variable(v) if !v.is_mut));

        table.exit_scope();
    }

    #[test]
    fn test_struct_aliases() {
        let mut table = SymbolTable::new();

        // Define a struct in a module
        let id = table.define(
            "Point",
            SymbolKind::Struct(StructSymbol {
                fields: vec!["x".to_string(), "y".to_string()],
            }),
            &["geometry".to_string()],
            None,
        );

        // Import the struct with an alias
        table.register_import("OtherPoint", id);

        // get_struct_aliases should return the alias mapping with module path
        let aliases = table.get_struct_aliases();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].0, "OtherPoint"); // alias name
        assert_eq!(aliases[0].1, vec!["geometry".to_string()]); // module path
        assert_eq!(aliases[0].2, "Point"); // original struct name
    }

    #[test]
    fn test_struct_aliases_same_name() {
        let mut table = SymbolTable::new();

        // Define a struct in a module
        let id = table.define(
            "Point",
            SymbolKind::Struct(StructSymbol {
                fields: vec!["x".to_string(), "y".to_string()],
            }),
            &["geometry".to_string()],
            None,
        );

        // Import the struct without an alias (same name)
        table.register_import("Point", id);

        // get_struct_aliases should NOT return same-name imports
        let aliases = table.get_struct_aliases();
        assert_eq!(aliases.len(), 0);
    }
}
