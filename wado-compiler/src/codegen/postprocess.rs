//! Post-processing for Wasm modules
//!
//! This module provides utilities to transform Wasm modules, such as
//! converting memory definitions to imports and dead code elimination.

use crate::hashmap::IndexSet;

use walrus::passes;
use wasm_encoder::{
    EntityType, ExportKind, ExportSection, ImportSection, MemoryType, Module, RawSection,
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

const IMPORT_SECTION_ID: u8 = 2;

/// Rewrite a wasm asset so it takes the component's memory as an import rather
/// than defining its own, letting it share memory with the other core modules.
///
/// Handles all three shapes `loader.rs` accepts (see [`MemorySource`]). Every
/// section other than the memory definition, the memory export, and the import
/// section is carried over byte-for-byte, in its original order. That leaves
/// every index space unchanged: the memory import takes slot 0, exactly where
/// the dropped definition sat, and the memory space is disjoint from the
/// function, global, and table spaces.
pub fn convert_memory_to_import(
    wasm_bytes: &[u8],
    import_module: &str,
    import_name: &str,
) -> Result<Vec<u8>, String> {
    // `None` once the module already imports its memory: it needs no new one,
    // and adding a second would be a duplicate.
    let to_import: Option<MemoryType> = match find_memory_source(wasm_bytes)? {
        MemorySource::AlreadyImported => None,
        MemorySource::Defined(mem) => Some(mem),
        MemorySource::Absent => Some(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        }),
    };
    let memory_import = |module: &mut Module, mut imports: ImportSection| {
        if let Some(mem) = to_import {
            imports.import(import_module, import_name, mem);
        }
        module.section(&imports);
    };

    let mut module = Module::new();
    let mut memory_import_written = false;
    for payload in Parser::new(0).parse_all(wasm_bytes) {
        let payload = payload.map_err(|e| format!("Parse error: {e}"))?;

        // The import section must precede every section that can reference an
        // import. When the module has none of its own, open one right before
        // the first such section (any id above the import section's own 2).
        if !memory_import_written
            && to_import.is_some()
            && let Some((id, _)) = payload.as_section()
            && id > IMPORT_SECTION_ID
        {
            memory_import(&mut module, ImportSection::new());
            memory_import_written = true;
        }

        match &payload {
            Payload::ImportSection(reader) => {
                let mut imports = ImportSection::new();
                for group in reader.clone() {
                    for entry in group.map_err(|e| format!("Parse error: {e}"))? {
                        let (_, import) = entry.map_err(|e| format!("Parse error: {e}"))?;
                        imports.import(
                            import.module,
                            import.name,
                            entity_type(import.ty).ok_or_else(|| {
                                format!(
                                    "unsupported import kind in {}/{}",
                                    import.module, import.name
                                )
                            })?,
                        );
                    }
                }
                memory_import(&mut module, imports);
                memory_import_written = true;
            }
            // Replaced by the import above.
            Payload::MemorySection(_) => {}
            Payload::ExportSection(reader) => {
                let mut exports = ExportSection::new();
                for export in reader.clone() {
                    let export = export.map_err(|e| format!("Parse error: {e}"))?;
                    if export.kind == wasmparser::ExternalKind::Memory {
                        continue;
                    }
                    exports.export(
                        export.name,
                        export_kind(export.kind)
                            .ok_or_else(|| format!("unsupported export kind: {}", export.name))?,
                        export.index,
                    );
                }
                module.section(&exports);
            }
            // Individual bodies arrive after `CodeSectionStart`, which already
            // copied the whole section verbatim.
            Payload::CodeSectionEntry(_) => {}
            // Everything else — including sections this rewrite knows nothing
            // about — is copied through rather than dropped.
            other => {
                if let Some((id, range)) = other.as_section() {
                    module.section(&RawSection {
                        id,
                        data: &wasm_bytes[range],
                    });
                }
            }
        }
    }

    if !memory_import_written && to_import.is_some() {
        memory_import(&mut module, ImportSection::new());
    }

    Ok(module.finish())
}

/// Where a wasm asset's memory comes from. These are the shapes `loader.rs`
/// admits: it rejects more than one memory, and `env.memory` is the only import
/// it allows — so all three arms are ordinary inputs, not error cases.
enum MemorySource {
    /// Defines exactly one. The rewrite drops it and imports the same shape.
    Defined(MemoryType),
    /// Already written against the component's memory; nothing to convert.
    AlreadyImported,
    /// Neither defines nor imports one, so it cannot touch linear memory. It
    /// still gets a minimal import, keeping every embedded module one shape.
    Absent,
}

fn find_memory_source(wasm_bytes: &[u8]) -> Result<MemorySource, String> {
    let mut defined: Option<MemoryType> = None;
    for payload in Parser::new(0).parse_all(wasm_bytes) {
        match payload.map_err(|e| format!("Parse error: {e}"))? {
            Payload::MemorySection(reader) => {
                for mem in reader {
                    let mem = mem.map_err(|e| format!("Parse error: {e}"))?;
                    if defined.is_some() {
                        return Err("module defines more than one memory".to_string());
                    }
                    defined = Some(MemoryType {
                        minimum: mem.initial,
                        maximum: mem.maximum,
                        memory64: mem.memory64,
                        shared: mem.shared,
                        page_size_log2: mem.page_size_log2,
                    });
                }
            }
            Payload::ImportSection(reader) => {
                for group in reader {
                    for entry in group.map_err(|e| format!("Parse error: {e}"))? {
                        let (_, import) = entry.map_err(|e| format!("Parse error: {e}"))?;
                        if matches!(import.ty, wasmparser::TypeRef::Memory(_)) {
                            return Ok(MemorySource::AlreadyImported);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(defined.map_or(MemorySource::Absent, MemorySource::Defined))
}

fn entity_type(ty: wasmparser::TypeRef) -> Option<EntityType> {
    use wasmparser::TypeRef;
    match ty {
        TypeRef::Func(idx) => Some(EntityType::Function(idx)),
        TypeRef::Memory(mem) => Some(EntityType::Memory(MemoryType {
            minimum: mem.initial,
            maximum: mem.maximum,
            memory64: mem.memory64,
            shared: mem.shared,
            page_size_log2: mem.page_size_log2,
        })),
        TypeRef::Global(_) | TypeRef::Table(_) | TypeRef::Tag(_) | TypeRef::FuncExact(_) => None,
    }
}

fn export_kind(kind: wasmparser::ExternalKind) -> Option<ExportKind> {
    use wasmparser::ExternalKind;
    match kind {
        ExternalKind::Func | ExternalKind::FuncExact => Some(ExportKind::Func),
        ExternalKind::Table => Some(ExportKind::Table),
        ExternalKind::Memory => Some(ExportKind::Memory),
        ExternalKind::Global => Some(ExportKind::Global),
        ExternalKind::Tag => Some(ExportKind::Tag),
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

    /// The imported memory type, if the module imports one.
    fn memory_import_of(wasm: &[u8]) -> Option<wasmparser::MemoryType> {
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::ImportSection(imports)) = payload {
                for group in imports.into_iter().flatten() {
                    for import in group {
                        if let Ok((_, imp)) = import
                            && let wasmparser::TypeRef::Memory(mem) = imp.ty
                        {
                            return Some(mem);
                        }
                    }
                }
            }
        }
        None
    }

    fn count_memory_imports(wasm: &[u8]) -> usize {
        let mut n = 0;
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::ImportSection(imports)) = payload {
                for group in imports.into_iter().flatten() {
                    for import in group {
                        if let Ok((_, imp)) = import
                            && matches!(imp.ty, wasmparser::TypeRef::Memory(_))
                        {
                            n += 1;
                        }
                    }
                }
            }
        }
        n
    }

    fn section_ids(wasm: &[u8]) -> Vec<u8> {
        Parser::new(0)
            .parse_all(wasm)
            .filter_map(|p| p.ok().and_then(|p| p.as_section().map(|(id, _)| id)))
            .collect()
    }

    fn export_names(wasm: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::ExportSection(exports)) = payload {
                for export in exports.into_iter().flatten() {
                    names.push(export.name.to_string());
                }
            }
        }
        names
    }

    fn validate(wasm: &[u8]) -> Result<(), String> {
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(wasm)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn test_convert_memory_to_import() {
        let bytes = libm_core_wasm();
        let converted = convert_memory_to_import(&bytes, "env", "memory").expect("conversion");

        assert!(memory_import_of(&converted).is_some(), "memory import");
        assert!(
            !section_ids(&converted).contains(&5),
            "memory definition must be gone"
        );
        validate(&converted).expect("converted libm must validate");
    }

    /// The rewrite touches three sections; everything else must survive
    /// byte-for-byte. It used to rebuild the module from a fixed list of
    /// section kinds, so a table, element, start, or custom section was
    /// silently dropped and the memory's `maximum` thrown away.
    #[test]
    fn convert_memory_to_import_preserves_other_sections() {
        let source = r#"
            (module
              (type $void (func))
              (type $get (func (result i32)))
              (memory 2 4)
              (table 1 funcref)
              (global $g (mut i32) (i32.const 7))
              (func $init (type $void))
              (func $answer (type $get) (result i32) (i32.const 42))
              (start $init)
              (elem declare func $answer)
              (data (i32.const 0) "hi")
              (export "memory" (memory 0))
              (export "answer" (func $answer))
              (export "table" (table 0))
              (export "g" (global $g)))
        "#;
        let bytes = wat::parse_str(source).expect("fixture must parse");
        let converted = convert_memory_to_import(&bytes, "env", "memory").expect("conversion");

        validate(&converted).expect("converted module must validate");

        let mem = memory_import_of(&converted).expect("memory import");
        assert_eq!(mem.initial, 2);
        assert_eq!(mem.maximum, Some(4), "maximum must survive the rewrite");

        let ids = section_ids(&converted);
        assert!(!ids.contains(&5), "memory definition must be gone");
        for (id, name) in [
            (0u8, "custom"),
            (1, "type"),
            (2, "import"),
            (3, "function"),
            (4, "table"),
            (6, "global"),
            (7, "export"),
            (8, "start"),
            (9, "element"),
            (10, "code"),
            (11, "data"),
        ] {
            assert!(ids.contains(&id), "{name} section must survive");
        }
        // Custom sections (id 0) may sit anywhere; the rest must stay ordered.
        let ordered: Vec<u8> = ids.iter().copied().filter(|id| *id != 0).collect();
        assert_eq!(ordered, {
            let mut sorted = ordered.clone();
            sorted.sort_unstable();
            sorted
        });

        let exports = export_names(&converted);
        assert!(!exports.contains(&"memory".to_string()), "memory export");
        for kept in ["answer", "table", "g"] {
            assert!(exports.contains(&kept.to_string()), "{kept} export");
        }
    }

    /// A module with no import section of its own still gets one, placed
    /// before every section that could reference an import.
    #[test]
    fn convert_memory_to_import_inserts_import_section() {
        let bytes = wat::parse_str("(module (memory 1) (func))").expect("fixture must parse");
        let converted = convert_memory_to_import(&bytes, "env", "memory").expect("conversion");
        validate(&converted).expect("converted module must validate");
        assert!(memory_import_of(&converted).is_some());
    }

    /// `loader.rs` accepts a wasm asset with no memory at all (it only rejects
    /// more than one), so the rewrite must still hand it the component's
    /// memory rather than refusing the module.
    #[test]
    fn convert_memory_to_import_accepts_a_module_with_no_memory() {
        let bytes = wat::parse_str(r#"(module (func (export "add_one")))"#).expect("fixture");
        let converted = convert_memory_to_import(&bytes, "env", "memory").expect("conversion");
        validate(&converted).expect("converted module must validate");
        assert!(memory_import_of(&converted).is_some(), "memory import");
    }

    /// `env.memory` is the one import `loader.rs` permits in a wasm asset, so a
    /// module already written against it is a normal input, not an error — and
    /// it must not come back with the import duplicated.
    #[test]
    fn convert_memory_to_import_passes_through_an_existing_memory_import() {
        let source = r#"
            (module
              (import "env" "memory" (memory 1))
              (func (export "load") (result i32) (i32.load (i32.const 0))))
        "#;
        let bytes = wat::parse_str(source).expect("fixture must parse");
        let converted = convert_memory_to_import(&bytes, "env", "memory").expect("conversion");
        validate(&converted).expect("converted module must validate");
        assert!(memory_import_of(&converted).is_some(), "memory import");
        assert_eq!(count_memory_imports(&converted), 1, "no duplicate import");
    }

    #[test]
    fn convert_memory_to_import_rejects_more_than_one_memory() {
        let bytes = wat::parse_str("(module (memory 1) (memory 1))").expect("fixture must parse");
        assert!(convert_memory_to_import(&bytes, "env", "memory").is_err());
    }
}
