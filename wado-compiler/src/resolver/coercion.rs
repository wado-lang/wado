//! Numeric literal coercion and type coercion.

use super::Resolver;
use super::types::{FunctionContext, TypeError};
use super::util;
use crate::ast::{self, Expr, Literal, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexSet;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt,
    TirStmtKind, TypeId, TypeTable,
};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn try_coerce_numeric_literal(
        &mut self,
        expr: &Expr,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        // Number literal coercion to integer
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && self.type_table.borrow().is_integer(target_type)
        {
            if util::is_float_only_literal(repr) {
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '{repr}' as integer (has decimal point or negative exponent)"
                    ),
                    span: lit.span,
                });
                return Some(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: 0,
                        repr: repr.clone(),
                    },
                    target_type,
                    lit.span,
                ));
            }
            return Some(match util::parse_u128_literal(repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_positive(
                        value,
                        target_type,
                        &self.type_table.borrow(),
                        repr,
                    ) {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: lit.span,
                        });
                    }
                    TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: value as u64,
                            repr: repr.clone(),
                        },
                        target_type,
                        lit.span,
                    )
                }
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: repr.clone(),
                        },
                        target_type,
                        lit.span,
                    )
                }
            });
        }

        // Negated number literal coercion to integer: -42 as i64
        if let Expr::Unary(unary) = expr
            && unary.op == UnaryOp::Neg
            && let Expr::Literal(lit) = &unary.expr
            && let Literal::Number(repr) = &lit.value
            && self.type_table.borrow().is_integer(target_type)
        {
            if util::is_float_only_literal(repr) {
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '-{repr}' as integer (has decimal point or negative exponent)"
                    ),
                    span: unary.span,
                });
                return Some(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: 0,
                        repr: format!("-{repr}"),
                    },
                    target_type,
                    unary.span,
                ));
            }
            return Some(match util::parse_u128_literal(repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_negative(
                        value,
                        target_type,
                        &self.type_table.borrow(),
                        repr,
                    ) {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: unary.span,
                        });
                    }
                    let neg_value = (value as u64 as i64).wrapping_neg().cast_unsigned();
                    TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: neg_value,
                            repr: format!("-{repr}"),
                        },
                        target_type,
                        unary.span,
                    )
                }
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: format!("-{repr}"),
                        },
                        target_type,
                        unary.span,
                    )
                }
            });
        }

        // Number literal coercion to float
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && self.type_table.borrow().is_float(target_type)
        {
            return Some(match util::parse_float_literal(repr) {
                Ok(value) => TirExpr::new(
                    TirExprKind::FloatLiteral {
                        value,
                        repr: repr.clone(),
                    },
                    target_type,
                    lit.span,
                ),
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    TirExpr::new(
                        TirExprKind::FloatLiteral {
                            value: 0.0,
                            repr: repr.clone(),
                        },
                        target_type,
                        lit.span,
                    )
                }
            });
        }

        // Negated number literal coercion to float: -3.14 as f32
        if let Expr::Unary(unary) = expr
            && unary.op == UnaryOp::Neg
            && let Expr::Literal(lit) = &unary.expr
            && let Literal::Number(repr) = &lit.value
            && self.type_table.borrow().is_float(target_type)
        {
            return Some(match util::parse_float_literal(repr) {
                Ok(value) => TirExpr::new(
                    TirExprKind::FloatLiteral {
                        value: -value,
                        repr: format!("-{repr}"),
                    },
                    target_type,
                    unary.span,
                ),
                Err(message) => {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                    TirExpr::new(
                        TirExprKind::FloatLiteral {
                            value: 0.0,
                            repr: format!("-{repr}"),
                        },
                        target_type,
                        unary.span,
                    )
                }
            });
        }

        // i128/u128 literal coercion
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && !util::is_float_only_literal(repr)
        {
            let struct_name = match self.type_table.borrow().get(target_type).clone() {
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
                    Ok(value) => {
                        // If value fits in u64/i64, use the cheaper from_u64/from_i64
                        let use_small = if name == "u128" {
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
                                    repr: repr.clone(),
                                },
                                inner_type,
                                lit.span,
                            );

                            let method_info =
                                LocalMethodName::new(name, None, method_name.to_string());
                            let mangled_func_name = method_info.to_mangled_name();

                            return Some(TirExpr::new(
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
                                lit.span,
                            ));
                        }

                        // Value doesn't fit in u64/i64, use from_pair
                        let (low, high) = util::unpack_i128(value);
                        return Some(self.build_from_pair_call(
                            &name,
                            low,
                            high,
                            target_type,
                            lit.span,
                        ));
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
            let struct_name = match self.type_table.borrow().get(target_type).clone() {
                ResolvedType::Struct { name, .. } => Some(name),
                _ => None,
            };

            if let Some(name) = struct_name
                && name == "i128"
            {
                let negated_repr = format!("-{repr}");
                if let Ok(value) = util::parse_i128_literal(&negated_repr) {
                    let (low, high) = util::unpack_i128(value);
                    return Some(self.build_from_pair_call(
                        &name,
                        low,
                        high,
                        target_type,
                        unary.span,
                    ));
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
                let tt = self.type_table.borrow();
                tt.is_integer(expected) || tt.is_float(expected)
            };
            if !is_numeric {
                continue;
            }
            if let Some(coerced) = self.try_coerce_numeric_literal(raw, expected) {
                *arg = coerced;
            }
        }
    }

    /// Try to coerce an expression to match the expected type.
    /// Handles numeric literals, null, string newtypes, and tuple-to-array coercion.
    /// Returns `None` if no coercion applies.
    pub(super) fn try_coerce(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        // Numeric literal coercion (int, float, i128/u128)
        if let Some(coerced) = self.try_coerce_numeric_literal(expr, target_type) {
            return Some(coerced);
        }

        // Null literal → Option<T>
        if let Expr::Literal(lit) = expr
            && matches!(&lit.value, Literal::Null)
            && self.type_table.borrow().as_option(target_type).is_some()
        {
            return Some(TirExpr::new(TirExprKind::Null, target_type, lit.span));
        }

        // String/template literal → String newtype
        let is_string_or_template = matches!(
            expr,
            Expr::Literal(lit) if matches!(&lit.value, Literal::String(_))
        ) || matches!(expr, Expr::TemplateString(_));

        if is_string_or_template {
            let base_id = self.type_table.borrow().get_ultimate_base_type(target_type);
            let is_string_newtype = matches!(
                self.type_table.borrow().get(base_id),
                ResolvedType::Struct { name, .. } if name == "String"
            ) && target_type != base_id;
            if is_string_newtype {
                let mut resolved = self.resolve_expr(expr, ctx, None);
                resolved.type_id = target_type;
                return Some(resolved);
            }
        }

        // Tuple literal → type implementing SequenceLiteralBuilder (Array<T> and user types)
        if let Some(coerced) = self.try_coerce_tuple_to_sequence(expr, ctx, target_type) {
            return Some(coerced);
        }

        // Anonymous struct literal → type implementing KeyValueLiteralBuilder
        if let Some(coerced) = self.try_coerce_struct_to_map(expr, ctx, target_type) {
            return Some(coerced);
        }

        // If an anonymous struct literal targets a generic instance that doesn't
        // implement KeyValueLiteral, report a compile error.
        if let Expr::StructLiteral(struct_lit) = expr
            && struct_lit.name.is_none()
            && matches!(
                self.type_table.borrow().get(target_type),
                ResolvedType::GenericInstance { .. }
            )
        {
            let type_name = self.type_table.borrow().type_name(target_type);
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
    pub(super) fn try_coerce_struct_to_map(
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
        let impl_module_source = self.current_module_source.clone();

        let span = expr.span();
        let string_type = self
            .type_table
            .borrow_mut()
            .make_struct("String".to_string(), ModuleSource::string());
        let i32_type = TypeTable::I32;

        // Get type args for monomorphization from builder type
        let builder_base_name = self
            .struct_name_for_type(builder_type)
            .unwrap_or_else(|| base_name.clone());
        let (type_arg_names, type_arg_ids): (Vec<String>, Vec<TypeId>) = {
            let tt = self.type_table.borrow();
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

        // Build LabeledBlock:
        //   __kv_lit: {
        //     let mut __b = Builder::new_literal(capacity);
        //     __b.insert_literal("k", v); ...
        //     break __kv_lit: __b.build();   // only for new API
        //   }
        let label = "__kv_lit".to_string();
        ctx.enter_scope();

        // --- Builder::new_literal(capacity) StaticCall ---
        let new_method_info = LocalMethodName::new(
            builder_base_name.clone(),
            Some(trait_name.clone()),
            "new_literal".to_string(),
        )
        .with_struct_type_args(&type_arg_names);

        let new_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "new_literal");

        let capacity = struct_lit.fields.len() as u64;
        let new_args = if use_new_api {
            vec![TirExpr::new(
                TirExprKind::IntLiteral {
                    value: capacity,
                    repr: (capacity as i64).to_string(),
                },
                i32_type,
                span,
            )]
        } else {
            vec![]
        };

        let new_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: impl_module_source,
                    name: new_mangled_name,
                    monomorph_info: if type_arg_ids.is_empty() {
                        None
                    } else {
                        Some(MonomorphInfo {
                            generic_name: format!("{builder_base_name}::new_literal"),
                            impl_type_args: type_arg_ids.clone(),
                            method_type_args: vec![],
                            is_blanket: false,
                        })
                    },
                    method_info: Some(new_method_info),
                },
                type_args: vec![],
                args: new_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
            },
            builder_type,
            span,
        );

        // let mut __b = Builder::new_literal(capacity);
        let builder_index = ctx.add_local("__b".to_string(), builder_type, true, None);
        let mut stmts = vec![TirStmt::new(
            TirStmtKind::Let {
                name: "__b".to_string(),
                local_index: builder_index,
                is_mut: true,
                is_reactive: false,
                type_id: builder_type,
                value: new_call,
                skip_value_copy: false,
            },
            span,
        )];

        // --- For each field: __b.insert_literal(key, value) ---
        // Use the mangled builder name (with type args) so WIR can resolve the instantiated
        // function directly, which is needed when inlining is blocked (e.g., nested generics).
        let insert_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "insert_literal");

        let insert_method_info = LocalMethodName::new(
            builder_base_name.clone(),
            Some(trait_name.clone()),
            "insert_literal".to_string(),
        );

        // Check for duplicate field names
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
                    expected: self.type_table.borrow().type_name(value_type),
                    found: self.type_table.borrow().type_name(value.type_id),
                    span: field.value.span(),
                });
            }

            // Build receiver
            let builder_local = TirExpr::new(
                TirExprKind::Local {
                    index: builder_index,
                    name: "__b".to_string(),
                },
                builder_type,
                span,
            );
            let receiver =
                self.adjust_receiver_for_self_kind(builder_local, insert_self_kind, false, span);

            // Key: string literal from field name
            let key_expr = TirExpr::new(
                TirExprKind::StringLiteral(field.name.clone()),
                string_type,
                span,
            );

            let insert_call = Self::build_tir_method_call(
                receiver,
                FunctionRef {
                    module_source: self.current_module_source.clone(),
                    name: insert_mangled_name.clone(),
                    monomorph_info: None,
                    method_info: Some(insert_method_info.clone()),
                },
                vec![],
                vec![CallArg::new(key_expr, false), CallArg::new(value, false)],
                TypeTable::UNIT,
                span,
            );
            stmts.push(TirStmt::new(TirStmtKind::Expr(insert_call), span));
        }

        // For new API: break __kv_lit: __b.build();
        // For legacy API: break __kv_lit: __b;
        let builder_local_final = TirExpr::new(
            TirExprKind::Local {
                index: builder_index,
                name: "__b".to_string(),
            },
            builder_type,
            span,
        );

        let result_expr = if use_new_api {
            // Use the mangled builder name (with type args) so WIR can resolve the instantiated
            // function directly, which is needed when inlining is blocked (e.g., nested generics).
            let build_mangled_name =
                MethodName::format_local(&mangled_builder_name, Some(&trait_name), "build");
            let build_method_info = LocalMethodName::new(
                builder_base_name.clone(),
                Some(trait_name),
                "build".to_string(),
            );
            let build_monomorph = if type_arg_ids.is_empty() {
                None
            } else {
                Some(MonomorphInfo {
                    generic_name: format!("{builder_base_name}::build"),
                    impl_type_args: type_arg_ids,
                    method_type_args: vec![],
                    is_blanket: false,
                })
            };
            Self::build_tir_method_call(
                builder_local_final,
                FunctionRef {
                    module_source: self.current_module_source.clone(),
                    name: build_mangled_name,
                    monomorph_info: build_monomorph,
                    method_info: Some(build_method_info),
                },
                vec![],
                vec![],
                target_type,
                span,
            )
        } else {
            builder_local_final
        };

        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(result_expr),
            },
            span,
        ));

        ctx.exit_scope();

        Some(TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: target_type,
            },
            target_type,
            span,
        ))
    }

    /// Try to coerce a tuple/sequence literal `[e0, e1, ...]` to a user-defined type
    /// implementing `SequenceLiteralBuilder`.
    ///
    /// Coerce a tuple/sequence literal `[e0, e1, ...]` to a type implementing
    /// `SequenceLiteralBuilder` (including built-in `Array<T>` and user types).
    pub(super) fn try_coerce_tuple_to_sequence(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        let Expr::TupleLiteral(tuple_lit) = expr else {
            return None;
        };

        let base_name = self.struct_name_for_type(target_type)?;
        let (seq_info, needs_newtype_cast) = self
            .find_sequence_literal_trait_impl(&base_name, target_type)
            .map(|info| (info, false))
            .or_else(|| {
                // For newtypes, try the base type's SequenceLiteral impl
                let base_type = self.type_table.borrow().get_newtype_base(target_type)?;
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
            let tt = self.type_table.borrow();
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

        let label = "__seq_lit".to_string();
        ctx.enter_scope();

        // --- Builder::new_literal(capacity) ---
        let new_method_info = LocalMethodName::new(
            builder_base_name.clone(),
            Some(trait_name.clone()),
            "new_literal".to_string(),
        )
        .with_struct_type_args(&type_arg_names);
        let new_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "new_literal");
        let capacity = tuple_lit.elements.len() as u64;
        let new_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: impl_module_source.clone(),
                    name: new_mangled_name,
                    monomorph_info: if type_arg_ids.is_empty() {
                        None
                    } else {
                        Some(MonomorphInfo {
                            generic_name: format!("{builder_base_name}::new_literal"),
                            impl_type_args: type_arg_ids.clone(),
                            method_type_args: vec![],
                            is_blanket: false,
                        })
                    },
                    method_info: Some(new_method_info),
                },
                type_args: vec![],
                args: vec![CallArg::new(
                    TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: capacity,
                            repr: (capacity as i64).to_string(),
                        },
                        TypeTable::I32,
                        span,
                    ),
                    false,
                )],
            },
            builder_type,
            span,
        );

        let builder_index = ctx.add_local("__b".to_string(), builder_type, true, None);
        let mut stmts = vec![TirStmt::new(
            TirStmtKind::Let {
                name: "__b".to_string(),
                local_index: builder_index,
                is_mut: true,
                is_reactive: false,
                type_id: builder_type,
                value: new_call,
                skip_value_copy: false,
            },
            span,
        )];

        // --- For each element: __b.push_literal(elem) ---
        let push_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "push_literal");
        let push_method_info = LocalMethodName::new(
            builder_base_name.clone(),
            Some(trait_name.clone()),
            "push_literal".to_string(),
        )
        .with_struct_type_args(&type_arg_names);

        for element in &tuple_lit.elements {
            let elem_expr = self.resolve_expr(element, ctx, Some(element_type));
            // Verify element type is compatible with the expected element type.
            // Catches heterogeneous literals like `[1, "hello"] as Array<i32>`.
            if elem_expr.type_id != element_type
                && elem_expr.type_id != TypeTable::UNKNOWN
                && element_type != TypeTable::UNKNOWN
                && !self.type_table.borrow().contains_type_param(element_type)
            {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: format!(
                        "homogeneous elements of type '{}'",
                        self.type_table.borrow().type_name(element_type)
                    ),
                    found: format!(
                        "heterogeneous element of type '{}'",
                        self.type_table.borrow().type_name(elem_expr.type_id)
                    ),
                    span: element.span(),
                });
            }
            let builder_local = TirExpr::new(
                TirExprKind::Local {
                    index: builder_index,
                    name: "__b".to_string(),
                },
                builder_type,
                span,
            );
            let receiver =
                self.adjust_receiver_for_self_kind(builder_local, push_self_kind, false, span);
            let push_call = Self::build_tir_method_call(
                receiver,
                FunctionRef {
                    module_source: impl_module_source.clone(),
                    name: push_mangled_name.clone(),
                    monomorph_info: if type_arg_ids.is_empty() {
                        None
                    } else {
                        Some(MonomorphInfo {
                            generic_name: format!("{builder_base_name}::push_literal"),
                            impl_type_args: type_arg_ids.clone(),
                            method_type_args: vec![],
                            is_blanket: false,
                        })
                    },
                    method_info: Some(push_method_info.clone()),
                },
                vec![],
                vec![CallArg::new(elem_expr, false)],
                TypeTable::UNIT,
                span,
            );
            stmts.push(TirStmt::new(TirStmtKind::Expr(push_call), span));
        }

        // break __seq_lit: __b.build();
        let builder_local_final = TirExpr::new(
            TirExprKind::Local {
                index: builder_index,
                name: "__b".to_string(),
            },
            builder_type,
            span,
        );
        let build_mangled_name =
            MethodName::format_local(&mangled_builder_name, Some(&trait_name), "build");
        let build_method_info = LocalMethodName::new(
            builder_base_name.clone(),
            Some(trait_name),
            "build".to_string(),
        )
        .with_struct_type_args(&type_arg_names);
        let build_call = Self::build_tir_method_call(
            builder_local_final,
            FunctionRef {
                module_source: impl_module_source,
                name: build_mangled_name,
                monomorph_info: if type_arg_ids.is_empty() {
                    None
                } else {
                    Some(MonomorphInfo {
                        generic_name: format!("{builder_base_name}::build"),
                        impl_type_args: type_arg_ids,
                        method_type_args: vec![],
                        is_blanket: false,
                    })
                },
                method_info: Some(build_method_info),
            },
            vec![],
            vec![],
            output_type,
            span,
        );

        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(build_call),
            },
            span,
        ));

        ctx.exit_scope();

        let block_expr = TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: output_type,
            },
            output_type,
            span,
        );

        if needs_newtype_cast {
            Some(TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(block_expr),
                    target_type,
                },
                target_type,
                span,
            ))
        } else {
            Some(block_expr)
        }
    }
}
