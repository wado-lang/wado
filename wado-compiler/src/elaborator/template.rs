//! Template string annotation.
//!
//! The combined walk now only records facts — it walks each interpolation
//! sub-expression so its types and use→def edges land on `ModuleSemantics`,
//! and returns a placeholder `TirExpr` whose `type_id` is the template's
//! result type (`String`). Reify rebuilds the real `TirExprKind::TemplateString`
//! / `TirExprKind::StringLiteral` shape from the AST and the recorded facts.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{TemplateFormatSpec, TirExpr, TirExprKind};

use super::Elaborator;
use super::types::FunctionContext;

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let string_type = self.get_string_struct_type();

        // Walk each interpolation sub-expression so its `expression_types`,
        // use→def edges, and any nested coercion / dispatch facts land on
        // `ModuleSemantics`. The returned `TirExpr` is discarded by the
        // body-walk's caller; reify is the sole TIR source.
        for part in &template.parts {
            if let ast::TemplatePart::Interpolation { expr, .. } = part {
                let _ = self.resolve_expr(expr, ctx, None);
            }
        }

        TirExpr::new(TirExprKind::Unit, string_type, template.span)
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
