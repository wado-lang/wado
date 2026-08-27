//! Annotation pass for template string expressions.
//!
//! The body walk records facts only: each interpolation
//! sub-expression is walked so its types, use→def edges, and any nested
//! coercion / dispatch facts land on `ModuleSemantics`. Reify rebuilds
//! the actual `TirExprKind::TemplateString` / `StringLiteral` shape from
//! the AST and the recorded facts.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::TypeId;

use super::Elaborator;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let string_type = self.get_string_struct_type();

        // Walk each interpolation sub-expression so its facts
        // (`expression_types`, use→def edges, nested coercion / dispatch)
        // land on `ModuleSemantics`. Reify is the sole producer of the
        // template's actual TIR shape (`StringLiteral` /
        // `TemplateString`) from the AST + these facts.
        for part in &template.parts {
            match part {
                ast::TemplatePart::Interpolation { expr, .. } => {
                    self.resolve_expr(expr, ctx, None);
                }
                // The gate reify relies on: it decodes these segments with no
                // diagnostic channel of its own.
                ast::TemplatePart::String(raw) => {
                    if let Err(message) = super::util::unescape_template_string(raw) {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message,
                            span: template.span,
                        });
                    }
                }
            }
        }

        string_type
    }
}
