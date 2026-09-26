//! Statement resolution (let, return, if, loop, break, continue, etc.).

use crate::ast::{
    self, AstId, AstVisitor, Block, BreakStmt, Condition, ConditionElement, Expr, ExprStmt,
    ForOfStmt, ForStmt, IfStmt, Item, LetStmt, Literal, Pattern, ReturnStmt, Stmt, TaskReturnStmt,
    Type, WhileStmt, walk_expr, walk_stmt,
};
use crate::compiler_host::CompilerHost;
use crate::primitive::PrimitiveType;
use crate::tir::{ResolvedType, TirPattern, TypeId, TypeTable};
use crate::tir_visitor::remap_local_reads;
use crate::token::Span;

use super::method_lookup::REPLACE_ON_ASSIGN_TYPE;
use super::types::{BindingSite, FunctionContext, MustBind, TypeError};
use super::tysys::TypeSystem;
use super::util;
use super::{Elaborator, ForwardDefaults};
use crate::ast::{BinaryOp, RangeKind, StructPatternField};
use crate::compiler_item::CompilerItem;
use crate::defs::DefId;
use crate::elaborator::expr::MemberOwner;
use crate::elaborator::orchestration::first_infer_span;
use crate::elaborator::sem::types::{BodyFacts, DesugarKind, ForOfIteratorInfo};
use crate::elaborator::synth::ArgClass;
use crate::elaborator::trait_env::written_type_source;
use crate::elaborator::trait_query::assoc_const_owner;
use crate::elaborator::types::{GenericNewtypeInfo, ImplMemberKind, ParamSlot, StructFieldInfo};
use crate::name::{
    constant_pattern_local_name, for_body_label, mangle_local_item_name, minted_name,
    namespace_member_alias,
};
use crate::resolve::Resolutions;
use crate::symbol_notation::render;
use crate::tir::StructDef;
use crate::{escape, hashmap, tir};

/// Tracks the reference binding mode for match ergonomics.
/// When matching a reference-typed scrutinee, bindings inherit the reference kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefBinding {
    None,
    Ref,
    MutRef,
}

/// Variables a pattern binds into the function context, as `(name,
/// local_index, type_id)` triples in declaration (pre-order). The body walk's
/// pattern resolvers return these instead of a `TirPattern`: reify
/// rebuilds the real pattern node independently, so the only thing the walk
/// must surface is the binding set (used by or-pattern validation).
type PatBindings = Vec<(String, u32, TypeId)>;

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Walk a block: resolve each statement and manage the lexical scope.
    /// `expected_type` reaches the trailing statement so its coercion fact lands.
    pub(super) fn resolve_block(
        &mut self,
        block: &Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) {
        self.resolve_block_with_position(block, ctx, expected_type, false);
    }

    /// Resolve a block whose value is consumed (the block is in expression
    /// position: an `Expr::Block`, an `if`/`match` arm, a labeled-block
    /// expression). The trailing `match`/`if`/labeled-block is resolved as a
    /// value even without an `expected_type`, so it keeps its arm-agreed type
    /// instead of being pinned to `Unit` by `resolve_stmt`.
    pub(super) fn resolve_block_value(
        &mut self,
        block: &Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) {
        self.resolve_block_with_position(block, ctx, expected_type, true);
    }

    fn resolve_block_with_position(
        &mut self,
        block: &Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) {
        let ctx = &mut ctx.enter_scope();
        let items = self.sem.decls.fn_local_items.clone();
        util::replaced(
            self,
            |elaborator| &mut elaborator.sem.decls.fn_local_items,
            items,
            |this| this.resolve_block_stmts(block, ctx, expected_type, tail_value),
        );
    }

    fn resolve_block_stmts(
        &mut self,
        block: &Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) {
        self.hoist_local_items(block);
        let len = block.stmts.len();
        for (i, s) in block.stmts.iter().enumerate() {
            // The trailing statement is resolved in value position when the
            // block's value is consumed — either an `expected_type` flows in
            // (for coercion) or the block itself sits in expression position
            // (`tail_value`). A bare `match`/`if` tail then keeps its
            // arm-agreed value type instead of `resolve_stmt` pinning it to
            // `Unit` (which only suits a discarded statement-position match).
            if (expected_type.is_some() || tail_value) && i == len - 1 {
                if let Stmt::Expr(expr_stmt) = s {
                    self.resolve_expr(&expr_stmt.expr, ctx, expected_type);
                    continue;
                }
                if let Stmt::If(if_stmt) = s {
                    self.resolve_if_stmt_with_expected(if_stmt, ctx, expected_type, tail_value);
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
                    self.resolve_labeled_block_with_expected(
                        labeled_block,
                        ctx,
                        expected_type,
                        tail_value,
                    );
                    continue;
                }
            }
            self.resolve_stmt(s, ctx);
        }
    }

    /// Bring a block's local items into scope ahead of its statements: structs, then
    /// newtypes to a fixpoint (a base may name a later one), then struct fields.
    fn hoist_local_items(&mut self, block: &Block) {
        let items: Vec<&ast::Item> = block
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Item(item) => Some(&**item),
                _ => None,
            })
            .collect();
        for item in &items {
            if let ast::Item::Struct(struct_decl) = item {
                self.declare_local_struct(struct_decl);
            }
        }
        let mut pending: Vec<&ast::Newtype> = items
            .iter()
            .filter_map(|item| match item {
                ast::Item::Newtype(newtype_decl) => Some(newtype_decl),
                _ => None,
            })
            .collect();
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|newtype_decl| !self.resolve_local_newtype(newtype_decl));
            if pending.len() == before {
                break;
            }
        }
        for item in items {
            self.resolve_local_item(item);
        }
    }

    /// Resolve a statement for its facts. Reify rebuilds the `TirStmt`(s)
    /// from the AST; desugared constructs that expand to multiple statements
    /// record their `DesugarKind` tag here.
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
            Stmt::Loop(loop_stmt) => self.resolve_block(&loop_stmt.body, ctx, None),
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
            Stmt::Continue(_) => {}
            Stmt::Assert(a) => self.desugar_assert(a, ctx),
            Stmt::LabeledBlock(labeled_block) => self.resolve_labeled_block(labeled_block, ctx),
            // Already resolved ahead of the block's statements.
            Stmt::Item(_) => {}
            // Parser error-recovery placeholder: the syntax error was already
            // reported, so there is nothing to record.
            Stmt::Error(_) => {}
        }
    }

    /// Resolve a `Stmt::Item` — a declaration inside a function body, scoped to
    /// the block that writes it. A struct or newtype is minted under a mangled
    /// `{name}@{AstId}` so sibling blocks cannot collide in the module's shared
    /// tables. The other kinds are parsed but not resolved, having no
    /// per-`AstId` fact for reify.
    fn resolve_local_item(&mut self, item: &ast::Item) {
        ForwardDefaults(self).visit_item(item);
        match item {
            ast::Item::Struct(struct_decl) => self.resolve_local_struct(struct_decl),
            // `hoist_local_items` resolved these ahead of the struct fields.
            ast::Item::Newtype(_) => {}
            // Parsed but not yet resolved — see the doc comment on
            // `resolve_local_item` above.
            ast::Item::Enum(_)
            | ast::Item::Variant(_)
            | ast::Item::Flags(_)
            | ast::Item::Impl(_)
            | ast::Item::Trait(_) => {}
            // `at_local_item_start` never recognizes these at block
            // position, so `parse_item` never produces one here — except
            // `Error`, which parse-error recovery can still surface (see
            // `Stmt::Error` handling in `resolve_stmt`, already a no-op).
            // Listed explicitly (no wildcard) so a future `Item` variant
            // forces a decision here instead of silently no-op'ing.
            ast::Item::Use(_)
            | ast::Item::Function(_)
            | ast::Item::Interface(_)
            | ast::Item::TupleTypeDecl(_)
            | ast::Item::BuiltinTypeDecl(_)
            | ast::Item::Resource(_)
            | ast::Item::World(_)
            | ast::Item::Test(_)
            | ast::Item::Global(_)
            | ast::Item::Error(_) => {}
        }
    }

    /// Give a local struct its identity before any of its block's
    /// declarations are resolved, so a type may name one written later.
    fn declare_local_struct(&mut self, struct_decl: &ast::StructDecl) {
        let def = self.tysys.def_at(struct_decl.id);
        // Mirrors `intern_all_decl_types`'s "base entry" for a module-level
        // generic struct: its usage sites mint separate `GenericInstance`
        // TypeIds, and this one exists so `type_id_of_decl` has something to
        // find. The head is the declaration, not the `Foo@AstId` storage
        // spelling, which names none.
        let type_id = self
            .tysys
            .type_table
            .borrow_mut()
            .make_struct(StructDef::Decl(def));
        self.tysys
            .type_table
            .borrow_mut()
            .register_decl_type(struct_decl.id, type_id);
        let mangled_name = mangle_local_item_name(&struct_decl.name, struct_decl.id);
        self.sem
            .decls
            .fn_local_items
            .insert(struct_decl.name.clone(), def);
        // The type parameters are final already — read off the AST, not
        // resolved — so only the fields are left for `resolve_local_struct`.
        let type_param_type_ids = Elaborator::<H>::slot_type_ids(
            &ParamSlot::list(&struct_decl.type_params),
            &self.tysys.type_table,
        );
        self.sem.decls.local.struct_fields.insert(
            def,
            StructFieldInfo {
                name: mangled_name,
                ..StructFieldInfo::of_decl(
                    self.current_module_source.clone(),
                    struct_decl,
                    Vec::new(),
                    type_param_type_ids,
                )
            },
        );
    }

    fn resolve_local_struct(&mut self, struct_decl: &ast::StructDecl) {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.register_generic_params(&struct_decl.type_params, 0);

        let mut field_ctx =
            FunctionContext::new(TypeTable::UNIT, format!("struct:{}", struct_decl.name));
        let fields: Vec<_> = struct_decl
            .fields
            .iter()
            .map(|field| {
                let type_id = scope.resolve_struct_field(field, &mut field_ctx);
                (field.name.clone(), type_id, field.visibility)
            })
            .collect();

        let type_params = scope.data_type_params(&struct_decl.type_params);
        drop(scope);

        self.sem
            .types
            .decl_type_params
            .insert(struct_decl.id, type_params);

        let def = self.tysys.def_at(struct_decl.id);
        self.sem
            .decls
            .local
            .struct_fields
            .get_mut(&def)
            .expect("`declare_local_struct` ran over this block first")
            .fields = fields;
    }

    /// Report what a signature's written types cannot mean.
    pub(super) fn reject_signature_annotations(
        &mut self,
        params: &[ast::Param],
        return_type: Option<&ast::Type>,
    ) {
        for param in params {
            self.reject_written_annotation(&param.ty);
        }
        if let Some(ty) = return_type {
            self.reject_written_annotation(ty);
        }
    }

    /// Report what a written type cannot mean: a name no declaration answers,
    /// then a `..X` naming something that is not a pack.
    pub(super) fn reject_written_annotation(&mut self, ty: &ast::Type) {
        self.reject_unresolved_annotation(ty);
        self.reject_non_pack_spreads(ty);
    }

    /// Report every `..X` in a written type whose `X` no type parameter list
    /// declares a pack.
    pub(super) fn reject_non_pack_spreads(&mut self, ty: &ast::Type) {
        let bad: Vec<(String, Span)> = ty
            .pack_spreads()
            .into_iter()
            .filter(|(name, _)| !self.binds_type_pack(name))
            .map(|(name, span)| (name.to_string(), span))
            .collect();
        for (name, span) in bad {
            let _ = self.emit(TypeError::SpreadOfNonPack { name, span });
        }
    }

    /// Whether `name` is a type pack the enclosing declaration declares. A
    /// scalar parameter spread in a tuple stands for one position, not a run.
    pub(super) fn binds_type_pack(&self, name: &str) -> bool {
        let Some(binder) = self.annotate_ctx.trait_ctx.type_params.get(name) else {
            return false;
        };
        self.tysys.type_table.borrow().is_type_pack(binder.type_id)
    }

    /// Report a written type position naming a type no declaration answers
    /// here. Unlike a bound or a signature it names no type parameter it does
    /// not already have in scope, so the site's answer is decisive.
    pub(super) fn reject_unresolved_annotation(&mut self, ty: &ast::Type) {
        if self.logger.has_errors() {
            return;
        }
        self.walk_type_heads(ty, &mut |scope, id, name, span, has_args| {
            if name == "Self" || scope.annotate_ctx.trait_ctx.type_params.contains_key(name) {
                return false;
            }
            if scope.reject_non_type_decl(id, name, span) {
                return true;
            }
            if scope.decl_key_at(Some(id), name).is_some() {
                return false;
            }
            // A bare name has tiers the module scope does not hold, `Self` and
            // the frame's parameters among them. A head carrying arguments has none.
            if !has_args && scope.resolve_named_type(id, name, span, false) != TypeTable::UNKNOWN {
                return false;
            }
            let _ = scope.emit(TypeError::UnknownType {
                name: name.to_string(),
                span,
            });
            true
        });
    }

    /// Resolve a local newtype, reporting whether its base came out known.
    ///
    /// An unknown base registers nothing: a newtype registered against
    /// `UNKNOWN` is indistinguishable from a real one, and the next round of
    /// the caller's fixpoint would bind a dependent to it and keep it.
    fn resolve_local_newtype(&mut self, newtype_decl: &ast::Newtype) -> bool {
        // A generic one names no single type: each instantiation resolves the
        // base AST with its arguments substituted, so what is recorded is the
        // declaration — the same entry a module-level generic newtype makes.
        let def = self.tysys.def_at(newtype_decl.id);
        if !newtype_decl.type_params.is_empty() {
            self.sem
                .decls
                .local
                .generic_newtypes
                .insert(def, GenericNewtypeInfo::of_decl(newtype_decl));
            self.sem
                .decls
                .fn_local_items
                .insert(newtype_decl.name.clone(), def);
            return true;
        }
        let base_type_id = self.resolve_type(&newtype_decl.ty);
        if base_type_id == TypeTable::UNKNOWN {
            return false;
        }
        self.sem.decls.local.declare_newtype(
            &self.tysys.type_table,
            def,
            newtype_decl.id,
            base_type_id,
        );
        self.sem
            .decls
            .fn_local_items
            .insert(newtype_decl.name.clone(), def);
        true
    }

    pub(super) fn resolve_labeled_block(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
    ) {
        self.resolve_labeled_block_with_expected(labeled_block, ctx, None, false);
    }

    fn resolve_labeled_block_with_expected(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) {
        // A stmt-position block yields no value, but it is still a break
        // target: its frame keeps an inner `break LABEL` from landing on an
        // outer block expression reusing the name. Its collected types are
        // dropped with the block's value.
        self.resolve_block_with_position(
            &labeled_block.block,
            &mut ctx.enter_labeled_block(labeled_block.label.clone(), expected_type),
            expected_type,
            tail_value,
        );
    }

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
            let resolved = self.resolve_type(annotated_type);
            self.reject_unresolved_annotation(annotated_type);
            let target_type = if first_infer_span(annotated_type).is_some() {
                TypeTable::ERROR
            } else {
                resolved
            };
            // Publish the resolved whole-pattern annotation so reify reads it
            // instead of re-running `resolve_type` against the AST.
            let key = let_stmt.id;
            self.sem.types.let_annotated_types.insert(key, target_type);

            // Special case: tuple literal with Tuple type annotation. A literal
            // with spread elements (`[..F::method()]`) needs the general
            // `resolve_tuple_literal` path, which expands each spread; the
            // element-wise fast path below would resolve a bare `Spread` and ICE.
            let has_spread = matches!(ast_value, ast::Expr::TupleLiteral(t)
                if t.elements.iter().any(|e| matches!(e, ast::Expr::Spread(..))));
            if let ast::Expr::TupleLiteral(tuple_lit) = ast_value
                && !has_spread
            {
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
                            let _ = self.emit(TypeError::PatternTypeMismatch {
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
                    // `resolve_expr` decides what an unnamed literal against a
                    // declared struct means. Deciding it a second time here is
                    // how the two spellings came to check different things.
                    let is_struct = matches!(
                        self.tysys.type_table.borrow().get(target_type),
                        ResolvedType::Struct { .. }
                    );
                    if is_struct || self.implicit_struct_target(Some(target_type)).is_some() {
                        (
                            self.resolve_expr(ast_value, ctx, Some(target_type)),
                            target_type,
                        )
                    } else if let Some(coerced) = self
                        .try_coerce_struct_newtype(ast_value, ctx, target_type)
                        .or_else(|| self.try_coerce_struct_to_map(ast_value, ctx, target_type))
                    {
                        (coerced, target_type)
                    } else {
                        self.report_if_not_a_map_target(target_type, struct_lit.span);
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

        // A `let … else` checks its annotation as the type pattern it is.
        if let Some(else_block) = &let_stmt.else_block {
            self.resolve_let_else(let_stmt, value_type, else_block, ctx);
            return;
        }

        if let_stmt.ty.is_some() {
            if self
                .tysys
                .type_table
                .borrow()
                .is_resource_narrowing(value_type, type_id)
            {
                self.reject_refutable_narrowing(
                    value_type,
                    type_id,
                    let_stmt.name_span,
                    BindingSite::Let,
                );
            } else {
                self.typecheck(value_type, type_id, ast_value.span());
            }
        }

        self.resolve_let_pattern(
            &let_stmt.pattern,
            type_id,
            let_stmt.is_mut,
            let_stmt.span,
            BindingSite::Let,
            ctx,
        );
        let Some(binding) = let_stmt.pattern.as_name() else {
            return;
        };
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
                ctx.closure_defaults
                    .insert(binding.name.to_string(), defaults);
            }
        }
    }

    /// Resolve a `let PAT = EXPR else { ... }` statement. The else block is
    /// resolved in its own scope (it must not see the pattern bindings) and
    /// must diverge; the refutable pattern's bindings then enter `ctx` so the
    /// rest of the enclosing block can use them.
    fn resolve_let_else(
        &mut self,
        let_stmt: &LetStmt,
        scrutinee_type: TypeId,
        else_block: &Block,
        ctx: &mut FunctionContext,
    ) {
        self.resolve_block(else_block, ctx, None);
        if !self.ast_block_always_exits(else_block) {
            let _ = self.emit(TypeError::LetElseMustDiverge {
                span: else_block.span,
            });
        }
        let pattern = let_stmt.else_pattern();
        if self.let_else_pattern_is_irrefutable(&pattern, scrutinee_type) {
            let _ = self.emit(TypeError::InvalidPattern {
                message: "irrefutable pattern in `let ... else`: the else block can never run; \
                          use a plain `let` instead"
                    .to_string(),
                span: let_stmt.name_span,
            });
        }
        self.resolve_if_pattern(&pattern, scrutinee_type, ctx, let_stmt.span);
    }

    /// Whether a `let ... else` pattern is definitely irrefutable (always
    /// binds), which would make its else block unreachable. Conservative: only
    /// the unambiguous top-level binding forms are flagged, so a refutable
    /// pattern is never wrongly rejected.
    fn let_else_pattern_is_irrefutable(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
    ) -> bool {
        match pattern {
            Pattern::Wildcard | Pattern::MutIdent { .. } => true,
            Pattern::Ident { name, .. } => {
                !self.is_known_case_of_type(scrutinee_type, name, None)
                    && !self.is_immutable_global(name)
            }
            Pattern::Typed { pattern, ty, .. } => {
                let target = self.resolve_type(ty);
                !self
                    .tysys
                    .type_table
                    .borrow()
                    .is_resource_narrowing(scrutinee_type, target)
                    && self.let_else_pattern_is_irrefutable(pattern, target)
            }
            // Destructuring may hold refutable sub-patterns; the rest are
            // refutable outright (or a parser-recovery placeholder). Never flag.
            Pattern::Tuple(..)
            | Pattern::Struct { .. }
            | Pattern::Literal(_)
            | Pattern::Variant { .. }
            | Pattern::Or(_)
            | Pattern::Range { .. }
            | Pattern::Error(_) => false,
        }
    }

    /// Report a tuple pattern naming more elements than the tuple holds, or,
    /// without `..`, fewer. Answers whether the arity fits.
    fn check_tuple_pattern_arity(
        &self,
        written: usize,
        has_rest: bool,
        held: usize,
        span: Span,
    ) -> bool {
        let [expected, found] = if has_rest && written > held {
            [
                format!("tuple with at least {written} elements"),
                format!("tuple with {held} elements"),
            ]
        } else if !has_rest && written != held {
            [
                format!("tuple with {held} elements"),
                format!("pattern with {written} elements"),
            ]
        } else {
            return true;
        };
        let _ = self.emit(TypeError::PatternTypeMismatch {
            expected,
            found,
            span,
        });
        false
    }

    /// The type each element of a tuple pattern matches against `held`,
    /// reporting an arity that does not fit. Only the elements ahead of a
    /// variadic pack have a fixed position, so a pattern over one stops there
    /// with `..`.
    fn tuple_pattern_element_types(
        &self,
        written: usize,
        has_rest: bool,
        held: &[TypeId],
        span: Span,
    ) -> Vec<TypeId> {
        let pack_at = {
            let table = self.tysys.type_table.borrow();
            held.iter().position(|&t| table.is_type_pack(t))
        };
        let fixed = if let Some(pack_at) = pack_at {
            if !has_rest || written > pack_at {
                let _ = self.emit(TypeError::PatternTypeMismatch {
                    expected: format!(
                        "`..` after at most {pack_at} elements, since a variadic pack follows them"
                    ),
                    found: format!("pattern with {written} elements"),
                    span,
                });
            }
            &held[..pack_at]
        } else {
            self.check_tuple_pattern_arity(written, has_rest, held.len(), span);
            held
        };
        (0..written)
            .map(|i| fixed.get(i).copied().unwrap_or(TypeTable::UNKNOWN))
            .collect()
    }

    /// Report the fields a struct pattern without `..` leaves out.
    fn check_struct_pattern_complete(
        &self,
        head: StructDef,
        fields: &[StructPatternField],
        span: Span,
    ) {
        let Some(struct_info) = self.lookup_struct_fields_of(head) else {
            return;
        };
        let missing: Vec<_> = struct_info
            .fields
            .iter()
            .filter(|(name, _, _)| !fields.iter().any(|f| f.field_name == *name))
            .map(|(name, _, _)| name.clone())
            .collect();
        if !missing.is_empty() {
            let _ = self.emit(TypeError::PatternTypeMismatch {
                expected: format!(
                    "all fields (missing: {}), or use `..` to ignore remaining fields",
                    missing.join(", ")
                ),
                found: format!(
                    "pattern with {} of {} fields",
                    fields.len(),
                    struct_info.fields.len()
                ),
                span,
            });
        }
    }

    /// Resolve an uninitialized let declaration, `let x: T;`, whose use before
    /// assignment the bind phase has already ruled out.
    fn resolve_uninit_let(&mut self, let_stmt: &LetStmt, ctx: &mut FunctionContext) {
        let annotated_type = let_stmt
            .ty
            .as_ref()
            .expect("parser ensures type annotation for uninit let");
        let type_id = self.resolve_type(annotated_type);
        self.reject_unresolved_annotation(annotated_type);
        match (&let_stmt.pattern, let_stmt.pattern.as_name()) {
            (_, Some(n)) => {
                self.bind_local(
                    ctx,
                    n.id,
                    n.name,
                    n.span,
                    let_stmt.is_mut || n.is_mut,
                    type_id,
                );
            }
            (Pattern::Wildcard, None) => {}
            _ => {
                let _ = self.emit(TypeError::InvalidPattern {
                    message: "an uninitialized `let` declares a single name; \
                              destructure where the value is given"
                        .to_string(),
                    span: let_stmt.name_span,
                });
            }
        }
    }

    /// Check the binding of a walk over a type pack, a `for` or a tuple
    /// comprehension: a name, `_`, or a tuple of them as long as the element.
    pub(super) fn check_pack_binding(
        &mut self,
        binding: &Pattern,
        binding_type: TypeId,
        span: Span,
    ) -> bool {
        let parts = match binding {
            Pattern::Tuple(elems, _) => elems.iter().collect(),
            binding => vec![binding],
        };
        if let Some(reason) = parts.iter().find_map(|p| refutable_shape(p)) {
            self.reject_refutable(BindingSite::ForOf, &reason, span);
            return false;
        }
        if !parts
            .iter()
            .all(|p| p.as_name().is_some() || matches!(p, Pattern::Wildcard))
        {
            let _ = self.emit(TypeError::InvalidPattern {
                message: "a walk over a type pack binds a name, `_`, or a tuple of them"
                    .to_string(),
                span,
            });
            return false;
        }
        let Pattern::Tuple(elems, has_rest) = binding else {
            return true;
        };
        let Some(held) = self.tysys.type_table.borrow().as_tuple(binding_type) else {
            let _ = self.emit(TypeError::PatternTypeMismatch {
                expected: "tuple type".to_string(),
                found: self.tysys.type_table.borrow().type_name(binding_type),
                span,
            });
            return false;
        };
        self.check_tuple_pattern_arity(elems.len(), *has_rest, held.len(), span)
    }

    /// Bind the names a type-pack walk's binding spells, an element past the
    /// end with the error type.
    pub(super) fn bind_pack_binding(
        &mut self,
        binding: &Pattern,
        binding_type: TypeId,
        is_mut: bool,
        ctx: &mut FunctionContext,
    ) {
        let elems = match binding {
            Pattern::Tuple(elems, _) => {
                let held = self
                    .tysys
                    .type_table
                    .borrow()
                    .elem_types_or_self(binding_type);
                elems
                    .iter()
                    .zip(held.into_iter().chain(std::iter::repeat(TypeTable::ERROR)))
                    .collect()
            }
            binding => vec![(binding, binding_type)],
        };
        for (pattern, ty) in elems {
            if let Some(n) = pattern.as_name() {
                self.bind_local(ctx, n.id, n.name, n.span, is_mut || n.is_mut, ty);
            }
        }
    }

    /// Why `pattern` itself, apart from its parts, can fail against `scrutinee`.
    /// A case cannot when no other case of its type holds a value.
    fn refutation(&mut self, pattern: &Pattern, scrutinee: TypeId) -> Option<String> {
        match pattern {
            Pattern::Variant { variant_name, .. }
                if !self.another_case_holds_a_value(scrutinee, variant_name) =>
            {
                None
            }
            _ => refutable_shape(pattern),
        }
    }

    /// Whether a value of `scrutinee` can be a case other than `name`. A type
    /// with no cases to ask about answers `true`.
    fn another_case_holds_a_value(&self, scrutinee: TypeId, name: &str) -> bool {
        let head = self
            .tysys
            .type_table
            .borrow()
            .scrutinee_structure_head(scrutinee);
        if let Some(enumeration) = self.tysys.enum_of_type(head) {
            return enumeration.cases.len() > 1;
        }
        let (Some(variant), Some(payloads)) = (
            self.tysys.variant_of_type(head),
            self.case_payload_types(head),
        ) else {
            return true;
        };
        let name = self.strip_ns_prefix(name).unwrap_or(name);
        variant
            .cases
            .iter()
            .zip(payloads)
            .any(|(case, payload)| case.name != name && !self.is_uninhabited(payload))
    }

    fn reject_refutable(&mut self, site: BindingSite, reason: &str, span: Span) {
        let (position, remedy) = match site {
            BindingSite::Let => ("`let` binding", "use `let ... else` or `if let` instead"),
            BindingSite::ForOf => (
                "`for` binding",
                "match on the element in the loop body instead",
            ),
        };
        let _ = self.emit(TypeError::InvalidPattern {
            message: format!("refutable pattern in {position}: {reason}; {remedy}"),
            span,
        });
    }

    /// Whether `case_name`, written under `qualifier`, is a case the scrutinee
    /// offers. A newtype's cases are its base's.
    pub(super) fn is_known_case_of_type(
        &mut self,
        type_id: TypeId,
        case_name: &str,
        qualifier: Option<&Type>,
    ) -> bool {
        if !self.pattern_qualifier_matches_scrutinee(type_id, qualifier) {
            return false;
        }
        let type_id = self
            .tysys
            .type_table
            .borrow()
            .scrutinee_structure_head(type_id);
        let resolved = self.tysys.type_table.borrow().get(type_id).clone();
        match &resolved {
            ResolvedType::Enum { .. } => self
                .tysys
                .enum_of_type(type_id)
                .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name)),
            ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => self
                .tysys
                .variant_of_type(type_id)
                .is_some_and(|info| info.case_named(case_name).is_some()),
            _ => false,
        }
    }

    /// Whether a written pattern qualifier names the type the scrutinee takes its
    /// cases from, by declaration rather than by the name written.
    pub(super) fn pattern_qualifier_matches_scrutinee(
        &mut self,
        scrutinee_type: TypeId,
        qualifier: Option<&Type>,
    ) -> bool {
        let Some(qualifier) = qualifier else {
            return true;
        };
        let base_type = self
            .tysys
            .type_table
            .borrow()
            .scrutinee_structure_head(scrutinee_type);
        let scrutinee_args = match self.tysys.type_table.borrow().get(base_type) {
            ResolvedType::Enum { .. } | ResolvedType::Variant { .. } => None,
            ResolvedType::GenericInstance { type_args, .. } => Some(type_args.clone()),
            _ => return false,
        };
        // Every name on the chain qualifies the same cases: `C::Green`,
        // `E::Green` and `Color::Green` where `type C = Color; type E = C`.
        let chain_defs = self
            .tysys
            .type_table
            .borrow()
            .structure_chain_defs(scrutinee_type);
        let names_scrutinee = |elab: &mut Self, q: &Type| {
            elab.qualifier_def(q)
                .is_some_and(|def| chain_defs.contains(&def))
        };
        match qualifier {
            Type::Named(t) => {
                names_scrutinee(self, qualifier)
                    // `ns::Case` parses as a `Named("ns")` qualifier plus a bare
                    // `Case`, so a prefix naming no type names a module.
                    || chain_defs
                        .iter()
                        .any(|def| self.namespace_reaches_type(&t.name, t.id, t.span, *def))
            }
            // The name answers first, so a qualifier naming another type never
            // has its arguments read: nothing it wrote there could be accepted.
            Type::Generic(g) => {
                names_scrutinee(self, qualifier)
                    && self.qualifier_args_agree(scrutinee_args.as_deref(), &g.args)
            }
            Type::NamespacedGeneric(ns) => {
                names_scrutinee(self, qualifier)
                    && (ns.args.is_empty()
                        || self.qualifier_args_agree(scrutinee_args.as_deref(), &ns.args))
            }
            Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _)
            | Type::Infer(_)
            | Type::Error(_) => false,
        }
    }

    /// Whether the type arguments a qualifier writes are the ones the scrutinee
    /// carries, each read at its own reference site rather than counted.
    fn qualifier_args_agree(&mut self, scrutinee: Option<&[TypeId]>, written: &[Type]) -> bool {
        let Some(scrutinee) = scrutinee else {
            return false;
        };
        scrutinee.len() == written.len()
            && written
                .iter()
                .zip(scrutinee)
                .all(|(w, s)| self.qualifier_arg_agrees(w, *s))
    }

    /// Whether one written qualifier argument is the scrutinee's at that
    /// position.
    fn qualifier_arg_agrees(&mut self, written: &Type, scrutinee: TypeId) -> bool {
        let resolved = self.resolve_type(written);
        if resolved == TypeTable::UNKNOWN && !matches!(written, Type::Infer(_)) {
            self.reject_unresolved_annotation(written);
        }
        let tt = self.tysys.type_table.borrow();
        // A generic body's own parameter, an `_`, or a name that reached no
        // type names no instantiation, so it can disagree with none.
        let abstract_at = |id: TypeId| {
            matches!(tt.get(id), ResolvedType::TypeParam { .. }) || tt.contains_undecided(id)
        };
        abstract_at(resolved)
            || abstract_at(scrutinee)
            || tt.type_key(resolved) == tt.type_key(scrutinee)
    }

    /// The declaration a written qualifier means, the resolve pass answering
    /// first: a generic newtype has one before any use names its arguments.
    fn qualifier_def(&mut self, qualifier: &Type) -> Option<DefId> {
        let (site, span, name) = match qualifier {
            Type::Named(t) => (t.id, t.span, t.name.clone()),
            Type::Generic(g) => (g.id, g.span, g.name.clone()),
            Type::NamespacedGeneric(ns) => (
                ns.id,
                ns.span,
                namespace_member_alias(&ns.namespace, &ns.name),
            ),
            _ => return None,
        };
        if let Some(def) = self.type_lookup().declaration_at(Some(site), &name) {
            return Some(def);
        }
        let resolved = self.resolve_named_type(site, &name, span, false);
        self.tysys.type_table.borrow().nominal_def(resolved)
    }

    /// Whether `def`'s type is reachable as a member of the namespace `alias`
    /// imports, a re-export counting as such a reach.
    fn namespace_reaches_type(&mut self, alias: &str, site: AstId, span: Span, def: DefId) -> bool {
        if self.namespace_alias_source(alias, site).is_none() {
            return false;
        }
        let name = self.tysys.type_table.borrow().def_name(def).to_string();
        let member = namespace_member_alias(alias, &name);
        let resolved = self.resolve_unsited_type_name(&member, span);
        self.tysys.type_table.borrow().nominal_def(resolved) == Some(def)
    }

    fn format_pattern_case_name(&self, case_name: &str, qualifier: Option<&Type>) -> String {
        let Some(qualifier) = qualifier else {
            return case_name.to_string();
        };
        format!("{}::{case_name}", format_pattern_qualifier_type(qualifier))
    }

    /// `type_id` wrapped in the scrutinee's reference kind — the one place that
    /// wrap happens, because it is where `&mut` onto a scalar is refused: Wasm
    /// GC has no interior pointer to one, so it would reference a copy.
    fn pattern_binding_type(
        &mut self,
        name: &str,
        type_id: TypeId,
        ref_binding: RefBinding,
        span: Span,
    ) -> TypeId {
        match ref_binding {
            RefBinding::None => type_id,
            RefBinding::Ref => self
                .tysys
                .type_table
                .borrow_mut()
                .intern(ResolvedType::Ref(type_id)),
            RefBinding::MutRef => {
                if self.tysys.is_replace_on_assign_place_type(type_id) {
                    let _ = self.emit(TypeError::CannotAssign {
                        message: format!(
                            "cannot bind '{name}' as a mutable reference to {REPLACE_ON_ASSIGN_TYPE}: \
                             destructure by value, or take the reference to the whole value"
                        ),
                        span,
                    });
                }
                self.tysys
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::MutRef(type_id))
            }
        }
    }

    /// Resolve a pattern that must bind: a `let`'s, or a `for`'s. A bare name
    /// there is a case of its type or a binding, never a global.
    pub(super) fn resolve_let_pattern(
        &mut self,
        pattern: &ast::Pattern,
        type_id: TypeId,
        is_mut: bool,
        span: Span,
        site: BindingSite,
        ctx: &mut FunctionContext,
    ) {
        self.resolve_if_pattern_inner(
            pattern,
            type_id,
            &mut ctx.replacing(|ctx| &mut ctx.must_bind, Some(MustBind { site, is_mut })),
            span,
            RefBinding::None,
        );
    }

    /// The struct a pattern destructures (a newtype's base), and whether its
    /// written name names it. A scrutinee that is no struct is reported instead.
    fn struct_pattern_head(
        &self,
        type_name: Option<&str>,
        type_name_id: Option<AstId>,
        scrutinee: TypeId,
        span: Span,
    ) -> Option<(StructDef, bool)> {
        let head = {
            let tt = self.tysys.type_table.borrow();
            match tt.get(tt.reflect_structure_head(scrutinee)) {
                ResolvedType::Struct { def, .. } => Some(*def),
                _ => None,
            }
        };
        let Some(head) = head else {
            let _ = self.emit(TypeError::PatternTypeMismatch {
                expected: "struct type".to_string(),
                found: self.tysys.type_table.borrow().type_name(scrutinee),
                span,
            });
            return None;
        };
        let name_matches = type_name.is_none_or(|written| {
            let matches = self.tysys.pattern_qualifier_matches(type_name_id, head);
            if !matches {
                let (expected, found) =
                    self.pattern_mismatch_names(type_name_id, written, scrutinee);
                let _ = self.emit(TypeError::PatternTypeMismatch {
                    expected,
                    found,
                    span,
                });
            }
            matches
        });
        Some((head, name_matches))
    }

    /// The two spellings a pattern mismatch prints.
    ///
    /// The written qualifier and the scrutinee's rendering can be the same
    /// text — two declarations of one name is precisely what this mismatch
    /// reports — so each takes its module then, in the same notation and under
    /// the same rule as [`crate::tir::TypeTable::type_names_for_mismatch`].
    fn pattern_mismatch_names(
        &self,
        site: Option<AstId>,
        written: &str,
        scrutinee: TypeId,
    ) -> (String, String) {
        let found = self.tysys.type_table.borrow().type_name(scrutinee);
        let plain = || (written.to_string(), found.clone());
        if found != written {
            return plain();
        }
        let Some(def) = site.and_then(|site| self.tysys.resolutions.declared_if_walked(site))
        else {
            return plain();
        };
        let defs = self.tysys.resolutions.defs();
        let expected = render(&defs.module(def).to_string(), defs.name(def));
        let qualified = self
            .tysys
            .type_table
            .borrow()
            .type_name_qualified(scrutinee);
        if expected == qualified {
            return plain();
        }
        (expected, qualified)
    }

    /// Resolve an expression statement
    pub(super) fn resolve_expr_stmt(&mut self, expr_stmt: &ExprStmt, ctx: &mut FunctionContext) {
        self.resolve_expr(&expr_stmt.expr, ctx, None);
    }

    pub(super) fn resolve_return(&mut self, ret_stmt: &ReturnStmt, ctx: &mut FunctionContext) {
        // An `async fn` names its result with `task return`; a bare `return`
        // still ends the function, carrying whatever was already delivered.
        if ctx.is_async && ret_stmt.value.is_some() {
            let _ = self.emit(TypeError::ReturnValueInAsync {
                span: ret_stmt.span,
            });
        }
        let return_type = ctx.return_type;
        // Use expected type for coercion (numeric literals, tuple to array,
        // etc.) and check the value type against the function return type.
        if let Some(expr) = ret_stmt.value.as_ref() {
            let mut value_type = self.resolve_expr(expr, ctx, Some(return_type));
            // Pin a deferred hole that rode a prior binding into the returned
            // value (`let v = gen()?; return Ok(v)`) against the return type.
            if self.type_has_infer_hole(value_type) {
                self.solve_infer_holes_against(value_type, return_type);
                value_type = self.apply_infer_holes(value_type);
            }
            self.typecheck_return(value_type, return_type, ret_stmt.span);
        }
    }

    pub(super) fn resolve_task_return(
        &mut self,
        tr_stmt: &TaskReturnStmt,
        ctx: &mut FunctionContext,
    ) {
        if !ctx.is_async {
            let _ = self.emit(TypeError::TaskReturnOutsideAsync { span: tr_stmt.span });
        }
        let expected = ctx.task_return_type;
        let mut value_type = self.resolve_expr(&tr_stmt.value, ctx, expected);
        // Unchecked, a mismatch reaches the CM binding, which flattens the
        // value against the *declared* result and mis-lowers it.
        let Some(expected) = expected else {
            return;
        };
        if self.type_has_infer_hole(value_type) {
            self.solve_infer_holes_against(value_type, expected);
            value_type = self.apply_infer_holes(value_type);
        }
        self.typecheck_return(value_type, expected, tr_stmt.span);
    }

    /// Reify rebuilds the `If` / if-let-chain TIR from the AST + the
    /// `DesugarKind::IfLetChain` tag; this walk only resolves the condition
    /// and blocks for their facts.
    pub(super) fn resolve_if_stmt(&mut self, if_stmt: &IfStmt, ctx: &mut FunctionContext) {
        self.resolve_if_stmt_with_expected(if_stmt, ctx, None, false);
    }

    /// Like `resolve_if_stmt` but propagates `expected_type` to blocks for
    /// coercion. Used when an if statement is the last statement in a block
    /// that needs type coercion (e.g., a match arm returning `List<T>` from an
    /// if-else with tuple literals).
    fn resolve_if_stmt_with_expected(
        &mut self,
        if_stmt: &IfStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) {
        match &if_stmt.condition {
            ast::Condition::Expr(expr) => {
                self.resolve_condition_expr(expr, ctx);
                self.resolve_block_with_position(
                    &if_stmt.then_block,
                    ctx,
                    expected_type,
                    tail_value,
                );
                if let Some(b) = &if_stmt.else_block {
                    self.resolve_block_with_position(b, ctx, expected_type, tail_value);
                }
            }
            ast::Condition::LetChain { elements, .. } => {
                self.record_desugar(if_stmt.id, DesugarKind::IfLetChain);
                // Resolve else_block in the outer scope (chain bindings are not
                // visible there) for its facts.
                if let Some(b) = &if_stmt.else_block {
                    self.resolve_block_with_position(b, ctx, expected_type, tail_value);
                }

                self.resolve_let_chain_stmts(
                    elements,
                    &if_stmt.then_block,
                    &mut ctx.enter_scope(),
                    expected_type,
                    tail_value,
                    if_stmt.span,
                );
            }
        }
    }

    /// Resolve a let-chain condition, one nesting level per element: a `Let`
    /// becomes a two-arm `Match`, an `Expr` an `If` guard, every failure falling
    /// through to `else_block` and the innermost success running `then_block`.
    /// This walk only resolves the scrutinees and conditions and binds the
    /// patterns; reify rebuilds the chain from the `DesugarKind::IfLetChain` tag.
    pub(super) fn resolve_let_chain_stmts(
        &mut self,
        elements: &[ConditionElement],
        then_block_ast: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
        span: Span,
    ) {
        if elements.is_empty() {
            self.resolve_block_with_position(then_block_ast, ctx, expected_type, tail_value);
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
                    tail_value,
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
                    tail_value,
                    span,
                );
            }
        }
    }

    /// The alias and type of the immutable global `ns::NAME` names in a pattern,
    /// which is a constant-value pattern as the bare `NAME` is.
    pub(super) fn namespaced_constant(
        &self,
        qualifier: Option<&Type>,
        name: &str,
    ) -> Option<(String, TypeId)> {
        let alias = self.sem.imports.pattern_ns_member(qualifier, name)?;
        let ty = self.immutable_global_type(&alias)?;
        Some((alias, ty))
    }

    /// Whether `name` refers to an immutable global (defined here or imported),
    /// which in pattern position is a constant-value match, not a binding.
    pub(super) fn is_immutable_global(&self, name: &str) -> bool {
        self.immutable_global_type(name).is_some()
    }

    /// The type of the immutable global `name` refers to, if it names one.
    fn immutable_global_type(&self, name: &str) -> Option<TypeId> {
        match self.sem.decls.current_module_globals.get(name) {
            Some(&(ty, mutable)) => (!mutable).then_some(ty),
            None => self
                .sem
                .decls
                .imported_globals
                .get(name)
                .and_then(|&(_, _, ty, mutable)| (!mutable).then_some(ty)),
        }
    }

    /// Where a constant pattern's `scrutinee == constant` is a trait call, dispatch
    /// it on the pattern and reserve the local reify holds the scrutinee in.
    fn resolve_constant_pattern(
        &mut self,
        pattern_id: AstId,
        scrutinee: TypeId,
        constant: TypeId,
        ctx: &mut FunctionContext,
        span: Span,
    ) {
        {
            let type_table = self.tysys.type_table.borrow();
            // A scalar compares by instruction, and a wide int's constant is
            // folded to a literal pattern.
            if type_table.is_scalar_primitive_like(scrutinee) || type_table.is_wide_int(scrutinee) {
                return;
            }
        }
        self.resolve_binary_op(
            scrutinee,
            BinaryOp::Eq,
            constant,
            span,
            span,
            Some(pattern_id),
        );
        if self.sem.types.operator_dispatch.contains_key(&pattern_id) {
            ctx.add_local(constant_pattern_local_name(), scrutinee, false, None);
        }
    }

    /// Bind a refutable pattern's variables into `ctx`, returning them in
    /// declaration order; reify builds the `TirPattern` from the same AST.
    pub(super) fn resolve_if_pattern(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> PatBindings {
        let (peeled_type, ref_binding) = self
            .tysys
            .peel_scrutinee_refs(scrutinee_type, RefBinding::None);
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
        if scrutinee_type == TypeTable::ERROR
            && let Some(subpatterns) = shape_checked_subpatterns(pattern)
        {
            return subpatterns
                .into_iter()
                .flat_map(|p| {
                    self.resolve_if_pattern_inner(p, TypeTable::ERROR, ctx, span, ref_binding)
                })
                .collect();
        }
        if let Some(MustBind { site, .. }) = ctx.must_bind
            && let Some(reason) = self.refutation(pattern, scrutinee_type)
        {
            self.reject_refutable(site, &reason, span);
        }
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
                let is_mut = matches!(pattern, Pattern::MutIdent { .. });
                // A bare identifier in a pattern context could be a variant/enum case
                // (e.g., `None`, `Red`) or a variable binding (e.g., `x`, `val`).
                // The parser does not use case to disambiguate; instead, we check
                // whether the name is a known case of the scrutinee type.
                if !is_mut && self.is_known_case_of_type(scrutinee_type, name, None) {
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
                // Immutable global constant: a constant-value pattern that
                // introduces no binding but reads the global — record the
                // use→def edge so it is not flagged dead (mirrors the expr path).
                // A pattern that must bind never reads one, since a constant
                // pattern there could only be rejected as refutable.
                if !is_mut
                    && ctx.must_bind.is_none()
                    && let Some(constant) = self.immutable_global_type(name)
                {
                    self.record_item_reference_by_name(*id, name);
                    let (peeled, _) = self.tysys.peel_scrutinee_refs(scrutinee_type, ref_binding);
                    self.typecheck(constant, peeled, *name_span);
                    self.resolve_constant_pattern(*id, scrutinee_type, constant, ctx, span);
                    return Vec::new();
                }
                let is_mut = is_mut || ctx.must_bind.is_some_and(|m| m.is_mut);
                let binding_type =
                    self.pattern_binding_type(name, scrutinee_type, ref_binding, *name_span);
                let index = self.bind_local(ctx, *id, name, *name_span, is_mut, binding_type);
                vec![(name.clone(), index, binding_type)]
            }
            Pattern::Literal(lit) => {
                match lit {
                    Literal::Number(repr) if util::is_float_only_literal(repr) => {
                        let _ = self.emit(TypeError::InvalidPattern {
                            message: "float literals cannot be used in match patterns".to_string(),
                            span,
                        });
                    }
                    Literal::Null => {
                        // If the scrutinee is a variant type with a `None` case,
                        // `null` lowers to a `None` variant pattern (no binding).
                        let _ = self.try_null_as_none_pattern(scrutinee_type);
                    }
                    _ => self.check_pattern_value(pattern, scrutinee_type, span),
                }
                Vec::new()
            }
            Pattern::Tuple(patterns, has_rest) => {
                let (scrutinee_type, ref_binding) =
                    self.tysys.peel_scrutinee_refs(scrutinee_type, ref_binding);
                let element_types =
                    if let Some(types) = self.tysys.type_table.borrow().as_tuple(scrutinee_type) {
                        types
                    } else {
                        let _ = self.emit(TypeError::PatternTypeMismatch {
                            expected: "tuple type".to_string(),
                            found: self.tysys.type_table.borrow().type_name(scrutinee_type),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    };
                let element_types = self.tuple_pattern_element_types(
                    patterns.len(),
                    *has_rest,
                    &element_types,
                    span,
                );
                patterns
                    .iter()
                    .zip(element_types)
                    .flat_map(|(p, ty)| {
                        self.resolve_if_pattern_inner(p, ty, ctx, span, ref_binding)
                    })
                    .collect()
            }
            Pattern::Variant {
                variant_name,
                variant_qualifier,
                name_id,
                name_span: _,
                bindings,
                span,
            } => {
                let (scrutinee_type, ref_binding) =
                    self.tysys.peel_scrutinee_refs(scrutinee_type, ref_binding);
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
                    // Resolve to literal patterns when possible for switch optimization.
                    if let Some(assoc) = self
                        .tysys
                        .associated_constant_qualified(variant_qualifier.as_ref(), variant_name)
                    {
                        self.check_inherent_member_visibility(
                            assoc.inherent_visibility,
                            Some(&assoc.module),
                            MemberOwner::Written(variant_qualifier.as_ref()),
                            variant_name,
                            ImplMemberKind::AssociatedConstant,
                            *name_id,
                            *span,
                        );
                        // Resolve the const body for its facts. An associated
                        // constant introduces no binding — it is either a literal
                        // or an opaque constant-value pattern — so return none
                        // either way.
                        let const_module = assoc.module.clone();
                        ctx.with_caller_bindings_hidden(|ctx| {
                            self.with_resolving_home(Some(const_module), |s| {
                                s.resolve_expr(&assoc.value, ctx, Some(assoc.ty))
                            })
                        });
                        self.typecheck(assoc.ty, scrutinee_type, *span);
                        if let Some(id) = *name_id {
                            self.resolve_constant_pattern(id, scrutinee_type, assoc.ty, ctx, *span);
                        }
                        return Vec::new();
                    }

                    if let Some((alias, constant)) =
                        self.namespaced_constant(variant_qualifier.as_ref(), variant_name)
                    {
                        if let Some(id) = *name_id {
                            self.record_item_reference_by_name(id, &alias);
                            self.resolve_constant_pattern(id, scrutinee_type, constant, ctx, *span);
                        }
                        self.typecheck(constant, scrutinee_type, *span);
                        return Vec::new();
                    }

                    // No variable can be spelled `V::Case`, `Case(x)` or `Case()`,
                    // so naming neither a case nor a constant is an error here.
                    let _ = self.emit(TypeError::PatternTypeMismatch {
                        expected: format!(
                            "valid case of {}",
                            self.tysys.type_table.borrow().type_name(scrutinee_type)
                        ),
                        found: qualified_variant_name,
                        span: *span,
                    });
                    return Vec::new();
                }

                // The cases come from the structure the scrutinee wraps, while
                // the qualifier is asked of the written type.
                let base_type = self
                    .tysys
                    .type_table
                    .borrow()
                    .reflect_structure_head(scrutinee_type);
                let resolved_type = self.tysys.type_table.borrow().get(base_type).clone();
                if !self
                    .pattern_qualifier_matches_scrutinee(scrutinee_type, variant_qualifier.as_ref())
                {
                    // The scrutinee's type, not its declaration: the qualifier
                    // is compared against the instantiation.
                    let scrutinee_name = self.tysys.type_table.borrow().type_name(scrutinee_type);
                    let expected = match &resolved_type {
                        ResolvedType::Enum { .. } => format!("valid case of enum {scrutinee_name}"),
                        ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => {
                            format!("valid case of variant {scrutinee_name}")
                        }
                        _ => "variant or enum case".to_string(),
                    };
                    let _ = self.emit(TypeError::PatternTypeMismatch {
                        expected,
                        found: qualified_variant_name,
                        span: *span,
                    });
                    return Vec::new();
                }
                let scrutinee_type = base_type;

                // Handle enum types (no payload, just discriminant matching)
                if let ResolvedType::Enum { .. } = &resolved_type {
                    if !bindings.is_empty() {
                        let _ = self.emit(TypeError::InvalidPattern {
                            message: format!("enum case `{variant_name}` does not have a payload"),
                            span: *span,
                        });
                    }
                    // Look up the enum case index
                    if let Some(enum_info) = self.tysys.enum_of_type(scrutinee_type).cloned() {
                        if let Some(case_data) =
                            enum_info.find_case(normalized_variant_name).cloned()
                        {
                            // Record pattern's case-name identifier -> enum case decl
                            if let Some(id) = name_id {
                                self.record_reference_to_def(*id, case_data.ast_id);
                            }
                            // Enum case carries no payload — no binding.
                            return Vec::new();
                        }
                        let _ = self.emit(TypeError::PatternTypeMismatch {
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
                    let enum_name = self.tysys.type_table.borrow().type_name(scrutinee_type);
                    let _ = self.emit(TypeError::PatternTypeMismatch {
                        expected: format!("enum type `{enum_name}`"),
                        found: "unknown enum".to_string(),
                        span: *span,
                    });
                    return Vec::new();
                }

                // Record use->def for the variant case name in the pattern
                // (e.g., `Some` in `Some(x)`). Points at the case declaration's
                // span so LSP jump-to-def from the pattern lands on the case decl.
                if let Some(id) = name_id
                    && let Some(case_ast_id) = self
                        .tysys
                        .variant_of_type(scrutinee_type)
                        .and_then(|info| info.case_named(normalized_variant_name))
                        .map(|(_, case)| case.ast_id)
                {
                    self.record_reference_to_def(*id, case_ast_id);
                }

                // The cases belong to the structure the scrutinee wraps, so the
                // declaration asked for the payload type is that structure's.
                let scrutinee_decl = self.tysys.type_def(base_type);
                let payload_type: TypeId = match &resolved_type {
                    // Non-generic variant
                    ResolvedType::Variant { .. } => self.get_variant_case_payload_type(
                        scrutinee_decl.expect("a variant answers with its declaration"),
                        normalized_variant_name,
                        &[],
                        *span,
                    ),
                    // Generic variant instantiation
                    ResolvedType::GenericInstance { type_args, .. } => {
                        // Check if this is a variant (not a struct)
                        let is_variant = scrutinee_decl
                            .is_some_and(|def| self.type_lookup().variant_cases_of(def).is_some());
                        if is_variant {
                            self.get_variant_case_payload_type(
                                scrutinee_decl.expect("a variant answers with its declaration"),
                                normalized_variant_name,
                                type_args,
                                *span,
                            )
                        } else {
                            let found = self.tysys.type_table.borrow().type_name(scrutinee_type);
                            let _ = self.emit(TypeError::PatternTypeMismatch {
                                expected: "variant type".to_string(),
                                found,
                                span: *span,
                            });
                            TypeTable::UNKNOWN
                        }
                    }
                    _ => {
                        let _ = self.emit(TypeError::PatternTypeMismatch {
                            expected: "variant or enum type".to_string(),
                            found: self.tysys.type_table.borrow().type_name(scrutinee_type),
                            span: *span,
                        });
                        TypeTable::UNKNOWN
                    }
                };
                if self.is_uninhabited(payload_type) {
                    let [scrutinee_name, payload_name] = [scrutinee_type, payload_type]
                        .map(|t| self.tysys.type_table.borrow().type_name(t));
                    let _ = self.emit(TypeError::InvalidPattern {
                        message: format!(
                            "unreachable: no `{scrutinee_name}` is a `{normalized_variant_name}`, whose payload `{payload_name}` has no value"
                        ),
                        span: *span,
                    });
                }

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
                type_name_id,
                fields,
                has_rest,
                span: pat_span,
            } => {
                let (scrutinee_type, ref_binding) =
                    self.tysys.peel_scrutinee_refs(scrutinee_type, ref_binding);
                let head = self.struct_pattern_head(
                    type_name.as_deref(),
                    *type_name_id,
                    scrutinee_type,
                    *pat_span,
                );

                let mut field_bindings: PatBindings = Vec::new();
                for field in fields {
                    let field_type = match head {
                        Some(_) => {
                            self.lookup_field_type(scrutinee_type, &field.field_name, field.span)
                                .1
                        }
                        None => TypeTable::ERROR,
                    };
                    if head.is_some_and(|(_, type_name_matches)| type_name_matches) {
                        self.check_field_visibility(
                            scrutinee_type,
                            &field.field_name,
                            Some(field.id),
                            field.span,
                        );
                    }
                    field_bindings.extend(self.resolve_if_pattern_inner(
                        &field.pattern,
                        field_type,
                        ctx,
                        field.span,
                        ref_binding,
                    ));
                }

                if let Some((head, _)) = head
                    && !has_rest
                {
                    self.check_struct_pattern_complete(head, fields, *pat_span);
                }
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
                    let first_names: Vec<(&str, tir::TypeId)> = first_bindings
                        .iter()
                        .map(|(n, _, t)| (n.as_str(), *t))
                        .collect();
                    let alt_names: Vec<(&str, tir::TypeId)> = alt_bindings
                        .iter()
                        .map(|(n, _, t)| (n.as_str(), *t))
                        .collect();

                    // Over an ERROR scrutinee a bare case name reads as a
                    // binding, so the names compare nothing.
                    if scrutinee_type != TypeTable::ERROR && first_names != alt_names {
                        let shown = |names: &[(&str, tir::TypeId)]| {
                            if names.is_empty() {
                                return "nothing".to_string();
                            }
                            let tt = self.tysys.type_table.borrow();
                            names
                                .iter()
                                .map(|(n, ty)| format!("`{n}: {}`", tt.type_name(*ty)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        let _ = self.emit(TypeError::InvalidPattern {
                            message: format!(
                                "or-pattern alternatives must bind the same names with the same types: \
                                 alternative 1 binds {}, but alternative {} binds {}",
                                shown(&first_names),
                                i + 1,
                                shown(&alt_names),
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
                let mut first_alt_ast_ids: hashmap::IndexMap<String, AstId> =
                    hashmap::IndexMap::default();
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
            Pattern::Typed {
                id,
                pattern: inner,
                ty,
                span: typed_span,
            } => {
                let target = self.resolve_ascription(*id, ty);
                if self
                    .tysys
                    .type_table
                    .borrow()
                    .is_resource_narrowing(scrutinee_type, target)
                {
                    self.require_handle_classes(target, *typed_span);
                    match ctx.must_bind {
                        Some(MustBind { site, .. }) => self.reject_refutable_narrowing(
                            scrutinee_type,
                            target,
                            *typed_span,
                            site,
                        ),
                        None => self.check_narrowing_shape(inner, ref_binding, *typed_span),
                    }
                } else {
                    self.check_ascription(scrutinee_type, target, *typed_span);
                }
                self.resolve_if_pattern_inner(inner, target, ctx, span, ref_binding)
            }
            // Parser error-recovery placeholder; inert.
            Pattern::Error(_) => Vec::new(),
        }
    }

    /// A narrowing tests the class a handle carries, so its target must number one.
    fn require_handle_classes(&mut self, target: TypeId, span: Span) {
        let unnumbered = {
            let tt = self.tysys.type_table.borrow();
            tt.narrowing_classes(target)
                .is_none()
                .then(|| tt.type_name(target))
        };
        if let Some(name) = unnumbered {
            let _ = self.emit(TypeError::ResourceClasses {
                message: format!(
                    "a type pattern cannot narrow to `{name}`: it declares no `#[cm(..., classes = \"lo..=hi\")]`"
                ),
                span,
            });
        }
    }

    /// Resolve the type a type pattern ascribes, and record it for reify.
    fn resolve_ascription(&mut self, id: AstId, ty: &Type) -> TypeId {
        let target = self.resolve_type(ty);
        self.reject_unresolved_annotation(ty);
        self.sem.types.pattern_ascriptions.insert(id, target);
        target
    }

    /// Check an ascription no host test decides: the scrutinee must be a
    /// `target` already, and a type parameter names no type to narrow to.
    fn check_ascription(&mut self, scrutinee: TypeId, target: TypeId, span: Span) {
        let is_type_param = matches!(
            self.tysys.type_table.borrow().get(target),
            ResolvedType::TypeParam { .. }
        );
        if is_type_param && scrutinee != target {
            let name = self.tysys.type_table.borrow().type_name(target);
            let _ = self.emit(TypeError::InvalidPattern {
                message: format!(
                    "a type pattern needs a concrete type: `{name}` is a type parameter, \
                     so nothing states whether it narrows"
                ),
                span,
            });
            return;
        }
        self.typecheck(scrutinee, target, span);
    }

    /// A narrowing is the host's answer about the value itself: it binds that
    /// value whole, and not through a reference.
    fn check_narrowing_shape(&mut self, inner: &Pattern, ref_binding: RefBinding, span: Span) {
        let message = if ref_binding != RefBinding::None {
            "a type pattern narrows a resource value, not a reference to one"
        } else if !matches!(
            inner,
            Pattern::Ident { .. } | Pattern::MutIdent { .. } | Pattern::Wildcard
        ) {
            "a narrowing type pattern binds a name or `_`"
        } else {
            return;
        };
        let _ = self.emit(TypeError::InvalidPattern {
            message: message.to_string(),
            span,
        });
    }

    /// A narrowing where the pattern cannot fail over to anything.
    fn reject_refutable_narrowing(
        &mut self,
        value: TypeId,
        target: TypeId,
        span: Span,
        site: BindingSite,
    ) {
        let reason = {
            let tt = self.tysys.type_table.borrow();
            format!(
                "not every `{}` is `{}`",
                tt.type_name(value),
                tt.type_name(target)
            )
        };
        self.reject_refutable(site, &reason, span);
    }

    /// True when the scrutinee is a variant type that has a `None` case, so a
    /// `null` literal pattern lowers to a `None` variant pattern (which binds
    /// nothing). Reify rebuilds the actual `None` pattern; the body walk
    /// only needs the yes/no answer for its binding/fact walk.
    fn try_null_as_none_pattern(&self, scrutinee_type: TypeId) -> bool {
        let Some(variant_info) = self.tysys.variant_of_type(scrutinee_type) else {
            return false;
        };
        let tt = self.tysys.type_table.borrow();
        variant_info
            .case_named(tt.compiler_variant_case_name(CompilerItem::OptionNone))
            .is_some()
    }

    /// The type a literal pattern demands of its scrutinee, when the scrutinee is
    /// not it. Whether its value is in range is [`Self::check_pattern_value`]'s.
    pub(super) fn literal_pattern_mismatch(
        &mut self,
        lit: &Literal,
        scrutinee_type: TypeId,
    ) -> Option<String> {
        let type_table = self.tysys.type_table.borrow();
        let head = type_table.representation_head(scrutinee_type);
        let expected = match lit {
            Literal::Number(_) | Literal::Byte(_)
                if !type_table.is_integer(head) && !type_table.is_wide_int(head) =>
            {
                "an integer type"
            }
            Literal::String(_) if !type_table.is_string(head) => "String",
            Literal::Char(_) if !type_table.is_primitive(head, PrimitiveType::Char) => "char",
            Literal::Bool(_) if !type_table.is_primitive(head, PrimitiveType::Bool) => "bool",
            _ => return None,
        };
        // An unsettled head judges nothing: an unresolved type is reported
        // where it is unresolved, and a type parameter decided per instance.
        let settled = match type_table.get(head) {
            ResolvedType::Primitive(_)
            | ResolvedType::Struct { .. }
            | ResolvedType::Enum { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Unit => true,
            ResolvedType::GenericInstance { .. } => type_table.is_concrete(head),
            _ => false,
        };
        drop(type_table);
        if !settled {
            return None;
        }
        // A string-literal arm tests the scrutinee with `==`, so any type
        // answering `Eq<String>` matches one the way a `String` does.
        if matches!(lit, Literal::String(_)) && self.compares_with_string_literal(scrutinee_type) {
            return None;
        }
        Some(expected.to_string())
    }

    /// Whether `scrutinee == "…"` resolves, which is what a string-literal
    /// pattern lowers to.
    fn compares_with_string_literal(&mut self, scrutinee: TypeId) -> bool {
        let Some(eq_trait) = self.tysys.compiler_trait_def(CompilerItem::Eq) else {
            return false;
        };
        let (written, method) = {
            let type_table = self.tysys.type_table.borrow();
            (
                type_table.type_name(scrutinee),
                type_table
                    .compiler_items()
                    .trait_method_name(CompilerItem::Eq)
                    .to_string(),
            )
        };
        // A newtype inherits its base's `Eq`, which is where the impl is.
        let (name, receiver) = self
            .tysys
            .trait_impl_base_lookup(&written, scrutinee, eq_trait);
        self.find_arithmetic_trait_impl(&name, receiver, eq_trait, &method, Some(&ArgClass::StrLit))
            .is_some()
    }

    /// Report a literal pattern or range bound that is no value of
    /// `scrutinee_type`: of another kind, or out of its range.
    fn check_pattern_value(&mut self, pattern: &Pattern, scrutinee_type: TypeId, span: Span) {
        if let Pattern::Literal(lit) = pattern
            && let Some(expected) = self.literal_pattern_mismatch(lit, scrutinee_type)
        {
            let _ = self.emit(TypeError::PatternTypeMismatch {
                expected,
                found: self.tysys.type_table.borrow().type_name(scrutinee_type),
                span,
            });
            return;
        }
        let message = {
            let tt = self.tysys.type_table.borrow();
            match pattern {
                Pattern::Literal(Literal::Number(repr)) => {
                    let (negated, digits) = repr
                        .strip_prefix('-')
                        .map_or((false, repr.as_str()), |digits| (true, digits));
                    util::parse_u128_literal(digits).ok().and_then(|magnitude| {
                        util::int_literal_range_error(
                            magnitude,
                            negated,
                            digits,
                            scrutinee_type,
                            &tt,
                        )
                    })
                }
                Pattern::Literal(Literal::Byte(raw)) => {
                    escape::unescape_byte(raw).ok().and_then(|v| {
                        util::int_value_range_error(
                            v.into(),
                            &format!("b'{raw}'"),
                            scrutinee_type,
                            &tt,
                        )
                    })
                }
                Pattern::Variant {
                    variant_name,
                    variant_qualifier: Some(qualifier),
                    bindings,
                    ..
                } if bindings.is_empty() => primitive_assoc_const_to_i128(
                    Some(qualifier),
                    variant_name,
                    &self.tysys.resolutions,
                )
                .and_then(|value| {
                    let shown = format!("{}::{variant_name}", written_type_source(qualifier));
                    util::int_value_range_error(value, &shown, scrutinee_type, &tt)
                }),
                _ => None,
            }
        };
        if let Some(message) = message {
            let _ = self.emit(TypeError::InvalidPattern { message, span });
        }
    }

    /// Validate a range pattern (`0..<10` or `'a'..='z'`) for the body walk,
    /// emitting the bad-bounds / reversed / empty diagnostics. Range
    /// patterns bind nothing and reify rebuilds the real `TirPattern::Range`,
    /// so no pattern node is produced here.
    fn resolve_range_pattern(
        &mut self,
        start: &Pattern,
        end: &Pattern,
        kind: RangeKind,
        scrutinee_type: TypeId,
        span: Span,
    ) {
        let is_unsigned = self
            .tysys
            .type_table
            .borrow()
            .is_unsigned_int(scrutinee_type);

        let resolutions = &self.tysys.resolutions;
        let start_val = util::range_endpoint_to_i128(start, is_unsigned, resolutions);
        let end_val = util::range_endpoint_to_i128(end, is_unsigned, resolutions);

        let (Some(start_val), Some(end_val)) = (start_val, end_val) else {
            let _ = self.emit(TypeError::InvalidPattern {
                message: "range pattern bounds must be integer or char literals".to_string(),
                span,
            });
            return;
        };
        for bound in [start, end] {
            self.check_pattern_value(bound, scrutinee_type, span);
        }

        // Check for reversed or empty range
        let inclusive = matches!(kind, RangeKind::Inclusive);
        let order = util::range_endpoints_ordered(start_val, end_val, is_unsigned);
        if order.is_gt() {
            let _ = self.emit(TypeError::InvalidPattern {
                message: "reversed range pattern".to_string(),
                span,
            });
            return;
        }
        if !inclusive && order.is_ge() {
            let _ = self.emit(TypeError::InvalidPattern {
                message: "empty range pattern".to_string(),
                span,
            });
        }
    }

    /// Get payload type for a variant case, substituting type parameters if needed
    pub(super) fn get_variant_case_payload_type(
        &mut self,
        variant: DefId,
        case_name: &str,
        type_args: &[TypeId],
        span: Span,
    ) -> TypeId {
        let payload_opt = self
            .type_lookup()
            .variant_cases_of(variant)
            .and_then(|info| info.case_named(case_name))
            .map(|(_, case)| case.payload);

        if let Some(payload) = payload_opt {
            // Substitute type parameters with concrete types
            return self.substitute_in_frame(payload, type_args);
        }

        // The declaration is a variant — it answered `variant_cases_of` or it
        // would not be one — so the only way here is a case it does not
        // declare. The diagnostic reads its spelling at the point of
        // reporting, off the declaration.
        let variant_name = self.tysys.resolutions.defs().name(variant).to_string();
        let _ = self.emit(TypeError::PatternTypeMismatch {
            expected: format!("valid case of variant {variant_name}"),
            found: case_name.to_string(),
            span,
        });
        TypeTable::UNKNOWN
    }

    /// Resolve a for-of loop.
    ///
    /// For tuples: compile-time expansion (one copy of the body per element).
    /// For non-tuples: iterator pattern via `into_iter()` + `next()`.
    pub(super) fn resolve_for_of(&mut self, for_of: &ForOfStmt, ctx: &mut FunctionContext) {
        // Unwrap an `.enumerate()` iterable at the AST level. The elaborator
        // never resolves it as a method call, so `mc.id` carries no annotations
        // — intentional, like the `tuple.len()` short-circuits on
        // `MethodDispatch`. Reify re-detects the pattern from `for_of.iterable`.
        let (actual_iterable, is_enumerate) = match &for_of.iterable {
            Expr::MethodCall(mc) if mc.method == "enumerate" && mc.args.is_empty() => {
                (&mc.receiver, true)
            }
            _ => (&for_of.iterable, false),
        };

        // Resolve the iterable to determine its type
        let iterable_type_id = self.resolve_expr(actual_iterable, ctx, None);

        // Check if it's a tuple type — looking through a single `&`/`&mut`
        // wrapper. A reference iterable (`&[..T]`) iterates element-by-ref
        // (`&T_k`), mirroring `for v of &list`.
        let tuple_info = {
            let type_table = self.tysys.type_table.borrow();
            type_table
                .as_tuple_through_ref(iterable_type_id)
                .map(|(elems, by_ref)| {
                    let has_type_pack = elems.iter().any(|e| type_table.is_type_pack(*e));
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
        ) && self
            .tysys
            .type_table
            .borrow()
            .contains_type_pack(iterable_type_id);

        if let Some((elems, has_type_pack, by_ref)) = tuple_info {
            if has_type_pack || is_zip_variadic {
                self.record_desugar(for_of.id, DesugarKind::ForOfVariadic);
                self.resolve_variadic_for_of(for_of, iterable_type_id, is_enumerate, by_ref, ctx);
            } else {
                self.record_desugar(for_of.id, DesugarKind::ForOfTuple);
                self.resolve_tuple_for_of(
                    for_of,
                    iterable_type_id,
                    &elems,
                    is_enumerate,
                    by_ref,
                    ctx,
                );
            }
        } else {
            // Check that the iterable type implements IntoIterator
            let mut inner_type_id = iterable_type_id;
            while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                self.tysys.type_table.borrow().get(inner_type_id).clone()
            {
                inner_type_id = t;
            }
            let into_iterator = self.tysys.compiler_trait(CompilerItem::IntoIterator);
            let implements_into_iter = into_iterator.is_some_and(|trait_| {
                self.tysys.type_implements_trait(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    iterable_type_id,
                    &trait_,
                ) || self.tysys.type_implements_trait(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    inner_type_id,
                    &trait_,
                )
            }) || matches!(
                self.tysys.type_table.borrow().get(iterable_type_id),
                ResolvedType::Unknown | ResolvedType::TypeParam { .. }
            );
            let into_iter_receiver = if implements_into_iter {
                // Reify expands the tag, so only an iterable that supports
                // iteration carries one.
                self.record_desugar(for_of.id, DesugarKind::ForOfIterator);
                if is_enumerate {
                    self.resolve_expr(&for_of.iterable, ctx, None)
                } else {
                    iterable_type_id
                }
            } else {
                if iterable_type_id != TypeTable::ERROR {
                    let type_name = self.tysys.type_table.borrow().type_name(iterable_type_id);
                    let _ = self.emit(TypeError::MissingTraitImpl {
                        type_name,
                        trait_name: "IntoIterator".to_string(),
                        span: for_of.span,
                    });
                }
                TypeTable::ERROR
            };
            self.resolve_iterator_for_of(for_of, into_iter_receiver, ctx);
        }
    }

    /// Create a deferred `VariadicForOf` TIR node for `for let v of iterable`
    /// where `iterable` has a tuple type containing `TypePack` elements.
    ///
    /// The body is resolved once with the loop variable having the `TypePack` type.
    /// The monomorphizer will expand this after type substitution resolves the
    /// `TypePack` to a concrete tuple.
    fn resolve_variadic_for_of(
        &mut self,
        for_of: &ForOfStmt,
        iterable: TypeId,
        is_enumerate: bool,
        by_ref: bool,
        ctx: &mut FunctionContext,
    ) {
        if self.reject_expanded_control_flow(&for_of.body, "variadic") {
            return;
        }

        let unique_id = ctx.fresh_serial();

        // Extract the element type for the loop binding.
        // For direct TypePack: iterable is Tuple([TypePack{T}]), binding type is TypePack.
        // For TupleZip: iterable is Tuple([Tuple([TypePack, TypePack])]), binding type is the inner tuple.
        let binding_type = {
            let inner = {
                let type_table = self.tysys.type_table.borrow();
                let (elems, _) = type_table
                    .as_tuple_through_ref(iterable)
                    .unwrap_or_else(|| panic!("variadic for-of requires tuple iterable"));
                // Prefer a direct TypePack element
                if let Some(tp) = elems.iter().find(|e| type_table.is_type_pack(**e)) {
                    // A mapped pack `..F::method()` binds the loop variable to
                    // the (pack-independent) return type, not the pack itself.
                    match type_table.get(*tp) {
                        ResolvedType::TypePack {
                            mapped_elem: Some(elem),
                            ..
                        } => *elem,
                        _ => *tp,
                    }
                } else {
                    // For TupleZip: use the first element type (all elements have the same shape)
                    elems[0]
                }
            };
            // By reference (`for v of &[..T]`), the loop variable is `&T_k`,
            // resolved here as `&TypePack`; expansion wraps each element in `&`.
            let bound = if by_ref {
                self.tysys.type_table.borrow_mut().make_ref(inner)
            } else {
                inner
            };
            // Expansion supplies the index literal per unrolled element.
            if is_enumerate {
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_tuple(vec![TypeTable::I32, bound])
            } else {
                bound
            }
        };

        if !self.check_pack_binding(&for_of.binding, binding_type, for_of.span) {
            return;
        }

        // Reify rebuilds the `VariadicForOf` node
        // (including the destructuring sub-bindings) from the AST + the
        // `DesugarKind::ForOfVariadic` tag. This walk binds the loop variable
        // and any destructured sub-bindings into `ctx` (recording their
        // symbols) and walks the body for its facts.
        let ctx = &mut ctx.enter_scope();
        if for_of.binding.as_name().is_none() {
            ctx.add_local_at(
                minted_name("pattern_temp", unique_id),
                binding_type,
                for_of.is_mut,
                None,
                Span::default(),
            );
        }
        self.bind_pack_binding(&for_of.binding, binding_type, for_of.is_mut, ctx);

        let index_binding = Self::enumerate_index_local(is_enumerate, &for_of.binding, ctx);
        let ctx = &mut ctx.enter_enumerate_body(index_binding);
        self.resolve_block(&for_of.body, ctx, None);
    }

    /// The name bound to the index of `for let [i, v] of t.enumerate()`, if the
    /// binding spells one.
    pub(super) fn enumerate_index_binding_name(binding: &Pattern) -> Option<&str> {
        let Pattern::Tuple(elems, _) = binding else {
            return None;
        };
        elems.first()?.as_name().map(|n| n.name)
    }

    /// Expand `for let v of tuple { body }` by unrolling the body once per
    /// element, binding the tuple to `$tuple_N` and each element to `v` in its
    /// own block, all inside a `$tuple_for_of_N` label.
    fn resolve_tuple_for_of(
        &mut self,
        for_of: &ForOfStmt,
        iterable: TypeId,
        elems: &[TypeId],
        is_enumerate: bool,
        by_ref: bool,
        ctx: &mut FunctionContext,
    ) {
        let span = for_of.span;

        if self.reject_expanded_control_flow(&for_of.body, "tuple") {
            return;
        }
        let unique_id = ctx.fresh_serial();

        // Store iterable in a temp variable to avoid re-evaluation (reify
        // rebuilds the `$tuple_N` binding; we reserve its local slot here so
        // the walk-order local indices stay in sync with reify).
        let tuple_type_id = iterable;
        let temp_name = format!("$tuple_{unique_id}");
        ctx.add_local(temp_name, tuple_type_id, false, None);

        // Capture each unrolled element's body facts separately. The
        // body is a single source sub-tree resolved once per element here;
        // without per-element capture every `AstId`-keyed map would be
        // overwritten so only the last element's facts survive (reify would
        // then dispatch every element to the last element's methods). Snapshot
        // the maps' pre-loop lengths; after each element, peel off and truncate
        // the freshly recorded tail. See `BodyFacts`.
        let overlay_base = self.sem.types.lens();
        let mut element_overlays: Vec<BodyFacts> = Vec::new();

        for &elem_type in elems {
            let ctx = &mut ctx.enter_scope();

            // When iterating through a reference, the element binds by reference
            // (`&T_k`); otherwise by value. Mirrors `tuple_element_binding`.
            let bind_elem_type = if by_ref {
                self.tysys.type_table.borrow_mut().make_ref(elem_type)
            } else {
                elem_type
            };

            // Reify rebuilds the per-element block (the
            // `$tuple_N.i` field access + binding + body) from the AST + the
            // `DesugarKind::ForOfTuple` tag and per-element overlays. This walk
            // binds the loop variable(s) into `ctx` and walks the body so every
            // element's facts are captured.
            // For enumerate the binding is `[idx, val]`, resolved against the
            // synthetic `[i32, elem_type]` tuple type.
            let binding_type = if is_enumerate {
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_tuple(vec![TypeTable::I32, bind_elem_type])
            } else {
                bind_elem_type
            };
            self.resolve_let_pattern(
                &for_of.binding,
                binding_type,
                for_of.is_mut,
                span,
                BindingSite::ForOf,
                ctx,
            );

            // Resolve the body AST (each expansion gets its own resolution with
            // different element types) for its facts.
            self.resolve_block(&for_of.body, ctx, None);

            // Capture this element's body annotations and reset the maps back to
            // their pre-loop state so the next element records from a clean
            // slate.
            element_overlays.push(self.sem.types.split_off(overlay_base));
        }

        // Record this for-of's per-element overlays as one instantiation (in
        // deterministic walk order). A nested inner for-of resolves once per
        // outer element, appending one entry per outer element; reify's visit
        // counter pairs them up in the same order.
        let for_of_key = for_of.id;
        self.sem
            .types
            .tuple_overlays
            .entry(for_of_key)
            .or_default()
            .push(element_overlays);
    }

    /// Lower a non-tuple `for let v of iterable { body }` into a labelled block
    /// binding `$iter_N = iterable.into_iter()` around a `loop` that matches
    /// `$iter_N.next()`, breaking on `None`. The synthetic local and both
    /// dispatches carry no defining `AstId`, so clicking `for` does not drag the
    /// user into `Iterator::next`. `into_iter_receiver_type` is the type of
    /// `for_of.iterable` as written, or `ERROR` where it cannot be iterated.
    fn resolve_iterator_for_of(
        &mut self,
        for_of: &ForOfStmt,
        into_iter_receiver_type: TypeId,
        ctx: &mut FunctionContext,
    ) {
        use super::method_call::MethodCallInput;

        let span = for_of.span;
        let unique_id = ctx.fresh_serial();
        let iter_var = format!("$iter_{unique_id}");
        let label = format!("$for_of_{unique_id}");

        // `<receiver>.into_iter()` — the synthetic call passes
        // `call_id == None` so `record_method_dispatch` skips it; the
        // outcome's `dispatch` carries the `(self_kind, is_ref_impl,
        // FunctionRef)` reify needs to reproduce the same call shape
        // (WEP 2026-05-26).
        let into_iter_outcome = self.resolve_method_call_with(
            MethodCallInput {
                receiver: into_iter_receiver_type,
                receiver_ast: None,
                method_name: "into_iter",
                method_id: None,
                call_id: None,
                defaults_site: None,
                type_args: vec![],
                args: &[],
                expected_type: None,
                span,
                required_trait: None,
            },
            ctx,
        );
        let into_iter_dispatch = into_iter_outcome.dispatch;
        let iter_type = into_iter_outcome.type_id;

        // Iterator-trait conformance check, mirroring the pre-refactor
        // surface error.
        let iterator = self.tysys.compiler_trait(CompilerItem::Iterator);
        if !iterator.is_some_and(|trait_| {
            self.tysys.type_implements_trait(
                &self.annotate_ctx,
                &self.type_lookup(),
                iter_type,
                &trait_,
            )
        }) && !matches!(
            self.tysys.type_table.borrow().get(iter_type),
            ResolvedType::Unknown | ResolvedType::Error | ResolvedType::TypeParam { .. }
        ) {
            let type_name = self.tysys.type_table.borrow().type_name(iter_type);
            let _ = self.emit(TypeError::MissingTraitImpl {
                type_name,
                trait_name: "Iterator".to_string(),
                span,
            });
        }

        // `let mut $iter_N = …;` — `defining_ast_id: None` keeps this
        // synthetic local out of `local_symbols`. Reify rebuilds the `let`;
        // we reserve the local slot here for walk-order parity.
        ctx.add_local(iter_var, iter_type, /* is_mut */ true, None);

        // Make `$for_of_N` visible to a body-level `break $for_of_N`
        // (no existing user does this, but the validation in `resolve_break`
        // would otherwise reject it).
        let ctx = &mut ctx.enter_label(label);

        // `$iter_N.next()` — dispatch on the `$iter_N` local, no AST.
        let next_outcome = self.resolve_method_call_with(
            MethodCallInput {
                receiver: iter_type,
                receiver_ast: None,
                method_name: "next",
                method_id: None,
                call_id: None,
                defaults_site: None,
                type_args: vec![],
                args: &[],
                expected_type: None,
                span,
                required_trait: None,
            },
            ctx,
        );
        let next_dispatch = next_outcome.dispatch;
        let option_type = next_outcome.type_id;

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
            .compiler_variant_case_name(CompilerItem::OptionSome)
            .to_string();
        // `.next()` returns `Option<Item>`. Extract the `Some` payload type
        // for the binding scrutinee. Bind out of the borrow first so the
        // `get_variant_case_payload_type` call below can re-borrow `&mut self`.
        let option_shape: Option<(DefId, Vec<TypeId>)> = {
            let decl = self.tysys.type_def(option_type);
            let is_variant =
                decl.is_some_and(|def| self.type_lookup().variant_cases_of(def).is_some());
            match self.tysys.type_table.borrow().get(option_type).clone() {
                ResolvedType::GenericInstance { type_args, .. } if is_variant => Some((
                    decl.expect("a variant answers with its declaration"),
                    type_args,
                )),
                ResolvedType::Variant { .. } if is_variant => Some((
                    decl.expect("a variant answers with its declaration"),
                    vec![],
                )),
                _ => None,
            }
        };
        let item_type = match option_shape {
            Some((def, type_args)) => {
                self.get_variant_case_payload_type(def, &some_case_name, &type_args, span)
            }
            // A non-Option `.next()` was diagnosed above, so the binding
            // carries the error.
            None => TypeTable::ERROR,
        };

        if let ResolvedType::MutRef(elem) = self.tysys.type_table.borrow().get(item_type).clone() {
            let elem_resolved = self.tysys.type_table.borrow().get(elem).clone();
            let elem_name = self.tysys.type_table.borrow().type_name(elem);
            if matches!(elem_resolved, ResolvedType::TypeParam { .. }) {
                let _ = self.emit(TypeError::CannotMutate {
                    message: format!(
                        "cannot iterate `&mut` over a list of generic type `{elem_name}`: the \
                         element type is not known to support in-place mutation. Constrain it to \
                         a concrete in-place type, or assign by index (`xs[i] = ...`)"
                    ),
                    span,
                });
            } else if self.is_replace_on_assign_element(elem) {
                let _ = self.emit(TypeError::CannotMutate {
                    message: format!(
                        "cannot iterate `&mut` over a list of `{elem_name}`: a write through \
                         `&mut {elem_name}` would be lost (no in-place interior). Assign by index \
                         instead, e.g. `xs[i] = ...`"
                    ),
                    span,
                });
            }
        }

        // WEP 2026-05-26: record the iterator-path
        // dispatch decision so reify can re-emit the synthetic
        // `into_iter()` / `next()` calls without re-dispatching. Only
        // record when both dispatches succeeded (the trait-check error
        // path above bailed without resolving them).
        if let (Some(into_iter), Some(next)) = (into_iter_dispatch, next_dispatch) {
            self.record_for_of_iterator(
                for_of.id,
                ForOfIteratorInfo {
                    into_iter_def: into_iter.method_def,
                    into_iter: into_iter.func,
                    into_iter_self_kind: into_iter.self_kind,
                    into_iter_is_ref_impl: into_iter.is_ref_impl,
                    next_def: next.method_def,
                    next: next.func,
                    next_self_kind: next.self_kind,
                    next_is_ref_impl: next.is_ref_impl,
                    item_type,
                    iter_type,
                },
            );
        }

        // Reify rebuilds the
        // `$for_of_N: { let mut $iter = …; loop { match $iter.next() { … } } }`
        // shape from the AST + the recorded `ForOfIteratorInfo`. This walk binds
        // the loop variable (preserving the binding's real `AstId`) and walks
        // the body for its facts.
        let mut scope = ctx.enter_scope();
        self.resolve_let_pattern(
            &for_of.binding,
            item_type,
            for_of.is_mut,
            span,
            BindingSite::ForOf,
            &mut scope,
        );
        self.resolve_block(&for_of.body, &mut scope, None);
    }

    /// Conservative superset of `boxing.rs`'s boxed set (also names flags /
    /// resource / newtype): a type with no sound `&mut` element write-back.
    fn is_replace_on_assign_element(&self, type_id: TypeId) -> bool {
        match self.tysys.type_table.borrow().get(type_id).clone() {
            ResolvedType::Primitive(_) => true,
            ResolvedType::Enum { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Function { .. }
            | ResolvedType::Resource { .. } => true,
            // The instance's own declaration, not its name looked up again.
            ResolvedType::GenericInstance { def, .. } => {
                self.lookup_variant_case_of_decl(def).is_some()
            }
            ResolvedType::Newtype { base_type, .. } => self.is_replace_on_assign_element(base_type),
            _ => false,
        }
    }

    /// Report a `break` or `continue` written in a for-of the compiler expands:
    /// the expansion leaves neither a loop to name. `kind` names the for-of.
    fn reject_expanded_control_flow(&mut self, body: &Block, kind: &str) -> bool {
        let mut finder = LoopControlFlowFinder { found: None };
        finder.visit_block(body);
        let Some((written, span)) = finder.found else {
            return false;
        };
        let _ = self.emit(TypeError::InvalidPattern {
            message: format!(
                "`{written}` is not allowed inside a {kind} for-of loop (the loop is expanded at compile time)"
            ),
            span,
        });
        true
    }

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
            let _ = self.emit(TypeError::UnknownBreakLabel {
                label: label.clone(),
                span: break_stmt.span,
            });
        }

        // Record the branch this break contributes to its labeled block; a
        // valueless one yields unit. Scan innermost-first, matching the
        // `expected_type` lookup above and WIR `br` depth resolution.
        if let Some(label) = &break_stmt.label {
            let branch_type = value.unwrap_or(TypeTable::UNIT);
            for target in ctx.labeled_block_targets.iter_mut().rev() {
                if &target.label == label {
                    target.break_types.push(branch_type);
                    break;
                }
            }
        }

        // Reify rebuilds the `Break` stmt.
    }

    /// Resolve a `while` or `while let` into a `loop`: the former guarded by
    /// `if !cond { break; }`, the latter by `match expr { pat => B, _ => break }`.
    /// A naked `break` / `continue` in the body already targets that synthesised
    /// loop, so unlike the C-style `for` no label re-targeting is needed.
    pub(super) fn resolve_while(&mut self, w: &WhileStmt, ctx: &mut FunctionContext) {
        // Reify rebuilds the `loop { if !cond { break }
        // B }` (or `loop { match e { pat => B, _ => break } }`) shape from the
        // `DesugarKind::While` / `WhileLetChain` tag + the AST. This walk
        // resolves the condition / scrutinees and walks the body for facts.
        match &w.condition {
            Condition::Expr(cond_expr) => {
                self.record_desugar(w.id, DesugarKind::While);
                self.resolve_condition_expr(cond_expr, ctx);
                self.resolve_block(&w.body, ctx, None);
            }
            Condition::LetChain {
                elements,
                span: cond_span,
            } => {
                self.record_desugar(w.id, DesugarKind::WhileLetChain);
                // The else-branch (an unconditional `break`) is rebuilt by reify;
                // the body walk only binds the chain patterns and walks the
                // then-body for facts.
                self.resolve_let_chain_stmts(
                    elements,
                    &w.body,
                    &mut ctx.enter_scope(),
                    None,
                    false,
                    *cond_span,
                );
            }
        }
    }

    /// Resolve a C-style `for` as `{ init; loop { if !cond { break; } $for_N_body: { B } update; } }`,
    /// where a `continue` breaks `B`'s label so `update` still runs.
    pub(super) fn resolve_for(&mut self, f: &ForStmt, ctx: &mut FunctionContext) {
        self.record_desugar(f.id, DesugarKind::CStyleFor);
        let body_label = for_body_label(ctx.fresh_serial());

        // The outer scope holds `init`'s bindings so the loop body can see
        // them while the surrounding function cannot.
        let ctx = &mut ctx.enter_scope();

        // Reify rebuilds the C-style-for desugar
        // (`{ init; loop { if !cond { break } $for_N_body: { B } update } }`,
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
                    let _ = self.emit(TypeError::InvalidPattern {
                        message: "for-header let-chain must consist of a single \
                                  `let pattern = expr` element"
                            .to_string(),
                        span: *cond_span,
                    });
                    return;
                };

                let scrutinee_type = self.resolve_expr(expr, ctx, None);
                let ctx = &mut ctx.enter_scope();
                self.resolve_if_pattern(pattern, scrutinee_type, ctx, elem_span);
                // Body and update both run inside the pattern scope so they can
                // name the bindings introduced by `pat`.
                self.resolve_for_labeled_body(&body_label, &f.body, ctx);
                self.resolve_for_update(f.update.as_ref(), ctx);
            }
        }
    }

    /// Resolve a for loop's body under its continue-retarget label, which
    /// validates as a known break target inside it.
    fn resolve_for_labeled_body(
        &mut self,
        body_label: &str,
        body: &Block,
        ctx: &mut FunctionContext,
    ) {
        // Reify rebuilds the labeled body block.
        self.resolve_block(body, &mut ctx.enter_label(body_label.to_string()), None);
    }

    /// Resolve a for loop's optional update expression for its facts
    ///.
    fn resolve_for_update(&mut self, update: Option<&Expr>, ctx: &mut FunctionContext) {
        if let Some(u) = update {
            self.resolve_expr(u, ctx, None);
        }
    }
}

impl TypeSystem {
    /// `type_id` with its reference layers peeled, and the reference kind a
    /// binding beneath takes under match ergonomics: any `&` downgrades `&mut`.
    pub(super) fn peel_scrutinee_refs(
        &self,
        type_id: TypeId,
        ref_binding: RefBinding,
    ) -> (TypeId, RefBinding) {
        let tt = self.type_table.borrow();
        let mut current = type_id;
        let mut ref_binding = ref_binding;
        loop {
            match tt.get(current) {
                ResolvedType::Ref(inner) => {
                    current = *inner;
                    ref_binding = RefBinding::Ref;
                }
                ResolvedType::MutRef(inner) => {
                    current = *inner;
                    if ref_binding == RefBinding::None {
                        ref_binding = RefBinding::MutRef;
                    }
                }
                _ => return (current, ref_binding),
            }
        }
    }

    /// Whether a struct pattern's qualifier declares the scrutinee's own head;
    /// an unplaced qualifier is left to its unresolved-name diagnostic.
    fn pattern_qualifier_matches(&self, site: Option<AstId>, head: StructDef) -> bool {
        let Some(written) = site.and_then(|site| self.resolutions.declared_if_walked(site)) else {
            return true;
        };
        head.decl() == Some(written)
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
        Type::Infer(_) => "_".to_string(),
        Type::Error(_) => "<error>".to_string(),
    }
}

/// Why a pattern that tests its value can fail, read off its shape alone.
fn refutable_shape(pattern: &Pattern) -> Option<String> {
    match pattern {
        Pattern::Literal(_) => Some("literal patterns may not match".to_string()),
        Pattern::Variant { variant_name, .. } => Some(format!("`{variant_name}` may not match")),
        Pattern::Or(_) => Some("or-patterns may not match".to_string()),
        Pattern::Range { .. } => Some("range patterns may not match".to_string()),
        Pattern::Ident { .. }
        | Pattern::MutIdent { .. }
        | Pattern::Wildcard
        | Pattern::Tuple(..)
        | Pattern::Struct { .. }
        | Pattern::Typed { .. }
        | Pattern::Error(_) => None,
    }
}

/// The subpatterns a pattern reaches through the scrutinee's shape; `None` for
/// one whose own checks stay sound over an ERROR scrutinee.
fn shape_checked_subpatterns(pattern: &Pattern) -> Option<Vec<&Pattern>> {
    match pattern {
        Pattern::Tuple(patterns, _) => Some(patterns.iter().collect()),
        Pattern::Struct { fields, .. } => Some(fields.iter().map(|f| &f.pattern).collect()),
        Pattern::Variant { bindings, .. } => Some(bindings.iter().collect()),
        Pattern::Ident { .. }
        | Pattern::MutIdent { .. }
        | Pattern::Wildcard
        | Pattern::Literal(_)
        | Pattern::Range { .. }
        | Pattern::Or(_)
        | Pattern::Typed { .. }
        | Pattern::Error(_) => None,
    }
}

/// Collect `(binding_name -> AstId)` from an AST `Pattern`. Used by the
/// or-pattern handler to align every alternative's `defining_ast_id` with
/// the first alternative's source node, so that LSP jump-to-def from a use
/// in the arm body lands on the first alternative's binding.
pub(super) fn collect_ast_pattern_binding_ids(
    pattern: &Pattern,
    out: &mut hashmap::IndexMap<String, AstId>,
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
        Pattern::Typed { pattern, .. } => collect_ast_pattern_binding_ids(pattern, out),
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Error(_) => {}
    }
}

/// Collect binding names, local indices, and types from a TIR pattern for or-pattern validation.
pub(super) fn collect_pattern_bindings_with_index(
    pattern: &TirPattern,
) -> Vec<(String, u32, tir::TypeId)> {
    let mut bindings = Vec::new();
    collect_pattern_bindings_with_index_inner(pattern, &mut bindings);
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    bindings
}

fn collect_pattern_bindings_with_index_inner(
    pattern: &TirPattern,
    out: &mut Vec<(String, u32, tir::TypeId)>,
) {
    match pattern {
        TirPattern::Binding {
            name,
            local_index,
            type_id,
        }
        | TirPattern::Narrow {
            name: Some(name),
            local_index,
            type_id,
            ..
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
        | TirPattern::Range { .. }
        | TirPattern::Narrow { name: None, .. } => {}
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
        TirPattern::Narrow {
            local_index, test, ..
        } => {
            if *local_index == from {
                *local_index = to;
                remap_local_reads(test, from, to);
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

/// A primitive integer's `MIN` / `MAX` written as a pattern (`i32::MIN`), its
/// qualifier resolved at its own site. `None` for anything else.
pub(super) fn primitive_assoc_const_to_i128(
    qualifier: Option<&Type>,
    const_name: &str,
    resolutions: &Resolutions,
) -> Option<i128> {
    let owner = assoc_const_owner(qualifier, resolutions)?;
    let (min, max) = resolutions.defs().primitive(owner)?.int_range()?;
    match const_name {
        "MIN" => Some(min),
        "MAX" => Some(max),
        _ => None,
    }
}

/// The first statement in a for-of's body that names the loop itself. The
/// compiler expands such a loop, so no such name survives.
struct LoopControlFlowFinder {
    found: Option<(&'static str, Span)>,
}

impl AstVisitor for LoopControlFlowFinder {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if self.found.is_some() {
            return;
        }
        match stmt {
            Stmt::Break(b) if b.label.is_none() => self.found = Some(("break", b.span)),
            Stmt::Continue(c) => self.found = Some(("continue", c.span)),
            // A loop owns the `break` and `continue` its body writes. What
            // drives the loop is still the enclosing block's, so the header is
            // walked and the body is not.
            Stmt::While(w) => self.visit_condition(&w.condition),
            Stmt::For(f) => {
                if let Some(init) = &f.init {
                    self.visit_stmt(init);
                }
                if let Some(condition) = &f.condition {
                    self.visit_condition(condition);
                }
                if let Some(update) = &f.update {
                    self.visit_expr(update);
                }
            }
            Stmt::ForOf(f) => self.visit_expr(&f.iterable),
            Stmt::Loop(_) => {}
            // `return` and `task return` leave the enclosing function and
            // `break LABEL` leaves a labeled block. Expansion moves none of
            // those targets, so each is walked for what it carries.
            Stmt::Break(_)
            | Stmt::Let(_)
            | Stmt::Expr(_)
            | Stmt::Return(_)
            | Stmt::TaskReturn(_)
            | Stmt::If(_)
            | Stmt::Match(_)
            | Stmt::Assert(_)
            | Stmt::LabeledBlock(_)
            | Stmt::Item(_)
            | Stmt::Error(_) => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if self.found.is_some() {
            return;
        }
        // A closure is a function boundary, so what it writes is its own.
        if matches!(expr, Expr::Closure(_)) {
            return;
        }
        walk_expr(self, expr);
    }

    /// A declaration written in the body carries its own bodies.
    fn visit_item(&mut self, _item: &Item) {}
}
