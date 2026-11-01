//! A Kiln generator's `probe`, checked when the generator compiles. The host
//! supplies a probe's parameters by name, so each must be one it supplies and
//! of the type `generate` is handed it as: `path`, `content` and `type` as an
//! `InputFile` holds them, and `options` as `generate` takes it. See
//! `docs/spec-kiln.md` § The Probe.

use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::primitive::PrimitiveType;
use crate::semantics::Semantics;
use crate::tir::{ResolvedType, TirParam, TypeId};
use crate::token::Span;

/// What is wrong with the generator's `export fn probe`, if it has one. A
/// missing `generate` is [`super::extract_options_descriptor`]'s to report.
#[must_use]
pub fn check_probe(sem: &Semantics, module: &ModuleSource) -> Vec<Diagnostic> {
    let Some(tir_module) = sem.tir_modules.get(module) else {
        return Vec::new();
    };
    let Some(probe) = tir_module
        .find_function("probe")
        .filter(|probe| probe.borrow().is_export)
    else {
        return Vec::new();
    };
    let Some(generate) = tir_module.find_function("generate") else {
        return Vec::new();
    };
    let probe = probe.borrow();
    let generate = generate.borrow();
    let types = &sem.types;
    // Nothing to check against where `generate` is not the shape the request
    // adapter rewrites.
    let Some(supplied) = supplied_parameters(sem, &generate.params) else {
        return Vec::new();
    };

    let mut diagnostics = Vec::new();
    let mut refuse = |span: &Span, message: String| {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: Code::KilnProbeSignature,
            message: format!("kiln: {message}"),
            span: Some(DiagnosticSpan::from_span(span, Some(&module.source_path()))),
        });
    };
    for param in &probe.params {
        match supplied.get(param.name.as_str()) {
            None => refuse(
                &param.span,
                format!(
                    "`probe` takes `{}`, but a host supplies only {}",
                    param.name,
                    spell(supplied.keys())
                ),
            ),
            Some(&want) if types.type_key(param.type_id) != types.type_key(want) => refuse(
                &param.span,
                format!(
                    "`probe` takes `{}` as `{}`, but a host supplies it as `{}`",
                    param.name,
                    types.name(param.type_id),
                    types.name(want)
                ),
            ),
            Some(_) => {}
        }
    }

    let answers_extent = types.as_result(probe.return_type).is_some_and(|(ok, err)| {
        matches!(types.get(ok), ResolvedType::Primitive(PrimitiveType::U64))
            && types
                .as_result(generate.return_type)
                .is_some_and(|(_, generate_err)| {
                    types.type_key(err) == types.type_key(generate_err)
                })
    });
    if !answers_extent {
        refuse(
            &probe.span,
            format!(
                "`probe` returns `{}`, but must return `Result<u64, _>` with `generate`'s error",
                types.name(probe.return_type)
            ),
        );
    }
    diagnostics
}

/// The probe parameters a host supplies, each with its type, read off
/// `generate` after the request adapter rewrote it: an `InputFile`'s fields,
/// and `options` where `generate` takes one.
fn supplied_parameters<'a>(
    sem: &'a Semantics,
    generate_params: &'a [TirParam],
) -> Option<IndexMap<&'a str, TypeId>> {
    let types = &sem.types;
    let primary = generate_params
        .iter()
        .find(|param| param.name == "primary")?;
    let ResolvedType::Struct { def, .. } = types.get(primary.type_id) else {
        return None;
    };
    let input_file = sem
        .tir_modules
        .get(types.struct_head_module(*def))?
        .find_struct(&types.struct_head_mangled_name(*def))?;
    let mut supplied: IndexMap<&str, TypeId> = input_file
        .fields
        .iter()
        .map(|field| (field.name.as_str(), field.type_id))
        .collect();
    if let Some(options) = generate_params.iter().find(|param| param.name == "options") {
        supplied.insert("options", options.type_id);
    }
    Some(supplied)
}

fn spell<'a>(names: impl Iterator<Item = &'a &'a str>) -> String {
    let names: Vec<String> = names.map(|name| format!("`{name}`")).collect();
    match names.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => unreachable!("a host supplies at least an `InputFile`'s fields"),
    }
}
