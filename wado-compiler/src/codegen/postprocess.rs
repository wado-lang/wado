//! Post-processing for Wasm modules
//!
//! This module provides utilities to transform Wasm modules, such as
//! converting memory definitions to imports and dead code elimination.

use crate::hashmap::IndexSet;

use walrus::passes;
use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, ImportSection, MemoryType, Module, RawSection,
};
use wasmparser::{Parser, Payload};

/// Perform dead code elimination on a Wasm module, keeping only the specified exports.
///
/// This uses walrus to remove unused functions and other items from the module.
/// The `keep_exports` set contains the names of exports that should be preserved.
/// The gc pass automatically treats exports as roots, so we remove unwanted exports first.
pub fn eliminate_dead_code(wasm_bytes: &[u8], keep_exports: &IndexSet<String>) -> Vec<u8> {
    let mut module =
        walrus::Module::from_buffer(wasm_bytes).expect("bundled module should be valid");

    // Remove exports that are not in the keep set
    // The gc pass will treat remaining exports as roots
    let exports_to_remove: Vec<_> = module
        .exports
        .iter()
        .filter(|e| !keep_exports.contains(&e.name))
        .map(walrus::Export::id)
        .collect();

    for id in exports_to_remove {
        module.exports.delete(id);
    }

    // Run garbage collection to remove unreferenced items
    // (exports are automatically treated as roots)
    passes::gc::run(&mut module);

    module.emit_wasm()
}

/// Extract the minimum memory pages from a Wasm module's memory section.
pub fn extract_memory_min_pages(wasm_bytes: &[u8]) -> u64 {
    let parser = Parser::new(0);
    for payload in parser.parse_all(wasm_bytes) {
        if let Ok(Payload::MemorySection(mems)) = payload
            && let Some(mem) = mems.into_iter().flatten().next()
        {
            return mem.initial;
        }
    }
    1
}

/// Convert a Wasm module that defines memory to one that imports memory
/// This allows the module to share memory with other modules in a component
pub fn convert_memory_to_import(
    wasm_bytes: &[u8],
    import_module: &str,
    import_name: &str,
) -> Result<Vec<u8>, String> {
    let parser = Parser::new(0);
    let mut module = Module::new();

    // Track what we find
    let mut memory_pages: u64 = 1;
    let mut type_bytes: Option<Vec<u8>> = None;
    let mut function_bytes: Option<Vec<u8>> = None;
    let mut global_bytes: Option<Vec<u8>> = None;
    let mut export_section_data: Vec<(String, ExportKind, u32)> = Vec::new();
    let mut data_bytes: Option<Vec<u8>> = None;

    // First pass: collect sections
    for payload in parser.parse_all(wasm_bytes) {
        let payload = payload.map_err(|e| format!("Parse error: {e}"))?;

        match &payload {
            Payload::TypeSection(_) => {
                // Get raw bytes for this section
                if let Some(range) = get_section_range(&payload) {
                    type_bytes = Some(wasm_bytes[range].to_vec());
                }
            }
            Payload::FunctionSection(_) => {
                if let Some(range) = get_section_range(&payload) {
                    function_bytes = Some(wasm_bytes[range].to_vec());
                }
            }
            Payload::MemorySection(mems) => {
                for mem in mems.clone() {
                    let mem = mem.map_err(|e| format!("{e}"))?;
                    memory_pages = mem.initial;
                }
            }
            Payload::GlobalSection(_) => {
                if let Some(range) = get_section_range(&payload) {
                    global_bytes = Some(wasm_bytes[range].to_vec());
                }
            }
            Payload::ExportSection(exports) => {
                for exp in exports.clone() {
                    let exp = exp.map_err(|e| format!("{e}"))?;
                    let kind = match exp.kind {
                        wasmparser::ExternalKind::Func => ExportKind::Func,
                        wasmparser::ExternalKind::Global => ExportKind::Global,
                        wasmparser::ExternalKind::Memory => {
                            continue; // Skip memory export
                        }
                        _ => continue,
                    };
                    export_section_data.push((exp.name.to_string(), kind, exp.index));
                }
            }
            Payload::CodeSectionStart { .. } => {}
            Payload::CodeSectionEntry(_) => {}
            Payload::DataSection(_) => {
                if let Some(range) = get_section_range(&payload) {
                    data_bytes = Some(wasm_bytes[range].to_vec());
                }
            }
            _ => {}
        }
    }

    // Second pass: rebuild with memory import
    let parser = Parser::new(0);
    let mut code_entries: Vec<Vec<u8>> = Vec::new();

    for payload in parser.parse_all(wasm_bytes) {
        let payload = payload.map_err(|e| format!("Parse error: {e}"))?;

        match payload {
            Payload::TypeSection(_) => {
                if let Some(bytes) = &type_bytes {
                    module.section(&RawSection { id: 1, data: bytes });
                }
            }
            Payload::ImportSection(_) => {
                // Skip original imports, we'll add our own
            }
            Payload::FunctionSection(_) => {
                // Add import section first (before function section)
                let mut imports = ImportSection::new();
                imports.import(
                    import_module,
                    import_name,
                    MemoryType {
                        minimum: memory_pages,
                        maximum: None,
                        memory64: false,
                        shared: false,
                        page_size_log2: None,
                    },
                );
                module.section(&imports);

                // Then add function section
                if let Some(bytes) = &function_bytes {
                    module.section(&RawSection { id: 3, data: bytes });
                }
            }
            Payload::MemorySection(_) => {
                // Skip - we're importing memory instead
            }
            Payload::GlobalSection(_) => {
                if let Some(bytes) = &global_bytes {
                    module.section(&RawSection { id: 6, data: bytes });
                }
            }
            Payload::ExportSection(_) => {
                let mut exports = ExportSection::new();
                for (name, kind, idx) in &export_section_data {
                    exports.export(name, *kind, *idx);
                }
                module.section(&exports);
            }
            Payload::CodeSectionStart { .. } => {
                // Start of code section
            }
            Payload::CodeSectionEntry(body) => {
                // Collect code entries
                let range = body.range();
                code_entries.push(wasm_bytes[range].to_vec());
            }
            Payload::DataSection(_) => {
                // Build code section before data section
                if !code_entries.is_empty() {
                    let mut code = CodeSection::new();
                    for entry in &code_entries {
                        code.raw(entry);
                    }
                    module.section(&code);
                    code_entries.clear();
                }

                // Add data section
                if let Some(bytes) = &data_bytes {
                    module.section(&RawSection {
                        id: 11,
                        data: bytes,
                    });
                }
            }
            _ => {}
        }
    }

    // If code section wasn't added before data, add it now
    if !code_entries.is_empty() {
        let mut code = CodeSection::new();
        for entry in &code_entries {
            code.raw(entry);
        }
        module.section(&code);
    }

    Ok(module.finish())
}

fn get_section_range(payload: &Payload) -> Option<std::ops::Range<usize>> {
    match payload {
        Payload::TypeSection(reader) => Some(reader.range()),
        Payload::FunctionSection(reader) => Some(reader.range()),
        Payload::GlobalSection(reader) => Some(reader.range()),
        Payload::DataSection(reader) => Some(reader.range()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stdlib;

    /// Parse the bundled `core:libm.wat` into core wasm bytes once per
    /// process so postprocess tests can reuse a non-trivial module.
    fn libm_core_wasm() -> Vec<u8> {
        wat::parse_bytes(stdlib::CORE_LIBM_WAT)
            .expect("core:libm.wat must parse as valid WAT")
            .into_owned()
    }

    #[test]
    fn test_convert_memory_to_import() {
        let bytes = libm_core_wasm();
        let result = convert_memory_to_import(&bytes, "env", "memory");
        assert!(result.is_ok(), "Failed to convert: {result:?}");

        let converted = result.unwrap();

        // Verify it's valid Wasm
        assert!(converted.starts_with(&[0, b'a', b's', b'm']));

        // Parse and verify structure
        let parser = Parser::new(0);
        let mut has_memory_import = false;
        let mut has_memory_definition = false;

        for payload in parser.parse_all(&converted) {
            match payload {
                Ok(Payload::ImportSection(imports)) => {
                    for group in imports.into_iter().flatten() {
                        for import in group {
                            if let Ok((_, imp)) = import
                                && matches!(imp.ty, wasmparser::TypeRef::Memory(_))
                            {
                                has_memory_import = true;
                            }
                        }
                    }
                }
                Ok(Payload::MemorySection(_)) => {
                    has_memory_definition = true;
                }
                _ => {}
            }
        }

        assert!(has_memory_import, "Should have memory import");
        assert!(!has_memory_definition, "Should not have memory definition");
    }
}
