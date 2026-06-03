//! Numeric literal coercion and type coercion.

use super::Elaborator;
use super::types::{FunctionContext, TypeError};
use super::util;
use crate::ast::{self, Expr, Literal, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName};
use crate::tir::{CallArg, FunctionRef, ResolvedType, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

/// Body-walk placeholder for a successful coercion. The combined walk no
/// longer builds the coercion's actual TIR (Stage 7-B); reify reads the
/// recorded `CoercionChoice` + `SequenceCoercionFacts` /
/// `KeyValueCoercionFacts` and emits the real expansion. The returned
/// `TirExpr` only needs the right `type_id` + `span` for the caller's
/// outer typecheck / `expression_types` recording.
fn placeholder(target_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(TirExprKind::Unit, target_type, span)
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Coerce a numeric literal (or negated numeric literal) to the
    /// expected integer / float / i128 / u128 type. On success records
    /// the coercion *and* the resolved expression types at every visited
    /// AST id (the outer literal, plus the inner literal for the
    /// `-NUM` shape) so callers that bypass [`Self::resolve_expr`]
    /// (e.g. [`Self::recoerce_literal_args`] after post-inference
    /// type-arg substitution, or [`Self::try_coerce`]) still leave a
    /// complete annotation trail for the future `reify` pass.
    pub(super) fn try_coerce_numeric_literal(
        &mut self,
        expr: &Expr,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        let coerced = self.try_coerce_numeric_literal_inner(expr, target_type)?;
        self.record_coercion(
            expr.id(),
            super::sem::types::CoercionKind::NumericLiteral,
            target_type,
        );
        self.record_expression_type(expr.id(), target_type);
        // `-NUM` consumes both the outer Unary node and the inner Literal
        // node directly (no recursive resolve_expr fires on the inner),
        // so record the inner literal's resolved type explicitly. Both
        // tokens render the same coerced numeric type to LSP hover.
        if let Expr::Unary(unary) = expr
            && let Expr::Literal(inner_lit) = &unary.expr
        {
            self.record_expression_type(inner_lit.id, target_type);
        }
        Some(coerced)
    }

    fn try_coerce_numeric_literal_inner(
        &mut self,
        expr: &Expr,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        // Number literal coercion to integer
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && self.tysys.type_table.borrow().is_integer(target_type)
        {
            if util::is_float_only_literal(repr) {
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '{repr}' as integer (has decimal point or negative exponent)"
                    ),
                    span: lit.span,
                });
                return Some(placeholder(target_type, lit.span));
            }
            return Some(match util::parse_u128_literal(repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_positive(
                        value,
                        target_type,
                        &self.tysys.type_table.borrow(),
                        repr,
                    ) {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: lit.span,
                        });
                    }
                    placeholder(target_type, lit.span)
                }
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    placeholder(target_type, lit.span)
                }
            });
        }

        // Negated number literal coercion to integer: -42 as i64
        if let Expr::Unary(unary) = expr
            && unary.op == UnaryOp::Neg
            && let Expr::Literal(lit) = &unary.expr
            && let Literal::Number(repr) = &lit.value
            && self.tysys.type_table.borrow().is_integer(target_type)
        {
            if util::is_float_only_literal(repr) {
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '-{repr}' as integer (has decimal point or negative exponent)"
                    ),
                    span: unary.span,
                });
                return Some(placeholder(target_type, unary.span));
            }
            return Some(match util::parse_u128_literal(repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_negative(
                        value,
                        target_type,
                        &self.tysys.type_table.borrow(),
                        repr,
                    ) {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: unary.span,
                        });
                    }
                    placeholder(target_type, unary.span)
                }
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    placeholder(target_type, unary.span)
                }
            });
        }

        // Number literal coercion to float
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && self.tysys.type_table.borrow().is_float(target_type)
        {
            return Some(match util::parse_float_literal(repr) {
                Ok(_) => placeholder(target_type, lit.span),
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    placeholder(target_type, lit.span)
                }
            });
        }

        // Negated number literal coercion to float: -3.14 as f32
        if let Expr::Unary(unary) = expr
            && unary.op == UnaryOp::Neg
            && let Expr::Literal(lit) = &unary.expr
            && let Literal::Number(repr) = &lit.value
            && self.tysys.type_table.borrow().is_float(target_type)
        {
            return Some(match util::parse_float_literal(repr) {
                Ok(_) => placeholder(target_type, unary.span),
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    placeholder(target_type, unary.span)
                }
            });
        }

        // i128/u128 literal coercion
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && !util::is_float_only_literal(repr)
        {
            let struct_name = match self.tysys.type_table.borrow().get(target_type).clone() {
                ResolvedType::Struct { name, .. } => Some(name),
                _ => None,
            };

            if let Some(name) = struct_name
                && (name == "u128" || name == "i128")
            {
                let parse_result = if name == "u128" {
                    util::parse_u128_literal(repr).map(|v| v as i128)
                } else {
                    util::parse_i128_literal(repr)
                };

                match parse_result {
                    Ok(_) => {
                        return Some(placeholder(target_type, lit.span));
                    }
                    Err(_) => {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: format!("invalid {name} literal: {repr}"),
                            span: lit.span,
                        });
                    }
                }
            }
        }

        // Negated i128 literal: -100 as i128
        if let Expr::Unary(unary) = expr
            && unary.op == ast::UnaryOp::Neg
            && let Expr::Literal(lit) = &unary.expr
            && let Literal::Number(repr) = &lit.value
            && !util::is_float_only_literal(repr)
        {
            let struct_name = match self.tysys.type_table.borrow().get(target_type).clone() {
                ResolvedType::Struct { name, .. } => Some(name),
                _ => None,
            };

            if let Some(name) = struct_name
                && name == "i128"
            {
                let negated_repr = format!("-{repr}");
                if util::parse_i128_literal(&negated_repr).is_ok() {
                    return Some(placeholder(target_type, unary.span));
                }
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!("invalid i128 literal: -{repr}"),
                    span: unary.span,
                });
            }
        }

        None
    }

    /// Re-coerce numeric literal arguments to inferred parameter types.
    ///
    /// When a generic call has its type arguments inferred (e.g. `two<T>(1 as u8, 2)`),
    /// numeric literal arguments resolved before inference may have picked up the
    /// default `i32`/`f64` type because the expected type was an unsubstituted
    /// `TypeParam`. After inference, we know the concrete `T`, so any literal arg
    /// whose corresponding parameter is now a numeric type should be re-coerced.
    ///
    /// `expected_param_types` should be the parameter types after substituting the
    /// inferred type arguments.
    pub(super) fn recoerce_literal_args(
        &mut self,
        raw_args: &[Expr],
        args: &mut [TirExpr],
        expected_param_types: &[TypeId],
    ) {
        for (i, arg) in args.iter_mut().enumerate() {
            let Some(raw) = raw_args.get(i) else {
                continue;
            };
            let Some(&expected) = expected_param_types.get(i) else {
                continue;
            };
            if !Self::is_literal_number_arg(Some(raw)) {
                continue;
            }
            if arg.type_id == expected {
                continue;
            }
            let is_numeric = {
                let tt = self.tysys.type_table.borrow();
                tt.is_integer(expected) || tt.is_float(expected)
            };
            if !is_numeric {
                continue;
            }
            // try_coerce_numeric_literal records `expression_types` for
            // every visited AST id (outer + inner `-NUM` literal) with
            // the new `expected` type, so the post-inference re-coercion
            // overwrites the stale pre-inference type that the original
            // resolve_expr wrapper wrote.
            if let Some(coerced) = self.try_coerce_numeric_literal(raw, expected) {
                *arg = coerced;
            }
        }
    }

    /// Try to coerce an expression to match the expected type.
    /// Handles numeric literals, null, string newtypes, and tuple-to-array coercion.
    /// Returns `None` if no coercion applies.
    ///
    /// Stage 4 of WEP 2026-05-26: each successful branch records the
    /// chosen [`super::sem::types::CoercionKind`] in
    /// [`super::sem::types::TypeAnnotations::coercions`] keyed by
    /// `expr.id()`. The variants that fan out into shared
    /// `try_coerce_*` sub-helpers (numeric / tuple-to-sequence /
    /// struct-to-map) record from inside those helpers so direct callers
    /// (`resolve_cast`, struct-field deferred-coercion, `resolve_let`,
    /// `recoerce_literal_args`) record uniformly. The variants
    /// implemented inline in this function (null / string-newtype /
    /// closure-fn-newtype) record here at the decision point. The future
    /// `reify` pass replays the same adaptation without re-checking
    /// expected-type compatibility.
    pub(super) fn try_coerce(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        // Numeric literal coercion (int, float, i128/u128) — sub-helper
        // records `NumericLiteral` and `expression_types` for the visited
        // AST id (and the inner `-NUM` literal id, when applicable).
        if let Some(coerced) = self.try_coerce_numeric_literal(expr, target_type) {
            return Some(coerced);
        }

        // Null literal → Option<T>
        if let Expr::Literal(lit) = expr
            && matches!(&lit.value, Literal::Null)
            && self
                .tysys
                .type_table
                .borrow()
                .as_option(target_type)
                .is_some()
        {
            self.record_coercion(
                expr.id(),
                super::sem::types::CoercionKind::NullToOption,
                target_type,
            );
            return Some(placeholder(target_type, lit.span));
        }

        // String/template literal → String newtype
        let is_string_or_template = matches!(
            expr,
            Expr::Literal(lit) if matches!(&lit.value, Literal::String(_))
        ) || matches!(expr, Expr::TemplateString(_));

        if is_string_or_template {
            let base_id = self
                .tysys
                .type_table
                .borrow()
                .get_ultimate_base_type(target_type);
            let string_struct_name = self
                .tysys
                .type_table
                .borrow()
                .compiler_items()
                .struct_name(crate::compiler_item::CompilerItem::String)
                .to_string();
            let is_string_newtype = matches!(
                self.tysys.type_table.borrow().get(base_id),
                ResolvedType::Struct { name, .. } if name == &string_struct_name
            ) && target_type != base_id;
            if is_string_newtype {
                // Walk the inner literal / template for fact recording.
                let _ = self.resolve_expr(expr, ctx, None);
                self.record_coercion(
                    expr.id(),
                    super::sem::types::CoercionKind::StringNewtype,
                    target_type,
                );
                // The inner resolve_expr wrote expression_types[expr.id]
                // with the unwrapped String type; overwrite with the
                // outer target newtype so reify reads the newtype here.
                self.record_expression_type(expr.id(), target_type);
                return Some(placeholder(target_type, expr.span()));
            }
        }

        // Closure literal → fn-type newtype. Walk the closure against the
        // unwrapped base fn type (so unannotated params are inferred from
        // the expected signature) and retag the recorded expression type.
        if matches!(expr, Expr::Closure(_)) {
            let base_id = self
                .tysys
                .type_table
                .borrow()
                .get_ultimate_base_type(target_type);
            let is_fn_newtype = matches!(
                self.tysys.type_table.borrow().get(base_id),
                ResolvedType::Function { .. }
            ) && target_type != base_id;
            if is_fn_newtype {
                // Walk the closure for fact recording (param types,
                // captures, body) under the unwrapped fn type.
                let _ = self.resolve_expr(expr, ctx, Some(base_id));
                self.record_coercion(
                    expr.id(),
                    super::sem::types::CoercionKind::ClosureToFnNewtype,
                    target_type,
                );
                // Same pattern as StringNewtype above: overwrite the
                // map's base-fn-type write with the outer newtype.
                self.record_expression_type(expr.id(), target_type);
                return Some(placeholder(target_type, expr.span()));
            }
        }

        // Tuple literal → type implementing SequenceLiteralBuilder (List<T> and user types).
        // The sub-helper records `TupleToSequence` and `expression_types`.
        if let Some(coerced) = self.try_coerce_tuple_to_sequence(expr, ctx, target_type) {
            return Some(coerced);
        }

        // Anonymous struct literal → type implementing KeyValueLiteralBuilder.
        // The sub-helper records `StructToMap` and `expression_types`.
        if let Some(coerced) = self.try_coerce_struct_to_map(expr, ctx, target_type) {
            return Some(coerced);
        }

        // If an anonymous struct literal targets a generic instance that doesn't
        // implement KeyValueLiteral, report a compile error.
        if let Expr::StructLiteral(struct_lit) = expr
            && struct_lit.name.is_none()
            && matches!(
                self.tysys.type_table.borrow().get(target_type),
                ResolvedType::GenericInstance { .. }
            )
        {
            let type_name = self.tysys.type_table.borrow().type_name(target_type);
            let _ = self.logger.error(TypeError::MissingTraitImpl {
                type_name,
                trait_name: "KeyValueLiteral".to_string(),
                span: expr.span(),
            });
        }

        None
    }

    /// Try to coerce an anonymous struct literal to a type implementing `KeyValueLiteralBuilder`.
    /// Desugars to a `LabeledBlock` that calls `Builder::new_literal(capacity)`, then
    /// `insert_literal(key, value)` for each field, then `build()`, so the monomorphize
    /// phase naturally discovers the required function instantiations.
    ///
    /// Records the coercion choice and resolved expression type at the
    /// decision point so every caller (`try_coerce`, `resolve_cast`,
    /// `resolve_let`'s struct-to-map branch) leaves an annotation —
    /// no caller can bypass recording.
    pub(super) fn try_coerce_struct_to_map(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        let coerced = self.try_coerce_struct_to_map_inner(expr, ctx, target_type)?;
        self.record_coercion(
            expr.id(),
            super::sem::types::CoercionKind::StructToMap,
            target_type,
        );
        self.record_expression_type(expr.id(), target_type);
        Some(coerced)
    }

    fn try_coerce_struct_to_map_inner(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        let Expr::StructLiteral(struct_lit) = expr else {
            return None;
        };
        if struct_lit.name.is_some() {
            return None;
        }

        // Get base struct name from target type
        let base_name = self.struct_name_for_type(target_type)?;

        // Check if target type implements KeyValueLiteralBuilder (or legacy KeyValueLiteral)
        let from_literal_info = self.find_key_value_literal_trait_impl(&base_name, target_type)?;
        let value_type = from_literal_info.value_type;
        let insert_self_kind = from_literal_info.self_kind;
        let trait_name = from_literal_info.trait_name.clone();
        let builder_type = from_literal_info.builder_type;
        let use_new_api = trait_name == "KeyValueLiteralBuilder";
        // Resolve the builder impl's home module — that is where
        // `Builder^Trait::new_literal` is registered, and (post-fix #1110)
        // where the monomorphizer expects to find the template. Fall back to
        // the receiver type's module for inherent / auto-derived impls; if
        // neither resolves, panic (no current-module fallback per #1110 (2)).
        let builder_name_for_lookup = self
            .struct_name_for_type(builder_type)
            .unwrap_or_else(|| base_name.clone());
        let builder_type_module = match self.tysys.type_table.borrow().get(builder_type) {
            ResolvedType::Struct { module_source, .. }
            | ResolvedType::GenericInstance { module_source, .. }
            | ResolvedType::Enum { module_source, .. }
            | ResolvedType::Variant { module_source, .. }
            | ResolvedType::Newtype { module_source, .. }
            | ResolvedType::Flags { module_source, .. }
            | ResolvedType::GenericResource { module_source, .. } => Some(module_source.clone()),
            _ => None,
        };
        // The builder's `Trait::new_literal` body lives in the impl-block's
        // module (`KeyValueLiteralBuilder` impls in `core:prelude/internal`
        // and the like), falling back to the builder type's own module
        // for inherent / auto-derived impls. Both producer-side; no
        // current-module fallback — if neither resolves, the builder
        // has no callable `new_literal` and that's a synthesis bug.
        let impl_module_source = self
            .tysys
            .trait_env
            .impl_module_for(
                &builder_name_for_lookup,
                &trait_name,
                builder_type_module.as_ref(),
            )
            .cloned()
            .or(builder_type_module)
            .unwrap_or_else(|| {
                panic!(
                    "KeyValueLiteralBuilder coercion: no home module for \
                     `{builder_name_for_lookup}^{trait_name}::new_literal` \
                     (builder type has no defining module and no impl in `TraitEnv`)"
                )
            });

        let span = expr.span();
        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        let i32_type = TypeTable::I32;

        // Get type args for monomorphization from builder type
        let builder_base_name = self
            .struct_name_for_type(builder_type)
            .unwrap_or_else(|| base_name.clone());
        let (type_arg_names, type_arg_ids): (Vec<String>, Vec<TypeId>) = {
            let tt = self.tysys.type_table.borrow();
            match tt.get(builder_type) {
                ResolvedType::GenericInstance { type_args, .. } => {
                    let names: Vec<String> = type_args
                        .iter()
                        .map(|&id| tt.mangle_type_name(id))
                        .collect();
                    (names, type_args.clone())
                }
                _ => (Vec::new(), Vec::new()),
            }
        };

        let mangled_builder_name = if type_arg_names.is_empty() {
            builder_base_name.clone()
        } else {
            crate::name::mangle_generic_name(&builder_base_name, &type_arg_names)
        };

        let new_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "new_literal");
        let insert_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "insert_literal");
        let build_mangled_name = if use_new_api {
            Some(MethodName::format_local(
                &mangled_builder_name,
                Some(&trait_name),
                "build",
            ))
        } else {
            None
        };

        // Stage 5 (WEP 2026-05-26): record the resolved
        // `KeyValueLiteralBuilder` impl data so reify can rebuild the
        // same `__kv_lit:` desugar block deterministically.
        let key = self.ann_key(expr.id());
        self.sem.types.key_value_coercions.insert(
            key,
            super::sem::types::KeyValueCoercionFacts {
                builder_type,
                value_type,
                insert_self_kind,
                trait_name,
                target_type,
                impl_module_source,
                builder_base_name,
                mangled_builder_name,
                type_arg_ids,
                type_arg_names,
                use_new_api,
                new_mangled_name,
                insert_mangled_name,
                build_mangled_name,
            },
        );

        // Reserve the `__b` builder local on the surrounding scope so
        // subsequent local-index accounting in the enclosing function
        // matches reify's expansion. The `string_type` / `i32_type`
        // intern calls above stay live so reify and downstream phases
        // see the same canonical `TypeId`s the elaborator picked.
        let _ = (string_type, i32_type);
        ctx.enter_scope();
        let _builder_index = ctx.add_local("__b".to_string(), builder_type, true, None);

        // Walk each field for fact recording + duplicate-field /
        // value-type diagnostics. Reify rebuilds the `__kv_lit:` desugar
        // block from the recorded `KeyValueCoercionFacts` + the AST.
        let mut seen_fields: IndexSet<&str> = IndexSet::default();
        for field in &struct_lit.fields {
            if !seen_fields.insert(field.name.as_str()) {
                let _ = self.logger.error(TypeError::DuplicateField {
                    name: field.name.clone(),
                    span: field.span,
                });
            }
        }

        for field in &struct_lit.fields {
            let value = self.resolve_expr(&field.value, ctx, Some(value_type));
            if value.type_id != value_type
                && value.type_id != TypeTable::UNKNOWN
                && value.type_id != TypeTable::NEVER
            {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: self.tysys.type_table.borrow().type_name(value_type),
                    found: self.tysys.type_table.borrow().type_name(value.type_id),
                    span: field.value.span(),
                });
            }
        }

        ctx.exit_scope();

        Some(placeholder(target_type, span))
    }

    /// Try to coerce a tuple/sequence literal `[e0, e1, ...]` to a user-defined type
    /// implementing `SequenceLiteralBuilder`.
    ///
    /// Coerce a tuple/sequence literal `[e0, e1, ...]` to a type implementing
    /// `SequenceLiteralBuilder` (including built-in `List<T>` and user types).
    ///
    /// `&mut <tuple-literal>` / `&<tuple-literal>` are looked through: the cast
    /// `&mut [...] as List<T>` parses as `(&mut [...]) as List<T>` (`&mut`
    /// binds tighter than `as`), but the user-facing semantics is to construct
    /// an `List<T>` and let the call site auto-borrow it. Without this
    /// passthrough the inner `[...]` would lower as a `tuple<>` `struct.new`
    /// and the call site's Wasm validation would fail because the tuple
    /// struct type is unrelated to the expected `List` struct type.
    pub(super) fn try_coerce_tuple_to_sequence(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        let coerced = self.try_coerce_tuple_to_sequence_inner(expr, ctx, target_type)?;
        self.record_coercion(
            expr.id(),
            super::sem::types::CoercionKind::TupleToSequence,
            target_type,
        );
        self.record_expression_type(expr.id(), target_type);
        Some(coerced)
    }

    fn try_coerce_tuple_to_sequence_inner(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        let tuple_lit = match expr {
            Expr::TupleLiteral(tuple_lit) => tuple_lit,
            Expr::Unary(unary) if matches!(unary.op, UnaryOp::Ref | UnaryOp::MutRef) => {
                if let Expr::TupleLiteral(tuple_lit) = &unary.expr {
                    tuple_lit
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        let base_name = self.struct_name_for_type(target_type)?;
        let (seq_info, needs_newtype_cast) = self
            .find_sequence_literal_trait_impl(&base_name, target_type)
            .map(|info| (info, false))
            .or_else(|| {
                // For newtypes, try the base type's SequenceLiteral impl
                let base_type = self
                    .tysys
                    .type_table
                    .borrow()
                    .get_newtype_base(target_type)?;
                let base_name = self.struct_name_for_type(base_type)?;
                self.find_sequence_literal_trait_impl(&base_name, base_type)
                    .map(|info| (info, true))
            })?;
        let element_type = seq_info.element_type;
        let push_self_kind = seq_info.self_kind;
        let trait_name = seq_info.trait_name.clone();
        let builder_type = seq_info.builder_type;
        let output_type = seq_info.output_type;
        let impl_module_source = seq_info.impl_module_source;
        let builder_base_name = self
            .struct_name_for_type(builder_type)
            .unwrap_or_else(|| base_name.clone());

        let span = expr.span();

        let (type_arg_names, type_arg_ids): (Vec<String>, Vec<TypeId>) = {
            let tt = self.tysys.type_table.borrow();
            match tt.get(builder_type) {
                ResolvedType::GenericInstance { type_args, .. } => {
                    let names: Vec<String> = type_args
                        .iter()
                        .map(|&id| tt.mangle_type_name(id))
                        .collect();
                    (names, type_args.clone())
                }
                _ => (Vec::new(), Vec::new()),
            }
        };

        let mangled_builder_name = if type_arg_names.is_empty() {
            builder_base_name.clone()
        } else {
            crate::name::mangle_generic_name(&builder_base_name, &type_arg_names)
        };

        let new_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "new_literal");
        let push_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "push_literal");
        let build_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "build");

        // Stage 5 (WEP 2026-05-26): record the resolved
        // `SequenceLiteralBuilder` impl data so reify can rebuild the
        // same `__seq_lit:` desugar block deterministically — the
        // trait-impl lookup chain (newtype peel + sequence-trait
        // search), the type-arg mangling, and the per-method mangled
        // names are not reproducible from the AST alone.
        let key = self.ann_key(expr.id());
        self.sem.types.sequence_coercions.insert(
            key,
            super::sem::types::SequenceCoercionFacts {
                builder_type,
                element_type,
                push_self_kind,
                trait_name,
                output_type,
                impl_module_source,
                builder_base_name,
                mangled_builder_name,
                type_arg_ids,
                type_arg_names,
                newtype_cast_to: if needs_newtype_cast {
                    Some(target_type)
                } else {
                    None
                },
                new_mangled_name,
                push_mangled_name,
                build_mangled_name,
            },
        );

        ctx.enter_scope();

        // Reserve the `__b` builder slot so subsequent local-index
        // accounting in the enclosing function stays consistent with
        // reify's expansion.
        let _builder_index = ctx.add_local("__b".to_string(), builder_type, true, None);

        // Walk each element for fact recording + heterogeneous-element
        // diagnostics. Reify rebuilds the `__seq_lit:` desugar block from
        // the recorded `SequenceCoercionFacts` + the AST.
        for element in &tuple_lit.elements {
            let elem_expr = self.resolve_expr(element, ctx, Some(element_type));
            if elem_expr.type_id != element_type
                && elem_expr.type_id != TypeTable::UNKNOWN
                && element_type != TypeTable::UNKNOWN
                && !self
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_param(element_type)
            {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: format!(
                        "homogeneous elements of type '{}'",
                        self.tysys.type_table.borrow().type_name(element_type)
                    ),
                    found: format!(
                        "heterogeneous element of type '{}'",
                        self.tysys.type_table.borrow().type_name(elem_expr.type_id)
                    ),
                    span: element.span(),
                });
            }
        }

        ctx.exit_scope();

        // Placeholder typed as the coercion's surface result — `target_type`
        // when newtype-cast, otherwise the builder's `output_type`. The
        // outer wrapper records `expression_types[expr.id]` from this
        // value's `type_id`, and reify reads it from the recorded
        // `SequenceCoercionFacts.newtype_cast_to` / `output_type`.
        let result_type = if needs_newtype_cast {
            target_type
        } else {
            output_type
        };
        Some(placeholder(result_type, span))
    }
}

/// Build the `from_pair` call that materializes a 128-bit value from its
/// `(low: u64, high: u64/i64)` halves. Pure and `self`-free so the
/// elaborator's coercion / cast paths and the reify pass produce
/// byte-identical TIR.
pub(super) fn build_int128_from_pair(
    type_name: &str,
    low: u64,
    high: i64,
    target_type: TypeId,
    span: Span,
) -> TirExpr {
    let low_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: low,
            repr: low.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let high_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: high.cast_unsigned(),
            repr: high.to_string(),
        },
        if type_name == "u128" {
            TypeTable::U64
        } else {
            TypeTable::I64
        },
        span,
    );

    let method_info = LocalMethodName::new(type_name.to_string(), None, "from_pair".to_string());
    let mangled_func_name = method_info.to_mangled_name();

    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: mangled_func_name,
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![
                CallArg::new(low_literal, false),
                CallArg::new(high_literal, false),
            ],
        },
        target_type,
        span,
    )
}

/// Construct the TIR call that materializes an `i128` / `u128` value from
/// a parsed numeric-literal `value`. Values that fit 64 bits take the
/// cheaper `from_u64` / `from_i64` path when `allow_small` is set; the
/// negated `-NUM` shape passes `allow_small = false` so it always uses
/// `from_pair`, matching the elaborator's historical output. Pure and
/// `self`-free so both the elaborator and reify build identical TIR.
pub(super) fn build_int128_literal_call(
    name: &str,
    value: i128,
    repr: &str,
    allow_small: bool,
    target_type: TypeId,
    span: Span,
) -> TirExpr {
    let use_small = allow_small
        && if name == "u128" {
            u64::try_from(value).is_ok()
        } else {
            i64::try_from(value).is_ok()
        };

    if use_small {
        let (inner_type, method_name, store_value) = if name == "u128" {
            (
                TypeTable::U64,
                "from_u64",
                u64::try_from(value).expect("value fits in u64"),
            )
        } else {
            (
                TypeTable::I64,
                "from_i64",
                i64::try_from(value)
                    .expect("value fits in i64")
                    .cast_unsigned(),
            )
        };

        let inner_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: store_value,
                repr: repr.to_string(),
            },
            inner_type,
            span,
        );

        let method_info = LocalMethodName::new(name.to_string(), None, method_name.to_string());
        let mangled_func_name = method_info.to_mangled_name();

        return TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::int128(),
                    name: mangled_func_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                type_args: vec![],
                args: vec![CallArg::new(inner_literal, false)],
            },
            target_type,
            span,
        );
    }

    let (low, high) = util::unpack_i128(value);
    build_int128_from_pair(name, low, high, target_type, span)
}

/// Build `u128::from_u64(inner)` / `i128::from_i64(inner)` for the
/// general (non-literal) `expr as i128/u128` cast path. `intermediate`
/// is the source expression already cast to the `u64` / `i64` width.
/// Pure and `self`-free so the elaborator and reify stay in lockstep.
pub(super) fn build_int128_from_intermediate(
    name: &str,
    intermediate: TirExpr,
    target_type: TypeId,
    span: Span,
) -> TirExpr {
    let method_name = if name == "u128" {
        "from_u64"
    } else {
        "from_i64"
    };
    let method_info = LocalMethodName::new(name.to_string(), None, method_name.to_string());
    let mangled_func_name = method_info.to_mangled_name();
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: mangled_func_name,
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![CallArg::new(intermediate, false)],
        },
        target_type,
        span,
    )
}
