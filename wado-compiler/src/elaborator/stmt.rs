//! Statement resolution (let, return, if, loop, break, continue, etc.).

use crate::ast::{
    self, AstId, Block, BreakStmt, Condition, ConditionElement, ContinueStmt, Expr, ExprStmt,
    ForOfStmt, ForStmt, IfStmt, LetStmt, Literal, LoopStmt, Pattern, ReturnStmt, Stmt,
    TaskReturnStmt, Type, WhileStmt,
};
use crate::compiler_host::CompilerHost;
use crate::tir::{
    PrimitiveType, ResolvedType, TirExpr, TirExprKind, TirPattern, TypeId, TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::typecheck::{TypeCheckResult, check_assignable};
use super::types::{FunctionContext, TypeError};
use super::util;
use super::util::placeholder;

/// Tracks the reference binding mode for match ergonomics.
/// When matching a reference-typed scrutinee, bindings inherit the reference kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefBinding {
    None,
    Ref,
    MutRef,
}

/// Variables a pattern binds into the function context, as `(name,
/// local_index, type_id)` triples in declaration (pre-order). The combined
/// walk's pattern resolvers return these instead of a `TirPattern`: reify
/// rebuilds the real pattern node independently, so the only thing the walk
/// must surface is the binding set (used by or-pattern validation).
type PatBindings = Vec<(String, u32, TypeId)>;

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Walk a block for its fact-recording side effects (Stage 7-B:
    /// records-only). reify rebuilds the `TirBlock` from the AST; this walk
    /// resolves each statement (recording types / dispatch / desugar facts and
    /// emitting diagnostics) and manages the lexical scope. `expected_type` is
    /// still propagated to the trailing statement so the coercion fact lands.
    pub(super) fn resolve_block(
        &mut self,
        block: &Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) {
        ctx.enter_scope();
        let len = block.stmts.len();
        for (i, s) in block.stmts.iter().enumerate() {
            // Propagate expected type to the last expression/statement for coercion
            if expected_type.is_some() && i == len - 1 {
                if let Stmt::Expr(expr_stmt) = s {
                    self.resolve_expr(&expr_stmt.expr, ctx, expected_type);
                    continue;
                }
                if let Stmt::If(if_stmt) = s {
                    self.resolve_if_stmt_with_expected(if_stmt, ctx, expected_type);
                    continue;
                }
                if let Stmt::Match(match_expr) = s {
                    let ty = self.resolve_match_expr(match_expr, ctx, expected_type);
                    // `resolve_match_expr` does not go through the
                    // `resolve_expr` wrapper; record the type explicitly (as
                    // the stmt-position arm does) so `ast_block_result_type`
                    // can read a trailing match's value type.
                    self.record_expression_type(match_expr.id, ty);
                    continue;
                }
                if let Stmt::LabeledBlock(labeled_block) = s {
                    self.resolve_labeled_block_with_expected(labeled_block, ctx, expected_type);
                    continue;
                }
            }
            self.resolve_stmt(s, ctx);
        }
        ctx.exit_scope();
    }

    /// Resolve a statement for its facts (Stage 7-B: records-only). reify
    /// rebuilds the `TirStmt`(s) from the AST; desugared constructs that expand
    /// to multiple statements record their `DesugarKind` tag here.
    pub(super) fn resolve_stmt(&mut self, stmt: &Stmt, ctx: &mut FunctionContext) {
        match stmt {
            Stmt::Let(let_stmt) => self.resolve_let(let_stmt, ctx),
            Stmt::Expr(expr_stmt) => self.resolve_expr_stmt(expr_stmt, ctx),
            Stmt::Return(ret_stmt) => self.resolve_return(ret_stmt, ctx),
            Stmt::TaskReturn(tr_stmt) => self.resolve_task_return(tr_stmt, ctx),
            Stmt::If(if_stmt) => self.resolve_if_stmt(if_stmt, ctx),
            Stmt::While(while_stmt) => self.resolve_while(while_stmt, ctx),
            Stmt::For(for_stmt) => self.resolve_for(for_stmt, ctx),
            Stmt::ForOf(for_of) => self.resolve_for_of(for_of, ctx),
            Stmt::Loop(loop_stmt) => self.resolve_loop(loop_stmt, ctx),
            Stmt::Match(match_expr) => {
                // A `match` in statement position discards its result, so pin
                // the expected type to `Unit` (the WIR builder drops each arm
                // body's value). Record the resolved type explicitly because
                // `resolve_match_expr` does not go through the `resolve_expr`
                // wrapper.
                let ty = self.resolve_match_expr(match_expr, ctx, Some(TypeTable::UNIT));
                self.record_expression_type(match_expr.id, ty);
            }
            Stmt::Break(break_stmt) => self.resolve_break(break_stmt, ctx),
            Stmt::Continue(continue_stmt) => self.resolve_continue(continue_stmt, ctx),
            Stmt::Assert(a) => self.desugar_assert(a, ctx),
            Stmt::LabeledBlock(labeled_block) => self.resolve_labeled_block(labeled_block, ctx),
            // Parser error-recovery placeholder: the syntax error was already
            // reported, so there is nothing to record.
            Stmt::Error(_) => {}
        }
    }

    /// Resolve a labeled block statement (Stage 7-B: records-only).
    pub(super) fn resolve_labeled_block(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
    ) {
        self.resolve_labeled_block_with_expected(labeled_block, ctx, None);
    }

    fn resolve_labeled_block_with_expected(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) {
        ctx.active_labels.push(labeled_block.label.clone());
        // resolve_block already handles scope entry/exit
        self.resolve_block(&labeled_block.block, ctx, expected_type);
        ctx.active_labels.pop();
    }

    /// Resolve a let statement (Stage 7-B: records-only).
    pub(super) fn resolve_let(&mut self, let_stmt: &LetStmt, ctx: &mut FunctionContext) {
        // Handle uninitialized declaration: `let x: T;` (no initializer)
        if let_stmt.value.is_none() {
            self.resolve_uninit_let(let_stmt, ctx);
            return;
        }

        // From here on `value` is guaranteed to be Some.
        let ast_value = let_stmt.value.as_ref().unwrap();

        // Check for tuple literal to array coercion when type annotation is present
        let (value_type, type_id) = if let Some(annotated_type) = &let_stmt.ty {
            let target_type = self.resolve_type(annotated_type);
            // Publish the resolved whole-pattern annotation so reify reads it
            // instead of re-running `resolve_type` against the AST.
            let key = self.ann_key(let_stmt.id);
            self.sem.types.let_annotated_types.insert(key, target_type);

            // Special case: tuple literal with Tuple type annotation
            if let ast::Expr::TupleLiteral(tuple_lit) = ast_value {
                {
                    let tuple_elems = self.tysys.type_table.borrow().as_tuple(target_type);
                    if let Some(expected_elem_types) = tuple_elems {
                        for (i, elem) in tuple_lit.elements.iter().enumerate() {
                            let expected = expected_elem_types.get(i).copied();
                            let resolved = self.resolve_expr(elem, ctx, expected);
                            if let Some(expected_type) = expected {
                                self.typecheck(resolved, expected_type, elem.span());
                            }
                        }

                        // Also check length mismatch
                        if tuple_lit.elements.len() != expected_elem_types.len() {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                expected: format!(
                                    "tuple with {} elements",
                                    expected_elem_types.len()
                                ),
                                found: format!("tuple with {} elements", tuple_lit.elements.len()),
                                span: ast_value.span(),
                            });
                        }

                        (target_type, target_type)
                    } else {
                        let value_type = self.resolve_expr(ast_value, ctx, Some(target_type));
                        (value_type, target_type)
                    }
                }
            } else if let ast::Expr::StructLiteral(struct_lit) = ast_value {
                // Handle implicit struct literal: let p: Point = { x: 1, y: 2 }
                if struct_lit.name.is_none() {
                    // Check if target type is a struct
                    let target_resolved = self.tysys.type_table.borrow().get(target_type).clone();
                    if let ResolvedType::Struct {
                        name,
                        module_source,
                        ..
                    } = target_resolved
                    {
                        let struct_type = target_type;

                        let struct_field_types: Vec<(String, TypeId)> = self
                            .lookup_struct_fields(&name)
                            .map(|info| {
                                info.fields
                                    .iter()
                                    .map(|(n, t, _)| (n.clone(), *t))
                                    .collect()
                            })
                            .unwrap_or_default();

                        // Check field visibility for cross-module struct literal
                        if module_source != self.current_module_source
                            && let Some(struct_info) = self.lookup_struct_fields(&name)
                        {
                            for (fname, _, is_pub) in &struct_info.fields {
                                if !is_pub && struct_lit.fields.iter().any(|f| f.name == *fname) {
                                    let _ = self.logger.error(TypeError::PrivateFieldAccess {
                                        struct_name: name.clone(),
                                        field_name: fname.clone(),
                                        span: struct_lit.span,
                                    });
                                }
                            }
                        }

                        for field in &struct_lit.fields {
                            // Check field name exists in struct definition
                            if !struct_field_types.iter().any(|(n, _)| n == &field.name)
                                && !struct_field_types.is_empty()
                            {
                                let _ = self.logger.error(TypeError::ExtraField {
                                    struct_name: name.clone(),
                                    field_name: field.name.clone(),
                                    span: field.span,
                                });
                            }
                            let expected_field_type = struct_field_types
                                .iter()
                                .find(|(n, _)| n == &field.name)
                                .map(|(_, type_id)| *type_id);
                            self.resolve_expr(&field.value, ctx, expected_field_type);
                        }

                        // Record the implicit struct literal's mangled name
                        // (the target struct's name as the elaborator picks
                        // it up) so reify reads it from
                        // `GenericInstantiation` instead of taking the
                        // anon-literal path that expects a synthesised
                        // `__anon_{…}` name.
                        self.record_generic_instantiation_with_mangle(
                            struct_lit.id,
                            vec![],
                            struct_type,
                            Some(name),
                        );
                        (struct_type, target_type)
                    } else if let Some(coerced) =
                        self.try_coerce_struct_to_map(ast_value, ctx, target_type)
                    {
                        (coerced.type_id, target_type)
                    } else {
                        // Target type does not implement KeyValueLiteral
                        let type_name = self.tysys.type_table.borrow().type_name(target_type);
                        let _ = self.logger.error(TypeError::MissingTraitImpl {
                            type_name,
                            trait_name: "KeyValueLiteral".to_string(),
                            span: struct_lit.span,
                        });
                        let value_type = self.resolve_expr(ast_value, ctx, None);
                        (value_type, target_type)
                    }
                } else {
                    // Named struct literal - resolve normally
                    let value_type = self.resolve_expr(ast_value, ctx, Some(target_type));
                    (value_type, target_type)
                }
            } else {
                // Use expected type for numeric literal coercion
                let value_type = self.resolve_expr(ast_value, ctx, Some(target_type));
                (value_type, target_type)
            }
        } else {
            let value_type = self.resolve_expr(ast_value, ctx, None);
            (value_type, value_type)
        };

        // Type check: if type annotation is present, verify value type matches.
        // Uses direct comparison instead of typecheck() because we need to catch
        // type-param-to-concrete mismatches (e.g., `let n: i32 = x` where x: T)
        // at definition time. check_assignable defers all type param cases because
        // trait impls legitimately use TypeParam-vs-concrete (monomorphized later).
        if let_stmt.ty.is_some()
            && value_type != type_id
            && value_type != TypeTable::UNKNOWN
            && value_type != TypeTable::NEVER
        {
            // Allow null (Option<unknown>) to be assigned to Option<T>
            let is_null_to_option = {
                let type_table = self.tysys.type_table.borrow();
                type_table
                    .as_option(value_type)
                    .is_some_and(|inner| inner == TypeTable::UNKNOWN)
                    && type_table.as_option(type_id).is_some()
            };
            // Function-type compatibility is structural over params and return,
            // ignoring effects (see `typecheck::check_assignable` rule 7). This
            // lets `let c: fn() with Stdout = || { ... }` accept a closure with
            // a synthesized `fn() with []` type without a spurious mismatch,
            // while still rejecting genuine signature mismatches such as a
            // closure with the wrong arity or parameter types. `check_assignable`
            // returns `Deferred` whenever either side contains a type param
            // (rule 3), so for generic signatures like `fn(T) -> T` we fall back
            // to a direct `TypeId` comparison of params and return — that
            // accepts identical generic shapes that differ only in effects
            // without admitting any type-param-to-concrete mismatches.
            let is_compatible_fn_type = {
                let type_table = self.tysys.type_table.borrow();
                if let (
                    ResolvedType::Function {
                        params: actual_params,
                        return_type: actual_return,
                        ..
                    },
                    ResolvedType::Function {
                        params: expected_params,
                        return_type: expected_return,
                        ..
                    },
                ) = (type_table.get(value_type), type_table.get(type_id))
                {
                    match check_assignable(value_type, type_id, &type_table) {
                        TypeCheckResult::Compatible => true,
                        TypeCheckResult::Deferred => {
                            actual_params.len() == expected_params.len()
                                && actual_params
                                    .iter()
                                    .zip(expected_params.iter())
                                    .all(|(a, e)| a == e)
                                && actual_return == expected_return
                        }
                        TypeCheckResult::Incompatible => false,
                    }
                } else {
                    false
                }
            };
            if !is_null_to_option && !is_compatible_fn_type {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: self.tysys.type_table.borrow().type_name(type_id),
                    found: self.tysys.type_table.borrow().type_name(value_type),
                    span: ast_value.span(),
                });
            }
        }

        // Stage 7-B: records-only. reify rebuilds the `Let` / `LetDestructure`
        // stmt from the AST + recorded facts (`let_annotated_types`,
        // `local_types`, the binding symbols). This walk binds the pattern into
        // `ctx`, records the local symbols, registers closure defaults, and
        // ran the type-mismatch diagnostic above (the resolved `value_type`'s
        // only consumer).
        match &let_stmt.pattern {
            ast::Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | ast::Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                let is_mut =
                    let_stmt.is_mut || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                ctx.add_local(name.clone(), type_id, is_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, is_mut, type_id);
                let mut closure_candidate = ast_value;
                while let ast::Expr::Unary(u) = closure_candidate {
                    closure_candidate = &u.expr;
                }
                if let ast::Expr::Closure(closure) = closure_candidate {
                    let defaults: Vec<(String, Option<ast::Expr>)> = closure
                        .params
                        .iter()
                        .map(|p| (p.name.clone(), p.default.clone()))
                        .collect();
                    if defaults.iter().any(|(_, d)| d.is_some()) {
                        ctx.closure_defaults.insert(name.clone(), defaults);
                    }
                }
            }
            ast::Pattern::Tuple(_, _) | ast::Pattern::Struct { .. } => {
                // Tuple / struct destructuring: binds the sub-patterns into
                // `ctx` and records their local symbols.
                self.resolve_let_pattern(
                    &let_stmt.pattern,
                    type_id,
                    let_stmt.is_mut,
                    let_stmt.span,
                    ctx,
                );
            }
            ast::Pattern::Wildcard => {
                // `let _ = expr;` — value evaluated for side effects, result
                // discarded; nothing to bind.
            }
            _ => {
                self.check_irrefutable_pattern(&let_stmt.pattern, let_stmt.span);
            }
        }
    }

    /// Resolve an uninitialized let declaration: `let x: T;`
    ///
    /// Emits a `TirStmtKind::Let` with a unit placeholder value so that
    /// the local is pre-allocated (Wasm zero-initializes locals) without
    /// emitting a `LocalSet`.  The bind phase has already verified that the
    /// variable is assigned before any use.
    fn resolve_uninit_let(&mut self, let_stmt: &LetStmt, ctx: &mut FunctionContext) {
        // Type annotation is guaranteed by the parser when there is no initializer.
        let type_id = self.resolve_type(
            let_stmt
                .ty
                .as_ref()
                .expect("parser ensures type annotation for uninit let"),
        );

        // Stage 7-B: records-only. reify rebuilds the pre-declared `Let` (with
        // its unit placeholder value) from the AST; this walk only binds the
        // local and records its symbol.
        match &let_stmt.pattern {
            ast::Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | ast::Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                let is_mut =
                    let_stmt.is_mut || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                ctx.add_local(name.clone(), type_id, is_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, is_mut, type_id);
            }
            _ => {
                self.check_irrefutable_pattern(&let_stmt.pattern, let_stmt.span);
            }
        }
    }

    /// Check that a pattern is irrefutable (guaranteed to match).
    ///
    /// Irrefutable patterns bind variables unconditionally and are valid in `let` bindings
    /// and `for` loops. Refutable patterns may fail to match and are only valid in `match`
    /// arms, `if let`, and `while let`.
    ///
    /// Emits a compile error and returns `false` if the pattern is refutable.
    fn check_irrefutable_pattern(&mut self, pattern: &ast::Pattern, span: Span) -> bool {
        match pattern {
            // `Pattern::Error` is a parser recovery placeholder; treat it as
            // irrefutable so it does not cascade a second "refutable" error.
            Pattern::Ident { .. }
            | Pattern::MutIdent { .. }
            | Pattern::Wildcard
            | Pattern::Error(_) => true,
            Pattern::Tuple(patterns, _) => patterns
                .iter()
                .all(|p| self.check_irrefutable_pattern(p, span)),
            Pattern::Struct { fields, .. } => fields
                .iter()
                .all(|f| self.check_irrefutable_pattern(&f.pattern, span)),
            Pattern::Literal(_) => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "refutable pattern in `let` binding: literal patterns may not match; use `if let` instead".to_string(),
                    span,
                });
                false
            }
            Pattern::Variant { variant_name, .. } => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!("refutable pattern in `let` binding: `{variant_name}` may not match; use `if let` instead"),
                    span,
                });
                false
            }
            Pattern::Or(_) => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "refutable pattern in `let` binding: or-patterns may not match; use `if let` or `match` instead".to_string(),
                    span,
                });
                false
            }
            Pattern::Range { .. } => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "refutable pattern in `let` binding: range patterns may not match; use `if let` instead".to_string(),
                    span,
                });
                false
            }
        }
    }

    pub(super) fn is_known_case_of_type(
        &mut self,
        type_id: TypeId,
        case_name: &str,
        qualifier: Option<&Type>,
    ) -> bool {
        if !self.pattern_qualifier_matches_scrutinee(type_id, qualifier) {
            return false;
        }
        let resolved = self.tysys.type_table.borrow().get(type_id).clone();
        match &resolved {
            ResolvedType::Enum { name, .. } => self
                .lookup_enum_case(name)
                .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name)),
            ResolvedType::Variant { name, .. } => self
                .lookup_variant_case(name)
                .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name)),
            ResolvedType::GenericInstance { name, .. } => self
                .lookup_variant_case(name)
                .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name)),
            _ => false,
        }
    }

    pub(super) fn pattern_qualifier_matches_scrutinee(
        &mut self,
        scrutinee_type: TypeId,
        qualifier: Option<&Type>,
    ) -> bool {
        let Some(qualifier) = qualifier else {
            return true;
        };
        let scrutinee_resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        let (scrutinee_name, scrutinee_module, scrutinee_arg_len) = match &scrutinee_resolved {
            ResolvedType::Enum {
                name,
                module_source,
            }
            | ResolvedType::Variant {
                name,
                module_source,
            } => (name.as_str(), module_source, None),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (name.as_str(), module_source, Some(type_args.len())),
            _ => {
                return false;
            }
        };
        match qualifier {
            Type::Named(t) => t.name == scrutinee_name,
            Type::Generic(g) => {
                g.name == scrutinee_name && scrutinee_arg_len.is_none_or(|n| n == g.args.len())
            }
            Type::NamespacedGeneric(ns) => {
                ns.name == scrutinee_name
                    && scrutinee_arg_len.is_none_or(|n| n == ns.args.len())
                    && self
                        .sem
                        .imports
                        .namespace_imports
                        .get(&ns.namespace)
                        .is_some_and(|m| m == scrutinee_module)
            }
            Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _)
            | Type::Error(_) => false,
        }
    }

    fn format_pattern_case_name(&self, case_name: &str, qualifier: Option<&Type>) -> String {
        let Some(qualifier) = qualifier else {
            return case_name.to_string();
        };
        format!("{}::{case_name}", format_pattern_qualifier_type(qualifier))
    }

    /// Build the lookup key for `associated_constants`, matching how the map was
    /// populated via `get_type_name` (base name only, no generic type arguments).
    ///
    /// For example, `Maybe<i32>::CONST` must look up as `"Maybe::CONST"` because
    /// `get_type_name` strips generic args when building the key.
    fn format_assoc_const_key(variant_name: &str, qualifier: Option<&Type>) -> String {
        let Some(qualifier) = qualifier else {
            return variant_name.to_string();
        };
        let base = match qualifier {
            Type::Named(t) => t.name.as_str(),
            Type::Generic(t) => t.name.as_str(),
            Type::NamespacedGeneric(t) => t.name.as_str(),
            Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _)
            | Type::Error(_) => return variant_name.to_string(),
        };
        format!("{base}::{variant_name}")
    }

    /// Resolve a let pattern (for tuple/struct destructuring).
    /// Applies match ergonomics: if `type_id` is `&T` or `&mut T` and the pattern is
    /// a compound pattern (tuple/struct), peels the reference and wraps bindings.
    pub(super) fn resolve_let_pattern(
        &mut self,
        pattern: &ast::Pattern,
        type_id: TypeId,
        is_mut: bool,
        span: Span,
        ctx: &mut FunctionContext,
    ) {
        // Match ergonomics for let patterns: peel references from the type
        // when the pattern is a compound (tuple/struct) pattern.
        let (peeled_type, ref_binding) = match pattern {
            ast::Pattern::Tuple(_, _) | ast::Pattern::Struct { .. } => {
                let mut current = type_id;
                let mut rb = RefBinding::None;
                while let resolved @ (ResolvedType::Ref(_) | ResolvedType::MutRef(_)) =
                    self.tysys.type_table.borrow().get(current).clone()
                {
                    match resolved {
                        ResolvedType::Ref(inner) => {
                            current = inner;
                            rb = RefBinding::Ref;
                        }
                        ResolvedType::MutRef(inner) => {
                            current = inner;
                            if rb == RefBinding::None {
                                rb = RefBinding::MutRef;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                (current, rb)
            }
            _ => (type_id, RefBinding::None),
        };
        self.resolve_let_pattern_inner(pattern, peeled_type, is_mut, span, ctx, ref_binding);
    }

    fn resolve_let_pattern_inner(
        &mut self,
        pattern: &ast::Pattern,
        type_id: TypeId,
        is_mut: bool,
        span: Span,
        ctx: &mut FunctionContext,
        ref_binding: RefBinding,
    ) {
        match pattern {
            ast::Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | ast::Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                let pat_mut = is_mut || matches!(pattern, ast::Pattern::MutIdent { .. });
                let binding_type = match ref_binding {
                    RefBinding::Ref => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(type_id)),
                    RefBinding::MutRef => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::MutRef(type_id)),
                    RefBinding::None => type_id,
                };
                ctx.add_local(name.clone(), binding_type, pat_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, pat_mut, binding_type);
            }
            ast::Pattern::Tuple(patterns, has_rest) => {
                // Get element types from the tuple type
                let elem_types = {
                    let type_table = self.tysys.type_table.borrow();
                    if let Some(elem_types) = type_table.as_tuple(type_id) {
                        elem_types
                    } else {
                        // Error: expected tuple type
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: "tuple type".to_string(),
                            found: type_table.type_name(type_id),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    }
                };

                // Check length
                if *has_rest {
                    if patterns.len() > elem_types.len() {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: format!("tuple with at least {} elements", patterns.len()),
                            found: format!("tuple with {} elements", elem_types.len()),
                            span,
                        });
                    }
                } else if patterns.len() != elem_types.len() {
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected: format!("tuple with {} elements", elem_types.len()),
                        found: format!("pattern with {} elements", patterns.len()),
                        span,
                    });
                }

                // Resolve each sub-pattern with its corresponding element type
                for (p, &elem_type) in patterns.iter().zip(
                    elem_types
                        .iter()
                        .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                ) {
                    self.resolve_let_pattern_inner(p, elem_type, is_mut, span, ctx, ref_binding);
                }
            }
            ast::Pattern::Struct {
                type_name,
                fields,
                has_rest,
                span: pat_span,
            } => {
                // Get struct name from type
                let struct_name = {
                    let type_table = self.tysys.type_table.borrow();
                    match type_table.get(type_id) {
                        ResolvedType::Struct { name, .. } => Some(name.clone()),
                        _ => None,
                    }
                };

                // If named pattern, verify the type matches
                if let Some(expected_name) = type_name
                    && let Some(actual_name) = &struct_name
                {
                    // Compare the short name (strip module prefix if needed,
                    // and any `<ns>::` namespace-import prefix the user wrote).
                    let expected_short = self
                        .strip_ns_prefix(expected_name)
                        .unwrap_or(expected_name.as_str());
                    let actual_short = actual_name.rsplit("::").next().unwrap_or(actual_name);
                    if actual_short != expected_short {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: expected_name.clone(),
                            found: self.tysys.type_table.borrow().type_name(type_id),
                            span: *pat_span,
                        });
                    }
                }

                if struct_name.is_none() {
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected: "struct type".to_string(),
                        found: self.tysys.type_table.borrow().type_name(type_id),
                        span: *pat_span,
                    });
                    return;
                }

                // Resolve each field pattern
                for field in fields {
                    let (_field_index, field_type) =
                        self.lookup_field_type(type_id, &field.field_name, field.span);
                    self.resolve_let_pattern_inner(
                        &field.pattern,
                        field_type,
                        is_mut,
                        field.span,
                        ctx,
                        ref_binding,
                    );
                }

                // Exhaustiveness check: without `..`, all fields must be listed
                if !has_rest
                    && let Some(ref sname) = struct_name
                    && let Some(struct_info) = self.lookup_struct_fields(sname)
                {
                    let total_fields = struct_info.fields.len();
                    if fields.len() != total_fields {
                        let missing: Vec<_> = struct_info
                            .fields
                            .iter()
                            .filter(|(name, _, _)| !fields.iter().any(|f| f.field_name == *name))
                            .map(|(name, _, _)| name.clone())
                            .collect();
                        if !missing.is_empty() {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                        expected: format!(
                                            "all fields (missing: {}), or use `..` to ignore remaining fields",
                                            missing.join(", ")
                                        ),
                                        found: format!(
                                            "pattern with {} of {} fields",
                                            fields.len(),
                                            total_fields
                                        ),
                                        span: *pat_span,
                                    });
                        }
                    }
                }

                let _ = has_rest;
            }
            // Wildcard binds nothing. Refutable patterns (literal / variant /
            // or / range) in let position already had an error emitted by
            // `check_irrefutable_pattern`; the parser error placeholder is
            // inert. None of them introduce a binding here.
            ast::Pattern::Wildcard
            | ast::Pattern::Literal(_)
            | ast::Pattern::Variant { .. }
            | ast::Pattern::Or(_)
            | ast::Pattern::Range { .. }
            | ast::Pattern::Error(_) => {}
        }
    }

    /// Resolve an expression statement
    pub(super) fn resolve_expr_stmt(&mut self, expr_stmt: &ExprStmt, ctx: &mut FunctionContext) {
        // Stage 7-B: records-only; reify rebuilds the `Expr` stmt.
        self.resolve_expr(&expr_stmt.expr, ctx, None);
    }

    /// Resolve a return statement (Stage 7-B: records-only).
    pub(super) fn resolve_return(&mut self, ret_stmt: &ReturnStmt, ctx: &mut FunctionContext) {
        // In async functions, `return expr` (with a value) is forbidden; use `task return expr`
        if ctx.is_async && ret_stmt.value.is_some() {
            let _ = self.logger.error(TypeError::InvalidLiteral {
                message:
                    "cannot use `return expr` in `export async fn`; use `task return expr` instead"
                        .to_string(),
                span: ret_stmt.span,
            });
        }
        let return_type = ctx.return_type;
        // Use expected type for coercion (numeric literals, tuple to array,
        // etc.) and check the value type against the function return type.
        if let Some(expr) = ret_stmt.value.as_ref() {
            let value_type = self.resolve_expr(expr, ctx, Some(return_type));
            self.typecheck_return(value_type, return_type, ret_stmt.span);
        }
    }

    /// Resolve a `task return` statement (Stage 7-B: records-only).
    pub(super) fn resolve_task_return(
        &mut self,
        tr_stmt: &TaskReturnStmt,
        ctx: &mut FunctionContext,
    ) {
        if !ctx.is_async {
            let _ = self.logger.error(TypeError::InvalidLiteral {
                message: "`task return` is only valid inside `export async fn`".to_string(),
                span: tr_stmt.span,
            });
        }
        let expected = ctx.task_return_type;
        self.resolve_expr(&tr_stmt.value, ctx, expected);
    }

    /// Resolve an if statement (Stage 7-B: records-only). reify rebuilds the
    /// `If` / if-let-chain TIR from the AST + the `DesugarKind::IfLetChain`
    /// tag; this walk only resolves the condition and blocks for their facts.
    pub(super) fn resolve_if_stmt(&mut self, if_stmt: &IfStmt, ctx: &mut FunctionContext) {
        self.resolve_if_stmt_with_expected(if_stmt, ctx, None);
    }

    /// Like `resolve_if_stmt` but propagates `expected_type` to blocks for
    /// coercion. Used when an if statement is the last statement in a block
    /// that needs type coercion (e.g., a match arm returning `List<T>` from an
    /// if-else with tuple literals). Stage 7-B: records-only.
    fn resolve_if_stmt_with_expected(
        &mut self,
        if_stmt: &IfStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) {
        match &if_stmt.condition {
            ast::Condition::Expr(expr) => {
                self.resolve_expr(expr, ctx, Some(TypeTable::BOOL));
                self.resolve_block(&if_stmt.then_block, ctx, expected_type);
                if let Some(b) = &if_stmt.else_block {
                    self.resolve_block(b, ctx, expected_type);
                }
            }
            ast::Condition::LetChain { elements, .. } => {
                self.record_desugar(if_stmt.id, super::sem::types::DesugarKind::IfLetChain);
                // Resolve else_block in the outer scope (chain bindings are not
                // visible there) for its facts.
                if let Some(b) = &if_stmt.else_block {
                    self.resolve_block(b, ctx, expected_type);
                }

                // Enter scope for chain element bindings and then_block.
                ctx.enter_scope();
                self.resolve_let_chain_stmts(
                    elements,
                    &if_stmt.then_block,
                    ctx,
                    expected_type,
                    if_stmt.span,
                );
                ctx.exit_scope();
            }
        }
    }

    /// Resolve a let-chain condition into a nested sequence of TIR statements.
    ///
    /// Each element of the chain adds one nesting level: a `Let` element becomes a
    /// two-arm `Match` expression statement (the pattern arm vs. a wildcard else
    /// arm; the scrutinee stays inline and is hoisted into a temp local later, in
    /// `translate::pattern`), and an `Expr` element becomes an `If` node (boolean
    /// guard). All levels that fail fall through to `else_block`; the innermost
    /// level runs `then_block`.
    ///
    /// The `else_block` TIR is cloned for each failure path. This duplicates else-block code
    /// in the output, but is typically small (e.g., `None` or a single `panic` call).
    ///
    /// Stage 7-B: records-only. The combined walk no longer builds the
    /// normalized `Match` / `If` chain TIR (reify rebuilds it from the
    /// `DesugarKind::IfLetChain` tag + the AST); this walk only resolves the
    /// scrutinees / conditions for their facts, binds the patterns into `ctx`,
    /// and recurses into the then-block. The else-block is resolved once by
    /// the caller (in the outer scope), so it is not threaded here.
    pub(super) fn resolve_let_chain_stmts(
        &mut self,
        elements: &[ConditionElement],
        then_block_ast: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        span: Span,
    ) {
        if elements.is_empty() {
            self.resolve_block(then_block_ast, ctx, expected_type);
            return;
        }
        // Process the current element first so its bindings are visible when
        // resolving subsequent elements and the then_block (via the recursion
        // below).
        match &elements[0] {
            ConditionElement::Let {
                pattern,
                expr,
                span: elem_span,
            } => {
                let scrutinee_type = self.resolve_expr(expr, ctx, None);
                // Adds pattern bindings to ctx — subsequent elements can see them.
                self.resolve_if_pattern(pattern, scrutinee_type, ctx, *elem_span);
                self.resolve_let_chain_stmts(
                    &elements[1..],
                    then_block_ast,
                    ctx,
                    expected_type,
                    span,
                );
            }
            ConditionElement::Expr(expr) => {
                self.resolve_expr(expr, ctx, Some(TypeTable::BOOL));
                self.resolve_let_chain_stmts(
                    &elements[1..],
                    then_block_ast,
                    ctx,
                    expected_type,
                    span,
                );
            }
        }
    }

    /// Resolve a pattern in an if-pattern context with type information from the scrutinee.
    /// Match ergonomics: if the scrutinee is `&T`, peels the reference and propagates
    /// `ref_binding` so that identifier bindings get `&InnerType` instead of `InnerType`.
    /// Bind a refutable pattern's variables into `ctx` and run the same
    /// disambiguation / diagnostics as reify's pattern builder, returning the
    /// bindings it introduced in declaration (pre-order). The combined walk
    /// only needs the binding side effects and facts — reify rebuilds the real
    /// `TirPattern` independently — so no `TirPattern` node is assembled here.
    pub(super) fn resolve_if_pattern(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> PatBindings {
        let mut peeled_type = scrutinee_type;
        let mut ref_binding = RefBinding::None;
        while let resolved @ (ResolvedType::Ref(_) | ResolvedType::MutRef(_)) =
            self.tysys.type_table.borrow().get(peeled_type).clone()
        {
            match resolved {
                ResolvedType::Ref(inner) => {
                    peeled_type = inner;
                    // &T always downgrades to Ref (most restrictive wins)
                    ref_binding = RefBinding::Ref;
                }
                ResolvedType::MutRef(inner) => {
                    peeled_type = inner;
                    // &mut T only sets MutRef if not already downgraded to Ref
                    if ref_binding == RefBinding::None {
                        ref_binding = RefBinding::MutRef;
                    }
                }
                _ => unreachable!(),
            }
        }
        self.resolve_if_pattern_inner(pattern, peeled_type, ctx, span, ref_binding)
    }

    fn resolve_if_pattern_inner(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        span: Span,
        ref_binding: RefBinding,
    ) -> PatBindings {
        match pattern {
            Pattern::Wildcard => Vec::new(),
            Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                // A bare identifier in a pattern context could be a variant/enum case
                // (e.g., `None`, `Red`) or a variable binding (e.g., `x`, `val`).
                // The parser does not use case to disambiguate; instead, we check
                // whether the name is a known case of the scrutinee type.
                if !matches!(pattern, Pattern::MutIdent { .. })
                    && self.is_known_case_of_type(scrutinee_type, name, None)
                {
                    // Delegate to the Variant branch with empty bindings.
                    // Preserve the identifier's AstId/span as name_id/name_span so
                    // LSP jump-to-def on `None`/`Red` still resolves to the case decl.
                    return self.resolve_if_pattern_inner(
                        &Pattern::Variant {
                            variant_name: name.clone(),
                            variant_qualifier: None,
                            name_id: Some(*id),
                            name_span: *name_span,
                            bindings: vec![],
                            span,
                        },
                        scrutinee_type,
                        ctx,
                        span,
                        ref_binding,
                    );
                }
                // Check if the identifier refers to an immutable global constant
                if !matches!(pattern, Pattern::MutIdent { .. }) {
                    if let Some(&(_ty, mutable)) = self.sem.decls.current_module_globals.get(name)
                        && !mutable
                    {
                        // Constant-value pattern: introduces no binding.
                        return Vec::new();
                    }
                    if let Some((_source_module, _original_name, _ty, mutable)) =
                        self.sem.decls.imported_globals.get(name)
                        && !*mutable
                    {
                        // Constant-value pattern: introduces no binding.
                        return Vec::new();
                    }
                }
                let is_mut = matches!(pattern, Pattern::MutIdent { .. });
                let binding_type = match ref_binding {
                    RefBinding::Ref => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(scrutinee_type)),
                    RefBinding::MutRef => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::MutRef(scrutinee_type)),
                    RefBinding::None => scrutinee_type,
                };
                let index = ctx.add_local(name.clone(), binding_type, is_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, is_mut, binding_type);
                vec![(name.clone(), index, binding_type)]
            }
            Pattern::Literal(lit) => {
                match lit {
                    Literal::Number(repr) => {
                        // Float literals cannot be used in match patterns
                        if util::is_float_only_literal(repr) {
                            let _ = self.logger.error(TypeError::InvalidPattern {
                                message: "float literals cannot be used in match patterns"
                                    .to_string(),
                                span,
                            });
                        }
                    }
                    Literal::Null => {
                        // If the scrutinee is a variant type with a `None` case,
                        // `null` lowers to a `None` variant pattern (no binding).
                        let _ = self.try_null_as_none_pattern(scrutinee_type);
                    }
                    _ => {}
                }
                Vec::new()
            }
            Pattern::Tuple(patterns, has_rest) => {
                // For tuple patterns, extract element types
                let element_types =
                    if let Some(types) = self.tysys.type_table.borrow().as_tuple(scrutinee_type) {
                        types
                    } else {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: "tuple type".to_string(),
                            found: self.tysys.type_table.borrow().type_name(scrutinee_type),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    };

                let _ = has_rest;
                let mut bindings: PatBindings = Vec::new();
                for (p, &ty) in patterns.iter().zip(
                    element_types
                        .iter()
                        .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                ) {
                    bindings.extend(self.resolve_if_pattern_inner(p, ty, ctx, span, ref_binding));
                }
                bindings
            }
            Pattern::Variant {
                variant_name,
                variant_qualifier,
                name_id,
                name_span: _,
                bindings,
                span,
            } => {
                // `<ns>::<Case>` (single `::`, prefix is a namespace import
                // alias) canonicalizes to the bare `<Case>`; the registries
                // below are keyed by canonical names. Multi-segment forms
                // (`<ns>::<Type>::<case>`) reach pattern resolution as a
                // `variant_qualifier` Type, not embedded in `variant_name`.
                let normalized_variant_name = self
                    .strip_ns_prefix(variant_name)
                    .unwrap_or(variant_name.as_str());
                let qualified_variant_name =
                    self.format_pattern_case_name(variant_name, variant_qualifier.as_ref());
                // Bare uppercase identifier that is not a known case of the scrutinee type.
                // Check if it's an associated constant (e.g., `i32::MAX`) before falling back
                // to a variable binding.
                if bindings.is_empty()
                    && !self.is_known_case_of_type(
                        scrutinee_type,
                        normalized_variant_name,
                        variant_qualifier.as_ref(),
                    )
                {
                    // Check for associated constants (e.g., `i32::MAX`, `f64::PI`).
                    // Use the base type name (no generic args) to match how
                    // `associated_constants` keys are built via `get_type_name`.
                    let assoc_const_key =
                        Self::format_assoc_const_key(variant_name, variant_qualifier.as_ref());
                    // Resolve to literal patterns when possible for switch optimization.
                    if let Some((_const_module, type_id, const_expr)) = self
                        .sem
                        .decls
                        .associated_constants
                        .get(&assoc_const_key)
                        .cloned()
                    {
                        // Resolve for side effects (records the const body's
                        // types for reify). Stage 7-B: `resolve_literal` is a
                        // placeholder, so classify the Literal-vs-ConstantValue
                        // pattern from the const body AST rather than the
                        // resolved value's kind. A literal body becomes a
                        // `Literal` pattern (switch optimization + exhaustiveness);
                        // anything else is an opaque `ConstantValue`.
                        // Resolve the const body for its facts. An associated
                        // constant introduces no binding (it is either a literal
                        // or an opaque constant-value pattern), so return no
                        // bindings either way.
                        self.resolve_expr(&const_expr, ctx, Some(type_id));
                        return Vec::new();
                    }

                    let binding_type = match ref_binding {
                        RefBinding::Ref => self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(scrutinee_type)),
                        RefBinding::MutRef => self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::MutRef(scrutinee_type)),
                        RefBinding::None => scrutinee_type,
                    };
                    let index =
                        ctx.add_local(qualified_variant_name.clone(), binding_type, false, None);
                    return vec![(qualified_variant_name, index, binding_type)];
                }

                let resolved_type = self.tysys.type_table.borrow().get(scrutinee_type).clone();
                if !self
                    .pattern_qualifier_matches_scrutinee(scrutinee_type, variant_qualifier.as_ref())
                {
                    let expected = match &resolved_type {
                        ResolvedType::Enum { name, .. } => format!("valid case of enum {name}"),
                        ResolvedType::Variant { name, .. }
                        | ResolvedType::GenericInstance { name, .. } => {
                            format!("valid case of variant {name}")
                        }
                        _ => "variant or enum case".to_string(),
                    };
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected,
                        found: qualified_variant_name,
                        span: *span,
                    });
                    return Vec::new();
                }

                // Handle enum types (no payload, just discriminant matching)
                if let ResolvedType::Enum { name, .. } = &resolved_type {
                    if !bindings.is_empty() {
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!("enum case `{variant_name}` does not have a payload"),
                            span: *span,
                        });
                    }
                    // Look up the enum case index
                    if let Some(enum_info) = self.lookup_enum_case(name).cloned() {
                        if let Some(case_data) =
                            enum_info.find_case(normalized_variant_name).cloned()
                        {
                            // Record pattern's case-name identifier -> enum case decl
                            if let Some(id) = name_id {
                                self.record_reference_to_decl(
                                    *id,
                                    &enum_info.module_source,
                                    case_data.ast_id,
                                );
                            }
                            // Enum case carries no payload — no binding.
                            return Vec::new();
                        }
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: format!(
                                "one of: {}",
                                enum_info
                                    .cases
                                    .iter()
                                    .map(|c| c.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            found: self
                                .format_pattern_case_name(variant_name, variant_qualifier.as_ref()),
                            span: *span,
                        });
                        return Vec::new();
                    }
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected: format!("enum type `{name}`"),
                        found: "unknown enum".to_string(),
                        span: *span,
                    });
                    return Vec::new();
                }

                // Record use->def for the variant case name in the pattern
                // (e.g., `Some` in `Some(x)`). Points at the case declaration's
                // span so LSP jump-to-def from the pattern lands on the case decl.
                let variant_type_name: Option<String> = match &resolved_type {
                    ResolvedType::Variant { name, .. } => Some(name.clone()),
                    ResolvedType::GenericInstance { name, .. } if self.contains_variant(name) => {
                        Some(name.clone())
                    }
                    _ => None,
                };
                if let Some(id) = name_id
                    && let Some(type_name) = variant_type_name.as_ref()
                    && let Some(variant_info) = self.lookup_variant_case(type_name).cloned()
                    && let Some(case_data) = variant_info
                        .cases
                        .iter()
                        .find(|c| c.name == normalized_variant_name)
                        .cloned()
                {
                    self.record_reference_to_decl(
                        *id,
                        &variant_info.module_source,
                        case_data.ast_id,
                    );
                }

                // Each variant case has exactly one payload type.
                // Determine the payload type for the variant case.
                let payload_type: TypeId = match &resolved_type {
                    // Non-generic variant
                    ResolvedType::Variant { name, .. } => self.get_variant_case_payload_type(
                        name,
                        normalized_variant_name,
                        &[],
                        *span,
                    ),
                    // Generic variant instantiation
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        // Check if this is a variant (not a struct)
                        if self.contains_variant(name) {
                            self.get_variant_case_payload_type(
                                name,
                                normalized_variant_name,
                                type_args,
                                *span,
                            )
                        } else {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                expected: "variant type".to_string(),
                                found: name.clone(),
                                span: *span,
                            });
                            TypeTable::UNKNOWN
                        }
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: "variant or enum type".to_string(),
                            found: format!("{resolved_type:?}"),
                            span: *span,
                        });
                        TypeTable::UNKNOWN
                    }
                };

                // Single payload = single binding pattern.
                // For backward compatibility, we still accept `Some(x)` as single binding.
                if bindings.len() == 1 {
                    self.resolve_if_pattern_inner(
                        &bindings[0],
                        payload_type,
                        ctx,
                        *span,
                        ref_binding,
                    )
                } else if bindings.is_empty() {
                    // Unit case like `None` - no bindings
                    Vec::new()
                } else {
                    // Multiple bindings are deprecated with single payload design.
                    // Error will be caught by test fixture updates.
                    let mut out: PatBindings = Vec::new();
                    for p in bindings {
                        out.extend(self.resolve_if_pattern_inner(
                            p,
                            TypeTable::UNKNOWN,
                            ctx,
                            *span,
                            ref_binding,
                        ));
                    }
                    out
                }
            }
            Pattern::Struct {
                type_name,
                fields,
                has_rest,
                span: pat_span,
            } => {
                // Verify type if named
                if let Some(expected_name) = type_name {
                    let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
                    if let ResolvedType::Struct { ref name, .. } = resolved {
                        let expected_short = self
                            .strip_ns_prefix(expected_name)
                            .unwrap_or(expected_name.as_str());
                        let actual_short = name.rsplit("::").next().unwrap_or(name);
                        if actual_short != expected_short {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                expected: expected_name.clone(),
                                found: self.tysys.type_table.borrow().type_name(scrutinee_type),
                                span: *pat_span,
                            });
                        }
                    }
                }

                let mut field_bindings: PatBindings = Vec::new();
                for field in fields {
                    let (_field_index, field_type) =
                        self.lookup_field_type(scrutinee_type, &field.field_name, field.span);
                    field_bindings.extend(self.resolve_if_pattern_inner(
                        &field.pattern,
                        field_type,
                        ctx,
                        field.span,
                        ref_binding,
                    ));
                }

                // Exhaustiveness check
                if !has_rest {
                    let struct_name = {
                        let type_table = self.tysys.type_table.borrow();
                        match type_table.get(scrutinee_type) {
                            ResolvedType::Struct { name, .. } => Some(name.clone()),
                            _ => None,
                        }
                    };
                    if let Some(ref sname) = struct_name
                        && let Some(struct_info) = self.lookup_struct_fields(sname)
                    {
                        let total_fields = struct_info.fields.len();
                        if fields.len() != total_fields {
                            let missing: Vec<_> = struct_info
                                .fields
                                .iter()
                                .filter(|(name, _, _)| {
                                    !fields.iter().any(|f| f.field_name == *name)
                                })
                                .map(|(name, _, _)| name.clone())
                                .collect();
                            if !missing.is_empty() {
                                let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                        expected: format!(
                                            "all fields (missing: {}), or use `..` to ignore remaining fields",
                                            missing.join(", ")
                                        ),
                                        found: format!(
                                            "pattern with {} of {} fields",
                                            fields.len(),
                                            total_fields
                                        ),
                                        span: *pat_span,
                                    });
                            }
                        }
                    }
                }

                let _ = has_rest;
                field_bindings
            }
            Pattern::Or(alternatives) => {
                // Resolve first alternative normally
                let Some(first_alt) = alternatives.first() else {
                    return Vec::new();
                };
                let mut first_bindings = self.resolve_if_pattern_inner(
                    first_alt,
                    scrutinee_type,
                    ctx,
                    span,
                    ref_binding,
                );
                // Match the old `collect_pattern_bindings_with_index` ordering
                // (sorted by name) so or-pattern validation compares stable
                // name lists across alternatives.
                first_bindings.sort_by(|a, b| a.0.cmp(&b.0));

                // Resolve subsequent alternatives and validate their bindings
                // against the first alternative's. The first alternative's
                // local indices are canonical; subsequent alternatives still
                // allocate their own locals (walk-order parity) but the scope
                // entries below are remapped to the first's.
                for (i, alt) in alternatives.iter().enumerate().skip(1) {
                    let mut alt_bindings =
                        self.resolve_if_pattern_inner(alt, scrutinee_type, ctx, span, ref_binding);
                    alt_bindings.sort_by(|a, b| a.0.cmp(&b.0));

                    // Validate same names and types
                    let first_names: Vec<(&str, crate::tir::TypeId)> = first_bindings
                        .iter()
                        .map(|(n, _, t)| (n.as_str(), *t))
                        .collect();
                    let alt_names: Vec<(&str, crate::tir::TypeId)> = alt_bindings
                        .iter()
                        .map(|(n, _, t)| (n.as_str(), *t))
                        .collect();

                    if first_names != alt_names {
                        let fn_: Vec<&str> = first_names.iter().map(|(n, _)| *n).collect();
                        let an: Vec<&str> = alt_names.iter().map(|(n, _)| *n).collect();
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!(
                                "or-pattern alternatives must bind the same names with the same types: \
                                 alternative 1 binds {:?}, but alternative {} binds {:?}",
                                fn_, i + 1, an,
                            ),
                            span,
                        });
                    }
                }

                // Update scope entries to use the first alternative's local indices
                // so the arm body resolves names to the correct locals.
                //
                // Also align each binding's `defining_ast_id` with the first
                // alternative's pattern, so that LSP jump-to-def on a use
                // inside the arm body points at the first alternative's
                // binding (the canonical definition site).
                let mut first_alt_ast_ids: crate::hashmap::IndexMap<String, AstId> =
                    crate::hashmap::IndexMap::default();
                collect_ast_pattern_binding_ids(first_alt, &mut first_alt_ast_ids);
                for (name, local_index, _type_id) in &first_bindings {
                    if let Some(scope) = ctx.scopes.last_mut()
                        && let Some(var) = scope.get_mut(name)
                    {
                        var.index = *local_index;
                        if let Some(first_id) = first_alt_ast_ids.get(name) {
                            var.defining_ast_id = Some(*first_id);
                        }
                    }
                }

                // The or-pattern's bindings are the first alternative's
                // (matching the old `collect_pattern_bindings_with_index` Or
                // handling, which collected only the first alternative).
                first_bindings
            }
            Pattern::Range {
                start,
                end,
                kind,
                span: range_span,
            } => {
                // Range patterns introduce no binding; resolve for the
                // reversed/empty-range diagnostics only.
                self.resolve_range_pattern(start, end, *kind, scrutinee_type, *range_span);
                Vec::new()
            }
            // Parser error-recovery placeholder; inert.
            Pattern::Error(_) => Vec::new(),
        }
    }

    /// True when the scrutinee is a variant type that has a `None` case, so a
    /// `null` literal pattern lowers to a `None` variant pattern (which binds
    /// nothing). Reify rebuilds the actual `None` pattern; the combined walk
    /// only needs the yes/no answer for its binding/fact walk.
    fn try_null_as_none_pattern(&self, scrutinee_type: TypeId) -> bool {
        let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        let variant_name = match &resolved {
            ResolvedType::Variant { name, .. } => name.clone(),
            ResolvedType::GenericInstance { name, .. } if self.contains_variant(name) => {
                name.clone()
            }
            _ => return false,
        };
        let Some(variant_info) = self.lookup_variant_case(&variant_name) else {
            return false;
        };
        let none_case_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .variant_case_name(crate::compiler_item::CompilerItem::OptionNone)
            .to_string();
        variant_info.cases.iter().any(|c| c.name == none_case_name)
    }

    /// Validate a range pattern (`0..<10` or `'a'..='z'`) for the combined
    /// walk, emitting the bad-bounds / reversed / empty diagnostics. Range
    /// patterns bind nothing and reify rebuilds the real `TirPattern::Range`,
    /// so no pattern node is produced here.
    fn resolve_range_pattern(
        &mut self,
        start: &Pattern,
        end: &Pattern,
        kind: crate::ast::RangeKind,
        scrutinee_type: TypeId,
        span: Span,
    ) {
        let scrutinee_resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        let is_unsigned = matches!(
            scrutinee_resolved,
            ResolvedType::Primitive(
                PrimitiveType::U8
                    | PrimitiveType::U16
                    | PrimitiveType::U32
                    | PrimitiveType::U64
                    | PrimitiveType::U128
            )
        ) || matches!(
            scrutinee_resolved,
            ResolvedType::Struct { ref name, .. } if name == "u128"
        );

        let start_val = self.pattern_to_i128(start, is_unsigned);
        let end_val = self.pattern_to_i128(end, is_unsigned);

        let (Some(start_val), Some(end_val)) = (start_val, end_val) else {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "range pattern bounds must be integer or char literals".to_string(),
                span,
            });
            return;
        };

        // Check for reversed or empty range
        let inclusive = matches!(kind, crate::ast::RangeKind::Inclusive);
        if start_val > end_val {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "reversed range pattern".to_string(),
                span,
            });
            return;
        }
        if !inclusive && start_val >= end_val {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "empty range pattern".to_string(),
                span,
            });
        }
    }

    fn pattern_to_i128(&self, pattern: &Pattern, is_unsigned: bool) -> Option<i128> {
        match pattern {
            Pattern::Literal(Literal::Number(repr)) => {
                if is_unsigned {
                    util::parse_u128_literal(repr).ok().map(|v| v as i128)
                } else {
                    util::parse_i128_literal(repr).ok()
                }
            }
            Pattern::Literal(Literal::Char(raw)) => {
                util::unescape_char(raw).ok().map(|c| c as i128)
            }
            Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                ..
            } if bindings.is_empty() => {
                // Could be an associated constant like i32::MAX, i32::MIN
                self.primitive_assoc_const_to_i128(variant_qualifier.as_ref(), variant_name)
            }
            _ => None,
        }
    }

    fn primitive_assoc_const_to_i128(
        &self,
        qualifier: Option<&Type>,
        const_name: &str,
    ) -> Option<i128> {
        primitive_assoc_const_to_i128(qualifier, const_name)
    }

    /// Get payload type for a variant case, substituting type parameters if needed
    pub(super) fn get_variant_case_payload_type(
        &mut self,
        variant_name: &str,
        case_name: &str,
        type_args: &[TypeId],
        span: Span,
    ) -> TypeId {
        // Clone payload first to avoid borrow conflict with substitute_type_params
        let payload_opt = self.lookup_variant_case(variant_name).and_then(|info| {
            info.cases
                .iter()
                .find(|case| case.name == case_name)
                .map(|case| case.payload)
        });

        if let Some(payload) = payload_opt {
            // Substitute type parameters with concrete types
            return self.substitute_type_params(payload, type_args);
        }

        // Check if variant exists but case not found
        if self.contains_variant(variant_name) {
            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                expected: format!("valid case of variant {variant_name}"),
                found: case_name.to_string(),
                span,
            });
        } else {
            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                expected: "known variant type".to_string(),
                found: variant_name.to_string(),
                span,
            });
        }
        TypeTable::UNKNOWN
    }
    /// Resolve a loop statement (infinite loop).
    ///
    /// Naked `continue` inside this loop's body must jump to the top of
    /// *this* loop, regardless of any enclosing C-style `for` whose
    /// continue-retarget label is still on the stack. Take + restore
    /// `for_continue_labels` around body resolution so the inner
    /// `resolve_continue` sees an empty stack and lowers naturally.
    pub(super) fn resolve_loop(&mut self, loop_stmt: &LoopStmt, ctx: &mut FunctionContext) {
        // Stage 7-B: records-only; reify rebuilds the `Loop` stmt.
        let saved = std::mem::take(&mut ctx.for_continue_labels);
        self.resolve_block(&loop_stmt.body, ctx, None);
        ctx.for_continue_labels = saved;
    }

    /// Resolve a for-of loop.
    ///
    /// For tuples: compile-time expansion (one copy of the body per element).
    /// For non-tuples: iterator pattern via `into_iter()` + `next()`.
    pub(super) fn resolve_for_of(&mut self, for_of: &ForOfStmt, ctx: &mut FunctionContext) {
        // Naked `continue` inside this for-of's body targets *this* loop,
        // not an enclosing C-style `for` body label. The iterable itself
        // is an expression — no `continue` stmt syntactically — but we
        // clear the stack early so all internal resolve_* calls share the
        // same invariant.
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);

        // Check if the iterable is `.enumerate()` on something.
        //
        // Stage 4 / WEP 2026-05-26 note: the `.enumerate()`
        // `MethodCallExpr` is unwrapped here at the AST level — the
        // elaborator never resolves it as a method call, so
        // `expression_types` and `method_dispatch` carry no entry for
        // `mc.id`. The future `reify` pass re-detects this pattern by
        // looking at `for_of.iterable` directly, so missing annotations
        // on `mc` are intentional (mirroring the `tuple.len()` /
        // `.zip()` short-circuits documented on `MethodDispatch`).
        let (actual_iterable, is_enumerate) = match &for_of.iterable {
            Expr::MethodCall(mc) if mc.method == "enumerate" && mc.args.is_empty() => {
                (&mc.receiver, true)
            }
            _ => (&for_of.iterable, false),
        };

        // Resolve the iterable to determine its type
        let iterable_type_id = self.resolve_expr(actual_iterable, ctx, None);
        let iterable = placeholder(iterable_type_id, actual_iterable.span());

        // Check if it's a tuple type — looking through a single `&`/`&mut`
        // wrapper. A reference iterable (`&[..T]`) iterates element-by-ref
        // (`&T_k`), mirroring `for v of &list`.
        let tuple_info = {
            let type_table = self.tysys.type_table.borrow();
            type_table
                .as_tuple_through_ref(iterable_type_id)
                .map(|(elems, by_ref)| {
                    let has_type_pack = elems
                        .iter()
                        .any(|e| matches!(type_table.get(*e), ResolvedType::TypePack { .. }));
                    (elems, has_type_pack, by_ref)
                })
        };
        // TupleZip with nested TypePacks: treat as variadic so expansion
        // is deferred to monomorphization when concrete types are known.
        // `TirExprKind::TupleZip` is produced only by the `<tuple>.zip()`
        // method arm when the receiver tuple contains a `TypePack`
        // (`method_call.rs`); a concrete-tuple `.zip()` expands inline and a
        // non-tuple receiver never yields a pack-containing result. So the AST
        // shape (a `.zip()` call) plus a pack-containing result type detect the
        // deferred form without reading the resolved `iterable.kind`.
        let is_zip_variadic = matches!(
            actual_iterable,
            Expr::MethodCall(mc) if mc.method == "zip" && mc.args.is_empty()
        ) && self.type_contains_pack(iterable_type_id);

        if let Some((elems, has_type_pack, by_ref)) = tuple_info {
            if has_type_pack || is_zip_variadic {
                assert!(
                    !is_enumerate,
                    "variadic for-of with .enumerate() is not yet supported"
                );
                self.record_desugar(for_of.id, super::sem::types::DesugarKind::ForOfVariadic);
                self.resolve_variadic_for_of(for_of, iterable, by_ref, ctx);
            } else {
                self.record_desugar(for_of.id, super::sem::types::DesugarKind::ForOfTuple);
                self.resolve_tuple_for_of(for_of, iterable, &elems, is_enumerate, by_ref, ctx);
            }
        } else {
            // Check that the iterable type implements IntoIterator
            let mut inner_type_id = iterable_type_id;
            while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                self.tysys.type_table.borrow().get(inner_type_id).clone()
            {
                inner_type_id = t;
            }
            let implements_into_iter = self.type_implements_trait(iterable_type_id, "IntoIterator")
                || self.type_implements_trait(inner_type_id, "IntoIterator")
                || matches!(
                    self.tysys.type_table.borrow().get(iterable_type_id),
                    ResolvedType::Unknown | ResolvedType::TypeParam { .. }
                );
            if !implements_into_iter {
                let type_name = self.tysys.type_table.borrow().type_name(iterable_type_id);
                let _ = self.logger.error(TypeError::MissingTraitImpl {
                    type_name,
                    trait_name: "IntoIterator".to_string(),
                    span: for_of.span,
                });
            }
            // Only record the desugar tag when the iterable actually
            // supports iteration; tagging an error-path node would lead
            // Stage 5 reify to expand a TIR shape the elaborator never
            // produced.
            if implements_into_iter {
                self.record_desugar(for_of.id, super::sem::types::DesugarKind::ForOfIterator);
            }
            self.resolve_iterator_for_of(for_of, ctx);
        }

        ctx.for_continue_labels = saved_continue;
    }

    /// Expand `for let v of tuple { body }` by unrolling the body once per element.
    ///
    /// Produces:
    /// ```text
    /// __tuple_for_of_N: {
    ///     let __tuple_N = <iterable>;
    ///     { let v = __tuple_N.0; body }
    ///     { let v = __tuple_N.1; body }
    ///     ...
    /// }
    /// ```
    /// Create a deferred `VariadicForOf` TIR node for `for let v of iterable`
    /// where `iterable` has a tuple type containing `TypePack` elements.
    ///
    /// The body is resolved once with the loop variable having the `TypePack` type.
    /// The monomorphizer will expand this after type substitution resolves the
    /// `TypePack` to a concrete tuple.
    fn resolve_variadic_for_of(
        &mut self,
        for_of: &ForOfStmt,
        iterable: TirExpr,
        by_ref: bool,
        ctx: &mut FunctionContext,
    ) {
        // Validate: no break/continue/return in variadic for-of
        if let Some((kind, bad_span)) = Self::find_control_flow_in_block(&for_of.body) {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: format!(
                    "`{kind}` is not allowed inside a variadic for-of loop (the loop is expanded at compile time)"
                ),
                span: bad_span,
            });
            return;
        }

        let unique_id = ctx.next_local;

        // Extract the element type for the loop binding.
        // For direct TypePack: iterable is Tuple([TypePack{T}]), binding type is TypePack.
        // For TupleZip: iterable is Tuple([Tuple([TypePack, TypePack])]), binding type is the inner tuple.
        let binding_type = {
            let inner = {
                let type_table = self.tysys.type_table.borrow();
                let (elems, _) = type_table
                    .as_tuple_through_ref(iterable.type_id)
                    .unwrap_or_else(|| panic!("variadic for-of requires tuple iterable"));
                // Prefer a direct TypePack element
                if let Some(tp) = elems
                    .iter()
                    .find(|e| matches!(type_table.get(**e), ResolvedType::TypePack { .. }))
                {
                    *tp
                } else {
                    // For TupleZip: use the first element type (all elements have the same shape)
                    elems[0]
                }
            };
            // By reference (`for v of &[..T]`), the loop variable is `&T_k`,
            // resolved here as `&TypePack`; expansion wraps each element in `&`.
            if by_ref {
                self.tysys.type_table.borrow_mut().make_ref(inner)
            } else {
                inner
            }
        };

        // Resolve the body with the binding having the element type
        let (binding_name, binding_id, binding_name_span) = match &for_of.binding {
            crate::ast::Pattern::Ident { id, name, span } => (name.clone(), Some(*id), Some(*span)),
            crate::ast::Pattern::Tuple(..) => {
                // For destructuring patterns like [a, b], use a synthetic name
                // and resolve the destructuring in the body
                (format!("__pattern_temp_{unique_id}"), None, None)
            }
            _ => {
                panic!("variadic for-of does not support this binding pattern")
            }
        };

        let is_mut = for_of.is_mut;
        let is_destructured = matches!(&for_of.binding, crate::ast::Pattern::Tuple(..));

        ctx.enter_scope();
        ctx.add_local(binding_name.clone(), binding_type, is_mut, binding_id);
        if let (Some(id), Some(name_span)) = (binding_id, binding_name_span) {
            self.record_local_symbol(id, &binding_name, name_span, is_mut, binding_type);
        }

        // Stage 7-B: records-only. reify rebuilds the `VariadicForOf` node
        // (including the destructuring sub-bindings) from the AST + the
        // `DesugarKind::ForOfVariadic` tag. This walk binds the loop variable
        // and any destructured sub-bindings into `ctx` (recording their
        // symbols) and walks the body for its facts.
        if is_destructured && let crate::ast::Pattern::Tuple(tp, _) = &for_of.binding {
            let inner_elems = self
                .tysys
                .type_table
                .borrow()
                .as_tuple(binding_type)
                .unwrap_or_else(|| vec![binding_type]);
            for (i, pat_elem) in tp.iter().enumerate() {
                if let crate::ast::Pattern::Ident {
                    id,
                    name,
                    span: name_span,
                } = pat_elem
                {
                    let elem_type: TypeId =
                        inner_elems.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                    ctx.add_local(name.clone(), elem_type, is_mut, Some(*id));
                    self.record_local_symbol(*id, name, *name_span, is_mut, elem_type);
                }
            }
        }

        for stmt in &for_of.body.stmts {
            self.resolve_stmt(stmt, ctx);
        }
        ctx.exit_scope();
    }

    fn resolve_tuple_for_of(
        &mut self,
        for_of: &ForOfStmt,
        iterable: TirExpr,
        elems: &[TypeId],
        is_enumerate: bool,
        by_ref: bool,
        ctx: &mut FunctionContext,
    ) {
        let span = for_of.span;

        // Validate: break, continue, and return are not allowed inside tuple for-of
        // because the loop is expanded at compile time into sequential blocks.
        if let Some((kind, bad_span)) = Self::find_control_flow_in_block(&for_of.body) {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: format!(
                    "`{kind}` is not allowed inside a tuple for-of loop (the loop is expanded at compile time)"
                ),
                span: bad_span,
            });
            return;
        }
        let unique_id = ctx.next_local;

        // Store iterable in a temp variable to avoid re-evaluation (reify
        // rebuilds the `__tuple_N` binding; we reserve its local slot here so
        // the walk-order local indices stay in sync with reify).
        let tuple_type_id = iterable.type_id;
        let temp_name = format!("__tuple_{unique_id}");
        ctx.add_local(temp_name, tuple_type_id, false, None);

        // Stage 5: capture each unrolled element's body facts separately. The
        // body is a single source sub-tree resolved once per element here;
        // without per-element capture every `AstId`-keyed map would be
        // overwritten so only the last element's facts survive (reify would
        // then dispatch every element to the last element's methods). Snapshot
        // the maps' pre-loop lengths; after each element, peel off and truncate
        // the freshly recorded tail. See `ElementOverlay`.
        let overlay_base = self
            .capture_tuple_overlays
            .then(|| self.sem.types.overlay_base_lens());
        let mut element_overlays: Vec<super::sem::types::ElementOverlay> = Vec::new();

        for &elem_type in elems {
            ctx.enter_scope();

            // When iterating through a reference, the element binds by reference
            // (`&T_k`); otherwise by value. Mirrors `tuple_element_binding`.
            let bind_elem_type = if by_ref {
                self.tysys.type_table.borrow_mut().make_ref(elem_type)
            } else {
                elem_type
            };

            // Stage 7-B: records-only. reify rebuilds the per-element block (the
            // `__tuple_N.i` field access + binding + body) from the AST + the
            // `DesugarKind::ForOfTuple` tag and per-element overlays. This walk
            // binds the loop variable(s) into `ctx` and walks the body so every
            // element's facts are captured.
            if is_enumerate {
                // For enumerate the binding is `[idx, val]`; resolve the pattern
                // against the synthetic `[i32, elem_type]` tuple type.
                let i32_type = TypeTable::I32;
                let enum_tuple_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_tuple(vec![i32_type, bind_elem_type]);
                self.resolve_let_pattern(
                    &for_of.binding,
                    enum_tuple_type,
                    for_of.is_mut,
                    span,
                    ctx,
                );
            } else {
                match &for_of.binding {
                    Pattern::Ident {
                        id,
                        name,
                        span: name_span,
                    }
                    | Pattern::MutIdent {
                        id,
                        name,
                        span: name_span,
                    } => {
                        let is_mut =
                            for_of.is_mut || matches!(&for_of.binding, Pattern::MutIdent { .. });
                        ctx.add_local(name.clone(), bind_elem_type, is_mut, Some(*id));
                        self.record_local_symbol(*id, name, *name_span, is_mut, bind_elem_type);
                    }
                    Pattern::Tuple(_, _) | Pattern::Struct { .. } => {
                        self.resolve_let_pattern(
                            &for_of.binding,
                            bind_elem_type,
                            for_of.is_mut,
                            span,
                            ctx,
                        );
                    }
                    Pattern::Wildcard => {
                        // Discard the element; nothing to bind.
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: "invalid binding pattern in for-of loop".to_string(),
                            span,
                        });
                    }
                }
            }

            // Resolve the body AST (each expansion gets its own resolution with
            // different element types) for its facts.
            self.resolve_block(&for_of.body, ctx, None);

            // Capture this element's body annotations and reset the maps back to
            // their pre-loop state so the next element records from a clean
            // slate (Stage 5; reify-only).
            if let Some(base) = overlay_base {
                element_overlays.push(self.sem.types.split_off_overlay(base));
            }

            ctx.exit_scope();
        }

        // Record this for-of's per-element overlays as one instantiation (in
        // deterministic walk order). A nested inner for-of resolves once per
        // outer element, appending one entry per outer element; reify's visit
        // counter pairs them up in the same order.
        if overlay_base.is_some() {
            let for_of_key = self.ann_key(for_of.id);
            self.sem
                .types
                .tuple_overlays
                .entry(for_of_key)
                .or_default()
                .push(element_overlays);
        }
    }

    /// Lower `for let v of iterable { body }` directly into TIR for non-tuple
    /// iterables. Produces:
    ///
    /// ```text
    /// __for_of_N: {
    ///     let mut __iter_N = iterable.into_iter();
    ///     loop {
    ///         match __iter_N.next() {
    ///             Option::Some(v) => body,
    ///             _ => break,
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// The synthetic `__iter_N` local is registered with `defining_ast_id:
    /// None` (same convention as `assert`'s `__cond` / `__vK` temps) so it
    /// never enters `local_symbols`. That keeps LSP hover / jump-to-def on
    /// the `for` keyword from surfacing the helper name. The `.into_iter()`
    /// / `.next()` dispatches go through [`Self::resolve_method_call_with`]
    /// with `method_id: None`, so no use→def edges are recorded against
    /// `for_of.id` either — clicking the `for` keyword no longer drags the
    /// user into `Iterator::next` in `core:prelude/list.wado`.
    ///
    /// `for_of.iterable` is resolved as-is — if the user wrote
    /// `for let item of expr.enumerate()`, the `.enumerate()` is part of
    /// the AST and flows naturally into the iterator chain. (The previous
    /// `is_enumerate` parameter survived only because `resolve_tuple_for_of`
    /// uses it to special-case the index binding; the iterator path has
    /// nothing to do with it. The pre-refactor implementation wrapped the
    /// already-enumerated AST in a second `.enumerate()`, producing
    /// `IterEnumerate<IterEnumerate<…>>` and ICE-ing at codegen with
    /// "unsubstituted `AssocTypeProjection` `Item` reached codegen" — see
    /// `tests/fixtures/for_of_iterator_enumerate.wado`.)
    fn resolve_iterator_for_of(&mut self, for_of: &ForOfStmt, ctx: &mut FunctionContext) {
        use super::method_call::MethodCallInput;

        let span = for_of.span;
        let unique_id = ctx.next_local;
        let iter_var = format!("__iter_{unique_id}");
        let label = format!("__for_of_{unique_id}");

        // Resolve the iterable receiver verbatim, then dispatch `.into_iter()`
        // on it. Whatever adapter chain the user wrote (e.g. `.enumerate()`,
        // `.filter(…)`, `.map(…)`) is already part of `for_of.iterable`.
        let into_iter_receiver_type = self.resolve_expr(&for_of.iterable, ctx, None);
        let into_iter_receiver = placeholder(into_iter_receiver_type, for_of.iterable.span());

        // `<receiver>.into_iter()`
        let into_iter_call = self.resolve_method_call_with(
            MethodCallInput {
                receiver: into_iter_receiver,
                method_name: "into_iter",
                method_id: None,
                call_id: None,
                type_args: vec![],
                args: &[],
                expected_type: None,
                span,
            },
            ctx,
        );
        // Capture the `(self_kind, is_ref_impl, FunctionRef)` the dispatch
        // chose (Gap 6 of WEP 2026-05-26): the synthetic call passed
        // `call_id == None` so `record_method_dispatch` skipped it, but
        // reify needs the receiver-adjustment inputs + the resolved
        // `FunctionRef` to reproduce the same call shape. Since Stage 7-B
        // `resolve_method_call_with` returns a typed placeholder, the
        // `FunctionRef` rides `pending_method_dispatch` rather than being
        // recovered from `into_iter_call.kind`.
        let into_iter_dispatch = self.pending_method_dispatch.take();
        let iter_type = into_iter_call.type_id;

        // Iterator-trait conformance check, mirroring the pre-refactor
        // surface error.
        if !self.type_implements_trait(iter_type, "Iterator")
            && !matches!(
                self.tysys.type_table.borrow().get(iter_type),
                ResolvedType::Unknown | ResolvedType::TypeParam { .. }
            )
        {
            let type_name = self.tysys.type_table.borrow().type_name(iter_type);
            let _ = self.logger.error(TypeError::MissingTraitImpl {
                type_name,
                trait_name: "Iterator".to_string(),
                span,
            });
        }

        // `let mut __iter_N = …;` — `defining_ast_id: None` keeps this
        // synthetic local out of `local_symbols`. Stage 7-B: reify rebuilds the
        // `let`; we reserve the local slot here for walk-order parity.
        let iter_local_index =
            ctx.add_local(iter_var.clone(), iter_type, /* is_mut */ true, None);

        // Make `__for_of_N` visible to a body-level `break __for_of_N`
        // (no existing user does this, but the validation in `resolve_break`
        // would otherwise reject it). Pop after the body has been resolved.
        ctx.active_labels.push(label);

        // `__iter_N.next()` — dispatch on the Local receiver, no AST.
        let iter_local_ref = TirExpr::new(
            TirExprKind::Local {
                index: iter_local_index,
                name: iter_var,
            },
            iter_type,
            span,
        );
        let next_call = self.resolve_method_call_with(
            MethodCallInput {
                receiver: iter_local_ref,
                method_name: "next",
                method_id: None,
                call_id: None,
                type_args: vec![],
                args: &[],
                expected_type: None,
                span,
            },
            ctx,
        );
        let next_dispatch = self.pending_method_dispatch.take();
        let option_type = next_call.type_id;

        // Build the `Option::Some(<user binding>)` arm pattern directly as
        // TIR. Resolving the user's `for_of.binding` against the Item type
        // delegates name binding / destructuring to `resolve_if_pattern_inner`,
        // which preserves the binding's real `AstId` (LSP hover on the loop
        // variable still works). The wrapping `TirPattern::Variant` is
        // built by hand so the lowering never synthesises an AST node — the
        // `Some` token has no source position, so giving it one would be
        // misleading.
        let some_case_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
            .to_string();
        // `.next()` returns `Option<Item>`. Extract the `Some` payload type
        // for the binding scrutinee. Bind out of the borrow first so the
        // `get_variant_case_payload_type` call below can re-borrow `&mut self`.
        let option_shape: Option<(String, Vec<TypeId>)> =
            match self.tysys.type_table.borrow().get(option_type).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } if self.contains_variant(&name) => Some((name, type_args)),
                ResolvedType::Variant { name, .. } if self.contains_variant(&name) => {
                    Some((name, vec![]))
                }
                _ => None,
            };
        let item_type = match option_shape {
            Some((name, type_args)) => {
                self.get_variant_case_payload_type(&name, &some_case_name, &type_args, span)
            }
            // `.next()` returned an unexpected non-Option type. The iterator-
            // trait check above (or method dispatch downstream) has already
            // diagnosed it; degrade to `UNKNOWN` to keep resolution going.
            None => TypeTable::UNKNOWN,
        };

        // Stage 5 (Gap 6 of WEP 2026-05-26): record the iterator-path
        // dispatch decision so reify can re-emit the synthetic
        // `into_iter()` / `next()` calls without re-dispatching. Only
        // record when both dispatches succeeded (the trait-check error
        // path above bailed without resolving them).
        if let (
            Some((into_iter_self_kind, into_iter_is_ref_impl, into_iter_func)),
            Some((next_self_kind, next_is_ref_impl, next_func)),
        ) = (into_iter_dispatch, next_dispatch)
        {
            self.record_for_of_iterator(
                for_of.id,
                super::sem::types::ForOfIteratorInfo {
                    into_iter: into_iter_func,
                    into_iter_self_kind,
                    into_iter_is_ref_impl,
                    next: next_func,
                    next_self_kind,
                    next_is_ref_impl,
                    item_type,
                    iter_type,
                },
            );
        }

        // Stage 7-B: records-only. reify rebuilds the
        // `__for_of_N: { let mut __iter = …; loop { match __iter.next() { … } } }`
        // shape from the AST + the recorded `ForOfIteratorInfo`. This walk binds
        // the loop variable (`resolve_if_pattern_inner`, preserving the
        // binding's real `AstId`) and walks the body for its facts.
        ctx.enter_scope();
        self.resolve_if_pattern_inner(&for_of.binding, item_type, ctx, span, RefBinding::None);
        self.resolve_block(&for_of.body, ctx, None);
        ctx.exit_scope();

        ctx.active_labels.pop();
    }

    /// Check if a block contains `break`, `continue`, or `return` at the top level
    /// (not inside nested loops/functions where they would be valid).
    /// Returns the kind name and span of the first offending statement.
    fn find_control_flow_in_block(block: &Block) -> Option<(&'static str, Span)> {
        for stmt in &block.stmts {
            if let Some(found) = Self::find_control_flow_in_stmt(stmt) {
                return Some(found);
            }
        }
        None
    }

    fn find_control_flow_in_stmt(stmt: &Stmt) -> Option<(&'static str, Span)> {
        match stmt {
            Stmt::Break(b) => Some(("break", b.span)),
            Stmt::Continue(c) => Some(("continue", c.span)),
            // return and task return are allowed — they exit the enclosing function,
            // which is well-defined even in compile-time-expanded blocks.
            Stmt::Return(_) | Stmt::TaskReturn(_) => None,
            // Recurse into blocks that don't introduce a new loop/function scope
            Stmt::If(if_stmt) => {
                if let Some(found) = Self::find_control_flow_in_block(&if_stmt.then_block) {
                    return Some(found);
                }
                if let Some(else_block) = &if_stmt.else_block {
                    return Self::find_control_flow_in_block(else_block);
                }
                None
            }
            Stmt::LabeledBlock(lb) => Self::find_control_flow_in_block(&lb.block),
            // Don't recurse into loops/closures — break/continue/return there are valid
            _ => None,
        }
    }

    /// Resolve a break statement (Stage 7-B: records-only).
    pub(super) fn resolve_break(&mut self, break_stmt: &BreakStmt, ctx: &mut FunctionContext) {
        // Resolve the break value against the target block's expected type
        // so that literals coerce correctly (e.g. `break label: 10` when the
        // block is used as `let x: i64 = label: { ... }`).
        let expected = break_stmt.label.as_ref().and_then(|label| {
            ctx.labeled_block_targets
                .iter()
                .rev()
                .find(|t| &t.label == label)
                .and_then(|t| t.expected_type)
        });
        let value = break_stmt
            .value
            .as_ref()
            .map(|v| self.resolve_expr(v, ctx, expected));

        // Validate that the target label exists
        if let Some(label) = &break_stmt.label
            && !ctx.active_labels.iter().any(|l| l == label)
        {
            let _ = self.logger.error(TypeError::UnknownIdentifier {
                name: format!("labeled break target not found: {label}"),
                span: break_stmt.span,
            });
        }

        // If breaking with a value to a labeled block expression, record the
        // type. Scan innermost-first so a `break label` inside a nested block
        // that reuses the same label name is attributed to the inner target —
        // consistent with the `expected_type` lookup above and with WIR `br`
        // depth resolution.
        if let (Some(label), Some(val)) = (&break_stmt.label, &value) {
            for target in ctx.labeled_block_targets.iter_mut().rev() {
                if &target.label == label {
                    target.break_types.push(*val);
                    break;
                }
            }
        }

        // Stage 7-B: records-only; reify rebuilds the `Break` stmt.
    }

    /// Resolve a continue statement (Stage 7-B: records-only). `continue`
    /// carries no facts; reify rebuilds the `Continue` (or the
    /// `break <for-body-label>` retarget) from the AST and
    /// `ctx.for_continue_labels`.
    pub(super) fn resolve_continue(
        &mut self,
        _continue_stmt: &ContinueStmt,
        _ctx: &FunctionContext,
    ) {
    }

    /// Resolve a `while` or `while let` loop directly into TIR.
    ///
    /// `while cond { B }` lowers to:
    ///
    /// ```text
    /// loop {
    ///     if !cond { break; }
    ///     B
    /// }
    /// ```
    ///
    /// `while let pat = expr { B }` (and the let-chain variant) lowers to:
    ///
    /// ```text
    /// loop {
    ///     match expr { pat => B, _ => break; }
    /// }
    /// ```
    ///
    /// Naked `break` / `continue` inside `B` target the synthesised
    /// `loop`, which is the correct semantics — no label re-targeting is
    /// required (unlike C-style `for`).
    pub(super) fn resolve_while(&mut self, w: &WhileStmt, ctx: &mut FunctionContext) {
        // Naked `continue` inside this while's body targets *this* loop,
        // not an enclosing C-style `for` body label.
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);
        // Stage 7-B: records-only. reify rebuilds the `loop { if !cond { break }
        // B }` (or `loop { match e { pat => B, _ => break } }`) shape from the
        // `DesugarKind::While` / `WhileLetChain` tag + the AST. This walk
        // resolves the condition / scrutinees and walks the body for facts.
        match &w.condition {
            Condition::Expr(cond_expr) => {
                self.record_desugar(w.id, super::sem::types::DesugarKind::While);
                self.resolve_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                self.resolve_block(&w.body, ctx, None);
            }
            Condition::LetChain {
                elements,
                span: cond_span,
            } => {
                self.record_desugar(w.id, super::sem::types::DesugarKind::WhileLetChain);
                // The else-branch (an unconditional `break`) is rebuilt by reify;
                // the combined walk only binds the chain patterns and walks the
                // then-body for facts.
                ctx.enter_scope();
                self.resolve_let_chain_stmts(elements, &w.body, ctx, None, *cond_span);
                ctx.exit_scope();
            }
        }

        ctx.for_continue_labels = saved_continue;
    }

    /// Resolve a C-style `for init; cond; update { B }` loop directly into TIR.
    ///
    /// Lowered shape:
    ///
    /// ```text
    /// {
    ///     init;                        // when present
    ///     loop {
    ///         if !cond { break; }      // omitted when `cond` is absent
    ///         __for_N_body: { B }
    ///         update;                  // when present
    ///     }
    /// }
    /// ```
    ///
    /// The outer `{ … }` is a fresh elaborator scope so `init`'s bindings stay
    /// local to the for. `B` is wrapped in `__for_N_body` so that naked
    /// `continue` (which would otherwise skip the `update`) is rerouted via
    /// [`Self::resolve_continue`] to `break __for_N_body`, letting control
    /// fall through to `update;` before the next iteration. Naked `break`
    /// already targets the innermost loop, which is the `loop {}` here, so
    /// no extra rewriting is needed.
    ///
    /// `while let` form `for init; let pat = e; update { B }` lowers to:
    ///
    /// ```text
    /// {
    ///     init;
    ///     loop {
    ///         match e {
    ///             pat => {
    ///                 __for_N_body: { B }
    ///                 update;
    ///             }
    ///             _ => break;
    ///         }
    ///     }
    /// }
    /// ```
    pub(super) fn resolve_for(&mut self, f: &ForStmt, ctx: &mut FunctionContext) {
        self.record_desugar(f.id, super::sem::types::DesugarKind::CStyleFor);
        let loop_id = ctx.next_loop_id;
        ctx.next_loop_id += 1;
        let body_label = format!("__for_{loop_id}_body");

        // Mirror `resolve_loop` / `resolve_while` / `resolve_for_of`: clear the
        // continue-retarget stack at the loop boundary so the invariant
        // ("the stack lists labels for the enclosing C-style `for` bodies that
        // a naked `continue` should `break` to, innermost-first") cannot be
        // violated by a future refactor that resolves part of the body
        // outside `resolve_for_labeled_body`. Today the push/pop inside
        // that helper alone would suffice, but the symmetry guards against
        // a body-resolution path moving above the helper.
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);

        // The outer scope holds `init`'s bindings so the loop body can see
        // them while the surrounding function cannot.
        ctx.enter_scope();

        // Stage 7-B: records-only. reify rebuilds the C-style-for desugar
        // (`{ init; loop { if !cond { break } __for_N_body: { B } update } }`,
        // or the `while let` form) from the `DesugarKind::CStyleFor` tag + the
        // AST. This walk resolves `init` / `cond` / scrutinee, binds the
        // for-header let pattern, and walks the body + update for their facts.
        // Body and update are resolved here (not up-front) because in the
        // let-chain form the pattern's bindings must be in scope for both — see
        // `lib/core/prelude/string.wado::String::find_char`.
        if let Some(init) = &f.init {
            self.resolve_stmt(init, ctx);
        }
        match &f.condition {
            None => {
                self.resolve_for_labeled_body(&body_label, &f.body, ctx);
                self.resolve_for_update(f.update.as_ref(), ctx);
            }
            Some(Condition::Expr(cond_expr)) => {
                self.resolve_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                self.resolve_for_labeled_body(&body_label, &f.body, ctx);
                self.resolve_for_update(f.update.as_ref(), ctx);
            }
            Some(Condition::LetChain {
                elements,
                span: cond_span,
            }) => {
                // The parser only accepts a single Let element in a for
                // header — multi-element let-chains are syntactically
                // limited to `if`/`while`. Future grammar evolution that
                // relaxes this should surface here as a diagnostic, not
                // an ICE.
                let single_let = if elements.len() == 1 {
                    match &elements[0] {
                        ConditionElement::Let {
                            pattern,
                            expr,
                            span: elem_span,
                        } => Some((pattern, expr, *elem_span)),
                        ConditionElement::Expr(_) => None,
                    }
                } else {
                    None
                };
                let Some((pattern, expr, elem_span)) = single_let else {
                    let _ = self.logger.error(TypeError::InvalidPattern {
                        message: "for-header let-chain must consist of a single \
                                  `let pattern = expr` element"
                            .to_string(),
                        span: *cond_span,
                    });
                    ctx.exit_scope();
                    ctx.for_continue_labels = saved_continue;
                    return;
                };

                let scrutinee_type = self.resolve_expr(expr, ctx, None);
                ctx.enter_scope();
                self.resolve_if_pattern(pattern, scrutinee_type, ctx, elem_span);
                // Body and update both run inside the pattern scope so they can
                // name the bindings introduced by `pat`.
                self.resolve_for_labeled_body(&body_label, &f.body, ctx);
                self.resolve_for_update(f.update.as_ref(), ctx);
                ctx.exit_scope();
            }
        }

        ctx.exit_scope();
        ctx.for_continue_labels = saved_continue;
    }

    /// Resolve a for loop's body wrapped in its continue-retarget label.
    /// Pushes `body_label` onto both `for_continue_labels` (so naked
    /// `continue` inside the body becomes `break <body_label>`) and
    /// `active_labels` (so the label validates as a known break target).
    fn resolve_for_labeled_body(
        &mut self,
        body_label: &str,
        body: &Block,
        ctx: &mut FunctionContext,
    ) {
        // Stage 7-B: records-only; reify rebuilds the labeled body block.
        ctx.for_continue_labels.push(body_label.to_string());
        ctx.active_labels.push(body_label.to_string());
        self.resolve_block(body, ctx, None);
        ctx.active_labels.pop();
        ctx.for_continue_labels.pop();
    }

    /// Resolve a for loop's optional update expression for its facts
    /// (Stage 7-B: records-only).
    fn resolve_for_update(&mut self, update: Option<&Expr>, ctx: &mut FunctionContext) {
        if let Some(u) = update {
            self.resolve_expr(u, ctx, None);
        }
    }
}

fn format_pattern_qualifier_type(ty: &Type) -> String {
    match ty {
        Type::Named(t) => t.name.clone(),
        Type::Generic(t) => {
            let args = t
                .args
                .iter()
                .map(format_pattern_qualifier_type)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}<{args}>", t.name)
        }
        Type::NamespacedGeneric(t) => {
            if t.args.is_empty() {
                format!("{}::{}", t.namespace, t.name)
            } else {
                let args = t
                    .args
                    .iter()
                    .map(format_pattern_qualifier_type)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}::{}<{args}>", t.namespace, t.name)
            }
        }
        Type::Function(_) => "fn".to_string(),
        Type::Tuple(types) => {
            let elems = types
                .iter()
                .map(format_pattern_qualifier_type)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{elems}]")
        }
        Type::Reference(inner) => format!("&{}", format_pattern_qualifier_type(inner)),
        Type::MutReference(inner) => format!("&mut {}", format_pattern_qualifier_type(inner)),
        Type::TypePackSpread(name, _) => format!("..{name}"),
        Type::Error(_) => "<error>".to_string(),
    }
}

/// Collect `(binding_name -> AstId)` from an AST `Pattern`. Used by the
/// or-pattern handler to align every alternative's `defining_ast_id` with
/// the first alternative's source node, so that LSP jump-to-def from a use
/// in the arm body lands on the first alternative's binding.
fn collect_ast_pattern_binding_ids(
    pattern: &Pattern,
    out: &mut crate::hashmap::IndexMap<String, AstId>,
) {
    match pattern {
        Pattern::Ident { id, name, .. } | Pattern::MutIdent { id, name, .. } => {
            out.entry(name.clone()).or_insert(*id);
        }
        Pattern::Tuple(patterns, _) => {
            for p in patterns {
                collect_ast_pattern_binding_ids(p, out);
            }
        }
        Pattern::Variant { bindings, .. } => {
            for p in bindings {
                collect_ast_pattern_binding_ids(p, out);
            }
        }
        Pattern::Struct { fields, .. } => {
            for f in fields {
                collect_ast_pattern_binding_ids(&f.pattern, out);
            }
        }
        Pattern::Or(alternatives) => {
            if let Some(first) = alternatives.first() {
                collect_ast_pattern_binding_ids(first, out);
            }
        }
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Error(_) => {}
    }
}

/// Collect binding names, local indices, and types from a TIR pattern for or-pattern validation.
pub(super) fn collect_pattern_bindings_with_index(
    pattern: &TirPattern,
) -> Vec<(String, u32, crate::tir::TypeId)> {
    let mut bindings = Vec::new();
    collect_pattern_bindings_with_index_inner(pattern, &mut bindings);
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    bindings
}

fn collect_pattern_bindings_with_index_inner(
    pattern: &TirPattern,
    out: &mut Vec<(String, u32, crate::tir::TypeId)>,
) {
    match pattern {
        TirPattern::Binding {
            name,
            local_index,
            type_id,
        } => {
            out.push((name.clone(), *local_index, *type_id));
        }
        TirPattern::Tuple(patterns, _) => {
            for p in patterns {
                collect_pattern_bindings_with_index_inner(p, out);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                collect_pattern_bindings_with_index_inner(p, out);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                collect_pattern_bindings_with_index_inner(&f.pattern, out);
            }
        }
        TirPattern::Or(alternatives) => {
            if let Some(first) = alternatives.first() {
                collect_pattern_bindings_with_index_inner(first, out);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
    }
}

/// Remap a specific `local_index` in a pattern to a new value.
pub(super) fn remap_pattern_local(pattern: &mut TirPattern, from: u32, to: u32) {
    match pattern {
        TirPattern::Binding { local_index, .. } => {
            if *local_index == from {
                *local_index = to;
            }
        }
        TirPattern::Tuple(patterns, _) => {
            for p in patterns {
                remap_pattern_local(p, from, to);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                remap_pattern_local(p, from, to);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                remap_pattern_local(&mut f.pattern, from, to);
            }
        }
        TirPattern::Or(alternatives) => {
            for p in alternatives {
                remap_pattern_local(p, from, to);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
    }
}

/// Resolve a primitive type's builtin associated constant (`i32::MIN`,
/// `u8::MAX`, …) to its `i128` value. Pure and `self`-free so both the
/// elaborator's pattern lowering and the reify pass share one source of
/// truth for the range-endpoint / const-pattern paths. Returns `None`
/// for non-primitive qualifiers or unknown const names.
pub(super) fn primitive_assoc_const_to_i128(
    qualifier: Option<&Type>,
    const_name: &str,
) -> Option<i128> {
    let ty_name = match qualifier? {
        Type::Named(named) => named.name.as_str(),
        Type::Generic(generic) => generic.name.as_str(),
        Type::NamespacedGeneric(namespaced) => namespaced.name.as_str(),
        Type::Function(_)
        | Type::Tuple(_)
        | Type::Reference(_)
        | Type::MutReference(_)
        | Type::TypePackSpread(_, _)
        | Type::Error(_) => return None,
    };
    match (ty_name, const_name) {
        ("i8", "MAX") => Some(i128::from(i8::MAX)),
        ("i8", "MIN") => Some(i128::from(i8::MIN)),
        ("i16", "MAX") => Some(i128::from(i16::MAX)),
        ("i16", "MIN") => Some(i128::from(i16::MIN)),
        ("i32", "MAX") => Some(i128::from(i32::MAX)),
        ("i32", "MIN") => Some(i128::from(i32::MIN)),
        ("i64", "MAX") => Some(i128::from(i64::MAX)),
        ("i64", "MIN") => Some(i128::from(i64::MIN)),
        ("u8", "MAX") => Some(i128::from(u8::MAX)),
        ("u8", "MIN") => Some(i128::from(u8::MIN)),
        ("u16", "MAX") => Some(i128::from(u16::MAX)),
        ("u16", "MIN") => Some(i128::from(u16::MIN)),
        ("u32", "MAX") => Some(i128::from(u32::MAX)),
        ("u32", "MIN") => Some(i128::from(u32::MIN)),
        ("u64", "MAX") => Some(i128::from(u64::MAX)),
        ("u64", "MIN") => Some(i128::from(u64::MIN)),
        _ => None,
    }
}
