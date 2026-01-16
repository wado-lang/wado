//! Builtin function registry for compiler intrinsics
//!
//! This module collects function signatures from lib/core/builtin.wado
//! and provides type information for code generation.

use std::collections::HashMap;

use crate::ast::{Function, Type};

/// Information about a builtin function
#[derive(Debug, Clone)]
pub struct BuiltinFunctionInfo {
    /// Function name (e.g., "stream_new")
    pub name: String,
    /// Canonical name from #[canonical("...")] attribute (e.g., "stream-new")
    /// None means this builtin compiles to Wasm instructions directly
    pub canonical_name: Option<String>,
    /// Parameter types
    pub params: Vec<(String, Type)>,
    /// Return type (None for void/diverging functions)
    pub return_type: Option<Type>,
    /// Whether this function diverges (returns !)
    pub diverges: bool,
}

/// Registry of builtin functions for code generation
///
/// Collects function signatures from core:builtin and provides:
/// - Type lookup for builtin:: calls
/// - Parameter validation (future)
#[derive(Debug, Default)]
pub struct BuiltinRegistry {
    /// function_name -> function info
    functions: HashMap<String, BuiltinFunctionInfo>,
}

impl BuiltinRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the registry from the embedded stdlib
    ///
    /// Parses lib/core/builtin.wado and registers all function signatures.
    pub fn build_from_stdlib() -> Self {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::stdlib;

        let source = stdlib::CORE_BUILTIN;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error in core:builtin");
        let mut parser = Parser::new(tokens);
        let module = parser.parse().expect("parser error in core:builtin");

        let mut registry = Self::new();
        for item in &module.items {
            if let crate::ast::Item::Function(func) = item {
                registry.register(func);
            }
        }
        registry
    }

    /// Register a builtin function from a parsed function declaration
    pub fn register(&mut self, func: &Function) {
        let params: Vec<(String, Type)> = func
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();

        let (return_type, diverges) = if let Some(ref ty) = func.return_type {
            // Check if it's the never type (!)
            if matches!(ty, Type::Named(named) if named.name == "!") {
                (None, true)
            } else {
                (Some(ty.clone()), false)
            }
        } else {
            (None, false)
        };

        // Extract canonical name from #[canonical("...")] attribute
        let canonical_name = func
            .attrs
            .iter()
            .find(|a| a.name == "canonical")
            .and_then(|a| a.args.clone());

        let info = BuiltinFunctionInfo {
            name: func.name.clone(),
            canonical_name,
            params,
            return_type,
            diverges,
        };

        self.functions.insert(func.name.clone(), info);
    }

    /// Get function info by name
    pub fn get(&self, name: &str) -> Option<&BuiltinFunctionInfo> {
        self.functions.get(name)
    }

    /// Get the return type of a builtin function
    pub fn get_return_type(&self, name: &str) -> Option<&Type> {
        self.functions
            .get(name)
            .and_then(|f| f.return_type.as_ref())
    }

    /// Check if a builtin function diverges (returns !)
    pub fn diverges(&self, name: &str) -> bool {
        self.functions
            .get(name)
            .map(|f| f.diverges)
            .unwrap_or(false)
    }

    /// Check if a function is registered as a builtin
    pub fn is_builtin(&self, name: &str) -> bool {
        self.functions.contains_key(name)
    }

    /// Get the number of registered builtins
    pub fn len(&self) -> usize {
        self.functions.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    /// Iterate over all registered builtin functions
    pub fn iter(&self) -> impl Iterator<Item = &BuiltinFunctionInfo> {
        self.functions.values()
    }

    /// Iterate over builtins that are imported (have #[canonical("...")] attribute)
    pub fn imported_builtins(&self) -> impl Iterator<Item = &BuiltinFunctionInfo> {
        self.functions
            .values()
            .filter(|f| f.canonical_name.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{NamedType, Param, SelfKind};
    use crate::token::Span;

    fn make_span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = BuiltinRegistry::new();

        let func = Function {
            name: "stream_new".to_string(),
            is_pub: false,
            type_params: vec![],
            attrs: vec![],
            params: vec![],
            return_type: Some(Type::Named(NamedType {
                name: "i64".to_string(),
                span: make_span(),
            })),
            effects: vec![],
            body: None,
            span: make_span(),
        };

        registry.register(&func);

        assert!(registry.is_builtin("stream_new"));
        assert!(!registry.is_builtin("unknown"));

        let info = registry.get("stream_new").unwrap();
        assert_eq!(info.name, "stream_new");
        assert!(info.params.is_empty());
        assert!(!info.diverges);
    }

    #[test]
    fn test_diverging_function() {
        let mut registry = BuiltinRegistry::new();

        let func = Function {
            name: "unreachable".to_string(),
            is_pub: false,
            type_params: vec![],
            attrs: vec![],
            params: vec![],
            return_type: Some(Type::Named(NamedType {
                name: "!".to_string(),
                span: make_span(),
            })),
            effects: vec![],
            body: None,
            span: make_span(),
        };

        registry.register(&func);

        assert!(registry.diverges("unreachable"));
        assert!(!registry.diverges("stream_new"));
    }

    #[test]
    fn test_function_with_params() {
        let mut registry = BuiltinRegistry::new();

        let func = Function {
            name: "stream_write".to_string(),
            is_pub: false,
            type_params: vec![],
            attrs: vec![],
            params: vec![
                Param {
                    name: "tx".to_string(),
                    ty: Type::Named(NamedType {
                        name: "i32".to_string(),
                        span: make_span(),
                    }),
                    self_kind: SelfKind::None,
                    span: make_span(),
                },
                Param {
                    name: "ptr".to_string(),
                    ty: Type::Named(NamedType {
                        name: "i32".to_string(),
                        span: make_span(),
                    }),
                    self_kind: SelfKind::None,
                    span: make_span(),
                },
                Param {
                    name: "len".to_string(),
                    ty: Type::Named(NamedType {
                        name: "i32".to_string(),
                        span: make_span(),
                    }),
                    self_kind: SelfKind::None,
                    span: make_span(),
                },
            ],
            return_type: Some(Type::Named(NamedType {
                name: "i32".to_string(),
                span: make_span(),
            })),
            effects: vec![],
            body: None,
            span: make_span(),
        };

        registry.register(&func);

        let info = registry.get("stream_write").unwrap();
        assert_eq!(info.params.len(), 3);
        assert_eq!(info.params[0].0, "tx");
        assert_eq!(info.params[1].0, "ptr");
        assert_eq!(info.params[2].0, "len");
    }

    #[test]
    fn test_parse_builtin_wado() {
        use crate::ast::Item;
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::stdlib::CORE_BUILTIN;

        // Parse the actual builtin.wado
        let mut lexer = Lexer::new(CORE_BUILTIN);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        let module = parser.parse().expect("parser error");

        // Build registry
        let mut registry = BuiltinRegistry::new();
        for item in &module.items {
            if let Item::Function(func) = item {
                registry.register(func);
            }
        }

        // Verify key builtins are registered
        assert!(registry.is_builtin("stream_new"), "stream_new not found");
        assert!(
            registry.is_builtin("stream_write"),
            "stream_write not found"
        );
        assert!(registry.is_builtin("array_len"), "array_len not found");
        assert!(registry.is_builtin("unreachable"), "unreachable not found");

        // Verify return types
        let stream_new = registry.get("stream_new").unwrap();
        assert!(
            stream_new.return_type.is_some(),
            "stream_new should have return type"
        );

        let stream_write = registry.get("stream_write").unwrap();
        assert_eq!(stream_write.params.len(), 3);

        // Verify unreachable diverges
        assert!(registry.diverges("unreachable"));

        // Print registry size for debugging
        eprintln!("Registry has {} builtins", registry.len());
    }
}
