//! Binary and unary operator resolution, including operator overloading.

use crate::ast::{self, AstId, BinaryOp, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::elaborator::synth::ArgSource;
use crate::hashmap::IndexMap;
use crate::name::{
    ARRAY_TYPE_NAME, FqTypeName, LocalMethodName, MethodName, Receiver, RefKind, minted_name,
};
use crate::primitive::PrimitiveType;
use crate::tir::{FunctionRef, MonomorphInfo, ResolvedType, TemplateId, TypeId};
use crate::token::Span;
use crate::type_table::TypeTable;
use crate::unparse::binary_op_str;

use super::Elaborator;
use super::coercion::{
    DeferredOperator, is_literal_arithmetic, is_primitive_literal_target,
    numeric_literal_pair_order, unary_passes_expected_type,
};
use super::expr::{IndexAccess, int_literal_repr, negated_literal};
use super::method_lookup::replace_on_assign_place;
use super::scope::ScopedBound;
use super::trait_query::primitive_has_operator;
use super::transposition::{Clean, Path, Reading, Step, Transposition, field_path, target_reads};
use super::types::{
    FunctionContext, MethodInfo, OperatorImpl, ResolvedTraitMethod, TypeError, append_reason_chain,
};
use super::tysys::TypeSystem;
use super::util::bound_param_name;
use crate::elaborator::reify::{CompoundHoist, collect_compound_hoists};
use crate::elaborator::sem::types::{
    AssignPlace, DestructuringFallback, DesugarKind, FallbackSite, OperatorDispatch,
};
use crate::elaborator::synth::ArgClass;
use crate::elaborator::trait_env::{ImplHeader, ImplTargetKey};
use crate::elaborator::types::RequiredTrait;
use crate::elaborator::tysys::{operator_compiler_item, operator_trait_method};
use crate::name::FqTraitName;

/// An operand whose operators come from its bounds.
struct BoundedOperand {
    /// What the dispatched method is named at; monomorphization substitutes it.
    receiver: FqTypeName,
    /// How the source writes the operand, for a diagnostic.
    spelled: String,
    bounds: Vec<ScopedBound>,
}

/// `-<integer literal>`, with the source text its range is judged against.
fn negated_int_literal(unary: &ast::UnaryExpr) -> Option<(&ast::LiteralExpr, &str)> {
    let lit = negated_literal(unary)?;
    int_literal_repr(lit).map(|repr| (lit, repr))
}

/// The right-hand side of an assignment passed to
/// [`Elaborator::assign_to_target`]. Either an AST expression (the
/// regular [`Elaborator::resolve_assign`] path) or an already-resolved
/// type (the [`Elaborator::resolve_compound_assign`] path, where the RHS
/// is `target op rhs` whose result type is computed via
/// [`Elaborator::resolve_binary_op`]).
///
/// The body walk only needs the resolved type and span: reify rebuilds
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

/// Where an assignment writes, resolved before the value it takes.
enum WritePlace {
    /// `recv[i] = v` through `IndexAssign`, its dispatch already recorded.
    IndexAssign {
        output_type: TypeId,
    },
    Place {
        target_type: TypeId,
    },
}

impl WritePlace {
    fn value_type(&self) -> TypeId {
        match self {
            Self::IndexAssign { output_type } => *output_type,
            Self::Place { target_type } => *target_type,
        }
    }
}

/// What `check_write` finds an assignment target to be, past the checks
/// that return early.
enum Lvalue {
    Place,
    NotAPlace,
    ThroughImmutableRef,
    /// Not a place, and its error is reported already.
    Reported,
}

/// The type the value written to `place` is resolved against. A refused place
/// still has its value resolved, so the value's own errors are reported too.
fn write_place_value_type(place: &Result<WritePlace, ()>) -> TypeId {
    place
        .as_ref()
        .map_or(TypeTable::ERROR, WritePlace::value_type)
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        let (mut left, mut right) = self.resolve_binary_operands_with_coercion(
            &binary.left,
            binary.op,
            &binary.right,
            ctx,
            expected_type,
        );
        // Operands share a type, so pin a holey side (`m.get_num() + 1`) to its
        // concrete sibling before operator dispatch, which would otherwise
        // mangle a trait-method name against the hole.
        if self.type_has_infer_hole(left) ^ self.type_has_infer_hole(right) {
            if self.type_has_infer_hole(left) {
                self.solve_infer_holes_against(left, right);
            } else {
                self.solve_infer_holes_against(right, left);
            }
            left = self.apply_infer_holes(left);
            right = self.apply_infer_holes(right);
        }
        // Pass the binary's source AstId so the operator-trait
        // dispatch path can record the decision under it.
        let resolved = self.resolve_binary_op(
            left,
            binary.op,
            right,
            binary.right.span(),
            binary.span,
            Some(binary.id),
        );
        if resolved != TypeTable::ERROR {
            let _ = self.defer_operator(DeferredOperator::Binary {
                left,
                op: binary.op,
                right,
                right_span: binary.right.span(),
                span: binary.span,
                id: binary.id,
            });
        }
        resolved
    }

    /// Resolve both operands of a binary op with the bidirectional
    /// numeric-literal coercion, shared with [`Self::desugar_comparison_chain`].
    /// A primitive coerces the literal to the other operand's type; a struct type
    /// such as `i128` takes each expected type from the operator trait's method
    /// signature, which covers an asymmetric `impl Shl<u32> for i8x16`.
    pub(super) fn resolve_binary_operands_with_coercion(
        &mut self,
        left_ast: &ast::Expr,
        op: BinaryOp,
        right_ast: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> (TypeId, TypeId) {
        // A comparison is a `bool` whatever its operands are.
        let expected_type = expected_type.filter(|_| !op.is_comparison());
        let left_is_literal = is_literal_arithmetic(left_ast);
        let right_is_literal = is_literal_arithmetic(right_ast);

        if left_is_literal && !right_is_literal {
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            let coerce_type = if self.literal_takes_operand_type(right) {
                Some(right)
            } else {
                self.find_operator_self_type(right, &op)
            };
            let left = self.resolve_expr(left_ast, ctx, coerce_type);
            (left, right)
        } else if right_is_literal && !left_is_literal {
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let coerce_type = if self.literal_takes_operand_type(left) {
                Some(left)
            } else {
                // Struct type: the literal has no type yet, so its class is
                // what selects among several `Add<Rhs>` impls.
                let rhs_class = self.synthesize_arg_class(right_ast, ctx);
                self.find_operator_rhs_type(left, &op, Some(&rhs_class), right_ast.span())
            };
            let right = self.resolve_expr(right_ast, ctx, coerce_type);
            (left, right)
        } else if left_is_literal && right_is_literal {
            let order = numeric_literal_pair_order(
                &self.tysys.type_table.borrow(),
                left_ast,
                right_ast,
                expected_type,
            );
            order.resolve(
                left_ast,
                right_ast,
                |expr, hint| self.resolve_expr(expr, ctx, hint),
                |&ty| ty,
            )
        } else if TypeSystem::is_null_literal(right_ast) && !TypeSystem::is_null_literal(left_ast) {
            // `expr == null`: resolve the non-null side first and feed its
            // type to the bare `null` so it resolves to a concrete
            // `Option<T>` instead of `Option<!>`.
            let left = self.resolve_expr(left_ast, ctx, expected_type);
            let right = self.resolve_expr(right_ast, ctx, Some(left));
            (left, right)
        } else if TypeSystem::is_null_literal(left_ast) && !TypeSystem::is_null_literal(right_ast) {
            // `null == expr`: symmetric to the above.
            let right = self.resolve_expr(right_ast, ctx, expected_type);
            let left = self.resolve_expr(left_ast, ctx, Some(right));
            (left, right)
        } else {
            let left_lit = op.is_comparison() && Self::takes_shape_from_expected_type(left_ast);
            let right_lit = op.is_comparison() && Self::takes_shape_from_expected_type(right_ast);
            if left_lit && !right_lit {
                let right = self.resolve_expr(right_ast, ctx, expected_type);
                let left_expected = if right == TypeTable::ERROR {
                    expected_type
                } else {
                    Some(right)
                };
                let left = self.resolve_expr(left_ast, ctx, left_expected);
                (left, right)
            } else {
                let left = self.resolve_expr(left_ast, ctx, expected_type);
                let right_expected = if op.is_comparison() && left != TypeTable::ERROR {
                    Some(left)
                } else {
                    expected_type
                };
                let right = self.resolve_expr(right_ast, ctx, right_expected);
                (left, right)
            }
        }
    }

    /// Whether a literal beside an operand of type `operand` takes that type:
    /// a primitive's, or the variable a pending call is still to answer, which
    /// the literal then waits at too.
    fn literal_takes_operand_type(&self, operand: TypeId) -> bool {
        is_primitive_literal_target(&self.tysys.type_table.borrow(), operand)
            || self.is_open_pending_var(operand)
    }

    /// Whether an operand's own elaboration would consult the expected type, so a
    /// comparison resolves the *other* side first and hands this one its type.
    /// Syntactic on purpose: reify decides the same order with no synthesis to
    /// ask, and a type-dependent order would desync the two walks inside a tuple
    /// `for-of`. A form left out just resolves on its own, so a subset is safe.
    pub(super) fn takes_shape_from_expected_type(expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::TupleLiteral(_) | ast::Expr::StaticMethodCall(_) => true,
            // A namespaced call (`Color::from_str(s)`) reads as a static path
            // too; a plain call's return type is its own.
            ast::Expr::Call(call) => {
                matches!(&call.callee, ast::Expr::Ident(id) if id.segments.len() >= 2)
            }
            _ => false,
        }
    }
}

impl TypeSystem {
    /// Whether `-x` / `~x` on `type_id` needs a trait impl: the primitive it
    /// bottoms out in, if any, carries no such operator.
    fn unary_operand_requires_trait(&self, op: UnaryOp, type_id: TypeId) -> bool {
        let item = match op {
            UnaryOp::Neg => CompilerItem::Neg,
            UnaryOp::BitNot => CompilerItem::BitNot,
            UnaryOp::Not | UnaryOp::Ref | UnaryOp::MutRef | UnaryOp::Deref | UnaryOp::Unchecked => {
                return false;
            }
        };
        !self
            .type_table
            .borrow()
            .primitive_head(type_id)
            .is_some_and(|prim| primitive_has_operator(prim.as_str(), item))
    }

    /// The primitive whose `core:prelude` impl answers an operator on `operand`
    /// that no Wasm instruction carries. `head` is `operand`'s representation head.
    fn primitive_op_receiver(&self, operand: TypeId, head: TypeId) -> Option<String> {
        if !self.binop_operand_requires_trait(operand) {
            return None;
        }
        let tt = self.type_table.borrow();
        matches!(tt.get(head), ResolvedType::Primitive(_))
            .then(|| tt.fq_base_type_name(head).into_string())
    }

    /// Whether a binary operator on `type_id` needs a trait impl. Either
    /// operand answering true means no primitive instruction can carry it.
    fn binop_operand_requires_trait(&self, type_id: TypeId) -> bool {
        let tt = self.type_table.borrow();
        match tt.get(type_id) {
            ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
            ResolvedType::Newtype { base_type, .. } => {
                let ultimate = tt.representation_head(*base_type);
                match tt.get(ultimate) {
                    ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
                    // SIMD aliases (`type f32x4 = v128`) and half aliases —
                    // see the `Primitive` arm below.
                    ResolvedType::Primitive(p) => *p == PrimitiveType::V128 || p.is_half(),
                    _ => false,
                }
            }
            // Types with no native Wasm binary-op support and no prelude
            // trait impl. Without rejection, codegen emits `ref.eq` /
            // `i32.eq` against a GC reference and fails validation.
            ResolvedType::Function { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::AssocTypeProjection { .. } => true,
            // `Array<T>` is a GC array reference: `i32.eq` against it produces
            // invalid core Wasm, and `wir_build` panics before that ("no scalar
            // lowering for BuiltinArray"). Its comparison dispatches through the
            // element-wise `Eq` / `Ord` impls in `core:prelude/array.wado`.
            ResolvedType::BuiltinArray(_) => true,
            // No Wasm instruction: `i32.eq` on a `v128` is invalid, and a half's
            // `u16` would add bit patterns. Their `core:prelude` impls answer.
            ResolvedType::Primitive(
                PrimitiveType::V128 | PrimitiveType::F16 | PrimitiveType::Bf16,
            ) => true,
            // `()` pushes nothing, so the `i32.eq` below it underflows the
            // stack; its `Eq` / `Ord` are the prelude's impls for `()`, and
            // the other operators have none. `Never` is deliberately absent:
            // `panic("boom") + 1` is legal.
            ResolvedType::Unit => true,
            _ => false,
        }
    }

    /// The one method an operator trait declares.
    fn operator_method_name(&self, item: CompilerItem) -> String {
        self.type_table
            .borrow()
            .compiler_items()
            .trait_method_name(item)
            .to_string()
    }

    /// The operator trait a dispatch means, as a requirement the bound search must match.
    pub(super) fn required_compiler_trait(&self, item: CompilerItem) -> Option<RequiredTrait> {
        Some(self.required_trait(self.compiler_trait_def(item)?))
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve a binary operator over pre-resolved operand types.
    ///
    /// Shared between [`Self::resolve_binary`] (the user-AST entry point)
    /// and elaborator-internal callers like
    /// [`Self::desugar_comparison_chain`] / [`Self::resolve_compound_assign`]
    /// that have already resolved both sides. Handles resource-handle equality,
    /// trait dispatch for non-primitive comparison / arithmetic / shift,
    /// flags-arith rejection, float-`%` rejection, and the trailing
    /// requires-trait diagnostic.
    pub(super) fn resolve_binary_op(
        &mut self,
        left: TypeId,
        op: BinaryOp,
        right: TypeId,
        right_span: Span,
        span: Span,
        origin: Option<ast::AstId>,
    ) -> TypeId {
        // A newtype over a reference with no comparison impl of its own
        // compares as that reference, whose operators are the impls written
        // for references.
        let own_impl = operator_compiler_item(&op)
            .and_then(|item| self.tysys.compiler_trait_def(item))
            .and_then(|trait_| self.tysys.own_impl_link(left, trait_));
        let (left, right) = if op.is_comparison() && own_impl.is_none() {
            (self.tysys.wrapped_ref_type(left), self.tysys.wrapped_ref_type(right))
        } else {
            (left, right)
        };
        // Check if this is a comparison operation on a non-primitive type
        // Non-primitives use Eq/Ord traits instead of direct Wasm instructions
        let left_type = self.tysys.type_table.borrow().get(left).clone();

        // Reify rebuilds a handle `==` / `!=` from the AST; project only the type.
        if self.tysys.compares_handles(op, left, right) {
            return TypeTable::BOOL;
        }

        let is_comparison = op.is_comparison();

        if is_comparison
            && let Some((resolved, receiver)) = self.resolve_ref_comparison(left, op, right)
        {
            let call = self.dispatch_trait_op_method(
                receiver,
                vec![(right, right_span)],
                &resolved,
                origin,
            );
            // Reify rebuilds `!=` and an ordering from `eq` / `cmp`.
            return if call == TypeTable::ERROR {
                TypeTable::ERROR
            } else {
                TypeTable::BOOL
            };
        }

        // A pair of references has only the operators an impl for the
        // reference writes, as `Eq for &T` does. A mixed pair (`&i32 == i32`)
        // falls through.
        let both_refs = matches!(
            (&left_type, self.tysys.type_table.borrow().get(right)),
            (
                ResolvedType::Ref(_) | ResolvedType::MutRef(_),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        );
        if both_refs {
            let type_name = self.tysys.type_table.borrow().type_name(left);
            let _ = self.emit(TypeError::OperatorNotApplicable {
                op: binary_op_str(op).to_string(),
                operands: vec![type_name],
                note: None,
                span,
            });
            return TypeTable::ERROR;
        }

        if is_comparison {
            // A newtype's representation would answer `<` as it is, so ask the
            // chain before it does.
            if matches!(
                op,
                BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
            ) && self.tysys.ord_withheld_by(left).is_some()
            {
                return self.operator_not_implemented(op, left, right, span);
            }
            // A type that erases to a scalar is still its own type, so an impl
            // it writes — or inherits from a link below — answers the
            // comparison before the erased form's instruction does.
            let comparison_impl_link = operator_compiler_item(&op)
                .and_then(|item| self.tysys.compiler_trait_def(item))
                .and_then(|trait_| self.tysys.own_impl_link(left, trait_));
            // The receiver for trait lookup, named by its declaring module and
            // read off the resolved type. `Enum` and `Flags` appear only under
            // the guard above — absent an impl of their own, `==` lowers to a
            // native discriminant or bitmask compare. `Variant` must be
            // present, since only `resolve_trait_method_for_op` records the
            // bound-driven synthesis request, and synthesis has already run.
            let struct_name = match &left_type {
                ResolvedType::Struct { .. }
                | ResolvedType::GenericInstance { .. }
                | ResolvedType::Variant { .. }
                // `fq_base_type_name` spells a raw GC array `Array` and the
                // unit type `()`, the names their `Eq` / `Ord` impls attach to.
                | ResolvedType::BuiltinArray(_)
                | ResolvedType::Unit => Some(
                    self.tysys
                        .type_table
                        .borrow()
                        .fq_base_type_name(left)
                        .into_string(),
                ),
                ResolvedType::Newtype { .. }
                | ResolvedType::Enum { .. }
                | ResolvedType::Flags { .. }
                    if let Some(link) = comparison_impl_link =>
                {
                    Some(
                        self.tysys
                            .type_table
                            .borrow()
                            .fq_base_type_name(link)
                            .into_string(),
                    )
                }
                ResolvedType::Primitive(_) => self.tysys.primitive_op_receiver(left, left),
                // No written impl answers, so the derivation the representation
                // carries does — named by that head, not by one peel, which on a
                // chain (`type B = A; type A = Point`) lands on another newtype.
                ResolvedType::Newtype { base_type, .. } => {
                    let tt = self.tysys.type_table.borrow();
                    let ultimate = tt.representation_head(*base_type);
                    match tt.get(ultimate) {
                        ResolvedType::Struct { .. }
                        | ResolvedType::GenericInstance { .. }
                        | ResolvedType::Variant { .. } => {
                            Some(tt.fq_base_type_name(ultimate).into_string())
                        }
                        _ => self.tysys.primitive_op_receiver(left, ultimate),
                    }
                }
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                let lookup_type_id = if let Some(link) = comparison_impl_link {
                    link
                } else {
                    self.tysys.type_table.borrow().representation_head(left)
                };

                // Handle Eq trait (== and !=)
                if matches!(op, BinaryOp::Eq | BinaryOp::NotEq) {
                    let eq_method = self.tysys.operator_method_name(CompilerItem::Eq);
                    let Some(resolved) = self.resolve_comparison_method(
                        CompilerItem::Eq,
                        &eq_method,
                        &struct_name,
                        lookup_type_id,
                        Some(&ArgClass::Exact(right)),
                    ) else {
                        return self.operator_not_implemented(op, left, right, span);
                    };
                    // Reify rebuilds the `!` wrapper for `!=`.
                    return self.dispatch_trait_op_method(
                        left,
                        vec![(right, right_span)],
                        &resolved,
                        origin,
                    );
                }

                // Handle Ord trait (<, >, <=, >=)
                if matches!(
                    op,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
                ) {
                    let cmp = self.tysys.operator_method_name(CompilerItem::Ord);
                    let Some(resolved) = self.resolve_comparison_method(
                        CompilerItem::Ord,
                        &cmp,
                        &struct_name,
                        lookup_type_id,
                        None,
                    ) else {
                        return self.operator_not_implemented(op, left, right, span);
                    };
                    let call = self.dispatch_trait_op_method(
                        left,
                        vec![(right, right_span)],
                        &resolved,
                        origin,
                    );
                    if call == TypeTable::ERROR {
                        return TypeTable::ERROR;
                    }
                    return TypeTable::BOOL;
                }
            }

            // A bounded operand compares through its bounds. Monomorphization
            // substitutes it with the concrete type and either resolves the
            // method call normally, or converts back to a binary op for primitives.
            if let Some(operand) = self.bounded_operand(left) {
                let is_eq = matches!(op, BinaryOp::Eq | BinaryOp::NotEq);
                let item = if is_eq {
                    CompilerItem::Eq
                } else {
                    CompilerItem::Ord
                };
                let method = self.tysys.operator_method_name(item);
                let Some((bound_trait_name, info)) = self.find_operator_in_bounds(
                    &operand.bounds,
                    left,
                    item,
                    &method,
                    Some(right),
                    span,
                ) else {
                    self.report_operator_bound_missing(&operand.spelled, item, span);
                    return TypeTable::ERROR;
                };
                // `Eq` keeps the bound's own spelling, so an argument it writes
                // (`S: Eq<String>`) names the impl that answers it.
                let trait_name = if is_eq {
                    bound_trait_name
                } else {
                    self.tysys.type_table.borrow().compiler_trait_fq(item)
                };
                let resolved =
                    ResolvedTraitMethod::through_bound(operand.receiver, trait_name, &method, info);
                let call = self.dispatch_trait_op_method(
                    left,
                    vec![(right, right_span)],
                    &resolved,
                    origin,
                );
                // Reify rebuilds the `!` wrapper for `!=`.
                if is_eq || call == TypeTable::ERROR {
                    return call;
                }
                return TypeTable::BOOL;
            }
        }

        // Non-primitive operands dispatch through Add/Sub/Mul/Div/Rem,
        // BitAnd/BitOr/BitXor and Shl/Shr.
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
                | BinaryOp::Shl
                | BinaryOp::Shr
        );

        if is_arithmetic_or_bitwise {
            let Some((item, method_name)) = operator_trait_method(&op) else {
                unreachable!("an arithmetic or bitwise operator names its trait method")
            };
            if let Some(struct_name) = self.tysys.operator_receiver_name(left) {
                let Some(trait_) = self.tysys.operator_trait_decl(&op) else {
                    return TypeTable::ERROR;
                };

                // For newtypes, resolve base type for trait impl fallback
                let (lookup_name, lookup_type_id) =
                    self.tysys
                        .trait_impl_base_lookup(&struct_name, left, trait_);

                // The right operand is resolved by now, so its type selects
                // among the receiver's `Add<Rhs>` impls (WEP 2026-07-31).
                let rhs_class = ArgClass::Exact(right);
                let mut admitted = self.find_arithmetic_trait_impls(
                    &struct_name,
                    left,
                    trait_,
                    method_name,
                    Some(&rhs_class),
                );
                let mut impl_name = struct_name.clone();
                let mut impl_type_id = left;
                if admitted.is_empty() {
                    // The impl is read on the link that wrote it, so the right
                    // operand is read there too: `impl Add for Vec2`
                    // dispatched through `Position` declares `&Vec2`, and the
                    // operand is a `Position` over the same base.
                    let rhs_base = self.tysys.own_impl_link(right, trait_).unwrap_or_else(|| {
                        self.tysys.type_table.borrow().representation_head(right)
                    });
                    let rhs_class = ArgClass::Exact(rhs_base);
                    admitted = self.find_arithmetic_trait_impls(
                        &lookup_name,
                        lookup_type_id,
                        trait_,
                        method_name,
                        Some(&rhs_class),
                    );
                    impl_name.clone_from(&lookup_name);
                    impl_type_id = lookup_type_id;
                }
                // Unique-or-error: several admitted impls are reported here,
                // where the span is, and dispatch falls through to the
                // operand-type error below.
                self.report_ambiguous_operator_rhs(&admitted, left, op, span);
                let trait_info_opt = match admitted.as_slice() {
                    [_only] => admitted.into_iter().next(),
                    [] | [_, _, ..] => None,
                };
                if let Some(info) = trait_info_opt {
                    let found = OperatorImpl {
                        info,
                        impl_name,
                        impl_type_id,
                    };
                    return self.dispatch_operator_impl(
                        left,
                        (right, right_span),
                        found,
                        method_name,
                        origin,
                    );
                }
            }

            if let Some(call) = self.dispatch_operator_through_bounds(
                left,
                item,
                method_name,
                Some((right, right_span)),
                span,
                origin,
            ) {
                return call;
            }
        }

        // Check for arithmetic on flags types: only bitwise ops are allowed
        {
            let left_resolved = self.tysys.type_table.borrow().get(left).clone();
            let is_flags_arith = matches!(
                op,
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod
            ) && matches!(left_resolved, ResolvedType::Flags { .. });
            if is_flags_arith {
                let op_char = binary_op_str(op);
                let type_name = self.tysys.type_table.borrow().type_name(left);
                let _ = self.emit(TypeError::OperatorNotApplicable {
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

        // Wasm has no float remainder and no float bitwise or shift
        // instruction. Lowering one anyway emits the i32 opcode over an f64
        // operand, which the validator reports as an opaque type mismatch.
        // Newtypes resolve to their base, so `type Meters = f64` is rejected
        // like `f64`.
        {
            let float_name = {
                let table = self.tysys.type_table.borrow();
                match table.primitive_head(left) {
                    Some(PrimitiveType::F32) => Some("f32"),
                    Some(PrimitiveType::F64) => Some("f64"),
                    _ => None,
                }
            };
            if let Some(type_name) = float_name {
                let rejected = match op {
                    BinaryOp::Mod => Some(("%", format!("use `{type_name}::fmod(a, b)` instead"))),
                    BinaryOp::Shl
                    | BinaryOp::Shr
                    | BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor => Some((
                        binary_op_str(op),
                        "bitwise and shift operators are integer-only".to_string(),
                    )),
                    _ => None,
                };
                if let Some((op_char, note)) = rejected {
                    let _ = self.emit(TypeError::OperatorNotApplicable {
                        op: op_char.to_string(),
                        operands: vec![type_name.to_string()],
                        note: Some(note),
                        span,
                    });
                    return TypeTable::ERROR;
                }
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
            let left_requires = self.tysys.binop_operand_requires_trait(left);
            let right_requires = self.tysys.binop_operand_requires_trait(right);
            if (left_requires || right_requires)
                && !op.is_short_circuit()
                && left != TypeTable::ERROR
                && right != TypeTable::ERROR
            {
                let type_table = self.tysys.type_table.borrow();
                let left_name = type_table.type_name(left);
                let right_name = type_table.type_name(right);
                let op_char = binary_op_str(op);
                drop(type_table);
                if left_name == right_name {
                    // Both operands share a type that lacks the operator's
                    // trait impl (e.g. `Point - Point` with no `Sub`).
                    return self.operator_not_implemented(op, left, right, span);
                }
                if let Some(item) = operator_compiler_item(&op)
                    && self.report_unmet_trait_impl_bound(left, &[item], Some(right), span)
                {
                    return TypeTable::ERROR;
                }
                // Mixed operand types that cannot combine under this
                // operator (e.g. `i32 - List<i32>`). Reported the same
                // way regardless of which operand is non-primitive.
                let _ = self.emit(TypeError::OperatorNotApplicable {
                    op: op_char.to_string(),
                    operands: vec![left_name, right_name],
                    note: None,
                    span,
                });
                return TypeTable::ERROR;
            }
        }

        // Type check: both operands of a binary operation must have the same type.
        // This applies to primitives, newtypes, and all non-overloaded binary ops.
        // (Operator overloading via traits is dispatched earlier and doesn't reach here.)
        // Logical operators (&&, ||) are excluded since both sides are always bool.
        // We compare resolved types (not just TypeIds) because generic instantiation
        // can create distinct TypeIds for the same logical type.
        if !op.is_short_circuit()
            && left != right
            && left != TypeTable::NEVER
            && right != TypeTable::NEVER
            && !self.tysys.type_table.borrow().contains_undecided(left)
            && !self.tysys.type_table.borrow().contains_undecided(right)
        {
            let type_table = self.tysys.type_table.borrow();
            let (left_name, right_name) = type_table.type_names_for_mismatch(left, right);
            if left_name != right_name {
                let _ = self.emit(TypeError::TypeMismatch {
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
            _ => left, // Arithmetic ops preserve the type
        };

        // Reify rebuilds the native `Binary` from the AST +
        // recorded operand `expression_types`; the body walk projects
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
        let inner_expected = match unary.op {
            UnaryOp::Ref | UnaryOp::MutRef => expected_type
                .and_then(|expected| self.tysys.pointee_of(expected))
                .or_else(|| {
                    let make = if unary.op == UnaryOp::MutRef {
                        TypeTable::make_mut_ref
                    } else {
                        TypeTable::make_ref
                    };
                    let pointee =
                        self.open_shape_at_pending_var(expected_type, 1, |tt, vars| {
                            make(tt, vars[0])
                        })?;
                    Some(pointee[0])
                }),
            UnaryOp::Unchecked => expected_type,
            _ if unary_passes_expected_type(unary) => expected_type,
            // Either keeps the variable a pending call is still to answer,
            // dispatched again once it is.
            UnaryOp::Neg | UnaryOp::BitNot
                if expected_type.is_some_and(|t| self.is_open_pending_var(t)) =>
            {
                expected_type
            }
            UnaryOp::Neg | UnaryOp::BitNot | UnaryOp::Not | UnaryOp::Deref => None,
        };
        // `&mut xs[i]` is the one position that reaches an element mutably, so the
        // subscript resolves through `IndexRefMut` and carries the mutability in
        // the accessor's signature rather than upgrading a `&` reference.
        let expr_type = match (&unary.op, &unary.expr) {
            (UnaryOp::MutRef, ast::Expr::Index(index)) => {
                self.resolve_index_access(index, ctx, IndexAccess::Mutable)
            }
            (UnaryOp::Ref, ast::Expr::Index(index)) => {
                self.resolve_index_access(index, ctx, IndexAccess::Shared)
            }
            _ => match negated_int_literal(unary) {
                // Not resolved on its own: reading `NUM` alone would judge it
                // against the *positive* `i32` boundary.
                Some((lit, repr)) => {
                    if expected_type.is_none() {
                        self.check_default_int_literal(repr, true, unary.span);
                    } else {
                        self.check_int_literal_parses(repr, lit.span);
                    }
                    self.record_expression_type(lit.id, TypeTable::I32);
                    TypeTable::I32
                }
                None => self.resolve_expr(&unary.expr, ctx, inner_expected),
            },
        };

        if unary.op == UnaryOp::MutRef {
            self.record_mut_ref(&unary.expr, ctx);
            if let Some(binding) = self.place_roots_at_immutable_binding(&unary.expr, ctx) {
                let _ = self.emit(TypeError::CannotMutate {
                    message: format!("cannot take &mut of immutable variable '{binding}'"),
                    span: unary.span,
                });
            }
            if matches!(&unary.expr, ast::Expr::FieldAccess(_) | ast::Expr::Index(_))
                && self.tysys.is_replace_on_assign_place_type(expr_type)
            {
                let _ = self.emit(TypeError::CannotMutate {
                    message: format!(
                        "cannot take a mutable reference to {}",
                        replace_on_assign_place()
                    ),
                    span: unary.span,
                });
            }
        }

        if matches!(unary.op, UnaryOp::Neg | UnaryOp::BitNot) {
            // An operand that is still a bare variable a pending call answers
            // has no head to dispatch on until it is answered.
            let dispatched = if self.is_open_pending_var(expr_type) {
                None
            } else {
                self.dispatch_unary_operator(unary.op, expr_type, unary.span, unary.id)
            };
            if dispatched != Some(TypeTable::ERROR) {
                let _ = self.defer_operator(DeferredOperator::Unary {
                    op: unary.op,
                    operand: expr_type,
                    span: unary.span,
                    id: unary.id,
                });
            }
            if let Some(dispatched) = dispatched {
                return dispatched;
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
                        let _ = self.emit(TypeError::TypeMismatch {
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

        // Reify rebuilds the `Unary` (`*ptr` / `&x` / `&mut x` /
        // `!b`) and folds `-literal` from the AST; the `&mut`-field
        // diagnostic and the deref/ref type computation above are the
        // record-only work. `assign_to_target`'s deref l-value check now
        // reads the operand type from `expression_types` instead of this
        // resolved `kind`.
        type_id
    }

    /// Dispatch `-` or `~` on an operand of type `expr_type`: the answer
    /// where an impl or a rejection decides it, `None` where the instruction
    /// on the operand does.
    pub(super) fn dispatch_unary_operator(
        &mut self,
        op: UnaryOp,
        expr_type: TypeId,
        span: Span,
        id: ast::AstId,
    ) -> Option<TypeId> {
        let (method_name, item, op_symbol) = match op {
            UnaryOp::Neg => ("neg", CompilerItem::Neg, "-"),
            UnaryOp::BitNot => ("bitnot", CompilerItem::BitNot, "~"),
            UnaryOp::Not | UnaryOp::Ref | UnaryOp::MutRef | UnaryOp::Deref | UnaryOp::Unchecked => {
                panic!("only `-` and `~` dispatch through a trait")
            }
        };
        if self.tysys.type_table.borrow().is_never(expr_type) {
            return Some(expr_type);
        }
        let trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(item)
            .to_string();
        if let Some(call) = self.dispatch_operator_through_bounds(
            expr_type,
            item,
            method_name,
            None,
            span,
            Some(id),
        ) {
            return Some(call);
        }
        if let Some(struct_name) = self.tysys.operator_receiver_name(expr_type) {
            let Some(trait_) = self.tysys.compiler_trait_def(item) else {
                return Some(TypeTable::ERROR);
            };
            let (lookup_name, lookup_type_id) =
                self.tysys
                    .trait_impl_base_lookup(&struct_name, expr_type, trait_);
            let resolved = self
                .resolve_trait_method_for_op(&struct_name, expr_type, trait_, method_name, None)
                .or_else(|| {
                    self.resolve_trait_method_for_op(
                        &lookup_name,
                        lookup_type_id,
                        trait_,
                        method_name,
                        None,
                    )
                });
            if let Some(resolved) = resolved {
                // Record the dispatch keyed by the unary expr's AstId
                // so reify can replay the `Neg::neg` / `BitNot::bitnot`
                // method call instead of emitting a bare `Unary` on a
                // struct (which codegen rejects: `expected i32, found
                // (ref $T)`). Mirrors the binary-operator path.
                return Some(self.dispatch_trait_op_method(expr_type, vec![], &resolved, Some(id)));
            }
        }

        // No impl answered, and the WIR lowering has no opcode for this
        // operand, so reject here: `scalar_kind` would panic instead.
        if expr_type != TypeTable::ERROR && self.tysys.unary_operand_requires_trait(op, expr_type) {
            if self.report_unmet_trait_impl_bound(expr_type, &[item], None, span) {
                return Some(TypeTable::ERROR);
            }
            let operand = self.tysys.type_table.borrow().type_name(expr_type);
            let _ = self.emit(TypeError::OperatorNotApplicable {
                op: op_symbol.to_string(),
                operands: vec![operand],
                note: Some(format!("type does not implement `{trait_name}`")),
                span,
            });
            return Some(TypeTable::ERROR);
        }
        None
    }

    /// Resolve an assignment expression.
    pub(super) fn resolve_assign(
        &mut self,
        assign: &ast::AssignExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        match &assign.target {
            ast::AssignTarget::Place(target) => {
                self.assign_to_target(target, AssignValue::Ast(&assign.value), ctx)
            }
            ast::AssignTarget::Destructuring { places, .. } => {
                self.resolve_destructuring_assign(assign, places, ctx)
            }
        }
    }

    /// Resolve `[t0, t1, …] = value` as `let [$da_0, $da_1, …] = value;` and
    /// then `t0 = $da_0; t1 = $da_1; …`. The tuple of the types the target
    /// places take is the value's expected type.
    fn resolve_destructuring_assign(
        &mut self,
        assign: &ast::AssignExpr,
        targets: &[ast::Expr],
        ctx: &mut FunctionContext,
    ) -> TypeId {
        self.record_desugar(assign.id, DesugarKind::DestructuringAssign);
        let cause = match Reading::of(assign) {
            Reading::Transposition(transposition) => {
                match self.transpositions.get(&assign.id).copied() {
                    None => Some(DestructuringFallback::Rebindable),
                    Some(clean) => {
                        match self.resolve_index_swap(assign, &transposition, clean, ctx) {
                            Ok(()) => return TypeTable::UNIT,
                            Err(cause) => cause,
                        }
                    }
                }
            }
            Reading::NotAPermutation => Some(DestructuringFallback::NotAPermutation),
            Reading::OtherContainers => Some(DestructuringFallback::OtherContainer),
            Reading::UnplainIndex => Some(DestructuringFallback::UnplainIndex),
            Reading::Permutation if targets.iter().all(|t| self.is_owned_place(t, ctx)) => {
                Some(DestructuringFallback::ReferenceReadable)
            }
            Reading::Permutation => Some(DestructuringFallback::NotOwned),
        };
        self.sem.types.destructuring_fallbacks.insert(
            assign.span,
            FallbackSite {
                cause,
                target_reads: target_reads(assign),
            },
        );
        let places: Vec<Result<WritePlace, ()>> = targets
            .iter()
            .map(|target| self.resolve_write_place(target, ctx))
            .collect();
        let expected = self
            .tysys
            .type_table
            .borrow_mut()
            .make_tuple(places.iter().map(write_place_value_type).collect());
        let value_type = self.resolve_expr(&assign.value, ctx, Some(expected));
        let Ok(places) = places.into_iter().collect::<Result<Vec<_>, ()>>() else {
            return TypeTable::ERROR;
        };
        if value_type == TypeTable::ERROR {
            return TypeTable::ERROR;
        }
        let element_types = self
            .tysys
            .type_table
            .borrow()
            .as_tuple(value_type)
            .filter(|elements| elements.len() == targets.len());
        let Some(element_types) = element_types else {
            let found = self.tysys.type_table.borrow().type_name(value_type);
            let _ = self.emit(TypeError::DestructuringArity {
                targets: targets.len(),
                found,
                span: assign.value.span(),
            });
            return TypeTable::ERROR;
        };
        // Reserve reify's `$da_N` locals, so both walks allocate the same frame.
        let ctx = &mut ctx.enter_scope();
        for &type_id in &element_types {
            let name = minted_name("da", ctx.fresh_serial());
            let _local_index = ctx.add_local(name, type_id, false, None);
        }
        let element_spans: Vec<Span> = match &assign.value {
            ast::Expr::TupleLiteral(tuple) if tuple.elements.len() == targets.len() => {
                tuple.elements.iter().map(ast::Expr::span).collect()
            }
            value => vec![value.span(); targets.len()],
        };
        for (((target, place), type_id), span) in targets
            .iter()
            .zip(&places)
            .zip(element_types)
            .zip(element_spans)
        {
            self.check_write(target, place, type_id, span, ctx);
        }
        TypeTable::UNIT
    }

    /// Resolve a transposition as its container's `index_swap`, recording the
    /// dispatch reify replays. `Err` falls back to the desugaring, with no
    /// dispatch recorded and no local taken, and says why where the desugaring
    /// does not report an error. The desugaring resolves again what this
    /// resolved: its facts replace these, and the logger reports a repeated
    /// diagnostic once.
    fn resolve_index_swap(
        &mut self,
        assign: &ast::AssignExpr,
        transposition: &Transposition<'_>,
        clean: Clean,
        ctx: &mut FunctionContext,
    ) -> Result<(), Option<DestructuringFallback>> {
        let container = transposition.container;
        let Some(root) = ctx.binding(transposition.container_root()) else {
            return Err(Some(DestructuringFallback::ContainerNotLocal));
        };
        let rebindable_root = clean.container_root_referenced && root.is_mut;
        let root_type = root.type_id;
        let container_type = self.resolve_expr(container, ctx, None);
        if container_type == TypeTable::ERROR {
            return Err(None);
        }
        if self.holds_rebindable_reference(
            &transposition.container_path(),
            root_type,
            rebindable_root,
        ) {
            return Err(Some(DestructuringFallback::Rebindable));
        }
        if matches!(
            self.tysys.type_table.borrow().get(container_type),
            ResolvedType::Ref(_)
        ) || self.place_roots_at_immutable_ref(container)
            || self
                .place_roots_at_immutable_binding(container, ctx)
                .is_some()
        {
            return Err(None);
        }
        let base_type_id = self.tysys.through_ref(container_type);
        let no_index_swap = |this: &Self, key: Option<TypeId>| {
            let tt = this.tysys.type_table.borrow();
            Err(Some(DestructuringFallback::NoIndexSwap {
                container: tt.type_name(base_type_id),
                key: key.map(|key| tt.type_name(key)),
            }))
        };
        let Some(head) = self.index_dispatch_head(base_type_id) else {
            return no_index_swap(self, None);
        };
        let (lookup_name, lookup_type_id) = self.tysys.newtype_base_lookup(&head, base_type_id);
        let lookup = |this: &mut Self, key: Option<TypeId>| {
            this.index_lookup_or_newtype_base(
                &head,
                base_type_id,
                &lookup_name,
                lookup_type_id,
                |s, n, t| s.find_index_swap_trait_impl(n, t, key),
            )
        };
        // The impl is the one at the key a read of `c[i]` selects.
        let key_class = self.synthesize_arg_class(transposition.i, ctx);
        let key = self.index_key_type(container_type, &key_class);
        let Some((info, _)) = lookup(self, key) else {
            return no_index_swap(self, key);
        };
        let index_type = info.index_type;
        let i_type = self.resolve_expr(transposition.i, ctx, index_type);
        let j_type = self.resolve_expr(transposition.j, ctx, index_type);
        if j_type == TypeTable::ERROR || i_type == TypeTable::ERROR {
            return Err(None);
        }
        // The desugaring's first write could change a mutable global.
        if transposition.index_reads().into_iter().any(|read| {
            matches!(
                self.assign_place_of(read.id),
                Some(AssignPlace::Global { mutable: true, .. })
            )
        }) {
            return Err(Some(DestructuringFallback::Rebindable));
        }
        if let Some(index_type) = index_type {
            self.typecheck(i_type, index_type, transposition.i.span());
            self.typecheck(j_type, index_type, transposition.j.span());
        }
        self.resolve_unread_subscripts(assign, index_type, ctx);
        // Reserve reify's `$da_N` for the second index, so both walks allocate
        // the same frame.
        let ctx = &mut ctx.enter_scope();
        let name = minted_name("da", ctx.fresh_serial());
        let _local_index = ctx.add_local(name, j_type, false, None);
        let function_ref = self.indexing_function_ref(&info, "index_swap");
        self.sem.types.index_swap_dispatch.insert(
            assign.id,
            OperatorDispatch {
                function_ref,
                method_def: Some(info.method_def),
                self_kind: info.self_kind,
                arg_ref_wraps: vec![false, false],
                return_type: TypeTable::UNIT,
                needs_deref: false,
            },
        );
        Ok(())
    }

    /// Resolve the parts of a transposition `index_swap` reads none of: the
    /// second target's container, and the right side, its indices at
    /// `index_type` as the targets'. A name there is still a reference, and a
    /// subscript there is a place, not an element read.
    fn resolve_unread_subscripts(
        &mut self,
        assign: &ast::AssignExpr,
        index_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) {
        let (ast::AssignTarget::Destructuring { places, .. }, ast::Expr::TupleLiteral(value)) =
            (&assign.target, &assign.value)
        else {
            unreachable!("a transposition assigns a tuple to two targets");
        };
        let ast::Expr::Index(second) = &places[1] else {
            unreachable!("a transposition's targets are subscripts");
        };
        self.resolve_expr(&second.expr, ctx, None);
        for element in &value.elements {
            let ast::Expr::Index(subscript) = element else {
                unreachable!("a transposition reads two subscripts");
            };
            self.resolve_expr(&subscript.expr, ctx, None);
            self.resolve_expr(&subscript.index, ctx, index_type);
        }
    }

    /// Whether `place` is owned: a local, or a field path from one, reached
    /// through no reference and no subscript.
    fn is_owned_place(&self, place: &ast::Expr, ctx: &FunctionContext) -> bool {
        field_path(place).is_some_and(|path| {
            ctx.lookup(&path.root.name)
                .is_some_and(|local| self.tysys.pointee_of(local.type_id).is_none())
        })
    }

    /// Whether a place on `path` that something else may write holds a
    /// reference, so the desugaring's first write could change which container
    /// the second names. The root is such a place where `root_writable`, and so
    /// is every place reached through a reference.
    fn holds_rebindable_reference(
        &mut self,
        path: &Path<'_>,
        root_type: TypeId,
        root_writable: bool,
    ) -> bool {
        let mut writable = root_writable;
        let mut place_type = root_type;
        for step in &path.steps {
            if let Some(pointee) = self.tysys.pointee_of(place_type) {
                if writable {
                    return true;
                }
                writable = true;
                place_type = pointee;
            }
            if let Step::Field(field) = step {
                place_type = self
                    .lookup_field_type(place_type, &field.field, field.span)
                    .1;
            }
        }
        writable && self.tysys.pointee_of(place_type).is_some()
    }

    /// The name an indexing trait impl on `base_type_id` is looked up under,
    /// or `None` for a type no such impl can be written for.
    fn index_dispatch_head(&self, base_type_id: TypeId) -> Option<String> {
        let tt = self.tysys.type_table.borrow();
        match tt.get(base_type_id) {
            ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. } => Some(tt.base_type_name(base_type_id)),
            // `arr[i]` dispatches through `impl … for Array<T>`, keyed by the
            // base name "Array".
            ResolvedType::BuiltinArray(_) => Some(ARRAY_TYPE_NAME.to_string()),
            _ => None,
        }
    }

    /// Resolve `expr` with every subscript `target` projects through standing as
    /// a `&mut` place. `Err` is the refusal, already reported: one of those
    /// subscripts hands out something no write can reach the element through.
    fn resolve_place_receiver(
        &mut self,
        expr: &ast::Expr,
        target: &ast::Expr,
        ctx: &mut FunctionContext,
    ) -> Result<TypeId, ()> {
        let projected = projected_subscripts_of_place(target);
        let resolved = self.resolve_expr(expr, &mut ctx.marking_mut_places(&projected), None);
        if let Some(refusal) = self
            .subscripts_written_through(target)
            .find_map(|id| self.subscript_write_refusal(id))
        {
            let _ = self.emit(TypeError::CannotMutate {
                message: refusal.to_string(),
                span: target.span(),
            });
            return Err(());
        }
        Ok(resolved)
    }

    /// The subscripts of [`projected_subscripts_of_place`] the write lands in.
    /// One behind a reference is not: `l[0].a` on a `List<&mut P>` writes into
    /// the `P` the element points to, whatever hands the element out.
    fn subscripts_written_through<'a>(
        &'a self,
        target: &'a ast::Expr,
    ) -> impl Iterator<Item = AstId> + 'a {
        place_bases(target)
            .take_while(|expr| {
                self.sem
                    .types
                    .expression_types
                    .get(&expr.id())
                    .is_none_or(|ty| self.reference_mutability(*ty).is_none())
            })
            .filter_map(subscript_id)
    }

    /// Why a write cannot reach the element this subscript names, if it cannot.
    /// Only `IndexRefMut`'s `&mut Output` can be written through: `IndexRef`
    /// hands out a shared alias, and `IndexValue` hands out a copy, which the
    /// write would land on and then discard.
    fn subscript_write_refusal(&self, index_id: AstId) -> Option<&'static str> {
        let dispatch = self.sem.types.operator_dispatch.get(&index_id)?;
        if !dispatch.needs_deref {
            return Some("cannot assign through a by-value index; the container hands out a copy");
        }
        matches!(
            self.tysys.type_table.borrow().get(dispatch.return_type),
            ResolvedType::Ref(_)
        )
        .then_some("cannot assign through immutable reference")
    }

    /// What an `Index` target reaching `check_write`'s general path is: a tuple
    /// index is a place. A read-only `Index` access lowers to `*recv.index(i)`
    /// over `&Output`, so writing through it is an immutable-reference error; an
    /// `IndexValue` access is a by-value call and no place at all.
    fn index_target_lvalue(&self, index_expr: &ast::IndexExpr) -> Lvalue {
        if let Some(dispatch) = self.sem.types.operator_dispatch.get(&index_expr.id) {
            return if dispatch.needs_deref {
                Lvalue::ThroughImmutableRef
            } else {
                Lvalue::NotAPlace
            };
        }
        // An unrecorded receiver is an unindexable one, which `resolve_index`
        // diagnosed.
        let Some(&recv_type) = self.sem.types.expression_types.get(&index_expr.expr.id()) else {
            return Lvalue::Reported;
        };
        let base = self.tysys.through_ref(recv_type);
        if self.tysys.type_table.borrow().is_tuple(base) {
            Lvalue::Place
        } else {
            Lvalue::NotAPlace
        }
    }

    /// What `*operand` is as a write target: a place, unless `operand` is an
    /// immutable reference (`&T`). Inspecting the *operand* type means
    /// `*5 = v` (operand `5: i32`, already diagnosed by `resolve_unary`) is
    /// not re-flagged here.
    fn deref_target_lvalue(&self, unary: &ast::UnaryExpr) -> Lvalue {
        // An ERROR-typed operand is not recorded, and was diagnosed already.
        let Some(&operand_type) = self.sem.types.expression_types.get(&unary.expr.id()) else {
            return Lvalue::Reported;
        };
        if matches!(
            self.tysys.type_table.borrow().get(operand_type),
            ResolvedType::Ref(_)
        ) {
            Lvalue::ThroughImmutableRef
        } else {
            Lvalue::Place
        }
    }

    /// Resolve an assignment `target = value`, where the value may be a
    /// user-AST expression or an already-resolved type.
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
        let place = self.resolve_write_place(target_ast, ctx);
        let value_span = value.span();
        let value_type = match value {
            AssignValue::Ast(expr) => {
                self.resolve_expr(expr, ctx, Some(write_place_value_type(&place)))
            }
            AssignValue::Resolved { type_id, .. } => type_id,
        };
        let Ok(place) = place else {
            return TypeTable::ERROR;
        };
        self.check_write(target_ast, &place, value_type, value_span, ctx)
    }

    /// Resolve the place `target_ast` names, ahead of the value written to it.
    /// `Err` is the refusal, already reported.
    fn resolve_write_place(
        &mut self,
        target_ast: &ast::Expr,
        ctx: &mut FunctionContext,
    ) -> Result<WritePlace, ()> {
        // Check for index assignment on custom types: arr[i] = value -> arr.index_assign(i, value)
        if let ast::Expr::Index(index_expr) = target_ast {
            let indexed_type = self.resolve_place_receiver(&index_expr.expr, target_ast, ctx)?;

            let indexed_immutable = matches!(
                self.tysys.type_table.borrow().get(indexed_type),
                ResolvedType::Ref(_)
            ) || self.place_roots_at_immutable_ref(&index_expr.expr);
            if indexed_immutable {
                let _ = self.emit(TypeError::CannotMutate {
                    message: "cannot assign through immutable reference".to_string(),
                    span: target_ast.span(),
                });
                return Err(());
            }
            if let Some(binding) = self.place_roots_at_immutable_binding(&index_expr.expr, ctx) {
                let _ = self.emit(TypeError::CannotMutate {
                    message: format!("cannot assign into immutable '{binding}'"),
                    span: target_ast.span(),
                });
                return Err(());
            }

            // Get base type (unwrap reference if needed)
            let base_type_id = self.tysys.through_ref(indexed_type);

            {
                if let Some(struct_name) = self.index_dispatch_head(base_type_id) {
                    // For newtypes, resolve the base type name for trait impl lookup
                    let (lookup_name, lookup_type_id) =
                        self.tysys.newtype_base_lookup(&struct_name, base_type_id);
                    let assign_info = self.index_lookup_or_newtype_base(
                        &struct_name,
                        base_type_id,
                        &lookup_name,
                        lookup_type_id,
                        Elaborator::find_index_assign_trait_impl,
                    );
                    let key_type = assign_info.as_ref().and_then(|(info, _)| info.index_type);
                    let index_type = self.resolve_expr(&index_expr.index, ctx, key_type);

                    // Reject &T/&mut T used as index expression (would ICE in codegen)
                    if let Some(expected) = self.tysys.pointee_of(index_type) {
                        self.typecheck(index_type, expected, index_expr.index.span());
                    }

                    if let Some((trait_info, _)) = assign_info {
                        let output_type = trait_info.output_type;
                        let func = self.indexing_function_ref(&trait_info, "index_assign");

                        // Record the
                        // resolved `IndexAssign` dispatch keyed by
                        // the inner `IndexExpr`'s `AstId` so reify
                        // can replay the same `arr.index_assign(idx,
                        // value)` shape for `arr[i] = v` and
                        // `arr[i] OP= v`.
                        self.record_index_assign_dispatch(
                            index_expr.id,
                            OperatorDispatch {
                                function_ref: func,
                                method_def: Some(trait_info.method_def),
                                self_kind: trait_info.self_kind,
                                arg_ref_wraps: vec![false, false],
                                return_type: TypeTable::UNIT,
                                needs_deref: false,
                            },
                        );

                        return Ok(WritePlace::IndexAssign { output_type });
                    }
                    // Where no `IndexRefMut` writes through instead, an
                    // `IndexAssign` impl missing only its bound says why.
                    if self
                        .index_lookup_or_newtype_base(
                            &struct_name,
                            base_type_id,
                            &lookup_name,
                            lookup_type_id,
                            Elaborator::find_index_mut_trait_impl,
                        )
                        .is_none()
                        && self.report_unmet_trait_impl_bound(
                            base_type_id,
                            &[CompilerItem::IndexAssign],
                            Some(index_type),
                            target_ast.span(),
                        )
                    {
                        return Err(());
                    }
                }
            }
        }

        // Standard assignment handling: the target isn't `Expr::Index`, or the
        // IndexAssign trait lookup returned None.
        let target_type = self.resolve_place_receiver(target_ast, target_ast, ctx)?;
        Ok(WritePlace::Place { target_type })
    }

    /// Check that `target_ast`, resolved to `place`, takes a value of
    /// `value_type`.
    fn check_write(
        &mut self,
        target_ast: &ast::Expr,
        place: &WritePlace,
        value_type: TypeId,
        value_span: Span,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        // Reject &T assigned where non-ref T expected
        self.typecheck(value_type, place.value_type(), value_span);
        // Reify rebuilds `arr.index_assign(idx, value)` from the recorded
        // `index_assign_dispatch` and the AST.
        if let WritePlace::IndexAssign { .. } = place {
            return TypeTable::UNIT;
        }

        // Handle assignment to global variables. The global is recognised
        // from the recorded `AssignPlace::Global` on the target ident, which
        // carries the resolved mutability.
        let global_assign = if let ast::Expr::Ident(id) = target_ast {
            match self.assign_place_of(id.id) {
                Some(AssignPlace::Global { name, mutable, .. }) => Some((name.clone(), *mutable)),
                _ => None,
            }
        } else {
            None
        };
        if let Some((name, is_mut)) = global_assign {
            if !is_mut {
                let _ = self.emit(TypeError::CannotMutate {
                    message: format!("cannot assign to immutable global variable '{name}'"),
                    span: target_ast.span(),
                });
                return TypeTable::ERROR;
            }
            // reify rebuilds the `GlobalVarSet` from the AST; project the
            // (unit) result type.
            return TypeTable::UNIT;
        }

        // A projection is writable only when nothing on the way to its storage
        // forbids it. Reported before the classification below, which only
        // decides whether the target is a place at all, so one target yields
        // one diagnostic.
        if matches!(target_ast, ast::Expr::FieldAccess(_) | ast::Expr::Index(_)) {
            if self.place_roots_at_immutable_ref(target_ast) {
                let _ = self.emit(TypeError::CannotMutate {
                    message: "cannot assign through immutable reference".to_string(),
                    span: target_ast.span(),
                });
                return TypeTable::ERROR;
            }
            if let Some(binding) = self.place_roots_at_immutable_binding(target_ast, ctx) {
                let _ = self.emit(TypeError::CannotMutate {
                    message: format!("cannot assign into immutable '{binding}'"),
                    span: target_ast.span(),
                });
                return TypeTable::ERROR;
            }
        }

        // Validate that the target is a valid l-value: each target shape is
        // classified from the AST + recorded facts.
        let lvalue = match target_ast {
            ast::Expr::FieldAccess(_) => Lvalue::Place,
            ast::Expr::Index(index_expr) => self.index_target_lvalue(index_expr),
            ast::Expr::Unary(u) if u.op == ast::UnaryOp::Deref => self.deref_target_lvalue(u),
            // An identifier target classified by the place fact `resolve_ident`
            // recorded: a function-frame `Local`, or a `&mut`-captured ident
            // (`*$ref` deref-capture, assignable iff the captured ref is
            // `&mut`). Globals returned above; anything else (function /
            // variant / enum / const ident, or a non-ident expression) is not
            // a place.
            ast::Expr::Ident(id) => match self.assign_place_of(id.id) {
                Some(
                    AssignPlace::Local
                    | AssignPlace::DerefCapture {
                        through_mut_ref: true,
                    },
                ) => Lvalue::Place,
                Some(AssignPlace::DerefCapture {
                    through_mut_ref: false,
                }) => Lvalue::ThroughImmutableRef,
                _ => Lvalue::NotAPlace,
            },
            _ => Lvalue::NotAPlace,
        };
        let message = match lvalue {
            // Reify rebuilds the `Assign` from the AST and the recorded types.
            Lvalue::Place => return TypeTable::UNIT,
            Lvalue::Reported => return TypeTable::ERROR,
            // An `ERROR` target was diagnosed where it was resolved.
            Lvalue::NotAPlace if place.value_type() == TypeTable::ERROR => {
                return TypeTable::ERROR;
            }
            Lvalue::ThroughImmutableRef => "cannot assign through immutable reference",
            Lvalue::NotAPlace => "cannot assign to an expression that is not a place",
        };
        let _ = self.emit(TypeError::CannotMutate {
            message: message.to_string(),
            span: target_ast.span(),
        });
        TypeTable::ERROR
    }

    /// Resolve `target op= value` as `target = target op value`, TIR-direct. The
    /// read side is an ordinary `resolve_expr` on the target, which dispatches
    /// the `Index` trait for a custom indexed read; the write side reuses
    /// [`Self::assign_to_target`], fed the already-resolved value through
    /// [`AssignValue::Resolved`]. `target` is evaluated twice.
    pub(super) fn resolve_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        self.record_desugar(compound.id, DesugarKind::CompoundAssign);
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
        // Reserve reify's `$ca_N` locals, resolving each impure sub-piece once
        // (a subscript against its index key type) so the annotate and reify
        // frames stay identical; later walks re-encounter each piece via
        // `compound_hoist_types` instead of re-resolving.
        let mut scope = ctx.enter_scope();
        // `h[0].n += 3` reads and writes one place, so the read side takes the
        // same `&mut` subscript the write side does.
        let mut marked = scope.marking_mut_places(&projected_subscripts_of_place(&compound.target));
        let mut frame = marked.replacing(|ctx| &mut ctx.compound_hoist_types, IndexMap::default());
        let ctx = &mut *frame;
        let mut hoists: Vec<CompoundHoist<'_>> = Vec::new();
        collect_compound_hoists(&compound.target, &mut hoists);
        for hoist in &hoists {
            let expected = hoist
                .index_ctx
                .and_then(|ix| self.compound_index_key_type(ix, ctx));
            let piece_type = self.resolve_expr(hoist.piece, ctx, expected);
            let name = minted_name("ca", ctx.fresh_serial());
            let _local_index = ctx.add_local(name, piece_type, false, None);
            ctx.compound_hoist_types
                .insert(hoist.piece.id(), piece_type);
        }

        let read_type = self.resolve_expr(&compound.target, ctx, None);
        let rhs_type = self.resolve_expr(&compound.value, ctx, Some(read_type));
        // When the operands are
        // operator-overloaded (non-primitive, e.g. `u128 /= u128`),
        // `resolve_binary_op` dispatches the combined value through the
        // trait method (`Div::div` → `div_rem`). Tag the record with the
        // compound's AstId so reify replays that method call instead of a raw
        // `Binary` (a primitive `/` on struct operands is invalid Wasm).
        let combined = self.resolve_binary_op(
            read_type,
            op,
            rhs_type,
            compound.value.span(),
            compound.span,
            Some(compound.id),
        );
        self.assign_to_target(
            &compound.target,
            AssignValue::Resolved {
                type_id: combined,
                span: compound.span,
            },
            ctx,
        )
    }

    /// Resolve `a OP1 b OP2 c …` as `(a OP1 b) & (b OP2 c) …`, every operand
    /// but the last bound once to a `$m_K` local, so each evaluates left to right.
    pub(super) fn desugar_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        assert!(
            chain.comparisons.len() >= 2,
            "the parser makes a single comparison a `Binary`"
        );
        self.record_desugar(chain.id, DesugarKind::ComparisonChain);

        // Enter a fresh scope for the `$m_K` bindings so they don't leak
        // into the surrounding function's local namespace.
        let ctx = &mut ctx.enter_scope();

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

        Self::bind_chain_operand(first_tir, ctx);
        Self::bind_chain_operand(right0_tir, ctx);
        let mut acc = self.resolve_binary_op(
            first_tir,
            cmp0.op,
            right0_tir,
            cmp0.right.span(),
            cmp0.op_span,
            Some(cmp0.id),
        );
        let mut prev = right0_tir;

        let last_idx = chain.comparisons.len() - 1;
        for idx in 1..chain.comparisons.len() {
            let cmp = &chain.comparisons[idx];
            // Coerce a literal `right` against `prev`'s type; non-literals
            // resolve normally (`resolve_expr` ignores `expected_type` when it
            // can't help).
            let right = self.resolve_expr(&cmp.right, ctx, Some(prev));
            if idx != last_idx {
                Self::bind_chain_operand(right, ctx);
            }
            let cmp_type = self.resolve_binary_op(
                prev,
                cmp.op,
                right,
                cmp.right.span(),
                cmp.op_span,
                Some(cmp.id),
            );
            // The `&` joining two comparisons is synthesised, so its right
            // operand is the comparison the chain just built, not written text.
            acc = self.resolve_binary_op(
                acc,
                BinaryOp::BitAnd,
                cmp_type,
                cmp.op_span,
                chain.span,
                None,
            );
            prev = right;
        }

        TypeTable::BOOL
    }

    /// Allocate a `$m_K` local for a comparison-chain operand, in walk order;
    /// reify rebuilds the `Let` itself.
    fn bind_chain_operand(type_id: TypeId, ctx: &mut FunctionContext) {
        let name = minted_name("m", ctx.fresh_serial());
        let _local_index = ctx.add_local(name, type_id, false, None);
    }

    /// `Output` for an operator dispatched through a bound: what the method
    /// was typed to return at the bound that answered, `typed`, which reads
    /// that bound's own `Output` (`T: Mul<Output = T>`) at its arguments.
    fn operator_output_type(
        &mut self,
        operand_type_id: TypeId,
        found_trait: &FqTraitName,
        typed: TypeId,
    ) -> TypeId {
        let Some(trait_) = self.tysys.trait_env.trait_def_of_fq(found_trait) else {
            return operand_type_id;
        };
        if !self.tysys.trait_env.declares_assoc_type(&trait_, "Output") {
            return operand_type_id;
        }
        // `Output = Self::Size` names the operand's own projection again,
        // which is the operand, whatever bindings the bound's frame carried.
        let table = self.tysys.type_table.borrow();
        if table.projects_alike(typed, operand_type_id) {
            operand_type_id
        } else {
            typed
        }
    }

    /// Report that `left` does not implement the trait `op` dispatches to, with
    /// the reason chain saying why. Under `#![no_prelude]` the trait is absent,
    /// and no chain explains its absence.
    fn operator_not_implemented(
        &mut self,
        op: BinaryOp,
        left: TypeId,
        right: TypeId,
        span: Span,
    ) -> TypeId {
        let item = operator_compiler_item(&op).expect("`&&` and `||` dispatch to no trait");
        if self.report_unmet_trait_impl_bound(left, &[item], Some(right), span) {
            return TypeTable::ERROR;
        }
        let trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(item)
            .to_string();
        let reason = self
            .tysys
            .compiler_trait_def(item)
            .map(|trait_| {
                self.tysys.trait_unimpl_reason_chain(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    left,
                    trait_,
                    &trait_name,
                )
            })
            .unwrap_or_default();
        let type_name = self.tysys.type_table.borrow().type_name(left);
        let _ = self.emit(TypeError::OperatorNotApplicable {
            op: binary_op_str(op).to_string(),
            operands: vec![type_name],
            note: Some(append_reason_chain(
                format!("type does not implement `{trait_name}`"),
                &reason,
            )),
            span,
        });
        TypeTable::ERROR
    }

    /// A comparison operator's trait method on `struct_name`, named through
    /// the compiler item that anchors the trait.
    fn resolve_comparison_method(
        &mut self,
        item: CompilerItem,
        method: &str,
        struct_name: &str,
        lookup_type_id: TypeId,
        rhs: Option<&ArgClass>,
    ) -> Option<ResolvedTraitMethod> {
        let trait_ = self.tysys.compiler_trait_def(item)?;
        self.resolve_trait_method_for_op(struct_name, lookup_type_id, trait_, method, rhs)
    }

    /// A comparison of a reference, answered by an impl written for the
    /// reference itself and chosen by the right operand, as a generic body
    /// instantiated at `&T` dispatches. A pair of references reaches the
    /// prelude's `Eq for &T`, which compares the pointees. Answers the method
    /// and the receiver it is dispatched on. `None` leaves the pair to the
    /// rules for values: nothing here dereferences the left operand.
    fn resolve_ref_comparison(
        &mut self,
        left: TypeId,
        op: BinaryOp,
        right: TypeId,
    ) -> Option<(ResolvedTraitMethod, TypeId)> {
        let kind = RefKind::from_resolved(self.tysys.type_table.borrow().get(left))?;
        let item = match op {
            BinaryOp::Eq | BinaryOp::NotEq => CompilerItem::Eq,
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => CompilerItem::Ord,
            _ => unreachable!("resolve_ref_comparison takes a comparison operator"),
        };
        let trait_ = self.tysys.compiler_trait_def(item)?;
        let method = self.tysys.operator_method_name(item);
        // `&mut X` coerces to `&X`, so a block for `&X` answers where none
        // for `&mut X` does. A `&` right operand never coerces to `&mut`, so
        // there the `&X` block goes first: `&mut a == &b` compares as `&a == &b`.
        let right_is_shared = matches!(
            self.tysys.type_table.borrow().get(right),
            ResolvedType::Ref(_)
        );
        let kinds: &[RefKind] = match kind {
            RefKind::Shared => &[RefKind::Shared],
            RefKind::Mut if right_is_shared => &[RefKind::Shared, RefKind::Mut],
            RefKind::Mut => &[RefKind::Mut, RefKind::Shared],
        };
        let (kind, info) = kinds.iter().find_map(|&kind| {
            let info = self.find_operator_impl_on(
                &ImplTargetKey::Ref(kind),
                left,
                trait_,
                &method,
                Some(&ArgClass::Exact(right)),
            )?;
            Some((kind, info))
        })?;
        // The block's `Self` is the reference it is written for, which a
        // coerced `&mut` left operand is not.
        let receiver = if kind == RefKind::Shared {
            let pointee = self.tysys.pointee_of(left).expect("`left` is a reference");
            self.tysys.type_table.borrow_mut().make_ref(pointee)
        } else {
            left
        };
        let found = OperatorImpl {
            info,
            impl_name: kind.prefix().to_string(),
            impl_type_id: receiver,
        };
        let mut resolved = ResolvedTraitMethod::of_operator_impl(&self.tysys, found, &method);
        resolved.return_type = self.tysys.auto_derive_return_type(item);
        Some((resolved, receiver))
    }

    /// An operator on a bounded operand, dispatched through its bounds. `None`
    /// where `receiver` is not one; a missing trait is reported as `ERROR`.
    fn dispatch_operator_through_bounds(
        &mut self,
        receiver: TypeId,
        item: CompilerItem,
        method_name: &str,
        rhs: Option<(TypeId, Span)>,
        span: Span,
        origin: Option<AstId>,
    ) -> Option<TypeId> {
        let operand = self.bounded_operand(receiver)?;
        let Some((found_trait, info)) = self.find_operator_in_bounds(
            &operand.bounds,
            receiver,
            item,
            method_name,
            rhs.map(|(rhs, _)| rhs),
            span,
        ) else {
            self.report_operator_bound_missing(&operand.spelled, item, span);
            return Some(TypeTable::ERROR);
        };
        let return_type = self.operator_output_type(receiver, &found_trait, info.return_type);
        let mut resolved =
            ResolvedTraitMethod::through_bound(operand.receiver, found_trait, method_name, info);
        resolved.return_type = return_type;
        Some(self.dispatch_trait_op_method(receiver, rhs.into_iter().collect(), &resolved, origin))
    }

    /// `ty` as an operand its bounds give operators to: a type parameter, or a
    /// projection its trait bounds (`type Item: Eq`). `None` for any other type.
    fn bounded_operand(&self, ty: TypeId) -> Option<BoundedOperand> {
        let (receiver, spelled) = {
            let tt = self.tysys.type_table.borrow();
            let resolved = tt.get(ty);
            if let Some(param) = bound_param_name(&tt, ty) {
                (FqTypeName::binder(param), param.clone())
            } else if matches!(resolved, ResolvedType::AssocTypeProjection { .. }) {
                (tt.fq_type_name(ty), tt.type_name(ty))
            } else {
                return None;
            }
        };
        Some(BoundedOperand {
            receiver,
            spelled,
            bounds: self.carried_bounds(ty),
        })
    }

    /// The bound in `bounds` that declares `item`'s method, with the method as
    /// that bound types it for `receiver` and `rhs`.
    fn find_operator_in_bounds(
        &mut self,
        bounds: &[ScopedBound],
        receiver: TypeId,
        item: CompilerItem,
        method_name: &str,
        rhs: Option<TypeId>,
        span: Span,
    ) -> Option<(FqTraitName, MethodInfo)> {
        let required = self.tysys.required_compiler_trait(item);
        let args = match rhs {
            Some(rhs) => ArgSource::Types(vec![self.tysys.type_table.borrow_mut().make_ref(rhs)]),
            None => ArgSource::NoArguments,
        };
        self.find_method_in_trait_bounds(
            None,
            false,
            bounds,
            method_name,
            receiver,
            span,
            required.as_ref(),
            args,
        )
    }

    /// Dispatch a binary operator to the method its admitted impl declares.
    fn dispatch_operator_impl(
        &mut self,
        left: TypeId,
        right: (TypeId, Span),
        found: OperatorImpl,
        method_name: &str,
        origin: Option<ast::AstId>,
    ) -> TypeId {
        let resolved = ResolvedTraitMethod::of_operator_impl(&self.tysys, found, method_name);
        self.dispatch_trait_op_method(left, vec![right], &resolved, origin)
    }

    /// Dispatch a unary or binary operator to a trait method: the sole place that
    /// checks its arguments, decides their `&`-wrapping and records the dispatch.
    fn dispatch_trait_op_method(
        &mut self,
        receiver: TypeId,
        args: Vec<(TypeId, Span)>,
        resolved: &ResolvedTraitMethod,
        origin: Option<ast::AstId>,
    ) -> TypeId {
        if args.len() != resolved.param_types.len() {
            assert!(
                self.logger.has_errors(),
                "only an impl whose arity differs from its trait's reaches here, \
                 and `TraitMethodArityMismatch` reports it"
            );
            return TypeTable::ERROR;
        }

        // A block written for a reference has that reference as its `Self`.
        let receiver_head = match self.ref_impl_of(resolved) {
            Some(_) => receiver,
            None => self.tysys.get_base_type(receiver),
        };
        // An impl read on a link below the receiver answers in the receiver's
        // type, as a method call does (WEP 2026-01-29).
        let return_type = match resolved.impl_type_id {
            Some(link) if link != receiver_head => {
                self.tysys
                    .substitute_newtype_in_type(resolved.return_type, link, receiver_head)
            }
            _ => resolved.return_type,
        };

        // Per argument, decide whether the parameter is by reference, compute the
        // logical expected type, and delegate to the same `typecheck` helper
        // `resolve_method_call` uses — so operator dispatch and a direct call
        // apply identical `check_assignable` rules and cannot diverge. Mismatches
        // accumulate in the logger rather than early-returning, as method calls do.
        let mut wrap_flags: Vec<bool> = Vec::with_capacity(args.len());
        for (&(arg, arg_span), &param_ty) in args.iter().zip(resolved.param_types.iter()) {
            let wrap = matches!(
                self.tysys.type_table.borrow().get(param_ty),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            );
            // For a `&Self` parameter the "value-level" expected type is the
            // receiver's type, preserving newtype identity when an impl on the
            // base is dispatched through a newtype receiver. A parameterized
            // operator trait (`impl Add<Feet> for Meters`) declares a
            // *different* referent, and that one is what the operand must
            // match. A by-value parameter is itself the expected type.
            let expected = if wrap {
                let referent = self.tysys.through_ref(param_ty);
                // A trait spelled with an argument (`Add<Feet>`) declares a
                // referent of its own. A bare `Add` declares `&Self`, which a
                // variadic `impl Ord for [..T]` resolves to a shape the operand
                // does not equal.
                let declares_rhs = (resolved.is_type_param_receiver
                    || !resolved.trait_name.args().is_empty())
                    && referent != receiver
                    && referent != receiver_head;
                if declares_rhs { referent } else { receiver }
            } else {
                param_ty
            };
            self.typecheck_into_callee(arg, expected, arg_span);
            wrap_flags.push(wrap);
        }

        let function_ref = match self.ref_impl_of(resolved) {
            Some((kind, header)) => self.ref_impl_function(kind, header, resolved, receiver),
            None => self.operator_impl_function(resolved, receiver),
        };

        // When the operator-dispatch request carries a source AST id in
        // `origin`, record the dispatch decision so reify can re-emit the
        // same method-call TIR for the binary / index expression.
        // Synthesised callers (e.g. the `&` joining a comparison chain's
        // links) pass `None` and the record is skipped — they have no
        // source-level node reify would key on.
        if let Some(ast_id) = origin {
            self.record_operator_dispatch(
                ast_id,
                OperatorDispatch {
                    function_ref,
                    method_def: resolved.method_def,
                    self_kind: resolved.self_kind,
                    arg_ref_wraps: wrap_flags,
                    return_type,
                    needs_deref: false,
                },
            );
        }

        // Reify rebuilds the overloaded operator's method call from the
        // recorded `operator_dispatch` (receiver adjustment via `self_kind`,
        // arg `&`-wrapping via `arg_ref_wraps`) + the AST.
        return_type
    }

    /// The block `resolved` dispatches to, where it is written for a `&X` /
    /// `&mut X` target, with that reference's kind.
    fn ref_impl_of(&self, resolved: &ResolvedTraitMethod) -> Option<(RefKind, &ImplHeader)> {
        let header = self.tysys.trait_env.impl_headers.get(&resolved.impl_def?)?;
        Some((header.target.ref_kind()?, header))
    }

    /// The function a `&X` / `&mut X` block's method is at `receiver`: the
    /// block's own for a concrete one, else the instance of its template.
    fn ref_impl_function(
        &self,
        kind: RefKind,
        header: &ImplHeader,
        resolved: &ResolvedTraitMethod,
        receiver: TypeId,
    ) -> FunctionRef {
        let template = LocalMethodName::of(
            Receiver::of_ref_impl(kind, &header.target_id),
            Some(resolved.trait_name.clone()),
            resolved.method_name.clone(),
        );
        let template_id = resolved
            .method_def
            .zip(resolved.impl_def)
            .map(|(def, block)| TemplateId::in_block(def, block));
        if header.is_concrete() {
            let function = template.at_owner(&header.target_id);
            return FunctionRef {
                module_source: header.module.clone(),
                name: function.to_mangled_name(),
                template: template_id,
                monomorph_info: None,
                method_info: Some(function),
            };
        }
        let pointee = self
            .tysys
            .pointee_of(receiver)
            .expect("a reference block answers a reference receiver");
        // `&T` binds `T` to the pointee; `&Holder<T>` to the pointee's arguments.
        let is_blanket = matches!(template.receiver(), Receiver::Ref(_));
        let impl_type_args = if is_blanket {
            vec![pointee]
        } else {
            self.tysys
                .type_table
                .borrow()
                .nominal_type_args(pointee)
                .unwrap_or_default()
        };
        let arg_names: Vec<FqTypeName> = impl_type_args
            .iter()
            .map(|&arg| self.tysys.type_table.borrow().fq_type_name(arg))
            .collect();
        let instance = template.with_struct_type_args(&arg_names);
        FunctionRef {
            module_source: header.module.clone(),
            name: instance.to_mangled_name(),
            template: template_id,
            monomorph_info: Some(MonomorphInfo {
                generic_name: template.to_mangled_name(),
                impl_type_args,
                method_type_args: vec![],
                is_blanket,
            }),
            method_info: Some(instance),
        }
    }

    /// The function an operator method is when a value type's impl, or a type
    /// parameter's bound, answers it.
    fn operator_impl_function(
        &self,
        resolved: &ResolvedTraitMethod,
        receiver: TypeId,
    ) -> FunctionRef {
        let receiver_fq = resolved.receiver.clone();
        let mangled_method_name = MethodName::format_local(
            &receiver_fq,
            Some(&resolved.trait_name),
            &resolved.method_name,
        );

        let mut method_info = LocalMethodName::new(
            receiver_fq,
            Some(resolved.trait_name.clone()),
            resolved.method_name.clone(),
        );
        method_info.is_type_param_receiver = resolved.is_type_param_receiver;

        // Only a *concrete* block's function lives in the block's module: a
        // generic block's instance is materialised in the receiver type's,
        // the convention `TraitEnv::concrete_impl_module_for` encodes.
        //
        // Everything else answers from the receiver's newtype chain, keyed on
        // the link the lookup was made on — peeling to the base would send an
        // impl written on the newtype to the base's module.
        let defs = self.tysys.resolutions.defs();
        let concrete_impl = resolved
            .impl_def
            .filter(|def| self.tysys.trait_env.impl_headers[def].is_concrete());
        let module_source = match concrete_impl {
            Some(def) => defs.module(def).clone(),
            None => self
                .impl_target_decl_key(receiver, &resolved.impl_name)
                .map_or_else(
                    || self.declaring_module_of(&resolved.impl_name),
                    |def| defs.module(def).clone(),
                ),
        };
        let template = match resolved.method_def {
            _ if resolved.is_type_param_receiver => None,
            Some(def) => Some(match resolved.impl_def {
                Some(block) => TemplateId::in_block(def, block),
                None => self.tysys.signatures.declared_template(def),
            }),
            None => Some(TemplateId::derived(module_source.clone(), &method_info)),
        };
        FunctionRef {
            module_source,
            name: mangled_method_name,
            template,
            monomorph_info: None,
            method_info: Some(method_info),
        }
    }
}

/// Every subscript an assignment target projects through, as in `h[0].n` or
/// `o[0][0].n` — the one place a subscript names a `&mut` rather than a value
/// read. A target that is the subscript itself is excluded: `IndexAssign`
/// writes into that one.
fn projected_subscripts_of_place(target: &ast::Expr) -> Vec<AstId> {
    place_bases(target).filter_map(subscript_id).collect()
}

/// The expressions a place projects from, innermost last: `o[0][0]`, `o[0]`
/// and `o` for `o[0][0].n`, ending at the first that projects from nothing.
fn place_bases(target: &ast::Expr) -> impl Iterator<Item = &ast::Expr> {
    fn base_of(place: &ast::Expr) -> Option<&ast::Expr> {
        match place {
            ast::Expr::FieldAccess(field) => Some(&field.expr),
            ast::Expr::Index(index) => Some(&index.expr),
            _ => None,
        }
    }
    std::iter::successors(base_of(target), |&at| base_of(at))
}

fn subscript_id(expr: &ast::Expr) -> Option<AstId> {
    match expr {
        ast::Expr::Index(index) => Some(index.id),
        _ => None,
    }
}
