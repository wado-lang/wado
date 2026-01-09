//! Symbol table for name resolution
//!
//! The symbol table tracks all definitions (functions, types, effects, etc.)
//! and their metadata. It supports module namespacing and scoped lookups.

use std::collections::HashMap;

use crate::ast::WasiImport;
use crate::token::Span;

/// Unique identifier for a symbol in the table
pub type SymbolId = usize;

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
    /// Whether this is a builtin function (e.g., println from core::cli)
    pub is_builtin: bool,
    /// WASI import metadata if this function maps to a WASI function
    pub wasi_import: Option<WasiImport>,
}

/// Effect symbol data
#[derive(Debug, Clone)]
pub struct EffectSymbol {
    /// Method names defined in this effect
    pub methods: Vec<String>,
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
    /// Variant names
    pub variants: Vec<String>,
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
            SymbolKind::Resource(r) => r.wasi_import.as_ref(),
            _ => None,
        }
    }
}

/// The symbol table
///
/// Tracks all symbols organized by module and supports scoped lookups
/// for local variables within function bodies.
#[derive(Debug, Default)]
pub struct SymbolTable {
    /// All symbols in the table
    symbols: Vec<Symbol>,
    /// Module path → symbol name → symbol id
    modules: HashMap<Vec<String>, HashMap<String, SymbolId>>,
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
        let id = self.symbols.len();
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
    pub fn lookup_in_module(&self, module_path: &[String], name: &str) -> Option<&Symbol> {
        self.modules
            .get(module_path)
            .and_then(|module| module.get(name))
            .map(|&id| &self.symbols[id])
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
        let id = self.symbols.len();
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
        &self.symbols
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
}
