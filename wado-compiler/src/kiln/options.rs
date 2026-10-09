//! Typed extraction of a Kiln generator's `Options` struct. The driver calls
//! [`extract_options_descriptor`] once per generator component, once its package
//! is type-resolved; the result drives both options validation and the cache-key
//! byte layout. Deliberately strict — only shapes that round-trip through the
//! Component-Model canonical encoder. See WEP 2026-04-12.

use serde::{Deserialize, Serialize};

use crate::ast::Visibility;
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::primitive::PrimitiveType;
use crate::semantics::Semantics;
use crate::symbol::SymbolKind;
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirField, TirModule, TypeId, TypeTable};
use crate::token::Span;

/// Structural description of a generator's `pub struct Options`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OptionsDescriptor {
    pub fields: Vec<OptionsField>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionsField {
    pub name: String,
    pub ty: OptionsType,
    /// The literal default the generator's `Options` declares for this field.
    /// Without one, a field is required unless it is an `Option`, `List` or map.
    pub default: Option<CanonicalValue>,
    /// Source position in the generator source. Not persisted across the
    /// descriptor-cache boundary — diagnostics emitted against a cached
    /// descriptor degrade to a zero span, which is acceptable because the
    /// cache is keyed by source hash and the user still sees the field
    /// name.
    #[serde(skip, default)]
    pub span: Span,
}

/// The set of shapes supported inside a generator's `Options` struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OptionsType {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    String,
    /// `Option<T>` — missing-and-`null` both resolve to `None`.
    Option(Box<OptionsType>),
    /// `List<T>` — a homogeneous array; a missing field resolves to the empty
    /// list, so a `List` option is optional without an explicit default.
    List(Box<OptionsType>),
    /// `TreeMap<String, V>` — an object whose keys are the author's own; a
    /// missing field resolves to the empty map, as a `List` does.
    Map(Box<OptionsType>),
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
            OptionsType::I8 => "i8".to_string(),
            OptionsType::I16 => "i16".to_string(),
            OptionsType::I32 => "i32".to_string(),
            OptionsType::I64 => "i64".to_string(),
            OptionsType::U8 => "u8".to_string(),
            OptionsType::U16 => "u16".to_string(),
            OptionsType::U32 => "u32".to_string(),
            OptionsType::U64 => "u64".to_string(),
            OptionsType::F32 => "f32".to_string(),
            OptionsType::F64 => "f64".to_string(),
            OptionsType::String => "String".to_string(),
            OptionsType::Option(inner) => format!("Option<{}>", inner.describe()),
            OptionsType::List(inner) => format!("List<{}>", inner.describe()),
            OptionsType::Map(inner) => format!("TreeMap<String, {}>", inner.describe()),
            OptionsType::Enum { name, .. } => name.clone(),
            OptionsType::Struct { name, .. } => name.clone(),
        }
    }
}

/// The compiler-side representation of a fully-validated options value.
///
/// Shape mirrors [`OptionsType`] so [`crate::kiln::cache::encode_options_canonical`]
/// can walk the two in lock-step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// An array value; empty when a `List` field was omitted.
    List(Vec<CanonicalValue>),
    /// A map value, sorted by key: the order it was written in is not part of
    /// the value, so the generator and the cache key both see one order.
    Map(Vec<(String, CanonicalValue)>),
}

/// Locate the `pub struct Options` the generator's entry module names, declared
/// there or re-exported, and describe it as an [`OptionsDescriptor`]. `Options` is optional — a generator with no
/// configuration gets an empty descriptor — while `generate` is required.
///
/// # Errors
/// When the module has no TIR or does not export `generate`. A shape failure
/// inside `Options` comes back batched, so one bad field hides no others.
pub fn extract_options_descriptor(
    sem: &Semantics,
    module: &ModuleSource,
) -> Result<OptionsDescriptor, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();

    let Some(tir_module) = sem.tir_modules.get(module) else {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: Code::GeneratorOptionsUnsupported,
            message: format!(
                "kiln: generator module {:?} has no TIR available",
                module.source_path()
            ),
            span: None,
        });
        return Err(diagnostics);
    };

    if tir_module.find_function("generate").is_none() {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: Code::GeneratorOptionsUnsupported,
            message: format!(
                "kiln: generator {:?} does not declare `generate` function",
                module.source_path()
            ),
            span: None,
        });
        return Err(diagnostics);
    }

    // The entry module names `Options`, by declaring it or by re-exporting the
    // declaration, which the generator's other entry points then share.
    let Some(symbol) = sem
        .symbols
        .lookup_in_module(module, "Options")
        .filter(|symbol| matches!(symbol.kind, SymbolKind::Struct(_)))
    else {
        return Ok(OptionsDescriptor { fields: vec![] });
    };
    let declaring = symbol.module_source();
    let options_struct = sem
        .tir_modules
        .get(declaring)
        .and_then(|declared_in| declared_in.find_struct(&symbol.name))
        .expect("a struct symbol has its TIR struct in the module declaring it");

    if !sem
        .symbols
        .effective_visibility_in_module(module, "Options")
        .is_some_and(Visibility::is_public)
    {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: Code::GeneratorOptionsUnsupported,
            message: "kiln: `Options` struct must be declared `pub`".to_string(),
            span: Some(span_of(&options_struct.span, declaring)),
        });
    }

    let mut visiting: IndexSet<(ModuleSource, String)> = IndexSet::default();
    visiting.insert((declaring.clone(), symbol.name.clone()));
    let mut descriptor_fields = Vec::with_capacity(options_struct.fields.len());
    for field in &options_struct.fields {
        // Diagnostic already pushed by lower_field on `None`; continue so
        // every bad field surfaces in one pass.
        if let Some(desc_field) =
            lower_field(field, sem, declaring, &mut visiting, &mut diagnostics)
        {
            descriptor_fields.push(desc_field);
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
    sem: &Semantics,
    module: &ModuleSource,
    visiting: &mut IndexSet<(ModuleSource, String)>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<OptionsField> {
    let ty = lower_type(
        field.type_id,
        sem,
        module,
        &field.name,
        visiting,
        diagnostics,
    )?;
    let default = field
        .default_expr
        .as_deref()
        .and_then(|e| evaluate_literal(e, &ty, &sem.types, module, &field.name, diagnostics));
    Some(OptionsField {
        name: field.name.clone(),
        ty,
        default,
        span: field.span,
    })
}

fn lower_type(
    type_id: TypeId,
    sem: &Semantics,
    module: &ModuleSource,
    field_name: &str,
    visiting: &mut IndexSet<(ModuleSource, String)>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<OptionsType> {
    let types = &sem.types;
    if let Some(inner_id) = types.as_option(type_id) {
        let inner = lower_type(inner_id, sem, module, field_name, visiting, diagnostics)?;
        return Some(OptionsType::Option(Box::new(inner)));
    }

    if let Some(elem_id) = types.as_list(type_id) {
        let inner = lower_type(elem_id, sem, module, field_name, visiting, diagnostics)?;
        return Some(OptionsType::List(Box::new(inner)));
    }

    if let Some((key_id, value_id)) = types.as_tree_map(type_id) {
        if !types.is_string(key_id) {
            push_unsupported(
                diagnostics,
                module,
                field_name,
                &format!(
                    "map key type `{}` is not supported in generator options; a map option is keyed by `String`",
                    types.type_name(key_id)
                ),
            );
            return None;
        }
        let inner = lower_type(value_id, sem, module, field_name, visiting, diagnostics)?;
        return Some(OptionsType::Map(Box::new(inner)));
    }

    match types.get(type_id) {
        ResolvedType::Primitive(p) => match p {
            PrimitiveType::Bool => Some(OptionsType::Bool),
            PrimitiveType::I8 => Some(OptionsType::I8),
            PrimitiveType::I16 => Some(OptionsType::I16),
            PrimitiveType::I32 => Some(OptionsType::I32),
            PrimitiveType::I64 => Some(OptionsType::I64),
            PrimitiveType::U8 => Some(OptionsType::U8),
            PrimitiveType::U16 => Some(OptionsType::U16),
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
        ResolvedType::Struct { .. } if types.is_string(type_id) => Some(OptionsType::String),
        ResolvedType::Struct { def, .. } => {
            let name = &types.struct_head_name(*def);
            let module_source = &types.struct_head_module(*def).clone();
            let nested = nested_struct_descriptor(
                name,
                module_source,
                sem,
                module,
                field_name,
                visiting,
                diagnostics,
            )?;
            Some(OptionsType::Struct {
                name: name.clone(),
                descriptor: nested,
            })
        }
        ResolvedType::Enum { def } => {
            let name = &types.def_name(*def).to_string();
            let module_source = &types.def_module(*def).clone();
            let variants =
                enum_variants(name, module_source, sem, module, field_name, diagnostics)?;
            Some(OptionsType::Enum {
                name: name.clone(),
                variants,
            })
        }
        _ => {
            push_unsupported(
                diagnostics,
                module,
                field_name,
                &format!(
                    "type `{}` is not supported in generator options",
                    types.type_name(type_id)
                ),
            );
            None
        }
    }
}

fn nested_struct_descriptor(
    struct_name: &str,
    struct_module: &ModuleSource,
    sem: &Semantics,
    module: &ModuleSource,
    field_name: &str,
    visiting: &mut IndexSet<(ModuleSource, String)>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<OptionsDescriptor> {
    let key = (struct_module.clone(), struct_name.to_string());
    if visiting.contains(&key) {
        push_unsupported(
            diagnostics,
            module,
            field_name,
            &format!("recursive struct `{struct_name}` is not supported in generator options"),
        );
        return None;
    }

    let tir_module = if let Some(m) = sem.tir_modules.get(struct_module) {
        m
    } else {
        push_unsupported(
            diagnostics,
            module,
            field_name,
            &format!(
                "nested struct `{struct_name}` declaring module {:?} is not available",
                struct_module.source_path()
            ),
        );
        return None;
    };

    let nested_struct = if let Some(s) = tir_module.find_struct(struct_name) {
        s
    } else {
        push_unsupported(
            diagnostics,
            module,
            field_name,
            &format!("nested struct `{struct_name}` not found in its declaring module"),
        );
        return None;
    };

    visiting.insert(key.clone());
    let mut nested_fields = Vec::with_capacity(nested_struct.fields.len());
    let mut any_failed = false;
    for field in &nested_struct.fields {
        match lower_field(field, sem, struct_module, visiting, diagnostics) {
            Some(f) => nested_fields.push(f),
            None => any_failed = true,
        }
    }
    visiting.shift_remove(&key);

    if any_failed {
        return None;
    }

    Some(OptionsDescriptor {
        fields: nested_fields,
    })
}

fn enum_variants(
    enum_name: &str,
    enum_module: &ModuleSource,
    sem: &Semantics,
    module: &ModuleSource,
    field_name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Vec<String>> {
    let tir_module = if let Some(m) = sem.tir_modules.get(enum_module) {
        m
    } else {
        push_unsupported(
            diagnostics,
            module,
            field_name,
            &format!(
                "enum `{enum_name}` declaring module {:?} is not available",
                enum_module.source_path()
            ),
        );
        return None;
    };

    let enum_decl = if let Some(e) = tir_module.find_enum(enum_name) {
        e
    } else {
        push_unsupported(
            diagnostics,
            module,
            field_name,
            &format!("enum `{enum_name}` not found in its declaring module"),
        );
        return None;
    };

    Some(enum_decl.cases.iter().map(|c| c.name.clone()).collect())
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
        (
            TirExprKind::IntLiteral { value, .. },
            OptionsType::I8 | OptionsType::I16 | OptionsType::I32 | OptionsType::I64,
        ) => Some(CanonicalValue::I64(*value as i64)),
        (
            TirExprKind::IntLiteral { value, .. },
            OptionsType::U8 | OptionsType::U16 | OptionsType::U32 | OptionsType::U64,
        ) => Some(CanonicalValue::U64(*value)),
        (TirExprKind::FloatLiteral { value, .. }, OptionsType::F32 | OptionsType::F64) => {
            if !value.is_finite() {
                push_unsupported(
                    diagnostics,
                    module,
                    field_name,
                    &format!("default float value must be finite, got {value}"),
                );
                return None;
            }
            Some(CanonicalValue::F64(*value))
        }
        (TirExprKind::StringLiteral(s), OptionsType::String) => {
            Some(CanonicalValue::String(s.clone()))
        }
        (TirExprKind::Null, OptionsType::Option(_)) => Some(CanonicalValue::None),
        (TirExprKind::EnumConstruct { case_name, .. }, OptionsType::Enum { variants, .. }) => {
            if variants.iter().any(|v| v == case_name) {
                Some(CanonicalValue::Enum(case_name.clone()))
            } else {
                push_unsupported(
                    diagnostics,
                    module,
                    field_name,
                    &format!("default enum case `{case_name}` not in declared variants"),
                );
                None
            }
        }
        (TirExprKind::StructLiteral { fields, .. }, OptionsType::Struct { descriptor, .. }) => {
            let mut out = Vec::with_capacity(descriptor.fields.len());
            for desc_field in &descriptor.fields {
                let matching_field = fields.iter().find(|f| f.name == desc_field.name);
                let value = match matching_field {
                    Some(f) => evaluate_literal(
                        &f.value,
                        &desc_field.ty,
                        types,
                        module,
                        &desc_field.name,
                        diagnostics,
                    )?,
                    None => {
                        if let Some(v) = &desc_field.default {
                            v.clone()
                        } else {
                            push_unsupported(
                                diagnostics,
                                module,
                                field_name,
                                &format!(
                                    "nested default omits `{}` which has no field-level default",
                                    desc_field.name
                                ),
                            );
                            return None;
                        }
                    }
                };
                out.push((desc_field.name.clone(), value));
            }
            Some(CanonicalValue::Struct(out))
        }
        (_, OptionsType::Option(inner)) => {
            evaluate_literal(expr, inner, types, module, field_name, diagnostics)
                .map(|v| CanonicalValue::Some(Box::new(v)))
        }
        (_, OptionsType::List(inner)) if let Some(elements) = coerced_array(expr) => elements
            .iter()
            .map(|e| evaluate_literal(e, inner, types, module, field_name, diagnostics))
            .collect::<Option<Vec<_>>>()
            .map(CanonicalValue::List),
        (_, OptionsType::Map(inner))
            if let Some(pairs) = coerced_array(expr)
                && let Some(pairs) = pairs.iter().map(literal_pair).collect::<Option<Vec<_>>>() =>
        {
            let mut entries = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                let value = evaluate_literal(value, inner, types, module, field_name, diagnostics)?;
                entries.push((key.to_string(), value));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Some(CanonicalValue::Map(entries))
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

/// The elements of a `[…]` or `{ k: v, … }` literal, which literal coercion
/// hands to the target's `From<Array<…>>` (WEP 2026-08-24).
fn coerced_array(expr: &TirExpr) -> Option<&[TirExpr]> {
    match &expr.kind {
        TirExprKind::ArrayLiteral { elements } => Some(elements),
        TirExprKind::Call { args, .. } => match args.split() {
            (None, [arg]) => match &arg.expr.kind {
                TirExprKind::ArrayLiteral { elements } => Some(elements),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// The key and value of one `k: v` member of a key-value literal.
fn literal_pair(pair: &TirExpr) -> Option<(&str, &TirExpr)> {
    let TirExprKind::TupleLiteral { elements } = &pair.kind else {
        return None;
    };
    let [key, value] = elements.as_slice() else {
        return None;
    };
    let TirExprKind::StringLiteral(key) = &key.kind else {
        return None;
    };
    Some((key, value))
}

fn span_of(span: &Span, module: &ModuleSource) -> DiagnosticSpan {
    DiagnosticSpan::from_span(span, Some(&module.source_path()))
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
            module.source_path()
        ),
        span: None,
    });
}

/// Access the TIR module that declared an Options struct. Exposed for the
/// options-validation layer and the CLI provider.
#[must_use]
pub fn tir_module<'a>(sem: &'a Semantics, module: &ModuleSource) -> Option<&'a TirModule> {
    sem.tir_modules.get(module)
}
