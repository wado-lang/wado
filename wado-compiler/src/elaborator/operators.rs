//! Binary and unary operator resolution, including operator overloading.

use crate::ast::{self, BinaryOp, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::name::{LocalMethodName, MethodName};
use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TypeId, TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::types::{FunctionContext, ResolvedTraitMethod, TypeError};

use super::util::placeholder;

/// The right-hand side of an assignment passed to
/// [`Elaborator::assign_to_target`]. Either an AST expression (the
/// regular [`Elaborator::resolve_assign`] path) or an already-resolved
/// type (the [`Elaborator::resolve_compound_assign`] path, where the RHS
/// is `target op rhs` whose result type is computed via
/// [`Elaborator::build_binary_op_tir`]).
///
/// The combined walk only needs the resolved type and span: reify rebuilds
/// the actual compound-assign TIR from the AST.
pub(super) enum AssignValue<'a> {
    Ast(&'a ast::Expr),
    Resolved { type_id: TypeId, span: Span },
}

impl AssignValue<'_> {
    fn span(&self) -> Span {
        match self {
            Self::Ast(expr) => expr.span(),
            Self::Resolved { span, .. } => *span,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
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
            let coerce_type = if self.tysys.type_table.borrow().is_numeric(right) {
                // Primitive type: coerce literal to the same type
                Some(right)
            } else {
                // Struct type: look up operator trait and use self type for lhs literal
                self.find_operator_self_type(right, &op)
            };
            let left = self.resolve_expr(left_ast, ctx, coerce_type);
            (
                placeholder(left, left_ast.span()),
                placeholder(right, right_ast.span()),
            )
        } else if right_is_numeric_literal && !left_is_numeric_literal {
            // Resolve left first, then coerce right
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let coerce_type = if self.tysys.type_table.borrow().is_numeric(left) {
                // Primitive type: coerce literal to the same type
                Some(left)
            } else {
                // Struct type: look up operator trait and use rhs parameter type
                self.find_operator_rhs_type(left, &op)
            };
            let right = self.resolve_expr(right_ast, ctx, coerce_type);
            (
                placeholder(left, left_ast.span()),
                placeholder(right, right_ast.span()),
            )
        } else if left_is_numeric_literal && right_is_numeric_literal {
            // Both literals - use expected type from context (e.g., assignment target)
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            (
                placeholder(left, left_ast.span()),
                placeholder(right, right_ast.span()),
            )
        } else if self.tysys.is_null_literal(right_ast) && !self.tysys.is_null_literal(left_ast) {
            // `expr == null`: resolve the non-null side first and feed its
            // type to the bare `null` so it resolves to a concrete
            // `Option<T>` instead of `Option<UNKNOWN>`.
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let right = self.resolve_expr(right_ast, ctx, Some(left));
            (
                placeholder(left, left_ast.span()),
                placeholder(right, right_ast.span()),
            )
        } else if self.tysys.is_null_literal(left_ast) && !self.tysys.is_null_literal(right_ast) {
            // `null == expr`: symmetric to the above.
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            let left = self.resolve_expr(left_ast, ctx, Some(right));
            (
                placeholder(left, left_ast.span()),
                placeholder(right, right_ast.span()),
            )
        } else {
            // Both non-literals - propagate expected type for coercion
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            (
                placeholder(left, left_ast.span()),
                placeholder(right, right_ast.span()),
            )
        }
    }

    /// True when an operand type has no native Wasm binary-op instruction
    /// and must therefore dispatch through a trait implementation. Used to
    /// detect operator misuse symmetrically: either operand being such a
    /// type means the operator cannot fall through to a primitive
    /// instruction.
    fn binop_operand_requires_trait(&self, type_id: TypeId) -> bool {
        let tt = self.tysys.type_table.borrow();
        match tt.get(type_id) {
            ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
            ResolvedType::Newtype { base_type, .. } => {
                let ultimate = tt.get_ultimate_base_type(*base_type);
                matches!(
                    tt.get(ultimate),
                    ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
                )
            }
            // Types with no native Wasm binary-op support and no prelude
            // trait impl. Without rejection, codegen emits `ref.eq` /
            // `i32.eq` against a GC reference and fails validation.
            ResolvedType::Function { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Reactive(_)
            | ResolvedType::AssocTypeProjection { .. } => true,
            _ => false,
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
    ) -> TypeId {
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
                // Stage 7-B: reify rebuilds the reference `==` / `!=`
                // (`RefEq` / `RefNotEq`) from the AST; project only the type.
                return TypeTable::BOOL;
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
                let _ = self.logger.error(TypeError::OperatorNotApplicable {
                    op: op_str.to_string(),
                    operands: vec![type_name],
                    note: None,
                    span,
                });
                return TypeTable::ERROR;
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
                        let _ = self.logger.error(TypeError::OperatorNotApplicable {
                            op: op_str.to_string(),
                            operands: vec![type_name],
                            note: Some("type does not implement `Eq`".to_string()),
                            span,
                        });
                        return TypeTable::ERROR;
                    };
                    let call = self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                    if op == BinaryOp::NotEq && call.type_id == TypeTable::BOOL {
                        // reify rebuilds the `!` wrapper for `!=`; project BOOL.
                        return TypeTable::BOOL;
                    }
                    return call.type_id;
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
                        let _ = self.logger.error(TypeError::OperatorNotApplicable {
                            op: op_str.to_string(),
                            operands: vec![type_name],
                            note: Some("type does not implement `Ord`".to_string()),
                            span,
                        });
                        return TypeTable::ERROR;
                    };
                    let cmp_call = self.build_trait_op_method_call_on_resolved(
                        left,
                        vec![right],
                        &resolved,
                        span,
                    );
                    if cmp_call.type_id == TypeTable::ERROR {
                        return TypeTable::ERROR;
                    }
                    return self.ord_bool_from_cmp(cmp_call, op, span).type_id;
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
                        // reify rebuilds the `!` wrapper for `!=`; project BOOL.
                        return TypeTable::BOOL;
                    }
                    return call.type_id;
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
                        return TypeTable::ERROR;
                    }
                    return self.ord_bool_from_cmp(cmp_call, op, span).type_id;
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
                    return self
                        .build_trait_op_method_call_on_resolved(left, vec![right], &resolved, span)
                        .type_id;
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
                    return self
                        .build_trait_op_method_call_on_resolved(left, vec![right], &resolved, span)
                        .type_id;
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
                    return self
                        .build_trait_op_method_call_on_resolved(left, vec![right], &resolved, span)
                        .type_id;
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
                let type_name = self.tysys.type_table.borrow().type_name(left.type_id);
                let _ = self.logger.error(TypeError::OperatorNotApplicable {
                    op: op_char.to_string(),
                    operands: vec![type_name],
                    note: Some(
                        "flags types support only bitwise operators (`|`, `&`, `^`)".to_string(),
                    ),
                    span,
                });
                return TypeTable::ERROR;
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
                let _ = self.logger.error(TypeError::OperatorNotApplicable {
                    op: "%".to_string(),
                    operands: vec![type_name.to_string()],
                    note: Some(format!("use `{type_name}::fmod(a, b)` instead")),
                    span,
                });
                return TypeTable::ERROR;
            }
        }

        // If either operand is a non-primitive type that cannot fall through
        // to a native Wasm binary instruction, the operator must dispatch
        // through a trait implementation. Reaching here means no matching
        // impl was found — emit a diagnostic before we accidentally build a
        // TIR `Binary` node that would crash codegen with "type mismatch:
        // expected ... found (ref $type)" on Wasm validation (see regression
        // fixture `typecheck_binop_fn_eq.wado`). Checking both operands keeps
        // the message symmetric: `a - lst` and `lst - a` report the same
        // error regardless of which side is the non-primitive one.
        {
            let left_requires = self.binop_operand_requires_trait(left.type_id);
            let right_requires = self.binop_operand_requires_trait(right.type_id);
            if (left_requires || right_requires)
                && !matches!(op, BinaryOp::And | BinaryOp::Or)
                && left.type_id != TypeTable::ERROR
                && right.type_id != TypeTable::ERROR
            {
                let type_table = self.tysys.type_table.borrow();
                let left_name = type_table.type_name(left.type_id);
                let right_name = type_table.type_name(right.type_id);
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
                drop(type_table);
                if left_name == right_name {
                    // Both operands share a type that lacks the operator's
                    // trait impl (e.g. `Point - Point` with no `Sub`).
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
                        BinaryOp::Eq | BinaryOp::NotEq => self
                            .tysys
                            .type_table
                            .borrow()
                            .compiler_items()
                            .trait_name(CompilerItem::Eq)
                            .to_string(),
                        BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => self
                            .tysys
                            .type_table
                            .borrow()
                            .compiler_items()
                            .trait_name(CompilerItem::Ord)
                            .to_string(),
                        _ => "?".to_string(),
                    };
                    let _ = self.logger.error(TypeError::OperatorNotApplicable {
                        op: op_char.to_string(),
                        operands: vec![left_name],
                        note: Some(format!("type does not implement `{trait_name}`")),
                        span,
                    });
                } else {
                    // Mixed operand types that cannot combine under this
                    // operator (e.g. `i32 - List<i32>`). Reported the same
                    // way regardless of which operand is non-primitive.
                    let _ = self.logger.error(TypeError::OperatorNotApplicable {
                        op: op_char.to_string(),
                        operands: vec![left_name, right_name],
                        note: None,
                        span,
                    });
                }
                return TypeTable::ERROR;
            }
        }

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

        // Stage 7-B: reify rebuilds the native `Binary` from the AST +
        // recorded operand `expression_types`; the combined walk projects
        // only the result type. `left` / `right` were resolved by the
        // caller and typechecked above for their side effects.
        type_id
    }

    /// Resolve a unary expression
    pub(super) fn resolve_unary(
        &mut self,
        unary: &ast::UnaryExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
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
        let expr_type = self.resolve_expr(&unary.expr, ctx, inner_expected);

        // Address-taken-local tracking for `&x` / `&mut x` is owned by reify
        // (`reify.rs` unary arm), which marks `TirFunction::address_taken_locals`
        // on the TIR it actually emits; the combined walk's marking was dead.
        // The `&mut`-on-immutable-local diagnostic stays here (annotate emits
        // diagnostics once) and reads the target off the AST now that
        // `resolve_ident` returns a placeholder: `&mut x` is an immutable-local
        // error only when `x` is a function-frame local (a `Local` `kind`
        // before 7-B), so classify it via a read-only `ctx.lookup`.
        if unary.op == UnaryOp::MutRef
            && let ast::Expr::Ident(id) = &unary.expr
            && let Some(local) = ctx.lookup(&id.name)
            && !local.is_mut
        {
            let _ = self.logger.error(TypeError::CannotAssign {
                message: format!("cannot take &mut of immutable variable '{}'", id.name),
                span: unary.span,
            });
        }

        // Reject &mut on struct field access when the field is a primitive type.
        // In Wasm GC, struct.get returns a value copy for primitives, so &mut field
        // creates a disconnected Box — mutations don't propagate back to the struct.
        // For GC reference types (struct, String, List, etc.), struct.get returns
        // the shared reference, so &mut field works correctly.
        // Detect the field-access shape from the AST (Stage 7-B:
        // `resolve_field_access` returns a placeholder, so its resolved
        // `kind` is no longer `FieldAccess`); the operand's `type_id` still
        // carries the field type via the placeholder.
        if unary.op == UnaryOp::MutRef && matches!(&unary.expr, ast::Expr::FieldAccess(_)) {
            let field_type = self.tysys.type_table.borrow().get(expr_type).clone();
            let base_type = self
                .tysys
                .type_table
                .borrow()
                .get(
                    self.tysys
                        .type_table
                        .borrow()
                        .get_ultimate_base_type(expr_type),
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
            let operand_resolved = self.tysys.type_table.borrow().get(expr_type).clone();
            let struct_name = match &operand_resolved {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => {
                    Some(name.clone())
                }
                _ => None,
            };
            if let Some(struct_name) = struct_name {
                let (lookup_name, lookup_type_id) =
                    self.newtype_base_lookup(&struct_name, expr_type);
                let resolved = self
                    .resolve_trait_method_for_op(
                        &struct_name,
                        expr_type,
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
                    return self
                        .build_trait_op_method_call_on_resolved(
                            placeholder(expr_type, unary.expr.span()),
                            vec![],
                            &resolved,
                            unary.span,
                        )
                        .type_id;
                }
            }
        }

        let type_id = match unary.op {
            UnaryOp::Not => TypeTable::BOOL,
            UnaryOp::Ref => self.tysys.type_table.borrow_mut().make_ref(expr_type),
            UnaryOp::MutRef => self.tysys.type_table.borrow_mut().make_mut_ref(expr_type),
            UnaryOp::Deref => {
                let outer = self.tysys.type_table.borrow().get(expr_type).clone();
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
                            found: self.tysys.type_table.borrow().type_name(expr_type),
                            span: unary.span,
                        });
                        TypeTable::ERROR
                    }
                }
            }
            _ => expr_type,
        };

        // Stage 7-B: reify rebuilds the `Unary` (`*ptr` / `&x` / `&mut x` /
        // `!b`) and folds `-literal` from the AST; the `&mut`-field
        // diagnostic and the deref/ref type computation above are the
        // record-only work. `assign_to_target`'s deref l-value check now
        // reads the operand type from `expression_types` instead of this
        // resolved `kind`.
        type_id
    }

    /// Resolve an assignment expression.
    pub(super) fn resolve_assign(
        &mut self,
        assign: &ast::AssignExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        self.assign_to_target(&assign.target, AssignValue::Ast(&assign.value), ctx)
    }

    /// Whether an `Index` assignment target reaching `assign_to_target`'s
    /// general path is an assignable place. The `IndexAssign` path above
    /// already returned for index-assignable receivers, so AST + recorded
    /// facts replace reading the resolved `target.kind`:
    ///   - a read-only `Index` trait access (recorded `operator_dispatch`
    ///     with `needs_deref`) lowers to `*recv.index(i)` over `&Output`, so
    ///     a write would go through an immutable reference (diagnosed here,
    ///     mirroring the old `Unary { Deref, expr: Ref(_) }` arm);
    ///   - an `IndexValue` access is a by-value method call — not a place;
    ///   - with no recorded read dispatch the only assignable shape is a
    ///     tuple index (`t[0]`, lowered to a `FieldAccess`); an unindexable
    ///     receiver was already diagnosed by `resolve_index`.
    fn index_target_assignable(&mut self, index_expr: &ast::IndexExpr) -> bool {
        let needs_deref = self
            .sem
            .types
            .operator_dispatch
            .get(&self.ann_key(index_expr.id))
            .map(|d| d.needs_deref);
        if let Some(needs_deref) = needs_deref {
            if needs_deref {
                let _ = self.logger.error(TypeError::CannotAssign {
                    message: "cannot assign through immutable reference".to_string(),
                    span: index_expr.span,
                });
            }
            return false;
        }
        let Some(recv_type) = self
            .sem
            .types
            .expression_types
            .get(&self.ann_key(index_expr.expr.id()))
            .copied()
        else {
            return false;
        };
        let table = self.tysys.type_table.borrow();
        // Mirror `resolve_index`'s one-level reference peel before the tuple
        // check.
        let base = match table.get(recv_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => recv_type,
        };
        matches!(
            table.get(base),
            ResolvedType::GenericInstance { name, module_source, .. }
                if TypeTable::is_tuple_type(name, module_source)
        )
    }

    /// Whether `*operand = v` is a valid l-value: it is, unless `operand`
    /// is an immutable reference (`&T`), in which case the write is through
    /// an immutable reference and is diagnosed here. AST + recorded-fact
    /// replacement for reading the resolved `Unary { Deref, expr }` kind
    /// (Stage 7-B made `resolve_unary` a placeholder): `operand`'s type
    /// rides on `expression_types`. Mirrors the old check, which inspected
    /// the *operand* type, so `*5 = v` (operand `5: i32`, already diagnosed
    /// by `resolve_unary`) is not re-flagged here.
    fn deref_target_assignable(&mut self, unary: &ast::UnaryExpr, span: Span) -> bool {
        let Some(operand_type) = self
            .sem
            .types
            .expression_types
            .get(&self.ann_key(unary.expr.id()))
            .copied()
        else {
            // ERROR-typed operand (not recorded): lenient, like the old
            // `get(ERROR)` arm that did not match `Ref`.
            return true;
        };
        let is_immutable_ref = matches!(
            self.tysys.type_table.borrow().get(operand_type),
            ResolvedType::Ref(_)
        );
        if is_immutable_ref {
            let _ = self.logger.error(TypeError::CannotAssign {
                message: "cannot assign through immutable reference".to_string(),
                span,
            });
            false
        } else {
            true
        }
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
        ctx: &mut FunctionContext,
    ) -> TypeId {
        // Check for index assignment on custom types: arr[i] = value -> arr.index_assign(i, value)
        if let ast::Expr::Index(index_expr) = target_ast {
            // Resolve the indexed expression to get its type
            let indexed_type = self.resolve_expr(&index_expr.expr, ctx, None);

            // Get base type (unwrap reference if needed)
            let base_type_id = match self.tysys.type_table.borrow().get(indexed_type) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                _ => indexed_type,
            };

            // Check for IndexAssign trait implementation
            // Arrays now use IndexAssign trait like other types
            {
                // Check for IndexAssign trait implementation
                let struct_name = match self.tysys.type_table.borrow().get(base_type_id).clone() {
                    ResolvedType::Struct { name, .. } => name,
                    ResolvedType::GenericInstance { name, .. } => name,
                    ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => name,
                    // `arr[i] = v` dispatches through `impl IndexAssign for Array<T>`,
                    // keyed by the base name "Array".
                    ResolvedType::BuiltinArray(_) => TypeTable::ARRAY_TYPE_NAME.to_string(),
                    _ => String::new(),
                };

                // For newtypes, resolve the base type name for trait impl lookup
                let (lookup_name, lookup_type_id) =
                    self.newtype_base_lookup(&struct_name, base_type_id);

                if !struct_name.is_empty() {
                    let index_type = self.resolve_expr(&index_expr.index, ctx, None);

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
                        let value_type = match value {
                            AssignValue::Ast(expr) => {
                                self.resolve_expr(expr, ctx, Some(trait_info.input_type))
                            }
                            AssignValue::Resolved { type_id, .. } => type_id,
                        };

                        // Check: reject &T/&mut T assigned where non-ref expected
                        self.typecheck(value_type, trait_info.input_type, value_span);

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
                                function_ref: func,
                                self_kind: trait_info.self_kind,
                                arg_ref_wraps: vec![false, false],
                                return_type: TypeTable::UNIT,
                                needs_deref: false,
                            },
                        );

                        // Stage 7-B: reify rebuilds `arr.index_assign(idx,
                        // value)` from the recorded `index_assign_dispatch` +
                        // AST; project only the (unit) result type. `index_type`
                        // / `value_type` were resolved above for their
                        // fact-recording side effects.
                        return TypeTable::UNIT;
                    }
                }
            }
        }

        // Standard assignment handling. Fall through here happens when the
        // target isn't `Expr::Index`, or when the IndexAssign trait lookup
        // returned None — `value` was not consumed on either of those paths.
        let target_type = self.resolve_expr(target_ast, ctx, None);
        let value_span = value.span();
        let value_type = match value {
            AssignValue::Ast(expr) => self.resolve_expr(expr, ctx, Some(target_type)),
            AssignValue::Resolved { type_id, .. } => type_id,
        };

        // Reject &T assigned where non-ref T expected
        self.typecheck(value_type, target_type, value_span);

        // Handle assignment to global variables. Stage 7-B: the target is a
        // placeholder, so the global is recognised from the recorded
        // `AssignPlace::Global` on the target ident (which carries the
        // resolved mutability) rather than the resolved `GlobalVarGet` kind.
        let global_assign = if let ast::Expr::Ident(id) = target_ast {
            match self.assign_place_of(id.id) {
                Some(super::sem::types::AssignPlace::Global { name, mutable, .. }) => {
                    Some((name.clone(), *mutable))
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((name, is_mut)) = global_assign {
            if !is_mut {
                let _ = self.logger.error(TypeError::CannotAssign {
                    message: format!("cannot assign to immutable global variable '{name}'"),
                    span: target_ast.span(),
                });
                return TypeTable::ERROR;
            }
            // reify rebuilds the `GlobalVarSet` from the AST; project the
            // (unit) result type.
            return TypeTable::UNIT;
        }

        // Validate that the target is a valid l-value. Stage 7-B: every
        // resolver returns a placeholder, so each target shape is classified
        // from the AST + recorded facts.
        let is_valid_lvalue = match target_ast {
            // A field access is always a place.
            ast::Expr::FieldAccess(_) => true,
            // The IndexAssign path at the top of `assign_to_target` already
            // returned for index-assignable receivers, so an `Index` target
            // here is a tuple index (a place), a read-only `Index` /
            // `IndexValue` trait access (not a place), or an unindexable
            // receiver (already diagnosed by `resolve_index`).
            ast::Expr::Index(index_expr) => self.index_target_assignable(index_expr),
            // `*x = v`: valid only through a mutable reference. The operand's
            // type rides on `expression_types`.
            ast::Expr::Unary(u) if u.op == ast::UnaryOp::Deref => {
                self.deref_target_assignable(u, target_ast.span())
            }
            // An identifier target classified by the place fact `resolve_ident`
            // recorded: a function-frame `Local`, or a `&mut`-captured ident
            // (`*__ref` deref-capture, assignable iff the captured ref is
            // `&mut`). Globals returned above; anything else (function /
            // variant / enum / const ident, or a non-ident expression) is not
            // a place.
            ast::Expr::Ident(id) => {
                // Clone to release the `&self` borrow before the diagnostic's
                // `&mut self.logger` use.
                match self.assign_place_of(id.id).cloned() {
                    Some(super::sem::types::AssignPlace::Local) => true,
                    Some(super::sem::types::AssignPlace::DerefCapture { through_mut_ref }) => {
                        if through_mut_ref {
                            true
                        } else {
                            let _ = self.logger.error(TypeError::CannotAssign {
                                message: "cannot assign through immutable reference".to_string(),
                                span: target_ast.span(),
                            });
                            false
                        }
                    }
                    _ => false,
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
            return TypeTable::ERROR;
        }

        // Stage 7-B: reify rebuilds the `Assign` from the AST + recorded
        // target / value types; project only the (unit) result type. The
        // target + value were resolved above for their side effects, and the
        // l-value validation / global-mutability checks already ran.
        TypeTable::UNIT
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
    ) -> TypeId {
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
        let read_type = self.resolve_expr(&compound.target, ctx, None);
        let rhs_type = self.resolve_expr(&compound.value, ctx, Some(read_type));
        let read = placeholder(read_type, compound.target.span());
        let rhs = placeholder(rhs_type, compound.value.span());
        // Stage 5 (Gap 11 / WEP 2026-05-26): when the operands are
        // operator-overloaded (non-primitive, e.g. `u128 /= u128`),
        // `build_binary_op_tir` dispatches the combined value through the
        // trait method (`Div::div` → `div_rem`). Tag the record with the
        // compound's AstId so reify replays that MethodCall instead of a raw
        // `Binary` (a primitive `/` on struct operands is invalid Wasm).
        // Cleared unconditionally so a primitive op — which never reaches the
        // dispatch-record path — leaves no stale id behind.
        self.pending_operator_ast_id = Some(compound.id);
        let combined = self.build_binary_op_tir(read, op, rhs, compound.span);
        self.pending_operator_ast_id = None;
        self.assign_to_target(
            &compound.target,
            AssignValue::Resolved {
                type_id: combined,
                span: compound.span,
            },
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
    ) -> TypeId {
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
        let m0_ref = self.bind_chain_middle(0, right0_tir, ctx);
        let acc_tir = self.build_binary_op_tir(first_tir, cmp0.op, m0_ref.clone(), cmp0.op_span);
        let mut acc_tir = placeholder(acc_tir, chain.span);
        let mut prev_tir = m0_ref;

        let last_idx = chain.comparisons.len() - 1;
        for idx in 1..chain.comparisons.len() {
            let cmp = &chain.comparisons[idx];
            // Coerce a literal `right` against `prev_tir`'s type; non-literals
            // resolve normally (`resolve_expr` ignores `expected_type` when it
            // can't help).
            let raw_right_type = self.resolve_expr(&cmp.right, ctx, Some(prev_tir.type_id));
            let raw_right = placeholder(raw_right_type, cmp.right.span());
            let right_tir = if idx == last_idx {
                // Tail operand: only one use, no binding needed.
                raw_right
            } else {
                self.bind_chain_middle(idx, raw_right, ctx)
            };
            let next_prev = right_tir.clone();
            let cmp_tir = self.build_binary_op_tir(prev_tir, cmp.op, right_tir, cmp.op_span);
            let cmp_tir = placeholder(cmp_tir, cmp.op_span);
            let acc_type = self.build_binary_op_tir(acc_tir, BinaryOp::And, cmp_tir, chain.span);
            acc_tir = placeholder(acc_type, chain.span);
            prev_tir = next_prev;
        }

        ctx.exit_scope();

        // Stage 7-B: reify rebuilds the `(a<b) && (b<c) …` Block (with the
        // `__mK` middle bindings) from the recorded `ComparisonChain`
        // desugar + the AST; the combined walk projects only the boolean
        // result type. The operand resolutions, middle-binding local
        // allocations, and per-comparison dispatch above ran for their
        // fact-recording side effects.
        TypeTable::BOOL
    }

    /// Helper: allocate a `__mK` local for a comparison-chain middle term and
    /// return a `TirExpr::Local` handle that callers splice into the
    /// surrounding comparisons. The `Let` binding itself is rebuilt by reify;
    /// the combined walk only needs the `add_local` side effect (walk-order
    /// parity) and the local handle's type.
    fn bind_chain_middle(
        &mut self,
        idx: usize,
        value: TirExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let type_id = value.type_id;
        let span = value.span;
        let name = format!("__m{idx}");
        let _local_index = ctx.add_local(name, type_id, false, None);
        placeholder(type_id, span)
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

        // Resolve the impl's module from the receiver's *actual* type, not its
        // bare name: same-named structs in different modules each have their own
        // operator impls (e.g. auto-derived `Eq`/`Ord`), and a by-name lookup
        // would route every call to whichever registered first. Peel newtypes to
        // the base that owns the inherited impl; fall back to the by-name lookup
        // when the receiver carries no declaring module.
        let receiver_base = self
            .tysys
            .type_table
            .borrow()
            .resolve_newtype_base(receiver.type_id);
        let module_source = self.type_decl_key(receiver_base).map_or_else(
            || self.find_struct_module_source(&resolved.impl_name),
            |(module, _)| module,
        );
        let function_ref = FunctionRef {
            module_source,
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
                    function_ref,
                    self_kind: resolved.self_kind,
                    arg_ref_wraps: wrap_flags,
                    return_type: resolved.return_type,
                    needs_deref: false,
                },
            );
        }

        // Stage 7-B: reify rebuilds the overloaded operator's `MethodCall`
        // from the recorded `operator_dispatch` (receiver adjustment via
        // `self_kind`, arg `&`-wrapping via `arg_ref_wraps`) + the AST; the
        // combined walk projects only the result type. `receiver` and
        // `args` were resolved / typechecked above for their side effects.
        placeholder(resolved.return_type, span)
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
