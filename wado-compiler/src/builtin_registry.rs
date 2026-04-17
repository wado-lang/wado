//! Builtin function registry for compiler intrinsics
//!
//! This module collects function signatures from lib/core/builtin.wado
//! and provides type information for code generation.

use crate::hashmap::IndexMap;
use std::cell::RefCell;

use crate::ast::{Function, Type};
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Information about a builtin function
#[derive(Debug, Clone)]
pub struct BuiltinFunctionInfo {
    /// Function name (e.g., "`stream_new`")
    pub name: String,
    /// Canonical name from #[canonical("...")] attribute (e.g., "stream-new")
    /// None means this builtin compiles to Wasm instructions directly
    pub canonical_name: Option<String>,
    /// Import namespace from #[namespace("...")] attribute (default: "wasi")
    /// Only relevant for functions with `canonical_name`
    pub namespace: String,
    /// Generic type parameter names (e.g., ["T"] for `fn array_new<T>`)
    pub type_params: Vec<String>,
    /// Parameter types (resolved to `TypeIds`)
    pub params: Vec<(String, TypeId)>,
    /// Return type (resolved to `TypeId`, UNIT for void functions)
    pub return_type: TypeId,
    /// Whether this function diverges (returns !)
    pub diverges: bool,
}

/// Registry of builtin functions for code generation
///
/// Collects function signatures from core:builtin and provides:
/// - Type lookup for `builtin::` calls
/// - Parameter validation (future)
#[derive(Debug, Default)]
pub struct BuiltinRegistry {
    /// `function_name` -> function info
    functions: IndexMap<String, BuiltinFunctionInfo>,
}

impl BuiltinRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the registry from the embedded stdlib
    ///
    /// Parses lib/core/builtin.wado and registers all function signatures.
    /// Types are resolved to `TypeId`s using the provided `TypeTable`.
    pub fn build_from_stdlib(type_table: &RefCell<TypeTable>) -> Self {
        use std::sync::OnceLock;

        use crate::ast::Module;
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::stdlib;

        static PARSED_MODULE: OnceLock<Module> = OnceLock::new();

        let module = PARSED_MODULE.get_or_init(|| {
            let source = stdlib::CORE_BUILTIN;
            let mut lexer = Lexer::new(source);
            let tokens = lexer.tokenize().expect("lexer error in core:builtin");
            let mut parser = Parser::new(tokens);
            parser.parse().expect("parser error in core:builtin")
        });

        let mut registry = Self::new();
        for item in &module.items {
            if let crate::ast::Item::Function(func) = item {
                registry.register(func, type_table);
            }
        }
        registry
    }

    /// Register a builtin function from a parsed function declaration
    fn register(&mut self, func: &Function, type_table: &RefCell<TypeTable>) {
        let type_params: Vec<String> = func.type_params.iter().map(|p| p.name.clone()).collect();

        let params: Vec<(String, TypeId)> = func
            .params
            .iter()
            .map(|p| {
                let type_id = Self::resolve_type(&p.ty, &type_params, type_table);
                (p.name.clone(), type_id)
            })
            .collect();

        let (return_type, diverges) = if let Some(ref ty) = func.return_type {
            // Check if it's the never type (!)
            if matches!(ty, Type::Named(named) if named.name == "!") {
                (TypeTable::NEVER, true)
            } else {
                (Self::resolve_type(ty, &type_params, type_table), false)
            }
        } else {
            (TypeTable::UNIT, false)
        };

        // Extract canonical info from #[canonical("namespace", "name")] attribute
        // args[0] = namespace, args[1] = canonical name
        let canonical_attr = func.attrs.iter().find(|a| a.name == "canonical");
        let (namespace, canonical_name) = if let Some(attr) = canonical_attr {
            if attr.args.len() >= 2 {
                // New format: #[canonical("wasi", "stream-new")]
                (
                    attr.args[0].as_str().to_string(),
                    Some(attr.args[1].as_str().to_string()),
                )
            } else if attr.args.len() == 1 {
                // Legacy single-arg format not supported anymore
                panic!(
                    "Invalid #[canonical] attribute: expected 2 arguments (namespace, name), got 1"
                );
            } else {
                // No arguments - this is an error
                panic!("Invalid #[canonical] attribute: expected 2 arguments (namespace, name)");
            }
        } else {
            // No canonical attribute - not an imported builtin
            ("wasi".to_string(), None)
        };

        let info = BuiltinFunctionInfo {
            name: func.name.clone(),
            canonical_name,
            namespace,
            type_params,
            params,
            return_type,
            diverges,
        };

        self.functions.insert(func.name.clone(), info);
    }

    /// Resolve an AST Type to a `TypeId`
    ///
    /// Handles primitive types, type parameters, and `builtin::array`<T>.
    fn resolve_type(ty: &Type, type_params: &[String], type_table: &RefCell<TypeTable>) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check if it's a type parameter
                if let Some(index) = type_params.iter().position(|p| p == &named.name) {
                    return type_table.borrow_mut().intern(ResolvedType::TypeParam {
                        index: index as u32,
                        name: named.name.clone(),
                    });
                }
                // Otherwise, check for primitive types
                match named.name.as_str() {
                    "i8" => TypeTable::I8,
                    "i16" => TypeTable::I16,
                    "i32" => TypeTable::I32,
                    "i64" => TypeTable::I64,
                    "i128" => TypeTable::I128,
                    "u8" => TypeTable::U8,
                    "u16" => TypeTable::U16,
                    "u32" => TypeTable::U32,
                    "u64" => TypeTable::U64,
                    "u128" => TypeTable::U128,
                    "f32" => TypeTable::F32,
                    "f64" => TypeTable::F64,
                    "bool" => TypeTable::BOOL,
                    "char" => TypeTable::CHAR,
                    "v128" => TypeTable::V128,
                    "!" => TypeTable::NEVER,
                    _ => TypeTable::UNIT, // Unknown type defaults to UNIT
                }
            }
            Type::NamespacedGeneric(ng) if ng.namespace == "builtin" && ng.name == "array" => {
                // builtin::array<T> -> BuiltinArray(T)
                if let Some(first_arg) = ng.args.first() {
                    let element_type = Self::resolve_type(first_arg, type_params, type_table);
                    type_table
                        .borrow_mut()
                        .intern(ResolvedType::BuiltinArray(element_type))
                } else {
                    TypeTable::UNIT
                }
            }
            Type::Tuple(elements) => {
                let element_types: Vec<TypeId> = elements
                    .iter()
                    .map(|t| Self::resolve_type(t, type_params, type_table))
                    .collect();
                type_table.borrow_mut().make_tuple(element_types)
            }
            _ => TypeTable::UNIT, // Other types default to UNIT
        }
    }

    /// Get function info by name
    pub fn get(&self, name: &str) -> Option<&BuiltinFunctionInfo> {
        self.functions.get(name)
    }

    /// Get function info by canonical name (e.g., "stream-new", "realloc")
    pub fn get_by_canonical(&self, canonical_name: &str) -> Option<&BuiltinFunctionInfo> {
        self.functions
            .values()
            .find(|f| f.canonical_name.as_deref() == Some(canonical_name))
    }

    /// Get the return type of a builtin function
    pub fn get_return_type(&self, name: &str) -> Option<TypeId> {
        self.functions.get(name).map(|f| f.return_type)
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

    fn make_type_table() -> RefCell<TypeTable> {
        RefCell::new(TypeTable::new())
    }

    #[test]
    fn test_register_and_get() {
        let type_table = make_type_table();
        let mut registry = BuiltinRegistry::new();

        let func = Function {
            id: crate::ast::AstId::SYNTHETIC,
            name: "stream_new".to_string(),
            name_span: make_span(),
            is_pub: false,
            is_export: false,
            is_async: false,
            type_params: vec![],
            attrs: vec![],
            params: vec![],
            return_type: Some(Type::Named(NamedType {
                name: "i64".to_string(),
                span: make_span(),
            })),
            effects: vec![],
            stores: vec![],
            body: None,
            span: make_span(),
        };

        registry.register(&func, &type_table);

        assert!(registry.is_builtin("stream_new"));
        assert!(!registry.is_builtin("unknown"));

        let info = registry.get("stream_new").unwrap();
        assert_eq!(info.name, "stream_new");
        assert!(info.params.is_empty());
        assert!(!info.diverges);
    }

    #[test]
    fn test_diverging_function() {
        let type_table = make_type_table();
        let mut registry = BuiltinRegistry::new();

        let func = Function {
            id: crate::ast::AstId::SYNTHETIC,
            name: "unreachable".to_string(),
            name_span: make_span(),
            is_pub: false,
            is_export: false,
            is_async: false,
            type_params: vec![],
            attrs: vec![],
            params: vec![],
            return_type: Some(Type::Named(NamedType {
                name: "!".to_string(),
                span: make_span(),
            })),
            effects: vec![],
            stores: vec![],
            body: None,
            span: make_span(),
        };

        registry.register(&func, &type_table);

        assert!(registry.diverges("unreachable"));
        assert!(!registry.diverges("stream_new"));
    }

    #[test]
    fn test_function_with_params() {
        let type_table = make_type_table();
        let mut registry = BuiltinRegistry::new();

        let func = Function {
            id: crate::ast::AstId::SYNTHETIC,
            name: "stream_write".to_string(),
            name_span: make_span(),
            is_pub: false,
            is_export: false,
            is_async: false,
            type_params: vec![],
            attrs: vec![],
            params: vec![
                Param {
                    id: crate::ast::AstId::SYNTHETIC,
                    name: "tx".to_string(),
                    name_span: make_span(),
                    ty: Type::Named(NamedType {
                        name: "i32".to_string(),
                        span: make_span(),
                    }),
                    self_kind: SelfKind::None,
                    is_mut: false,
                    span: make_span(),
                },
                Param {
                    id: crate::ast::AstId::SYNTHETIC,
                    name: "ptr".to_string(),
                    name_span: make_span(),
                    ty: Type::Named(NamedType {
                        name: "i32".to_string(),
                        span: make_span(),
                    }),
                    self_kind: SelfKind::None,
                    is_mut: false,
                    span: make_span(),
                },
                Param {
                    id: crate::ast::AstId::SYNTHETIC,
                    name: "len".to_string(),
                    name_span: make_span(),
                    ty: Type::Named(NamedType {
                        name: "i32".to_string(),
                        span: make_span(),
                    }),
                    self_kind: SelfKind::None,
                    is_mut: false,
                    span: make_span(),
                },
            ],
            return_type: Some(Type::Named(NamedType {
                name: "i32".to_string(),
                span: make_span(),
            })),
            effects: vec![],
            stores: vec![],
            body: None,
            span: make_span(),
        };

        registry.register(&func, &type_table);

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

        // Build registry with type table
        let type_table = make_type_table();
        let mut registry = BuiltinRegistry::new();
        for item in &module.items {
            if let Item::Function(func) = item {
                registry.register(func, &type_table);
            }
        }

        // Verify key builtins are registered
        assert!(registry.is_builtin("array_len"), "array_len not found");
        assert!(registry.is_builtin("unreachable"), "unreachable not found");
        assert!(registry.is_builtin("realloc"), "realloc not found");
        assert!(
            registry.is_builtin("call_indirect_stdout_write_via_stream"),
            "call_indirect_stdout_write_via_stream not found"
        );

        // Verify return types
        let realloc = registry.get("realloc").unwrap();
        assert_ne!(
            realloc.return_type,
            TypeTable::UNIT,
            "realloc should have non-unit return type"
        );

        // Verify unreachable diverges
        assert!(registry.diverges("unreachable"));

        // Print registry size for debugging
        eprintln!("Registry has {} builtins", registry.len());
    }
}
