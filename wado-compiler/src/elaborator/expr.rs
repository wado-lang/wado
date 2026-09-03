//! Expression resolution (literals, identifiers, field access, index,
//! if-expressions, match, cast, struct/tuple literals, etc.).

use super::scope::BinderInScope;
use super::sig::AssocConstSig;
use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{
    self, AstId, AstVisitor, Condition, Expr, IfExpr, LabeledBlockExpr, Literal, MatchArm,
};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, mangle_generic_name};
use crate::tir::{FunctionRef, ResolvedType, TirField, TirStruct, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::call::turbofish_holes;
use super::infer::InferCtx;
use super::instantiate::Instantiation;
use super::typecheck::{TypeCheckResult, check_assignable};
use super::types::{FunctionContext, TypeError, VarRef};
use super::util;

/// Outcome of trying to derive type arguments for a generic function
/// reference from an expected `fn(...)` (or `&fn(...)`) type. Distinguishes
/// the three failure modes the caller treats differently.
enum FuncRefInference {
    /// Every real type parameter was bound from the expected signature.
    Ok(Vec<TypeId>),
    /// The expected type is a `fn(...)` shape but its parameter count
    /// disagrees with the declaration. Surfaced as a focused diagnostic.
    ArityMismatch {
        expected_params: usize,
        found_params: usize,
    },
    /// The expected type is not a function shape (no `fn(...)` directly
    /// or via `&`/`&mut`), or some parameters could not be bound. Callers
    /// fall through to the generic bare-reference diagnostic.
    NotApplicable,
}

/// The literal's source text when it denotes an integer — the shape whose type
/// is settled by defaulting to `i32` rather than by a coercion.
pub(super) fn int_literal_repr(lit: &ast::LiteralExpr) -> Option<&str> {
    match &lit.value {
        Literal::Number(repr) if !util::is_float_only_literal(repr) => Some(repr.as_str()),
        _ => None,
    }
}

/// An integer literal standing as an operand, bare or negated — the shape
/// reify re-types to a cast's target width.
fn int_literal_operand(expr: &Expr) -> Option<(&ast::LiteralExpr, &str)> {
    let lit = match expr {
        Expr::Literal(lit) => lit,
        Expr::Unary(unary) => negated_literal(unary)?,
        _ => return None,
    };
    int_literal_repr(lit).map(|repr| (lit, repr))
}

/// The literal `-NUM` negates, which both range checks read as one literal so
/// the boundary is the signed minimum.
pub(super) fn negated_literal(unary: &ast::UnaryExpr) -> Option<&ast::LiteralExpr> {
    match (&unary.op, &unary.expr) {
        (ast::UnaryOp::Neg, Expr::Literal(lit)) => Some(lit),
        _ => None,
    }
}

/// How a subscript is being used, which decides the indexing trait it selects:
/// `&mut xs[i]` reaches the element through `IndexRefMut` so the mutability rides
/// on the signature, while every other position reads it shared.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IndexAccess {
    /// `xs[i]` — value semantics hand back a copy, through `IndexValue`.
    Value,
    /// `&xs[i]` — `IndexRef` aliases the element in place.
    Shared,
    /// `&mut xs[i]` — `IndexRefMut` carries the mutability in its signature.
    Mutable,
}

/// Per spread base in an anonymous literal: whether it is a key-value map, and
/// (for a plain struct base) its defining module plus field list
/// `(name, concrete type, declared index, visibility)`.
type BaseSpreadInfo = (
    bool,
    Option<(
        ModuleSource,
        Vec<(String, TypeId, u32, crate::ast::Visibility)>,
    )>,
);

/// One field of an anonymous composition's union, and where its value comes from.
pub(super) struct UnionField {
    pub(super) name: String,
    pub(super) type_id: TypeId,
    pub(super) source: UnionSource,
}

/// Where a composed union field's value is read from.
#[derive(Clone, Copy)]
pub(super) enum UnionSource {
    /// An explicit `name: value` field, by index into `struct_lit.fields`.
    Explicit(usize),
    /// A spread base's field: `spread_base_types[base_idx].field_index`.
    Base { base_idx: usize, field_index: u32 },
}

/// The selected indexing impl's key type must be the one the index expression
/// was elaborated against: both sides ask the same question, so a disagreement
/// means one of them changed. This used to be *repaired* by resolving the index
/// a second time, elaborating one AST node twice.
fn debug_assert_key_matches(impl_key: Option<TypeId>, elaborated: TypeId) {
    if let Some(key) = impl_key {
        debug_assert_eq!(
            key, elaborated,
            "indexing impl selected for a key type the index was not elaborated against"
        );
    }
}

/// Peel references off `type_id` and, if it names a struct, return its
/// `(name, defining module, type arguments)`. Shared by the resolve and reify
/// spread-field projections so both classify a base identically.
pub(super) fn peel_to_struct(
    tt: &TypeTable,
    type_id: TypeId,
) -> Option<(crate::tir::StructDef, Vec<TypeId>)> {
    let peeled = tt.peel_refs(type_id);
    match tt.get(peeled) {
        // An anonymous shape names no declaration, so the head is what
        // answers for it — a `DefId` would drop the whole anon-composition
        // path on the floor.
        ResolvedType::Struct { def, .. } => Some((*def, Vec::new())),
        ResolvedType::GenericInstance { def, type_args } => {
            Some((crate::tir::StructDef::Decl(*def), type_args.clone()))
        }
        _ => None,
    }
}

/// The union field plan for anonymous composition `{ ..a, field: v, ..b }`:
/// members in source order, last contributor winning a name collision but
/// keeping the first-occurrence position. Shared by resolve and reify so both
/// agree on the shape. `base_field_lists[i]` is `(name, type, declared index)`.
pub(super) fn compose_union_plan(
    struct_lit: &ast::StructLiteralExpr,
    base_field_lists: &[Vec<(String, TypeId, u32)>],
    explicit_field_types: &[TypeId],
) -> Vec<UnionField> {
    // `IndexMap::insert` keeps an existing key's position and updates its value,
    // giving first-occurrence order with last-contributor value/source.
    let mut merged: IndexMap<String, UnionField> = IndexMap::default();
    let mut apply = |name: String, type_id: TypeId, source: UnionSource| {
        merged.insert(
            name.clone(),
            UnionField {
                name,
                type_id,
                source,
            },
        );
    };

    for member in struct_lit.members() {
        match member {
            ast::LiteralMember::Spread(si, _) => {
                for (fname, fty, fidx) in &base_field_lists[si] {
                    apply(
                        fname.clone(),
                        *fty,
                        UnionSource::Base {
                            base_idx: si,
                            field_index: *fidx,
                        },
                    );
                }
            }
            ast::LiteralMember::Field(pos, field) => {
                apply(
                    field.name.clone(),
                    explicit_field_types[pos],
                    UnionSource::Explicit(pos),
                );
            }
        }
    }
    merged.into_values().collect()
}

/// What retargeting a branch tail touches: the literals that adopt the type,
/// and the nested `if` / `match` nodes whose recorded type reify reads back as
/// their result, so it has to follow.
#[derive(Default)]
struct NumericLiteralTails<'a> {
    literals: Vec<&'a ast::Expr>,
    branches: Vec<AstId>,
}

/// Whether a branch already produces `target`; `never` fits any of them.
fn agrees_with_target(ty: TypeId, target: TypeId) -> bool {
    ty == target || ty == TypeTable::NEVER
}

/// A struct-literal field as the body walk knows it: the name it was written
/// under, its declared position, and the type its value resolved to.
pub(super) struct ResolvedField {
    name: String,
    type_id: TypeId,
    field_index: u32,
    /// Where the value was written, for a diagnostic that names this field.
    span: Span,
}

/// Pair each written field with the declared type of its slot. `fields` is
/// neither ordered nor complete, so the index decides and the name confirms it.
fn declared_pairs<'a>(
    fields: &'a [ResolvedField],
    declared: &'a [TypeId],
    declared_names: &'a [&'a str],
) -> impl Iterator<Item = (&'a ResolvedField, TypeId)> {
    debug_assert_eq!(declared.len(), declared_names.len());
    fields.iter().filter_map(|field| {
        let slot = field.field_index as usize;
        if declared_names.get(slot) != Some(&field.name.as_str()) {
            return None;
        }
        declared.get(slot).map(|&type_id| (field, type_id))
    })
}

/// Shape projection of a match-arm pattern, used solely for exhaustiveness /
/// overlap analysis on the AST. It captures exactly the pattern shape the
/// checks read, one distinction per `TirPattern` distinction they depend on:
/// catch-all (wildcard / binding / reversed-or-empty range / bad range bound),
/// enum / variant case names, bool literals, integer ranges and points, an
/// opaque `Other` (strings, structs, tuples, constant-value patterns), and
/// `Or` alternatives.
enum ExhPattern {
    CatchAll,
    EnumCase(String),
    VariantCase(String),
    BoolLit(bool),
    /// Inclusive integer range `[lo, hi]`.
    Range(i128, i128),
    IntLit(i128),
    Other,
    Or(Vec<ExhPattern>),
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve an AST expression to its TIR form. Records the resolved
    /// [`TypeId`] in [`super::sem::TypeAnnotations::expression_types`]
    /// before returning so reify can read the type without re-running
    /// inference. All sub-expression recursion routes back through this
    /// entry point, so every visited [`AstId`] leaves an annotation —
    /// including operands of binary ops, call arguments, and trailing
    /// block values.
    pub(super) fn resolve_expr(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        let ast_id = expr.id();
        let type_id = self.resolve_expr_inner(expr, ctx, expected_type);
        self.record_expression_type(ast_id, type_id);
        type_id
    }

    /// Resolve an expression standing in condition position, which must be
    /// `bool`: nothing coerces to it. `bool` is checked, never passed down as an
    /// expected type — an expectation reaches the operands, where it is wrong.
    pub(super) fn resolve_condition_expr(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let type_id = self.resolve_expr(expr, ctx, None);
        if type_id == TypeTable::BOOL
            || type_id == TypeTable::UNKNOWN
            || type_id == TypeTable::ERROR
        {
            return type_id;
        }
        let (expected, found) = self
            .tysys
            .type_table
            .borrow()
            .type_names_for_mismatch(TypeTable::BOOL, type_id);
        let _ = self.emit(TypeError::TypeMismatch {
            expected,
            found,
            span: expr.span(),
        });
        TypeTable::BOOL
    }

    fn resolve_expr_inner(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Power-assert capture hook. While `desugar_assert` is resolving
        // an assert condition, the scanner-flagged sub-expressions are
        // extracted into `let __vK = <resolved>;` bindings and replaced
        // with `Local(__vK)`. Common case (no assert in flight): a
        // single `Option` discriminant check, so the cost on the hot
        // path is negligible. See `elaborator/assert.rs` for the design.
        if let Some(cap_ctx) = ctx.assert_capture_ctx.as_ref() {
            let ast_id = expr.id();
            if let Some(slot_idx) = cap_ctx.slot_for(ast_id) {
                return self.resolve_with_assert_capture(
                    ast_id,
                    slot_idx,
                    expr,
                    ctx,
                    expected_type,
                );
            }
        }

        // Compound-assign once-eval hook (see `compound_hoist_types`).
        if !ctx.compound_hoist_types.is_empty()
            && let Some(&type_id) = ctx.compound_hoist_types.get(&expr.id())
        {
            return type_id;
        }

        // Try literal coercion when expected type is known
        if let Some(target_type) = expected_type
            && let Some(coerced) = self.try_coerce(expr, ctx, target_type)
        {
            return coerced;
        }

        // Main expression dispatch
        match expr {
            Expr::Literal(lit) => self.resolve_literal(lit, expected_type),
            Expr::Ident(ident) => self.resolve_ident(ident, ctx, expected_type),
            Expr::Binary(binary) => self.resolve_binary(binary, ctx, expected_type),
            Expr::Unary(unary) => self.resolve_unary(unary, ctx, expected_type),
            Expr::Assign(assign) => self.resolve_assign(assign, ctx),
            Expr::TupleComprehension(comp) => self.resolve_tuple_comprehension(comp, ctx),
            Expr::Call(call) => self.resolve_call(call, ctx, expected_type),
            Expr::MethodCall(method_call) => {
                self.resolve_method_call(method_call, ctx, expected_type)
            }
            Expr::StaticMethodCall(static_call) => {
                self.resolve_static_method_call(static_call, ctx)
            }
            Expr::FieldAccess(field_access) => self.resolve_field_access(field_access, ctx),
            Expr::Index(index) => self.resolve_index(index, ctx, IndexAccess::Value),
            Expr::Block(block) => {
                // Walk the block for its facts; reify rebuilds the `Block`
                // node. Read the overall type from `expression_types` (AST
                // level) via the shared block-result rule so a trailing
                // `if/else` propagates its branch-agreed type, not `Unit`.
                self.resolve_block_value(block, ctx, expected_type);
                self.ast_block_result_type(block)
            }
            Expr::If(if_expr) => self.resolve_if_expr(if_expr, ctx, expected_type),
            Expr::Match(match_expr) => self.resolve_match_expr(match_expr, ctx, expected_type),
            Expr::Closure(closure) => self.resolve_closure(closure, ctx, expected_type),
            Expr::TemplateString(template) => self.resolve_template_string(template, ctx),
            Expr::Cast(cast) => self.resolve_cast(cast, ctx),
            Expr::StructLiteral(struct_lit) => {
                self.resolve_struct_literal(struct_lit, ctx, expected_type)
            }
            Expr::CompoundAssign(compound) => self.resolve_compound_assign(compound, ctx),
            Expr::ComparisonChain(chain) => self.desugar_comparison_chain(chain, ctx),
            Expr::TupleLiteral(tuple_lit) => {
                self.resolve_tuple_literal(tuple_lit, ctx, expected_type)
            }
            Expr::LabeledBlock(lb) => {
                ctx.push_labeled_block_frame(lb.label.clone(), expected_type);

                ctx.enter_scope();
                // A labeled block yields via `break label: value`, not a tail
                // expression, so its trailing statement stays in statement
                // position (a discarded tail `match` may have arms of
                // differing types).
                self.resolve_block(&lb.block, ctx, expected_type);
                ctx.exit_scope();

                let target = ctx.pop_labeled_block_frame();

                // Reify rebuilds the `LabeledBlock` from the AST, re-running
                // the same unification; project only the result type.
                self.unify_labeled_block(lb, &target.break_types, expected_type)
            }
            Expr::Matches(m) => self.desugar_matches_expr(m, ctx, expected_type),
            Expr::Spread(..) => {
                panic!("Spread expression should only appear inside TupleLiteral handling")
            }
            Expr::TryOp(qm) => self.resolve_question_mark(qm, ctx, expected_type),
            Expr::Range(range) => self.resolve_range(range, ctx),
            Expr::WithHandler(w) => self.resolve_with_handler(w, ctx, expected_type),
            Expr::Resume(r) => self.resolve_resume(r, ctx),
            // Parser error-recovery placeholder: the syntax error was already
            // reported, so resolve to the error type to suppress cascades.
            Expr::Error(_e) => TypeTable::ERROR,
        }
    }

    /// The type of a labeled block expression: its `break` values and its
    /// fall-through tail unified into one, every disagreement reported.
    fn unify_labeled_block(
        &mut self,
        lb: &LabeledBlockExpr,
        break_types: &[TypeId],
        expected_type: Option<TypeId>,
    ) -> TypeId {
        let tail_type = self.labeled_block_tail_type(lb);
        let branch_types: Vec<TypeId> = break_types
            .iter()
            .copied()
            .chain(std::iter::once(tail_type))
            .collect();
        let result_type =
            expected_type.unwrap_or_else(|| self.representative_branch_type(&branch_types));

        // Report a `break label: null` whose `Option<...>` inner could not be
        // inferred against a resolved non-`Option` result. A type still UNKNOWN
        // means every break was a bare `null`, which the first call reports.
        if !self.report_uninferable_result(result_type, lb.span, "labeled block") {
            self.report_unresolved_null_breaks(result_type, &lb.block, &lb.label);
        }

        for &branch_type in &branch_types {
            if branch_type != TypeTable::NEVER {
                self.check_branch_type(branch_type, result_type, lb.span);
            }
        }
        result_type
    }

    /// What the path reaching the block's end yields, or `never` when no path
    /// does. A trailing `loop` left only by `break label` reaches no tail.
    fn labeled_block_tail_type(&self, lb: &LabeledBlockExpr) -> TypeId {
        if self.ast_labeled_block_falls_through(&lb.block, &lb.label) {
            self.ast_block_result_type(&lb.block)
        } else {
            TypeTable::NEVER
        }
    }

    /// The branch that types a block the use site expects nothing from: the
    /// first carrying a real value. A `never`, `unit` or unresolved branch
    /// steps aside, and a block holding only those takes its first.
    fn representative_branch_type(&self, branch_types: &[TypeId]) -> TypeId {
        let tt = self.tysys.type_table.borrow();
        branch_types
            .iter()
            .copied()
            .find(|&t| t != TypeTable::NEVER && t != TypeTable::UNIT && !tt.is_indefinite(t))
            .or_else(|| {
                branch_types
                    .iter()
                    .copied()
                    .find(|&t| t != TypeTable::NEVER)
            })
            .unwrap_or(branch_types[0])
    }

    /// Range-check an integer literal against the `i32` it defaults to, the
    /// boundary chosen by `negated` — `-NUM` is one literal, so `-2147483648`
    /// fits where the bare `2147483648` does not.
    ///
    /// Only the defaulted case. An expectation still pending here is one no
    /// coercion took — a type parameter awaiting inference — and it re-coerces
    /// the literal afterwards, checking the range against the type it lands on.
    pub(super) fn check_default_int_literal(&mut self, repr: &str, negated: bool, span: Span) {
        let Some(value) = self.check_int_literal_parses(repr, span) else {
            return;
        };
        let message = {
            let table = self.tysys.type_table.borrow();
            if negated {
                util::check_int_range_negative(value, TypeTable::I32, &table, repr)
            } else {
                util::check_int_range_positive(value, TypeTable::I32, &table, repr)
            }
        };
        if let Some(message) = message {
            let _ = self.emit(TypeError::InvalidLiteral { message, span });
        }
    }

    /// Parse an integer literal, reporting a malformed or wider-than-`u128`
    /// one. Always this walk's job: nothing downstream reports it, and reify
    /// reads such a literal as `0`.
    pub(super) fn check_int_literal_parses(&mut self, repr: &str, span: Span) -> Option<u128> {
        match util::parse_u128_literal(repr) {
            Ok(value) => Some(value),
            Err(message) => {
                let _ = self.emit(TypeError::InvalidLiteral { message, span });
                None
            }
        }
    }

    pub(super) fn resolve_literal(
        &mut self,
        lit: &ast::LiteralExpr,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Reify rebuilds every literal node from the AST; the
        // body walk only needs the literal's type and its parse / unescape
        // diagnostics. The returned value is a placeholder, so this projects
        // only the type while preserving the validation side effects.
        match &lit.value {
            Literal::Number(repr) => {
                // Default type: i32 if integer-compatible, f64 if float-only
                if util::is_float_only_literal(repr) {
                    // Must be float (has decimal point or negative exponent)
                    if let Err(message) = util::parse_float_literal(repr) {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                    }
                    TypeTable::F64
                } else {
                    if expected_type.is_none() {
                        self.check_default_int_literal(repr, false, lit.span);
                    } else {
                        self.check_int_literal_parses(repr, lit.span);
                    }
                    TypeTable::I32
                }
            }
            Literal::Bool(_) => TypeTable::BOOL,
            Literal::Char(raw) => {
                if let Err(message) = util::unescape_char(raw) {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                }
                TypeTable::CHAR
            }
            Literal::String(raw) => {
                let string_type = self.get_string_struct_type();
                if let Err(message) = util::unescape_string(raw) {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                }
                string_type
            }
            Literal::Bytes(raw) => {
                let byte_list_type = self.tysys.type_table.borrow_mut().make_byte_list();
                if let Err(message) = util::unescape_bytes(raw) {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                }
                byte_list_type
            }
            Literal::Byte(raw) => {
                if let Err(message) = util::unescape_byte(raw) {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message,
                        span: lit.span,
                    });
                }
                TypeTable::U8
            }
            // `Option` of the bottom type: a value of every `Option<T>` and of
            // no other type. `Unknown` here would instead defer every check it
            // meets, which is how a `null` used to reach a non-nullable slot.
            Literal::Null => self
                .tysys
                .type_table
                .borrow_mut()
                .make_option(TypeTable::NEVER),
            Literal::Unit => TypeTable::UNIT,
            Literal::LocationFile => {
                // #file - returns the current module source as a string
                self.get_string_struct_type()
            }
            Literal::LocationLine => {
                // #line - returns the line number (1-indexed)
                TypeTable::I32
            }
            Literal::LocationFunction => {
                // #function - returns the current function name
                self.get_string_struct_type()
            }
            Literal::DataSection => {
                // #data - returns the __DATA__ section content as a String
                let data = self
                    .tysys
                    .signatures
                    .data_section(&self.current_module_source)
                    .map(str::to_owned);
                let string_type = self.get_string_struct_type();
                if data.is_none() {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message: "`#data` requires a `__DATA__` section in the source file"
                            .to_owned(),
                        span: lit.span,
                    });
                }
                string_type
            }
            Literal::IncludeStr(raw_path) => {
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let string_type = self.get_string_struct_type();
                if let Some(bytes) = self.tysys.included_files.get(&key) {
                    if std::str::from_utf8(bytes).is_err() {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message: format!("file is not valid UTF-8: \"{raw_path}\""),
                            span: lit.span,
                        });
                    }
                } else {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message: format!("file not found: \"{raw_path}\""),
                        span: lit.span,
                    });
                }
                string_type
            }
            Literal::IncludeBytes(raw_path) => {
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let array_u8_type = self.tysys.type_table.borrow_mut().make_byte_list();
                if !self.tysys.included_files.contains_key(&key) {
                    let _ = self.emit(TypeError::InvalidLiteral {
                        message: format!("file not found: \"{raw_path}\""),
                        span: lit.span,
                    });
                }
                array_u8_type
            }
        }
    }

    /// Resolve an identifier expression
    pub(super) fn resolve_ident(
        &mut self,
        ident: &ast::IdentExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Canonicalize `ns::member` to its `ns$member` alias; the registries
        // below are keyed by these aliases. The rewritten ident keeps the
        // original `id` so use→def edges still resolve back to the user's text.
        let canonical_ident;
        let ident = if let Some(canon) = self.sem.imports.canonical_ns_ref(&ident.name) {
            canonical_ident = ast::IdentExpr {
                id: ident.id,
                name: canon,
                segments: ident.segments.clone(),
                type_args: ident.type_args.clone(),
                type_args_on_prefix: ident.type_args_on_prefix,
                span: ident.span,
            };
            &canonical_ident
        } else {
            ident
        };

        // Check local variables, including captures from outer scope
        if let Some(var_ref) = ctx.lookup_or_capture(&ident.name) {
            match var_ref {
                VarRef::Local {
                    index: _,
                    type_id,
                    defining_ast_id,
                } => {
                    self.record_reference_opt(ident.id, defining_ast_id);
                    // Reify rebuilds the `Local` (`reify_ident`);
                    // record the place so `assign_to_target` can classify an
                    // ident l-value without the resolved `kind`.
                    self.record_assign_place(ident.id, super::sem::types::AssignPlace::Local);
                    return type_id;
                }
                VarRef::Capture {
                    index: _,
                    type_id,
                    defining_ast_id,
                } => {
                    self.record_reference_opt(ident.id, defining_ast_id);
                    // Reify rebuilds the `Capture`. A by-value
                    // capture is not an l-value, so no place is recorded.
                    return type_id;
                }
                VarRef::DerefCapture {
                    index: _,
                    ref_type_id,
                    inner_type_id,
                    defining_ast_id,
                } => {
                    self.record_reference_opt(ident.id, defining_ast_id);
                    // Deref capture: `*self.__capture_N` where the field holds
                    // `&mut T` (mutable closure capture). Reify
                    // rebuilds the `*capture` shape; record the place so the
                    // assign path can validate it (assignable iff the captured
                    // reference is `&mut`, not a shared `&`).
                    let through_mut_ref = !matches!(
                        self.tysys.type_table.borrow().get(ref_type_id),
                        ResolvedType::Ref(_)
                    );
                    self.record_assign_place(
                        ident.id,
                        super::sem::types::AssignPlace::DerefCapture { through_mut_ref },
                    );
                    return inner_type_id;
                }
            }
        }

        // Check for associated constants (e.g., f64::PI, i32::MAX). The
        // constant's body is *foreign* AST owned by `const_module`; we
        // re-resolve it here only for the consumer's inference side effects.
        // Its per-`AstId` facts carry the const body's own globally-unique
        // ids, so they cannot clobber a consumer node (the historic
        // cross-module collision, issue #1342). Reify produces the const's
        // TIR under `with_const_module_perspective(const_module)` and does
        // not read these consumer-side entries.
        if let Some(assoc) = self.associated_constant_of_path(ident) {
            self.check_inherent_member_visibility(
                assoc.inherent_visibility,
                Some(&assoc.module),
                MemberOwner::Named(assoc_const_owner_segment(ident)),
                ident.segments.last().map_or(&ident.name, |s| &s.name),
                super::types::ImplMemberKind::AssociatedConstant,
                Some(ident.id),
                ident.span,
            );
            // Not an l-value.
            let vantage = (assoc.module.clone(), assoc.value.id().space());
            let const_module = assoc.module.clone();
            self.with_default_scope_module(Some(const_module), |s| {
                s.with_foreign_vantage(Some(vantage), |s| {
                    s.resolve_expr(&assoc.value, ctx, Some(assoc.ty))
                })
            });
            return assoc.ty;
        }

        // Check for qualified variant case names like Color::Red (without parentheses)
        if ident.name.contains("::")
            && let Some(result) = self.resolve_qualified_case(ident, expected_type)
        {
            return result;
        }

        // Check for global variables in current module
        if let Some(&(ty, mutable)) = self.sem.decls.current_module_globals.get(&ident.name) {
            self.record_item_reference_by_name(ident.id, &ident.name);
            // Reify rebuilds the `GlobalVarGet`; record the place so
            // `assign_to_target` validates global mutability + emits the
            // `GlobalVarSet` projection without the resolved `kind`.
            self.record_assign_place(
                ident.id,
                super::sem::types::AssignPlace::Global {
                    name: ident.name.clone(),
                    mutable,
                },
            );
            return ty;
        }

        // Check for imported global variables
        if let Some((original_name, ty, mutable)) = self
            .sem
            .decls
            .imported_globals
            .get(&ident.name)
            .map(|(_src, orig, ty, m)| (orig.clone(), *ty, *m))
        {
            self.record_item_reference_by_name(ident.id, &ident.name);
            // Reify rebuilds the imported `GlobalVarGet` (keyed by
            // the original name); record the place for the assign path. Keep
            // the original (source) name for the immutable-global diagnostic,
            // matching the pre-7-B message that read it off `GlobalVarGet`.
            self.record_assign_place(
                ident.id,
                super::sem::types::AssignPlace::Global {
                    name: original_name,
                    mutable,
                },
            );
            return ty;
        }

        // Check if it's a known function (function reference)
        if self
            .sem
            .decls
            .function_return_types
            .contains_key(&ident.name)
            || self.sem.decls.imported_functions.contains(&ident.name)
        {
            return self.resolve_func_ref_ident(ident, expected_type);
        }

        // Check if it's a prelude function (panic, unreachable)
        // These are defined in core:rt and re-exported by core:prelude
        if matches!(ident.name.as_str(), "panic" | "unreachable") {
            return TypeTable::UNKNOWN;
        }

        // A default expression looks its identifier up in the callee's lexical
        // scope, which is what gives it the definition module's private globals
        // and functions (issue #1486).
        if let Some(fallback) = self.annotate_ctx.default_scope_module.clone()
            && fallback != self.current_module_source
            && let Some(result) = self.resolve_ident_in_fallback_module(ident, &fallback)
        {
            return result;
        }

        // Unknown variable - report error
        let _ = self.emit(TypeError::UnknownIdentifier {
            name: ident.name.clone(),
            span: ident.span,
        });
        TypeTable::ERROR
    }

    /// Look up an identifier in the callee module's global scope during
    /// default-expression resolution. Supports globals and function refs.
    fn resolve_ident_in_fallback_module(
        &mut self,
        ident: &ast::IdentExpr,
        fallback: &ModuleSource,
    ) -> Option<TypeId> {
        // Reify resolves the fallback-module global / `FuncRef` its own
        // way; project the type only. This default-expr path is never an
        // assignment target, so no place is recorded.
        let (owner, name) = self.declaring_module_of_ident(&ident.name, fallback);
        if let Some((ty, _)) = self.tysys.signatures.global(&owner, &name) {
            return Some(ty);
        }
        let sig = self.free_function_sig_at(ident.id)?.clone();
        Some(
            self.compute_func_ref_type_from_sig(&sig, &[])
                .unwrap_or(TypeTable::UNKNOWN),
        )
    }

    /// Where `name` is *declared*, as seen from `fallback`. The signature
    /// tables are keyed by declaring module, so a name `fallback` merely
    /// imported or re-exported is not found under `fallback` itself.
    fn declaring_module_of_ident(
        &self,
        name: &str,
        fallback: &ModuleSource,
    ) -> (ModuleSource, String) {
        if self.tysys.signatures.global(fallback, name).is_some()
            || self
                .decl_in_module(fallback, name)
                .is_some_and(|def| self.tysys.signatures.function_sig(def).is_some())
        {
            return (fallback.clone(), name.to_string());
        }
        // Imports and re-exports are different maps; a default may name either.
        let resolved = self
            .symbols
            .imported(fallback, name)
            .or_else(|| self.symbols.lookup_in_module(fallback, name));
        match resolved {
            Some(symbol) => (symbol.module.clone(), symbol.name.clone()),
            None => (fallback.clone(), name.to_string()),
        }
    }

    /// A turbofish on a case path (`Maybe::<i32>::Nothing`) must name exactly
    /// the declaring type's parameters; an enum or a flags type declares none.
    fn check_case_turbofish_arity(
        &mut self,
        ident: &ast::IdentExpr,
        type_name: &str,
        expected: usize,
    ) {
        if ident.type_args.is_empty() || ident.type_args.len() == expected {
            return;
        }
        let expected_text = match expected {
            0 => "no type arguments".to_string(),
            1 => "1 type argument".to_string(),
            n => format!("{n} type arguments"),
        };
        let found = ident.type_args.len();
        let _ = self.emit(TypeError::InvalidLiteral {
            message: format!("`{type_name}` takes {expected_text}, the turbofish supplies {found}"),
            span: ident.span,
        });
    }

    /// Resolve a qualified case reference `Type::Case` — a payload-less variant
    /// case, an enum case, or a flags member. `None` when the prefix names no
    /// such type, so the caller can try other interpretations;
    /// `Some(TypeTable::ERROR)` when it is invalid.
    fn resolve_qualified_case(
        &mut self,
        ident: &ast::IdentExpr,
        expected_type: Option<TypeId>,
    ) -> Option<TypeId> {
        let pos = ident.name.find("::")?;
        let prefix = &ident.name[..pos];
        let suffix = &ident.name[pos + 2..];

        // The type a case is qualified with is the segment just before the
        // case's own name — the head for `Color::Red`, the second segment for
        // `ns::Color::Red` — and the resolve walk answered for it in the
        // module that wrote it. So a reference inside a foreign default
        // resolves in the declaring module without a second, module-scoped
        // lookup beside the first.
        let owner = ident
            .segments
            .len()
            .checked_sub(2)
            .and_then(|i| self.tysys.resolutions.declared(ident.segments[i].id));
        // A newtype reaches its base's members and keeps its own identity, so
        // `C::Green` on `type C = Color` reads Color's cases and is a `C` —
        // the implicit form of `Color::Green as C`.
        let through_newtype = owner.and_then(|def| {
            super::types::newtype_member_owner(&self.type_lookup(), &self.tysys, def)
        });
        let owner = through_newtype.map(|(base, _)| base).or(owner);
        macro_rules! lookup_case {
            ($of:ident) => {
                owner.and_then(|def| self.type_lookup().$of(def)).cloned()
            };
        }

        let variant_info = lookup_case!(variant_cases_of);
        if let Some(variant_info) = variant_info {
            // Find the case by name
            if let Some((_case_index, case_data)) = variant_info
                .cases
                .iter()
                .enumerate()
                .find(|(_, c)| c.name == suffix)
                .map(|(i, c)| (i, c.clone()))
            {
                self.record_qualified_case(ident, prefix, case_data.ast_id);
                self.check_case_turbofish_arity(ident, prefix, variant_info.type_params.len());
                // Unit variant - payload must be unit type
                let payload_is_unit = matches!(
                    self.tysys.type_table.borrow().get(case_data.payload),
                    ResolvedType::Unit
                );
                if !payload_is_unit {
                    let _ = self.emit(TypeError::ArgumentCountMismatch {
                        expected: 1,
                        found: 0,
                        span: ident.span,
                    });
                    return Some(TypeTable::ERROR);
                }

                // Infer variant type for generic variants
                let variant_type = if variant_info.type_params.is_empty() {
                    self.tysys
                        .type_table
                        .borrow()
                        .type_id_of_decl(variant_info.defined_at)
                } else {
                    {
                        // `Maybe::<i32>::Nothing` pins its slots here: a
                        // payload-less case has no payload to infer from, so
                        // the turbofish is the only source besides the
                        // expected type.
                        let holes = turbofish_holes(&ident.type_args);
                        let explicit_args: Vec<TypeId> = ident
                            .type_args
                            .iter()
                            .enumerate()
                            .map(|(i, t)| {
                                if holes[i] {
                                    TypeTable::UNKNOWN
                                } else {
                                    self.resolve_type(t)
                                }
                            })
                            .collect();
                        let inferred = self.tysys.infer_variant_type_args(
                            &self.annotate_ctx,
                            &variant_info,
                            &case_data,
                            None,
                            expected_type,
                            &explicit_args,
                            &holes,
                        );
                        self.defer_uninferable_variant(inferred, prefix, &variant_info, ident.span)
                    }
                };

                // Record generic type args for
                // payload-less variant references that compile to a
                // `VariantConstruct` (e.g. `Option::<i32>::None`).
                let type_args = match self.tysys.type_table.borrow().get(variant_type) {
                    ResolvedType::GenericInstance { type_args, .. } => type_args.clone(),
                    _ => Vec::new(),
                };
                self.record_generic_instantiation(ident.id, type_args, variant_type);

                // Reify rebuilds the payload-less
                // `VariantConstruct` from the AST + recorded generic
                // instantiation. Not an l-value.
                return Some(through_newtype.map_or(variant_type, |(_, named)| named));
            }
        }

        // Check for enum case: Color::Red (enums have no payload)
        let enum_info = lookup_case!(enum_cases_of);
        if let Some(enum_info) = enum_info
            && let Some(case_data) = enum_info.find_case(suffix).cloned()
        {
            self.record_qualified_case(ident, prefix, case_data.ast_id);
            self.check_case_turbofish_arity(ident, prefix, 0);
            let enum_type = self
                .tysys
                .type_table
                .borrow()
                .type_id_of_decl(enum_info.defined_at);

            // Reify rebuilds the `EnumConstruct`. Not an l-value.
            return Some(through_newtype.map_or(enum_type, |(_, named)| named));
        }

        // Check for flags member: PathFlags::SymlinkFollow
        // Flags members are bitmask integers (1 << index) represented as IntLiteral
        let flags_info = lookup_case!(flags_members_of);
        if let Some(flags_info) = flags_info
            && let Some(member) = flags_info
                .members
                .iter()
                .find(|m| m.name == suffix)
                .cloned()
        {
            self.record_qualified_case(ident, prefix, member.ast_id);
            self.check_case_turbofish_arity(ident, prefix, 0);
            return Some(through_newtype.map_or(flags_info.type_id, |(_, named)| named));
        }
        None
    }

    /// Build a function type from a canonical signature. With `type_args`
    /// empty the function must be non-generic (effect-only params count as
    /// non-generic); a non-empty `type_args` substitutes the signature's
    /// `TypeParam` slots positionally.
    pub(super) fn compute_func_ref_type_from_sig(
        &mut self,
        sig: &super::sem::decls::FunctionSig,
        type_args: &[TypeId],
    ) -> Option<TypeId> {
        // Every slot must be pinned: a bare reference to a generic function
        // is not a value, and a turbofish must name every slot.
        if type_args.len() != sig.decl.type_params.len() {
            return None;
        }
        let inst = sig.decl.instantiate(&self.tysys.type_table, type_args);
        if inst.param_types.contains(&TypeTable::ERROR) || inst.return_type == TypeTable::ERROR {
            return None;
        }
        Some(self.tysys.type_table.borrow_mut().make_function(
            inst.param_types,
            inst.return_type,
            sig.effects.clone(),
            Vec::new(),
        ))
    }

    /// Resolve a bare identifier naming a user-defined function as a function
    /// reference value. A generic one takes its type params from a turbofish,
    /// else positionally from an expected `fn(…)` type, else gets a dedicated
    /// diagnostic. An aliased import emits a `FuncRef` under the defining-module
    /// name, not the alias, so it keys the same as a direct reference.
    fn resolve_func_ref_ident(
        &mut self,
        ident: &ast::IdentExpr,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        self.record_item_reference_by_name(ident.id, &ident.name);

        let Some((sig, _def_module, _defining_name)) = self.lookup_func_sig_for_ref(ident) else {
            // Fallback: known function but its signature is unreachable
            // (shouldn't normally happen). Emit a stub FuncRef so downstream
            // stays sane.
            return TypeTable::UNKNOWN;
        };

        let real_type_param_count = sig.decl.type_params.len();

        // (a) Turbofish on the identifier: `name::<T, ...>`.
        if !ident.type_args.is_empty() {
            if ident.type_args.len() != real_type_param_count {
                let _ = self.emit(TypeError::GenericFunctionRefArgCountMismatch {
                    name: ident.name.clone(),
                    expected: real_type_param_count,
                    found: ident.type_args.len(),
                    span: ident.span,
                });
                return TypeTable::ERROR;
            }
            let resolved_args: Vec<TypeId> = ident
                .type_args
                .iter()
                .map(|t| self.resolve_type(t))
                .collect();
            let type_id = self
                .compute_func_ref_type_from_sig(&sig, &resolved_args)
                .unwrap_or(TypeTable::UNKNOWN);
            self.record_func_ref_instantiation(ident.id, &resolved_args, type_id);
            // Reify rebuilds the turbofish `FuncRef` from the
            // recorded instantiation. Project the type only.
            return type_id;
        }

        // Non-generic function: keep the original behaviour.
        if real_type_param_count == 0 {
            let type_id = self
                .compute_func_ref_type_from_sig(&sig, &[])
                .unwrap_or(TypeTable::UNKNOWN);
            return type_id;
        }

        // (b) Generic without turbofish: try to infer from `expected_type`.
        if let Some(expected) = expected_type {
            match self.infer_func_ref_type_args(&sig, expected) {
                FuncRefInference::Ok(inferred) => {
                    let type_id = self
                        .compute_func_ref_type_from_sig(&sig, &inferred)
                        .unwrap_or(TypeTable::UNKNOWN);
                    self.record_func_ref_instantiation(ident.id, &inferred, type_id);
                    // Reify rebuilds the inferred-generic `FuncRef`
                    // from the recorded instantiation. Project the type only.
                    return type_id;
                }
                FuncRefInference::ArityMismatch {
                    expected_params,
                    found_params,
                } => {
                    let _ = self.emit(TypeError::GenericFunctionRefArityMismatch {
                        name: ident.name.clone(),
                        expected_params,
                        found_params,
                        span: ident.span,
                    });
                    return TypeTable::ERROR;
                }
                FuncRefInference::NotApplicable => {}
            }
        }

        // (c) Generic, no usable type context: dedicated diagnostic.
        let _ = self.emit(TypeError::BareGenericFunctionRef {
            name: ident.name.clone(),
            span: ident.span,
        });
        TypeTable::ERROR
    }

    /// Record the resolved type arguments and instance type of a generic
    /// function-reference identifier so reify can rebuild the same
    /// `FuncRef { type_args }`. Without the recorded args reify would emit
    /// `FuncRef { type_args: [] }`, leaving the name unmangled after
    /// monomorphization and tripping the `lower::closure` invariant
    /// ("`FuncRef` should be wrapped in a Closure"). Non-generic references
    /// pass an empty `type_args` and are skipped — they need no record.
    fn record_func_ref_instantiation(
        &mut self,
        ident_id: AstId,
        type_args: &[TypeId],
        instance_type: TypeId,
    ) {
        if type_args.is_empty() {
            return;
        }
        let key = ident_id;
        self.sem.types.generic_instantiations.insert(
            key,
            super::sem::types::GenericInstantiation {
                type_args: type_args.to_vec(),
                instance_type,
                mangled_name: None,
                is_union: false,
            },
        );
    }

    /// Canonical signature, defining module, and defining name for a
    /// function-reference identifier (local or imported, possibly aliased).
    /// The name is the *defining* one — `"foo"` for `use { foo as bar }` —
    /// keeping the TIR `FuncRef` aligned with the post-monomorphization
    /// key space.
    fn lookup_func_sig_for_ref(
        &self,
        ident: &ast::IdentExpr,
    ) -> Option<(super::sem::decls::FunctionSig, ModuleSource, String)> {
        let def = self.free_function_at(ident.id)?;
        let sig = self.tysys.signatures.function_sig(def)?.clone();
        let defs = self.tysys.resolutions.defs();
        Some((sig, defs.module(def).clone(), defs.name(def).to_string()))
    }

    /// Derive type arguments for a generic function reference from an expected
    /// `fn(…)` type. Only the simple positional case: the expected type must be a
    /// `Function` (possibly behind a ref) of matching arity, and every real type
    /// parameter must end up bound. `ArityMismatch` is separated from
    /// `NotApplicable` so callers can raise a focused diagnostic.
    fn infer_func_ref_type_args(
        &mut self,
        sig: &super::sem::decls::FunctionSig,
        expected: TypeId,
    ) -> FuncRefInference {
        let (expected_params, expected_return) = {
            let table = self.tysys.type_table.borrow();
            // Peel `&fn(...)` / `&mut fn(...)` — function values auto-deref
            // at call sites, so an expected reference-to-fn pins the same
            // signature for inference purposes as a bare `fn(...)` would.
            let mut probe = expected;
            loop {
                match table.get(probe) {
                    crate::tir::ResolvedType::Function {
                        params,
                        return_type,
                        ..
                    } => break (params.clone(), *return_type),
                    crate::tir::ResolvedType::Ref(inner)
                    | crate::tir::ResolvedType::MutRef(inner) => probe = *inner,
                    _ => return FuncRefInference::NotApplicable,
                }
            }
        };
        let decl_param_count = sig.decl.param_types.len();
        if expected_params.len() != decl_param_count {
            return FuncRefInference::ArityMismatch {
                expected_params: expected_params.len(),
                found_params: decl_param_count,
            };
        }

        let type_param_ids: Vec<TypeId> = sig.type_param_ids.iter().map(|&(_, id)| id).collect();
        let decl_params = &sig.decl.param_types;
        let decl_return = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        if type_param_ids.is_empty() {
            return FuncRefInference::NotApplicable;
        }

        let mut infer = super::infer::InferCtx::new(&self.tysys.type_table, type_param_ids.clone());
        for (decl, expected) in decl_params.iter().zip(expected_params.iter()) {
            infer.add(*decl, *expected);
        }
        infer.add_expected_return(decl_return, expected_return);
        let (inferred, bindings) = infer.solve_with_bindings();
        if !type_param_ids.iter().all(|id| bindings.contains_key(id)) {
            return FuncRefInference::NotApplicable;
        }
        FuncRefInference::Ok(inferred)
    }

    /// Resolve a binary expression
    pub(super) fn resolve_field_access(
        &mut self,
        field_access: &ast::FieldAccessExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let expr_type = self.resolve_expr(&field_access.expr, ctx, None);

        // Record use→def reference for the field name, pointing at the field
        // definition's AstId in the struct declaration.
        self.record_field_reference(expr_type, &field_access.field, field_access.field_id);

        // Look up field type from struct type (also emits the field-not-found
        // / tuple-index-out-of-bounds diagnostics). Reify re-derives the
        // `field_index` from the receiver type, so only the result type is
        // needed here.
        let (_field_index, field_type) =
            self.lookup_field_type(expr_type, &field_access.field, field_access.span);

        // Check field visibility: non-pub fields cannot be accessed from other modules
        self.check_field_visibility(
            expr_type,
            &field_access.field,
            Some(field_access.id),
            field_access.span,
        );

        field_type
    }

    /// Record a use→def reference for a struct field access.
    /// `receiver_type` is the type of the struct being accessed;
    /// `field_name` is the accessed field; `use_id` is the `AstId` of the
    /// field-name token at the use site.
    pub(super) fn record_field_reference(
        &mut self,
        receiver_type: TypeId,
        field_name: &str,
        use_id: AstId,
    ) {
        let resolved = self.tysys.type_table.borrow().get(receiver_type).clone();
        let struct_head = match resolved {
            ResolvedType::Struct { def, .. } => Some(def),
            ResolvedType::GenericInstance { def, .. } => Some(crate::tir::StructDef::Decl(def)),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                return self.record_field_reference(inner, field_name, use_id);
            }
            ResolvedType::Newtype { base_type, .. } => {
                return self.record_field_reference(base_type, field_name, use_id);
            }
            _ => return,
        };
        if let Some(info) = struct_head.and_then(|head| self.lookup_struct_fields_of(head)) {
            for ((fname, _, _), fid) in info.fields.iter().zip(info.field_ast_ids.iter()) {
                if fname == field_name {
                    self.record_reference_to_def(use_id, *fid);
                    return;
                }
            }
        }
    }

    /// Element type at a literal index into a tuple, which may carry a
    /// variadic pack.
    ///
    /// An index that lands on the pack is rejected: the pack's arity and its
    /// per-position types are only known once it expands, so neither the bound
    /// nor the element type can be decided here. Only the scalar prefix ahead
    /// of the pack (`[i32, ..T]`.0) has a fixed position.
    pub(super) fn tuple_literal_index_type(
        type_table: &std::cell::RefCell<TypeTable>,
        elements: &[TypeId],
        index: usize,
    ) -> Result<TypeId, String> {
        let table = type_table.borrow();
        let Some(pack_pos) = elements
            .iter()
            .position(|&t| matches!(table.get(t), ResolvedType::TypePack { .. }))
        else {
            return elements.get(index).copied().ok_or_else(|| {
                format!(
                    "tuple index {index} out of bounds, tuple has {} elements",
                    elements.len()
                )
            });
        };
        if index < pack_pos {
            return Ok(elements[index]);
        }
        Err(format!(
            "tuple index {index} lands on a variadic pack, whose arity and element \
             types are only known once it expands; walk the tuple with `for-of` or \
             a comprehension instead"
        ))
    }

    /// Look up field type from a struct or tuple type
    pub(super) fn lookup_field_type(
        &mut self,
        struct_type: TypeId,
        field_name: &str,
        span: Span,
    ) -> (u32, TypeId) {
        // Clone the type to avoid borrow issues
        let resolved = self.tysys.type_table.borrow().get(struct_type).clone();
        match resolved {
            // Struct field access
            ResolvedType::Struct { def, .. } => {
                let hit = self.lookup_struct_fields_of(def).map(|info| {
                    info.fields
                        .iter()
                        .enumerate()
                        .find(|(_, (fname, _, _))| fname == field_name)
                        .map(|(index, (_, ftype, _))| (index as u32, *ftype))
                });
                match hit {
                    Some(Some(found)) => return found,
                    Some(None) => {
                        let name = self.tysys.type_table.borrow().type_name(struct_type);
                        return self.field_not_found(&name, field_name, span);
                    }
                    None => {}
                }
            }
            // Reference types - look through to inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                return self.lookup_field_type(inner, field_name, span);
            }
            // Newtype - look through to base type for field access
            ResolvedType::Newtype { base_type, .. } => {
                return self.lookup_field_type(base_type, field_name, span);
            }
            // Generic instance - look up field from generic struct definition
            // and substitute type parameters with concrete type args.
            // Tuples use numeric field access (0, 1, 2, ...).
            ResolvedType::GenericInstance { type_args, .. } => {
                let name = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(struct_type)
                    .expect("a generic instance names a declaration")
                    .0;
                // Tuple field access (numeric field names: 0, 1, 2, ...)
                if TypeTable::is_tuple_type(&name)
                    && let Ok(index) = field_name.parse::<usize>()
                {
                    match Self::tuple_literal_index_type(&self.tysys.type_table, &type_args, index)
                    {
                        Ok(elem) => return (index as u32, elem),
                        Err(message) => {
                            let _ = self.emit(TypeError::InvalidLiteral { message, span });
                            return (0, TypeTable::UNKNOWN);
                        }
                    }
                }
                // Clone fields to avoid borrow issues. The declaration the
                // instance names answers first: a function-local generic
                // struct's fields are keyed by its identity, and the spelling
                // reaches them only through the local-item render index.
                let fields_clone = self.struct_fields_of_type(struct_type).cloned();
                if let Some(struct_info) = fields_clone {
                    for (index, (fname, ftype, _)) in struct_info.fields.iter().enumerate() {
                        if fname == field_name {
                            // Substitute type parameters with concrete types
                            let concrete_type = self.substitute_type_params(*ftype, &type_args);
                            return (index as u32, concrete_type);
                        }
                    }
                    return self.field_not_found(&name, field_name, span);
                }
            }
            _ => {}
        }
        (0, TypeTable::UNKNOWN)
    }

    /// Report an access to a field the struct does not declare. Answering
    /// `Unknown` instead lets the access reach codegen with no type to lower
    /// from, and silently turns any pattern matched against it irrefutable.
    fn field_not_found(
        &mut self,
        struct_name: &str,
        field_name: &str,
        span: Span,
    ) -> (u32, TypeId) {
        let _ = self.emit(TypeError::ExtraField {
            struct_name: struct_name.to_string(),
            field_name: field_name.to_string(),
            span,
        });
        (0, TypeTable::UNKNOWN)
    }

    /// The module that wrote `node`, per [`super::scope::Scope::foreign_vantage`].
    /// `None` judges here, for a site carrying no id.
    pub(super) fn visibility_vantage(&self, node: Option<ast::AstId>) -> ModuleSource {
        match (&self.annotate_ctx.foreign_vantage, node) {
            (Some((module, space)), Some(id)) if id.space() == *space => module.clone(),
            _ => self.current_module_source.clone(),
        }
    }

    /// Enforce an inherent impl member's rung of the visibility ladder.
    /// `owner` names the declaring type, resolved only to fill a diagnostic.
    /// `visibility` is `None` where the member does not decide its own reach.
    pub(super) fn check_inherent_member_visibility(
        &mut self,
        visibility: Option<crate::ast::Visibility>,
        impl_module: Option<&ModuleSource>,
        owner: MemberOwner<'_>,
        member_name: &str,
        member_kind: super::types::ImplMemberKind,
        node: Option<ast::AstId>,
        span: Span,
    ) {
        let (Some(visibility), Some(impl_module)) = (visibility, impl_module) else {
            return;
        };
        let vantage = self.visibility_vantage(node);
        if *impl_module == vantage {
            return;
        }
        let same_package = impl_module.same_package(&vantage);
        if visibility.reachable_from(same_package) {
            return;
        }
        let type_name = match owner {
            MemberOwner::Type(id) => self.tysys.type_id_to_string(id),
            MemberOwner::Named(name) => name.to_string(),
            MemberOwner::Written(Some(ty)) => self.get_type_name(ty),
            MemberOwner::Written(None) => member_name.to_string(),
        };
        let _ = self.emit(TypeError::PrivateMemberAccess {
            type_name,
            member_name: member_name.to_string(),
            member_kind,
            visibility,
            span,
        });
    }

    pub(super) fn check_field_visibility(
        &mut self,
        struct_type: TypeId,
        field_name: &str,
        node: Option<ast::AstId>,
        span: Span,
    ) {
        let resolved = self.tysys.type_table.borrow().get(struct_type).clone();
        let (struct_name, module_source) = match resolved {
            ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => self
                .tysys
                .type_table
                .borrow()
                .nominal_head(struct_type)
                .expect("a nominal type names a declaration"),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.check_field_visibility(inner, field_name, node, span);
                return;
            }
            ResolvedType::Newtype { base_type, .. } => {
                self.check_field_visibility(base_type, field_name, node, span);
                return;
            }
            _ => return,
        };

        // Same module — always allowed
        let vantage = self.visibility_vantage(node);
        if module_source == vantage {
            return;
        }

        let same_package = module_source.same_package(&vantage);
        if let Some(struct_info) = self.struct_fields_of_type(struct_type) {
            for (fname, _, vis) in &struct_info.fields {
                if fname == field_name && !vis.reachable_from(same_package) {
                    let _ = self.emit(TypeError::PrivateFieldAccess {
                        struct_name,
                        field_name: field_name.to_string(),
                        visibility: *vis,
                        span,
                    });
                    return;
                }
            }
        }
    }

    /// Substitute type parameters in a type with concrete type arguments.
    ///
    /// Treats `type_args` as a dense substitution map keyed by `TypeParam`
    /// index (i.e. `TypeParam { index: i }` is replaced by `type_args[i]`),
    /// delegating the heavy lifting to
    /// [`TypeTable::substitute_type_params`].
    pub(super) fn substitute_type_params(
        &mut self,
        type_id: TypeId,
        type_args: &[TypeId],
    ) -> TypeId {
        if type_args.is_empty() {
            return type_id;
        }
        let substitution: IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &t)| (i as u32, t))
            .collect();
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(type_id, &substitution)
    }

    /// Substitute type parameters using a TypeId-to-TypeId map.
    /// Unlike `substitute_type_params` (which substitutes by index), this only
    /// replaces `TypeIds` that are explicitly in the map, leaving all others unchanged.
    /// This is used in struct literal field type fixup to avoid incorrectly replacing
    /// impl-scope `TypeParams` that share the same index as the struct's own `TypeParams`.
    pub(super) fn substitute_type_params_by_map(
        &mut self,
        type_id: TypeId,
        map: &IndexMap<TypeId, TypeId>,
    ) -> TypeId {
        if map.is_empty() {
            return type_id;
        }
        if let Some(&concrete) = map.get(&type_id) {
            return concrete;
        }
        let resolved_type = self.tysys.type_table.borrow().get(type_id).clone();
        match resolved_type {
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type_params_by_map(elem, map);
                if new_elem == elem {
                    type_id
                } else {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::BuiltinArray(new_elem))
                }
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type_params_by_map(inner, map);
                if new_inner == inner {
                    type_id
                } else {
                    self.tysys.type_table.borrow_mut().make_ref(new_inner)
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type_params_by_map(inner, map);
                if new_inner == inner {
                    type_id
                } else {
                    self.tysys.type_table.borrow_mut().make_mut_ref(new_inner)
                }
            }
            ResolvedType::GenericInstance {
                def,
                type_args: inner_args,
            } => {
                let new_args: Vec<TypeId> = inner_args
                    .iter()
                    .map(|&a| self.substitute_type_params_by_map(a, map))
                    .collect();
                if new_args == inner_args {
                    type_id
                } else {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_generic_instance(def, new_args)
                }
            }
            _ => type_id,
        }
    }

    /// The element type of `t[i]` where `t` is a pack-typed tuple and `i` is
    /// the index of an enclosing variadic `.enumerate()` — the one non-literal
    /// subscript a tuple admits, since unrolling fixes it per element.
    ///
    /// A mapped pack (`[..Option<F>]`) yields its mapped element, matching how
    /// the variadic for-of binds one.
    pub(super) fn variadic_enumerate_subscript_type(
        type_table: &std::cell::RefCell<TypeTable>,
        elements: &[TypeId],
        index_expr: &ast::Expr,
        ctx: &FunctionContext,
    ) -> Option<TypeId> {
        let ast::Expr::Ident(ident) = index_expr else {
            return None;
        };
        let local = ctx.lookup(&ident.name)?;
        if !ctx.variadic_enumerate_indices.contains(&local.index) {
            return None;
        }
        let type_table = type_table.borrow();
        elements.iter().find_map(|&e| match type_table.get(e) {
            ResolvedType::TypePack {
                mapped_elem: Some(elem),
                ..
            } => Some(*elem),
            ResolvedType::TypePack { .. } => Some(e),
            _ => None,
        })
    }

    /// Resolve an index expression
    /// [`Self::resolve_index`] for a subscript reached outside
    /// [`Self::resolve_expr`], which is otherwise the only place a visited
    /// [`AstId`] is annotated. `&xs[i]` and `&mut xs[i]` resolve the subscript
    /// by access mode, so they come through here instead.
    pub(super) fn resolve_index_access(
        &mut self,
        index: &ast::IndexExpr,
        ctx: &mut FunctionContext,
        access: IndexAccess,
    ) -> TypeId {
        let type_id = self.resolve_index(index, ctx, access);
        self.record_expression_type(index.id, type_id);
        type_id
    }

    pub(super) fn resolve_index(
        &mut self,
        index: &ast::IndexExpr,
        ctx: &mut FunctionContext,
        access: IndexAccess,
    ) -> TypeId {
        let expr_type = self.resolve_expr(&index.expr, ctx, None);

        let base_type_id = match self.tysys.type_table.borrow().get(expr_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => expr_type,
        };
        let base_type = self.tysys.type_table.borrow().get(base_type_id).clone();

        // Handle tuple indexing: t[0] is equivalent to t.0
        if let ResolvedType::GenericInstance {
            def,
            type_args: ref elements,
        } = base_type
            && TypeTable::is_tuple_type(self.tysys.type_table.borrow().def_name(def))
        {
            // Tuple indexing requires a constant integer index
            if let ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Number(repr),
                ..
            }) = &index.index
                && !util::is_float_only_literal(repr)
                && let Ok(idx) = repr.parse::<usize>()
            {
                match Self::tuple_literal_index_type(&self.tysys.type_table, elements, idx) {
                    Ok(elem) => return elem,
                    Err(message) => {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message,
                            span: index.span,
                        });
                        return TypeTable::UNKNOWN;
                    }
                }
            }
            // Unrolling fixes the index to a literal per element.
            if let Some(elem) = Self::variadic_enumerate_subscript_type(
                &self.tysys.type_table,
                elements,
                &index.index,
                ctx,
            ) {
                return elem;
            }
            // Non-constant index on tuple
            let _ = self.emit(TypeError::InvalidLiteral {
                message: "tuple index must be a constant integer".to_string(),
                span: index.span,
            });
            return TypeTable::UNKNOWN;
        }

        // For List and custom types, look for Index or IndexValue trait implementation
        // (List implements IndexValue<i32> with type Output = T)
        let struct_name = match &base_type {
            ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. } => self
                .tysys
                .type_table
                .borrow()
                .nominal_head(base_type_id)
                .map(|(n, _)| n)
                .unwrap_or_default(),
            // The raw GC array dispatches `[]` through `impl IndexValue /
            // IndexAssign for Array<T>`, keyed by the base name "Array".
            ResolvedType::BuiltinArray(_) => TypeTable::ARRAY_TYPE_NAME.to_string(),
            _ => String::new(),
        };

        // For newtypes, also resolve the base type name for trait impl lookup
        let (lookup_name, lookup_type_id) =
            self.tysys.newtype_base_lookup(&struct_name, base_type_id);

        if !struct_name.is_empty() {
            // A subscript selects its impl by key type, so the key is
            // synthesized before the impl is chosen — the ordering an overloaded
            // method call has, answered the same way. Only a key synthesis
            // cannot type (a compound literal, whose type the impl supplies)
            // still falls back to pre-selecting an impl for its expected type.
            let key_class = self.synthesize_arg_class(&index.index, ctx);
            let expected_key = self.index_key_type(&key_class).or_else(|| {
                self.index_lookup_or_newtype_base(
                    &struct_name,
                    base_type_id,
                    &lookup_name,
                    lookup_type_id,
                    |s, n, t| s.find_index_value_trait_impl(n, t, None),
                )
                .and_then(|(i, _)| i.index_type)
            });
            let index_type = self.resolve_expr(&index.index, ctx, expected_key);

            // Reject &T/&mut T used as index expression (would ICE in codegen)
            let derefed_index_type = match self.tysys.type_table.borrow().get(index_type) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Some(*inner),
                _ => None,
            };
            if let Some(expected) = derefed_index_type {
                self.typecheck(index_type, expected, index.index.span());
            }

            // A `&mut` subscript asks `IndexRefMut` first so the element arrives
            // as `&mut T` from a `&mut` container. Its `Output: RefMut` bound is
            // what turns a replace-on-assign element back to the shared lookup.
            // Looked up once: the by-value arm below reuses this instead of
            // asking again. Only a value read needs it up front — a reference
            // access that resolves never reaches the by-value lowering.
            let value_impl = (access == IndexAccess::Value)
                .then(|| {
                    self.index_lookup_or_newtype_base(
                        &struct_name,
                        base_type_id,
                        &lookup_name,
                        lookup_type_id,
                        |s, n, t| s.find_index_value_trait_impl(n, t, Some(index_type)),
                    )
                })
                .flatten();
            let index_trait_info = (access == IndexAccess::Mutable)
                .then(|| {
                    self.index_lookup_or_newtype_base(
                        &struct_name,
                        base_type_id,
                        &lookup_name,
                        lookup_type_id,
                        |s, n, t| s.find_index_mut_trait_impl_as_ref(n, t, Some(index_type)),
                    )
                    .map(|found| (found, "index_ref_mut"))
                })
                .flatten()
                .or_else(|| {
                    // A value read prefers the copy `IndexValue` gives it, so a
                    // container offering both keeps its by-value shape; one that
                    // only aliases is still read through `IndexRef` plus a deref.
                    let aliases_only = access != IndexAccess::Value || value_impl.is_none();
                    aliases_only
                        .then(|| {
                            self.index_lookup_or_newtype_base(
                                &struct_name,
                                base_type_id,
                                &lookup_name,
                                lookup_type_id,
                                |s, n, t| s.find_index_trait_impl(n, t, Some(index_type)),
                            )
                            .map(|found| (found, "index_ref"))
                        })
                        .flatten()
                });
            if let Some(((trait_info, matched_type_id), index_method)) = index_trait_info {
                debug_assert_key_matches(trait_info.index_type, index_type);

                let receiver = self.fq_index_receiver(matched_type_id);
                let mangled_method_name =
                    MethodName::format_local(&receiver, Some(&trait_info.trait_name), index_method);

                // `index_ref` returns `&Output`; `index_ref_mut` returns `&mut Output`.
                let ref_output_type = {
                    let mut tt = self.tysys.type_table.borrow_mut();
                    if index_method == "index_ref_mut" {
                        tt.make_mut_ref(trait_info.output_type)
                    } else {
                        tt.make_ref(trait_info.output_type)
                    }
                };

                let func = FunctionRef {
                    module_source: trait_info.impl_module_source.clone(),
                    name: mangled_method_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        receiver,
                        Some(trait_info.trait_name.clone()),
                        index_method.to_string(),
                    )),
                };

                // Record the
                // operator dispatch keyed off the `IndexExpr`'s
                // `AstId`. Reify reads `operator_dispatch[index.id]`
                // to reproduce the `*<method-call>` shape — the
                // `Ref(Output)` `return_type` is the signal that the
                // outer `Deref` wrap is needed.
                self.record_operator_dispatch(
                    index.id,
                    super::sem::types::OperatorDispatch {
                        function_ref: func,
                        method_def: Some(trait_info.method_def),
                        self_kind: trait_info.self_kind,
                        arg_ref_wraps: vec![false],
                        return_type: ref_output_type,
                        needs_deref: true,
                    },
                );

                return trait_info.output_type;
            }

            let index_value_info = value_impl.or_else(|| {
                self.index_lookup_or_newtype_base(
                    &struct_name,
                    base_type_id,
                    &lookup_name,
                    lookup_type_id,
                    |s, n, t| s.find_index_value_trait_impl(n, t, Some(index_type)),
                )
            });
            if let Some((trait_info, matched_type_id)) = index_value_info {
                debug_assert_key_matches(trait_info.index_type, index_type);

                let receiver = self.fq_index_receiver(matched_type_id);
                let mangled_method_name = MethodName::format_local(
                    &receiver,
                    Some(&trait_info.trait_name),
                    "index_value",
                );

                // IndexValue returns Output directly (not a reference)
                let func = FunctionRef {
                    module_source: trait_info.impl_module_source.clone(),
                    name: mangled_method_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        receiver,
                        Some(trait_info.trait_name.clone()),
                        "index_value".to_string(),
                    )),
                };

                // Record the
                // operator dispatch keyed off the `IndexExpr`'s
                // `AstId`. The `return_type` (the `Output` directly,
                // not wrapped in `Ref`) is reify's signal that no
                // outer `Deref` wrap is needed — the IndexValue
                // shape returns the value by copy.
                self.record_operator_dispatch(
                    index.id,
                    super::sem::types::OperatorDispatch {
                        function_ref: func,
                        method_def: Some(trait_info.method_def),
                        self_kind: trait_info.self_kind,
                        arg_ref_wraps: vec![false],
                        return_type: trait_info.output_type,
                        needs_deref: false,
                    },
                );

                return trait_info.output_type;
            }
        }

        // Fallback: report error for unsupported indexing
        let type_name = self.tysys.type_table.borrow().type_name(expr_type);
        let _ = self.emit(TypeError::MissingTraitImpl {
            type_name,
            trait_name: "Index or IndexValue".to_string(),
            span: index.span,
        });
        TypeTable::UNKNOWN
    }

    /// The key type of the `Index` / `IndexAssign` impl dispatching
    /// `index_expr`'s receiver (`Index<K>` and `IndexAssign<K>` share `K`), or
    /// `None` when there is none. Lets a compound assign resolve a hoisted
    /// subscript against its key type before the target is walked.
    pub(super) fn compound_index_key_type(
        &mut self,
        index_expr: &ast::IndexExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TypeId> {
        let recv_type = self.resolve_expr(&index_expr.expr, ctx, None);
        let base_type_id = match self.tysys.type_table.borrow().get(recv_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => recv_type,
        };
        let struct_name = match self.tysys.type_table.borrow().get(base_type_id).clone() {
            ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. } => self
                .tysys
                .type_table
                .borrow()
                .nominal_head(base_type_id)
                .map(|(n, _)| n)
                .unwrap_or_default(),
            ResolvedType::BuiltinArray(_) => TypeTable::ARRAY_TYPE_NAME.to_string(),
            _ => return None,
        };
        if struct_name.is_empty() {
            return None;
        }
        let (lookup_name, lookup_type_id) =
            self.tysys.newtype_base_lookup(&struct_name, base_type_id);
        self.index_lookup_or_newtype_base(
            &struct_name,
            base_type_id,
            &lookup_name,
            lookup_type_id,
            |s, n, t| s.find_index_trait_impl(n, t, None),
        )
        .and_then(|(i, _)| i.index_type)
        .or_else(|| {
            self.index_lookup_or_newtype_base(
                &struct_name,
                base_type_id,
                &lookup_name,
                lookup_type_id,
                super::Elaborator::find_index_assign_trait_impl,
            )
            .and_then(|(i, _)| i.index_type)
        })
    }

    /// Resolve an if expression
    pub(super) fn resolve_if_expr(
        &mut self,
        if_expr: &IfExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        match &if_expr.condition {
            Condition::LetChain { elements, .. } => {
                self.record_desugar(if_expr.id, super::sem::types::DesugarKind::IfLetChain);
                // The chain bindings are not visible in `else`, so resolve it
                // in the outer scope.
                if let Some(b) = &if_expr.else_block {
                    self.resolve_block_value(b, ctx, expected_type);
                }

                // Enter scope for chain elements and then_block
                ctx.enter_scope();
                self.resolve_let_chain_stmts(
                    elements,
                    &if_expr.then_block,
                    ctx,
                    expected_type,
                    true,
                    if_expr.span,
                );
                ctx.exit_scope();

                let type_id = if let Some(ty) = expected_type {
                    ty
                } else {
                    // The chain's result is what the then block and the else
                    // block agree on, exactly as in the `Condition::Expr` arm.
                    let (then_type, else_type) = self.if_branch_types(if_expr);
                    match (
                        self.agreed_branch_type(&[then_type, else_type]),
                        &if_expr.else_block,
                    ) {
                        (Some(agreed), _) => agreed,
                        (None, None) => TypeTable::UNIT,
                        (None, Some(else_block)) => {
                            let (then_name, else_name) = self
                                .tysys
                                .type_table
                                .borrow()
                                .type_names_for_mismatch(then_type, else_type);
                            let _ = self.emit(TypeError::TypeMismatch {
                                expected: then_name,
                                found: else_name,
                                span: else_block.span,
                            });
                            then_type
                        }
                    }
                };

                // An `if let` whose branches are all bare `null` leaves the
                // type unresolved; report it rather than ICEing in codegen.
                // When one branch resolved, the other's `null` tail is checked
                // against it — the sibling's type is what agreement adopted.
                if !self.report_uninferable_result(type_id, if_expr.span, "if expression") {
                    let mut blocks: Vec<&ast::Block> = vec![&if_expr.then_block];
                    if let Some(eb) = &if_expr.else_block {
                        blocks.push(eb);
                    }
                    self.report_unresolved_null_tails_in_blocks(type_id, &blocks);
                }

                // Same arm-agreement rule as the `Condition::Expr` arm below:
                // `expected_type = Some(X)` pins `type_id` unconditionally, so
                // the chain and else blocks could still disagree and a divergent
                // branch would silently miscompile. Skipped at `Unit`, which is
                // statement position — the branches drop their values there.
                if expected_type.is_some() && type_id != TypeTable::UNIT {
                    // `resolve_let_chain_stmts` resolves the then-branch under
                    // the same `expected_type`, so a mismatch there is already
                    // diagnosed — and re-checking via `block_result_type` would
                    // report a spurious "found ()". The else-block is resolved
                    // independently, so check it directly.
                    if let Some(eb) = &if_expr.else_block {
                        let else_type = self.ast_block_result_type(eb);
                        self.check_branch_type(else_type, type_id, eb.span);
                    } else {
                        // Missing `else` with a non-Unit expected
                        // type: the implicit `else { () }` cannot
                        // produce the expected type. See the
                        // `Condition::Expr` arm for the rationale
                        // (without this guard the WIR builder
                        // would produce `(if (result T) ...)`
                        // without an else and `wasmparser` would
                        // reject the module at `-O0`).
                        self.check_branch_type(TypeTable::UNIT, type_id, if_expr.span);
                    }
                }

                // Reify rebuilds the if-let-chain (recorded via
                // `DesugarKind::IfLetChain`) from the AST. The body walk
                // ran `resolve_let_chain_stmts` for its fact-recording side
                // effects (pattern bindings, element resolution) and computed
                // the result type. Project only the result type.
                type_id
            }
            Condition::Expr(expr) => {
                // Resolve the condition and both blocks for their facts; reify
                // rebuilds the `If` node. The result type is inferred from the
                // AST (`ast_block_result_type`) below.
                self.resolve_condition_expr(expr, ctx);
                self.resolve_block_value(&if_expr.then_block, ctx, expected_type);
                if let Some(b) = &if_expr.else_block {
                    self.resolve_block_value(b, ctx, expected_type);
                }

                let type_id = if let Some(ty) = expected_type {
                    ty
                } else {
                    let (then_type, else_type) = self.if_branch_types(if_expr);

                    // `never` is the bottom type: a branch returning `never` is compatible
                    // with any type, so the result type comes from the non-never branch.
                    //
                    // An indefinite branch defers to its sibling's resolved
                    // type; its tail is patched below.
                    match (
                        self.agreed_branch_type(&[then_type, else_type]),
                        &if_expr.else_block,
                    ) {
                        (Some(agreed), _) => agreed,
                        (None, None) => {
                            if then_type != TypeTable::UNIT {
                                let type_name = self.tysys.type_table.borrow().type_name(then_type);
                                let _ = self.emit(TypeError::TypeMismatch {
                                    expected: "()".to_string(),
                                    found: type_name,
                                    span: if_expr.then_block.span,
                                });
                            }
                            TypeTable::UNIT
                        }
                        (None, Some(else_block)) => {
                            let (then_name, else_name) = self
                                .tysys
                                .type_table
                                .borrow()
                                .type_names_for_mismatch(then_type, else_type);
                            let _ = self.emit(TypeError::TypeMismatch {
                                expected: then_name,
                                found: else_name,
                                span: else_block.span,
                            });
                            then_type
                        }
                    }
                };

                // Report any unresolved `null` tail in either branch against
                // the determined result type — AST mirror of the old
                // `patch_unresolved_null` pass (whose TIR mutation was dead).
                // When the type stayed indefinite (both branches a bare `null`)
                // `report_uninferable_result` already fired and the null pass
                // is skipped.
                if !self.report_uninferable_result(type_id, if_expr.span, "if expression") {
                    let mut blocks: Vec<&ast::Block> = vec![&if_expr.then_block];
                    if let Some(eb) = &if_expr.else_block {
                        blocks.push(eb);
                    }
                    self.report_unresolved_null_tails_in_blocks(type_id, &blocks);
                }

                // Same rule as `resolve_match_expr`: an if-expression whose
                // result is consumed needs branches that agree. Inference
                // diagnoses the `expected_type = None` case, but `Some(X)`
                // bypasses it and would emit an `(if (result X) …)` whose other
                // side pushes the wrong type. Skipped at `Unit`.
                if expected_type.is_some() && type_id != TypeTable::UNIT {
                    let then_type = self.ast_block_result_type(&if_expr.then_block);
                    self.check_branch_type(then_type, type_id, if_expr.then_block.span);
                    if let Some(eb) = &if_expr.else_block {
                        let else_type = self.ast_block_result_type(eb);
                        self.check_branch_type(
                            else_type,
                            type_id,
                            if_expr.else_block.as_ref().unwrap().span,
                        );
                    } else {
                        // Without an explicit `else` the implicit branch is `()`,
                        // which cannot satisfy a non-Unit expected type.
                        // `type_id` is left as-is: the recorded diagnostic
                        // aborts before WIR build, so a result-typed `if` with
                        // no else never reaches `wasmparser`.
                        self.check_branch_type(TypeTable::UNIT, type_id, if_expr.span);
                    }
                }

                // Reify rebuilds the `If` node from the AST; the
                // body walk resolved the condition and both blocks for
                // their fact-recording side effects and ran branch-agreement /
                // null diagnostics off the AST (`ast_block_result_type`).
                // Project only the result type.
                type_id
            }
        }
    }

    /// Emit a `TypeMismatch` error when a branch's block-result type
    /// is incompatible with the surrounding context's expected type.
    /// `never` and `unknown` (and unresolved type-params) defer via
    /// `check_assignable`'s `Deferred` / `Compatible` rules. Used by
    /// `resolve_if_expr` (`Condition::Expr` arm) to validate then /
    /// else branches when an outer type annotation pinned the
    /// expected result type; the same rule lives inline in
    /// `resolve_match_expr` for arm bodies.
    pub(super) fn check_branch_type(&mut self, actual: TypeId, expected: TypeId, span: Span) {
        let result = {
            let tt = self.tysys.type_table.borrow();
            check_assignable(actual, expected, &tt)
        };
        if matches!(result, TypeCheckResult::Incompatible) {
            let (expected_name, found_name) = self
                .tysys
                .type_table
                .borrow()
                .type_names_for_mismatch(expected, actual);
            let _ = self.emit(TypeError::TypeMismatch {
                expected: expected_name,
                found: found_name,
                span,
            });
        }
    }

    /// Reports a `CannotInferType` error when every branch of a construct
    /// produced an indefinite type, leaving the result one too. Returns `true`
    /// when an error was reported, so the caller can skip the `null`-patching
    /// pass (which requires a resolved target type).
    fn report_uninferable_result(
        &mut self,
        result_type: TypeId,
        span: Span,
        construct: &str,
    ) -> bool {
        if !self.tysys.type_table.borrow().is_indefinite(result_type) {
            return false;
        }
        let _ = self.emit(TypeError::CannotInferType {
            message: format!("cannot infer the type of this {construct}; add a type annotation"),
            span,
        });
        true
    }

    /// Reports each `null` branch value that could not be reconciled with a
    /// branch construct's resolved (non-`Option`) result type. `null` is an
    /// `Option`, so against e.g. `i32` it is a type mismatch — surfaced here
    /// because `check_assignable` treats the still-`UNKNOWN` `null` leniently.
    fn report_unresolved_nulls(&mut self, unresolved: &[Span], result_type: TypeId) {
        for &span in unresolved {
            let expected = self.tysys.type_table.borrow().type_name(result_type);
            let _ = self.emit(TypeError::TypeMismatch {
                expected,
                found: "null".to_string(),
                span,
            });
        }
    }

    /// Report each unresolved-`null` tail in `blocks` against a resolved
    /// non-`Option` `result_type` (AST replacement for the
    /// `patch_unresolved_null_in_block` + `report_unresolved_nulls` pass).
    /// A `null` tail is an `Option`, so when `result_type` is itself an
    /// `Option` every tail fits and nothing is reported.
    fn report_unresolved_null_tails_in_blocks(
        &mut self,
        result_type: TypeId,
        blocks: &[&ast::Block],
    ) {
        if self
            .tysys
            .type_table
            .borrow()
            .as_option(result_type)
            .is_some()
        {
            return;
        }
        let mut spans = Vec::new();
        let ctx = self.ctrl_flow_ctx();
        for block in blocks {
            super::control_flow::collect_unresolved_null_tails_in_block(ctx, block, &mut spans);
        }
        self.report_unresolved_nulls(&spans, result_type);
    }

    /// Report each unresolved-`null` arm body of `match_expr` against a
    /// resolved non-`Option` `result_type` (AST replacement for the
    /// per-arm `patch_unresolved_null` + `report_unresolved_nulls` pass).
    fn report_unresolved_null_match_arms(
        &mut self,
        result_type: TypeId,
        match_expr: &ast::MatchExpr,
    ) {
        if self
            .tysys
            .type_table
            .borrow()
            .as_option(result_type)
            .is_some()
        {
            return;
        }
        let mut spans = Vec::new();
        let ctx = self.ctrl_flow_ctx();
        for arm in &match_expr.arms {
            super::control_flow::collect_unresolved_null_tails(ctx, &arm.body, &mut spans);
        }
        self.report_unresolved_nulls(&spans, result_type);
    }

    /// Report each `break <label>: null` value inside `block` against a
    /// resolved non-`Option` `result_type` (AST replacement for the
    /// `NullBreakPatcher` pass).
    fn report_unresolved_null_breaks(
        &mut self,
        result_type: TypeId,
        block: &ast::Block,
        label: &str,
    ) {
        if self
            .tysys
            .type_table
            .borrow()
            .as_option(result_type)
            .is_some()
        {
            return;
        }
        let spans = {
            let ctx = self.ctrl_flow_ctx();
            super::control_flow::collect_unresolved_null_breaks(ctx, block, label)
        };
        self.report_unresolved_nulls(&spans, result_type);
    }

    /// The types an `if`'s two branches settle on, after a numeric literal
    /// adopts its sibling's type. `if let` agrees its branches here too.
    fn if_branch_types(&mut self, if_expr: &IfExpr) -> (TypeId, TypeId) {
        let mut then_type = self.ast_block_result_type(&if_expr.then_block);
        let mut else_type = if_expr
            .else_block
            .as_ref()
            .map_or(TypeTable::UNIT, |b| self.ast_block_result_type(b));

        if then_type != else_type
            && let Some(eb) = &if_expr.else_block
        {
            if let Some(t) = self.coerce_block_numeric_literal_tail(eb, then_type) {
                else_type = t;
            } else if let Some(t) =
                self.coerce_block_numeric_literal_tail(&if_expr.then_block, else_type)
            {
                then_type = t;
            }
        }
        (then_type, else_type)
    }

    /// Give a numeric-literal branch tail the type a sibling branch fixed, as
    /// the coercion in `let x: T = <branch>` does. `None` when none applied.
    fn coerce_numeric_literal_tail(&mut self, expr: &ast::Expr, target: TypeId) -> Option<TypeId> {
        let mut tails = NumericLiteralTails::default();
        if !self.collect_numeric_literal_tails(expr, target, &mut tails) {
            return None;
        }
        self.retarget_numeric_literal_tails(tails, target)
    }

    fn coerce_block_numeric_literal_tail(
        &mut self,
        block: &ast::Block,
        target: TypeId,
    ) -> Option<TypeId> {
        let mut tails = NumericLiteralTails::default();
        if !self.collect_block_numeric_literal_tails(block, target, &mut tails) {
            return None;
        }
        self.retarget_numeric_literal_tails(tails, target)
    }

    fn retarget_numeric_literal_tails(
        &mut self,
        tails: NumericLiteralTails<'_>,
        target: TypeId,
    ) -> Option<TypeId> {
        if tails.literals.is_empty() {
            return None;
        }
        for literal in tails.literals {
            // A refusal leaves the earlier literals retargeted, which is
            // harmless: the caller's mismatch aborts before WIR build.
            self.try_coerce_numeric_literal(literal, target)?;
        }
        for branch in tails.branches {
            self.record_expression_type(branch, target);
        }
        Some(target)
    }

    /// Collect the tails `expr` has to retarget to land on `target`. `false`
    /// when one of them is neither a numeric literal nor already `target`: the
    /// caller then retargets nothing and reports the branches as written.
    fn collect_numeric_literal_tails<'a>(
        &self,
        expr: &'a ast::Expr,
        target: TypeId,
        out: &mut NumericLiteralTails<'a>,
    ) -> bool {
        match expr {
            ast::Expr::Block(block) => self.collect_block_numeric_literal_tails(block, target, out),
            ast::Expr::If(if_expr) => {
                out.branches.push(if_expr.id);
                self.collect_if_numeric_literal_tails(
                    &if_expr.then_block,
                    if_expr.else_block.as_ref(),
                    target,
                    out,
                )
            }
            ast::Expr::Match(match_expr) => {
                self.collect_match_numeric_literal_tails(match_expr, target, out)
            }
            _ if super::coercion::is_numeric_literal_expr(expr) => {
                out.literals.push(expr);
                true
            }
            _ => self
                .ast_expr_type(expr)
                .is_some_and(|ty| agrees_with_target(ty, target)),
        }
    }

    fn collect_block_numeric_literal_tails<'a>(
        &self,
        block: &'a ast::Block,
        target: TypeId,
        out: &mut NumericLiteralTails<'a>,
    ) -> bool {
        match block.stmts.last() {
            Some(ast::Stmt::Expr(e)) => self.collect_numeric_literal_tails(&e.expr, target, out),
            // `block_result_type` reads a trailing `if` / `match` statement as
            // the block's value, so `else { if … }` retargets like `else if …`.
            // A trailing `if` records no type: reify recomputes one from the
            // branches it has just built.
            Some(ast::Stmt::If(if_stmt)) => self.collect_if_numeric_literal_tails(
                &if_stmt.then_block,
                if_stmt.else_block.as_ref(),
                target,
                out,
            ),
            Some(ast::Stmt::Match(match_expr)) => {
                self.collect_match_numeric_literal_tails(match_expr, target, out)
            }
            // No expression tail to retarget, so the block has to agree already.
            _ => agrees_with_target(self.ast_block_result_type(block), target),
        }
    }

    /// Whether every tail of a match arm is a numeric literal, leaving the arm
    /// no type of its own for the result to be read from.
    fn is_numeric_literal_arm(&self, body: &ast::Expr, body_type: TypeId) -> bool {
        let mut tails = NumericLiteralTails::default();
        self.collect_numeric_literal_tails(body, body_type, &mut tails)
            && !tails.literals.is_empty()
    }

    /// The two halves of an `if`, in either its expression or its statement
    /// spelling. A missing `else` is `()`, which no numeric target accepts.
    fn collect_if_numeric_literal_tails<'a>(
        &self,
        then_block: &'a ast::Block,
        else_block: Option<&'a ast::Block>,
        target: TypeId,
        out: &mut NumericLiteralTails<'a>,
    ) -> bool {
        let Some(else_block) = else_block else {
            return false;
        };
        self.collect_block_numeric_literal_tails(then_block, target, out)
            && self.collect_block_numeric_literal_tails(else_block, target, out)
    }

    fn collect_match_numeric_literal_tails<'a>(
        &self,
        match_expr: &'a ast::MatchExpr,
        target: TypeId,
        out: &mut NumericLiteralTails<'a>,
    ) -> bool {
        out.branches.push(match_expr.id);
        match_expr
            .arms
            .iter()
            .all(|arm| self.collect_numeric_literal_tails(&arm.body, target, out))
    }

    /// The type a set of branches agrees on, `None` when they disagree — the
    /// caller reports that in its own terms. The one place branch agreement is
    /// decided: `if`, `if let` and `match` all route here.
    pub(super) fn agreed_branch_type(&self, branches: &[TypeId]) -> Option<TypeId> {
        let mut agreed: Option<TypeId> = None;
        for &branch in branches {
            agreed = Some(match agreed {
                None => branch,
                Some(acc) => self.agree_two_branches(acc, branch)?,
            });
        }
        agreed
    }

    fn agree_two_branches(&self, a: TypeId, b: TypeId) -> Option<TypeId> {
        if a == b {
            return Some(a);
        }
        if a == TypeTable::NEVER {
            return Some(b);
        }
        if b == TypeTable::NEVER {
            return Some(a);
        }
        let (a_unknown, b_unknown) = {
            let tt = self.tysys.type_table.borrow();
            (tt.is_indefinite(a), tt.is_indefinite(b))
        };
        if a_unknown && !b_unknown {
            return Some(b);
        }
        if b_unknown && !a_unknown {
            return Some(a);
        }
        self.tysys.type_table.borrow().resource_join(a, b)
    }

    pub(super) fn resolve_match_expr(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        let mut scrutinee_type = self.resolve_expr(&match_expr.expr, ctx, None);

        // Resolve each arm for its facts (binding + guard + body). Surface only
        // the arm bodies' `(type_id, span)` — reify rebuilds the match node, so
        // no `TirMatchArm` is retained.
        let mut arm_bodies: Vec<(TypeId, Span)> = match_expr
            .arms
            .iter()
            .map(|arm| self.resolve_match_arm(arm, scrutinee_type, ctx, expected_type))
            .collect();

        // A hole from a generic scrutinee (`match gen() { … }`) flows through
        // the bindings into the arm bodies (same `TypeId`). Solve it against the
        // expected type or a concrete sibling arm and concretise before the
        // result-type selection below.
        if arm_bodies.iter().any(|&(t, _)| self.type_has_infer_hole(t))
            || self.type_has_infer_hole(scrutinee_type)
        {
            let target = expected_type
                .filter(|&t| t != TypeTable::UNKNOWN && !self.type_has_infer_hole(t))
                .or_else(|| {
                    arm_bodies.iter().map(|(t, _)| *t).find(|&t| {
                        t != TypeTable::NEVER
                            && !self.type_has_infer_hole(t)
                            && !self.tysys.type_table.borrow().is_indefinite(t)
                    })
                });
            if let Some(target) = target {
                for &(arm_type, _) in &arm_bodies {
                    self.solve_infer_holes_against(arm_type, target);
                }
            }
            for (t, _) in &mut arm_bodies {
                *t = self.apply_infer_holes(*t);
            }
            scrutinee_type = self.apply_infer_holes(scrutinee_type);
        }

        self.check_match_exhaustiveness(&match_expr.arms, scrutinee_type, match_expr.span);

        // A numeric-literal arm holds its default type for want of a sibling
        // saying otherwise, so it is in no position to fix the result: without
        // this, `match k { 1 => 0, _ => a }` resolves to `i32` and rejects the
        // `u64` arm, while the same match with its arms swapped compiles.
        let literal_arms: Vec<bool> = match_expr
            .arms
            .iter()
            .zip(&arm_bodies)
            .map(|(arm, &(arm_type, _))| self.is_numeric_literal_arm(&arm.body, arm_type))
            .collect();

        let type_id = expected_type.unwrap_or_else(|| {
            // Skip `never`-typed arms: `never` is the bottom type and is compatible
            // with any type, so the match result type is determined by the non-never arms.
            //
            // Also skip arms whose type is indefinite: a sibling arm with a
            // fully-resolved type (e.g. `Option::Some(s)` where `s: String`)
            // wins, and we patch the unresolved arm bodies below.
            let tt = self.tysys.type_table.borrow();
            arm_bodies
                .iter()
                .zip(&literal_arms)
                .filter(|&(_, is_literal)| !*is_literal)
                .map(|((t, _), _)| *t)
                .find(|&t| t != TypeTable::NEVER && !tt.is_indefinite(t))
                .or_else(|| {
                    arm_bodies
                        .iter()
                        .map(|(t, _)| *t)
                        .find(|&t| t != TypeTable::NEVER && !tt.is_indefinite(t))
                })
                .or_else(|| {
                    arm_bodies
                        .iter()
                        .map(|(t, _)| *t)
                        .find(|&t| t != TypeTable::NEVER)
                })
                .unwrap_or_else(|| {
                    // All arms return `never` — the match itself is `never`.
                    arm_bodies
                        .first()
                        .map(|(t, _)| *t)
                        .unwrap_or(TypeTable::UNIT)
                })
        });

        // Whichever order the arms are written in: the first-arm pick above
        // would make a parent-typed later arm a mismatch.
        let type_id = if expected_type.is_some() {
            type_id
        } else {
            arm_bodies.iter().fold(type_id, |acc, (arm_type, _)| {
                self.agreed_branch_type(&[acc, *arm_type]).unwrap_or(acc)
            })
        };

        // Report any `null`-bodied arm whose `Option<???>` inner could not be
        // inferred against a resolved non-`Option` result — AST mirror of the
        // old `patch_unresolved_null` pass (whose TIR mutation was dead). When
        // the match type itself stayed UNKNOWN (every arm a bare `null`)
        // `report_uninferable_result` already fired and the null pass is
        // skipped.
        if !self.report_uninferable_result(type_id, match_expr.span, "match expression") {
            self.report_unresolved_null_match_arms(type_id, match_expr);
        }

        // Retarget literal arms to the unified type before the arm-agreement
        // check below would reject their `i32`/`f64` default.
        for (i, arm) in match_expr.arms.iter().enumerate() {
            if arm_bodies[i].0 != type_id
                && let Some(t) = self.coerce_numeric_literal_tail(&arm.body, type_id)
            {
                arm_bodies[i].0 = t;
            }
        }

        // Reject arms whose body type disagrees with the match's result type;
        // otherwise the wasm result is picked from one arm while another pushes
        // something else. Skipped at `Unit`, which is statement position —
        // `translate_match` drops each arm's value there.
        if type_id != TypeTable::UNIT {
            for &(arm_type, arm_span) in &arm_bodies {
                let result = {
                    let tt = self.tysys.type_table.borrow();
                    check_assignable(arm_type, type_id, &tt)
                };
                if matches!(result, TypeCheckResult::Incompatible) {
                    let (expected_name, found_name) = self
                        .tysys
                        .type_table
                        .borrow()
                        .type_names_for_mismatch(type_id, arm_type);
                    let _ = self.emit(TypeError::TypeMismatch {
                        expected: expected_name,
                        found: found_name,
                        span: arm_span,
                    });
                }
            }
        }

        // No analysis reads a resolved match structure — missing-return walks
        // the arms off the AST in `control_flow.rs`.
        type_id
    }

    /// Resolve one match arm for its facts: bind the arm pattern into `ctx`,
    /// resolve the optional guard, and resolve the body. Returns the body's
    /// `(type_id, span)` so `resolve_match_expr` can compute the match result
    /// type and run arm-agreement diagnostics. No `TirMatchArm` / `TirPattern`
    /// is built — reify rebuilds the match node from the AST, and
    /// exhaustiveness now reads the AST arms directly.
    pub(super) fn resolve_match_arm(
        &mut self,
        arm: &MatchArm,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> (TypeId, Span) {
        ctx.enter_scope();

        self.resolve_if_pattern(&arm.pattern, scrutinee_type, ctx, arm.span);
        if let Some(g) = arm.guard.as_ref() {
            self.resolve_expr(g, ctx, Some(TypeTable::BOOL));
        }
        let body_type = self.resolve_expr(&arm.body, ctx, expected_type);

        ctx.exit_scope();

        (body_type, arm.body.span())
    }

    /// Exhaustiveness runs on the AST: the body walk materializes no
    /// `TirMatchArm` / `TirPattern` to check. Each arm pattern is classified
    /// into an [`ExhPattern`] — a shape projection carrying every distinction
    /// the checks make (case-name disambiguation, range bounds,
    /// const-vs-literal, reversed/empty-range → catch-all) — and the checks
    /// read that projection.
    fn check_match_exhaustiveness(
        &mut self,
        arms: &[MatchArm],
        scrutinee_type: TypeId,
        span: Span,
    ) {
        // Classify each arm pattern once (shape only), pairing it with whether
        // the arm is guardless (guarded arms never contribute to coverage).
        let classified: Vec<(bool, ExhPattern)> = arms
            .iter()
            .map(|arm| {
                let guardless = arm.guard.is_none();
                (guardless, self.exh_pattern(&arm.pattern, scrutinee_type))
            })
            .collect();

        // Always check for overlapping range patterns first.
        self.check_range_overlaps(&classified, span);

        // If any arm has a wildcard or binding pattern (without a guard), the match is exhaustive
        if classified
            .iter()
            .any(|(guardless, pat)| *guardless && Self::is_catch_all_pattern(pat))
        {
            return;
        }

        let tt = self.tysys.type_table.borrow();
        let resolved = tt.get(scrutinee_type).clone();
        drop(tt);

        match &resolved {
            ResolvedType::Enum { .. } => {
                if let Some(enum_info) = self.enum_of_type(scrutinee_type) {
                    let all_cases: IndexSet<&str> =
                        enum_info.cases.iter().map(|c| c.name.as_str()).collect();
                    let covered: IndexSet<&str> = {
                        let mut names = Vec::new();
                        for (_, pat) in &classified {
                            Self::collect_enum_case_names(pat, &mut names);
                        }
                        names.into_iter().collect()
                    };
                    let missing: Vec<&&str> = all_cases.difference(&covered).collect();
                    if !missing.is_empty() {
                        let missing_names: Vec<String> =
                            missing.iter().map(|s| (*s).to_string()).collect();
                        let _ = self.emit(TypeError::InvalidPattern {
                            message: format!(
                                "non-exhaustive match: missing {}",
                                Self::format_missing_cases(&missing_names),
                            ),
                            span,
                        });
                    }
                }
            }
            ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => {
                self.check_variant_exhaustiveness(&classified, scrutinee_type, span);
            }
            ResolvedType::Primitive(crate::tir::PrimitiveType::Bool) => {
                let has_true = classified
                    .iter()
                    .any(|(_, pat)| Self::pattern_contains_bool(pat, true));
                let has_false = classified
                    .iter()
                    .any(|(_, pat)| Self::pattern_contains_bool(pat, false));
                if !has_true || !has_false {
                    let mut missing = Vec::new();
                    if !has_true {
                        missing.push("true".to_string());
                    }
                    if !has_false {
                        missing.push("false".to_string());
                    }
                    let _ = self.emit(TypeError::InvalidPattern {
                        message: format!(
                            "non-exhaustive match: missing {}",
                            Self::format_missing_cases(&missing),
                        ),
                        span,
                    });
                }
            }
            ResolvedType::Primitive(prim) => {
                if let Some((type_min, type_max)) = Self::primitive_range(*prim) {
                    self.check_integer_range_exhaustiveness(&classified, type_min, type_max, span);
                }
            }
            _ => {
                // For other types (strings, structs, etc.) we don't check exhaustiveness.
            }
        }
    }

    /// Project an AST match-arm pattern onto the shape exhaustiveness reads,
    /// mirroring `resolve_if_pattern_inner`'s `TirPattern`-shape decisions.
    /// References are peeled first (as `resolve_if_pattern` does) so case-name
    /// disambiguation uses the underlying type.
    fn exh_pattern(&mut self, pattern: &ast::Pattern, scrutinee_type: TypeId) -> ExhPattern {
        let mut peeled = scrutinee_type;
        while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
            self.tysys.type_table.borrow().get(peeled).clone()
        {
            peeled = inner;
        }
        self.exh_pattern_inner(pattern, peeled)
    }

    fn exh_pattern_inner(&mut self, pattern: &ast::Pattern, scrutinee_type: TypeId) -> ExhPattern {
        match pattern {
            ast::Pattern::Wildcard | ast::Pattern::Error(_) => ExhPattern::CatchAll,
            ast::Pattern::Ident { name, .. } | ast::Pattern::MutIdent { name, .. } => {
                // A bare identifier is a case when it names one (delegates to the
                // Variant branch), an opaque constant-value pattern when it names
                // an immutable global, or otherwise a binding (catch-all).
                if !matches!(pattern, ast::Pattern::MutIdent { .. })
                    && self.is_known_case_of_type(scrutinee_type, name, None)
                {
                    return self.exh_pattern_inner(
                        &ast::Pattern::Variant {
                            variant_name: name.clone(),
                            variant_qualifier: None,
                            name_id: None,
                            name_span: Span::default(),
                            bindings: vec![],
                            span: Span::default(),
                        },
                        scrutinee_type,
                    );
                }
                if !matches!(pattern, ast::Pattern::MutIdent { .. }) {
                    if let Some(&(_ty, mutable)) = self.sem.decls.current_module_globals.get(name)
                        && !mutable
                    {
                        return ExhPattern::Other;
                    }
                    if let Some((_m, _n, _ty, mutable)) = self.sem.decls.imported_globals.get(name)
                        && !*mutable
                    {
                        return ExhPattern::Other;
                    }
                }
                ExhPattern::CatchAll
            }
            ast::Pattern::Literal(lit) => self.exh_literal(lit, scrutinee_type),
            ast::Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                ..
            } => self.exh_variant(
                variant_name,
                variant_qualifier.as_ref(),
                bindings,
                scrutinee_type,
            ),
            ast::Pattern::Or(alternatives) => ExhPattern::Or(
                alternatives
                    .iter()
                    .map(|alt| self.exh_pattern_inner(alt, scrutinee_type))
                    .collect(),
            ),
            ast::Pattern::Range {
                start, end, kind, ..
            } => self.exh_range(start, end, *kind, scrutinee_type),
            ast::Pattern::Tuple(_, _) | ast::Pattern::Struct { .. } => ExhPattern::Other,
        }
    }

    /// True when the scrutinee is an unsigned integer type (governs literal
    /// parsing). Mirrors the `is_unsigned` check in `resolve_if_pattern_inner`.
    fn exh_is_unsigned(&self, scrutinee_type: TypeId) -> bool {
        let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        matches!(
            resolved,
            ResolvedType::Primitive(
                crate::tir::PrimitiveType::U8
                    | crate::tir::PrimitiveType::U16
                    | crate::tir::PrimitiveType::U32
                    | crate::tir::PrimitiveType::U64
                    | crate::tir::PrimitiveType::U128
            )
        ) || matches!(resolved, ResolvedType::Struct { def, .. } if self.tysys.type_table.borrow().struct_head_name(def) == "u128")
    }

    fn exh_literal(&self, lit: &Literal, scrutinee_type: TypeId) -> ExhPattern {
        match lit {
            Literal::Number(repr) => {
                if util::is_float_only_literal(repr) {
                    // Old path returned `Wildcard` (a catch-all) after emitting
                    // the float-literal error during binding.
                    return ExhPattern::CatchAll;
                }
                if self.exh_is_unsigned(scrutinee_type) {
                    match util::parse_u128_literal(repr) {
                        Ok(v) => ExhPattern::IntLit(v as i128),
                        Err(_) => ExhPattern::IntLit(0),
                    }
                } else {
                    match util::parse_i128_literal(repr) {
                        Ok(v) => ExhPattern::IntLit(v),
                        Err(_) => ExhPattern::IntLit(0),
                    }
                }
            }
            Literal::Bool(b) => ExhPattern::BoolLit(*b),
            Literal::Char(raw) => {
                ExhPattern::IntLit(util::unescape_char(raw).unwrap_or('\0') as i128)
            }
            Literal::Byte(raw) => {
                ExhPattern::IntLit(i128::from(util::unescape_byte(raw).unwrap_or(0)))
            }
            Literal::Null => {
                // `null` coerces to a `None` variant pattern when the scrutinee
                // has a `None` case; otherwise it is an opaque `Null` literal.
                if self.exh_null_none_case(scrutinee_type).is_some() {
                    ExhPattern::VariantCase(
                        self.tysys
                            .type_table
                            .borrow()
                            .compiler_variant_case_name(
                                crate::compiler_item::CompilerItem::OptionNone,
                            )
                            .to_string(),
                    )
                } else {
                    ExhPattern::Other
                }
            }
            _ => ExhPattern::Other,
        }
    }

    /// Mirrors `try_null_as_none_pattern`: returns the `None` case name when the
    /// scrutinee is a variant type that has a `None` case.
    fn exh_null_none_case(&self, scrutinee_type: TypeId) -> Option<()> {
        let variant_info = self.variant_of_type(scrutinee_type)?;
        let none_case_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_variant_case_name(crate::compiler_item::CompilerItem::OptionNone)
            .to_string();
        variant_info
            .cases
            .iter()
            .any(|c| c.name == none_case_name)
            .then_some(())
    }

    fn exh_variant(
        &mut self,
        variant_name: &str,
        variant_qualifier: Option<&ast::Type>,
        bindings: &[ast::Pattern],
        scrutinee_type: TypeId,
    ) -> ExhPattern {
        let normalized = self
            .strip_ns_prefix(variant_name)
            .unwrap_or(variant_name)
            .to_string();

        // Bare uppercase identifier that is not a known case: an associated
        // constant resolves to a `Literal` (if the const body is a literal) or
        // an opaque `ConstantValue`; otherwise it is a binding (catch-all).
        if bindings.is_empty()
            && !self.is_known_case_of_type(scrutinee_type, &normalized, variant_qualifier)
        {
            if let Some(AssocConstSig {
                value: const_expr, ..
            }) = self.associated_constant_qualified(variant_qualifier, variant_name)
            {
                if let ast::Expr::Literal(lit) = &const_expr {
                    match &lit.value {
                        Literal::Number(repr) if !util::is_float_only_literal(repr) => {
                            if self.exh_is_unsigned(scrutinee_type) {
                                if let Ok(v) = util::parse_u128_literal(repr) {
                                    return ExhPattern::IntLit(v as i128);
                                }
                            } else if let Ok(v) = util::parse_i128_literal(repr) {
                                return ExhPattern::IntLit(v);
                            }
                        }
                        Literal::Bool(v) => return ExhPattern::BoolLit(*v),
                        Literal::Char(raw) => {
                            let c = util::unescape_char(raw).unwrap_or('\0');
                            return ExhPattern::IntLit(c as i128);
                        }
                        Literal::Byte(raw) => {
                            return ExhPattern::IntLit(i128::from(
                                util::unescape_byte(raw).unwrap_or(0),
                            ));
                        }
                        _ => {}
                    }
                }
                // Opaque constant-value pattern.
                return ExhPattern::Other;
            }
            // Binding (catch-all).
            return ExhPattern::CatchAll;
        }

        // Qualifier mismatch → old path returned `Wildcard` (catch-all).
        if !self.pattern_qualifier_matches_scrutinee(scrutinee_type, variant_qualifier) {
            return ExhPattern::CatchAll;
        }

        let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        match &resolved {
            ResolvedType::Enum { .. } => {
                if let Some(enum_info) = self.enum_of_type(scrutinee_type)
                    && enum_info.find_case(&normalized).is_some()
                {
                    return ExhPattern::EnumCase(normalized);
                }
                // Unknown enum / case → old path returned `Wildcard`.
                ExhPattern::CatchAll
            }
            ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => {
                ExhPattern::VariantCase(normalized)
            }
            _ => ExhPattern::CatchAll,
        }
    }

    fn exh_range(
        &self,
        start: &ast::Pattern,
        end: &ast::Pattern,
        kind: ast::RangeKind,
        scrutinee_type: TypeId,
    ) -> ExhPattern {
        let is_unsigned = self.exh_is_unsigned(scrutinee_type);
        let (Some(start_val), Some(end_val)) = (
            self.exh_pattern_to_i128(start, is_unsigned),
            self.exh_pattern_to_i128(end, is_unsigned),
        ) else {
            // Bad bounds → old path returned `Wildcard` (catch-all).
            return ExhPattern::CatchAll;
        };
        let inclusive = matches!(kind, ast::RangeKind::Inclusive);
        // Reversed / empty ranges → old path returned `Wildcard` (catch-all).
        if start_val > end_val || (!inclusive && start_val >= end_val) {
            return ExhPattern::CatchAll;
        }
        let hi = if inclusive { end_val } else { end_val - 1 };
        ExhPattern::Range(start_val, hi)
    }

    /// Resolve a range-bound AST pattern to its `i128` value. Mirrors
    /// `Elaborator::pattern_to_i128`.
    fn exh_pattern_to_i128(&self, pattern: &ast::Pattern, is_unsigned: bool) -> Option<i128> {
        match pattern {
            ast::Pattern::Literal(Literal::Number(repr)) => {
                if is_unsigned {
                    util::parse_u128_literal(repr).ok().map(|v| v as i128)
                } else {
                    util::parse_i128_literal(repr).ok()
                }
            }
            ast::Pattern::Literal(Literal::Char(raw)) => {
                util::unescape_char(raw).ok().map(|c| c as i128)
            }
            ast::Pattern::Literal(Literal::Byte(raw)) => {
                util::unescape_byte(raw).ok().map(i128::from)
            }
            ast::Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                ..
            } if bindings.is_empty() => {
                super::stmt::primitive_assoc_const_to_i128(variant_qualifier.as_ref(), variant_name)
            }
            _ => None,
        }
    }

    fn check_variant_exhaustiveness(
        &self,
        classified: &[(bool, ExhPattern)],
        scrutinee_type: TypeId,
        span: Span,
    ) {
        if let Some(variant_info) = self.variant_of_type(scrutinee_type) {
            let all_cases: IndexSet<&str> =
                variant_info.cases.iter().map(|c| c.name.as_str()).collect();
            let covered: IndexSet<&str> = {
                let mut names = Vec::new();
                for (_, pat) in classified {
                    Self::collect_variant_case_names(pat, &mut names);
                }
                names.into_iter().collect()
            };
            let missing: Vec<&&str> = all_cases.difference(&covered).collect();
            if !missing.is_empty() {
                let missing_names: Vec<String> = missing.iter().map(|s| (*s).to_string()).collect();
                let _ = self.emit(TypeError::InvalidPattern {
                    message: format!(
                        "non-exhaustive match: missing {}",
                        Self::format_missing_cases(&missing_names),
                    ),
                    span,
                });
            }
        }
    }

    fn is_catch_all_pattern(pattern: &ExhPattern) -> bool {
        match pattern {
            ExhPattern::CatchAll => true,
            ExhPattern::Or(alternatives) => alternatives.iter().any(Self::is_catch_all_pattern),
            _ => false,
        }
    }

    fn collect_enum_case_names<'a>(pattern: &'a ExhPattern, out: &mut Vec<&'a str>) {
        match pattern {
            ExhPattern::EnumCase(case_name) => out.push(case_name),
            ExhPattern::Or(alternatives) => {
                for alt in alternatives {
                    Self::collect_enum_case_names(alt, out);
                }
            }
            _ => {}
        }
    }

    fn collect_variant_case_names<'a>(pattern: &'a ExhPattern, out: &mut Vec<&'a str>) {
        match pattern {
            ExhPattern::VariantCase(variant_name) => out.push(variant_name),
            ExhPattern::Or(alternatives) => {
                for alt in alternatives {
                    Self::collect_variant_case_names(alt, out);
                }
            }
            _ => {}
        }
    }

    fn pattern_contains_bool(pattern: &ExhPattern, value: bool) -> bool {
        match pattern {
            ExhPattern::BoolLit(b) => *b == value,
            ExhPattern::Or(alternatives) => alternatives
                .iter()
                .any(|p| Self::pattern_contains_bool(p, value)),
            _ => false,
        }
    }

    fn format_missing_cases(cases: &[String]) -> String {
        match cases.len() {
            1 => format!("case `{}`", cases[0]),
            2 => format!("cases `{}` and `{}`", cases[0], cases[1]),
            _ => {
                let last = &cases[cases.len() - 1];
                let rest: Vec<String> = cases[..cases.len() - 1]
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect();
                format!("cases {}, and `{last}`", rest.join(", "))
            }
        }
    }

    fn primitive_range(prim: crate::tir::PrimitiveType) -> Option<(i128, i128)> {
        use crate::tir::PrimitiveType;
        match prim {
            PrimitiveType::I8 => Some((i128::from(i8::MIN), i128::from(i8::MAX))),
            PrimitiveType::I16 => Some((i128::from(i16::MIN), i128::from(i16::MAX))),
            PrimitiveType::I32 => Some((i128::from(i32::MIN), i128::from(i32::MAX))),
            PrimitiveType::I64 => Some((i128::from(i64::MIN), i128::from(i64::MAX))),
            PrimitiveType::U8 => Some((0, i128::from(u8::MAX))),
            PrimitiveType::U16 => Some((0, i128::from(u16::MAX))),
            PrimitiveType::U32 => Some((0, i128::from(u32::MAX))),
            PrimitiveType::U64 => Some((0, i128::from(u64::MAX))),
            PrimitiveType::Char => Some((0, 0x0010_FFFF)),
            _ => None,
        }
    }

    fn collect_ranges_from_pattern(pattern: &ExhPattern) -> Vec<(i128, i128)> {
        match pattern {
            ExhPattern::Range(start, end) => vec![(*start, *end)],
            ExhPattern::IntLit(v) => vec![(*v, *v)],
            ExhPattern::BoolLit(b) => vec![(i128::from(*b), i128::from(*b))],
            ExhPattern::Or(alts) => {
                let mut result = Vec::new();
                for alt in alts {
                    result.extend(Self::collect_ranges_from_pattern(alt));
                }
                result
            }
            _ => vec![],
        }
    }

    fn check_integer_range_exhaustiveness(
        &self,
        classified: &[(bool, ExhPattern)],
        type_min: i128,
        type_max: i128,
        span: Span,
    ) {
        // Collect all ranges from all arms (only arms without guards count)
        let mut all_ranges: Vec<(i128, i128)> = Vec::new();
        let mut has_catch_all = false;

        for (guardless, pat) in classified {
            if *guardless && Self::is_catch_all_pattern(pat) {
                has_catch_all = true;
            }
            if !*guardless {
                continue;
            }
            all_ranges.extend(Self::collect_ranges_from_pattern(pat));
        }

        if has_catch_all {
            return;
        }

        // Check exhaustiveness: sort ranges and verify they cover [type_min, type_max]
        if all_ranges.is_empty() {
            let _ = self.emit(TypeError::InvalidPattern {
                message: "non-exhaustive match: integer type requires a wildcard `_` or full range coverage".to_string(),
                span,
            });
            return;
        }

        all_ranges.sort_unstable();
        // Merge overlapping/adjacent ranges
        let mut merged: Vec<(i128, i128)> = Vec::new();
        for (lo, hi) in all_ranges {
            if let Some(last) = merged.last_mut() {
                if lo <= last.1 + 1 {
                    last.1 = last.1.max(hi);
                } else {
                    merged.push((lo, hi));
                }
            } else {
                merged.push((lo, hi));
            }
        }

        // Check if merged ranges cover [type_min, type_max]
        let covers = merged.len() == 1 && merged[0].0 <= type_min && merged[0].1 >= type_max;
        if !covers {
            let _ = self.emit(TypeError::InvalidPattern {
                message: "non-exhaustive match: not all values in the integer range are covered"
                    .to_string(),
                span,
            });
        }
    }

    fn check_range_overlaps(&self, classified: &[(bool, ExhPattern)], span: Span) {
        // Collect ranges per arm (only guardless arms)
        let mut arm_ranges: Vec<Vec<(i128, i128)>> = Vec::new();
        for (guardless, pat) in classified {
            if !*guardless {
                continue;
            }
            let ranges = Self::collect_ranges_from_pattern(pat);
            if !ranges.is_empty() {
                arm_ranges.push(ranges);
            }
        }

        // Check for overlaps between different arms
        for i in 0..arm_ranges.len() {
            for j in (i + 1)..arm_ranges.len() {
                for &(a_lo, a_hi) in &arm_ranges[i] {
                    for &(b_lo, b_hi) in &arm_ranges[j] {
                        if a_lo <= b_hi && b_lo <= a_hi {
                            let _ = self.emit(TypeError::InvalidPattern {
                                message: "overlapping range patterns in match arms".to_string(),
                                span,
                            });
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Collect outer-binding names that are mutated inside an expression.
    /// Mutation = direct or compound assignment whose target's root
    /// identifier is the binding (e.g. `count`, `point.x`, `arr[i]`,
    /// `pair.p.children[i].name` all resolve to their root ident).
    ///
    /// Nested closures are skipped: they have their own capture context
    /// and run their own collector.
    pub(super) fn collect_mutated_vars(expr: &ast::Expr, result: &mut IndexSet<String>) {
        let mut collector = MutatedVarsCollector { result };
        collector.visit_expr(expr);
    }

    /// The method replacing a rejected `Slice<T>` ↔ `List<T>` cast.
    fn slice_list_conversion(
        tt: &TypeTable,
        source_type: TypeId,
        target_type: TypeId,
    ) -> Option<&'static str> {
        let source_base = tt.representation_head(source_type);
        let target_base = tt.representation_head(target_type);
        let slice_elem = |id| match tt.get(id) {
            ResolvedType::GenericInstance { def, type_args }
                if tt.compiler_item_def(crate::compiler_item::CompilerItem::Slice)
                    == Some(*def)
                    && type_args.len() == 1 =>
            {
                Some(type_args[0])
            }
            _ => None,
        };
        if let Some(elem) = slice_elem(source_base)
            && tt.as_list(target_base) == Some(elem)
        {
            return Some("to_list");
        }
        if let Some(elem) = slice_elem(target_base)
            && tt.as_list(source_base) == Some(elem)
        {
            return Some("as_slice");
        }
        None
    }

    pub(super) fn resolve_cast(
        &mut self,
        cast: &ast::CastExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let target_type = self.resolve_type(&cast.target_type);
        // A cast names its own result type, so nothing downstream reports an
        // unresolved target: reify's `ann_expression_types` filters the
        // recorded `UNKNOWN` out and its `.expect` is what fails instead.
        if target_type == TypeTable::UNKNOWN {
            let _ = self.emit(TypeError::UnknownType {
                name: self.get_type_name_full(&cast.target_type),
                span: cast.target_type.span(),
            });
            return TypeTable::ERROR;
        }

        // `[1, 2, 3] as List<i32>`, `[1, 2, 3] as SeqVec<i32>`
        if let Some(coerced) = self.try_coerce_tuple_to_sequence(&cast.expr, ctx, target_type) {
            return coerced;
        }

        // `{ a: 1, b: 2 } as TreeMap<String, i32>`
        if let Some(coerced) = self.try_coerce_struct_to_map(&cast.expr, ctx, target_type) {
            return coerced;
        }

        // `[1, 2] as [i64, i64]`: each element takes the target's own element
        // type, as the annotated form does.
        let has_spread = matches!(&cast.expr, ast::Expr::TupleLiteral(t)
            if t.elements.iter().any(|e| matches!(e, ast::Expr::Spread(..))));
        let expected_elems = self.tysys.type_table.borrow().as_tuple(target_type);
        if let ast::Expr::TupleLiteral(tuple_lit) = &cast.expr
            && !has_spread
            && let Some(expected_elems) = expected_elems
        {
            if tuple_lit.elements.len() != expected_elems.len() {
                let from_name = self.tysys.type_table.borrow().type_name(target_type);
                let _ = self.emit(TypeError::InvalidCast {
                    from: format!("a {}-element tuple", tuple_lit.elements.len()),
                    to: from_name,
                    hint: "the two tuples have different arities".to_string(),
                    span: cast.span,
                });
                return target_type;
            }
            for (elem, expected) in tuple_lit.elements.iter().zip(expected_elems) {
                let resolved = self.resolve_expr(elem, ctx, Some(expected));
                self.typecheck(resolved, expected, elem.span());
            }
            self.sem
                .types
                .expression_types
                .insert(cast.expr.id(), target_type);
            return target_type;
        }
        // A spread needs the general literal path, which expands it; the
        // typecheck reports a widening that expansion cannot do.
        if matches!(&cast.expr, ast::Expr::TupleLiteral(_))
            && has_spread
            && expected_elems.is_some()
        {
            let resolved = self.resolve_expr(&cast.expr, ctx, Some(target_type));
            self.typecheck(resolved, target_type, cast.expr.span());
            return target_type;
        }

        // Cast to i128/u128: expr as u128 → u128::from_u64(expr as u64)
        // For large literals: 170... as i128 → i128::from_pair(low, high)
        //
        // Which pair of words the literal has to fit is how the value is
        // stored, so the representation answers: `type Signed = i128` is that
        // pair too, and reading the name instead let an oversized literal
        // through it with no diagnostic. The cast still yields `target_type`.
        let repr_target = self
            .tysys
            .type_table
            .borrow()
            .representation_head(target_type);
        let struct_name = match self.tysys.type_table.borrow().get(repr_target).clone() {
            ResolvedType::Struct { .. } => {
                let tt = self.tysys.type_table.borrow();
                tt.nominal_def(repr_target).map(|def| {
                    (
                        FqTypeName::declared(tt.defs(), def),
                        tt.def_name(def).to_string(),
                    )
                })
            }
            _ => None,
        };

        if let Some((ref name, ref simple)) = struct_name
            && (simple == "u128" || simple == "i128")
        {
            // Handle number literal cast specially to support values > u64
            if let ast::Expr::Literal(lit) = &cast.expr
                && let Literal::Number(repr) = &lit.value
                && !util::is_float_only_literal(repr)
            {
                let parse_result = if simple == "u128" {
                    util::parse_u128_literal(repr).map(|v| v as i128)
                } else {
                    util::parse_i128_literal(repr)
                };

                match parse_result {
                    Ok(_) => return target_type,
                    Err(_) => {
                        let _ = self.emit(TypeError::InvalidLiteral {
                            message: format!("invalid {name} literal: {repr}"),
                            span: lit.span,
                        });
                    }
                }
            }

            // Handle negated number literal cast: -170... as i128
            if let ast::Expr::Unary(unary) = &cast.expr
                && unary.op == ast::UnaryOp::Neg
                && let ast::Expr::Literal(lit) = &unary.expr
                && let Literal::Number(repr) = &lit.value
                && !util::is_float_only_literal(repr)
                && simple == "i128"
            {
                // Parse the negated value directly using Rust's i128
                let negated_repr = format!("-{repr}");
                if util::parse_i128_literal(&negated_repr).is_ok() {
                    return target_type;
                }
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: format!("invalid i128 literal: -{repr}"),
                    span: unary.span,
                });
            }

            // General expression cast (not a literal)
            let source_type = self.resolve_expr(&cast.expr, ctx, None);

            // Check if source type is a numeric type we can convert from
            if self.tysys.type_table.borrow().is_integer(source_type)
                || self.tysys.type_table.borrow().is_float(source_type)
            {
                // Reify emits the two-step form,
                // `name::from_u64/from_i64(expr as u64/i64)`.
                return target_type;
            }
        }

        // Reify re-types a literal operand to the target's width — what makes
        // `as` the way to write a bit pattern (`0xFF as i8`) and how a literal
        // reaches a target wider than `i32` (`65 as i128`). It never lands on
        // `i32`, so the defaulted range check must not judge it.
        let source_type = match int_literal_operand(&cast.expr) {
            Some((lit, repr)) => {
                self.check_int_literal_parses(repr, lit.span);
                self.record_expression_type(cast.expr.id(), TypeTable::I32);
                self.record_expression_type(lit.id, TypeTable::I32);
                TypeTable::I32
            }
            None => self.resolve_expr(&cast.expr, ctx, None),
        };

        if source_type == TypeTable::ERROR {
            return TypeTable::ERROR;
        }

        // Every coercion above declined, so nothing relates these two: `as`
        // between aggregates is only ever a newtype step, which shares a base.
        let unrelated_aggregate = {
            let tt = self.tysys.type_table.borrow();
            // `i128` / `u128` are structs here; their rules below cover a
            // wide-int source only, so such a target never exempts an aggregate.
            let wide_int = |id| {
                matches!(tt.get(tt.representation_head(id)), ResolvedType::Struct { def, .. }
                    if tt.struct_head_name(*def) == "i128" || tt.struct_head_name(*def) == "u128")
            };
            // A tuple is a `GenericInstance` of a tuple head, so no arm of its own.
            let source_base = tt.representation_head(source_type);
            let source_is_aggregate = !wide_int(source_type)
                && matches!(
                    tt.get(source_base),
                    ResolvedType::Struct { .. }
                        | ResolvedType::GenericInstance { .. }
                        | ResolvedType::Variant { .. }
                );
            source_is_aggregate && source_base != tt.representation_head(target_type)
        };
        if unrelated_aggregate {
            let tt = self.tysys.type_table.borrow();
            let from_name = tt.type_name(source_type);
            let to_name = tt.type_name(target_type);
            let conversion = Self::slice_list_conversion(&tt, source_type, target_type);
            drop(tt);
            let hint = match conversion {
                Some(method) => format!(
                    "a slice is a view and a `List` owns its elements; use `.{method}()` instead"
                ),
                None => "the two types share no representation; `as` reinterprets only \
                         across a newtype boundary"
                    .to_string(),
            };
            let _ = self.emit(TypeError::InvalidCast {
                from: from_name,
                to: to_name,
                hint,
                span: cast.span,
            });
            // The target is the cast's answer, as the other invalid-cast arms
            // leave it; `error` would bury this one under a cascade.
            return target_type;
        }

        // Casts *from* i128/u128 (including newtypes of them) support:
        // f64/f32 (correctly rounded), the integer widths (truncating),
        // and i128 ↔ u128 (bit reinterpret) — each modulo newtypes, which
        // share their base's representation. Reify lowers them
        // (`try_reify_int128_source_cast`); here reject anything else, so
        // an unsupported target fails with a diagnostic instead of leaking
        // the wide-int struct ref into codegen. `char` targets are
        // excluded: the char-cast diagnostic below already covers them.
        {
            use crate::tir::PrimitiveType;
            let tt = self.tysys.type_table.borrow();
            let source_is_wide_int = matches!(
                tt.get(tt.representation_head(source_type)),
                ResolvedType::Struct { def, .. }
                    if tt.struct_head_name(*def) == "i128" || tt.struct_head_name(*def) == "u128"
            );
            let target_supported = !source_is_wide_int
                || match tt.get(tt.representation_head(target_type)) {
                    ResolvedType::Primitive(
                        PrimitiveType::F64
                        | PrimitiveType::F32
                        | PrimitiveType::I64
                        | PrimitiveType::U64
                        | PrimitiveType::I32
                        | PrimitiveType::U32
                        | PrimitiveType::I16
                        | PrimitiveType::U16
                        | PrimitiveType::I8
                        | PrimitiveType::U8
                        | PrimitiveType::Char,
                    ) => true,
                    ResolvedType::Struct { def, .. } => {
                        tt.struct_head_name(*def) == "i128" || tt.struct_head_name(*def) == "u128"
                    }
                    _ => false,
                };
            if !target_supported {
                let from_name = tt.type_name(source_type);
                let to_name = tt.type_name(target_type);
                drop(tt);
                let _ = self.emit(TypeError::InvalidCast {
                    from: from_name,
                    to: to_name,
                    hint: "i128/u128 can only be cast to numeric types".to_string(),
                    span: cast.span,
                });
            }
        }

        // Validate char casts: prohibit integer/float -> char (use char::from_u32 instead)
        // Exception: u8 -> char is always valid (0..255 are valid Unicode scalar values)
        let source_base = self
            .tysys
            .type_table
            .borrow()
            .representation_head(source_type);
        let target_base = self
            .tysys
            .type_table
            .borrow()
            .representation_head(target_type);
        if target_base == TypeTable::CHAR
            && source_base != TypeTable::CHAR
            && source_base != TypeTable::U8
        {
            let from_name = self.tysys.type_table.borrow().type_name(source_type);
            let _ = self.emit(TypeError::InvalidCast {
                from: from_name,
                to: "char".to_string(),
                hint: "use char::from_u32() or char::from_i32() for checked conversion".to_string(),
                span: cast.span,
            });
        }
        // char -> non-integer is invalid (char -> integer extracts code point)
        if source_base == TypeTable::CHAR
            && target_base != TypeTable::CHAR
            && !self.tysys.type_table.borrow().is_integer(target_base)
        {
            let to_name = self.tysys.type_table.borrow().type_name(target_type);
            let _ = self.emit(TypeError::InvalidCast {
                from: "char".to_string(),
                to: to_name,
                hint: "char can only be cast to integer types".to_string(),
                span: cast.span,
            });
        }

        // Reify rebuilds the `Cast` from `cast.expr` + the target
        // type recorded in `expression_types[cast.id]`; the char-cast
        // diagnostics above are the record-only work.
        target_type
    }

    /// The struct declaration an unnamed literal's target names, or `None`
    /// where it declares none and the literal interns by its fields.
    fn implicit_struct_target(&self, expected_type: Option<TypeId>) -> Option<crate::defs::DefId> {
        match *self.tysys.type_table.borrow().get(expected_type?) {
            ResolvedType::Struct {
                def: crate::tir::StructDef::Decl(def),
                ..
            } => Some(def),
            _ => None,
        }
    }

    pub(super) fn resolve_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // A literal with no name is a shape — unless the target declares a
        // struct, in which case `{ x: 1 }` *is* `Point { x: 1 }`. One body
        // decides that; how the declaration was reached is the only difference.
        let implicit_decl = if struct_lit.name.is_some() {
            None
        } else {
            let Some(def) = self.implicit_struct_target(expected_type) else {
                return self.resolve_anonymous_struct_literal(struct_lit, ctx, expected_type);
            };
            Some(def)
        };

        // `ns::Struct` canonicalizes to its `ns$Struct` alias for the registry
        // lookups below (struct_fields, symbols, …).
        let name: Option<String> = struct_lit.name.as_ref().map(|raw| {
            self.sem
                .imports
                .canonical_ns_ref(raw)
                .unwrap_or_else(|| raw.clone())
        });

        // Record use→def reference for the struct type name.
        if let (Some(name_id), Some(written)) = (struct_lit.name_id, name.as_ref()) {
            self.record_item_reference_by_name(name_id, written);
        }

        // Which declaration the written name means is the resolve pass's
        // answer, keyed on the name's own site: the module that wrote it is
        // the vantage that decides, aliases and `ns::Type` included, and a
        // literal inside a *foreign* default resolves in the module that wrote
        // the default rather than the one splicing it in. Everything below
        // reads the declaration; the name and module are renderings of it, for
        // the mangled instance name and the diagnostics.
        let struct_decl = implicit_decl.or_else(|| {
            struct_lit
                .name_id
                .and_then(|id| self.tysys.resolutions.declared(id))
        });
        let declared = struct_decl.and_then(|def| self.lookup_struct_fields_of_decl(def));
        // The canonical name, not the import alias — and for a function-local
        // `struct` its mangled storage name, which is what makes the
        // instance's `TypeId` its own.
        let (struct_name, struct_module_source) = if let Some(info) = declared {
            (info.name.clone(), info.module_source.clone())
        } else {
            // The name is not a struct, or reaches no declaration at all.
            // Diagnose rather than silently falling back — that fallback
            // creates a TypeId whose key does not match the registered struct
            // in WIR build, which used to surface as a downstream
            // `StructLiteral expected Ref WirType` panic. The best-effort pair
            // is still returned so later passes have something.
            // Reachable only for a written name: an implicit literal took its
            // declaration from the target's head, which has fields.
            let written = name.clone().unwrap_or_default();
            let _ = self.emit(TypeError::UnknownType {
                name: written.clone(),
                span: struct_lit.span,
            });
            (written, self.current_module_source.clone())
        };
        // `struct_name` is the storage name, carrying a local struct's
        // `@AstId`. A diagnostic says what the programmer wrote (§9).
        let display_name = struct_decl
            .map(|def| self.tysys.resolutions.defs().name(def).to_string())
            .unwrap_or_else(|| struct_name.clone());

        // Get expected field types using (name, module_source) lookup.
        //
        // An annotation naming this struct's instantiation pins the declared
        // parameters, so substitute them: `let c: P<u32> = P { left: 8, … }`
        // must expect `u32` for `left`, not the bare `T` a literal cannot be
        // typed by — it would settle on the default `i32` and then mismatch.
        let expected_args =
            expected_type.and_then(|ty| match self.tysys.type_table.borrow().get(ty) {
                ResolvedType::GenericInstance { def, type_args } if Some(*def) == struct_decl => {
                    Some(type_args.clone())
                }
                _ => None,
            });
        let resolved_struct_fields: Option<Vec<(String, TypeId)>> =
            self.struct_fields_of_written_decl(struct_decl).map(|info| {
                let params = info.type_param_type_ids.clone();
                let fields: Vec<(String, TypeId)> = info
                    .fields
                    .iter()
                    .map(|(name, type_id, _)| (name.clone(), *type_id))
                    .collect();
                let Some(args) = expected_args.filter(|a| a.len() == params.len()) else {
                    return fields;
                };
                let mut tt = self.tysys.type_table.borrow_mut();
                let substitution: crate::hashmap::IndexMap<u32, TypeId> = params
                    .iter()
                    .zip(args.iter())
                    .filter_map(|(param, arg)| match tt.get(*param) {
                        ResolvedType::TypeParam { index, .. }
                        | ResolvedType::TypePack { index, .. } => Some((*index, *arg)),
                        _ => None,
                    })
                    .collect();
                fields
                    .into_iter()
                    .map(|(name, type_id)| {
                        (name, tt.substitute_type_params(type_id, &substitution))
                    })
                    .collect()
            });
        // A struct that did not resolve has unknown fields, not zero of them —
        // its own diagnostic covers that, and the field checks below must stay
        // quiet. A struct that resolved to zero fields accepts none.
        let struct_fields_known = resolved_struct_fields.is_some();
        let struct_field_types: Vec<(String, TypeId)> = resolved_struct_fields.unwrap_or_default();

        // A named struct base is a complete `S`, so any field before the spread
        // (or a second spread) is fully overwritten and unused.
        let named_spread = struct_lit.spreads.first();
        if let Some(second) = struct_lit.spreads.get(1) {
            let _ = self.emit(TypeError::InvalidLiteral {
                message: "a named struct literal allows at most one `..base` spread".to_string(),
                span: second.span,
            });
        }
        if let Some(spread) = named_spread
            && spread.field_pos > 0
        {
            let _ = self.emit(TypeError::InvalidLiteral {
                message: "a field before `..base` is overwritten and never used; \
                          put `..base` first"
                    .to_string(),
                span: spread.span,
            });
        }
        // A spread with no other fields is a deep copy of `base`; use `base`.
        if let Some(spread) = named_spread
            && struct_lit.fields.is_empty()
        {
            let _ = self.emit(TypeError::InvalidLiteral {
                message: "`{ ..base }` with no other fields just copies `base`; \
                          use `base` directly"
                    .to_string(),
                span: spread.span,
            });
        }
        let spread_base_type: Option<TypeId> =
            named_spread.map(|spread| self.resolve_expr(&spread.expr, ctx, expected_type));

        // Record use→def references for each field name, pointing at the
        // field definition's AstId in the struct declaration.
        let field_refs: Vec<(AstId, AstId)> = self
            .struct_fields_of_written_decl(struct_decl)
            .map(|info| {
                struct_lit
                    .fields
                    .iter()
                    .filter_map(|f| {
                        info.fields
                            .iter()
                            .zip(info.field_ast_ids.iter())
                            .find(|((fname, _, _), _)| fname == &f.name)
                            .map(|(_, def_id)| (f.name_id, *def_id))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (use_id, def_id) in field_refs {
            self.record_reference_to_def(use_id, def_id);
        }

        // Resolve field expressions, converting tuple literals to arrays when needed.
        // For generic structs, tuple-to-sequence coercion may be deferred to a second
        // pass after type arguments are inferred from field values.
        // Indexes into `struct_lit.fields`, not into `fields`, which is sorted
        // into declaration order before the second pass reads it.
        let mut deferred_coercions: Vec<usize> = Vec::new();
        let fields: Vec<ResolvedField> = struct_lit
            .fields
            .iter()
            .enumerate()
            .map(|(provided_idx, field)| {
                let is_tuple_literal = matches!(&field.value, ast::Expr::TupleLiteral(_));

                let expected_field_type = struct_field_types
                    .iter()
                    .find(|(name, _)| name == &field.name)
                    .map(|(_, type_id)| *type_id);

                // For tuple literals in generic struct fields where the field type
                // contains type params (e.g., List<T>), skip providing the expected
                // type so the tuple isn't coerced yet. Instead, resolve as a plain
                // tuple and defer coercion to after type inference.
                let needs_deferred_coercion = is_tuple_literal
                    && expected_field_type
                        .is_some_and(|t| self.tysys.type_table.borrow().contains_type_param(t));
                let effective_expected = if needs_deferred_coercion {
                    None
                } else {
                    expected_field_type
                };

                // Use expected type for literal coercion (e.g., 0 -> u64 when field is u64)
                let type_id = self.resolve_expr(&field.value, ctx, effective_expected);

                // Track tuple literals whose coercion was deferred because the field
                // type had unresolved type parameters. After type inference, we'll
                // re-coerce with the concrete type (the second pass below records
                // the coercion via `try_coerce_tuple_to_sequence`; reify replays
                // it). The test is read from the AST — a spread tuple used to
                // resolve to a block (never deferred), so only spread-free tuple
                // literals are deferred here.
                let tuple_is_spread_free = matches!(
                    &field.value,
                    ast::Expr::TupleLiteral(t)
                        if !t.elements.iter().any(|e| matches!(e, Expr::Spread(..)))
                );
                let coercion_deferred = needs_deferred_coercion && tuple_is_spread_free;
                if coercion_deferred {
                    deferred_coercions.push(provided_idx);
                }
                // A field whose declared type names a slot is not a constraint
                // on the value — the value is what fixes the slot. Fields
                // sharing a slot are compared to each other in
                // `infer_struct_type_args`; comparing one against the
                // *inferred* argument instead reads back whatever the caller's
                // expected type put there, not this literal's answer.
                let field_names_slot = expected_field_type
                    .is_some_and(|t| self.tysys.type_table.borrow().contains_rigid_param(t));
                let check_deferred = coercion_deferred || field_names_slot;

                // Check field name exists in struct definition
                if struct_fields_known && !struct_field_types.iter().any(|(n, _)| n == &field.name)
                {
                    let _ = self.emit(TypeError::ExtraField {
                        struct_name: display_name.clone(),
                        field_name: field.name.clone(),
                        span: field.span,
                    });
                }

                // Check field value type against declared struct field type.
                // A field whose coercion was deferred still holds its literal
                // shape — the sequence coercion has not run — so checking it
                // here would compare `[…]` against the sequence it is about to
                // become. The second pass checks it once coerced.
                if !check_deferred
                    && let Some((_, expected_type_id)) =
                        struct_field_types.iter().find(|(n, _)| n == &field.name)
                {
                    self.typecheck(type_id, *expected_type_id, field.value.span());
                }

                let decl_idx = struct_field_types
                    .iter()
                    .position(|(n, _)| n == &field.name)
                    .unwrap_or(provided_idx);

                ResolvedField {
                    name: field.name.clone(),
                    type_id,
                    field_index: decl_idx as u32,
                    span: field.value.span(),
                }
            })
            .collect();

        // struct_module_source was already determined above (before field resolution).

        // Check for missing fields: fields without a declared default must be
        // provided; fields with `= expr` are synthesized from the default
        // expression (pure, resolved in the struct's module scope).
        let struct_field_defaults: Vec<Option<ast::Expr>> = self
            .struct_fields_of_written_decl(struct_decl)
            .map(|info| info.field_defaults.clone())
            .unwrap_or_default();
        let mut fields = fields;
        // Field names the user actually wrote in the literal, captured before
        // default synthesis below so an omitted-but-defaulted field is not
        // mistaken for an explicitly-provided one (matters for the visibility
        // check further down).
        let provided_names: IndexSet<String> = fields.iter().map(|f| f.name.clone()).collect();
        if !struct_field_types.is_empty() && struct_lit.spreads.is_empty() {
            for (idx, (expected_name, expected_type_id)) in struct_field_types.iter().enumerate() {
                if provided_names.contains(expected_name) {
                    continue;
                }
                let default_ast = struct_field_defaults.get(idx).and_then(Option::clone);
                if let Some(default_expr) = default_ast {
                    // The default is *foreign* AST owned by the struct's
                    // declaring module. Its free identifiers (e.g. a private
                    // `global` of that module) resolve in its scope via
                    // `default_scope_module` (the same callee-scope fallback
                    // `pad_args_with_defaults` uses for function defaults).
                    // Only scope is redirected, not fact keying: the default's
                    // nodes carry their own globally-unique `AstId`s, so its
                    // facts can't collide with a local node. `expected_type_id`
                    // still drives literal / `null → None` coercion.
                    let resolved = if struct_module_source == self.current_module_source {
                        self.resolve_expr(&default_expr, ctx, Some(*expected_type_id))
                    } else {
                        self.with_default_scope_module(Some(struct_module_source.clone()), |s| {
                            s.resolve_expr(&default_expr, ctx, Some(*expected_type_id))
                        })
                    };
                    self.typecheck(resolved, *expected_type_id, struct_lit.span);
                    fields.push(ResolvedField {
                        name: expected_name.clone(),
                        type_id: resolved,
                        field_index: idx as u32,
                        span: default_expr.span(),
                    });
                } else {
                    let _ = self.emit(TypeError::MissingField {
                        struct_name: display_name.clone(),
                        field_name: expected_name.clone(),
                        span: struct_lit.span,
                    });
                }
            }
            fields.sort_by_key(|f| f.field_index);
        }

        // Check field visibility: a non-pub field may not be *set* from another
        // module. Omitting a private field is allowed when it has a default —
        // the default is evaluated in the defining module, so encapsulation is
        // preserved — so only flag fields the user explicitly provided, not the
        // defaults synthesized above.
        let vantage = self.visibility_vantage(Some(struct_lit.id));
        if struct_module_source != vantage
            && let Some(struct_info) = self.struct_fields_of_written_decl(struct_decl)
        {
            let same_package = struct_module_source.same_package(&vantage);
            for (fname, _, vis) in &struct_info.fields {
                // Flagged when explicitly set, or read from `base` via a spread.
                let set_explicitly = provided_names.contains(fname);
                let read_via_spread = !struct_lit.spreads.is_empty() && !set_explicitly;
                if !vis.reachable_from(same_package) && (set_explicitly || read_via_spread) {
                    let _ = self.emit(TypeError::PrivateFieldAccess {
                        struct_name: display_name.clone(),
                        field_name: fname.clone(),
                        visibility: *vis,
                        span: struct_lit.span,
                    });
                }
            }
        }

        // `struct_name` / `struct_module_source` were just reassigned to the
        // canonical storage identity, so one `struct_fields_in` lookup on it
        // answers both "is this generic" and "whose fields are these". Checking
        // a module-level name set and a local-struct table separately could name
        // two different structs when a local shadows a module-level generic.
        let is_generic_struct = self
            .struct_fields_of_written_decl(struct_decl)
            .is_some_and(|info| !info.type_param_bounds.is_empty());
        let (struct_type, _mangled_struct_name, _fields) = if is_generic_struct {
            // This is a generic struct - infer type arguments from field values.
            // `expected_type` lets the caller's annotation (e.g.
            // `let x: Container<i32> = Container { value: 0 }`) fill phantom
            // parameters that never appear in a field, matching the
            // behaviour of plain function calls.
            let type_args = self.infer_struct_type_args(
                struct_decl,
                &fields,
                expected_type.or(spread_base_type),
                struct_lit.span,
            );

            // Substitute type parameters in field value types.
            // This is necessary for empty array literals in self-referential fields
            // (e.g., `children: []` in `Node<K> { children: List<&Node<K>> }`)
            // which get typed with TypeParams before inference.
            //
            // Use map-based substitution (TypeId → TypeId) instead of index-based, so
            // that only the struct's own TypeParam TypeIds are replaced. Index-based
            // substitution incorrectly replaces TypeParams from outer scopes (e.g., impl
            // type params) that happen to share the same index as the struct's TypeParams.
            let mut fields: Vec<ResolvedField> = if type_args.is_empty() {
                fields
            } else {
                let struct_param_map: IndexMap<TypeId, TypeId> = self
                    .struct_fields_of_written_decl(struct_decl)
                    .map(|info| {
                        info.type_param_type_ids
                            .iter()
                            .zip(type_args.iter())
                            .map(|(&param_id, &concrete_id)| (param_id, concrete_id))
                            .collect()
                    })
                    .unwrap_or_default();
                fields
                    .into_iter()
                    .map(|mut field| {
                        field.type_id =
                            self.substitute_type_params_by_map(field.type_id, &struct_param_map);
                        field
                    })
                    .collect()
            };

            // Second pass: apply deferred tuple-to-sequence coercion now that
            // concrete type arguments are known. For example, [10, 20, 30] in
            // `Container<i32> { items: [10, 20, 30] }` needs List<i32> coercion,
            // but at first pass the field type was List<T> (type param).
            for &ast_idx in &deferred_coercions {
                let ast_field = &struct_lit.fields[ast_idx];
                let Some(concrete_type) = struct_field_types
                    .iter()
                    .find(|(name, _)| name == &ast_field.name)
                    .map(|(_, type_id)| {
                        if type_args.is_empty() {
                            *type_id
                        } else {
                            self.substitute_type_params(*type_id, &type_args)
                        }
                    })
                else {
                    continue;
                };
                let Some(field_idx) = fields.iter().position(|f| f.name == ast_field.name) else {
                    continue;
                };
                if let Some(coerced) =
                    self.try_coerce_tuple_to_sequence(&ast_field.value, ctx, concrete_type)
                {
                    fields[field_idx].type_id = coerced;
                }
                // The check the first pass skipped — but only once the slot is
                // actually filled. A field type that still names a rigid
                // parameter is one this literal did not pin, and comparing
                // against a declaration's own slot is the very thing the first
                // pass was skipping.
                if !self
                    .tysys
                    .type_table
                    .borrow()
                    .contains_rigid_param(concrete_type)
                {
                    self.typecheck(
                        fields[field_idx].type_id,
                        concrete_type,
                        ast_field.value.span(),
                    );
                }
            }

            // Check trait bounds on inferred type arguments
            if let Some(struct_info) = self.struct_fields_of_written_decl(struct_decl).cloned() {
                for (i, (param_name, bounds)) in struct_info.type_param_bounds.iter().enumerate() {
                    if let Some(&type_arg) = type_args.get(i) {
                        for bound in bounds {
                            let Some(bound_def) = self.bound_trait_def(bound.site) else {
                                continue;
                            };
                            if !self.tysys.type_implements_trait(
                                &self.annotate_ctx,
                                &self.type_lookup(),
                                type_arg,
                                bound_def,
                            ) {
                                let type_name = self.tysys.type_id_to_string(type_arg);
                                let reason = self.tysys.trait_unimpl_reason_chain(
                                    &self.annotate_ctx,
                                    &self.type_lookup(),
                                    type_arg,
                                    &bound.name,
                                );
                                let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                                    type_name,
                                    trait_name: bound.name.clone(),
                                    param_name: param_name.clone(),
                                    reason,
                                    span: struct_lit.span,
                                });
                            }
                        }
                    }
                }
            }

            // The declaration comes from the node that declares it where the
            // walk can see one — a function-local generic struct's storage
            // name is mangled, and no declaration is registered under that
            // spelling. A literal may still name nothing at all: the
            // undefined-struct diagnostic is emitted above, and the walk
            // continues on `unknown` rather than stopping at the first
            // unresolved name.
            let declared_at = self
                .struct_fields_of_written_decl(struct_decl)
                .map(|info| info.defined_at);
            let struct_type = {
                let def = declared_at.and_then(|ast| self.tysys.resolutions.defs().of_ast_id(ast));
                match def {
                    Some(def) => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .make_generic_instance(def, type_args.clone()),
                    None => TypeTable::UNKNOWN,
                }
            };
            // Build mangled name with type arguments
            let arg_names: Vec<String> = type_args
                .iter()
                .map(|&t| self.tysys.type_table.borrow().type_name(t))
                .collect();
            let mangled_name = mangle_generic_name(&struct_name, &arg_names);
            // WEP 2026-05-26: record the inferred
            // type_args + the resulting `GenericInstance` + the mangled
            // name so reify can emit `TirExprKind::StructLiteral { struct_type,
            // struct_name, … }` without re-running `infer_struct_type_args`
            // or `mangle_generic_name`.
            self.record_generic_instantiation_with_mangle(
                struct_lit.id,
                type_args,
                struct_type,
                Some(mangled_name.clone()),
            );
            (struct_type, mangled_name, fields)
        } else {
            let defined_at = self
                .struct_fields_of_written_decl(struct_decl)
                .map(|info| info.defined_at);
            let struct_type = defined_at.map_or(TypeTable::UNKNOWN, |defined_at| {
                self.tysys.type_table.borrow().type_id_of_decl(defined_at)
            });
            (struct_type, struct_name, fields)
        };

        if let (Some(base_ty), Some(spread)) = (spread_base_type, named_spread) {
            self.typecheck(base_ty, struct_type, spread.span);
        }

        // Reify rebuilds the `StructLiteral` (`reify_struct_literal`)
        // from the AST + the recorded `generic_instantiations` mangled name /
        // instance type; the body walk resolved the fields (and applied any
        // deferred tuple-to-sequence coercion) for their fact-recording side
        // effects. Project only the struct type.
        struct_type
    }

    /// Field list of a struct-typed value for spread projection:
    /// `(field name, concrete type, declared index, visibility)`, plus the
    /// struct's defining module. `None` when `type_id` is not a struct.
    pub(super) fn spread_struct_fields(
        &self,
        type_id: TypeId,
    ) -> Option<(
        ModuleSource,
        Vec<(String, TypeId, u32, crate::ast::Visibility)>,
    )> {
        let (head, type_args) = peel_to_struct(&self.tysys.type_table.borrow(), type_id)?;
        let info = self.lookup_struct_fields_of(head)?.clone();
        let subst: IndexMap<u32, TypeId> = (0..type_args.len() as u32)
            .zip(type_args.iter().copied())
            .collect();
        let fields = info
            .fields
            .iter()
            .enumerate()
            .map(|(i, (fname, fty, vis))| {
                let concrete = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .substitute_type_params(*fty, &subst);
                (fname.clone(), concrete, i as u32, *vis)
            })
            .collect();
        Some((info.module_source, fields))
    }

    /// Diagnose an anonymous composition: a member whose every field is
    /// overwritten by a later member is dead, and a spread may not read a field
    /// unreachable across a module boundary. Called only from the resolve pass.
    fn check_union_composition(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        base_types: &[TypeId],
        base_info: &[BaseSpreadInfo],
    ) {
        let base_names: Vec<Vec<String>> = base_info
            .iter()
            .map(|(_, f)| {
                f.as_ref()
                    .map(|(_, fs)| fs.iter().map(|(n, ..)| n.clone()).collect())
                    .unwrap_or_default()
            })
            .collect();

        // Dead-write: a member whose every field is contributed by a later member.
        let members: Vec<(crate::token::Span, Vec<String>)> = struct_lit
            .members()
            .iter()
            .map(|m| match m {
                ast::LiteralMember::Spread(si, sp) => (sp.span, base_names[*si].clone()),
                ast::LiteralMember::Field(_, f) => (f.span, vec![f.name.clone()]),
            })
            .collect();
        for i in 0..members.len() {
            let names = &members[i].1;
            if names.is_empty() {
                continue;
            }
            let fully_shadowed = names
                .iter()
                .all(|n| members[i + 1..].iter().any(|(_, later)| later.contains(n)));
            if fully_shadowed {
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: "this member is fully overwritten by a later spread/field \
                              and has no effect"
                        .to_string(),
                    span: members[i].0,
                });
            }
        }

        // A base field is read only where that base is its final contributor;
        // an overridden field is never read, so it needs no reachability check.
        let mut final_base_src: IndexMap<String, usize> = IndexMap::default();
        for m in struct_lit.members() {
            match m {
                ast::LiteralMember::Spread(si, _) => {
                    for name in &base_names[si] {
                        final_base_src.insert(name.clone(), si);
                    }
                }
                ast::LiteralMember::Field(_, f) => {
                    final_base_src.shift_remove(&f.name);
                }
            }
        }
        for (name, &base_idx) in &final_base_src {
            let Some((module, fields)) = &base_info[base_idx].1 else {
                continue;
            };
            let vantage = self.visibility_vantage(Some(struct_lit.id));
            if *module == vantage {
                continue;
            }
            let same_package = module.same_package(&vantage);
            let Some((.., vis)) = fields.iter().find(|(n, ..)| n == name) else {
                continue;
            };
            if !vis.reachable_from(same_package) {
                let _ = self.emit(TypeError::PrivateFieldAccess {
                    struct_name: self.tysys.type_id_to_string(base_types[base_idx]),
                    field_name: name.clone(),
                    visibility: *vis,
                    span: struct_lit.span,
                });
            }
        }
    }

    /// Resolve an anonymous struct literal `{ x: 1, y: 2 }` by inferring a struct type
    /// from the field names and types.
    fn resolve_anonymous_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Gather each base's field list once, reused below. A `TreeMap` is a
        // struct too, so a `From<Array<[K, V]>>` impl marks it as a map rather
        // than a composable struct.
        let spread_base_types: Vec<TypeId> = struct_lit
            .spreads
            .iter()
            .map(|spread| self.resolve_expr(&spread.expr, ctx, None))
            .collect();
        let base_info: Vec<BaseSpreadInfo> = spread_base_types
            .iter()
            .map(|&t| {
                let is_map = self.is_key_value_literal_target(t);
                let fields = if is_map {
                    None
                } else {
                    self.spread_struct_fields(t)
                };
                (is_map, fields)
            })
            .collect();

        let has_spread = !struct_lit.spreads.is_empty();
        let compose_union = has_spread && base_info.iter().all(|(m, f)| !m && f.is_some());
        let all_map = base_info.iter().all(|(m, _)| *m);
        let expected_is_map = expected_type.is_some_and(|t| self.is_key_value_literal_target(t));
        // A pure key-value merge with a map-typed target is the only valid
        // non-composition spread.
        let is_kv_merge = has_spread && all_map && expected_is_map;
        // A single spread with no other members deep-copies `base` (a multi-spread
        // composition of distinct bases does not).
        let is_copy = struct_lit.spreads.len() == 1 && struct_lit.fields.is_empty();
        // A base that already errored is skipped, to avoid a cascading diagnostic.
        let base_errored = spread_base_types
            .iter()
            .any(|&t| t == TypeTable::ERROR || t == TypeTable::UNKNOWN);

        if !base_errored {
            if is_copy {
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: "`{ ..base }` with no other members just copies `base`; \
                              use `base` directly"
                        .to_string(),
                    span: struct_lit.spreads[0].span,
                });
            } else if has_spread && !compose_union && !is_kv_merge {
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: "a `..base` spread must be a struct value (composition) or a \
                              key-value map with a map-typed target; a non-struct base or a \
                              mix of struct and map spreads is not allowed"
                        .to_string(),
                    span: struct_lit.spreads[0].span,
                });
            }
        }

        let mut resolved_fields: Vec<ResolvedField> = Vec::new();
        for (index, field) in struct_lit.fields.iter().enumerate() {
            let type_id = self.resolve_expr(&field.value, ctx, None);
            resolved_fields.push(ResolvedField {
                name: field.name.clone(),
                type_id,
                field_index: index as u32,
                span: field.value.span(),
            });
        }

        // A composition's shape is the union of its bases and explicit fields;
        // otherwise just the explicit fields.
        let effective_fields: Vec<(String, TypeId)> = if compose_union {
            self.check_union_composition(struct_lit, &spread_base_types, &base_info);
            let base_field_lists: Vec<Vec<(String, TypeId, u32)>> = base_info
                .iter()
                .map(|(_, f)| {
                    f.as_ref()
                        .map(|(_, fs)| {
                            fs.iter()
                                .map(|(n, ty, i, _)| (n.clone(), *ty, *i))
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect();
            let explicit_types: Vec<TypeId> = resolved_fields.iter().map(|f| f.type_id).collect();
            compose_union_plan(struct_lit, &base_field_lists, &explicit_types)
                .into_iter()
                .map(|uf| (uf.name, uf.type_id))
                .collect()
        } else {
            resolved_fields
                .iter()
                .map(|f| (f.name.clone(), f.type_id))
                .collect()
        };

        // An anonymous struct literal names no declaration: its head is the
        // shape its fields make, so two literals of one shape intern to one
        // type, and the rendered spelling is derived from that shape.
        let shape = self.tysys.type_table.borrow_mut().intern_anon_struct(
            self.current_module_source.clone(),
            effective_fields
                .iter()
                .map(|(fname, fty)| (fname.clone(), *fty))
                .collect(),
        );
        let head = crate::tir::StructDef::Anon(shape);
        let anon_name = self.tysys.type_table.borrow().anon_struct_mangle(shape);

        let module_source = self.current_module_source.clone();

        let existing_type = self.tysys.type_table.borrow().find_struct_type(head);
        if let Some(existing_type) = existing_type {
            self.record_generic_instantiation_with_mangle(
                struct_lit.id,
                vec![],
                existing_type,
                Some(anon_name),
            );
            self.mark_generic_instantiation_union(struct_lit.id, compose_union);
            // Reify rebuilds the anonymous `StructLiteral`
            // (`reify_anonymous_struct_literal`); project only the type.
            return existing_type;
        }

        let struct_type = self.tysys.type_table.borrow_mut().make_struct(head);

        // Register field info so field access works
        let field_info = super::types::StructFieldInfo {
            name: anon_name.clone(),
            module_source,
            // Anonymous struct literals have no `StructDecl`; the literal
            // expression's own `AstId` is the closest thing to a declaration
            // site and is unique per literal, which is what matters here.
            defined_at: struct_lit.id,
            fields: effective_fields
                .iter()
                .map(|(fname, fty)| (fname.clone(), *fty, crate::ast::Visibility::Public))
                .collect(),
            field_ast_ids: Vec::new(),
            field_defaults: vec![None; effective_fields.len()],
            type_param_bounds: Vec::new(),
            type_param_type_ids: Vec::new(),
            type_param_defaults: Vec::new(),
        };
        self.sem.decls.anon_struct_fields.insert(shape, field_info);

        // Create TirStruct definition for codegen
        let tir_fields: Vec<TirField> = effective_fields
            .iter()
            .enumerate()
            .map(|(i, (fname, fty))| TirField {
                name: fname.clone(),
                visibility: crate::ast::Visibility::Public,
                type_id: *fty,
                index: i as u32,
                span: struct_lit.span,
                is_secret: false,
                wire_name_override: None,
                serde_default: false,
                serde_positional: false,
                default_expr: None,
            })
            .collect();

        self.sem.decls.pending_anonymous_structs.push(TirStruct {
            def: head,
            type_args: Vec::new(),
            name: anon_name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: crate::ast::Visibility::Private,
            type_params: Vec::new(),
            monomorph_info: None,
            fields: tir_fields,
            span: struct_lit.span,
            wire_name_policy: None,
        });

        self.record_generic_instantiation_with_mangle(
            struct_lit.id,
            vec![],
            struct_type,
            Some(anon_name),
        );
        self.mark_generic_instantiation_union(struct_lit.id, compose_union);

        // Reify rebuilds the anonymous `StructLiteral`; the body walk
        // registered the struct type, field info, and pending TirStruct above
        // for their side effects. Project only the type.
        struct_type
    }

    /// Flag the anonymous instantiation at `ast_id` as a union composition so
    /// reify projects union fields from the spread bases.
    fn mark_generic_instantiation_union(&mut self, ast_id: AstId, is_union: bool) {
        if is_union && let Some(gi) = self.sem.types.generic_instantiations.get_mut(&ast_id) {
            gi.is_union = true;
        }
    }

    /// Infer a generic struct's type arguments by running [`InferCtx`] over its
    /// declared field types against the literal's values. An `expected_type`
    /// that is a `GenericInstance` of the same struct is unified in too, so a
    /// phantom parameter still lands concrete. Returns the *partial* result: an
    /// unbound parameter keeps its `TypeParam` id for the monomorphizer.
    pub(super) fn infer_struct_type_args(
        &mut self,
        struct_decl: Option<crate::defs::DefId>,
        fields: &[ResolvedField],
        expected_type: Option<TypeId>,
        span: Span,
    ) -> Vec<TypeId> {
        let Some(struct_info) = self.struct_fields_of_written_decl(struct_decl).cloned() else {
            return vec![];
        };
        if struct_info.type_param_type_ids.is_empty() {
            return vec![];
        }

        // Instantiate the declaration's slots. Inside a generic body the
        // literal's fields carry the *enclosing* item's parameters, which can
        // share `(name, index)` with the struct's own — `IterMap { inner:
        // *self, f }` inside `Iterator::map<U>` renders both as `IterMap<I,
        // U>` while meaning different things.
        let inst = self.instantiate(
            &struct_info.type_param_type_ids,
            &Instantiation {
                kind: "struct",
                name: &struct_info.name,
                span,
            },
        );
        let decl_field_types: Vec<TypeId> = struct_info.fields.iter().map(|(_, t, _)| *t).collect();
        let field_types = self.instantiate_types(&decl_field_types, &inst);
        let decl_field_names: Vec<&str> = struct_info
            .fields
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect();

        // Two fields mentioning one slot are each other's evidence: the slot
        // takes what the first of them says, so a later one has to agree.
        // Nothing else compares them — the first-pass check is skipped for a
        // field naming a slot (the value is what fixes the slot), and checking
        // against the *inferred* argument instead cannot work here, because the
        // caller's expected type fills the same slot: that is what turned the
        // prelude's `IterChain { first: *self, second: other }` into `expected
        // StrCharIter, found J`. Only the literal's own fields are consulted.
        let mut agreed: IndexMap<TypeId, TypeId> = IndexMap::default();
        for (struct_field, expected_field_type) in
            declared_pairs(fields, &field_types, &decl_field_names)
        {
            let mut bindings: IndexMap<TypeId, TypeId> = IndexMap::default();
            super::infer::unify(
                &self.tysys.type_table,
                expected_field_type,
                struct_field.type_id,
                &mut bindings,
            );
            for (var, answer) in bindings {
                let Some(&first) = agreed.get(&var) else {
                    agreed.insert(var, answer);
                    continue;
                };
                let disagrees = {
                    let table = self.tysys.type_table.borrow();
                    !table.contains_undecided(first)
                        && !table.contains_undecided(answer)
                        && matches!(
                            super::typecheck::check_assignable(answer, first, &table),
                            super::typecheck::TypeCheckResult::Incompatible
                        )
                };
                if disagrees {
                    let _ = self.emit(TypeError::TypeMismatch {
                        expected: self.tysys.type_table.borrow().type_name(first),
                        found: self.tysys.type_table.borrow().type_name(answer),
                        span: struct_field.span,
                    });
                }
            }
        }

        let mut infer = InferCtx::new(&self.tysys.type_table, inst.vars.clone());

        for (struct_field, expected_field_type) in
            declared_pairs(fields, &field_types, &decl_field_names)
        {
            infer.add(expected_field_type, struct_field.type_id);
        }

        // Back-infer from the caller's expected type: if it's a GenericInstance
        // of this same struct, unify its type-args against the declaration-order
        // type params so phantoms (fields-less params) get concrete bindings.
        if let Some(expected) = expected_type {
            let expected_resolved = self.tysys.type_table.borrow().get(expected).clone();
            if let ResolvedType::GenericInstance {
                def,
                type_args: expected_args,
            } = expected_resolved
                && Some(def) == struct_decl
                && expected_args.len() == struct_info.type_param_type_ids.len()
            {
                for (&var, &expected_arg) in inst.vars.iter().zip(expected_args.iter()) {
                    infer.add_expected_return(var, expected_arg);
                }
            }
        }

        let mut inferred = infer.solve();
        // A phantom parameter — one no field mentions — is not an inference
        // failure: the declaration's own parameter *is* the answer, and
        // monomorphization substitutes it. A slot a field does mention and
        // nothing solved is a failure, so its variable stays put to be blamed
        // and reported.
        //
        // Recorded before the answers are, so a phantom's variable is solved
        // to that parameter rather than left unsolved and pinned to `error`
        // at finalize behind no diagnostic.
        for (slot, answer) in inferred.iter_mut().enumerate() {
            let decl_param = struct_info.type_param_type_ids[slot];
            let is_phantom = {
                let table = self.tysys.type_table.borrow();
                let index = match table.get(decl_param) {
                    ResolvedType::TypeParam { index, .. }
                    | ResolvedType::TypePack { index, .. } => *index,
                    _ => continue,
                };
                !decl_field_types
                    .iter()
                    .any(|&f| table.contains_type_param_index(f, index))
            };
            if inst.vars.get(slot) == Some(answer) && is_phantom {
                *answer = decl_param;
            }
        }
        self.record_instantiation(&inst, &inferred);
        self.blame_unsolved(&inst, &inferred);
        inferred
    }

    /// Check if a type contains a `TypePack` (variadic pack parameter).
    pub(super) fn type_contains_pack(&self, type_id: TypeId) -> bool {
        let ty = self.tysys.type_table.borrow().get(type_id).clone();
        match ty {
            ResolvedType::TypePack { .. } => true,
            ResolvedType::GenericInstance { def, type_args }
                if TypeTable::is_tuple_type(self.tysys.type_table.borrow().def_name(def)) =>
            {
                type_args.iter().any(|e| self.type_contains_pack(*e))
            }
            _ => false,
        }
    }

    /// The local slot bound to the index of `for let [i, v] of t.enumerate()`,
    /// once the binding is in scope. `None` when the form is not an enumerate
    /// or the index position is a wildcard.
    pub(super) fn enumerate_index_local(
        is_enumerate: bool,
        binding: &ast::Pattern,
        ctx: &FunctionContext,
    ) -> Option<u32> {
        if !is_enumerate {
            return None;
        }
        let name = Self::enumerate_index_binding_name(binding)?;
        ctx.lookup(&name).map(|local| local.index)
    }

    /// Split a `t.enumerate()` iterable into its receiver and the flag, leaving
    /// any other expression alone. The suffix stays in the AST so the formatter
    /// round-trips it, matching how the statement for-of reads it.
    pub(super) fn split_enumerate(iterable: &Expr) -> (&Expr, bool) {
        match iterable {
            Expr::MethodCall(mc) if mc.method == "enumerate" && mc.args.is_empty() => {
                (&mc.receiver, true)
            }
            other => (other, false),
        }
    }

    /// The pack a comprehension's iterable walks: its `(name, index)` and the
    /// type the binding takes for one element.
    ///
    /// A mapped pack (`[..StructField<T, F>]`) binds the mapped element, the
    /// same choice the variadic for-of makes.
    pub(super) fn comprehension_pack_elem(
        type_table: &std::cell::RefCell<TypeTable>,
        iterable_type: TypeId,
    ) -> Option<TypeId> {
        Self::comprehension_pack_of(type_table, iterable_type).map(|(_, _, elem)| elem)
    }

    pub(super) fn comprehension_pack(
        &self,
        iterable_type: TypeId,
    ) -> Option<(String, u32, TypeId)> {
        Self::comprehension_pack_of(&self.tysys.type_table, iterable_type)
    }

    fn comprehension_pack_of(
        type_table: &std::cell::RefCell<TypeTable>,
        iterable_type: TypeId,
    ) -> Option<(String, u32, TypeId)> {
        let type_table = type_table.borrow();
        let (elems, _) = type_table.as_tuple_through_ref(iterable_type)?;
        elems.iter().find_map(|&e| match type_table.get(e) {
            ResolvedType::TypePack {
                name,
                index,
                mapped_elem,
            } => Some((name.clone(), *index, mapped_elem.unwrap_or(e))),
            _ => None,
        })
    }

    /// Resolve `[for let v of tuple { expr }]`.
    ///
    /// Only a pack-typed tuple is walkable: a concrete tuple's elements have
    /// unrelated types, so the body would need resolving once per element.
    /// The result is the mapped pack `[..body_type]` — one element per source
    /// element, each carrying the body's type at that element.
    pub(super) fn resolve_tuple_comprehension(
        &mut self,
        comp: &ast::TupleComprehensionExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let (source, is_enumerate) = Self::split_enumerate(&comp.iterable);
        let iterable_type = self.resolve_expr(source, ctx, None);
        let Some((pack_name, pack_index, elem_type)) = self.comprehension_pack(iterable_type)
        else {
            let type_name = self.tysys.type_table.borrow().type_name(iterable_type);
            let _ = self.emit(TypeError::InvalidPattern {
                message: format!(
                    "a tuple comprehension walks a variadic tuple (`[..T]`); `{type_name}` is not one"
                ),
                span: comp.span,
            });
            return TypeTable::UNKNOWN;
        };

        let binding_type = if is_enumerate {
            self.tysys
                .type_table
                .borrow_mut()
                .make_tuple(vec![TypeTable::I32, elem_type])
        } else {
            elem_type
        };

        ctx.enter_scope();
        self.bind_comprehension_pattern(&comp.binding, binding_type, comp.span, ctx);
        let index_binding = Self::enumerate_index_local(is_enumerate, &comp.binding, ctx);
        if let Some(local) = index_binding {
            ctx.variadic_enumerate_indices.push(local);
        }
        let body_type = self.resolve_expr(&comp.body, ctx, None);
        if index_binding.is_some() {
            ctx.variadic_enumerate_indices.pop();
        }
        ctx.exit_scope();

        // A body that yields the element unchanged reproduces the source shape;
        // anything else maps the pack through the body's type.
        if body_type == elem_type {
            return iterable_type;
        }
        let mut type_table = self.tysys.type_table.borrow_mut();
        let mapped = type_table.make_mapped_type_pack(pack_name, pack_index, body_type);
        type_table.make_tuple(vec![mapped])
    }

    /// Bind a comprehension's element pattern: an ident, or the sub-idents of a
    /// tuple pattern (`[i, v]`).
    fn bind_comprehension_pattern(
        &mut self,
        binding: &ast::Pattern,
        binding_type: TypeId,
        fallback_span: Span,
        ctx: &mut FunctionContext,
    ) {
        match binding {
            ast::Pattern::Ident { id, name, span } => {
                ctx.add_local_at(name.clone(), binding_type, false, Some(*id), *span);
                self.record_local_symbol(*id, name, *span, false, binding_type);
            }
            ast::Pattern::Tuple(elems, _) => {
                let inner = self
                    .tysys
                    .type_table
                    .borrow()
                    .as_tuple(binding_type)
                    .unwrap_or_else(|| vec![binding_type]);
                for (i, elem) in elems.iter().enumerate() {
                    if let ast::Pattern::Ident { id, name, span } = elem {
                        let elem_type = inner.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                        ctx.add_local_at(name.clone(), elem_type, false, Some(*id), *span);
                        self.record_local_symbol(*id, name, *span, false, elem_type);
                    }
                }
            }
            _ => {
                let _ = self.emit(TypeError::InvalidPattern {
                    message: "a tuple comprehension binds an identifier or `[i, v]`".to_string(),
                    span: fallback_span,
                });
            }
        }
    }

    /// Resolve a tuple literal expression: `[1, 2, 3]` or `[1, "hello", true]`
    pub(super) fn resolve_tuple_literal(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // When the expected type is a concrete tuple of matching arity and the
        // literal has no spread elements, propagate per-element expected types
        // so numeric literals and nested tuples are coerced to the target shape.
        let expected_elem_types: Option<Vec<TypeId>> = expected_type.and_then(|ty| {
            let has_spread = tuple_lit
                .elements
                .iter()
                .any(|e| matches!(e, Expr::Spread(..)));
            if has_spread {
                return None;
            }
            let elems = self.tysys.type_table.borrow().as_tuple(ty)?;
            if elems.len() != tuple_lit.elements.len() {
                return None;
            }
            Some(elems)
        });

        // Resolve each element for its side effects and collect the element
        // types so the tuple `TypeId` matches what reify builds.
        // Reify's `reify_tuple_literal` owns the element / spread-expansion /
        // single-evaluation-temporary construction (with its own `ctx`), so
        // this records only the types + the spread diagnostic.
        let mut elem_types: Vec<TypeId> = Vec::new();
        for (elem_idx, elem) in tuple_lit.elements.iter().enumerate() {
            if let Expr::Spread(inner, _span) = elem {
                let spread_type_id = self.resolve_expr(inner, ctx, None);
                if self.type_contains_pack(spread_type_id) {
                    // A direct `TypePack` (`[..T::method()]`) or a tuple
                    // containing one (`[..rest]` where `rest: [..T]`): the
                    // spread element keeps the spread's own type; monomorphize
                    // expands it later.
                    elem_types.push(spread_type_id);
                } else if let Some(mapped) = self.spread_pack_map_type(inner, spread_type_id) {
                    // Pack-map `..F::method()` whose return type is
                    // pack-independent: a homogeneous pack of the return type,
                    // arity `|F|`, expanded at monomorphization.
                    elem_types.push(mapped);
                } else {
                    let spread_type = self.tysys.type_table.borrow().get(spread_type_id).clone();
                    if let ResolvedType::GenericInstance {
                        def,
                        type_args: inner_elems,
                    } = spread_type
                        && TypeTable::is_tuple_type(self.tysys.type_table.borrow().def_name(def))
                    {
                        // A concrete tuple spread expands inline to one element
                        // per field.
                        elem_types.extend(inner_elems);
                    } else {
                        let _ = self.emit(crate::elaborator::types::TypeError::InvalidLiteral {
                            message: "spread operator `..` can only be used with tuple types"
                                .to_string(),
                            span: elem.span(),
                        });
                        elem_types.push(spread_type_id);
                    }
                }
            } else {
                let elem_expected = expected_elem_types.as_ref().map(|v| v[elem_idx]);
                let resolved = self.resolve_expr(elem, ctx, elem_expected);
                elem_types.push(resolved);
            }
        }

        self.tysys.type_table.borrow_mut().make_tuple(elem_types)
    }

    /// The `(name, index)` of the type pack a spread operand's static call is
    /// made on: `F::method()` parses as a `Call` whose callee is a two-segment
    /// path `F::method`; returns `Some` when `F` is a type pack in scope.
    pub(super) fn call_pack_subject(&self, inner: &Expr) -> Option<(String, u32)> {
        let Expr::Call(call) = inner else {
            return None;
        };
        let Expr::Ident(id) = &call.callee else {
            return None;
        };
        if id.segments.len() != 2 {
            return None;
        }
        let subject_name = &id.segments[0].name;
        let &BinderInScope {
            index,
            type_id: tid,
            ..
        } = self.annotate_ctx.trait_ctx.type_params.get(subject_name)?;
        matches!(
            self.tysys.type_table.borrow().get(tid),
            ResolvedType::TypePack { .. }
        )
        .then(|| (subject_name.clone(), index))
    }

    /// If `inner` is a static call on a type-pack subject (`F::method()`) whose
    /// return type `result_type` is pack-independent, build the mapped pack
    /// `..F::method()` — `result_type` repeated `|F|` times.
    fn spread_pack_map_type(&mut self, inner: &Expr, result_type: TypeId) -> Option<TypeId> {
        let (name, index) = self.call_pack_subject(inner)?;
        // Only this scope knows `F` is a pack rather than a plain type param,
        // so record the answer for reify instead of leaving it to re-derive one.
        self.sem
            .types
            .pack_spread_subjects
            .insert(inner.id(), (name.clone(), index));
        Some(
            self.tysys
                .type_table
                .borrow_mut()
                .make_mapped_type_pack(name, index, result_type),
        )
    }

    /// Reconstruct the operand's expected type for `?` from the `?`-stripped
    /// expected payload `u` and the function's return type: `Option<u>` or
    /// `Result<u, F>`. `None` when there is no payload or the return type is not
    /// an Option/Result (a `?`-misuse `resolve_question_mark` reports).
    fn question_mark_operand_expected(
        &mut self,
        expected_payload: Option<TypeId>,
        return_type: TypeId,
    ) -> Option<TypeId> {
        let u = expected_payload?;
        if u == TypeTable::UNKNOWN || u == TypeTable::ERROR {
            return None;
        }
        // `None` error slot => Option wrapper; `Some(err)` => Result<_, err>.
        let result_err = {
            let tt = self.tysys.type_table.borrow();
            if tt.as_option(return_type).is_some() {
                None
            } else if let ResolvedType::GenericInstance { type_args, .. } = tt.get(return_type)
                && tt.is_result(return_type)
                && type_args.len() == 2
            {
                Some(type_args[1])
            } else {
                return None;
            }
        };
        let mut tt = self.tysys.type_table.borrow_mut();
        Some(match result_err {
            None => tt.make_option(u),
            Some(err) => tt.make_result(u, err),
        })
    }

    /// Resolve the postfix `?`, desugaring `expr?` into a match that unwraps the
    /// success case and returns early on failure: `Result<T, E>` in a function
    /// returning `Result<U, F>` becomes
    /// `match expr { Ok(v) => v, Err(e) => return Result::Err(F::from(e)) }`,
    /// and `Option<T>` becomes `match expr { Some(v) => v, None => return null }`.
    pub(super) fn resolve_question_mark(
        &mut self,
        qm: &ast::TryOpExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Propagate the `?`-stripped expected type backward to the operand, so a
        // generic call whose `T` is only in the Ok/Some payload infers from an
        // LHS annotation (`let v: U = call()?`) without a turbofish.
        let operand_expected = self.question_mark_operand_expected(expected_type, ctx.return_type);
        let inner_type = self.resolve_expr(&qm.expr, ctx, operand_expected);
        let tt = self.tysys.type_table.borrow();
        let type_name = tt.type_name(inner_type);

        // Determine whether the operand is Option<T> or Result<T, E>
        let is_option = tt.as_option(inner_type).is_some();
        let is_result = tt.is_result(inner_type);
        drop(tt);

        if !is_option && !is_result {
            let _ = self.emit(TypeError::InvalidQuestionMark {
                message: format!("cannot use ? on type {type_name}"),
                span: qm.span,
            });
            return TypeTable::UNIT;
        }

        // Check that the enclosing function returns a compatible type
        let return_type = ctx.return_type;
        let tt = self.tysys.type_table.borrow();
        let ret_is_option = tt.as_option(return_type).is_some();
        let ret_is_result = tt.is_result(return_type);
        drop(tt);

        if is_option && !ret_is_option {
            let _ = self.emit(TypeError::InvalidQuestionMark {
                message: "cannot use ? on Option in a function returning Result".to_string(),
                span: qm.span,
            });
            return TypeTable::UNIT;
        }
        if is_result && !ret_is_result {
            if ret_is_option {
                let _ = self.emit(TypeError::InvalidQuestionMark {
                    message: "cannot use ? on Result in a function returning Option".to_string(),
                    span: qm.span,
                });
            } else {
                let _ = self.emit(TypeError::InvalidQuestionMark {
                    message: "? requires function to return Result or Option".to_string(),
                    span: qm.span,
                });
            }
            return TypeTable::UNIT;
        }

        if is_option {
            self.resolve_question_mark_option(inner_type, ctx)
        } else {
            self.resolve_question_mark_result(inner_type, ctx, qm.id)
        }
    }

    fn resolve_question_mark_option(
        &mut self,
        inner_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let some_type = self
            .tysys
            .type_table
            .borrow()
            .as_option(inner_type)
            .unwrap();

        // Allocate a local for the Some payload binding (walk-order parity).
        ctx.enter_scope();
        let _v_local = ctx.add_local("__qm_v".to_string(), some_type, false, None);
        ctx.exit_scope();

        // Reify rebuilds the `Option` `?` desugar
        // (`reify_question_mark_option`) from the AST, allocating its own
        // `__qm_v` local. The body walk keeps the scope/local allocation
        // (walk-order parity) and projects the unwrapped `Some` payload type.
        some_type
    }

    fn resolve_question_mark_result(
        &mut self,
        inner_type: TypeId,
        ctx: &mut FunctionContext,
        qm_id: AstId,
    ) -> TypeId {
        let return_type = ctx.return_type;

        // Extract T, E from inner Result<T, E>
        let tt = self.tysys.type_table.borrow();
        let (ok_type, inner_err_type) = match tt.get(inner_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => panic!("? operand must be Result<T, E>"),
        };
        // Extract F from return Result<U, F>
        let outer_err_type = match tt.get(return_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => type_args[1],
            _ => panic!("? return type must be Result<U, F>"),
        };
        drop(tt);

        ctx.enter_scope();
        // The `__qm_v` local is allocated for walk-order parity; reify rebuilds
        // the `?` desugar and its own bindings, so the index is not kept here.
        ctx.add_local("__qm_v".to_string(), ok_type, false, None);
        ctx.add_local("__qm_e".to_string(), inner_err_type, false, None);

        // Record the `From::from(e)` conversion facts when the inner and outer
        // error types differ (no-op when they match). `resolve_from_call`
        // writes `FromCallFacts` keyed by the `?` AstId; reify replays the
        // conversion from the AST + those facts.
        if inner_err_type != outer_err_type {
            let _ = self.resolve_from_call(outer_err_type, inner_err_type, qm_id);
        }

        ctx.exit_scope();

        // Reify rebuilds the `Result` `?` desugar
        // (`reify_question_mark_result`) from the AST + the recorded
        // `FromCallFacts`. The body walk keeps the scope / local allocation
        // and the `resolve_from_call` fact-recording, and projects the
        // unwrapped `Ok` payload type.
        ok_type
    }

    /// Record the facts for `target_type::from(value)` off the
    /// `impl From<from_type> for target_type`, and answer with the conversion's
    /// result type. `caller_id` is the source expression that triggered the
    /// conversion — the `?` operator, an explicit `T::from(v)` — and the facts
    /// are recorded under it so reify rebuilds the `Call` without re-walking
    /// impl blocks or re-mangling.
    pub(super) fn resolve_from_call(
        &mut self,
        target_type: TypeId,
        from_type: TypeId,
        caller_id: crate::ast::AstId,
    ) -> TypeId {
        let tt = self.tysys.type_table.borrow();
        let target_name = tt.type_name(target_type);
        let from_name = tt.fq_type_name(from_type);
        let from_trait_name = tt.compiler_trait_fq(crate::compiler_item::CompilerItem::From);
        drop(tt);

        // `From<SourceType>` as the trait segment disambiguates several `From`
        // impls on one target type.
        let from_trait = from_trait_name.clone().with_args(vec![from_name.clone()]);
        // The receiver the method name is built from — the same value reify
        // puts on the call's `method_info`, so the two cannot drift.
        let target_receiver = self.qualified_receiver_name(&target_name);
        let method_name = MethodName::format_local(&target_receiver, Some(&from_trait), "from");

        // The block that provides the `From` impl, and where its body lives.
        let (impl_def, module_source) = self.find_from_impl(&target_name, &from_name);

        let key = caller_id;
        self.sem.types.from_call_facts.insert(
            key,
            super::sem::types::FromCallFacts {
                method_def: impl_def.and_then(|def| self.tysys.declared_method(def, "from")),
                module_source,
                mangled_name: method_name,
                target_name: target_receiver,
                from_name,
                from_trait_name,
            },
        );

        target_type
    }

    /// The `impl From<from_name> for target_name` block and the module that
    /// wrote it. No block where the synthesis pass mints the impl later.
    fn find_from_impl(
        &self,
        target_name: &str,
        from_name: &crate::name::FqTypeName,
    ) -> (Option<crate::defs::DefId>, ModuleSource) {
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        // Read off the impl headers: a block's trait reference and its
        // argument are header facts, so the impls are reached by the target's
        // canonical key rather than by scanning every module for one whose
        // written target name matches.
        let declares_from = |key: &crate::defs::DefId| -> bool {
            self.tysys
                .trait_env
                .impl_headers
                .get(key)
                .is_some_and(|header| {
                    header.trait_name.as_deref() == Some(from_trait_name.as_str())
                        && matches!(&header.trait_type, Some(ast::Type::Generic(g))
                        if g.args.first().is_some_and(|arg| {
                            // The header's argument and the call's source type
                            // are compared as the declarations they name, not
                            // as the spellings each side wrote.
                            super::trait_env::written_type_arg(arg, &self.tysys.resolutions)
                                == *from_name
                        }))
                })
        };
        let keys = self
            .tysys
            .trait_env
            .all_impl_keys(&self.impl_target(target_name));
        // The current module wins a tie.
        let defs = self.tysys.resolutions.defs();
        keys.iter()
            .find(|key| *defs.module(**key) == self.current_module_source && declares_from(key))
            .or_else(|| keys.iter().find(|key| declares_from(key)))
            .map(|key| (Some(*key), defs.module(*key).clone()))
            // The `From` impl may be synthesized later, so a miss is not an error.
            .unwrap_or_else(|| (None, self.current_module_source.clone()))
    }
}

enum LiteralOrdValue {
    Int(i128),
    Float(f64),
    Char(u32),
}

impl LiteralOrdValue {
    fn is_greater_than(&self, other: &Self) -> bool {
        match (self, other) {
            (LiteralOrdValue::Int(a), LiteralOrdValue::Int(b)) => a > b,
            (LiteralOrdValue::Float(a), LiteralOrdValue::Float(b)) => a > b,
            (LiteralOrdValue::Char(a), LiteralOrdValue::Char(b)) => a > b,
            _ => false, // different kinds — type mismatch error handles this
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Extract a compile-time orderable value from a literal expression.
    /// Returns the value in its native representation to avoid precision loss.
    fn extract_literal_ord_value(expr: &Expr) -> Option<LiteralOrdValue> {
        match expr {
            Expr::Literal(lit) => match &lit.value {
                Literal::Number(s) => {
                    let s = s.replace('_', "");
                    if s.contains('.') {
                        s.parse::<f64>().ok().map(LiteralOrdValue::Float)
                    } else if s.starts_with("0x") || s.starts_with("0X") {
                        i128::from_str_radix(&s[2..], 16)
                            .ok()
                            .map(LiteralOrdValue::Int)
                    } else if s.starts_with("0b") || s.starts_with("0B") {
                        i128::from_str_radix(&s[2..], 2)
                            .ok()
                            .map(LiteralOrdValue::Int)
                    } else if s.starts_with("0o") || s.starts_with("0O") {
                        i128::from_str_radix(&s[2..], 8)
                            .ok()
                            .map(LiteralOrdValue::Int)
                    } else {
                        s.parse::<i128>().ok().map(LiteralOrdValue::Int)
                    }
                }
                Literal::Char(s) => super::util::unescape_char(s)
                    .ok()
                    .map(|c| LiteralOrdValue::Char(c as u32)),
                Literal::Byte(s) => super::util::unescape_byte(s)
                    .ok()
                    .map(|b| LiteralOrdValue::Int(i128::from(b))),
                _ => None,
            },
            Expr::Unary(unary) if unary.op == ast::UnaryOp::Neg => {
                match Self::extract_literal_ord_value(&unary.expr)? {
                    LiteralOrdValue::Int(v) => Some(LiteralOrdValue::Int(-v)),
                    LiteralOrdValue::Float(v) => Some(LiteralOrdValue::Float(-v)),
                    LiteralOrdValue::Char(_) => None,
                }
            }
            Expr::Cast(cast) => Self::extract_literal_ord_value(&cast.expr),
            _ => None,
        }
    }

    /// Resolve a range expression: `a..<b` or `a..=b`
    pub(super) fn resolve_range(
        &mut self,
        range: &crate::ast::RangeExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        use crate::ast::RangeKind;

        // Bidirectional coercion: resolve non-literal first to infer the element type
        let start_is_literal = self.tysys.is_numeric_literal(&range.start);
        let end_is_literal = self.tysys.is_numeric_literal(&range.end);

        let (start, end) = if start_is_literal && !end_is_literal {
            let end = self.resolve_expr(&range.end, ctx, None);
            let start = self.resolve_expr(&range.start, ctx, Some(end));
            (start, end)
        } else {
            let start = self.resolve_expr(&range.start, ctx, None);
            let end = self.resolve_expr(&range.end, ctx, Some(start));
            (start, end)
        };

        // Check type mismatch between start and end
        if start != end && start != TypeTable::ERROR && end != TypeTable::ERROR {
            let type_table = self.tysys.type_table.borrow();
            let start_name = type_table.type_name(start);
            let end_name = type_table.type_name(end);
            if start_name != end_name {
                let op_str = match range.kind {
                    RangeKind::Exclusive => "..<",
                    RangeKind::Inclusive => "..=",
                };
                let _ = self.emit(TypeError::TypeMismatch {
                    expected: start_name,
                    found: format!(
                        "{end_name} (range `{op_str}` requires both operands to have the same type)"
                    ),
                    span: range.span,
                });
                return TypeTable::ERROR;
            }
        }

        let element_type = start;

        // Check that the element type implements Ord
        let ord_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::Ord)
            .to_string();
        let ord = self
            .tysys
            .compiler_trait_def(crate::compiler_item::CompilerItem::Ord);
        if element_type != TypeTable::ERROR
            && !ord.is_some_and(|trait_| {
                self.tysys.type_implements_trait(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    element_type,
                    trait_,
                )
            })
        {
            let type_name = self.tysys.type_id_to_string(element_type);
            let reason = self.tysys.trait_unimpl_reason_chain(
                &self.annotate_ctx,
                &self.type_lookup(),
                element_type,
                &ord_trait_name,
            );
            let _ = self.emit(TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name: ord_trait_name,
                param_name: "T".to_string(),
                reason,
                span: range.span,
            });
            return TypeTable::ERROR;
        }

        // Check for reversed range literals (start > end)
        if let Some(start_val) = Self::extract_literal_ord_value(&range.start)
            && let Some(end_val) = Self::extract_literal_ord_value(&range.end)
        {
            let is_reversed = start_val.is_greater_than(&end_val);
            if is_reversed {
                let op_str = match range.kind {
                    RangeKind::Exclusive => "..<",
                    RangeKind::Inclusive => "..=",
                };
                let _ = self.emit(TypeError::InvalidLiteral {
                    message: format!(
                        "reversed range `{op_str}` is not supported (start must be less than end)"
                    ),
                    span: range.span,
                });
                return TypeTable::ERROR;
            }
        }

        let item = match range.kind {
            RangeKind::Exclusive => crate::compiler_item::CompilerItem::RangeExclusive,
            RangeKind::Inclusive => crate::compiler_item::CompilerItem::RangeInclusive,
        };
        let struct_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .struct_name(item)
            .to_string();

        let struct_type = {
            let def = self
                .tysys
                .type_table
                .borrow()
                .require_compiler_item_def(item);
            self.tysys
                .type_table
                .borrow_mut()
                .make_generic_instance(def, vec![element_type])
        };

        // Mangled name for the resulting `TirExprKind::StructLiteral`.
        // The monomorphizer keys instantiation lookup on this form
        // (`RangeExclusive<i32>`), so there is one spelling of it: the body
        // walk mangles it here and records it, and reify reads it back instead
        // of running its own `type_name(t)` + `mangle_generic_name`.
        let arg_names = vec![self.tysys.type_table.borrow().type_name(element_type)];
        let mangled_name = mangle_generic_name(&struct_name, &arg_names);
        self.record_generic_instantiation_with_mangle(
            range.id,
            vec![element_type],
            struct_type,
            Some(mangled_name),
        );

        struct_type
    }
}

/// Walks a closure body and records the outer bindings it mutates: the root
/// identifier of each `Assign` / `CompoundAssign` target, the one that survives
/// `.field` and `[index]` accessors. A nested closure is not descended — it runs
/// its own collector. Everything else falls through to `AstVisitor`'s `walk_*`
/// defaults, so there is no `_ => {}` here for new syntax to slip past.
struct MutatedVarsCollector<'a> {
    result: &'a mut IndexSet<String>,
}

impl MutatedVarsCollector<'_> {
    /// Walk an l-value down to its root identifier so `point.x = ...`
    /// and `arr[i] = ...` count as mutations of `point` / `arr`.
    fn root_ident_of_lvalue(expr: &ast::Expr) -> Option<&str> {
        match expr {
            ast::Expr::Ident(id) => Some(&id.name),
            ast::Expr::FieldAccess(fa) => Self::root_ident_of_lvalue(&fa.expr),
            ast::Expr::Index(idx) => Self::root_ident_of_lvalue(&idx.expr),
            _ => None,
        }
    }
}

impl AstVisitor for MutatedVarsCollector<'_> {
    fn visit_expr(&mut self, expr: &ast::Expr) {
        match expr {
            ast::Expr::Assign(a) => {
                if let Some(name) = Self::root_ident_of_lvalue(&a.target) {
                    self.result.insert(name.to_string());
                }
                // Still descend into the target (it may contain
                // sub-expressions like `arr[bump()] = ...`) and the value.
                ast::walk_expr(self, expr);
            }
            ast::Expr::CompoundAssign(ca) => {
                if let Some(name) = Self::root_ident_of_lvalue(&ca.target) {
                    self.result.insert(name.to_string());
                }
                ast::walk_expr(self, expr);
            }
            // Nested closures get their own capture context — skip.
            ast::Expr::Closure(_) => {}
            // Everything else: let the generic walker recurse into every
            // sub-expression. Adding new `Expr` variants therefore does
            // not require touching this collector.
            _ => ast::walk_expr(self, expr),
        }
    }
}

/// How to name the type an impl member is declared on.
pub(super) enum MemberOwner<'a> {
    Type(TypeId),
    Named(&'a str),
    Written(Option<&'a ast::Type>),
}

/// The segment naming an associated constant's owner — `K` in `K::SECRET` and
/// in `ns::K::SECRET`.
fn assoc_const_owner_segment(ident: &ast::IdentExpr) -> &str {
    ident
        .segments
        .len()
        .checked_sub(2)
        .map_or(ident.name.as_str(), |i| ident.segments[i].name.as_str())
}
