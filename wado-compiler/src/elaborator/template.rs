//! Annotation pass for template string expressions, tagged and untagged.
//!
//! The body walk records facts only; reify rebuilds the shape from the AST
//! and those facts.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::TypeId;

use super::Elaborator;
use super::types::{FunctionContext, TypeError};

/// A template's static parts: the raw segments — one more than the holes —
/// and each hole's resolved type and specifier, in source order.
pub(super) struct TemplateParts {
    pub segments: Vec<String>,
    pub holes: Vec<(TypeId, Option<String>)>,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        self.resolve_template_parts(template, ctx);
        self.get_string_struct_type()
    }

    /// Walk each interpolation so its facts (`expression_types`, use→def
    /// edges, nested coercion / dispatch) land on `ModuleSemantics`, and
    /// validate each segment's escapes. `None` where a segment is malformed.
    pub(super) fn resolve_template_parts(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TemplateParts> {
        let mut segments = Vec::new();
        let mut holes = Vec::new();
        let mut pending = String::new();
        let mut sound = true;
        for part in &template.parts {
            match part {
                ast::TemplatePart::Interpolation { expr, format } => {
                    let ty = self.resolve_expr(expr, ctx, None);
                    segments.push(std::mem::take(&mut pending));
                    holes.push((
                        self.apply_infer_holes(ty),
                        format.as_ref().map(|f| f.spec.clone()),
                    ));
                }
                // The gate reify relies on: it decodes these segments with no
                // diagnostic channel of its own.
                ast::TemplatePart::String(raw) => {
                    if let Err(message) = super::util::unescape_template_string(raw) {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message,
                            span: template.span,
                        });
                        sound = false;
                    }
                    pending.push_str(raw);
                }
            }
        }
        segments.push(pending);
        sound.then_some(TemplateParts { segments, holes })
    }
}
