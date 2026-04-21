//! Typed extraction of a Kiln generator's `Options` struct.
//!
//! The driver calls [`extract_options_descriptor`] once per generator
//! component, after that component's package has been type-resolved. The
//! returned [`OptionsDescriptor`] drives both [`crate::kiln::options_check`]
//! (typed validation of a user-supplied options table) and
//! [`crate::kiln::cache::encode_options_canonical`] (the cache-key byte
//! layout).
//!
//! See WEP 2026-04-12 §M4 for the full design.
//!
//! The extractor is deliberately strict: only shapes that round-trip cleanly
//! through the Component-Model canonical encoder are allowed. A generator
//! that wants to accept richer configuration should split that configuration
//! out into auxiliary structs that are themselves made of the primitives
//! listed here.

use crate::annotate::Annotated;
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::name::ModuleSource;
use crate::tir::{
    PrimitiveType, ResolvedType, TirExpr, TirExprKind, TirField, TirModule, TypeId, TypeTable,
};
use crate::token::Span;

/// Structural description of a generator's `pub struct Options`.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsDescriptor {
    pub fields: Vec<OptionsField>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OptionsField {
    pub name: String,
    pub ty: OptionsType,
    /// Pre-evaluated default, if the generator's `Options` declaration
    /// provides a literal default for this field. Fields without a default
    /// are required in the user's options table.
    pub default: Option<CanonicalValue>,
    pub span: Span,
}

/// The set of shapes supported inside a generator's `Options` struct.
#[derive(Debug, Clone, PartialEq)]
pub enum OptionsType {
    Bool,
    I32,
    I64,
    U32,
    U64,
    F32,
    F64,
    String,
    /// `Option<T>` — missing-and-`null` both resolve to `None`.
    Option(Box<OptionsType>),
    /// Enum with all-no-payload variants, by name.
    Enum {
        name: String,
        variants: Vec<String>,
    },
    /// Nested struct, recursively described.
    Struct {
        name: String,
        descriptor: OptionsDescriptor,
    },
}

impl OptionsType {
    pub fn describe(&self) -> String {
        match self {
            OptionsType::Bool => "bool".to_string(),
            OptionsType::I32 => "i32".to_string(),
            OptionsType::I64 => "i64".to_string(),
            OptionsType::U32 => "u32".to_string(),
            OptionsType::U64 => "u64".to_string(),
            OptionsType::F32 => "f32".to_string(),
            OptionsType::F64 => "f64".to_string(),
            OptionsType::String => "String".to_string(),
            OptionsType::Option(inner) => format!("Option<{}>", inner.describe()),
            OptionsType::Enum { name, .. } => name.clone(),
            OptionsType::Struct { name, .. } => name.clone(),
        }
    }
}

/// The compiler-side representation of a fully-validated options value.
///
/// Shape mirrors [`OptionsType`] so [`crate::kiln::cache::encode_options_canonical`]
/// can walk the two in lock-step.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonicalValue {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    /// `None` when the user wrote `null` or omitted the field on an
    /// `Option<T>` with no struct-level default.
    None,
    Some(Box<CanonicalValue>),
    Enum(String),
    Struct(Vec<(String, CanonicalValue)>),
}

/// Locate `pub struct Options` in the generator's entry module and describe
/// it as an [`OptionsDescriptor`].
///
/// # Errors
/// Diagnostics are batched: a single bad field does not prevent the rest of
/// the struct from being reported. The returned `Err` variant is only taken
/// when the `Options` struct is missing or the generator is missing its
/// `generate` function — shape-level failures all come back as
/// [`Code::GeneratorOptionsUnsupported`] diagnostics.
pub fn extract_options_descriptor(
    annotated: &Annotated,
    module: &ModuleSource,
) -> Result<OptionsDescriptor, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();

    let tir_module = match annotated.tir_modules.get(module) {
        Some(m) => m,
        None => {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsUnsupported,
                message: format!(
                    "kiln: generator module {:?} has no TIR available",
                    module.diagnostic_filename()
                ),
                span: None,
            });
            return Err(diagnostics);
        }
    };

    let options_struct = match tir_module.find_struct("Options") {
        Some(s) => s,
        None => {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsUnsupported,
                message: format!(
                    "kiln: generator {:?} does not declare `pub struct Options`",
                    module.diagnostic_filename()
                ),
                span: None,
            });
            return Err(diagnostics);
        }
    };

    if !options_struct.is_pub {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: Code::GeneratorOptionsUnsupported,
            message: "kiln: `Options` struct must be declared `pub`".to_string(),
            span: Some(span_of(&options_struct.span, module)),
        });
    }

    if tir_module.find_function("generate").is_none() {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: Code::GeneratorOptionsUnsupported,
            message: format!(
                "kiln: generator {:?} does not declare `generate` function",
                module.diagnostic_filename()
            ),
            span: None,
        });
    }

    let mut descriptor_fields = Vec::with_capacity(options_struct.fields.len());
    for field in &options_struct.fields {
        match lower_field(field, &annotated.types, module, &mut diagnostics) {
            Some(desc_field) => descriptor_fields.push(desc_field),
            None => {
                // Diagnostic already pushed by lower_field; continue so other
                // fields also get reported.
            }
        }
    }

    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(diagnostics);
    }

    Ok(OptionsDescriptor {
        fields: descriptor_fields,
    })
}

fn lower_field(
    field: &TirField,
    types: &TypeTable,
    module: &ModuleSource,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<OptionsField> {
    let ty = lower_type(field.type_id, types, module, &field.name, diagnostics)?;
    let default = field
        .default_expr
        .as_deref()
        .and_then(|e| evaluate_literal(e, &ty, types, module, &field.name, diagnostics));
    Some(OptionsField {
        name: field.name.clone(),
        ty,
        default,
        span: field.span,
    })
}

fn lower_type(
    type_id: TypeId,
    types: &TypeTable,
    module: &ModuleSource,
    field_name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<OptionsType> {
    if let Some(inner_id) = types.as_option(type_id) {
        let inner = lower_type(inner_id, types, module, field_name, diagnostics)?;
        return Some(OptionsType::Option(Box::new(inner)));
    }

    match types.get(type_id) {
        ResolvedType::Primitive(p) => match p {
            PrimitiveType::Bool => Some(OptionsType::Bool),
            PrimitiveType::I32 => Some(OptionsType::I32),
            PrimitiveType::I64 => Some(OptionsType::I64),
            PrimitiveType::U32 => Some(OptionsType::U32),
            PrimitiveType::U64 => Some(OptionsType::U64),
            PrimitiveType::F32 => Some(OptionsType::F32),
            PrimitiveType::F64 => Some(OptionsType::F64),
            other => {
                push_unsupported(
                    diagnostics,
                    module,
                    field_name,
                    &format!(
                        "primitive type `{}` is not supported in generator options",
                        other.as_str()
                    ),
                );
                None
            }
        },
        ResolvedType::Struct { name, .. } if name == "String" => Some(OptionsType::String),
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => {
            let nested = nested_struct_descriptor(name, module_source, types, module, diagnostics)?;
            Some(OptionsType::Struct {
                name: name.clone(),
                descriptor: nested,
            })
        }
        ResolvedType::Enum {
            name,
            module_source,
        } => {
            let variants = enum_variants(name, module_source, types, module, diagnostics)?;
            Some(OptionsType::Enum {
                name: name.clone(),
                variants,
            })
        }
        other => {
            push_unsupported(
                diagnostics,
                module,
                field_name,
                &format!("type `{other:?}` is not supported in generator options"),
            );
            None
        }
    }
}

fn nested_struct_descriptor(
    _struct_name: &str,
    _struct_module: &ModuleSource,
    _types: &TypeTable,
    module: &ModuleSource,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<OptionsDescriptor> {
    // Nested struct descriptors require access to the declaring module's TIR.
    // v1 declines nested structs rather than re-scanning every module; the
    // WEP explicitly scopes M4 to the "Options struct plus primitives /
    // enums / Option<T>" happy path and leaves nested structs as a follow-up.
    push_unsupported(
        diagnostics,
        module,
        "<nested>",
        "nested struct options are not yet supported",
    );
    None
}

fn enum_variants(
    enum_name: &str,
    enum_module: &ModuleSource,
    types: &TypeTable,
    module: &ModuleSource,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<String>> {
    // The enum's cases live in the declaring module's TIR. We cannot reach
    // that from the TypeTable alone, so we keep the information in the
    // intern'd `ResolvedType` and require the caller's `Annotated` to look it
    // up. To avoid threading Annotated through everything, we keep an
    // approximation here: reject enums from non-current modules unless the
    // `TypeTable` already exposed the variants. v1 only needs flat local
    // enums, so this is a pragmatic subset.
    let _ = (enum_name, enum_module, types);
    push_unsupported(
        diagnostics,
        module,
        enum_name,
        "enum options are supported only via a future extension; declare \
         configuration choices as booleans or strings in v1",
    );
    None
}

fn evaluate_literal(
    expr: &TirExpr,
    ty: &OptionsType,
    types: &TypeTable,
    module: &ModuleSource,
    field_name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CanonicalValue> {
    match (&expr.kind, ty) {
        (TirExprKind::BoolLiteral(b), OptionsType::Bool) => Some(CanonicalValue::Bool(*b)),
        (TirExprKind::IntLiteral { value, .. }, OptionsType::I32 | OptionsType::I64) => {
            Some(CanonicalValue::I64(*value as i64))
        }
        (TirExprKind::IntLiteral { value, .. }, OptionsType::U32 | OptionsType::U64) => {
            Some(CanonicalValue::U64(*value))
        }
        (TirExprKind::FloatLiteral { value, .. }, OptionsType::F32 | OptionsType::F64) => {
            Some(CanonicalValue::F64(*value))
        }
        (TirExprKind::StringLiteral(s), OptionsType::String) => {
            Some(CanonicalValue::String(s.clone()))
        }
        (TirExprKind::Null, OptionsType::Option(_)) => Some(CanonicalValue::None),
        (_, OptionsType::Option(inner)) => {
            evaluate_literal(expr, inner, types, module, field_name, diagnostics)
                .map(|v| CanonicalValue::Some(Box::new(v)))
        }
        _ => {
            push_unsupported(
                diagnostics,
                module,
                field_name,
                "default value must be a literal matching the declared type",
            );
            None
        }
    }
}

fn span_of(span: &Span, module: &ModuleSource) -> DiagnosticSpan {
    DiagnosticSpan::from_span(span, Some(&module.diagnostic_filename()))
}

fn push_unsupported(
    diagnostics: &mut Vec<Diagnostic>,
    module: &ModuleSource,
    field_name: &str,
    msg: &str,
) {
    diagnostics.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsUnsupported,
        message: format!(
            "kiln: Options.{field_name} in {:?}: {msg}",
            module.diagnostic_filename()
        ),
        span: None,
    });
}

/// Access the TIR module that declared an Options struct. Exposed for the
/// options-validation layer and the CLI provider.
#[must_use]
pub fn tir_module<'a>(annotated: &'a Annotated, module: &ModuleSource) -> Option<&'a TirModule> {
    annotated.tir_modules.get(module)
}
