//! World registry for Component Model world definitions
//!
//! This module collects world definitions from lib/wasi/*.wado
//! and provides export signature information for code generation.

use indexmap::IndexMap;

use crate::ast::{Type, WorldDecl, WorldExport};

/// Information about a world export function
#[derive(Debug, Clone)]
pub struct WorldExportInfo {
    /// Function name (e.g., "run")
    pub name: String,
    /// Whether this is an async function
    pub is_async: bool,
    /// Parameter types
    pub params: Vec<(String, Type)>,
    /// Return type (if any)
    pub return_type: Option<Type>,
}

impl WorldExportInfo {
    /// Create from a parsed `WorldExport`
    pub fn from_ast(export: &WorldExport) -> Self {
        let params = export
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();

        Self {
            name: export.name.clone(),
            is_async: export.is_async,
            params,
            return_type: export.return_type.clone(),
        }
    }

    /// Check if this export returns an HTTP response.
    ///
    /// Returns true if the return type is `Result<Response, ErrorCode>`,
    /// which indicates this is an HTTP handler export.
    pub fn returns_http_response(&self) -> bool {
        let Some(return_type) = &self.return_type else {
            return false;
        };

        if let Type::Generic(generic) = return_type
            && generic.name == "Result"
            && generic.args.len() == 2
            && let Type::Named(ok_type) = &generic.args[0]
        {
            return ok_type.name == "Response";
        }
        false
    }
}

/// Information about a world definition
#[derive(Debug, Clone)]
pub struct WorldInfo {
    /// World name (e.g., "Command")
    pub name: String,
    /// Exported functions
    pub exports: Vec<WorldExportInfo>,
}

impl WorldInfo {
    /// Check if this world has any async export.
    pub fn has_async_export(&self) -> bool {
        self.exports.iter().any(|e| e.is_async)
    }

    /// Check if this world has an HTTP handler export.
    ///
    /// Returns true if any export returns `Result<Response, ErrorCode>`.
    pub fn has_http_handler_export(&self) -> bool {
        self.exports
            .iter()
            .any(WorldExportInfo::returns_http_response)
    }
}

/// Registry of world definitions for code generation
///
/// Collects world definitions from wasi/*.wado and provides:
/// - Export signature lookup for component generation
#[derive(Debug, Clone, Default)]
pub struct WorldRegistry {
    /// `world_name` -> world info
    worlds: IndexMap<String, WorldInfo>,
}

impl WorldRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a world from a parsed world declaration
    pub fn register(&mut self, world: &WorldDecl) {
        let exports = world
            .exports
            .iter()
            .map(WorldExportInfo::from_ast)
            .collect();

        let info = WorldInfo {
            name: world.name.clone(),
            exports,
        };

        self.worlds.insert(world.name.clone(), info);
    }

    /// Get world info by name
    pub fn get(&self, name: &str) -> Option<&WorldInfo> {
        self.worlds.get(name)
    }

    /// Get an export from a specific world
    pub fn get_export(&self, world_name: &str, export_name: &str) -> Option<&WorldExportInfo> {
        self.worlds
            .get(world_name)
            .and_then(|w| w.exports.iter().find(|e| e.name == export_name))
    }

    /// Check if a world is registered
    pub fn has_world(&self, name: &str) -> bool {
        self.worlds.contains_key(name)
    }

    /// Get the number of registered worlds
    pub fn len(&self) -> usize {
        self.worlds.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.worlds.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Span;

    fn make_span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = WorldRegistry::new();

        let world = WorldDecl {
            name: "Command".to_string(),
            imports: vec![],
            exports: vec![WorldExport {
                name: "run".to_string(),
                is_async: true,
                params: vec![],
                return_type: Some(Type::Generic(crate::ast::GenericType {
                    name: "Result".to_string(),
                    args: vec![Type::Tuple(vec![]), Type::Tuple(vec![])],
                    span: make_span(),
                })),
                span: make_span(),
            }],
            span: make_span(),
        };

        registry.register(&world);

        assert!(registry.has_world("Command"));
        assert!(!registry.has_world("Unknown"));

        let info = registry.get("Command").unwrap();
        assert_eq!(info.name, "Command");
        assert_eq!(info.exports.len(), 1);

        let run_export = registry.get_export("Command", "run").unwrap();
        assert_eq!(run_export.name, "run");
        assert!(run_export.is_async);
        assert!(run_export.params.is_empty());
        assert!(run_export.return_type.is_some());
    }

    #[test]
    fn test_parse_cli_wado() {
        use crate::ast::Item;
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::stdlib::WASI_CLI;

        // Parse the actual cli.wado
        let mut lexer = Lexer::new(WASI_CLI);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        let module = parser.parse().expect("parser error");

        // Build registry
        let mut registry = WorldRegistry::new();
        for item in &module.items {
            if let Item::World(world) = item {
                registry.register(world);
            }
        }

        // Verify Command world is registered
        assert!(registry.has_world("Command"), "Command world not found");

        // Verify run export
        let run_export = registry
            .get_export("Command", "run")
            .expect("run export not found");
        assert!(run_export.is_async, "run should be async");
        assert!(run_export.params.is_empty(), "run should have no params");
    }
}
