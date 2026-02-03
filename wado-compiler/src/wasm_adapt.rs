//! Wasm Adapt Phase - Prepares TIR for WebAssembly Component Model code generation
//!
//! This phase runs between optimize and codegen:
//! ```text
//! lower -> optimize -> wasm_adapt -> codegen
//! ```
//!
//! Responsibilities:
//! 1. CM boundary analysis - Analyze export signatures to determine required glue code
//! 2. Attach `CmExportInfo` to `TirFunctions` that are world exports
//! 3. Compute scratch locals needed for CM operations
//! 4. Analyze WASI function return types to determine required CM converters
//!
//! Design principles:
//! - Metadata over TIR for glue code: CM glue uses low-level Wasm operations that
//!   don't map cleanly to TIR, so we use metadata to tell codegen what to generate
//! - Keep codegen simple: codegen should just convert TIR to Wasm without
//!   needing to analyze world definitions or export signatures
//! - Centralize CM analysis: All CM-related type analysis should be in this module
//!   to avoid duplication between optimize and codegen phases

use crate::ast::Type;
use crate::project::Project;

/// Wasm value type for CM scratch locals
///
/// This mirrors `wasm_encoder::ValType` but is simpler and doesn't require
/// the `wasm_encoder` dependency in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmValType {
    I32,
    I64,
    F32,
    F64,
    /// Nullable anyref - used for storing GC objects
    AnyRef,
}

/// A scratch local variable needed for CM glue code
#[derive(Debug, Clone)]
pub struct CmScratchLocal {
    /// Local variable name (for debugging in WAT output)
    pub name: String,
    /// Wasm value type
    pub val_type: CmValType,
}

/// CM export information attached to `TirFunction`
///
/// This metadata tells codegen how to generate Component Model glue code
/// for a world export function.
#[derive(Debug, Clone, Default)]
pub struct CmExportInfo {
    /// Whether this is an async export (from world definition)
    pub is_async: bool,
    /// Whether this export returns Result<Response, `ErrorCode`> (HTTP handler)
    pub is_http_handler: bool,
    /// Scratch locals needed for CM glue code
    pub scratch_locals: Vec<CmScratchLocal>,
    /// CM functions that must be imported (e.g., "task-return", "future-new")
    pub required_imports: Vec<String>,
}

impl CmExportInfo {
    /// Create `CmExportInfo` for an async export
    pub fn async_export(is_http_handler: bool) -> Self {
        let mut required_imports = vec!["task-return".to_string()];
        let mut scratch_locals = Vec::new();

        if is_http_handler {
            // HTTP handler needs additional imports for response creation
            required_imports.extend([
                "future-new".to_string(),
                "future-write".to_string(),
                "http-fields-constructor".to_string(),
                "http-response-new".to_string(),
            ]);

            // Pre-computed scratch locals for HTTP response creation
            // These are the locals needed for CM glue code in codegen
            scratch_locals.extend([
                CmScratchLocal {
                    name: "_http_future".to_string(),
                    val_type: CmValType::I64,
                },
                CmScratchLocal {
                    name: "_trailers_rx".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_trailers_tx".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_headers_handle".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_write_result".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_result_disc".to_string(),
                    val_type: CmValType::I32,
                },
                CmScratchLocal {
                    name: "_response_handle".to_string(),
                    val_type: CmValType::I32,
                },
            ]);
        }

        Self {
            is_async: true,
            is_http_handler,
            scratch_locals,
            required_imports,
        }
    }

    /// Create `CmExportInfo` for a sync export
    pub fn sync_export() -> Self {
        Self {
            is_async: false,
            is_http_handler: false,
            scratch_locals: Vec::new(),
            required_imports: Vec::new(),
        }
    }
}

// =============================================================================
// CM Converter Analysis
// =============================================================================

/// Identifies which CM converter function is needed for a given return type.
///
/// This centralizes the CM type analysis that determines what converter functions
/// are needed to convert Component Model representations (in linear memory) to
/// Wado's GC-based types.
///
/// Returns `None` if no converter is needed, or `Some(converter_name)` where
/// `converter_name` is the function name in `core/internal` (e.g., `"cm_list_string_to_array"`).
#[must_use]
pub fn get_cm_converter_for_type(return_type: &Type) -> Option<&'static str> {
    match return_type {
        // Array<String> -> cm_list_string_to_array
        Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
            if matches!(&g.args[0], Type::Named(n) if n.name == "String") {
                return Some("cm_list_string_to_array");
            }
            // Array<u8> -> cm_list_u8_to_array
            if matches!(&g.args[0], Type::Named(n) if n.name == "u8") {
                return Some("cm_list_u8_to_array");
            }
            // Array<[String, String]> -> cm_list_tuple_string_string_to_array
            if let Type::Tuple(tuple_types) = &g.args[0]
                && tuple_types.len() == 2
                && matches!(&tuple_types[0], Type::Named(n) if n.name == "String")
                && matches!(&tuple_types[1], Type::Named(n) if n.name == "String")
            {
                return Some("cm_list_tuple_string_string_to_array");
            }
            // Tuple<String, String> syntax (alternative to [String, String])
            if let Type::Generic(inner_g) = &g.args[0]
                && inner_g.name == "Tuple"
                && inner_g.args.len() == 2
                && matches!(&inner_g.args[0], Type::Named(n) if n.name == "String")
                && matches!(&inner_g.args[1], Type::Named(n) if n.name == "String")
            {
                return Some("cm_list_tuple_string_string_to_array");
            }
            None
        }
        // Option<String> -> cm_option_string_to_option
        Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
            if matches!(&g.args[0], Type::Named(n) if n.name == "String") {
                return Some("cm_option_string_to_option");
            }
            None
        }
        _ => None,
    }
}

/// Types of CM converters that may be needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmConverterKind {
    /// `cm_list_string_to_array` converter
    ListString,
    /// `cm_list_u8_to_array` converter
    ListU8,
    /// `cm_list_tuple_string_string_to_array` converter
    ListTupleString,
    /// `cm_option_string_to_option` converter
    OptionString,
}

/// Information about what CM converters are needed for a set of WASI functions.
#[derive(Debug, Clone, Default)]
pub struct CmConverterRequirements {
    /// Set of required converters
    needed: std::collections::HashSet<CmConverterKind>,
}

impl CmConverterRequirements {
    /// Analyze a return type and update requirements.
    pub fn analyze_type(&mut self, return_type: &Type) {
        if let Some(converter) = get_cm_converter_for_type(return_type) {
            let kind = match converter {
                "cm_list_string_to_array" => CmConverterKind::ListString,
                "cm_list_u8_to_array" => CmConverterKind::ListU8,
                "cm_list_tuple_string_string_to_array" => CmConverterKind::ListTupleString,
                "cm_option_string_to_option" => CmConverterKind::OptionString,
                _ => return,
            };
            self.needed.insert(kind);
        }
    }

    /// Check if any converters are needed.
    #[must_use]
    pub fn any_needed(&self) -> bool {
        !self.needed.is_empty()
    }

    /// Check if a specific converter is needed.
    #[must_use]
    pub fn needs(&self, kind: CmConverterKind) -> bool {
        self.needed.contains(&kind)
    }
}

/// Run the `wasm_adapt` phase on a Project
///
/// This analyzes world exports and attaches `CmExportInfo` to the corresponding
/// `TirFunctions`. The Project is modified in place.
pub fn wasm_adapt(mut project: Project) -> Project {
    // Look up the target world from the registry in Project
    let world_info = project.world_registry.get(&project.target_world).cloned();

    if let Some(world_info) = world_info {
        // Update project's HTTP handler flag based on world analysis
        project.has_http_handler_export = world_info.has_http_handler_export();

        // Analyze each world export and attach CmExportInfo to corresponding TirFunction
        for export in &world_info.exports {
            let is_http_handler = export.returns_http_response();
            let cm_export_info = if export.is_async {
                CmExportInfo::async_export(is_http_handler)
            } else {
                CmExportInfo::sync_export()
            };

            // Find and update the corresponding TirFunction in the entry module
            let entry_module = project
                .tir_modules
                .get_mut(&project.entry_module_source)
                .expect("entry module should exist");

            for func_rc in &entry_module.functions {
                let mut func = func_rc.borrow_mut();
                if func.name == export.name {
                    func.cm_export_info = Some(cm_export_info.clone());
                    break;
                }
            }
        }
    }

    // Attach CmExportInfo to test functions (__test_*)
    // Test functions are async exports that use task.return, just like CLI Command's run
    let entry_module = project
        .tir_modules
        .get_mut(&project.entry_module_source)
        .expect("entry module should exist");

    for func_rc in &entry_module.functions {
        let mut func = func_rc.borrow_mut();
        if func.name.starts_with("__test_") && func.cm_export_info.is_none() {
            // Test functions are async (need task.return) but not HTTP handlers
            func.cm_export_info = Some(CmExportInfo::async_export(false));
        }
    }

    project
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{GenericType, NamedType};
    use crate::token::Span;

    fn make_span() -> Span {
        Span::new(0, 0, 0, 0)
    }

    #[test]
    fn test_cm_export_info_async() {
        let info = CmExportInfo::async_export(false);
        assert!(info.is_async);
        assert!(!info.is_http_handler);
        assert!(info.required_imports.contains(&"task-return".to_string()));
    }

    #[test]
    fn test_cm_export_info_http_handler() {
        let info = CmExportInfo::async_export(true);
        assert!(info.is_async);
        assert!(info.is_http_handler);
        assert!(info.required_imports.contains(&"task-return".to_string()));
        assert!(
            info.required_imports
                .contains(&"http-response-new".to_string())
        );
        assert!(info.required_imports.contains(&"future-new".to_string()));
    }

    #[test]
    fn test_cm_export_info_sync() {
        let info = CmExportInfo::sync_export();
        assert!(!info.is_async);
        assert!(!info.is_http_handler);
        assert!(info.required_imports.is_empty());
    }

    #[test]
    fn test_get_cm_converter_array_string() {
        let return_type = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_list_string_to_array")
        );
    }

    #[test]
    fn test_get_cm_converter_array_u8() {
        let return_type = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                name: "u8".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_list_u8_to_array")
        );
    }

    #[test]
    fn test_get_cm_converter_option_string() {
        let return_type = Type::Generic(GenericType {
            name: "Option".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_option_string_to_option")
        );
    }

    #[test]
    fn test_get_cm_converter_array_tuple_string() {
        let return_type = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Tuple(vec![
                Type::Named(NamedType {
                    name: "String".to_string(),
                    span: make_span(),
                }),
                Type::Named(NamedType {
                    name: "String".to_string(),
                    span: make_span(),
                }),
            ])],
            span: make_span(),
        });
        assert_eq!(
            get_cm_converter_for_type(&return_type),
            Some("cm_list_tuple_string_string_to_array")
        );
    }

    #[test]
    fn test_get_cm_converter_none() {
        let return_type = Type::Named(NamedType {
            name: "i32".to_string(),
            span: make_span(),
        });
        assert_eq!(get_cm_converter_for_type(&return_type), None);
    }

    #[test]
    fn test_cm_converter_requirements_analyze() {
        let mut req = CmConverterRequirements::default();
        assert!(!req.any_needed());

        // Analyze Array<String>
        let array_string = Type::Generic(GenericType {
            name: "Array".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        req.analyze_type(&array_string);
        assert!(req.needs(CmConverterKind::ListString));
        assert!(req.any_needed());

        // Analyze Option<String>
        let option_string = Type::Generic(GenericType {
            name: "Option".to_string(),
            args: vec![Type::Named(NamedType {
                name: "String".to_string(),
                span: make_span(),
            })],
            span: make_span(),
        });
        req.analyze_type(&option_string);
        assert!(req.needs(CmConverterKind::OptionString));
    }
}
