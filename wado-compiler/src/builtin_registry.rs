//! Builtin function registry for compiler intrinsics
//!
//! This module collects function signatures from lib/core/builtin.wado
//! and provides type information for code generation.

use crate::hashmap::IndexMap;
use std::cell::RefCell;

use crate::ast::{CmBoundary, Function, Type};
use crate::compiler_item::CompilerItem;
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
#[derive(Debug, Default, Clone)]
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
        use crate::parser::Parser;
        use crate::stdlib;

        static PARSED_MODULE: OnceLock<Module> = OnceLock::new();

        let module = PARSED_MODULE.get_or_init(|| {
            let source =
                stdlib::get_stdlib_module("core:builtin").expect("core:builtin is bundled");
            let r = crate::lexer::lex(source);
            assert!(
                r.errors.is_empty(),
                "lexer error in core:builtin: {:?}",
                r.errors
            );
            let mut parser = Parser::new(r.tokens);
            parser.parse_strict().expect("parser error in core:builtin")
        });

        let mut registry = Self::new();
        for item in &module.items {
            if let crate::ast::Item::Function(func) = item {
                registry.register(func, type_table);
            }
        }
        registry
    }

    /// Register every `#[canonical(...)]` no-body function declared in
    /// `module` with the registry. Used to fold in functions from
    /// loader-synthesized wasm-asset modules
    /// (`ModuleSource::Wasm`) so calls into a wasm asset's exports go
    /// through the same `TirImport` path as `core:builtin` declarations.
    pub fn register_wasm_module(
        &mut self,
        module: &crate::ast::Module,
        type_table: &RefCell<TypeTable>,
    ) {
        for item in &module.items {
            if let crate::ast::Item::Function(func) = item
                && func.body.is_none()
                && func.attrs.iter().any(is_canonical_builtin)
            {
                self.register(func, type_table);
            }
        }
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

        // Extract canonical info from `#[canonical("namespace", "name")]`.
        // The parser is the single source of truth for this attribute's shape;
        // a malformed form (`#[canonical(...)]` without two string arguments)
        // never reaches `CmBoundary::Canonical`, so the function falls back to
        // the Wasm-instruction default below.
        let (namespace, canonical_name) = func
            .attrs
            .iter()
            .find_map(|a| match &a.cm_boundary {
                Some(CmBoundary::Canonical { namespace, name }) => {
                    Some((namespace.clone(), Some(name.clone())))
                }
                Some(CmBoundary::Import(_) | CmBoundary::WorldImport(_) | CmBoundary::Name(_))
                | None => None,
            })
            .unwrap_or_else(|| ("wasi".to_string(), None));

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
    /// Handles primitive types, type parameters, and `Array<T>`.
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
                if let Some(id) = TypeTable::primitive_by_name(&named.name) {
                    return id;
                }
                // `i128` / `u128` are prelude struct declarations, which
                // `core:builtin` imports like any other module's type — these
                // arms are what honour that import, since the name is matched
                // before the import list is consulted.
                match named.name.as_str() {
                    "i128" => type_table
                        .borrow_mut()
                        .make_compiler_struct(CompilerItem::I128),
                    "u128" => type_table
                        .borrow_mut()
                        .make_compiler_struct(CompilerItem::U128),
                    _ => TypeTable::UNIT, // Unknown type defaults to UNIT
                }
            }
            // `Array<T>` is the user-facing spelling of the raw GC array
            // builtin; the builtin module's own signatures use it.
            Type::Generic(g) if g.name == TypeTable::ARRAY_TYPE_NAME => {
                if let Some(first_arg) = g.args.first() {
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
            Type::Reference(inner) => {
                let inner_id = Self::resolve_type(inner, type_params, type_table);
                type_table.borrow_mut().make_ref(inner_id)
            }
            Type::MutReference(inner) => {
                let inner_id = Self::resolve_type(inner, type_params, type_table);
                type_table.borrow_mut().make_mut_ref(inner_id)
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

/// Returns true if the attribute is a well-formed `#[canonical(ns, name)]` that
/// marks a CM canonical built-in.
fn is_canonical_builtin(attr: &crate::ast::Attribute) -> bool {
    matches!(attr.cm_boundary, Some(CmBoundary::Canonical { .. }))
}
