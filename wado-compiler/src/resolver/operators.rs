//! Binary and unary operator resolution, including operator overloading.

use crate::ast::{self, BinaryOp, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    CallArg, FunctionRef, PrimitiveType, ResolvedType, TirBinaryOp, TirExpr, TirExprKind,
    TirUnaryOp, TypeId, TypeTable,
};

use super::Resolver;
use super::types::{FunctionContext, TypeError};
use super::util;

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Bidirectional coercion: if one operand is a numeric literal and the other is not,
        // resolve the non-literal first and use its type to coerce the literal.
        //
        // For primitive numeric types: coerce the literal to the other operand's type.
        // For struct types (e.g., i128/u128): look up the operator trait to determine
        // the expected type for each operand from the method signature. This naturally
        // handles operators with asymmetric parameter types (e.g., Shl::shl(&self, rhs: u32))
        // without special-casing specific operators.
        let left_is_numeric_literal = self.is_numeric_literal(&binary.left);
        let right_is_numeric_literal = self.is_numeric_literal(&binary.right);

        let (left, right) = if left_is_numeric_literal && !right_is_numeric_literal {
            // Resolve right first, then coerce left
            let right = self.resolve_expr(&binary.right, ctx, expected_type);
            let coerce_type = if self.type_table.borrow().is_numeric(right.type_id) {
                // Primitive type: coerce literal to the same type
                Some(right.type_id)
            } else {
                // Struct type: look up operator trait and use self type for lhs literal
                self.find_operator_self_type(right.type_id, &binary.op)
            };
            let left = self.resolve_expr(&binary.left, ctx, coerce_type);
            (left, right)
        } else if right_is_numeric_literal && !left_is_numeric_literal {
            // Resolve left first, then coerce right
            let left = self.resolve_expr(&binary.left, ctx, expected_type);
            let coerce_type = if self.type_table.borrow().is_numeric(left.type_id) {
                // Primitive type: coerce literal to the same type
                Some(left.type_id)
            } else {
                // Struct type: look up operator trait and use rhs parameter type
                self.find_operator_rhs_type(left.type_id, &binary.op)
            };
            let right = self.resolve_expr(&binary.right, ctx, coerce_type);
            (left, right)
        } else if left_is_numeric_literal && right_is_numeric_literal {
            // Both literals - use expected type from context (e.g., assignment target)
            let left = self.resolve_expr(&binary.left, ctx, expected_type);
            let right = self.resolve_expr(&binary.right, ctx, expected_type);
            (left, right)
        } else {
            // Both non-literals - propagate expected type for coercion
            let left = self.resolve_expr(&binary.left, ctx, expected_type);
            let right = self.resolve_expr(&binary.right, ctx, expected_type);
            (left, right)
        };

        // Check if this is a comparison operation on a non-primitive type
        // Non-primitives use Eq/Ord traits instead of direct Wasm instructions
        let left_type = self.type_table.borrow().get(left.type_id).clone();
        let is_comparison = matches!(
            binary.op,
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
                    let tt = self.type_table.borrow();
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
                    let tt = self.type_table.borrow();
                    tt.get_newtype_base(left.type_id).unwrap_or(left.type_id)
                };

                // Handle Eq trait (== and !=)
                if matches!(binary.op, BinaryOp::Eq | BinaryOp::NotEq) {
                    let Some(trait_info) = self.find_eq_trait_impl(&struct_name, lookup_type_id)
                    else {
                        let type_name = self.type_table.borrow().type_name(left.type_id);
                        let op_str = if binary.op == BinaryOp::Eq {
                            "=="
                        } else {
                            "!="
                        };
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!(
                                "operator `{op_str}` cannot be applied to type `{type_name}` (does not implement Eq trait)"
                            ),
                            span: binary.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, binary.span);
                    };
                    {
                        let receiver = self.adjust_receiver_for_self_kind(
                            left,
                            trait_info.self_kind,
                            false,
                            binary.span,
                        );

                        let arg_ref_type = self
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(right.type_id));

                        let arg_ref = TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(right),
                            },
                            arg_ref_type,
                            binary.span,
                        );

                        let mangled_method_name = MethodName::format_local(
                            &struct_name,
                            Some(&trait_info.trait_name),
                            "eq",
                        );

                        let call_expr = TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(receiver),
                                func: FunctionRef {
                                    module_source: self.find_struct_module_source(&struct_name),
                                    name: mangled_method_name,
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        struct_name.clone(),
                                        Some(trait_info.trait_name),
                                        "eq".to_string(),
                                    )),
                                    is_cm_binding: false,
                                },
                                type_args: vec![],
                                args: vec![CallArg::new(arg_ref, false)],
                            },
                            TypeTable::BOOL,
                            binary.span,
                        );

                        // Apply negation for !=
                        if binary.op == BinaryOp::NotEq {
                            return TirExpr::new(
                                TirExprKind::Unary {
                                    op: TirUnaryOp::Not,
                                    expr: Box::new(call_expr),
                                },
                                TypeTable::BOOL,
                                binary.span,
                            );
                        }

                        return call_expr;
                    }
                }

                // Handle Ord trait (<, >, <=, >=)
                // Ord::cmp returns Ordering enum with discriminants: Less=0, Equal=1, Greater=2
                if matches!(
                    binary.op,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
                ) {
                    let Some(trait_info) = self.find_ord_trait_impl(&struct_name, lookup_type_id)
                    else {
                        let type_name = self.type_table.borrow().type_name(left.type_id);
                        let op_str = match binary.op {
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
                            span: binary.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, binary.span);
                    };
                    {
                        let receiver = self.adjust_receiver_for_self_kind(
                            left,
                            trait_info.self_kind,
                            false,
                            binary.span,
                        );

                        let arg_ref_type = self
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(right.type_id));

                        let arg_ref = TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(right),
                            },
                            arg_ref_type,
                            binary.span,
                        );

                        // Get Ordering type for cmp return value
                        let ordering_type_id =
                            self.type_table.borrow_mut().intern(ResolvedType::Enum {
                                name: "Ordering".to_string(),
                                module_source: ModuleSource::prelude(),
                            });

                        let mangled_method_name = MethodName::format_local(
                            &struct_name,
                            Some(&trait_info.trait_name),
                            "cmp",
                        );

                        let cmp_call = TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(receiver),
                                func: FunctionRef {
                                    module_source: self.find_struct_module_source(&struct_name),
                                    name: mangled_method_name,
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        struct_name.clone(),
                                        Some(trait_info.trait_name),
                                        "cmp".to_string(),
                                    )),
                                    is_cm_binding: false,
                                },
                                type_args: vec![],
                                args: vec![CallArg::new(arg_ref, false)],
                            },
                            ordering_type_id,
                            binary.span,
                        );

                        // Determine comparison operator and Ordering variant:
                        // < : cmp(a, b) == Ordering::Less
                        // > : cmp(a, b) == Ordering::Greater
                        // <= : cmp(a, b) != Ordering::Greater
                        // >= : cmp(a, b) != Ordering::Less
                        let (compare_op, case_name, case_index): (TirBinaryOp, &str, u32) =
                            match binary.op {
                                BinaryOp::Lt => (TirBinaryOp::Eq, "Less", 0),
                                BinaryOp::Gt => (TirBinaryOp::Eq, "Greater", 2),
                                BinaryOp::LtEq => (TirBinaryOp::NotEq, "Greater", 2),
                                BinaryOp::GtEq => (TirBinaryOp::NotEq, "Less", 0),
                                _ => unreachable!(),
                            };

                        // Create Ordering enum value for comparison
                        let ordering_variant = TirExpr::new(
                            TirExprKind::EnumConstruct {
                                enum_type: ordering_type_id,
                                case_name: case_name.to_string(),
                                case_index,
                            },
                            ordering_type_id,
                            binary.span,
                        );

                        return TirExpr::new(
                            TirExprKind::Binary {
                                op: compare_op,
                                left: Box::new(cmp_call),
                                right: Box::new(ordering_variant),
                            },
                            TypeTable::BOOL,
                            binary.span,
                        );
                    }
                }
            }
        }

        // Check if this is an arithmetic or bitwise operation on a non-primitive type
        // Non-primitives use Add/Sub/Mul/Div/Rem/BitAnd/BitOr/BitXor traits
        let is_arithmetic_or_bitwise = matches!(
            binary.op,
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
                let (trait_name, method_name) = match binary.op {
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
                    // Adjust receiver for self kind (&self)
                    let receiver =
                        self.adjust_receiver_for_self_kind(left, trait_info.self_kind, false, binary.span);

                    // Create reference type for the argument (rhs: &Self)
                    let arg_ref_type = self
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(right.type_id));

                    let arg_ref = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(right),
                        },
                        arg_ref_type,
                        binary.span,
                    );

                    let mangled_method_name = MethodName::format_local(
                        &impl_name,
                        Some(&trait_info.trait_name),
                        method_name,
                    );

                    return TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef {
                                module_source: self.find_struct_module_source(&impl_name),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    impl_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    method_name.to_string(),
                                )),
                                is_cm_binding: false,
                            },
                            type_args: vec![],
                            args: vec![CallArg::new(arg_ref, false)],
                        },
                        trait_info.output_type,
                        binary.span,
                    );
                }
            }
        }

        // Check if this is a shift operation on a non-primitive type
        // Non-primitives use Shl/Shr traits (with rhs: u32, not &Self)
        let is_shift = matches!(binary.op, BinaryOp::Shl | BinaryOp::Shr);

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
                let (trait_name, method_name) = match binary.op {
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
                    // Adjust receiver for self kind (&self)
                    let receiver =
                        self.adjust_receiver_for_self_kind(left, trait_info.self_kind, false, binary.span);

                    // For shift operations, rhs is u32 (not &Self), so pass directly
                    let mangled_method_name = MethodName::format_local(
                        &impl_name,
                        Some(&trait_info.trait_name),
                        method_name,
                    );

                    return TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef {
                                module_source: self.find_struct_module_source(&impl_name),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    impl_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    method_name.to_string(),
                                )),
                                is_cm_binding: false,
                            },
                            type_args: vec![],
                            args: vec![CallArg::new(right, false)], // Pass rhs directly (u32)
                        },
                        trait_info.output_type,
                        binary.span,
                    );
                }
            }
        }

        // Check for arithmetic on flags types: only bitwise ops are allowed
        {
            let left_resolved = self.type_table.borrow().get(left.type_id).clone();
            let is_flags_arith = matches!(
                binary.op,
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod
            ) && matches!(left_resolved, ResolvedType::Flags { .. });
            if is_flags_arith {
                let op_char = match binary.op {
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
                    span: binary.span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, binary.span);
            }
        }

        // Check for modulo on float types: Wasm has no float remainder instruction.
        // Users should use f64::fmod() or f32::fmod() instead.
        {
            let left_resolved = self.type_table.borrow().get(left.type_id).clone();
            if matches!(binary.op, BinaryOp::Mod)
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
                    span: binary.span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, binary.span);
            }
        }

        // If the left operand is a non-primitive type (Struct, GenericInstance, or a
        // Newtype whose ultimate base is non-primitive), we should have dispatched via
        // a trait above.  Reaching here means the required trait is not implemented.
        {
            let requires_trait = match &left_type {
                ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
                ResolvedType::Newtype { base_type, .. } => {
                    let tt = self.type_table.borrow();
                    let ultimate = tt.get_ultimate_base_type(*base_type);
                    matches!(
                        tt.get(ultimate),
                        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
                    )
                }
                _ => false,
            };
            if requires_trait
                && !matches!(binary.op, BinaryOp::And | BinaryOp::Or)
                && left.type_id != TypeTable::ERROR
                && right.type_id != TypeTable::ERROR
            {
                let type_table = self.type_table.borrow();
                let type_name = type_table.type_name(left.type_id);
                let op_char = match binary.op {
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
                let trait_name = match binary.op {
                    BinaryOp::Add => "Add",
                    BinaryOp::Sub => "Sub",
                    BinaryOp::Mul => "Mul",
                    BinaryOp::Div => "Div",
                    BinaryOp::Mod => "Rem",
                    BinaryOp::BitAnd => "BitAnd",
                    BinaryOp::BitOr => "BitOr",
                    BinaryOp::BitXor => "BitXor",
                    BinaryOp::Shl => "Shl",
                    BinaryOp::Shr => "Shr",
                    BinaryOp::Eq | BinaryOp::NotEq => "Eq",
                    BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => "Ord",
                    _ => "?",
                };
                drop(type_table);
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!(
                        "operator `{op_char}` cannot be applied to type `{type_name}`: type does not implement `{trait_name}`"
                    ),
                    span: binary.span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, binary.span);
            }
        }

        let op = util::convert_binary_op(binary.op);

        // Type check: both operands of a binary operation must have the same type.
        // This applies to primitives, newtypes, and all non-overloaded binary ops.
        // (Operator overloading via traits is dispatched earlier and doesn't reach here.)
        // Logical operators (&&, ||) are excluded since both sides are always bool.
        // We compare resolved types (not just TypeIds) because generic instantiation
        // can create distinct TypeIds for the same logical type.
        if !matches!(binary.op, BinaryOp::And | BinaryOp::Or)
            && left.type_id != right.type_id
            && left.type_id != TypeTable::ERROR
            && right.type_id != TypeTable::ERROR
            && left.type_id != TypeTable::NEVER
            && right.type_id != TypeTable::NEVER
        {
            let type_table = self.type_table.borrow();
            let left_name = type_table.type_name(left.type_id);
            let right_name = type_table.type_name(right.type_id);
            if left_name != right_name {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: left_name,
                    found: right_name,
                    span: binary.span,
                });
            }
        }

        // Determine result type based on operator
        let type_id = match binary.op {
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
                op,
                right: Box::new(right),
            },
            type_id,
            binary.span,
        )
    }

    /// Resolve a unary expression
    pub(super) fn resolve_unary(
        &mut self,
        unary: &ast::UnaryExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Special case: `&mut || { body }` - mutable closure desugaring
        if unary.op == UnaryOp::MutRef
            && let ast::Expr::Closure(closure_expr) = &unary.expr
        {
            return self.resolve_mutable_closure(closure_expr, ctx, unary.span);
        }

        let expr = self.resolve_expr(&unary.expr, ctx, None);
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
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: "mutable variable".to_string(),
                found: format!("immutable variable '{name}'"),
                span: unary.span,
            });
        }

        // Reject &mut on struct field access when the field is a primitive type.
        // In Wasm GC, struct.get returns a value copy for primitives, so &mut field
        // creates a disconnected Box — mutations don't propagate back to the struct.
        // For GC reference types (struct, String, Array, etc.), struct.get returns
        // the shared reference, so &mut field works correctly.
        if unary.op == UnaryOp::MutRef && matches!(&expr.kind, TirExprKind::FieldAccess { .. }) {
            let field_type = self.type_table.borrow().get(expr.type_id).clone();
            let base_type = self
                .type_table
                .borrow()
                .get(
                    self.type_table
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

        // Check for negation on non-primitive types that implement Neg trait
        if unary.op == UnaryOp::Neg {
            let expr_type = self.type_table.borrow().get(expr.type_id).clone();
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

                // Find the Neg trait implementation
                let neg_info = self
                    .find_arithmetic_trait_impl(&struct_name, expr.type_id, "Neg", "neg")
                    .map(|info| (info, struct_name.clone()))
                    .or_else(|| {
                        self.find_arithmetic_trait_impl(&lookup_name, lookup_type_id, "Neg", "neg")
                            .map(|info| (info, lookup_name.clone()))
                    });
                if let Some((trait_info, impl_name)) = neg_info {
                    // Adjust receiver for self kind (&self)
                    let receiver =
                        self.adjust_receiver_for_self_kind(expr, trait_info.self_kind, false, unary.span);

                    let mangled_method_name =
                        MethodName::format_local(&impl_name, Some(&trait_info.trait_name), "neg");

                    return TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef {
                                module_source: self.find_struct_module_source(&impl_name),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    impl_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    "neg".to_string(),
                                )),
                                is_cm_binding: false,
                            },
                            type_args: vec![],
                            args: vec![],
                        },
                        trait_info.output_type,
                        unary.span,
                    );
                }
            }
        }

        // Check for bitwise NOT on non-primitive types that implement BitNot trait
        if unary.op == UnaryOp::BitNot {
            let expr_type = self.type_table.borrow().get(expr.type_id).clone();
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

                // Find the BitNot trait implementation
                let bitnot_info = self
                    .find_arithmetic_trait_impl(&struct_name, expr.type_id, "BitNot", "bitnot")
                    .map(|info| (info, struct_name.clone()))
                    .or_else(|| {
                        self.find_arithmetic_trait_impl(
                            &lookup_name,
                            lookup_type_id,
                            "BitNot",
                            "bitnot",
                        )
                        .map(|info| (info, lookup_name.clone()))
                    });
                if let Some((trait_info, impl_name)) = bitnot_info {
                    // Adjust receiver for self kind (&self)
                    let receiver =
                        self.adjust_receiver_for_self_kind(expr, trait_info.self_kind, false, unary.span);

                    let mangled_method_name = MethodName::format_local(
                        &impl_name,
                        Some(&trait_info.trait_name),
                        "bitnot",
                    );

                    return TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef {
                                module_source: self.find_struct_module_source(&impl_name),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    impl_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    "bitnot".to_string(),
                                )),
                                is_cm_binding: false,
                            },
                            type_args: vec![],
                            args: vec![],
                        },
                        trait_info.output_type,
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
            UnaryOp::Ref => self.type_table.borrow_mut().make_ref(expr.type_id),
            UnaryOp::MutRef => self.type_table.borrow_mut().make_mut_ref(expr.type_id),
            UnaryOp::Deref => {
                let outer = self.type_table.borrow().get(expr.type_id).clone();
                match outer {
                    ResolvedType::MutRef(inner) => inner,
                    ResolvedType::Ref(inner) => {
                        // Dereffing a shared reference: &(&mut T) yields &T, not &mut T.
                        // Mutability cannot propagate outward through a shared reference.
                        let inner_resolved = self.type_table.borrow().get(inner).clone();
                        if let ResolvedType::MutRef(u) = inner_resolved {
                            self.type_table.borrow_mut().make_ref(u)
                        } else {
                            inner
                        }
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: "reference type".to_string(),
                            found: self.type_table.borrow().type_name(expr.type_id),
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

    /// Resolve an assignment expression
    pub(super) fn resolve_assign(
        &mut self,
        assign: &ast::AssignExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Check for index assignment on custom types: arr[i] = value -> arr.index_assign(i, value)
        if let ast::Expr::Index(index_expr) = &assign.target {
            // Resolve the indexed expression to get its type
            let indexed_expr = self.resolve_expr(&index_expr.expr, ctx, None);

            // Get base type (unwrap reference if needed)
            let base_type_id = match self.type_table.borrow().get(indexed_expr.type_id) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                _ => indexed_expr.type_id,
            };

            // Check for IndexAssign trait implementation
            // Arrays now use IndexAssign trait like other types
            {
                // Check for IndexAssign trait implementation
                let struct_name = match self.type_table.borrow().get(base_type_id).clone() {
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
                    let derefed_index_type = match self.type_table.borrow().get(index_type) {
                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Some(*inner),
                        _ => None,
                    };
                    if let Some(expected) = derefed_index_type {
                        self.check_ref_type_mismatch(index_type, expected, index_expr.index.span());
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
                        let value =
                            self.resolve_expr(&assign.value, ctx, Some(trait_info.input_type));

                        // Check: reject &T/&mut T assigned where non-ref expected
                        self.check_ref_type_mismatch(
                            value.type_id,
                            trait_info.input_type,
                            assign.value.span(),
                        );

                        let receiver = self.adjust_receiver_for_self_kind(
                            indexed_expr,
                            trait_info.self_kind,
                            false,
                            assign.span,
                        );

                        // Get the mangled method name: StructName^IndexAssign<IndexType>::index_assign
                        let mangled_method_name = MethodName::format_local(
                            &lookup_name,
                            Some(&trait_info.trait_name),
                            "index_assign",
                        );

                        return TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(receiver),
                                func: FunctionRef {
                                    module_source: trait_info.impl_module_source.clone(),
                                    name: mangled_method_name,
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        lookup_name,
                                        Some(trait_info.trait_name),
                                        "index_assign".to_string(),
                                    )),
                                    is_cm_binding: false,
                                },
                                type_args: vec![],
                                args: vec![
                                    CallArg::new(index_resolved, false),
                                    CallArg::new(value, false),
                                ],
                            },
                            TypeTable::UNIT,
                            assign.span,
                        );
                    }
                }
            }
        }

        // Standard assignment handling
        let target = self.resolve_expr(&assign.target, ctx, None);
        // Use target's type as expected type for value resolution
        // This enables coercion of empty array literals [] to the field's Array<T> type
        let value = self.resolve_expr(&assign.value, ctx, Some(target.type_id));

        // Reject &T assigned where non-ref T expected
        self.check_ref_type_mismatch(value.type_id, target.type_id, assign.value.span());

        // Handle assignment to global variables
        if let TirExprKind::GlobalVarGet {
            module_source,
            name,
        } = &target.kind
        {
            // Check if the global is mutable (check both local and imported globals)
            let is_mutable = self
                .current_module_globals
                .get(name)
                .map(|(_, m)| *m)
                .or_else(|| {
                    // For imported globals, the name in the TIR is the original name from source
                    // We need to find it by iterating through imported_globals
                    self.imported_globals
                        .values()
                        .find(|(src, orig_name, _, _)| src == module_source && orig_name == name)
                        .map(|(_, _, _, m)| *m)
                });

            if let Some(is_mut) = is_mutable {
                if !is_mut {
                    let _ = self.logger.error(TypeError::CannotAssign {
                        message: format!("cannot assign to immutable global variable '{name}'"),
                        span: assign.target.span(),
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, assign.span);
                }
                // Generate GlobalVarSet instead of Assign
                return TirExpr::new(
                    TirExprKind::GlobalVarSet {
                        module_source: module_source.clone(),
                        name: name.clone(),
                        value: Box::new(value),
                    },
                    TypeTable::UNIT,
                    assign.span,
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
                let inner_type = self.type_table.borrow().get(expr.type_id).clone();
                if matches!(inner_type, ResolvedType::Ref(_)) {
                    let _ = self.logger.error(TypeError::CannotAssign {
                        message: "cannot assign through immutable reference".to_string(),
                        span: assign.target.span(),
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
                span: assign.target.span(),
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, assign.span);
        }

        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value),
            },
            TypeTable::UNIT,
            assign.span,
        )
    }

    /// Resolve a compound assignment (already desugared, but handle anyway)
    pub(super) fn resolve_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // This should have been desugared, but handle it anyway
        let target = self.resolve_expr(&compound.target, ctx, None);
        let value = self.resolve_expr(&compound.value, ctx, Some(target.type_id));

        let op = match compound.op {
            ast::CompoundAssignOp::Add => TirBinaryOp::Add,
            ast::CompoundAssignOp::Sub => TirBinaryOp::Sub,
            ast::CompoundAssignOp::Mul => TirBinaryOp::Mul,
            ast::CompoundAssignOp::Div => TirBinaryOp::Div,
            ast::CompoundAssignOp::Mod => TirBinaryOp::Mod,
            ast::CompoundAssignOp::BitAnd => TirBinaryOp::BitAnd,
            ast::CompoundAssignOp::BitOr => TirBinaryOp::BitOr,
            ast::CompoundAssignOp::BitXor => TirBinaryOp::BitXor,
            ast::CompoundAssignOp::Shl => TirBinaryOp::Shl,
            ast::CompoundAssignOp::Shr => TirBinaryOp::Shr,
        };

        // target = target op value
        let binary = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(target.clone()),
                op,
                right: Box::new(value),
            },
            target.type_id,
            compound.span,
        );

        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(binary),
            },
            TypeTable::UNIT,
            compound.span,
        )
    }

    /// Resolve a comparison chain (already desugared, but handle anyway)
    pub(super) fn resolve_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // This should have been desugared to binary && chain
        // Just resolve the first expression for now
        self.resolve_expr(&chain.first, ctx, None)
    }
}
