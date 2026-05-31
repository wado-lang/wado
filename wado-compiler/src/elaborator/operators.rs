//! Binary and unary operator resolution, including operator overloading.

use crate::ast::{self, BinaryOp, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::name::{LocalMethodName, MethodName};
use crate::tir::{
    CallArg, FunctionRef, PrimitiveType, ResolvedType, TirBinaryOp, TirExpr, TirExprKind,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::types::{FunctionContext, ResolvedTraitMethod, TypeError};
use super::util;

/// The right-hand side of an assignment passed to
/// [`Elaborator::assign_to_target`]. Either an AST expression (the
/// regular [`Elaborator::resolve_assign`] path) or an already-resolved
/// TIR value (the [`Elaborator::resolve_compound_assign`] path, where the
/// RHS is `target op rhs` computed via
/// [`Elaborator::build_binary_op_tir`]).
///
/// The `Resolved` payload is boxed because `TirExpr` is ~460 bytes; an
/// unboxed variant would balloon every `AssignValue<'_>` (including
/// the `Ast(&expr)` form that fits in 8 bytes) to that size and waste
/// stack space on every compound-assign visit.
pub(super) enum AssignValue<'a> {
    Ast(&'a ast::Expr),
    Resolved(Box<TirExpr>),
}

impl AssignValue<'_> {
    fn span(&self) -> Span {
        match self {
            Self::Ast(expr) => expr.span(),
            Self::Resolved(tir) => tir.span,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        let (left, right) = self.resolve_binary_operands_with_coercion(
            &binary.left,
            binary.op,
            &binary.right,
            ctx,
            expected_type,
        );
        // Stage 5 (Gap 11): pin the binary's source AstId on the
        // side-channel so the operator-trait dispatch path can record
        // the decision under it. Cleared by
        // `build_trait_op_method_call_on_resolved` on success; we
        // also clear here defensively in case
        // `build_binary_op_tir` takes a native (non-dispatch) path,
        // so a later synthesised binary call doesn't pick up a stale
        // id from this entry.
        self.pending_operator_ast_id = Some(binary.id);
        let result = self.build_binary_op_tir(left, binary.op, right, binary.span);
        self.pending_operator_ast_id = None;
        result
    }

    /// Resolve both operands of a binary op, applying the standard
    /// bidirectional numeric-literal coercion. Shared between
    /// [`Self::resolve_binary`] and elaborator-internal callers like
    /// [`Self::desugar_comparison_chain`].
    ///
    /// For primitive numeric types: coerce the literal to the other operand's type.
    /// For struct types (e.g. `i128`/`u128`): look up the operator trait to determine
    /// the expected type for each operand from the method signature. This naturally
    /// handles operators with asymmetric parameter types (e.g. `Shl::shl(&self, rhs: u32)`)
    /// without special-casing specific operators.
    pub(super) fn resolve_binary_operands_with_coercion(
        &mut self,
        left_ast: &ast::Expr,
        op: BinaryOp,
        right_ast: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> (TirExpr, TirExpr) {
        let left_is_numeric_literal = self.tysys.is_numeric_literal(left_ast);
        let right_is_numeric_literal = self.tysys.is_numeric_literal(right_ast);

        if left_is_numeric_literal && !right_is_numeric_literal {
            // Resolve right first, then coerce left
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            let coerce_type = if self.tysys.type_table.borrow().is_numeric(right.type_id) {
                // Primitive type: coerce literal to the same type
                Some(right.type_id)
            } else {
                // Struct type: look up operator trait and use self type for lhs literal
                self.find_operator_self_type(right.type_id, &op)
            };
            let left = self.resolve_expr(left_ast, ctx, coerce_type);
            (left, right)
        } else if right_is_numeric_literal && !left_is_numeric_literal {
            // Resolve left first, then coerce right
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let coerce_type = if self.tysys.type_table.borrow().is_numeric(left.type_id) {
                // Primitive type: coerce literal to the same type
                Some(left.type_id)
            } else {
                // Struct type: look up operator trait and use rhs parameter type
                self.find_operator_rhs_type(left.type_id, &op)
            };
            let right = self.resolve_expr(right_ast, ctx, coerce_type);
            (left, right)
        } else if left_is_numeric_literal && right_is_numeric_literal {
            // Both literals - use expected type from context (e.g., assignment target)
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            (left, right)
        } else if self.tysys.is_null_literal(right_ast) && !self.tysys.is_null_literal(left_ast) {
            // `expr == null`: resolve the non-null side first and feed its
            // type to the bare `null` so it resolves to a concrete
            // `Option<T>` instead of `Option<UNKNOWN>`.
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let right = self.resolve_expr(right_ast, ctx, Some(left.type_id));
            (left, right)
        } else if self.tysys.is_null_literal(left_ast) && !self.tysys.is_null_literal(right_ast) {
            // `null == expr`: symmetric to the above.
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            let left = self.resolve_expr(left_ast, ctx, Some(right.type_id));
            (left, right)
        } else {
            // Both non-literals - propagate expected type for coercion
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            (left, right)
        }
    }

    /// Build a binary-op `TirExpr` given pre-resolved operands.
    ///
    /// Shared between [`Self::resolve_binary`] (the user-AST entry point)
    /// and elaborator-internal callers like
    /// [`Self::desugar_comparison_chain`] / [`Self::resolve_compound_assign`]
    /// that have already resolved both sides into TIR. Handles ref-equality,
    /// trait dispatch for non-primitive comparison / arithmetic / shift,
    /// flags-arith rejection, float-`%` rejection, and the trailing
    /// requires-trait diagnostic.
    pub(super) fn build_binary_op_tir(
        &mut self,
        left: TirExpr,
        op: BinaryOp,
        right: TirExpr,
        span: Span,
    ) -> TirExpr {
        // Check if this is a comparison operation on a non-primitive type
        // Non-primitives use Eq/Ord traits instead of direct Wasm instructions
        let left_type = self.tysys.type_table.borrow().get(left.type_id).clone();

        // Reference types (&T, &mut T): only == and != are allowed (identity via ref.eq).
        // All other operators (ordering, arithmetic, bitwise) are rejected.
        if matches!(&left_type, ResolvedType::Ref(_) | ResolvedType::MutRef(_)) {
            let right_type = self.tysys.type_table.borrow().get(right.type_id).clone();
            let both_refs = matches!(
                (&left_type, &right_type),
                (ResolvedType::Ref(_), ResolvedType::Ref(_))
                    | (ResolvedType::MutRef(_), ResolvedType::MutRef(_))
            );
            if both_refs && matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
                // Type check: reference types must match
                if left.type_id != right.type_id
                    && left.type_id != TypeTable::ERROR
                    && right.type_id != TypeTable::ERROR
                {
                    let type_table = self.tysys.type_table.borrow();
                    let left_name = type_table.type_name(left.type_id);
                    let right_name = type_table.type_name(right.type_id);
                    if left_name != right_name {
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: left_name,
                            found: right_name,
                            span,
                        });
                    }
                }
                let op = if op == BinaryOp::Eq {
                    TirBinaryOp::RefEq
                } else {
                    TirBinaryOp::RefNotEq
                };
                return TirExpr::new(
                    TirExprKind::Binary {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    TypeTable::BOOL,
                    span,
                );
            } else if both_refs {
                // All operators other than == and != are invalid on reference types
                let type_name = self.tysys.type_table.borrow().type_name(left.type_id);
                let op_str = match op {
                    BinaryOp::Lt => "<",
                    BinaryOp::LtEq => "<=",
                    BinaryOp::Gt => ">",
                    BinaryOp::GtEq => ">=",
                    BinaryOp::Add => "+",
                    BinaryOp::Sub => "-",
                    BinaryOp::Mul => "*",
                    BinaryOp::Div => "/",
                    BinaryOp::Mod => "%",
                    BinaryOp::BitAnd => "&",
                    BinaryOp::BitOr => "|",
                    BinaryOp::BitXor => "^",
                    BinaryOp::Shl => "<<",
                    BinaryOp::Shr => ">>",
                    BinaryOp::And => "&&",
                    BinaryOp::Or => "||",
                    _ => unreachable!(),
                };
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!("operator `{op_str}` cannot be applied to type `{type_name}`"),
                    span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
            }
            // If left is a reference but right is not (e.g., &i32 == i32),
            // fall through to the normal type mismatch error below.
        }

        let is_comparison = matches!(
            op,
            BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
        );

        if is_comparison {
            // Get struct name for trait lookup.
            // Newtypes of primitives (e.g. type Radians = f64) use primitive comparison.
            // Newtypes of structs need trait-based comparison via the base type's impl.
            let struct_name = match &left_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                ResolvedType::Newtype { base_type, .. } => {
                    let tt = self.tysys.type_table.borrow();
                    let ultimate = tt.get_ultimate_base_type(*base_type);
                    match tt.get(ultimate) {
                        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => {
                            drop(tt);
                            self.struct_name_for_type(*base_type)
                        }
                        _ => None,
                    }
                }
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // For newtypes, use the base type ID for trait lookup
                let lookup_type_id = {
                    let tt = self.tysys.type_table.borrow();
                    tt.get_newtype_base(left.type_id).unwrap_or(left.type_id)
                };

                // Handle Eq trait (== and !=)
                if matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
                    let eq_trait_name = self
                        .tysys
                        .type_table
                        .borrow()
                        .compiler_items()
                        .trait_name(CompilerItem::Eq)
                        .to_string();
                    let Some(resolved) = self.resolve_trait_method_for_op(
                        &struct_name,
                        lookup_type_id,
                        &eq_trait_name,
                        "eq",
                        false,
                    ) else {
                        let type_name = self.tysys.type_table.borrow().type_name(left.type_id);
                        let op_str = if op == BinaryOp::Eq { "==" } else { "!=" };
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!(
                                "operator `{op_str}` cannot be applied to type `{type_name}` (does not implement Eq trait)"
                            ),
                            span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
                    };
                    let call = self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                    if op == BinaryOp::NotEq && call.type_id == TypeTable::BOOL {
                        return TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Not,
                                expr: Box::new(call),
                            },
                            TypeTable::BOOL,
                            span,
                        );
                    }
                    return call;
                }

                // Handle Ord trait (<, >, <=, >=)
                if matches!(
                    op,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
                ) {
                    let ord_trait_name = self
                        .tysys
                        .type_table
                        .borrow()
                        .compiler_items()
                        .trait_name(CompilerItem::Ord)
                        .to_string();
                    let Some(resolved) = self.resolve_trait_method_for_op(
                        &struct_name,
                        lookup_type_id,
                        &ord_trait_name,
                        "cmp",
                        false,
                    ) else {
                        let type_name = self.tysys.type_table.borrow().type_name(left.type_id);
                        let op_str = match op {
                            BinaryOp::Lt => "<",
                            BinaryOp::Gt => ">",
                            BinaryOp::LtEq => "<=",
                            BinaryOp::GtEq => ">=",
                            _ => unreachable!(),
                        };
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!(
                                "operator `{op_str}` cannot be applied to type `{type_name}` (does not implement Ord trait)"
                            ),
                            span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
                    };
                    let cmp_call = self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                    if cmp_call.type_id == TypeTable::ERROR {
                        return cmp_call;
                    }
                    return self.ord_bool_from_cmp(cmp_call, op, span);
                }
            }

            // TypeParam/TypePack with trait bounds: emit trait method calls for comparison operators.
            // Monomorphization substitutes T with the concrete type and either resolves
            // the method call normally, or converts back to a binary op for primitives.
            let type_param_name = match &left_type {
                ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } => {
                    Some(name.clone())
                }
                _ => None,
            };
            if let Some(name) = type_param_name
                && let Some(bounds) = self.trait_ctx.type_param_bounds.get(&name).cloned()
            {
                let bound_names: Vec<String> = bounds.iter().map(|b| b.name.clone()).collect();
                if matches!(op, BinaryOp::Eq | BinaryOp::NotEq)
                    && let Some((_trait_name, info)) =
                        self.find_method_in_trait_bounds(&bound_names, "eq", left.type_id)
                {
                    let eq_trait_name = self
                        .tysys
                        .type_table
                        .borrow()
                        .compiler_items()
                        .trait_name(CompilerItem::Eq)
                        .to_string();
                    let resolved = ResolvedTraitMethod {
                        trait_name: eq_trait_name,
                        method_name: "eq".to_string(),
                        impl_name: name.clone(),
                        self_kind: info.self_kind,
                        return_type: info.return_type,
                        param_types: info.param_types,
                        is_type_param_receiver: true,
                    };
                    let call = self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                    if op == BinaryOp::NotEq && call.type_id == TypeTable::BOOL {
                        return TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Not,
                                expr: Box::new(call),
                            },
                            TypeTable::BOOL,
                            span,
                        );
                    }
                    return call;
                }
                if matches!(
                    op,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
                ) && let Some((_trait_name, info)) =
                    self.find_method_in_trait_bounds(&bound_names, "cmp", left.type_id)
                {
                    let ord_trait_name = self
                        .tysys
                        .type_table
                        .borrow()
                        .compiler_items()
                        .trait_name(CompilerItem::Ord)
                        .to_string();
                    let resolved = ResolvedTraitMethod {
                        trait_name: ord_trait_name,
                        method_name: "cmp".to_string(),
                        impl_name: name.clone(),
                        self_kind: info.self_kind,
                        return_type: info.return_type,
                        param_types: info.param_types,
                        is_type_param_receiver: true,
                    };
                    let cmp_call = self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                    if cmp_call.type_id == TypeTable::ERROR {
                        return cmp_call;
                    }
                    return self.ord_bool_from_cmp(cmp_call, op, span);
                }
            }
        }

        // Check if this is an arithmetic or bitwise operation on a non-primitive type
        // Non-primitives use Add/Sub/Mul/Div/Rem/BitAnd/BitOr/BitXor traits
        let is_arithmetic_or_bitwise = matches!(
            op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Mod
                | BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
        );

        if is_arithmetic_or_bitwise {
            // Get struct name for trait lookup
            let struct_name = match &left_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => {
                    Some(name.clone())
                }
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // Determine which trait and method to use based on operator
                let (trait_name, method_name) = match op {
                    BinaryOp::Add => ("Add", "add"),
                    BinaryOp::Sub => ("Sub", "sub"),
                    BinaryOp::Mul => ("Mul", "mul"),
                    BinaryOp::Div => ("Div", "div"),
                    BinaryOp::Mod => ("Rem", "rem"),
                    BinaryOp::BitAnd => ("BitAnd", "bitand"),
                    BinaryOp::BitOr => ("BitOr", "bitor"),
                    BinaryOp::BitXor => ("BitXor", "bitxor"),
                    _ => unreachable!(),
                };

                // For newtypes, resolve base type for trait impl fallback
                let (lookup_name, lookup_type_id) =
                    self.newtype_base_lookup(&struct_name, left.type_id);

                // Find the arithmetic trait implementation
                let (trait_info_opt, impl_name) = self
                    .find_arithmetic_trait_impl(&struct_name, left.type_id, trait_name, method_name)
                    .map(|info| (Some(info), struct_name.clone()))
                    .unwrap_or_else(|| {
                        let info = self.find_arithmetic_trait_impl(
                            &lookup_name,
                            lookup_type_id,
                            trait_name,
                            method_name,
                        );
                        (info, lookup_name.clone())
                    });
                if let Some(trait_info) = trait_info_opt {
                    let resolved = ResolvedTraitMethod {
                        trait_name: trait_info.trait_name,
                        method_name: method_name.to_string(),
                        impl_name,
                        self_kind: trait_info.self_kind,
                        return_type: trait_info.output_type,
                        param_types: trait_info.rhs_type.map(|t| vec![t]).unwrap_or_default(),
                        is_type_param_receiver: false,
                    };
                    return self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                }
            }

            // TypeParam with trait bounds: resolve arithmetic operators via trait bound methods
            if let ResolvedType::TypeParam { name, .. } = &left_type
                && let Some(bounds) = self.trait_ctx.type_param_bounds.get(name).cloned()
            {
                let operand_type_id = left.type_id;
                let bound_names: Vec<String> = bounds.iter().map(|b| b.name.clone()).collect();
                let (trait_name, method_name) = match op {
                    BinaryOp::Add => ("Add", "add"),
                    BinaryOp::Sub => ("Sub", "sub"),
                    BinaryOp::Mul => ("Mul", "mul"),
                    BinaryOp::Div => ("Div", "div"),
                    BinaryOp::Mod => ("Rem", "rem"),
                    BinaryOp::BitAnd => ("BitAnd", "bitand"),
                    BinaryOp::BitOr => ("BitOr", "bitor"),
                    BinaryOp::BitXor => ("BitXor", "bitxor"),
                    _ => unreachable!(),
                };
                if let Some((_found_trait, info)) =
                    self.find_method_in_trait_bounds(&bound_names, method_name, left.type_id)
                {
                    // For type-param arithmetic operators, Output == Self is the common
                    // case and TypeParam types get properly substituted by monomorphization.
                    // Using AssocTypeProjection would cause unresolved types for primitives
                    // that don't register associated types.
                    let resolved = ResolvedTraitMethod {
                        trait_name: trait_name.to_string(),
                        method_name: method_name.to_string(),
                        impl_name: name.clone(),
                        self_kind: info.self_kind,
                        return_type: operand_type_id,
                        param_types: info.param_types,
                        is_type_param_receiver: true,
                    };
                    return self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                }
            }
        }

        // Check if this is a shift operation on a non-primitive type
        // Non-primitives use Shl/Shr traits (with rhs: u32, not &Self)
        let is_shift = matches!(op, BinaryOp::Shl | BinaryOp::Shr);

        if is_shift {
            // Get struct name for trait lookup
            let struct_name = match &left_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => {
                    Some(name.clone())
                }
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // Determine which trait and method to use based on operator
                let (trait_name, method_name) = match op {
                    BinaryOp::Shl => ("Shl", "shl"),
                    BinaryOp::Shr => ("Shr", "shr"),
                    _ => unreachable!(),
                };

                // For newtypes, resolve base type for trait impl fallback
                let (lookup_name, lookup_type_id) =
                    self.newtype_base_lookup(&struct_name, left.type_id);

                // Find the shift trait implementation
                let (trait_info_opt, impl_name) = self
                    .find_arithmetic_trait_impl(&struct_name, left.type_id, trait_name, method_name)
                    .map(|info| (Some(info), struct_name.clone()))
                    .unwrap_or_else(|| {
                        let info = self.find_arithmetic_trait_impl(
                            &lookup_name,
                            lookup_type_id,
                            trait_name,
                            method_name,
                        );
                        (info, lookup_name.clone())
                    });
                if let Some(trait_info) = trait_info_opt {
                    // Shift traits declare `rhs: u32` (not `&Self`), so the
                    // shared builder will type-check against u32 and will not
                    // wrap the operand in `&`.
                    let resolved = ResolvedTraitMethod {
                        trait_name: trait_info.trait_name,
                        method_name: method_name.to_string(),
                        impl_name,
                        self_kind: trait_info.self_kind,
                        return_type: trait_info.output_type,
                        param_types: trait_info.rhs_type.map(|t| vec![t]).unwrap_or_default(),
                        is_type_param_receiver: false,
                    };
                    return self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                }
            }
        }

        // Check for arithmetic on flags types: only bitwise ops are allowed
        {
            let left_resolved = self.tysys.type_table.borrow().get(left.type_id).clone();
            let is_flags_arith = matches!(
                op,
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod
            ) && matches!(left_resolved, ResolvedType::Flags { .. });
            if is_flags_arith {
                let op_char = match op {
                    BinaryOp::Add => "+",
                    BinaryOp::Sub => "-",
                    BinaryOp::Mul => "*",
                    BinaryOp::Div => "/",
                    BinaryOp::Mod => "%",
                    _ => unreachable!(),
                };
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!(
                        "arithmetic operator `{op_char}` is not allowed on flags types; use bitwise operators (`|`, `&`, `^`) instead"
                    ),
                    span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
            }
        }

        // Check for modulo on float types: Wasm has no float remainder instruction.
        // Users should use f64::fmod() or f32::fmod() instead.
        {
            let left_resolved = self.tysys.type_table.borrow().get(left.type_id).clone();
            if matches!(op, BinaryOp::Mod)
                && matches!(
                    left_resolved,
                    ResolvedType::Primitive(PrimitiveType::F32 | PrimitiveType::F64)
                )
            {
                let type_name = match left_resolved {
                    ResolvedType::Primitive(PrimitiveType::F32) => "f32",
                    _ => "f64",
                };
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!(
                        "operator `%` is not supported on `{type_name}`; use `{type_name}::fmod(a, b)` instead"
                    ),
                    span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
            }
        }

        // If the left operand is a non-primitive type that cannot fall through
        // to a native Wasm binary instruction, it must dispatch through a
        // trait implementation.  Reaching here with such a type means no
        // matching trait impl was found — emit a diagnostic before we can
        // accidentally build a TIR `Binary` node that would crash codegen
        // with "type mismatch: expected ... found (ref $type)" on Wasm
        // validation (see regression fixture `typecheck_binop_fn_eq.wado`).
        {
            let requires_trait = match &left_type {
                ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
                ResolvedType::Newtype { base_type, .. } => {
                    let tt = self.tysys.type_table.borrow();
                    let ultimate = tt.get_ultimate_base_type(*base_type);
                    matches!(
                        tt.get(ultimate),
                        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
                    )
                }
                // Types with no native Wasm binary-op support and no trait
                // implementation in the prelude.  Without this rejection,
                // codegen emits `ref.eq` / `i32.eq` against a GC reference
                // and fails validation.
                ResolvedType::Function { .. }
                | ResolvedType::Resource { .. }
                | ResolvedType::GenericResource { .. }
                | ResolvedType::Reactive(_)
                | ResolvedType::AssocTypeProjection { .. } => true,
                _ => false,
            };
            if requires_trait
                && !matches!(op, BinaryOp::And | BinaryOp::Or)
                && left.type_id != TypeTable::ERROR
                && right.type_id != TypeTable::ERROR
            {
                let type_table = self.tysys.type_table.borrow();
                let type_name = type_table.type_name(left.type_id);
                let op_char = match op {
                    BinaryOp::Add => "+",
                    BinaryOp::Sub => "-",
                    BinaryOp::Mul => "*",
                    BinaryOp::Div => "/",
                    BinaryOp::Mod => "%",
                    BinaryOp::BitAnd => "&",
                    BinaryOp::BitOr => "|",
                    BinaryOp::BitXor => "^",
                    BinaryOp::Shl => "<<",
                    BinaryOp::Shr => ">>",
                    BinaryOp::Eq => "==",
                    BinaryOp::NotEq => "!=",
                    BinaryOp::Lt => "<",
                    BinaryOp::LtEq => "<=",
                    BinaryOp::Gt => ">",
                    BinaryOp::GtEq => ">=",
                    _ => "?",
                };
                let (eq_trait_name, ord_trait_name) = {
                    let items = type_table.compiler_items();
                    (
                        items.trait_name(CompilerItem::Eq).to_string(),
                        items.trait_name(CompilerItem::Ord).to_string(),
                    )
                };
                let trait_name: String = match op {
                    BinaryOp::Add => "Add".to_string(),
                    BinaryOp::Sub => "Sub".to_string(),
                    BinaryOp::Mul => "Mul".to_string(),
                    BinaryOp::Div => "Div".to_string(),
                    BinaryOp::Mod => "Rem".to_string(),
                    BinaryOp::BitAnd => "BitAnd".to_string(),
                    BinaryOp::BitOr => "BitOr".to_string(),
                    BinaryOp::BitXor => "BitXor".to_string(),
                    BinaryOp::Shl => "Shl".to_string(),
                    BinaryOp::Shr => "Shr".to_string(),
                    BinaryOp::Eq | BinaryOp::NotEq => eq_trait_name,
                    BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => ord_trait_name,
                    _ => "?".to_string(),
                };
                drop(type_table);
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!(
                        "operator `{op_char}` cannot be applied to type `{type_name}`: type does not implement `{trait_name}`"
                    ),
                    span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
            }
        }

        let tir_op = util::convert_binary_op(op);

        // Type check: both operands of a binary operation must have the same type.
        // This applies to primitives, newtypes, and all non-overloaded binary ops.
        // (Operator overloading via traits is dispatched earlier and doesn't reach here.)
        // Logical operators (&&, ||) are excluded since both sides are always bool.
        // We compare resolved types (not just TypeIds) because generic instantiation
        // can create distinct TypeIds for the same logical type.
        if !matches!(op, BinaryOp::And | BinaryOp::Or)
            && left.type_id != right.type_id
            && left.type_id != TypeTable::ERROR
            && right.type_id != TypeTable::ERROR
            && left.type_id != TypeTable::NEVER
            && right.type_id != TypeTable::NEVER
        {
            let type_table = self.tysys.type_table.borrow();
            let left_name = type_table.type_name(left.type_id);
            let right_name = type_table.type_name(right.type_id);
            if left_name != right_name {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: left_name,
                    found: right_name,
                    span,
                });
            }
        }

        // Determine result type based on operator
        let type_id = match op {
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or => TypeTable::BOOL,
            _ => left.type_id, // Arithmetic ops preserve the type
        };

        TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(left),
                op: tir_op,
                right: Box::new(right),
            },
            type_id,
            span,
        )
    }

    /// Resolve a unary expression
    pub(super) fn resolve_unary(
        &mut self,
        unary: &ast::UnaryExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // For `&x` / `&mut x`, peel one layer of the expected type so the
        // operand sees the underlying expected shape. This lets a generic
        // function reference taken by `&identity` (expected `&fn(i32) -> i32`)
        // pin its type arguments from the inner `fn(i32) -> i32` the same
        // way a bare `identity` to a `fn(...)` parameter would.
        let inner_expected = if matches!(unary.op, UnaryOp::Ref | UnaryOp::MutRef) {
            expected_type.and_then(|expected| {
                let table = self.tysys.type_table.borrow();
                match table.get(expected) {
                    ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Some(*inner),
                    _ => None,
                }
            })
        } else {
            None
        };
        let expr = self.resolve_expr(&unary.expr, ctx, inner_expected);
        let op = util::convert_unary_op(unary.op);

        // Track address-taken locals for &x and &mut x
        if matches!(unary.op, UnaryOp::Ref | UnaryOp::MutRef)
            && let TirExprKind::Local { index, .. } = &expr.kind
        {
            ctx.address_taken_locals.insert(*index);
        }

        // Check that &mut is only applied to mutable locals
        if unary.op == UnaryOp::MutRef
            && let TirExprKind::Local { name, .. } = &expr.kind
            && let Some(local) = ctx.lookup(name)
            && !local.is_mut
        {
            let _ = self.logger.error(TypeError::CannotAssign {
                message: format!("cannot take &mut of immutable variable '{name}'"),
                span: unary.span,
            });
        }

        // Reject &mut on struct field access when the field is a primitive type.
        // In Wasm GC, struct.get returns a value copy for primitives, so &mut field
        // creates a disconnected Box — mutations don't propagate back to the struct.
        // For GC reference types (struct, String, Array, etc.), struct.get returns
        // the shared reference, so &mut field works correctly.
        if unary.op == UnaryOp::MutRef && matches!(&expr.kind, TirExprKind::FieldAccess { .. }) {
            let field_type = self.tysys.type_table.borrow().get(expr.type_id).clone();
            let base_type = self
                .tysys
                .type_table
                .borrow()
                .get(
                    self.tysys
                        .type_table
                        .borrow()
                        .get_ultimate_base_type(expr.type_id),
                )
                .clone();
            if matches!(field_type, ResolvedType::Primitive(_))
                || matches!(base_type, ResolvedType::Primitive(_))
            {
                let _ = self.logger.error(TypeError::CannotAssign {
                    message: "cannot take mutable reference to primitive struct field; use the struct reference directly".to_string(),
                    span: unary.span,
                });
            }
        }

        // Unary trait operators (`-x`, `~x`) dispatch through the same
        // `resolve_trait_method_for_op` + `build_trait_op_method_call_on_resolved`
        // pipeline as binary operators.  The builder handles the zero-arg
        // case via `resolved.param_types.is_empty()`.
        if let Some((trait_name, method_name)) = match unary.op {
            UnaryOp::Neg => Some(("Neg", "neg")),
            UnaryOp::BitNot => Some(("BitNot", "bitnot")),
            _ => None,
        } {
            let expr_type = self.tysys.type_table.borrow().get(expr.type_id).clone();
            let struct_name = match &expr_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => {
                    Some(name.clone())
                }
                _ => None,
            };
            if let Some(struct_name) = struct_name {
                let (lookup_name, lookup_type_id) =
                    self.newtype_base_lookup(&struct_name, expr.type_id);
                let resolved = self
                    .resolve_trait_method_for_op(
                        &struct_name,
                        expr.type_id,
                        trait_name,
                        method_name,
                        false,
                    )
                    .or_else(|| {
                        self.resolve_trait_method_for_op(
                            &lookup_name,
                            lookup_type_id,
                            trait_name,
                            method_name,
                            false,
                        )
                    });
                if let Some(resolved) = resolved {
                    // Record the dispatch keyed by the unary expr's AstId
                    // (via the `pending_operator_ast_id` side-channel) so
                    // reify can replay the `Neg::neg` / `BitNot::bitnot`
                    // method call instead of emitting a bare `Unary` on a
                    // struct (which codegen rejects: `expected i32, found
                    // (ref $T)`). Mirrors the binary-operator path.
                    self.pending_operator_ast_id = Some(unary.id);
                    return self.build_trait_op_method_call_on_resolved(
                        expr,
                        vec![],
                        &resolved,
                        unary.span,
                    );
                }
            }
        }

        // Constant folding: fold -literal into a negative literal
        if unary.op == UnaryOp::Neg {
            match &expr.kind {
                TirExprKind::IntLiteral { value, repr } => {
                    // Fold -N into a negative literal
                    // Use wrapping negation to handle edge cases like -i64::MIN
                    // Store as u64 (two's complement representation)
                    let neg_value = (*value as i64).wrapping_neg().cast_unsigned();
                    return TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: neg_value,
                            repr: format!("-{repr}"),
                        },
                        expr.type_id,
                        unary.span,
                    );
                }
                TirExprKind::FloatLiteral { value, repr } => {
                    // Fold -N.M into a negative float literal
                    return TirExpr::new(
                        TirExprKind::FloatLiteral {
                            value: -value,
                            repr: format!("-{repr}"),
                        },
                        expr.type_id,
                        unary.span,
                    );
                }
                // Handle -(N as T) -> (-N) as T for integer casts
                TirExprKind::Cast {
                    expr: inner,
                    target_type,
                } => {
                    if let TirExprKind::IntLiteral { value, repr } = &inner.kind {
                        let neg_value = (*value as i64).wrapping_neg().cast_unsigned();
                        let neg_literal = TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: neg_value,
                                repr: format!("-{repr}"),
                            },
                            inner.type_id,
                            unary.span,
                        );
                        return TirExpr::new(
                            TirExprKind::Cast {
                                expr: Box::new(neg_literal),
                                target_type: *target_type,
                            },
                            *target_type,
                            unary.span,
                        );
                    }
                }
                _ => {}
            }
        }

        let type_id = match unary.op {
            UnaryOp::Not => TypeTable::BOOL,
            UnaryOp::Ref => self.tysys.type_table.borrow_mut().make_ref(expr.type_id),
            UnaryOp::MutRef => self
                .tysys
                .type_table
                .borrow_mut()
                .make_mut_ref(expr.type_id),
            UnaryOp::Deref => {
                let outer = self.tysys.type_table.borrow().get(expr.type_id).clone();
                match outer {
                    ResolvedType::MutRef(inner) => inner,
                    ResolvedType::Ref(inner) => {
                        // Dereffing a shared reference: &(&mut T) yields &T, not &mut T.
                        // Mutability cannot propagate outward through a shared reference.
                        let inner_resolved = self.tysys.type_table.borrow().get(inner).clone();
                        if let ResolvedType::MutRef(u) = inner_resolved {
                            self.tysys.type_table.borrow_mut().make_ref(u)
                        } else {
                            inner
                        }
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: "reference type".to_string(),
                            found: self.tysys.type_table.borrow().type_name(expr.type_id),
                            span: unary.span,
                        });
                        TypeTable::ERROR
                    }
                }
            }
            _ => expr.type_id,
        };

        TirExpr::new(
            TirExprKind::Unary {
                op,
                expr: Box::new(expr),
            },
            type_id,
            unary.span,
        )
    }

    /// Resolve an assignment expression.
    pub(super) fn resolve_assign(
        &mut self,
        assign: &ast::AssignExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        self.assign_to_target(
            &assign.target,
            AssignValue::Ast(&assign.value),
            assign.span,
            ctx,
        )
    }

    /// Build an assignment TIR for `target = value`, where the value may
    /// be a user-AST expression or an already-resolved [`TirExpr`].
    ///
    /// This is the shared core of [`Self::resolve_assign`] and
    /// [`Self::resolve_compound_assign`]: both go through the same target
    /// dispatch (`IndexAssign` trait on custom types, `GlobalVarSet` on
    /// globals, plain `Assign` otherwise), differing only in where the
    /// right-hand value comes from.
    pub(super) fn assign_to_target(
        &mut self,
        target_ast: &ast::Expr,
        value: AssignValue<'_>,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Check for index assignment on custom types: arr[i] = value -> arr.index_assign(i, value)
        if let ast::Expr::Index(index_expr) = target_ast {
            // Resolve the indexed expression to get its type
            let indexed_expr = self.resolve_expr(&index_expr.expr, ctx, None);

            // Get base type (unwrap reference if needed)
            let base_type_id = match self.tysys.type_table.borrow().get(indexed_expr.type_id) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                _ => indexed_expr.type_id,
            };

            // Check for IndexAssign trait implementation
            // Arrays now use IndexAssign trait like other types
            {
                // Check for IndexAssign trait implementation
                let struct_name = match self.tysys.type_table.borrow().get(base_type_id).clone() {
                    ResolvedType::Struct { name, .. } => name,
                    ResolvedType::GenericInstance { name, .. } => name,
                    ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => name,
                    _ => String::new(),
                };

                // For newtypes, resolve the base type name for trait impl lookup
                let (lookup_name, lookup_type_id) =
                    self.newtype_base_lookup(&struct_name, base_type_id);

                if !struct_name.is_empty() {
                    let index_resolved = self.resolve_expr(&index_expr.index, ctx, None);
                    let index_type = index_resolved.type_id;

                    // Reject &T/&mut T used as index expression (would ICE in codegen)
                    let derefed_index_type = match self.tysys.type_table.borrow().get(index_type) {
                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Some(*inner),
                        _ => None,
                    };
                    if let Some(expected) = derefed_index_type {
                        self.typecheck(index_type, expected, index_expr.index.span());
                    }

                    let assign_info = self
                        .find_index_assign_trait_impl(&struct_name, base_type_id, index_type)
                        .or_else(|| {
                            self.find_index_assign_trait_impl(
                                &lookup_name,
                                lookup_type_id,
                                index_type,
                            )
                        });
                    if let Some(trait_info) = assign_info {
                        // Generate: expr.index_assign(index, value)
                        let value_span = value.span();
                        let value_tir = match value {
                            AssignValue::Ast(expr) => {
                                self.resolve_expr(expr, ctx, Some(trait_info.input_type))
                            }
                            AssignValue::Resolved(tir) => *tir,
                        };

                        // Check: reject &T/&mut T assigned where non-ref expected
                        self.typecheck(value_tir.type_id, trait_info.input_type, value_span);

                        let receiver = self.adjust_receiver_for_self_kind(
                            indexed_expr,
                            trait_info.self_kind,
                            false,
                            span,
                        );

                        // Get the mangled method name: StructName^IndexAssign<IndexType>::index_assign
                        let mangled_method_name = MethodName::format_local(
                            &lookup_name,
                            Some(&trait_info.trait_name),
                            "index_assign",
                        );

                        let func = FunctionRef {
                            module_source: trait_info.impl_module_source.clone(),
                            name: mangled_method_name,
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                lookup_name,
                                Some(trait_info.trait_name),
                                "index_assign".to_string(),
                            )),
                        };

                        // Stage 5 (WEP 2026-05-26): record the
                        // resolved `IndexAssign` dispatch keyed by
                        // the inner `IndexExpr`'s `AstId` so reify
                        // can replay the same `arr.index_assign(idx,
                        // value)` shape for `arr[i] = v` and
                        // `arr[i] OP= v`.
                        self.record_index_assign_dispatch(
                            index_expr.id,
                            super::sem::types::OperatorDispatch {
                                function_ref: func.clone(),
                                self_kind: trait_info.self_kind,
                                arg_ref_wraps: vec![false, false],
                                return_type: TypeTable::UNIT,
                                needs_deref: false,
                            },
                        );

                        return Self::build_tir_method_call(
                            receiver,
                            func,
                            vec![],
                            vec![
                                CallArg::new(index_resolved, false),
                                CallArg::new(value_tir, false),
                            ],
                            TypeTable::UNIT,
                            span,
                        );
                    }
                }
            }
        }

        // Standard assignment handling. Fall through here happens when the
        // target isn't `Expr::Index`, or when the IndexAssign trait lookup
        // returned None — `value` was not consumed on either of those paths.
        let target = self.resolve_expr(target_ast, ctx, None);
        let value_span = value.span();
        let value_tir = match value {
            AssignValue::Ast(expr) => self.resolve_expr(expr, ctx, Some(target.type_id)),
            AssignValue::Resolved(tir) => *tir,
        };

        // Reject &T assigned where non-ref T expected
        self.typecheck(value_tir.type_id, target.type_id, value_span);

        // Handle assignment to global variables
        if let TirExprKind::GlobalVarGet {
            module_source,
            name,
        } = &target.kind
        {
            // Check if the global is mutable (check both local and imported globals)
            let is_mutable = self
                .sem
                .decls
                .current_module_globals
                .get(name)
                .map(|(_, m)| *m)
                .or_else(|| {
                    // For imported globals, the name in the TIR is the original name from source
                    // We need to find it by iterating through imported_globals
                    self.sem
                        .decls
                        .imported_globals
                        .values()
                        .find(|(src, orig_name, _, _)| src == module_source && orig_name == name)
                        .map(|(_, _, _, m)| *m)
                });

            if let Some(is_mut) = is_mutable {
                if !is_mut {
                    let _ = self.logger.error(TypeError::CannotAssign {
                        message: format!("cannot assign to immutable global variable '{name}'"),
                        span: target_ast.span(),
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
                }
                // Generate GlobalVarSet instead of Assign
                return TirExpr::new(
                    TirExprKind::GlobalVarSet {
                        module_source: module_source.clone(),
                        name: name.clone(),
                        value: Box::new(value_tir),
                    },
                    TypeTable::UNIT,
                    span,
                );
            }
        }

        // Validate that the target is a valid l-value
        let is_valid_lvalue = match &target.kind {
            TirExprKind::Local { .. } => true,
            TirExprKind::FieldAccess { .. } => true,
            TirExprKind::Index { .. } => true,
            // Dereference is a valid l-value only through mutable reference
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr,
                ..
            } => {
                let inner_type = self.tysys.type_table.borrow().get(expr.type_id).clone();
                if matches!(inner_type, ResolvedType::Ref(_)) {
                    let _ = self.logger.error(TypeError::CannotAssign {
                        message: "cannot assign through immutable reference".to_string(),
                        span: target_ast.span(),
                    });
                    false
                } else {
                    true
                }
            }
            _ => false,
        };

        if !is_valid_lvalue {
            // Report error for invalid assignment target
            let _ = self.logger.error(TypeError::CannotAssign {
                message: "expression is not assignable".to_string(),
                span: target_ast.span(),
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
        }

        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value_tir),
            },
            TypeTable::UNIT,
            span,
        )
    }

    /// Resolve `target op= value` as the equivalent `target = target op value`,
    /// fully TIR-direct.
    ///
    /// The READ side comes from `resolve_expr(&compound.target, …)`, which
    /// naturally dispatches the `Index` trait for indexed reads on custom
    /// types. The WRITE side reuses [`Self::assign_to_target`], which
    /// handles `GlobalVarSet`, the `IndexAssign` trait, and plain `Assign`
    /// — feeding it the already-resolved combined value via
    /// [`AssignValue::Resolved`] so no synthesised AST is involved.
    ///
    /// Note: `target` is evaluated twice. For pure l-values (locals,
    /// globals, fields of locals) that is fine; for impure l-values like
    /// `arr[bump()] += 1` the inner sub-expressions run twice, matching
    /// the historical desugar-phase behaviour. Binding impure
    /// sub-expressions to temporaries first is a separate concern.
    pub(super) fn resolve_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        self.record_desugar(compound.id, super::sem::types::DesugarKind::CompoundAssign);
        let op = match compound.op {
            ast::CompoundAssignOp::Add => BinaryOp::Add,
            ast::CompoundAssignOp::Sub => BinaryOp::Sub,
            ast::CompoundAssignOp::Mul => BinaryOp::Mul,
            ast::CompoundAssignOp::Div => BinaryOp::Div,
            ast::CompoundAssignOp::Mod => BinaryOp::Mod,
            ast::CompoundAssignOp::BitAnd => BinaryOp::BitAnd,
            ast::CompoundAssignOp::BitOr => BinaryOp::BitOr,
            ast::CompoundAssignOp::BitXor => BinaryOp::BitXor,
            ast::CompoundAssignOp::Shl => BinaryOp::Shl,
            ast::CompoundAssignOp::Shr => BinaryOp::Shr,
        };
        let read = self.resolve_expr(&compound.target, ctx, None);
        let rhs = self.resolve_expr(&compound.value, ctx, Some(read.type_id));
        let combined = self.build_binary_op_tir(read, op, rhs, compound.span);
        self.assign_to_target(
            &compound.target,
            AssignValue::Resolved(Box::new(combined)),
            compound.span,
            ctx,
        )
    }

    /// Build a `TirExpr` for `a OP1 b OP2 c [OP3 d …]` as the equivalent
    /// `(a OP1 b) && (b OP2 c) [&& (c OP3 d) …]`, TIR-direct.
    ///
    /// Middle terms appear in two comparisons each, so they are bound to a
    /// `__mK` local inside a synthesised `TirExpr::Block` — `foo() < bar() <
    /// baz()` calls `bar()` exactly once.
    pub(super) fn desugar_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirBlock, TirStmt, TirStmtKind};

        if chain.comparisons.is_empty() {
            // Degenerate parse — no chain expansion fires, so this is not
            // a desugar site. Do not record `ComparisonChain` here.
            return self.resolve_expr(&chain.first, ctx, None);
        }

        // Single comparison: no middle term to bind, just one Binary.
        // Same reasoning: no chain expansion took place.
        if chain.comparisons.len() == 1 {
            let cmp = &chain.comparisons[0];
            let (left_tir, right_tir) = self.resolve_binary_operands_with_coercion(
                &chain.first,
                cmp.op,
                &cmp.right,
                ctx,
                None,
            );
            // Stage 5 (Gap 11 / WEP 2026-05-26): when the comparison
            // takes the operator-trait dispatch path inside
            // `build_binary_op_tir` (non-primitive operands → Eq /
            // Ord trait methods), tag the recording with the chain's
            // AstId so reify can replay the same method-call + Ord
            // wrap shape. Cleared on every path so a later
            // synthesised binary call doesn't pick up a stale id.
            self.pending_operator_ast_id = Some(chain.id);
            let result = self.build_binary_op_tir(left_tir, cmp.op, right_tir, cmp.op_span);
            self.pending_operator_ast_id = None;
            return result;
        }

        // Multi-comparison: actual chain expansion. Tag the node so the
        // future `reify` pass can replay the same `(a < b) && (b < c)`
        // shape with the same `__mK` middle bindings.
        self.record_desugar(chain.id, super::sem::types::DesugarKind::ComparisonChain);

        // Enter a fresh scope for the `__mK` bindings so they don't leak
        // into the surrounding function's local namespace.
        ctx.enter_scope();
        let mut stmts: Vec<TirStmt> = Vec::new();

        // First comparison: resolve `chain.first` and `cmp[0].right` with
        // the same bidirectional coercion `resolve_binary` would apply.
        let cmp0 = &chain.comparisons[0];
        let (first_tir, right0_tir) = self.resolve_binary_operands_with_coercion(
            &chain.first,
            cmp0.op,
            &cmp0.right,
            ctx,
            None,
        );

        // Bind `right0` to `__m0` — it is reused by the next comparison.
        let m0_ref = self.bind_chain_middle(0, right0_tir, chain.span, &mut stmts, ctx);
        let mut acc_tir =
            self.build_binary_op_tir(first_tir, cmp0.op, m0_ref.clone(), cmp0.op_span);
        let mut prev_tir = m0_ref;

        let last_idx = chain.comparisons.len() - 1;
        for idx in 1..chain.comparisons.len() {
            let cmp = &chain.comparisons[idx];
            // Coerce a literal `right` against `prev_tir`'s type; non-literals
            // resolve normally (`resolve_expr` ignores `expected_type` when it
            // can't help).
            let raw_right = self.resolve_expr(&cmp.right, ctx, Some(prev_tir.type_id));
            let right_tir = if idx == last_idx {
                // Tail operand: only one use, no binding needed.
                raw_right
            } else {
                self.bind_chain_middle(idx, raw_right, chain.span, &mut stmts, ctx)
            };
            let next_prev = right_tir.clone();
            let cmp_tir = self.build_binary_op_tir(prev_tir, cmp.op, right_tir, cmp.op_span);
            acc_tir = self.build_binary_op_tir(acc_tir, BinaryOp::And, cmp_tir, chain.span);
            prev_tir = next_prev;
        }

        ctx.exit_scope();

        // Wrap the bindings + final && chain in a TIR Block whose value is
        // the boolean result.
        stmts.push(TirStmt::new(TirStmtKind::Expr(acc_tir), chain.span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, chain.span)),
            TypeTable::BOOL,
            chain.span,
        )
    }

    /// Helper: bind a comparison-chain middle term to a `__mK` local and
    /// return a `TirExpr::Local` handle that callers can splice into the
    /// surrounding comparisons.
    fn bind_chain_middle(
        &mut self,
        idx: usize,
        value: TirExpr,
        span: Span,
        stmts: &mut Vec<crate::tir::TirStmt>,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirStmt, TirStmtKind};
        let type_id = value.type_id;
        let name = format!("__m{idx}");
        let local_index = ctx.add_local(name.clone(), type_id, false, None);
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.clone(),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: false,
            },
            span,
        ));
        TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name,
            },
            type_id,
            span,
        )
    }

    /// Single TIR-level builder for every operator that dispatches to a
    /// trait method — unary (`Neg::neg`, `BitNot::bitnot`) as well as
    /// binary (`Eq::eq`, `Ord::cmp`, `Add::add`, …, `Shl::shl`).  Inputs
    /// are already-resolved [`TirExpr`]s: the receiver and zero or more
    /// argument operands matching `resolved.param_types` in order.
    ///
    /// The builder is the single source of truth for:
    ///
    /// 1. Type-checking each argument against the trait's declared
    ///    parameter type.  Mismatches emit `TypeMismatch` and return an
    ///    `ERROR` expression; there is no way for a caller to skip the
    ///    check.  For `&Self` parameters the expected value type is the
    ///    receiver's own type so newtype dispatch (base-impl method
    ///    called through the newtype) still accepts the newtype on both
    ///    sides.
    /// 2. Adjusting the receiver via
    ///    [`Self::adjust_receiver_for_self_kind`].
    /// 3. Wrapping each argument in `&` iff the matching parameter type
    ///    is a reference — never otherwise, so `Shl::shl(&self, rhs:
    ///    u32)` receives the `u32` by value.
    /// 4. Constructing the [`TirExprKind::MethodCall`] with the correct
    ///    mangled name and `resolved.return_type`.
    fn build_trait_op_method_call_on_resolved(
        &mut self,
        receiver: TirExpr,
        args: Vec<TirExpr>,
        resolved: &ResolvedTraitMethod,
        span: Span,
    ) -> TirExpr {
        if args.len() != resolved.param_types.len() {
            // This is an internal invariant violation by the caller — the
            // resolve-site should always line up operand count with the
            // trait's arity.  Return ERROR so the rest of resolve recovers
            // gracefully, but flag it as a mismatch too so tests notice.
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: format!("{} arg(s)", resolved.param_types.len()),
                found: format!("{} arg(s)", args.len()),
                span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
        }

        // For each argument, decide whether the trait parameter is by
        // reference, compute the logical expected value type for the arg,
        // and delegate to `Elaborator::typecheck` — the same helper that
        // `resolve_method_call` uses at its own argument-typecheck tail.
        //
        // Using the shared primitive means operator dispatch and direct
        // method calls apply identical `check_assignable` rules
        // (Unknown/Error deferral, newtype propagation, reference
        // unwrapping) and any divergence there is impossible by
        // construction.  We no longer early-return on mismatch; errors
        // are accumulated via the logger and compilation fails at the
        // end, matching method-call behavior.
        let mut wrap_flags: Vec<bool> = Vec::with_capacity(args.len());
        for (arg, &param_ty) in args.iter().zip(resolved.param_types.iter()) {
            let wrap = matches!(
                self.tysys.type_table.borrow().get(param_ty),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            );
            // For `&Self` parameters the "value-level" expected type is
            // the receiver's type (preserving newtype identity when an
            // impl on the base is dispatched through a newtype receiver).
            // For concrete parameter types (e.g. `rhs: u32` on
            // `Shl::shl`) the expected type is the parameter type itself.
            let expected = if wrap { receiver.type_id } else { param_ty };
            self.typecheck(arg.type_id, expected, span);
            wrap_flags.push(wrap);
        }

        let receiver =
            self.adjust_receiver_for_self_kind(receiver, resolved.self_kind, false, span);

        let call_args: Vec<CallArg> = args
            .into_iter()
            .zip(wrap_flags.iter().copied())
            .map(|(arg, wrap)| {
                let arg_expr = if wrap {
                    let arg_ref_type = self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(arg.type_id));
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(arg),
                        },
                        arg_ref_type,
                        span,
                    )
                } else {
                    arg
                };
                CallArg::new(arg_expr, false)
            })
            .collect();

        let mangled_method_name = MethodName::format_local(
            &resolved.impl_name,
            Some(&resolved.trait_name),
            &resolved.method_name,
        );

        let mut method_info = LocalMethodName::new(
            resolved.impl_name.clone(),
            Some(resolved.trait_name.clone()),
            resolved.method_name.clone(),
        );
        method_info.is_type_param_receiver = resolved.is_type_param_receiver;

        let function_ref = FunctionRef {
            module_source: self.find_struct_module_source(&resolved.impl_name),
            name: mangled_method_name,
            monomorph_info: None,
            method_info: Some(method_info),
        };

        // Stage 5 (Gap 11 of WEP 2026-05-26): when the operator-dispatch
        // request carries a source AST id on the
        // [`Self::pending_operator_ast_id`] side-channel, record the
        // dispatch decision so reify can re-emit the same `MethodCall`
        // TIR for the binary / index expression. Synthesised callers
        // (e.g. `desugar_comparison_chain`'s inner comparisons) leave
        // the channel `None` and the record is skipped — they have no
        // source-level `BinaryExpr` reify would key on.
        if let Some(ast_id) = self.pending_operator_ast_id.take() {
            self.record_operator_dispatch(
                ast_id,
                super::sem::types::OperatorDispatch {
                    function_ref: function_ref.clone(),
                    self_kind: resolved.self_kind,
                    arg_ref_wraps: wrap_flags,
                    return_type: resolved.return_type,
                    needs_deref: false,
                },
            );
        }

        Self::build_tir_method_call(
            receiver,
            function_ref,
            vec![],
            call_args,
            resolved.return_type,
            span,
        )
    }

    /// Wrap an `Ord::cmp` method call into a `bool` by comparing the returned
    /// `Ordering` variant against the one that makes the operator true.
    /// `<`   → `cmp == Less`, `>` → `cmp == Greater`,
    /// `<=`  → `cmp != Greater`, `>=` → `cmp != Less`.
    fn ord_bool_from_cmp(&mut self, cmp_call: TirExpr, op: BinaryOp, span: Span) -> TirExpr {
        ord_bool_from_cmp(cmp_call, op, span, &self.tysys.type_table)
    }
}

/// Free-function form of [`Elaborator::ord_bool_from_cmp`] so the reify
/// pass produces an identical `Ordering`-comparison wrapper without an
/// `Elaborator`. `<` → `cmp == Less`, `>` → `cmp == Greater`,
/// `<=` → `cmp != Greater`, `>=` → `cmp != Less`.
pub(super) fn ord_bool_from_cmp(
    cmp_call: TirExpr,
    op: BinaryOp,
    span: Span,
    type_table: &std::cell::RefCell<TypeTable>,
) -> TirExpr {
    let ordering_type_id = type_table
        .borrow_mut()
        .make_compiler_enum(CompilerItem::Ordering);
    // Look up Ordering's `Less` / `Greater` cases through the
    // `CompilerItem` registry so a stdlib rename of either case
    // flows here without touching the operator-lowering path.
    let (less_name, less_index, greater_name, greater_index) = {
        let tt = type_table.borrow();
        let items = tt.compiler_items();
        let (_, _, less_name, less_index) = items.require_enum_case(CompilerItem::OrderingLess);
        let (_, _, greater_name, greater_index) =
            items.require_enum_case(CompilerItem::OrderingGreater);
        (
            less_name.to_string(),
            less_index,
            greater_name.to_string(),
            greater_index,
        )
    };
    let (compare_op, case_name, case_index): (TirBinaryOp, String, u32) = match op {
        BinaryOp::Lt => (TirBinaryOp::Eq, less_name, less_index),
        BinaryOp::Gt => (TirBinaryOp::Eq, greater_name, greater_index),
        BinaryOp::LtEq => (TirBinaryOp::NotEq, greater_name, greater_index),
        BinaryOp::GtEq => (TirBinaryOp::NotEq, less_name, less_index),
        _ => unreachable!(),
    };
    let ordering_variant = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type_id,
            case_name,
            case_index,
        },
        ordering_type_id,
        span,
    );
    TirExpr::new(
        TirExprKind::Binary {
            op: compare_op,
            left: Box::new(cmp_call),
            right: Box::new(ordering_variant),
        },
        TypeTable::BOOL,
        span,
    )
}
