//! Numeric literal coercion and type coercion.

use super::Elaborator;
use super::types::{FunctionContext, TypeError};
use super::util;
use crate::ast::{self, Expr, Literal, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexSet;
use crate::tir::{ResolvedType, TypeId};

/// Whether `expr` is a literal — the only position implicit conversion reaches
/// (WEP 2026-08-24). A template string, a variable, and a call are not.
fn is_literal_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) | Expr::TupleLiteral(_) => true,
        Expr::StructLiteral(struct_lit) => struct_lit.name.is_none(),
        Expr::Unary(unary) => unary.op == UnaryOp::Neg && matches!(&unary.expr, Expr::Literal(_)),
        _ => false,
    }
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
    ) -> Option<TypeId> {
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
    ) -> Option<TypeId> {
        // Number literal coercion to integer
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && self.tysys.type_table.borrow().is_integer(target_type)
        {
            if util::is_float_only_literal(repr) {
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '{repr}' as integer (has decimal point or negative exponent)"
                    ),
                    span: lit.span,
                });
                return Some(target_type);
            }
            return Some(match util::parse_u128_literal(repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_positive(
                        value,
                        target_type,
                        &self.tysys.type_table.borrow(),
                        repr,
                    ) {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: lit.span,
                        });
                    }
                    target_type
                }
                Err(message) => {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    target_type
                }
            });
        }

        if let Expr::Literal(lit) = expr
            && let Literal::Byte(raw) = &lit.value
            && self.tysys.type_table.borrow().is_integer(target_type)
        {
            return Some(match util::unescape_byte(raw) {
                Ok(byte) => {
                    if let Some(err_msg) = util::check_int_range_positive(
                        u128::from(byte),
                        target_type,
                        &self.tysys.type_table.borrow(),
                        &byte.to_string(),
                    ) {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: lit.span,
                        });
                    }
                    target_type
                }
                Err(message) => {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    target_type
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
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '-{repr}' as integer (has decimal point or negative exponent)"
                    ),
                    span: unary.span,
                });
                return Some(target_type);
            }
            return Some(match util::parse_u128_literal(repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_negative(
                        value,
                        target_type,
                        &self.tysys.type_table.borrow(),
                        repr,
                    ) {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: unary.span,
                        });
                    }
                    target_type
                }
                Err(message) => {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    target_type
                }
            });
        }

        // Number literal coercion to float
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && self.tysys.type_table.borrow().is_float(target_type)
        {
            return Some(match util::parse_float_literal(repr) {
                Ok(_) => target_type,
                Err(message) => {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    target_type
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
                Ok(_) => target_type,
                Err(message) => {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    target_type
                }
            });
        }

        // i128/u128 literal coercion
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && !util::is_float_only_literal(repr)
        {
            let struct_name = match self.tysys.type_table.borrow().get(target_type) {
                ResolvedType::Struct { def, .. } => {
                    Some(self.tysys.type_table.borrow().struct_head_name(*def))
                }
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
                        return Some(target_type);
                    }
                    Err(_) => {
                        let _ = self.emit(TypeError::InvalidLiteral {
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
            let struct_name = match self.tysys.type_table.borrow().get(target_type) {
                ResolvedType::Struct { def, .. } => {
                    Some(self.tysys.type_table.borrow().struct_head_name(*def))
                }
                _ => None,
            };

            if let Some(name) = struct_name
                && name == "i128"
            {
                let negated_repr = format!("-{repr}");
                if util::parse_i128_literal(&negated_repr).is_ok() {
                    return Some(target_type);
                }
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: format!("invalid i128 literal: -{repr}"),
                    span: unary.span,
                });
            }
        }

        None
    }

    /// Re-coerce numeric literal arguments to inferred parameter types: a literal
    /// resolved before inference took the default `i32` / `f64`, its expected
    /// type still being an unsubstituted `TypeParam`, so once `T` is concrete
    /// every literal at a now-numeric parameter is coerced again.
    /// `expected_param_types` must already carry the inferred type arguments.
    pub(super) fn recoerce_literal_args(
        &mut self,
        raw_args: &[Expr],
        args: &mut [TypeId],
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
            if *arg == expected {
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

    /// Try to coerce an expression to the expected type — numeric literals,
    /// null, string newtypes, tuple-to-array — or `None` when none applies. Each
    /// success records its [`super::sem::types::CoercionKind`] keyed by
    /// `expr.id()`, from inside the shared `try_coerce_*` helpers where one
    /// exists, so reify can replay the adaptation without re-checking.
    pub(super) fn try_coerce(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TypeId> {
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
            return Some(target_type);
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
                .representation_head(target_type);
            let string_struct_name = self
                .tysys
                .type_table
                .borrow()
                .compiler_struct_name(crate::compiler_item::CompilerItem::String)
                .to_string();
            let is_string_newtype = matches!(
                self.tysys.type_table.borrow().get(base_id),
                ResolvedType::Struct { def, .. }
                    if self.tysys.type_table.borrow().struct_head_name(*def) == string_struct_name
            ) && target_type != base_id;
            if is_string_newtype {
                // Walk the inner literal / template for fact recording.
                self.resolve_expr(expr, ctx, None);
                self.record_coercion(
                    expr.id(),
                    super::sem::types::CoercionKind::StringNewtype,
                    target_type,
                );
                // The inner resolve_expr wrote expression_types[expr.id]
                // with the unwrapped String type; overwrite with the
                // outer target newtype so reify reads the newtype here.
                self.record_expression_type(expr.id(), target_type);
                return Some(target_type);
            }
        }

        let is_bytes_literal = matches!(
            expr,
            Expr::Literal(lit)
                if matches!(&lit.value, Literal::Bytes(_) | Literal::IncludeBytes(_))
        );
        if is_bytes_literal {
            let list_u8 = self
                .tysys
                .type_table
                .borrow_mut()
                .make_list(crate::tir::TypeTable::U8);
            let base_id = self
                .tysys
                .type_table
                .borrow()
                .representation_head(target_type);
            if base_id == list_u8 {
                if let Expr::Literal(lit) = expr
                    && let Literal::Bytes(raw) = &lit.value
                    && let Err(message) = util::unescape_bytes(raw)
                {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                }
                self.record_coercion(
                    expr.id(),
                    super::sem::types::CoercionKind::BytesNewtype,
                    target_type,
                );
                self.record_expression_type(expr.id(), target_type);
                return Some(target_type);
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
                .representation_head(target_type);
            let is_fn_newtype = matches!(
                self.tysys.type_table.borrow().get(base_id),
                ResolvedType::Function { .. }
            ) && target_type != base_id;
            if is_fn_newtype {
                // Walk the closure for fact recording (param types,
                // captures, body) under the unwrapped fn type.
                self.resolve_expr(expr, ctx, Some(base_id));
                self.record_coercion(
                    expr.id(),
                    super::sem::types::CoercionKind::ClosureToFnNewtype,
                    target_type,
                );
                // Same pattern as StringNewtype above: overwrite the
                // map's base-fn-type write with the outer newtype.
                self.record_expression_type(expr.id(), target_type);
                return Some(target_type);
            }
        }

        // Sequence literal → a type with a `From<Array<E>>` impl (`List<T>`
        // and user types). The sub-helper records `TupleToSequence` and
        // `expression_types`.
        if let Some(coerced) = self.try_coerce_tuple_to_sequence(expr, ctx, target_type) {
            return Some(coerced);
        }

        // Key-value literal → a type with a `From<Array<[K, V]>>` impl. The
        // sub-helper records `StructToMap` and `expression_types`.
        if let Some(coerced) = self.try_coerce_struct_to_map(expr, ctx, target_type) {
            return Some(coerced);
        }

        // A key-value literal whose generic target builds from no pair array
        // at all: say what is missing where it is written.
        if let Expr::StructLiteral(struct_lit) = expr
            && struct_lit.name.is_none()
            && matches!(
                self.tysys.type_table.borrow().get(target_type),
                ResolvedType::GenericInstance { .. }
            )
        {
            self.report_if_not_a_map_target(target_type, expr.span());
        }

        None
    }

    /// Report an object literal written against a type that builds from no pair
    /// array at all. Asked of the target, so a coercion that declined for
    /// another reason — an ambiguity it has already reported — says nothing
    /// here that would contradict it.
    pub(super) fn report_if_not_a_map_target(
        &mut self,
        target_type: TypeId,
        span: crate::token::Span,
    ) {
        if self.is_key_value_literal_target(target_type) {
            return;
        }
        let type_name = self.tysys.type_table.borrow().type_name(target_type);
        let _ = self.emit(TypeError::InvalidLiteral {
            message: format!(
                "`{type_name}` implements no `From<Array<[K, V]>>`, so it cannot be built from an \
                 object literal"
            ),
            span,
        });
    }

    /// Coerce an anonymous struct literal into a type implementing
    /// `From<Array<[K, V]>>` (WEP 2026-08-24).
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
    ) -> Option<TypeId> {
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
    ) -> Option<TypeId> {
        let Expr::StructLiteral(struct_lit) = expr else {
            return None;
        };
        if struct_lit.name.is_some() {
            return None;
        }

        let span = expr.span();
        let (from_info, output_type, needs_newtype_cast) =
            self.find_literal_from_array(target_type, true, span)?;
        let (key_type, value_type) = {
            let tt = self.tysys.type_table.borrow();
            let pair = tt.as_tuple(from_info.element_type).expect(
                "find_from_array_impls with want_pair answers only with a two-element tuple",
            );
            (pair[0], pair[1])
        };

        // Every key a literal can write is a field name, so the impl's key type
        // must accept a `String`. Refusing here is what keeps a `From<Array<[K,
        // V]>>` with another `K` from reaching WIR build as a type mismatch;
        // computed keys are what would give such a `K` a literal to be written
        // from.
        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        let key_incompatible = matches!(
            super::typecheck::check_assignable(
                string_type,
                key_type,
                &self.tysys.type_table.borrow(),
            ),
            super::typecheck::TypeCheckResult::Incompatible
        );
        if key_incompatible {
            let (type_name, key_name) = {
                let tt = self.tysys.type_table.borrow();
                (tt.type_name(target_type), tt.type_name(key_type))
            };
            let _ = self.emit(TypeError::InvalidLiteral {
                message: format!(
                    "`{type_name}` builds from keys of type `{key_name}`; an object literal \
                     writes `String` keys"
                ),
                span,
            });
            // Recover as the target type: the literal named a real impl, so a
            // second "not constructible" report would describe the same fault.
            return Some(target_type);
        }
        self.solve_infer_holes_against(key_type, string_type);

        let spread = if struct_lit.spreads.is_empty() {
            None
        } else {
            let found = self.literal_spread_call(output_type);
            if found.is_none() {
                let type_name = self.tysys.type_table.borrow().type_name(target_type);
                let _ = self.emit(TypeError::MissingTraitImpl {
                    type_name,
                    trait_name: "LiteralSpread".to_string(),
                    span,
                });
            }
            found
        };

        let call = self.literal_from_call(&from_info, output_type);
        self.sem.types.key_value_coercions.insert(
            expr.id(),
            super::sem::types::KeyValueCoercionFacts {
                value_type,
                pair_type: from_info.element_type,
                newtype_cast_to: needs_newtype_cast.then_some(target_type),
                call,
                spread,
            },
        );

        let mut seen_fields: IndexSet<&str> = IndexSet::default();
        for field in &struct_lit.fields {
            if !seen_fields.insert(field.name.as_str()) {
                let _ = self.emit(TypeError::DuplicateField {
                    name: field.name.clone(),
                    span: field.span,
                });
            }
        }

        // Members in source order, so each subexpression is walked exactly
        // where it is written — and, for a spread, in the same order reify's
        // fold walks them, so the `__acc` reserved below lands on the index
        // reify will allocate for it.
        let has_spread = !struct_lit.spreads.is_empty();
        if has_spread {
            ctx.enter_scope();
        }
        let mut value_type = value_type;
        for member in struct_lit.members() {
            match member {
                crate::ast::LiteralMember::Spread(_, spread) => {
                    self.resolve_expr(&spread.expr, ctx, Some(output_type));
                }
                crate::ast::LiteralMember::Field(_, field) => {
                    let value = self.resolve_expr(&field.value, ctx, Some(value_type));
                    // Route through the shared check rather than comparing ids:
                    // an undecided value type — a callee's slot the call site
                    // instantiated — defers to its solver instead of rejecting
                    // every value.
                    let incompatible = matches!(
                        super::typecheck::check_assignable(
                            value,
                            value_type,
                            &self.tysys.type_table.borrow(),
                        ),
                        super::typecheck::TypeCheckResult::Incompatible
                    );
                    if incompatible {
                        self.convert_literal_element(&field.value, value, value_type);
                    } else {
                        // The values are what decide an open value type.
                        self.solve_infer_holes_against(value_type, value);
                        value_type = self.apply_infer_holes(value_type);
                    }
                }
            }
        }
        if has_spread {
            ctx.add_local("__acc".to_string(), output_type, true, None);
            ctx.exit_scope();
        }

        Some(target_type)
    }

    /// Coerce a sequence literal `[e0, e1, …]` into a type implementing
    /// `From<Array<E>>`, built-in `List<T>` included. A leading `&` /
    /// `&mut` is looked through: `&mut [...] as List<T>` parses as
    /// `(&mut [...]) as List<T>`, but means a `List<T>` the call site
    /// auto-borrows — lowering the inner literal as a tuple fails validation.
    pub(super) fn try_coerce_tuple_to_sequence(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TypeId> {
        let coerced = self.try_coerce_tuple_to_sequence_inner(expr, ctx, target_type)?;
        self.record_coercion(
            expr.id(),
            super::sem::types::CoercionKind::TupleToSequence,
            target_type,
        );
        self.record_expression_type(expr.id(), target_type);
        Some(coerced)
    }

    /// The `From<Array<E>>` a literal targeting `target_type` coerces through,
    /// with what `from` returns and whether the target is a newtype the result
    /// is cast back to. `want_pair` selects the literal's form; see
    /// [`Self::find_from_array_impls`].
    ///
    /// Several admitted impls are an ambiguity the site reports here: only the
    /// literal knows which form it was written in, and `T::from(…)` spelled out
    /// resolves it by ordinary overload resolution.
    fn find_literal_from_array(
        &mut self,
        target_type: TypeId,
        want_pair: bool,
        span: crate::token::Span,
    ) -> Option<(super::types::FromArrayInfo, TypeId, bool)> {
        let resolve = |elaborator: &mut Self, ty: TypeId| {
            let name = elaborator.literal_target_name(ty)?;
            match elaborator
                .find_from_array_impls(&name, ty, want_pair)
                .as_slice()
            {
                [only] => Some(Ok(only.clone())),
                [] => None,
                several => Some(Err(several.len())),
            }
        };
        // A newtype over a literal-constructible type is built through the base
        // and cast back.
        let (found, output_type, needs_newtype_cast) =
            if let Some(found) = resolve(self, target_type) {
                (found, target_type, false)
            } else {
                let base_type = self
                    .tysys
                    .type_table
                    .borrow()
                    .get_newtype_base(target_type)?;
                (resolve(self, base_type)?, base_type, true)
            };
        match found {
            Ok(info) => Some((info, output_type, needs_newtype_cast)),
            Err(count) => {
                let type_name = self.tysys.type_table.borrow().type_name(target_type);
                let _ = self.emit(TypeError::AmbiguousLiteralConversion {
                    type_name,
                    count,
                    span,
                });
                None
            }
        }
    }

    /// The name a literal's target type carries its impls under. Broader than
    /// [`super::tysys::TypeSystem::struct_name_for_type`], which omits the
    /// nominal shapes that are not structs: a variant is a literal target too
    /// (`core:value::Value` is the case that matters).
    fn literal_target_name(&self, target_type: TypeId) -> Option<String> {
        self.tysys.struct_name_for_type(target_type).or_else(|| {
            self.tysys
                .type_table
                .borrow()
                .nominal_head(target_type)
                .map(|(name, _)| name)
        })
    }

    /// Record the `From` a literal element converts through to reach its
    /// slot's type, reporting why where none applies (WEP 2026-08-24).
    ///
    /// Implicit conversion is confined to a literal position: only an element
    /// the source wrote as a literal is offered one, so `[1, "x"] as
    /// List<Value>` compiles while `[a, b]` still asks for `Value::from(a)`.
    fn convert_literal_element(&mut self, element: &Expr, found_type: TypeId, slot_type: TypeId) {
        if self.record_literal_conversion(element, found_type, slot_type) {
            return;
        }
        let (found, slot) = {
            let tt = self.tysys.type_table.borrow();
            (tt.type_name(found_type), tt.type_name(slot_type))
        };
        // `null`'s own type is what a target converts from to accept it, and
        // `Option<!>` reads badly in the message that says none was found.
        if self.tysys.is_null_literal(element) {
            let _ = self.emit(TypeError::InvalidLiteral {
                message: format!(
                    "`null` names no value of `{slot}`; an `Option` accepts it, and any other \
                     type by implementing `From<Option<!>>`"
                ),
                span: element.span(),
            });
            return;
        }
        let reason = if is_literal_expr(element) {
            format!("`{slot}` has no `From<{found}>`")
        } else {
            "only a literal converts implicitly".to_string()
        };
        let _ = self.emit(TypeError::InvalidLiteral {
            message: format!(
                "cannot use `{found}` where `{slot}` is expected: {reason}; \
                 write `{slot}::from(…)`"
            ),
            span: element.span(),
        });
    }

    /// [`Self::convert_literal_element`]'s lookup half: `true` once a `From`
    /// is recorded for `element`.
    fn record_literal_conversion(
        &mut self,
        element: &Expr,
        found_type: TypeId,
        slot_type: TypeId,
    ) -> bool {
        if !is_literal_expr(element) {
            return false;
        }
        let Some(name) = self.literal_target_name(slot_type) else {
            return false;
        };
        let Some(from_def) = self
            .tysys
            .compiler_trait_def(crate::compiler_item::CompilerItem::From)
        else {
            return false;
        };
        let found = self
            .find_arithmetic_trait_impls(&name, slot_type, from_def, "from", None)
            .into_iter()
            .find(|info| info.rhs_type == Some(found_type));
        let Some(info) = found else {
            return false;
        };
        let call = self.literal_from_call(
            &super::types::FromArrayInfo {
                impl_def: Some(info.impl_def),
                element_type: found_type,
                array_type: found_type,
                impl_module_source: info.impl_module_source.clone(),
                trait_name: info.trait_name,
            },
            slot_type,
        );
        self.sem
            .types
            .literal_conversions
            .insert(element.id(), call);
        true
    }

    /// Whether `type_id` is a map — a type a `{ k: v, … }` literal builds
    /// through `From<Array<[K, V]>>` — rather than a composable struct.
    pub(super) fn is_key_value_literal_target(&mut self, type_id: TypeId) -> bool {
        self.literal_target_name(type_id)
            .is_some_and(|name| !self.find_from_array_impls(&name, type_id, true).is_empty())
    }

    /// The `LiteralSpread::spread_literal` a `..base` member calls on
    /// `output_type`, or `None` where the type does not implement the trait.
    fn literal_spread_call(
        &mut self,
        output_type: TypeId,
    ) -> Option<super::sem::types::LiteralCallee> {
        let name = self.literal_target_name(output_type)?;
        let trait_ = self
            .tysys
            .compiler_trait_def(crate::compiler_item::CompilerItem::LiteralSpread)?;
        let info =
            self.find_arithmetic_trait_impl(&name, output_type, trait_, "spread_literal", None)?;
        Some(self.literal_callee(
            Some(info.impl_def),
            info.impl_module_source,
            info.trait_name,
            output_type,
            "spread_literal",
        ))
    }

    /// Assemble the `from` call's facts for a resolved `From<Array<E>>`,
    /// already remangled.
    fn literal_from_call(
        &mut self,
        from_info: &super::types::FromArrayInfo,
        output_type: TypeId,
    ) -> super::sem::types::LiteralFromCall {
        super::sem::types::LiteralFromCall {
            from_type: from_info.array_type,
            output_type,
            callee: self.literal_callee(
                from_info.impl_def,
                from_info.impl_module_source.clone(),
                from_info.trait_name.clone(),
                output_type,
                "from",
            ),
        }
    }

    /// Name the trait method a literal calls on `output_type`, remangled.
    fn literal_callee(
        &mut self,
        impl_def: Option<crate::defs::DefId>,
        impl_module_source: crate::module_source::ModuleSource,
        trait_name: crate::name::FqTraitName,
        output_type: TypeId,
        method: &'static str,
    ) -> super::sem::types::LiteralCallee {
        let mut callee = super::sem::types::LiteralCallee {
            method_def: impl_def.and_then(|def| self.tysys.declared_method(def, method)),
            impl_module_source,
            trait_name,
            target_base_name: self.tysys.fq_receiver_head(output_type),
            type_arg_ids: self
                .tysys
                .type_table
                .borrow()
                .nominal_type_args(output_type)
                .unwrap_or_default(),
            type_arg_names: Vec::new(),
            method,
            mangled_name: String::new(),
        };
        callee.remangle(&self.tysys.type_table.borrow());
        callee
    }

    fn try_coerce_tuple_to_sequence_inner(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TypeId> {
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

        let span = expr.span();
        let (from_info, output_type, needs_newtype_cast) =
            self.find_literal_from_array(target_type, false, span)?;
        let element_type = from_info.element_type;

        // `[..a, b]` splices one tuple into another and `[..T::method()]`
        // expands a type pack (WEP 2026-03-14); both stay with the tuple the
        // literal already is, and `Array<T>` could not gain either — a fixed
        // array does not grow. Reported after the target is known to be
        // literal-constructible, so a tuple target still takes the tuple path,
        // and before the element walk, which would hand the spread to
        // `resolve_expr`.
        if let Some(spread) = tuple_lit
            .elements
            .iter()
            .find(|element| matches!(element, Expr::Spread(..)))
        {
            let type_name = self.tysys.type_table.borrow().type_name(target_type);
            let _ = self.emit(TypeError::InvalidLiteral {
                message: format!(
                    "`..base` is a tuple spread; a sequence literal building `{type_name}` \
                     cannot carry one"
                ),
                span: spread.span(),
            });
            return Some(target_type);
        }

        // WEP 2026-08-24: record the resolved `From<Array<E>>` so reify
        // rebuilds the same array literal and `from` call. The names come from
        // `remangle`, the one place that derives them from the types — the
        // sweep calls the same thing once a solved variable changes one.
        //
        // Keyed on the literal's own id, not `expr`'s: a peeled `&mut [...]`
        // reaches reify as the inner `TupleLiteral`, which is where the lookup
        // happens.
        let call = self.literal_from_call(&from_info, output_type);
        self.sem.types.sequence_coercions.insert(
            tuple_lit.id,
            super::sem::types::SequenceCoercionFacts {
                element_type,
                newtype_cast_to: needs_newtype_cast.then_some(target_type),
                call,
            },
        );

        // The first element decides an open element type; every element after
        // it is checked against that answer. Reading `element_type` afresh each
        // round is what makes the decision stick — left as the variable, it
        // would defer for every later element and wave through `[1, "abc"]`.
        let mut element_type = element_type;
        for element in &tuple_lit.elements {
            let elem_expr = self.resolve_expr(element, ctx, Some(element_type));
            // Route through the shared check rather than comparing ids: a
            // rigid `T` element type rejects a concrete element (it is the
            // enclosing body's own parameter), while a variable or a pack
            // defers to its solver.
            let incompatible = matches!(
                super::typecheck::check_assignable(
                    elem_expr,
                    element_type,
                    &self.tysys.type_table.borrow(),
                ),
                super::typecheck::TypeCheckResult::Incompatible
            );
            if incompatible {
                self.convert_literal_element(element, elem_expr, element_type);
            } else {
                // Where the target left the element type open — a callee's
                // slot the call site instantiated — the elements decide it.
                // Nothing else can: the literal is the only evidence of what
                // this sequence holds.
                self.solve_infer_holes_against(element_type, elem_expr);
                element_type = self.apply_infer_holes(element_type);
            }
        }

        // The coercion's surface result — `target_type` when newtype-cast,
        // otherwise what `from` returns. Reify reads the shape it emits from
        // the recorded `SequenceCoercionFacts.newtype_cast_to` / `output_type`.
        let result_type = if needs_newtype_cast {
            target_type
        } else {
            output_type
        };
        Some(result_type)
    }
}
