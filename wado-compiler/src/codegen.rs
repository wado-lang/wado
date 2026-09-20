//! Wasm code generation — emits a WIR module as a Wasm component binary.
//!
//! Takes a linked `NirPackage` and a `WirPackage` and produces the final
//! Wasm component bytes.
//!
//! Pipeline: `WirPackage` → `emit` (core bytes) → `component` (wrapped) → `Vec<u8>`

use crate::ProviderComponent;
use crate::module_source::ModuleSource;
use crate::nir_package::NirPackage;
use crate::wir::WirPackage;

mod component;
mod component_context;
mod emit;

/// A binary the WIR pipeline produced that does not validate, with the
/// diagnosis and the bytes themselves. Saving it is the host's to do.
pub struct InvalidArtifact {
    /// What failed to validate, for the message: `core Wasm module` or
    /// `component`.
    pub subject: &'static str,
    /// What the host names the saved bytes after.
    pub file_stem: &'static str,
    pub wasm: Vec<u8>,
    /// Everything known about the failure, ready to print.
    pub report: String,
}

/// Emit a Wasm component binary from a linked package and its WIR module.
pub fn emit_wasm(
    package: &NirPackage,
    wir_package: &WirPackage,
    providers: &[ProviderComponent],
) -> Result<Vec<u8>, Box<InvalidArtifact>> {
    // Step 1: Emit core module bytes from WirPackage
    let core_module =
        emit::emit_core_module(wir_package, package.strip_names, package.codegen_flags);

    // Step 2: Validate core module (catch errors before component wrapping)
    if !package.skip_validation
        && let Some(invalid) = validate_core_module(&core_module, &package.entry_module_source)
    {
        return Err(Box::new(invalid));
    }

    // Step 3: Wrap in Component Model
    let wasm = component::build_component(package, &core_module, wir_package, providers);

    // Step 4: Validate
    if !package.skip_validation
        && let Some(invalid) = validate_wasm(&wasm, &package.entry_module_source)
    {
        return Err(Box::new(invalid));
    }

    Ok(wasm)
}

/// Validate `wasm`, answering what `describe` can say about where a failure
/// landed. Performs no I/O: the bytes travel to the host that can save them.
fn validate_or_report(
    wasm: &[u8],
    entry_module: &ModuleSource,
    subject: &'static str,
    file_stem: &'static str,
    describe: impl FnOnce(&[u8], usize) -> Option<String>,
    undescribed: &str,
) -> Option<InvalidArtifact> {
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    let Err(e) = validator.validate_all(wasm) else {
        return None;
    };
    let context = describe(wasm, e.offset()).unwrap_or_else(|| undescribed.to_string());
    let context = context.trim_end();
    Some(InvalidArtifact {
        subject,
        file_stem,
        wasm: wasm.to_vec(),
        report: format!(
            "Internal compiler error: WIR pipeline generated an invalid {subject}\n\
             Entry module: {entry_module}\n\
             Validation error: {e}\n\
             {context}"
        ),
    })
}

/// Validate core Wasm module (before component wrapping).
fn validate_core_module(wasm: &[u8], entry_module: &ModuleSource) -> Option<InvalidArtifact> {
    validate_or_report(
        wasm,
        entry_module,
        "core Wasm module",
        "invalid-core",
        describe_offending_location,
        "  (could not locate the offending function)",
    )
}

/// Describe where a core-Wasm validation error landed: the containing function
/// (index + demangled name), the body-relative byte offset, and a disassembled
/// window of operators around the failure so the failing instruction is named
/// rather than left as a raw module offset.
fn describe_offending_location(wasm: &[u8], offset: usize) -> Option<String> {
    use crate::hashmap::IndexMap;
    use wasmparser::{Name, Parser, Payload};
    let mut import_funcs = 0u32;
    let mut defined = 0u32;
    let mut names: IndexMap<u32, String> = IndexMap::default();
    // The function whose body contains `offset`, as (index, body start, ops).
    let mut hit: Option<(u32, usize, Vec<(usize, String)>)> = None;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.ok()? {
            Payload::ImportSection(reader) => {
                for imports in reader.into_iter().flatten() {
                    for entry in imports {
                        if let Ok((_, imp)) = entry
                            && matches!(imp.ty, wasmparser::TypeRef::Func(_))
                        {
                            import_funcs += 1;
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let idx = import_funcs + defined;
                defined += 1;
                let range = body.range();
                if hit.is_none() && range.contains(&offset) {
                    let mut ops = Vec::new();
                    if let Ok(reader) = body.get_operators_reader() {
                        for item in reader.into_iter_with_offsets() {
                            match item {
                                Ok((op, off)) => ops.push((off, format!("{op:?}"))),
                                Err(_) => break,
                            }
                        }
                    }
                    hit = Some((idx, range.start, ops));
                }
            }
            Payload::CustomSection(c) if c.name() == "name" => {
                let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(
                    c.data(),
                    c.data_offset(),
                ));
                for sub in reader {
                    if let Ok(Name::Function(map)) = sub {
                        for naming in map.into_iter().flatten() {
                            names.insert(naming.index, naming.name.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let (idx, body_start, ops) = hit?;
    let name = names.get(&idx).map_or("<anonymous>", String::as_str);
    let rel = offset.saturating_sub(body_start);

    // The validator points at the failing operator's start; the pivot is the
    // last operator at or before the error offset. `None` means the offset is
    // in the locals / prologue before the first operator — show leading context
    // without a misleading `>>` marker.
    let mut disasm = String::new();
    if ops.is_empty() {
        disasm.push_str("    (no operators decoded)\n");
    } else {
        let pivot = ops.iter().rposition(|(off, _)| *off <= offset);
        let center = pivot.unwrap_or(0);
        let start = center.saturating_sub(4);
        let end = (center + 3).min(ops.len());
        for (i, (off, text)) in ops.iter().enumerate().take(end).skip(start) {
            let marker = if pivot == Some(i) { ">>" } else { "  " };
            let rel_off = off.saturating_sub(body_start);
            disasm.push_str(&format!("    {marker} +0x{rel_off:04x}  {text}\n"));
        }
    }

    let note = if ops.is_empty() {
        ""
    } else if ops.iter().any(|(off, _)| *off <= offset) {
        " (`>>` marks the failing operator)"
    } else {
        " (failure is in the function prologue, before the first operator below)"
    };
    Some(format!(
        "Offending function: func #{idx} {name}\n\
         Failure at body offset +0x{rel:04x}{note}:\n\
         {disasm}"
    ))
}

/// The outer component's instances in declaration order, so a validator message
/// naming `instance N` names something a reader can find.
fn describe_component_instances(wasm: &[u8]) -> Option<String> {
    use wasmparser::{ComponentTypeRef, Parser, Payload};
    let mut depth = 0usize;
    let mut instances: Vec<String> = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.ok()? {
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => depth = depth.saturating_sub(1),
            Payload::ComponentImportSection(reader) if depth == 0 => {
                for import in reader.into_iter().flatten() {
                    if matches!(import.ty, ComponentTypeRef::Instance(_)) {
                        instances.push(format!("imported `{}`", import.name.name));
                    }
                }
            }
            Payload::ComponentInstanceSection(reader) if depth == 0 => {
                for _ in reader.into_iter().flatten() {
                    instances.push("instantiated in this component".to_string());
                }
            }
            _ => {}
        }
    }
    if instances.is_empty() {
        return None;
    }
    let mut out = String::from("Component instances, in declaration order:\n");
    for (idx, what) in instances.iter().enumerate() {
        out.push_str(&format!("    instance {idx}  {what}\n"));
    }
    Some(out)
}

/// Validate the wrapped component (after `validate_core_module`).
fn validate_wasm(wasm: &[u8], entry_module: &ModuleSource) -> Option<InvalidArtifact> {
    validate_or_report(
        wasm,
        entry_module,
        "component",
        "invalid-component",
        |wasm, _| describe_component_instances(wasm),
        "  (could not read the component's instances)",
    )
}
