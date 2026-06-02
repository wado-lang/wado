//! Template string resolution.
//!
//! Resolves sub-expressions and emits `TirExprKind::TemplateString` nodes.
//! The actual expansion into formatting code happens in the synthesis phase
//! (pre-monomorphize). Template expansion emits trait method calls that the
//! monomorphizer resolves to concrete implementations.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{TemplateFormatSpec, TirExpr, TirExprKind, TirTemplatePart};

use super::Elaborator;
use super::types::FunctionContext;

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let string_type = self.get_string_struct_type();
        let span = template.span;

        // Fast path: no interpolations → concatenate at compile time
        let has_interpolation = template
            .parts
            .iter()
            .any(|p| matches!(p, ast::TemplatePart::Interpolation { .. }));

        if !has_interpolation {
            let mut combined = String::new();
            for part in &template.parts {
                if let ast::TemplatePart::String(s) = part {
                    let unescaped = super::util::unescape_template_string(s).unwrap_or_default();
                    combined.push_str(&unescaped);
                }
            }
            return TirExpr::new(TirExprKind::StringLiteral(combined), string_type, span);
        }

        // Fast path: single interpolation, no format spec, already String
        if template.parts.len() == 1
            && let ast::TemplatePart::Interpolation { expr, format: None } = &template.parts[0]
        {
            let resolved = self.resolve_expr(expr, ctx, None);
            if resolved.type_id == string_type {
                return resolved;
            }
        }

        // General case: resolve sub-expressions and emit TemplateString node
        let mut parts = Vec::new();
        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    if !s.is_empty() {
                        let unescaped =
                            super::util::unescape_template_string(s).unwrap_or_default();
                        if !unescaped.is_empty() {
                            parts.push(TirTemplatePart::Literal(unescaped));
                        }
                    }
                }
                ast::TemplatePart::Interpolation { expr, format } => {
                    let resolved = self.resolve_expr(expr, ctx, None);
                    let format_spec = format.as_ref().map(|f| parse_format_spec(&f.spec));
                    parts.push(TirTemplatePart::Interpolation {
                        expr: Box::new(resolved),
                        format_spec,
                    });
                }
            }
        }

        TirExpr::new(TirExprKind::TemplateString { parts }, string_type, span)
    }
}

/// Parse a format specifier string like "05", "<10", "#x", ".2" etc.
/// Syntax: `[[fill]align][sign][#][0][width][.precision]type`
pub fn parse_format_spec(spec: &str) -> TemplateFormatSpec {
    let chars: Vec<char> = spec.chars().collect();
    let len = chars.len();
    let mut i = 0;

    let mut fill = None;
    let mut align = None;
    let mut sign_plus = false;
    let mut alternate = false;
    let mut zero_pad = false;
    let mut width = None;
    let mut precision = None;
    let mut type_char = None;

    // Parse [fill][align]: fill is any char, align is '<', '^', '>'
    if i + 1 < len && matches!(chars[i + 1], '<' | '^' | '>') {
        fill = Some(chars[i]);
        align = Some(chars[i + 1]);
        i += 2;
    } else if i < len && matches!(chars[i], '<' | '^' | '>') {
        align = Some(chars[i]);
        i += 1;
    }

    // Parse [sign]: '+'
    if i < len && chars[i] == '+' {
        sign_plus = true;
        i += 1;
    }

    // Parse [#]: alternate form
    if i < len && chars[i] == '#' {
        alternate = true;
        i += 1;
    }

    // Parse [0]: zero-pad
    if i < len && chars[i] == '0' && (i + 1 >= len || chars[i + 1].is_ascii_digit()) {
        zero_pad = true;
        i += 1;
    }

    // Parse [width]: digits
    let width_start = i;
    while i < len && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i > width_start {
        let w: String = chars[width_start..i].iter().collect();
        width = w.parse().ok();
    }

    // Parse [.precision]: '.' followed by digits. A `None` precision lowers to
    // PRECISION_DEFAULT (sequence Inspect applies its default cap).
    // TODO: add a spec form (e.g. `.!`) for PRECISION_INFINITE so `{x:?}` can
    // opt out of the default cap — there is no surface syntax for it yet.
    if i < len && chars[i] == '.' {
        i += 1;
        let prec_start = i;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i > prec_start {
            let p: String = chars[prec_start..i].iter().collect();
            precision = p.parse().ok();
        } else {
            precision = Some(0);
        }
    }

    // Parse type: b, o, x, X, e, E, ?
    if i < len && matches!(chars[i], 'b' | 'o' | 'x' | 'X' | 'e' | 'E' | '?') {
        type_char = Some(chars[i]);
    }

    TemplateFormatSpec {
        fill,
        align,
        sign_plus,
        alternate,
        zero_pad,
        width,
        precision,
        type_char,
    }
}
