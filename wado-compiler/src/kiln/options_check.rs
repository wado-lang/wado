//! Typed validation of a user-supplied options table against a
//! [`OptionsDescriptor`].
//!
//! Returns a [`CanonicalValue`] tree whose shape mirrors the descriptor, so
//! the downstream encoder in [`crate::kiln::cache`] can walk the pair in
//! lock-step. All diagnostics are batched: a single malformed table surfaces
//! every mismatched / missing / unknown field at once.

use crate::ast::AttrValue;
use crate::compiler_host::{Code, Diagnostic, Severity};
use crate::hashmap::IndexMap;

use super::options::{CanonicalValue, OptionsDescriptor, OptionsField, OptionsType};

/// Wrap a validated options tree together with the descriptor it was built
/// against. The encoder needs both: the descriptor defines field order, the
/// canonical values define content.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalOptions {
    pub descriptor: OptionsDescriptor,
    pub values: Vec<(String, CanonicalValue)>,
}

/// Validate a user-supplied options blob against a descriptor.
///
/// `value` is `None` when the user omitted the options table entirely; each
/// field's declared default is used.
///
/// # Errors
/// Every type mismatch, missing required field, unknown field, and default
/// fallback failure is emitted as a diagnostic. The full list is returned
/// together so the CLI can print them in one shot.
pub fn validate(
    descriptor: &OptionsDescriptor,
    value: Option<&AttrValue>,
) -> Result<CanonicalOptions, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let mut output = Vec::with_capacity(descriptor.fields.len());

    let empty: IndexMap<String, AttrValue> = IndexMap::default();
    let (provided, path): (&IndexMap<String, AttrValue>, String) = match value {
        None => (&empty, "options".to_string()),
        Some(AttrValue::Object(obj)) => (obj, "options".to_string()),
        Some(other) => {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!(
                    "kiln: expected options table, got {}",
                    attr_value_kind(other)
                ),
                span: None,
            });
            return Err(diagnostics);
        }
    };

    for field in &descriptor.fields {
        let field_path = format!("{path}.{}", field.name);
        let canonical = match provided.get(&field.name) {
            Some(supplied) => check_field(field, supplied, &field_path, &mut diagnostics),
            None => apply_default(field, &field_path, &mut diagnostics),
        };
        if let Some(value) = canonical {
            output.push((field.name.clone(), value));
        }
    }

    for key in provided.keys() {
        if !descriptor.fields.iter().any(|f| &f.name == key) {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!("kiln: unknown options field `{path}.{key}`"),
                span: None,
            });
        }
    }

    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(diagnostics);
    }

    Ok(CanonicalOptions {
        descriptor: descriptor.clone(),
        values: output,
    })
}

fn check_field(
    field: &OptionsField,
    supplied: &AttrValue,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CanonicalValue> {
    check_value(&field.ty, supplied, path, diagnostics)
}

fn check_value(
    ty: &OptionsType,
    supplied: &AttrValue,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CanonicalValue> {
    match (ty, supplied) {
        (OptionsType::Bool, AttrValue::Bool(b)) => Some(CanonicalValue::Bool(*b)),
        (OptionsType::I32 | OptionsType::I64, AttrValue::Int(n)) => {
            let bounded = if matches!(ty, OptionsType::I32) {
                if !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(n) {
                    push_mismatch(diagnostics, path, ty, supplied);
                    return None;
                }
                *n
            } else {
                *n
            };
            Some(CanonicalValue::I64(bounded))
        }
        (OptionsType::U32 | OptionsType::U64, AttrValue::Int(n)) => {
            if *n < 0 {
                push_mismatch(diagnostics, path, ty, supplied);
                return None;
            }
            let v = *n as u64;
            if matches!(ty, OptionsType::U32) && v > u64::from(u32::MAX) {
                push_mismatch(diagnostics, path, ty, supplied);
                return None;
            }
            Some(CanonicalValue::U64(v))
        }
        (OptionsType::F32 | OptionsType::F64, AttrValue::Float(f)) => Some(CanonicalValue::F64(*f)),
        (OptionsType::F32 | OptionsType::F64, AttrValue::Int(n)) => {
            Some(CanonicalValue::F64(*n as f64))
        }
        (OptionsType::String, AttrValue::String(s)) => Some(CanonicalValue::String(s.clone())),
        (OptionsType::Enum { variants, name }, AttrValue::String(s)) => {
            if variants.iter().any(|v| v == s) {
                Some(CanonicalValue::Enum(s.clone()))
            } else {
                diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    code: Code::GeneratorOptionsInvalid,
                    message: format!(
                        "kiln: `{path}` expected one of {name}::{{{}}}, got \"{s}\"",
                        variants.join(", ")
                    ),
                    span: None,
                });
                None
            }
        }
        (OptionsType::Option(_inner), AttrValue::String(s)) if s == "null" => {
            // "null" as a string is still a string; fall through to mismatch below.
            push_mismatch(diagnostics, path, ty, supplied);
            None
        }
        (OptionsType::Option(inner), value) => {
            let inner_value = check_value(inner, value, path, diagnostics)?;
            Some(CanonicalValue::Some(Box::new(inner_value)))
        }
        (OptionsType::Struct { descriptor, .. }, AttrValue::Object(obj)) => {
            let nested = descriptor_validate_object(descriptor, obj, path, diagnostics)?;
            Some(CanonicalValue::Struct(nested))
        }
        _ => {
            push_mismatch(diagnostics, path, ty, supplied);
            None
        }
    }
}

fn descriptor_validate_object(
    descriptor: &OptionsDescriptor,
    obj: &IndexMap<String, AttrValue>,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<(String, CanonicalValue)>> {
    let mut out = Vec::with_capacity(descriptor.fields.len());
    let mut any_error = false;
    for field in &descriptor.fields {
        let child_path = format!("{path}.{}", field.name);
        let canonical = match obj.get(&field.name) {
            Some(v) => check_value(&field.ty, v, &child_path, diagnostics),
            None => apply_default(field, &child_path, diagnostics),
        };
        match canonical {
            Some(v) => out.push((field.name.clone(), v)),
            None => any_error = true,
        }
    }
    for key in obj.keys() {
        if !descriptor.fields.iter().any(|f| &f.name == key) {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!("kiln: unknown options field `{path}.{key}`"),
                span: None,
            });
            any_error = true;
        }
    }
    if any_error { None } else { Some(out) }
}

fn apply_default(
    field: &OptionsField,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CanonicalValue> {
    if let Some(default) = &field.default {
        return Some(default.clone());
    }
    if matches!(field.ty, OptionsType::Option(_)) {
        return Some(CanonicalValue::None);
    }
    diagnostics.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsInvalid,
        message: format!(
            "kiln: required options field `{path}` of type {} is missing",
            field.ty.describe()
        ),
        span: None,
    });
    None
}

fn push_mismatch(
    diagnostics: &mut Vec<Diagnostic>,
    path: &str,
    ty: &OptionsType,
    supplied: &AttrValue,
) {
    diagnostics.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsInvalid,
        message: format!(
            "kiln: `{path}` expected {}, got {}",
            ty.describe(),
            attr_value_kind(supplied)
        ),
        span: None,
    });
}

fn attr_value_kind(v: &AttrValue) -> &'static str {
    match v {
        AttrValue::String(_) => "string",
        AttrValue::Int(_) => "integer",
        AttrValue::Float(_) => "float",
        AttrValue::Bool(_) => "bool",
        AttrValue::Array(_) => "array",
        AttrValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiln::options::{CanonicalValue, OptionsDescriptor, OptionsField, OptionsType};
    use crate::token::Span;

    fn field(name: &str, ty: OptionsType, default: Option<CanonicalValue>) -> OptionsField {
        OptionsField {
            name: name.to_string(),
            ty,
            default,
            span: Span::new(0, 0, 1, 1),
        }
    }

    #[test]
    fn missing_table_uses_defaults() {
        let desc = OptionsDescriptor {
            fields: vec![
                field(
                    "enabled",
                    OptionsType::Bool,
                    Some(CanonicalValue::Bool(false)),
                ),
                field(
                    "name",
                    OptionsType::String,
                    Some(CanonicalValue::String("hi".to_string())),
                ),
            ],
        };
        let result = validate(&desc, None).unwrap();
        assert_eq!(
            result.values,
            vec![
                ("enabled".to_string(), CanonicalValue::Bool(false)),
                ("name".to_string(), CanonicalValue::String("hi".to_string())),
            ]
        );
    }

    #[test]
    fn unknown_field_rejected() {
        let desc = OptionsDescriptor {
            fields: vec![field(
                "enabled",
                OptionsType::Bool,
                Some(CanonicalValue::Bool(false)),
            )],
        };
        let mut obj: IndexMap<String, AttrValue> = IndexMap::default();
        obj.insert("enabled".to_string(), AttrValue::Bool(true));
        obj.insert("extra".to_string(), AttrValue::Int(1));
        let err = validate(&desc, Some(&AttrValue::Object(obj))).unwrap_err();
        assert!(err.iter().any(|d| d.message.contains("extra")));
    }

    #[test]
    fn missing_required_field_rejected() {
        let desc = OptionsDescriptor {
            fields: vec![field("name", OptionsType::String, None)],
        };
        let err = validate(&desc, None).unwrap_err();
        assert!(err.iter().any(|d| d.message.contains("required")));
    }

    #[test]
    fn type_mismatch_rejected() {
        let desc = OptionsDescriptor {
            fields: vec![field("enabled", OptionsType::Bool, None)],
        };
        let mut obj: IndexMap<String, AttrValue> = IndexMap::default();
        obj.insert("enabled".to_string(), AttrValue::String("yes".to_string()));
        let err = validate(&desc, Some(&AttrValue::Object(obj))).unwrap_err();
        assert!(err.iter().any(|d| d.message.contains("expected bool")));
    }

    #[test]
    fn option_none_when_omitted() {
        let desc = OptionsDescriptor {
            fields: vec![field(
                "rule",
                OptionsType::Option(Box::new(OptionsType::String)),
                None,
            )],
        };
        let result = validate(&desc, None).unwrap();
        assert_eq!(
            result.values,
            vec![("rule".to_string(), CanonicalValue::None)]
        );
    }

    #[test]
    fn option_wraps_value() {
        let desc = OptionsDescriptor {
            fields: vec![field(
                "rule",
                OptionsType::Option(Box::new(OptionsType::String)),
                None,
            )],
        };
        let mut obj: IndexMap<String, AttrValue> = IndexMap::default();
        obj.insert("rule".to_string(), AttrValue::String("expr".to_string()));
        let result = validate(&desc, Some(&AttrValue::Object(obj))).unwrap();
        assert_eq!(
            result.values,
            vec![(
                "rule".to_string(),
                CanonicalValue::Some(Box::new(CanonicalValue::String("expr".to_string())))
            )]
        );
    }
}
