//! Typed validation of a user-supplied options table against a
//! [`OptionsDescriptor`].
//!
//! Returns a [`CanonicalValue`] tree whose shape mirrors the descriptor, so
//! the downstream encoder in [`crate::kiln::cache`] can walk the pair in
//! lock-step. All diagnostics are batched: a single malformed table surfaces
//! every mismatched / missing / unknown field at once, each pointing at the
//! key it names — or, where the offender has no key of its own, at the key
//! that owns it ([`OptionsAnchor`]).

use crate::ast::{AttrObject, AttrValue};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::token::Span;

use super::options::{CanonicalValue, OptionsDescriptor, OptionsField, OptionsType};

/// Where the options table was written, so every diagnostic can point at a
/// key instead of at the start of the document.
///
/// `span` is what a diagnostic with no key of its own — a missing required
/// field, a list element, an `options` value that is not a table — is blamed
/// on: the `options:` key, or the `use` clause when the clause wrote none.
#[derive(Debug, Clone, Copy)]
pub struct OptionsAnchor<'a> {
    pub file: &'a str,
    pub span: Span,
}

/// One node of the options tree under validation: its dotted path for the
/// message and the key to squiggle for it.
#[derive(Debug, Clone, Copy)]
struct Site<'a> {
    file: &'a str,
    path: &'a str,
    span: Span,
}

impl<'a> Site<'a> {
    fn child(self, path: &'a str, span: Span) -> Self {
        Site {
            file: self.file,
            path,
            span,
        }
    }

    /// The diagnostic span for `span`, in this site's file.
    fn blame(self, span: Span) -> Option<DiagnosticSpan> {
        Some(DiagnosticSpan::from_span(&span, Some(self.file)))
    }

    /// The diagnostic span for this site's own key.
    fn here(self) -> Option<DiagnosticSpan> {
        self.blame(self.span)
    }
}

/// Wrap a validated options tree together with the descriptor it was built
/// against. The encoder needs both: the descriptor defines field order, the
/// canonical values define content.
#[derive(Debug, Clone, PartialEq, Default)]
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
    anchor: OptionsAnchor<'_>,
) -> Result<CanonicalOptions, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let root = Site {
        file: anchor.file,
        path: "options",
        span: anchor.span,
    };

    let empty = AttrObject::default();
    let provided: &AttrObject = match value {
        None => &empty,
        Some(AttrValue::Object(obj)) => obj,
        Some(other) => {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!(
                    "kiln: expected options table, got {}",
                    attr_value_kind(other)
                ),
                span: root.here(),
            });
            return Err(diagnostics);
        }
    };

    let Some(values) = descriptor_validate_object(descriptor, provided, root, &mut diagnostics)
    else {
        assert!(
            diagnostics.iter().any(|d| d.severity == Severity::Error),
            "a rejected options table must say why"
        );
        return Err(diagnostics);
    };
    Ok(CanonicalOptions {
        descriptor: descriptor.clone(),
        values,
    })
}

fn check_value(
    ty: &OptionsType,
    supplied: &AttrValue,
    site: Site<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CanonicalValue> {
    match (ty, supplied) {
        (OptionsType::Bool, AttrValue::Bool(b)) => Some(CanonicalValue::Bool(*b)),
        (
            OptionsType::I8 | OptionsType::I16 | OptionsType::I32 | OptionsType::I64,
            AttrValue::Int(n),
        ) => {
            let bounds = match ty {
                OptionsType::I8 => Some((i64::from(i8::MIN), i64::from(i8::MAX))),
                OptionsType::I16 => Some((i64::from(i16::MIN), i64::from(i16::MAX))),
                OptionsType::I32 => Some((i64::from(i32::MIN), i64::from(i32::MAX))),
                _ => None,
            };
            if let Some((lo, hi)) = bounds
                && !(lo..=hi).contains(n)
            {
                push_mismatch(diagnostics, site, ty, supplied);
                return None;
            }
            Some(CanonicalValue::I64(*n))
        }
        (
            OptionsType::U8 | OptionsType::U16 | OptionsType::U32 | OptionsType::U64,
            AttrValue::Int(n),
        ) => {
            if *n < 0 {
                push_mismatch(diagnostics, site, ty, supplied);
                return None;
            }
            let v = *n as u64;
            let max = match ty {
                OptionsType::U8 => Some(u64::from(u8::MAX)),
                OptionsType::U16 => Some(u64::from(u16::MAX)),
                OptionsType::U32 => Some(u64::from(u32::MAX)),
                _ => None,
            };
            if let Some(max) = max
                && v > max
            {
                push_mismatch(diagnostics, site, ty, supplied);
                return None;
            }
            Some(CanonicalValue::U64(v))
        }
        (OptionsType::F32 | OptionsType::F64, AttrValue::Float(f)) => {
            if !f.is_finite() {
                diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    code: Code::GeneratorOptionsInvalid,
                    message: format!("kiln: `{}` float value must be finite, got {f}", site.path),
                    span: site.here(),
                });
                return None;
            }
            Some(CanonicalValue::F64(*f))
        }
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
                        "kiln: `{}` expected one of {name}::{{{}}}, got \"{s}\"",
                        site.path,
                        variants.join(", ")
                    ),
                    span: site.here(),
                });
                None
            }
        }
        (OptionsType::Option(_inner), AttrValue::String(s)) if s == "null" => {
            // "null" as a string is still a string; fall through to mismatch below.
            push_mismatch(diagnostics, site, ty, supplied);
            None
        }
        (OptionsType::Option(inner), value) => {
            let inner_value = check_value(inner, value, site, diagnostics)?;
            Some(CanonicalValue::Some(Box::new(inner_value)))
        }
        (OptionsType::Struct { descriptor, .. }, AttrValue::Object(obj)) => {
            let nested = descriptor_validate_object(descriptor, obj, site, diagnostics)?;
            Some(CanonicalValue::Struct(nested))
        }
        (OptionsType::List(inner), AttrValue::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            let mut ok = true;
            for (i, item) in items.iter().enumerate() {
                // An element carries no key of its own: blame the list's key.
                let item_path = format!("{}[{i}]", site.path);
                match check_value(inner, item, site.child(&item_path, site.span), diagnostics) {
                    Some(v) => out.push(v),
                    None => ok = false,
                }
            }
            ok.then_some(CanonicalValue::List(out))
        }
        _ => {
            push_mismatch(diagnostics, site, ty, supplied);
            None
        }
    }
}

fn descriptor_validate_object(
    descriptor: &OptionsDescriptor,
    obj: &AttrObject,
    site: Site<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<(String, CanonicalValue)>> {
    let mut out = Vec::with_capacity(descriptor.fields.len());
    let mut any_error = false;
    for field in &descriptor.fields {
        let child_path = format!("{}.{}", site.path, field.name);
        let canonical = match obj.get(&field.name) {
            Some(entry) => check_value(
                &field.ty,
                &entry.value,
                site.child(&child_path, entry.key_span),
                diagnostics,
            ),
            // Nothing was written: blame the key that owns the object.
            None => apply_default(field, site.child(&child_path, site.span), diagnostics),
        };
        match canonical {
            Some(v) => out.push((field.name.clone(), v)),
            None => any_error = true,
        }
    }
    for (key, entry) in obj {
        if !descriptor.fields.iter().any(|f| &f.name == key) {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!("kiln: unknown options field `{}.{key}`", site.path),
                span: site.blame(entry.key_span),
            });
            any_error = true;
        }
    }
    if any_error { None } else { Some(out) }
}

fn apply_default(
    field: &OptionsField,
    site: Site<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CanonicalValue> {
    if let Some(default) = &field.default {
        return Some(default.clone());
    }
    if matches!(field.ty, OptionsType::Option(_)) {
        return Some(CanonicalValue::None);
    }
    if matches!(field.ty, OptionsType::List(_)) {
        return Some(CanonicalValue::List(Vec::new()));
    }
    diagnostics.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsInvalid,
        message: format!(
            "kiln: required options field `{}` of type {} is missing",
            site.path,
            field.ty.describe()
        ),
        span: site.here(),
    });
    None
}

fn push_mismatch(
    diagnostics: &mut Vec<Diagnostic>,
    site: Site<'_>,
    ty: &OptionsType,
    supplied: &AttrValue,
) {
    diagnostics.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsInvalid,
        message: format!(
            "kiln: `{}` expected {}, got {}",
            site.path,
            ty.describe(),
            attr_value_kind(supplied)
        ),
        span: site.here(),
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
    use crate::ast::AttrEntry;
    use crate::kiln::options::{CanonicalValue, OptionsDescriptor, OptionsField, OptionsType};
    use crate::token::Span;

    const FILE: &str = "src/main.wado";

    /// The `options:` key — where a diagnostic naming no key of its own lands.
    const ANCHOR: (usize, usize) = (2, 5);

    fn anchor() -> OptionsAnchor<'static> {
        OptionsAnchor {
            file: FILE,
            span: Span::new(0, 7, ANCHOR.0, ANCHOR.1),
        }
    }

    /// An attribute entry keyed at `line`:`column`, so a diagnostic about the
    /// key is distinguishable from one anchored on the enclosing `options:`.
    fn entry_at(line: usize, column: usize, value: AttrValue) -> AttrEntry {
        AttrEntry {
            key_span: Span::new(0, 0, line, column),
            value,
        }
    }

    fn entry(value: AttrValue) -> AttrEntry {
        entry_at(3, 9, value)
    }

    /// The file and `(line, column)` a diagnostic squiggles.
    fn at(d: &Diagnostic) -> (&str, usize, usize) {
        let span = d
            .span
            .as_ref()
            .expect("every options diagnostic carries a span");
        (span.file.as_str(), span.line, span.column)
    }

    fn only(diagnostics: &[Diagnostic]) -> &Diagnostic {
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        &diagnostics[0]
    }

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
        let result = validate(&desc, None, anchor()).unwrap();
        assert_eq!(
            result.values,
            vec![
                ("enabled".to_string(), CanonicalValue::Bool(false)),
                ("name".to_string(), CanonicalValue::String("hi".to_string())),
            ]
        );
    }

    #[test]
    fn unknown_field_points_at_the_key() {
        let desc = OptionsDescriptor {
            fields: vec![field(
                "enabled",
                OptionsType::Bool,
                Some(CanonicalValue::Bool(false)),
            )],
        };
        let mut obj = AttrObject::default();
        obj.insert("enabled".to_string(), entry_at(3, 9, AttrValue::Bool(true)));
        obj.insert("extra".to_string(), entry_at(4, 9, AttrValue::Int(1)));
        let err = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap_err();
        let d = only(&err);
        assert!(d.message.contains("extra"), "{}", d.message);
        assert_eq!(at(d), (FILE, 4, 9));
    }

    #[test]
    fn missing_required_field_points_at_the_options_key() {
        let desc = OptionsDescriptor {
            fields: vec![field("name", OptionsType::String, None)],
        };
        let err = validate(&desc, None, anchor()).unwrap_err();
        let d = only(&err);
        assert!(d.message.contains("required"), "{}", d.message);
        assert_eq!(at(d), (FILE, ANCHOR.0, ANCHOR.1));
    }

    #[test]
    fn non_table_options_point_at_the_options_key() {
        let desc = OptionsDescriptor {
            fields: vec![field("name", OptionsType::String, None)],
        };
        let err = validate(
            &desc,
            Some(&AttrValue::String("nope".to_string())),
            anchor(),
        )
        .unwrap_err();
        let d = only(&err);
        assert!(
            d.message.contains("expected options table, got string"),
            "{}",
            d.message
        );
        assert_eq!(at(d), (FILE, ANCHOR.0, ANCHOR.1));
    }

    #[test]
    fn list_field_defaults_to_empty_and_validates_elements() {
        let desc = OptionsDescriptor {
            fields: vec![field(
                "entries",
                OptionsType::List(Box::new(OptionsType::String)),
                None,
            )],
        };
        // Omitted → empty list (a `List` field is optional like `Option`).
        let result = validate(&desc, None, anchor()).unwrap();
        assert_eq!(
            result.values,
            vec![("entries".to_string(), CanonicalValue::List(vec![]))]
        );
        // Supplied array → a `List` of the element type.
        let mut obj = AttrObject::default();
        obj.insert(
            "entries".to_string(),
            entry(AttrValue::Array(vec![
                AttrValue::String("a".to_string()),
                AttrValue::String("b".to_string()),
            ])),
        );
        let result = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap();
        assert_eq!(
            result.values,
            vec![(
                "entries".to_string(),
                CanonicalValue::List(vec![
                    CanonicalValue::String("a".to_string()),
                    CanonicalValue::String("b".to_string()),
                ])
            )]
        );
        // A wrong element type is rejected, on the key that owns the list.
        let mut obj = AttrObject::default();
        obj.insert(
            "entries".to_string(),
            entry_at(6, 13, AttrValue::Array(vec![AttrValue::Int(1)])),
        );
        let err = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap_err();
        let d = only(&err);
        assert!(d.message.contains("`options.entries[0]`"), "{}", d.message);
        assert_eq!(at(d), (FILE, 6, 13));
    }

    #[test]
    fn non_finite_float_points_at_the_key() {
        let desc = OptionsDescriptor {
            fields: vec![field("ratio", OptionsType::F64, None)],
        };
        let mut obj = AttrObject::default();
        obj.insert(
            "ratio".to_string(),
            entry_at(7, 11, AttrValue::Float(f64::INFINITY)),
        );
        let err = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap_err();
        let d = only(&err);
        assert!(d.message.contains("must be finite"), "{}", d.message);
        assert_eq!(at(d), (FILE, 7, 11));
    }

    #[test]
    fn type_mismatch_points_at_the_key() {
        let desc = OptionsDescriptor {
            fields: vec![field("enabled", OptionsType::Bool, None)],
        };
        let mut obj = AttrObject::default();
        obj.insert(
            "enabled".to_string(),
            entry_at(8, 15, AttrValue::String("yes".to_string())),
        );
        let err = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap_err();
        let d = only(&err);
        assert!(d.message.contains("expected bool"), "{}", d.message);
        assert_eq!(at(d), (FILE, 8, 15));
    }

    #[test]
    fn nested_struct_diagnostics_point_at_the_nested_key() {
        let nested = OptionsDescriptor {
            fields: vec![field("depth", OptionsType::U8, None)],
        };
        let desc = OptionsDescriptor {
            fields: vec![field(
                "tuning",
                OptionsType::Struct {
                    name: "tuning".to_string(),
                    descriptor: nested,
                },
                None,
            )],
        };

        // An unknown nested key is blamed on that key.
        let mut inner = AttrObject::default();
        inner.insert("depth".to_string(), entry_at(5, 13, AttrValue::Int(1)));
        inner.insert("dpeth".to_string(), entry_at(6, 13, AttrValue::Int(2)));
        let mut obj = AttrObject::default();
        obj.insert(
            "tuning".to_string(),
            entry_at(4, 9, AttrValue::Object(inner)),
        );
        let err = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap_err();
        let d = only(&err);
        assert!(
            d.message.contains("`options.tuning.dpeth`"),
            "{}",
            d.message
        );
        assert_eq!(at(d), (FILE, 6, 13));

        // A missing nested field has no key: blame the one that owns it.
        let mut obj = AttrObject::default();
        obj.insert(
            "tuning".to_string(),
            entry_at(4, 9, AttrValue::Object(AttrObject::default())),
        );
        let err = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap_err();
        let d = only(&err);
        assert!(
            d.message.contains("`options.tuning.depth`"),
            "{}",
            d.message
        );
        assert_eq!(at(d), (FILE, 4, 9));
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
        let result = validate(&desc, None, anchor()).unwrap();
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
        let mut obj = AttrObject::default();
        obj.insert(
            "rule".to_string(),
            entry(AttrValue::String("expr".to_string())),
        );
        let result = validate(&desc, Some(&AttrValue::Object(obj)), anchor()).unwrap();
        assert_eq!(
            result.values,
            vec![(
                "rule".to_string(),
                CanonicalValue::Some(Box::new(CanonicalValue::String("expr".to_string())))
            )]
        );
    }
}
