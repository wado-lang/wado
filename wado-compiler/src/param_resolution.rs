//! Compile-time parameter resolution (`#[param]`).
//!
//! Runs after link and before monomorphize/lower. For each `#[param]` global it
//! resolves an override (highest priority first: `-D NAME=value`, then the
//! `from_env` environment variable) and, on success, replaces the global's
//! initializer with the converted literal. The rewritten global then flows
//! through the ordinary pipeline — scalar literals are eligible for Constant
//! Global Promotion, `String` uses the existing lazy-init path — so no new
//! optimization is needed.
//!
//! v1 converts override strings to the declared type natively in Rust (after a
//! trim), matching the built-in impls of `LenientFromStr` (radix prefixes, `_`
//! separators, `nan`/`inf`, `1`/`0` for `bool`). v2 swaps this native path for
//! evaluating the trait via wasm-CTFE, lifting the built-in-only restriction.
//! The conversion is isolated in [`convert_builtin`] so only that boundary
//! changes. See `wep-2026-04-26-compile-time-params.md`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_host::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};
use crate::compiler_item::CompilerItem;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::logger::{Bail, Logger};
use crate::lower::wide_int_literal::{create_i128_literal, create_u128_literal};
use crate::tir::{TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

/// Severity for one class of param-resolution diagnostic, set per `wado`
/// invocation (`--param-unknown` / `--param-invalid` / `--param-missing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamPolicyLevel {
    /// Fail the build.
    Error,
    /// Diagnose and fall back to the initializer (or ignore the stray `-D`).
    Warn,
    /// Fall back silently.
    Ignore,
}

impl ParamPolicyLevel {
    /// Parse a CLI level string (`error` / `warn` / `ignore`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "ignore" => Some(Self::Ignore),
            _ => None,
        }
    }
}

/// The three resolution-policy levels. Defaults are strict where a mistake is
/// likely (`unknown`, `invalid`), lenient where the default is normal
/// (`missing`).
#[derive(Debug, Clone, Copy)]
pub struct ParamPolicy {
    /// `-D NAME=value` matching no `#[param]`.
    pub unknown: ParamPolicyLevel,
    /// Override resolved but unconvertible to the declared type.
    pub invalid: ParamPolicyLevel,
    /// No override; the initializer would be used.
    pub missing: ParamPolicyLevel,
}

impl Default for ParamPolicy {
    fn default() -> Self {
        Self {
            unknown: ParamPolicyLevel::Error,
            invalid: ParamPolicyLevel::Error,
            missing: ParamPolicyLevel::Ignore,
        }
    }
}

/// Resolve every `#[param]` global in `flat` against `overrides` / env / policy.
///
/// Returns `Err(Bail)` if any diagnostic was emitted at `error` level.
pub fn resolve_params<H: CompilerHost>(
    flat: &mut FlatPackage,
    overrides: &IndexMap<String, String>,
    policy: &ParamPolicy,
    file: &str,
    logger: &Logger<'_, H>,
) -> Result<(), Bail> {
    // Nothing to resolve and no stray `-D` to flag — skip the type-table work.
    if overrides.is_empty() && flat.globals.iter().all(|g| g.param.is_none()) {
        return Ok(());
    }

    let type_table = flat.type_table.clone();
    // `String` / `i128` / `u128` are library struct types (not primitive
    // `TypeId`s), so resolve their concrete ids once for comparison.
    let builtins = {
        let mut tt = type_table.borrow_mut();
        BuiltinParamTypes {
            string: tt.make_compiler_struct(CompilerItem::String),
            i128: tt.make_compiler_struct(CompilerItem::I128),
            u128: tt.make_compiler_struct(CompilerItem::U128),
        }
    };

    // Declared parameter names, for unknown-`-D` detection (flat namespace
    // across the whole compilation unit).
    let declared: IndexSet<String> = flat
        .globals
        .iter()
        .filter_map(|g| g.param.as_ref().map(|p| p.name.clone()))
        .collect();

    let mut had_error = false;
    let mut emit = |level: ParamPolicyLevel, code: Code, message: String, span: Option<Span>| {
        let diag_span = span.map(|s| DiagnosticSpan::from_span(&s, Some(file)));
        match level {
            ParamPolicyLevel::Error => {
                had_error = true;
                let _ = logger.error(Diagnostic {
                    severity: Severity::Error,
                    code,
                    message,
                    span: diag_span,
                });
            }
            ParamPolicyLevel::Warn => match diag_span {
                Some(s) => logger.warn_at(code, message, s),
                None => logger.warn(code, message),
            },
            ParamPolicyLevel::Ignore => {}
        }
    };

    for global in &mut flat.globals {
        let Some(spec) = global.param.clone() else {
            continue;
        };

        // v1 supports built-in scalar types only — regardless of any override.
        if !is_supported_type(global.ty, &builtins) {
            let type_name = type_table.borrow().type_name(global.ty);
            emit(
                ParamPolicyLevel::Error,
                Code::ParamAttr,
                format!("#[param] on {type_name}: only built-in types are supported in v1"),
                Some(global.span),
            );
            continue;
        }

        // `-D` takes precedence over `from_env`.
        let (raw, from_env_name) = match overrides.get(&spec.name) {
            Some(value) => (Some(value.clone()), None),
            None => match &spec.from_env {
                Some(env) => (logger.host().env_var(env), Some(env.clone())),
                None => (None, None),
            },
        };

        let Some(raw) = raw else {
            emit(
                policy.missing,
                Code::ParamMissing,
                format!("compile-time parameter {} was not provided", spec.name),
                Some(global.span),
            );
            continue;
        };

        let trimmed = raw.trim();
        if let Some(literal) =
            convert_builtin(trimmed, global.ty, &builtins, &type_table, global.span)
        {
            global.initializer = literal;
        } else {
            let type_name = type_table.borrow().type_name(global.ty);
            let origin = match &from_env_name {
                Some(env) => format!("environment variable {env}"),
                None => format!("parameter {}", spec.name),
            };
            emit(
                policy.invalid,
                Code::ParamInvalid,
                format!("cannot parse \"{trimmed}\" as {type_name} for {origin}"),
                Some(global.span),
            );
        }
    }

    for name in overrides.keys() {
        if !declared.contains(name) {
            emit(
                policy.unknown,
                Code::ParamUnknown,
                format!("unknown compile-time parameter: {name}"),
                None,
            );
        }
    }

    if had_error { Err(Bail) } else { Ok(()) }
}

/// Concrete `TypeId`s of the library scalar types (`String` / `i128` / `u128`),
/// resolved once per compilation since they are structs, not primitive ids.
struct BuiltinParamTypes {
    string: TypeId,
    i128: TypeId,
    u128: TypeId,
}

/// Whether `ty` is a built-in scalar `#[param]` supports in v1.
fn is_supported_type(ty: TypeId, builtins: &BuiltinParamTypes) -> bool {
    ty == builtins.string
        || ty == builtins.i128
        || ty == builtins.u128
        || matches!(
            ty,
            TypeTable::I8
                | TypeTable::I16
                | TypeTable::I32
                | TypeTable::I64
                | TypeTable::U8
                | TypeTable::U16
                | TypeTable::U32
                | TypeTable::U64
                | TypeTable::F32
                | TypeTable::F64
                | TypeTable::BOOL
                | TypeTable::CHAR
        )
}

/// Convert a trimmed override string to a typed literal initializer.
///
/// Returns `None` for an unconvertible value (handled per `--param-invalid`).
/// The caller guarantees `ty` is a supported built-in scalar.
fn convert_builtin(
    raw: &str,
    ty: TypeId,
    builtins: &BuiltinParamTypes,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Option<TirExpr> {
    if ty == builtins.string {
        return Some(TirExpr::new(
            TirExprKind::StringLiteral(raw.to_string()),
            builtins.string,
            span,
        ));
    }
    if ty == builtins.i128 {
        let v = parse_lenient_int(raw)?;
        return Some(create_i128_literal(v, ty, &type_table.borrow(), span));
    }
    if ty == builtins.u128 {
        let v = parse_lenient_uint(raw)?;
        return Some(create_u128_literal(v, ty, &type_table.borrow(), span));
    }
    match ty {
        TypeTable::BOOL => {
            let b = match raw.to_ascii_lowercase().as_str() {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return None,
            };
            Some(TirExpr::new(
                TirExprKind::BoolLiteral(b),
                TypeTable::BOOL,
                span,
            ))
        }
        TypeTable::CHAR => {
            let mut chars = raw.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(TirExpr::new(
                TirExprKind::CharLiteral(c),
                TypeTable::CHAR,
                span,
            ))
        }
        // f32/f64 both store the f64 parse; codegen narrows for `f32`, matching
        // how reify builds float literals (`reify_literal`).
        TypeTable::F32 => Some(float_literal(
            parse_lenient_float(raw)?,
            raw,
            TypeTable::F32,
            span,
        )),
        TypeTable::F64 => Some(float_literal(
            parse_lenient_float(raw)?,
            raw,
            TypeTable::F64,
            span,
        )),
        TypeTable::I8 | TypeTable::I16 | TypeTable::I32 | TypeTable::I64 => {
            let v = parse_lenient_int(raw)?;
            if !signed_fits(v, ty) {
                return None;
            }
            Some(int_literal(v as u64, raw, ty, span))
        }
        TypeTable::U8 | TypeTable::U16 | TypeTable::U32 | TypeTable::U64 => {
            let v = parse_lenient_uint(raw)?;
            if !unsigned_fits(v, ty) {
                return None;
            }
            Some(int_literal(v as u64, raw, ty, span))
        }
        // `TypeId` is not an enum, so the catch-all is unavoidable. The caller
        // gates on `is_supported_type`, so any other type reaching here is a
        // drift between the two lists — surface it rather than silently
        // reporting every override as invalid.
        _ => unreachable!("convert_builtin reached unsupported type id {ty:?}"),
    }
}

fn int_literal(value: u64, repr: &str, ty: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value,
            repr: repr.to_string(),
        },
        ty,
        span,
    )
}

fn float_literal(value: f64, repr: &str, ty: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::FloatLiteral {
            value,
            repr: repr.to_string(),
        },
        ty,
        span,
    )
}

fn signed_fits(v: i128, ty: TypeId) -> bool {
    match ty {
        TypeTable::I8 => i8::try_from(v).is_ok(),
        TypeTable::I16 => i16::try_from(v).is_ok(),
        TypeTable::I32 => i32::try_from(v).is_ok(),
        TypeTable::I64 => i64::try_from(v).is_ok(),
        _ => false,
    }
}

fn unsigned_fits(v: u128, ty: TypeId) -> bool {
    match ty {
        TypeTable::U8 => u8::try_from(v).is_ok(),
        TypeTable::U16 => u16::try_from(v).is_ok(),
        TypeTable::U32 => u32::try_from(v).is_ok(),
        TypeTable::U64 => u64::try_from(v).is_ok(),
        _ => false,
    }
}

/// Parse a lenient signed integer: optional sign, `0x`/`0o`/`0b` radix prefix
/// (case-insensitive), `_` separators anywhere in the digit body.
fn parse_lenient_int(s: &str) -> Option<i128> {
    let (sign, rest) = split_sign(s);
    let cleaned = rest.replace('_', "");
    let (radix, digits) = split_radix(&cleaned)?;
    if sign == "-" {
        i128::from_str_radix(&format!("-{digits}"), radix).ok()
    } else {
        i128::from_str_radix(digits, radix).ok()
    }
}

/// Parse a lenient unsigned integer. A leading `-` is rejected.
fn parse_lenient_uint(s: &str) -> Option<u128> {
    let (sign, rest) = split_sign(s);
    if sign == "-" {
        return None;
    }
    let cleaned = rest.replace('_', "");
    let (radix, digits) = split_radix(&cleaned)?;
    u128::from_str_radix(digits, radix).ok()
}

/// Parse a lenient float: `_` separators stripped, then Rust's `f64` parser
/// (decimal, exponent, `nan` / `inf` / `infinity`, all case-insensitive).
fn parse_lenient_float(s: &str) -> Option<f64> {
    s.replace('_', "").parse::<f64>().ok()
}

/// Split an optional leading `+` / `-` sign, returning `(sign, rest)`.
fn split_sign(s: &str) -> (&str, &str) {
    if let Some(rest) = s.strip_prefix('-') {
        ("-", rest)
    } else if let Some(rest) = s.strip_prefix('+') {
        ("+", rest)
    } else {
        ("", s)
    }
}

/// Split a radix prefix off the (separator-stripped) digit body, returning
/// `(radix, digits)`. `None` when the body is empty.
fn split_radix(body: &str) -> Option<(u32, &str)> {
    if body.is_empty() {
        return None;
    }
    let radix = if let Some(rest) = strip_prefix_ci(body, "0x") {
        return Some((16, rest));
    } else if let Some(rest) = strip_prefix_ci(body, "0o") {
        return Some((8, rest));
    } else if let Some(rest) = strip_prefix_ci(body, "0b") {
        return Some((2, rest));
    } else {
        10
    };
    Some((radix, body))
}

/// Strip a 2-char ASCII prefix case-insensitively.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = s.as_bytes();
    let pre = prefix.as_bytes();
    if bytes.len() >= pre.len()
        && bytes[..pre.len()].eq_ignore_ascii_case(pre)
        && bytes.len() > pre.len()
    {
        Some(&s[pre.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_radix_separators_and_sign() {
        assert_eq!(parse_lenient_int("42"), Some(42));
        assert_eq!(parse_lenient_int("-42"), Some(-42));
        assert_eq!(parse_lenient_int("+42"), Some(42));
        assert_eq!(parse_lenient_int("1_000"), Some(1000));
        assert_eq!(parse_lenient_int("0xFF"), Some(255));
        assert_eq!(parse_lenient_int("0XfF"), Some(255));
        assert_eq!(parse_lenient_int("0o17"), Some(15));
        assert_eq!(parse_lenient_int("0b1010"), Some(10));
        assert_eq!(parse_lenient_int("-0x10"), Some(-16));
        // A leading zero is not octal.
        assert_eq!(parse_lenient_int("010"), Some(10));
        assert_eq!(parse_lenient_int("0xFF_FF"), Some(0xFFFF));
        assert_eq!(parse_lenient_int(""), None);
        assert_eq!(parse_lenient_int("0x"), None);
        assert_eq!(parse_lenient_int("forty-two"), None);
        assert_eq!(parse_lenient_int("3.14"), None);
    }

    #[test]
    fn unsigned_rejects_negative() {
        assert_eq!(parse_lenient_uint("255"), Some(255));
        assert_eq!(parse_lenient_uint("0xff"), Some(255));
        assert_eq!(parse_lenient_uint("-1"), None);
    }

    #[test]
    fn width_range_checks() {
        assert!(signed_fits(127, TypeTable::I8));
        assert!(!signed_fits(128, TypeTable::I8));
        assert!(signed_fits(-128, TypeTable::I8));
        assert!(!signed_fits(-129, TypeTable::I8));
        assert!(unsigned_fits(255, TypeTable::U8));
        assert!(!unsigned_fits(256, TypeTable::U8));
    }

    #[test]
    fn float_accepts_inf_nan_and_separators() {
        assert_eq!(parse_lenient_float("2.5"), Some(2.5));
        assert_eq!(parse_lenient_float("1_000.5"), Some(1000.5));
        assert_eq!(parse_lenient_float("1e3"), Some(1000.0));
        assert_eq!(parse_lenient_float("inf"), Some(f64::INFINITY));
        assert_eq!(parse_lenient_float("-INFINITY"), Some(f64::NEG_INFINITY));
        assert!(parse_lenient_float("NaN").is_some_and(f64::is_nan));
        assert_eq!(parse_lenient_float("forty-two"), None);
    }
}
