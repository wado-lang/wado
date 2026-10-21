//! Annotation pass for template string expressions, tagged and untagged.
//!
//! The body walk records facts only; reify rebuilds the shape from the AST
//! and those facts.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::format_spec::{self, FormatKind};
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;

use super::Elaborator;
use super::types::{FunctionContext, TypeError};
use crate::escape::unescape_template_string;

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
                ast::TemplatePart::Interpolation { expr, format, .. } => {
                    let ty = self.resolve_expr(expr, ctx, None);
                    let ty = self.apply_infer_holes(ty);
                    let spec = format.as_ref().map(|f| f.spec.clone());
                    self.check_hole_format_trait(ty, spec.as_deref(), expr.span());
                    segments.push(std::mem::take(&mut pending));
                    holes.push((ty, spec));
                }
                // The gate reify relies on: it decodes these segments with no
                // diagnostic channel of its own.
                ast::TemplatePart::String(raw) => {
                    if let Err(message) = unescape_template_string(raw) {
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

    /// A specifier binds under any tag, whether or not the tag renders the
    /// hole, so the check reads the declared bounds rather than a call.
    fn check_hole_format_trait(&mut self, hole_type: TypeId, spec: Option<&str>, span: Span) {
        // A malformed spec is the parser's to report.
        let kind = match spec.map(format_spec::parse) {
            None => FormatKind::Display,
            Some(Ok(spec)) => spec.kind,
            Some(Err(_)) => return,
        };
        if kind == FormatKind::Inspect {
            return;
        }
        let item = kind.trait_item();
        let receiver = self.tysys.type_table.borrow().peel_refs(hole_type);
        if matches!(
            self.tysys.type_table.borrow().get(receiver),
            ResolvedType::Error | ResolvedType::Unknown
        ) {
            return;
        }
        let trait_ = self
            .tysys
            .compiler_trait(item)
            .expect("every format kind names a prelude trait");
        if self.tysys.type_implements_trait(
            &self.annotate_ctx,
            &self.type_lookup(),
            receiver,
            &trait_,
        ) {
            return;
        }
        let (type_name, trait_name) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.type_name(receiver),
                tt.compiler_items().trait_name(item).to_string(),
            )
        };
        let _ = self.emit(TypeError::TemplateHoleMissingTrait {
            type_name,
            trait_name,
            is_display: item == CompilerItem::Display,
            span,
        });
    }
}
