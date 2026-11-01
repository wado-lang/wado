//! Annotation pass for template string expressions, tagged and untagged.
//!
//! The body walk records facts only; reify rebuilds the shape from the AST
//! and those facts.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::format_spec::{self, FormatKind};
use crate::synthesis::template::interpolation_dispatch_type;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;

use super::Elaborator;
use super::types::{FunctionContext, TypeError};
use crate::escape::unescape_template_string;

/// Each hole's resolved type and specifier, in source order.
pub(super) struct TemplateParts {
    pub holes: Vec<(TypeId, Option<String>)>,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        if let Some(parts) = self.resolve_template_parts(template, ctx) {
            for ((ty, spec), expr) in parts.holes.iter().zip(template.interpolations()) {
                self.check_hole_format_trait(*ty, spec.as_deref(), expr.span());
            }
        }
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
        let mut holes = Vec::new();
        let mut sound = true;
        for part in &template.parts {
            match part {
                ast::TemplatePart::Interpolation { expr, format, .. } => {
                    let ty = self.resolve_expr(expr, ctx, None);
                    let ty = self.apply_infer_holes(ty);
                    holes.push((ty, format.as_ref().map(|f| f.spec.clone())));
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
                }
            }
        }
        sound.then_some(TemplateParts { holes })
    }

    /// A specifier binds under any tag, whether or not the tag renders the
    /// hole, so the check reads the declared bounds rather than a call.
    pub(super) fn check_hole_format_trait(
        &mut self,
        hole_type: TypeId,
        spec: Option<&str>,
        span: Span,
    ) {
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
        let receiver = interpolation_dispatch_type(
            hole_type,
            kind,
            &self.tysys.trait_env,
            &self.tysys.type_table.borrow(),
        );
        if matches!(
            self.tysys.type_table.borrow().get(receiver),
            ResolvedType::Error | ResolvedType::Unknown
        ) {
            return;
        }
        // A variable an enclosing call has yet to answer (a closure parameter
        // typed by the callee's own parameter) has no bounds to read here.
        if self.tysys.is_unbound_type_param(receiver)
            && !self.scope_type_param_ids().contains(&receiver)
        {
            return;
        }
        let trait_ = self
            .tysys
            .compiler_trait(item)
            .expect("every format kind names a prelude trait");
        if self
            .tysys
            .type_implements_trait(&self.annotate_ctx, receiver, &trait_)
            || self.report_unmet_trait_impl_bound(receiver, &[item], None, span)
        {
            return;
        }
        let (type_name, trait_name) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.name(receiver),
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
