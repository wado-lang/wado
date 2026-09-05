//! Annotation of a tagged template literal (WEP 2026-01-10): the tag called
//! on the anonymous type the template's shape mints.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{TemplateHole, TemplateShape, TypeId, TypeTable};
use crate::unparse::unparse_expr_source;

use super::Elaborator;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_tagged_template(
        &mut self,
        tagged: &ast::TaggedTemplateExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        let Some(shape) = self.resolve_template_shape(&tagged.template, ctx) else {
            return TypeTable::ERROR;
        };

        let template_ty = self.template_type_of(shape, tagged.id, tagged.span);
        self.sem
            .types
            .tagged_templates
            .insert(tagged.id, template_ty);

        let call = ast::CallExpr {
            id: tagged.id,
            callee: tagged.tag.clone(),
            type_args: Vec::new(),
            args: Vec::new(),
            has_trailing_comma: false,
            span: tagged.span,
        };
        let result =
            self.resolve_call_with_args(&call, ctx, expected_type, Some(vec![template_ty]));

        // Reify rebuilds the call from the dispatch fact, so a path that
        // records none is a tag reify cannot build: a variant case, a
        // closure-typed binding, an abstract `T::method`. Each resolves as
        // some other call kind, which either accepts the template silently or
        // reports an arity it never had.
        if !self
            .sem
            .types
            .static_method_dispatch
            .contains_key(&tagged.id)
        {
            let _ = self.emit(TypeError::InvalidLiteral {
                message: "a template tag must name a function or a static method".to_string(),
                span: tagged.tag.span(),
            });
            return TypeTable::ERROR;
        }
        result
    }

    /// The template's shape, or `None` where a hole cannot be a member of one:
    /// an error, an undecided inference variable, or a type parameter of the
    /// enclosing item. A hole also carries its source text, which only a tag
    /// reads.
    fn resolve_template_shape(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TemplateShape> {
        let parts = self.resolve_template_parts(template, ctx)?;
        let mut sound = true;
        let holes: Vec<TemplateHole> = parts
            .holes
            .into_iter()
            .zip(template.interpolations())
            .map(|((ty, spec), expr)| {
                sound &= self.hole_type_admissible(ty, expr.span());
                TemplateHole {
                    ty,
                    spec,
                    source: unparse_expr_source(expr),
                }
            })
            .collect();
        sound.then_some(TemplateShape {
            segments: parts.segments,
            holes,
        })
    }

    /// Whether `ty` can be a hole of a shape: decided, and free of the
    /// enclosing item's type parameters (a known gap of the WEP).
    fn hole_type_admissible(&mut self, ty: TypeId, span: crate::token::Span) -> bool {
        if ty == TypeTable::ERROR || ty == TypeTable::UNKNOWN {
            return false;
        }
        if self.type_has_infer_hole(ty) {
            let _ = self.emit(TypeError::CannotInferType {
                message: "cannot infer the type of this template hole; annotate the value"
                    .to_string(),
                span,
            });
            return false;
        }
        if self.tysys.type_table.borrow().contains_type_param(ty) {
            let type_name = self.tysys.type_table.borrow().type_name(ty);
            let _ = self.emit(TypeError::InvalidLiteral {
                message: format!(
                    "a tagged template hole of type `{type_name}` mentions a type parameter; \
                     a template's type is minted per shape and cannot be generic over the \
                     enclosing item"
                ),
                span,
            });
            return false;
        }
        true
    }

    /// The anonymous type `shape` denotes, minted on its first sighting in
    /// this module.
    fn template_type_of(
        &mut self,
        shape: TemplateShape,
        defined_at: ast::AstId,
        span: crate::token::Span,
    ) -> TypeId {
        let fields: Vec<(String, TypeId)> = {
            let mut tt = self.tysys.type_table.borrow_mut();
            shape
                .holes
                .iter()
                .enumerate()
                .map(|(k, hole)| (TemplateShape::field_name(k), tt.hole_field_type(hole.ty)))
                .collect()
        };
        let (id, name, existing) = {
            let mut tt = self.tysys.type_table.borrow_mut();
            let id = tt.intern_template_shape(self.current_module_source.clone(), shape);
            let existing = tt.find_struct_type(crate::tir::StructDef::Anon(id));
            (id, tt.anon_struct_mangle(id), existing)
        };
        match existing {
            Some(ty) => ty,
            None => self.mint_anonymous_struct(id, &name, &fields, defined_at, span),
        }
    }
}
