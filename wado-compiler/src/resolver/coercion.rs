//! Numeric literal coercion and type coercion.

use crate::ast::{self, Expr, Literal, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::name::ModuleSource;
use crate::tir::{FunctionRef, ResolvedType, TirExpr, TirExprKind, TypeId, TypeTable};

use super::Resolver;
use super::types::{FunctionContext, TypeError};
use super::util;
use crate::name::LocalMethodName;

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn try_coerce_numeric_literal(
        &mut self,
        expr: &Expr,
        target_type: TypeId,
    ) -> Option<TirExpr> {
        // Number literal coercion to integer
        if let Expr::Literal(lit) = expr
            && let Literal::Number(num_lit) = &lit.value
            && self.type_table.borrow().is_integer(target_type)
        {
            if util::is_float_only_literal(&num_lit.repr) {
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '{}' as integer (has decimal point or negative exponent)",
                        num_lit.repr
                    ),
                    span: lit.span,
                });
                return Some(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: 0,
                        repr: num_lit.repr.clone(),
                    },
                    target_type,
                    lit.span,
                ));
            }
            return Some(match util::parse_u128_literal(&num_lit.repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_positive(
                        value,
                        target_type,
                        &self.type_table.borrow(),
                        &num_lit.repr,
                    ) {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: err_msg,
                            span: lit.span,
                        });
                    }
                    TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: value as u64,
                            repr: num_lit.repr.clone(),
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
                            repr: num_lit.repr.clone(),
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
            && let Literal::Number(num_lit) = &lit.value
            && self.type_table.borrow().is_integer(target_type)
        {
            if util::is_float_only_literal(&num_lit.repr) {
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!(
                        "cannot use float literal '-{}' as integer (has decimal point or negative exponent)",
                        num_lit.repr
                    ),
                    span: unary.span,
                });
                return Some(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: 0,
                        repr: format!("-{}", num_lit.repr),
                    },
                    target_type,
                    unary.span,
                ));
            }
            return Some(match util::parse_u128_literal(&num_lit.repr) {
                Ok(value) => {
                    if let Some(err_msg) = util::check_int_range_negative(
                        value,
                        target_type,
                        &self.type_table.borrow(),
                        &num_lit.repr,
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
                            repr: format!("-{}", num_lit.repr),
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
                            repr: format!("-{}", num_lit.repr),
                        },
                        target_type,
                        unary.span,
                    )
                }
            });
        }

        // Number literal coercion to float
        if let Expr::Literal(lit) = expr
            && let Literal::Number(num_lit) = &lit.value
            && self.type_table.borrow().is_float(target_type)
        {
            return Some(match util::parse_float_literal(&num_lit.repr) {
                Ok(value) => TirExpr::new(
                    TirExprKind::FloatLiteral {
                        value,
                        repr: num_lit.repr.clone(),
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
                            repr: num_lit.repr.clone(),
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
            && let Literal::Number(num_lit) = &lit.value
            && self.type_table.borrow().is_float(target_type)
        {
            return Some(match util::parse_float_literal(&num_lit.repr) {
                Ok(value) => TirExpr::new(
                    TirExprKind::FloatLiteral {
                        value: -value,
                        repr: format!("-{}", num_lit.repr),
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
                            repr: format!("-{}", num_lit.repr),
                        },
                        target_type,
                        unary.span,
                    )
                }
            });
        }

        // i128/u128 literal coercion
        if let Expr::Literal(lit) = expr
            && let Literal::Number(num_lit) = &lit.value
            && !util::is_float_only_literal(&num_lit.repr)
        {
            let struct_name = match self.type_table.borrow().get(target_type).clone() {
                ResolvedType::Struct { name, .. } => Some(name),
                _ => None,
            };

            if let Some(name) = struct_name
                && (name == "u128" || name == "i128")
            {
                let parse_result = if name == "u128" {
                    util::parse_u128_literal(&num_lit.repr).map(|v| v as i128)
                } else {
                    util::parse_i128_literal(&num_lit.repr)
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
                                (TypeTable::U64, "from_u64", value as u64)
                            } else {
                                (TypeTable::I64, "from_i64", value as u64)
                            };

                            let inner_literal = TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: store_value,
                                    repr: num_lit.repr.clone(),
                                },
                                inner_type,
                                lit.span,
                            );

                            let method_info =
                                LocalMethodName::new(name.clone(), None, method_name.to_string());
                            let mangled_func_name = method_info.to_mangled_name();

                            return Some(TirExpr::new(
                                TirExprKind::StaticCall {
                                    func: FunctionRef::External {
                                        module_source: ModuleSource::int128(),
                                        name: mangled_func_name,
                                        monomorph_info: None,
                                        method_info: Some(method_info),
                                    },
                                    args: vec![inner_literal],
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
                            message: format!("invalid {} literal: {}", name, num_lit.repr),
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
            && let Literal::Number(num_lit) = &lit.value
            && !util::is_float_only_literal(&num_lit.repr)
        {
            let struct_name = match self.type_table.borrow().get(target_type).clone() {
                ResolvedType::Struct { name, .. } => Some(name),
                _ => None,
            };

            if let Some(name) = struct_name
                && name == "i128"
            {
                let negated_repr = format!("-{}", num_lit.repr);
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
                    message: format!("invalid i128 literal: -{}", num_lit.repr),
                    span: unary.span,
                });
            }
        }

        None
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
            && matches!(
                self.type_table.borrow().get(target_type),
                ResolvedType::Option(_)
            )
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

        // Tuple literal → Array<T>
        let element_type_opt = self.type_table.borrow().as_array(target_type);
        if let Expr::TupleLiteral(tuple_lit) = expr
            && let Some(element_type) = element_type_opt
        {
            let elements: Vec<TirExpr> = tuple_lit
                .elements
                .iter()
                .map(|elem| self.resolve_expr(elem, ctx, Some(element_type)))
                .collect();

            return Some(TirExpr::new(
                TirExprKind::ArrayLiteral { elements },
                target_type,
                expr.span(),
            ));
        }

        None
    }
}
