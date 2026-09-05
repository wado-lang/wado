//! Argument type synthesis — what a call site knows about an argument's type
//! before any candidate signature exists, which overload selection (WEP
//! 2026-07-31) needs and elaboration cannot yet give. The resulting
//! [`ArgClass`] must *contain* the real type, so an inexact premise widens
//! rather than guessing, and synthesis leaves no trace but interning.

use super::sig::AssocConstSig;
use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::name::FqTypeName;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

use super::Elaborator;
use super::callee::CalleeRef;
use super::types::{FunctionContext, MethodOwner};
use super::util::is_float_only_literal;

/// What a call site knows about one argument's type. Each variant denotes a
/// *set* of types (see the module docs); a candidate survives selection when
/// its parameter type is in that set at every position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ArgClass {
    /// Exactly this type. A `&mut T` argument also answers a `&T` parameter —
    /// argument passing's one coercion.
    Exact(TypeId),
    /// A type whose head declaration is known but whose arguments are not:
    /// `0..<n` is a `RangeExclusive` of *something*. Admits any parameter with
    /// that head, and any newtype over one.
    Head(FqTypeName),
    IntLit,
    FloatLit,
    StrLit,
    NullLit,
    /// No type was produced; admits every parameter. The reason is carried so
    /// the ambiguity diagnostic can say what defeated selection.
    Opaque(OpaqueReason),
}

/// Why synthesis produced no type. A closed set: the judgement matches on every
/// `ast::Expr` variant with no wildcard arm, so a new expression form must
/// either get a rule or name the reason it has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpaqueReason {
    /// A closure whose parameter or return types are unannotated: the
    /// parameter it is passed to is what types it.
    Closure,
    /// `[…]`, `{…}`, a spread — a compound literal builds whatever the
    /// expected type's builder trait says.
    CompoundLiteral,
    /// The type exists but depends on inference: an open generic return type,
    /// `?`, `resume`, branches that disagree.
    Inference,
    /// A name or type that did not resolve. Error recovery — the real walk
    /// reports it.
    Unresolved,
}

impl ArgClass {
    /// The greatest lower bound — what a primitive binary operator knows from
    /// its operands. Sharpening is admissible only because both operands and
    /// the result share one type there, so an `Exact` operand pins the result.
    fn meet(self, other: Self) -> Self {
        match (&self, &other) {
            (Self::Exact(_), _) => self,
            (_, Self::Exact(_)) => other,
            (Self::IntLit, Self::FloatLit) | (Self::FloatLit, Self::IntLit) => Self::FloatLit,
            _ if self == other => self,
            _ => Self::Opaque(OpaqueReason::Inference),
        }
    }
}

/// The call site's scope, plus the names the argument binds for itself — a
/// block's `let`, a match arm's pattern, an `if let`. Those shadow the call
/// site: `l.push(match e { E::A(x) => x, … })` must not be answered with an
/// outer `x`'s type. A shadowed name is opaque rather than typed, since typing
/// it would mean running the binder's own inference.
struct SynthScope<'a> {
    ctx: &'a FunctionContext,
    shadowed: Vec<String>,
}

impl<'a> SynthScope<'a> {
    fn new(ctx: &'a FunctionContext) -> Self {
        Self {
            ctx,
            shadowed: Vec::new(),
        }
    }

    fn shadows(&self, name: &str) -> bool {
        self.shadowed.iter().any(|n| n == name)
    }
}

/// One call's argument classes, computed on demand and remembered.
///
/// Selection consults it only for an overload set — several concrete candidates
/// of one trait declaration at different argument lists — so a call with a
/// single candidate pays nothing. The memo keeps the several lookup attempts one
/// method call makes from re-synthesizing.
pub(super) struct ArgProbe<'a> {
    args: &'a [ast::Expr],
    ctx: &'a FunctionContext,
    memo: Vec<Option<ArgClass>>,
}

impl<'a> ArgProbe<'a> {
    pub(super) fn new(args: &'a [ast::Expr], ctx: &'a FunctionContext) -> Self {
        Self {
            args,
            ctx,
            memo: Vec::new(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.args.len()
    }

    /// The classes synthesized so far, in argument order, releasing the
    /// borrow of the `FunctionContext` the caller needs back to elaborate the
    /// arguments. `None` where selection never asked.
    pub(super) fn take_classes(self) -> Vec<Option<ArgClass>> {
        self.memo
    }

    pub(super) fn class<H: CompilerHost>(
        &mut self,
        elaborator: &mut Elaborator<'_, H>,
        index: usize,
    ) -> ArgClass {
        if self.memo.len() != self.args.len() {
            self.memo.resize(self.args.len(), None);
        }
        if let Some(class) = &self.memo[index] {
            return class.clone();
        }
        // Copied out first: synthesis needs them while `self.memo` is written.
        let (args, ctx) = (self.args, self.ctx);
        let class = elaborator.synthesize_arg_class(&args[index], ctx);
        self.memo[index] = Some(class.clone());
        class
    }
}

/// The names a pattern binds. Shares the walk with the or-pattern handler so
/// one pattern shape cannot be a binder there and invisible here.
fn pattern_binding_names(pattern: &ast::Pattern) -> Vec<String> {
    let mut bindings = crate::hashmap::IndexMap::default();
    super::stmt::collect_ast_pattern_binding_ids(pattern, &mut bindings);
    bindings.into_keys().collect()
}

/// The names an `if let` / `while let` condition binds for its then-branch.
fn condition_binding_names(condition: &ast::Condition) -> Vec<String> {
    match condition {
        ast::Condition::Expr(_) => Vec::new(),
        ast::Condition::LetChain { elements, .. } => elements
            .iter()
            .flat_map(|element| match element {
                ast::ConditionElement::Let { pattern, .. } => pattern_binding_names(pattern),
                ast::ConditionElement::Expr(_) => Vec::new(),
            })
            .collect(),
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Classify one argument. See the module documentation for the soundness
    /// invariant and the side-effect discipline this entry point enforces.
    pub(super) fn synthesize_arg_class(
        &mut self,
        expr: &ast::Expr,
        ctx: &FunctionContext,
    ) -> ArgClass {
        let logger = self.logger;
        let _quiet = logger.quiet();
        #[cfg(debug_assertions)]
        let facts_before = self.sem.fact_count();
        let mut scope = SynthScope::new(ctx);
        let class = self.with_reference_recording_suppressed(|s| s.synth(expr, &mut scope));
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            facts_before,
            self.sem.fact_count(),
            "argument synthesis recorded a fact; it must only read"
        );
        class
    }

    /// Whether a candidate's parameter type is in `class`'s denoted set.
    /// Openness first: a parameter still mentioning a type parameter is not
    /// yet comparable and admits everything.
    pub(super) fn class_admits(&self, param: TypeId, class: &ArgClass) -> bool {
        let tt = self.tysys.type_table.borrow();
        if param == TypeTable::UNKNOWN || param == TypeTable::ERROR || tt.contains_type_param(param)
        {
            return true;
        }
        match class {
            ArgClass::Opaque(_) => true,
            ArgClass::Exact(t) => {
                if *t == param {
                    return true;
                }
                matches!(
                    (tt.get(param), tt.get(*t)),
                    (ResolvedType::Ref(p), ResolvedType::MutRef(a)) if p == a
                )
            }
            // A newtype over the head is admitted too; admitting more is the
            // safe side.
            ArgClass::Head(head) => {
                tt.fq_base_type_name(param) == *head
                    || tt.fq_base_type_name(tt.representation_head(param)) == *head
            }
            ArgClass::IntLit => {
                let base = tt.representation_head(param);
                matches!(
                    tt.get(base),
                    ResolvedType::Primitive(
                        PrimitiveType::I8
                            | PrimitiveType::I16
                            | PrimitiveType::I32
                            | PrimitiveType::I64
                            | PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::U64
                            | PrimitiveType::F32
                            | PrimitiveType::F64
                    )
                ) || tt.wide_int_item(base).is_some()
            }
            ArgClass::FloatLit => matches!(
                tt.get(tt.representation_head(param)),
                ResolvedType::Primitive(PrimitiveType::F32 | PrimitiveType::F64)
            ),
            ArgClass::StrLit => tt.base_type_name(tt.representation_head(param)) == "String",
            ArgClass::NullLit => tt.as_option(param).is_some(),
        }
    }

    /// The soundness invariant: every argument must have elaborated to a type
    /// in its class's denotation. A violation means synthesis
    /// under-approximated, which is the direction that selects the wrong impl,
    /// so it is a compiler bug and not a diagnostic.
    ///
    /// Checked on *every* method-call argument in debug builds, not only the
    /// ones selection asked about: a judgement that restates typing rules the
    /// elaborator also implements drifts silently unless the whole corpus
    /// exercises it.
    pub(super) fn verify_arg_synthesis(
        &mut self,
        selected_with: &[Option<ArgClass>],
        args_ast: &[ast::Expr],
        ctx: &FunctionContext,
        args: &[TypeId],
        span: crate::token::Span,
    ) {
        // Once an error is reported the arguments carry recovery types, which
        // say nothing about synthesis.
        if !cfg!(debug_assertions) || self.logger.has_errors() {
            return;
        }
        for (index, arg_ast) in args_ast.iter().enumerate() {
            let Some(arg) = args.get(index) else { continue };
            if *arg == TypeTable::ERROR || *arg == TypeTable::UNKNOWN {
                continue;
            }
            let class = match selected_with.get(index).and_then(Clone::clone) {
                Some(class) => class,
                None => self.synthesize_arg_class(arg_ast, ctx),
            };
            assert!(
                self.class_admits(*arg, &class),
                "argument synthesis under-approximated at {span:?}: argument {} elaborated to \
                 `{}`, which the synthesized class {class:?} excludes",
                index + 1,
                self.tysys.type_table.borrow().type_name(*arg),
            );
        }
    }

    /// How an argument's class reads in a diagnostic — the reason-chain step
    /// that says what this argument contributed to selection.
    pub(super) fn describe_arg_class(&self, class: &ArgClass) -> String {
        let tt = self.tysys.type_table.borrow();
        match class {
            ArgClass::Exact(t) => format!("has type `{}`", tt.type_name(*t)),
            ArgClass::Head(head) => format!(
                "is a `{}` whose type arguments are not pinned here",
                head.to_display()
            ),
            ArgClass::IntLit => {
                "is an integer literal, which admits every numeric parameter".to_string()
            }
            ArgClass::FloatLit => {
                "is a float literal, which admits every float parameter".to_string()
            }
            ArgClass::StrLit => {
                "is a string literal, which admits `String` and its newtypes".to_string()
            }
            ArgClass::NullLit => "is `null`, which admits every `Option`".to_string(),
            ArgClass::Opaque(OpaqueReason::Closure) => {
                "is a closure, so the parameter is what would type it".to_string()
            }
            ArgClass::Opaque(OpaqueReason::CompoundLiteral) => {
                "is a compound literal, so the parameter is what would type it".to_string()
            }
            ArgClass::Opaque(OpaqueReason::Inference) => {
                "has a type that depends on inference here, so it admits every candidate"
                    .to_string()
            }
            ArgClass::Opaque(OpaqueReason::Unresolved) => {
                "did not resolve, so it admits every candidate".to_string()
            }
        }
    }

    /// The judgement. One arm per `ast::Expr` variant, no wildcard: an
    /// expression form either has a rule or names the reason it has none.
    fn synth(&mut self, expr: &ast::Expr, scope: &mut SynthScope<'_>) -> ArgClass {
        use ast::Expr;
        match expr {
            Expr::Literal(lit) => self.synth_literal(&lit.value),
            Expr::TemplateString(_) => ArgClass::StrLit,
            // A tag's result is whatever the tag returns; the callee is
            // resolved against a type the classifier does not mint.
            Expr::TaggedTemplate(_) => ArgClass::Opaque(OpaqueReason::Inference),
            Expr::Ident(id) => self.synth_ident(id, scope),
            Expr::Unary(u) => self.synth_unary(u, scope),
            Expr::Binary(b) => self.synth_binary(b, scope),
            Expr::ComparisonChain(_) | Expr::Matches(_) => ArgClass::Exact(TypeTable::BOOL),
            Expr::Assign(_) | Expr::CompoundAssign(_) => ArgClass::Exact(TypeTable::UNIT),
            Expr::Cast(c) => self.synth_type(&c.target_type),
            Expr::Call(call) => self.synth_call(call, scope),
            Expr::MethodCall(m) => self.synth_method_call(m, scope),
            // A static call's receiver head may be generic, and then the
            // expected type supplies its arguments.
            Expr::StaticMethodCall(_) => ArgClass::Opaque(OpaqueReason::Inference),
            Expr::FieldAccess(fa) => self.synth_field_access(fa, scope),
            Expr::Index(idx) => self.synth_index(idx, scope),
            Expr::Range(range) => self.synth_range(range, scope),
            Expr::StructLiteral(sl) => match &sl.name {
                Some(name) => self.synth_named_type(name),
                None => ArgClass::Opaque(OpaqueReason::CompoundLiteral),
            },
            Expr::Block(block) => self.synth_tail(&block.stmts, scope),
            // Typed by its `break label: value`s, which the tail joins only when
            // it carries a value of its own — so reading the tail alone would
            // answer `unit` for a block the breaks type.
            Expr::LabeledBlock(_) => ArgClass::Opaque(OpaqueReason::Inference),
            Expr::WithHandler(w) => self.synth_tail(&w.body.stmts, scope),
            Expr::If(if_expr) => self.synth_branches(
                &if_expr.condition,
                &if_expr.then_block,
                if_expr.else_block.as_ref(),
                scope,
            ),
            Expr::Match(m) => self.synth_match(m, scope),
            Expr::Closure(_) => ArgClass::Opaque(OpaqueReason::Closure),
            Expr::TupleLiteral(_) | Expr::Spread(_, _) => {
                ArgClass::Opaque(OpaqueReason::CompoundLiteral)
            }
            Expr::TryOp(_) | Expr::Resume(_) | Expr::TupleComprehension(_) => {
                ArgClass::Opaque(OpaqueReason::Inference)
            }
            Expr::Error(_) => ArgClass::Opaque(OpaqueReason::Unresolved),
        }
    }

    fn synth_literal(&mut self, lit: &ast::Literal) -> ArgClass {
        match lit {
            ast::Literal::Number(raw) => {
                if is_float_only_literal(raw) {
                    ArgClass::FloatLit
                } else {
                    ArgClass::IntLit
                }
            }
            ast::Literal::Byte(_) | ast::Literal::LocationLine => ArgClass::IntLit,
            ast::Literal::String(_)
            | ast::Literal::IncludeStr(_)
            | ast::Literal::LocationFile
            | ast::Literal::LocationFunction
            | ast::Literal::DataSection => ArgClass::StrLit,
            ast::Literal::Char(_) => ArgClass::Exact(TypeTable::CHAR),
            ast::Literal::Bool(_) => ArgClass::Exact(TypeTable::BOOL),
            ast::Literal::Unit => ArgClass::Exact(TypeTable::UNIT),
            ast::Literal::Null => ArgClass::NullLit,
            // A `List<u8>` whatever the expected type says — the elaborator
            // does not consult it here.
            ast::Literal::Bytes(_) | ast::Literal::IncludeBytes(_) => {
                ArgClass::Exact(self.tysys.type_table.borrow_mut().make_byte_list())
            }
        }
    }

    /// Locals, captures, globals and associated constants have a declared
    /// type. A function reference and a generic case do not: their type args
    /// come from the expected type.
    fn synth_ident(&mut self, id: &ast::IdentExpr, scope: &SynthScope<'_>) -> ArgClass {
        if scope.shadows(&id.name) {
            return ArgClass::Opaque(OpaqueReason::Inference);
        }
        // The registries below are keyed by the canonical `ns$member` alias,
        // as `resolve_ident` reads them.
        let canonical = self.sem.imports.canonical_ns_ref(&id.name);
        let name = canonical.as_deref().unwrap_or(&id.name);
        let ctx = scope.ctx;
        if !name.contains("::") {
            if let Some(local) = ctx.lookup(name) {
                return self.class_of_type(local.type_id);
            }
            if let Some((_, inner)) = ctx.deref_overrides.get(name) {
                return self.class_of_type(*inner);
            }
            if let Some(outer) = ctx.outer_locals.get(name) {
                return self.class_of_type(outer.type_id);
            }
            if let Some(&(ty, _)) = self.sem.decls.current_module_globals.get(name) {
                return self.class_of_type(ty);
            }
            if let Some((_, _, ty, _)) = self.sem.decls.imported_globals.get(name) {
                return self.class_of_type(*ty);
            }
            // A bare function name is a function *reference*, whose type the
            // expected type instantiates; anything else did not resolve.
            let is_function = self.sem.decls.function_return_types.contains_key(name)
                || self.sem.decls.imported_functions.contains(name);
            return ArgClass::Opaque(if is_function {
                OpaqueReason::Inference
            } else {
                OpaqueReason::Unresolved
            });
        }
        if let Some(AssocConstSig { ty, .. }) = self.associated_constant_of_path(id) {
            return self.class_of_type(ty);
        }
        let name = name.to_string();
        self.synth_qualified_case(id, &name)
    }

    /// `Color::Red`, `Flags::Bit`, and a payload-less case of a non-generic
    /// variant. A generic variant's type arguments come from the expected
    /// type, so it stays open.
    ///
    /// The qualifier is a path segment the resolve walk answered for, so which
    /// `Color` it names comes off that site rather than off the spelling.
    fn synth_qualified_case(&mut self, id: &ast::IdentExpr, name: &str) -> ArgClass {
        // The last `::`, so the split names the same segment `owner` does:
        // in `ns::Color::Red` that is `Color`, not the `ns` prefix.
        let Some(pos) = name.rfind("::") else {
            return ArgClass::Opaque(OpaqueReason::Unresolved);
        };
        let (prefix, suffix) = (&name[..pos], &name[pos + 2..]);
        let owner = id.segments.len().checked_sub(2).map(|i| id.segments[i].id);
        if let Some(info) = self.lookup_variant_cases_at(owner, prefix) {
            let declared_at = info.defined_at;
            let generic = !info.type_params.is_empty();
            if info.cases.iter().any(|c| c.name == suffix) {
                if generic {
                    return ArgClass::Opaque(OpaqueReason::Inference);
                }
                return self.synth_decl_type(declared_at);
            }
        }
        if let Some(info) = self.type_lookup().enum_cases_at(owner, prefix) {
            let declared_at = info.defined_at;
            if info.find_case(suffix).is_some() {
                return self.synth_decl_type(declared_at);
            }
        }
        if let Some(info) = self.lookup_flags_members_at(owner, prefix) {
            let type_id = info.type_id;
            if info.members.iter().any(|m| m.name == suffix) {
                return self.class_of_type(type_id);
            }
        }
        ArgClass::Opaque(OpaqueReason::Unresolved)
    }

    fn synth_decl_type(&self, declared_at: ast::AstId) -> ArgClass {
        match self.tysys.type_table.borrow().type_of_symbol(&declared_at) {
            Some(type_id) => self.class_of_type(type_id),
            None => ArgClass::Opaque(OpaqueReason::Unresolved),
        }
    }

    fn synth_unary(&mut self, u: &ast::UnaryExpr, scope: &mut SynthScope<'_>) -> ArgClass {
        let operand = self.synth(&u.expr, scope);
        let open = ArgClass::Opaque(OpaqueReason::Inference);
        match u.op {
            ast::UnaryOp::Ref | ast::UnaryOp::MutRef => {
                let ArgClass::Exact(t) = operand else {
                    return open;
                };
                let mut tt = self.tysys.type_table.borrow_mut();
                ArgClass::Exact(if matches!(u.op, ast::UnaryOp::Ref) {
                    tt.make_ref(t)
                } else {
                    tt.make_mut_ref(t)
                })
            }
            ast::UnaryOp::Deref => {
                let ArgClass::Exact(t) = operand else {
                    return open;
                };
                match self.tysys.type_table.borrow().get(t) {
                    ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                        ArgClass::Exact(*inner)
                    }
                    _ => open,
                }
            }
            // `!x` is bool-only: unlike `-x` / `~x` it dispatches to no trait.
            ast::UnaryOp::Not => {
                if operand == ArgClass::Exact(TypeTable::BOOL) {
                    ArgClass::Exact(TypeTable::BOOL)
                } else {
                    open
                }
            }
            ast::UnaryOp::Neg => self.synth_unary_op_output(operand, CompilerItem::Neg, "neg"),
            ast::UnaryOp::BitNot => {
                self.synth_unary_op_output(operand, CompilerItem::BitNot, "bitnot")
            }
        }
    }

    /// A primitive operand keeps its own class; anything else takes the
    /// operator impl's `Output`.
    fn synth_unary_op_output(
        &mut self,
        operand: ArgClass,
        item: CompilerItem,
        method_name: &str,
    ) -> ArgClass {
        let open = ArgClass::Opaque(OpaqueReason::Inference);
        match operand {
            ArgClass::IntLit | ArgClass::FloatLit => operand,
            ArgClass::Exact(t) => {
                if self.tysys.type_table.borrow().is_numeric(t) {
                    return ArgClass::Exact(t);
                }
                let Some(name) = self.tysys.struct_name_for_type(t) else {
                    return open;
                };
                let Some(trait_) = self.tysys.compiler_trait_def(item) else {
                    return open;
                };
                match self.find_arithmetic_trait_impl(&name, t, trait_, method_name, None) {
                    Some(info) => self.class_of_type(info.output_type),
                    None => open,
                }
            }
            ArgClass::Head(_) | ArgClass::StrLit | ArgClass::NullLit | ArgClass::Opaque(_) => open,
        }
    }

    fn synth_binary(&mut self, b: &ast::BinaryExpr, scope: &mut SynthScope<'_>) -> ArgClass {
        use ast::BinaryOp;
        match b.op {
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or => ArgClass::Exact(TypeTable::BOOL),
            // A shift's result is its left operand's type.
            BinaryOp::Shl | BinaryOp::Shr => self.synth_arith_operand(&b.left, scope),
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Mod
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor => {
                let left = self.synth_arith_operand(&b.left, scope);
                let right = self.synth_arith_operand(&b.right, scope);
                left.meet(right)
            }
        }
    }

    /// An operand of an arithmetic operator: a primitive keeps its class, a
    /// user type answers through the operator impl it dispatches to — which
    /// the meet then treats as the pinned side.
    fn synth_arith_operand(&mut self, expr: &ast::Expr, scope: &mut SynthScope<'_>) -> ArgClass {
        let class = self.synth(expr, scope);
        let open = ArgClass::Opaque(OpaqueReason::Inference);
        match class {
            // Which impl decides the result depends on both operands, not on
            // this position alone.
            ArgClass::Exact(t) if !self.tysys.type_table.borrow().is_numeric(t) => open,
            ArgClass::IntLit | ArgClass::FloatLit | ArgClass::Exact(_) => class,
            ArgClass::Head(_) | ArgClass::StrLit | ArgClass::NullLit | ArgClass::Opaque(_) => open,
        }
    }

    /// A named function's declared return type, or an indirect call through a
    /// `fn`-typed value. A generic callee's return type is open.
    fn synth_call(&mut self, call: &ast::CallExpr, scope: &mut SynthScope<'_>) -> ArgClass {
        let ast::Expr::Ident(ident) = &call.callee else {
            let ArgClass::Exact(callee) = self.synth(&call.callee, scope) else {
                return ArgClass::Opaque(OpaqueReason::Inference);
            };
            return match self.as_fn_signature(callee) {
                Some(sig) => self.class_of_type(sig.return_type),
                None => ArgClass::Opaque(OpaqueReason::Unresolved),
            };
        };
        if scope.shadows(&ident.name) {
            return ArgClass::Opaque(OpaqueReason::Inference);
        }
        // A `fn`-typed local shadows a same-named function, as in `resolve_call`.
        if !ident.name.contains("::")
            && let Some(local) = scope.ctx.lookup(&ident.name)
            && let Some(sig) = self.as_fn_signature(local.type_id)
        {
            return self.class_of_type(sig.return_type);
        }
        let Some(callee) = self.synth_callee_ref(ident) else {
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        if !self.lookup_function_type_params(&callee).is_empty() {
            return ArgClass::Opaque(OpaqueReason::Inference);
        }
        let return_type = self.lookup_function_return_type(&callee, None);
        self.class_of_type(return_type)
    }

    /// The callee identity of a plain `name(…)` call, read off its own
    /// reference site. A variant constructor, a static path or an effect
    /// operation names no function there and is left to the expected type.
    fn synth_callee_ref(&self, ident: &ast::IdentExpr) -> Option<CalleeRef> {
        if ident.name.contains("::") {
            return None;
        }
        Some(self.callee_of(self.free_function_at(ident.id)?))
    }

    fn synth_method_call(
        &mut self,
        call: &ast::MethodCallExpr,
        scope: &mut SynthScope<'_>,
    ) -> ArgClass {
        // A turbofish means the method is generic and its own arguments shape
        // the return type; the substitution is inference's job, not this one.
        if !call.type_args.is_empty() {
            return ArgClass::Opaque(OpaqueReason::Inference);
        }
        let ArgClass::Exact(receiver) = self.synth(&call.receiver, scope) else {
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        match self.method_return_type(receiver, &call.method, call.span) {
            Some(return_type) => self.class_of_type(return_type),
            None => ArgClass::Opaque(OpaqueReason::Unresolved),
        }
    }

    /// The declared return type of `receiver.method(…)`, read in the order the
    /// real dispatch uses — a concrete ref impl, the inherent method, the base
    /// type's trait impls. Another order would answer with a signature the call
    /// never selects.
    fn method_return_type(
        &mut self,
        receiver: TypeId,
        method: &str,
        span: crate::token::Span,
    ) -> Option<TypeId> {
        let base = self.tysys.get_base_type(receiver);
        let name = self.tysys.struct_name_for_type(base)?;
        let type_args = match self.tysys.type_table.borrow().get(base) {
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => Some(type_args.clone()),
            _ => None,
        };
        let ref_kind = crate::name::RefKind::from_resolved(
            &self.tysys.type_table.borrow().get(receiver).clone(),
        );
        if let Some(ref_kind) = ref_kind
            && let Some(found) = self.find_trait_method_for_type(
                &super::trait_env::ImplTargetKey::Ref(ref_kind),
                method,
                type_args.as_deref(),
                Some(base),
                span,
                None,
                None,
            )
            && !found.is_blanket_ref_impl
        {
            let info = found.method_info;
            return Some(self.return_type_for_receiver(info.return_type, info.owner, receiver));
        }
        if let Some(info) = self.lookup_method_info(receiver, method) {
            return Some(self.return_type_for_receiver(info.return_type, info.owner, receiver));
        }
        let target = self.impl_target_of(base, &crate::name::DeclName::new(&name));
        let found = self.find_trait_method_for_type(
            &target,
            method,
            type_args.as_deref(),
            Some(base),
            span,
            None,
            None,
        )?;
        let info = found.method_info;
        Some(self.return_type_for_receiver(info.return_type, info.owner, receiver))
    }

    /// A method inherited through a newtype returns the newtype, not the base
    /// its `impl` was written against. Elaboration substitutes the same way, so
    /// reading the declaration alone would under-approximate the class.
    fn return_type_for_receiver(
        &mut self,
        return_type: TypeId,
        owner: MethodOwner,
        receiver: TypeId,
    ) -> TypeId {
        let Some(base_type_id) = owner.newtype_base() else {
            return return_type;
        };
        let newtype_id = self.tysys.get_base_type(receiver);
        self.tysys
            .substitute_newtype_in_type(return_type, base_type_id, newtype_id)
    }

    fn synth_field_access(
        &mut self,
        access: &ast::FieldAccessExpr,
        scope: &mut SynthScope<'_>,
    ) -> ArgClass {
        let ArgClass::Exact(receiver) = self.synth(&access.expr, scope) else {
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        let (_, field_type) = self.lookup_field_type(receiver, &access.field, access.span);
        self.class_of_type(field_type)
    }

    fn synth_index(&mut self, index: &ast::IndexExpr, scope: &mut SynthScope<'_>) -> ArgClass {
        let ArgClass::Exact(container) = self.synth(&index.expr, scope) else {
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        let key_class = self.synth(&index.index, scope);
        let Some(key) = self.index_key_type(&key_class) else {
            // Which impl answers depends on the key's type, so an unpinned key
            // leaves the element type unknown.
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        let base = self.tysys.get_base_type(container);
        let Some(name) = self.tysys.struct_name_for_type(base) else {
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        let (lookup_name, lookup_type) = self.tysys.newtype_base_lookup(&name, base);
        let found = self
            .index_lookup_or_newtype_base(&name, base, &lookup_name, lookup_type, |s, n, t| {
                s.find_index_value_trait_impl(n, t, Some(key))
                    .map(|i| i.output_type)
                    .or_else(|| {
                        s.find_index_trait_impl(n, t, Some(key))
                            .map(|i| i.output_type)
                    })
            })
            .map(|(output, _)| output);
        match found {
            Some(output) => self.class_of_type(output),
            None => ArgClass::Opaque(OpaqueReason::Inference),
        }
    }

    /// The type a subscript key really elaborates to. Unlike an argument, an
    /// index is resolved with *no* expected type before its impl is selected
    /// (`resolve_index`), so a literal key takes its default type there — and
    /// synthesis must read the key the same way or it would answer for an impl
    /// the real walk never picks.
    pub(super) fn index_key_type(&mut self, key: &ArgClass) -> Option<TypeId> {
        match key {
            ArgClass::Exact(t) => Some(*t),
            ArgClass::IntLit => Some(TypeTable::I32),
            ArgClass::FloatLit => Some(TypeTable::F64),
            ArgClass::StrLit => Some(self.get_string_struct_type()),
            ArgClass::Head(_) | ArgClass::NullLit | ArgClass::Opaque(_) => None,
        }
    }

    /// `a..<b` is a `RangeExclusive<T>`: `T` when both endpoints pin it, the
    /// head alone otherwise — which is still enough to tell the two range
    /// types apart from a plain index.
    fn synth_range(&mut self, range: &ast::RangeExpr, scope: &mut SynthScope<'_>) -> ArgClass {
        let item = match range.kind {
            ast::RangeKind::Exclusive => crate::compiler_item::CompilerItem::RangeExclusive,
            ast::RangeKind::Inclusive => crate::compiler_item::CompilerItem::RangeInclusive,
        };
        let range_decl = self.tysys.type_table.borrow().compiler_item_def(item);
        let start = self.synth(&range.start, scope);
        let element = start.meet(self.synth(&range.end, scope));
        let Some(def) = range_decl else {
            return ArgClass::Opaque(OpaqueReason::Unresolved);
        };
        match element {
            ArgClass::Exact(t) => ArgClass::Exact(
                self.tysys
                    .type_table
                    .borrow_mut()
                    .make_generic_instance(def, vec![t]),
            ),
            _ => ArgClass::Head(FqTypeName::of_head(self.tysys.resolutions.defs(), def)),
        }
    }

    /// An `if` in either spelling: the expression form and the tail-statement
    /// form share their shape and their result rule. Without an `else` there is
    /// no second branch to agree with.
    fn synth_branches(
        &mut self,
        condition: &ast::Condition,
        then_block: &ast::Block,
        else_block: Option<&ast::Block>,
        scope: &mut SynthScope<'_>,
    ) -> ArgClass {
        let Some(else_block) = else_block else {
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        // An `if let` binds for the then-branch only.
        let bound = condition_binding_names(condition);
        let then_class = self.with_bindings(scope, bound, |s, scope| {
            s.synth_tail(&then_block.stmts, scope)
        });
        let else_class = self.synth_tail(&else_block.stmts, scope);
        self.join_classes(then_class, else_class)
    }

    fn synth_match(&mut self, m: &ast::MatchExpr, scope: &mut SynthScope<'_>) -> ArgClass {
        let mut arms = m.arms.iter();
        let Some(first) = arms.next() else {
            return ArgClass::Opaque(OpaqueReason::Inference);
        };
        let mut class = self.synth_match_arm(first, scope);
        for arm in arms {
            let next = self.synth_match_arm(arm, scope);
            class = self.join_classes(class, next);
        }
        class
    }

    fn synth_match_arm(&mut self, arm: &ast::MatchArm, scope: &mut SynthScope<'_>) -> ArgClass {
        let bound = pattern_binding_names(&arm.pattern);
        self.with_bindings(scope, bound, |s, scope| s.synth(&arm.body, scope))
    }

    /// A block's value is its trailing statement's, by the same rule
    /// [`super::control_flow::block_result_type`] reads it back with: a tail
    /// expression, a tail `if`/`else` or `match`, or nothing. A block that ends
    /// by diverging has whatever type its position demands.
    fn synth_tail(&mut self, statements: &[ast::Stmt], scope: &mut SynthScope<'_>) -> ArgClass {
        let Some((last, leading)) = statements.split_last() else {
            return ArgClass::Exact(TypeTable::UNIT);
        };
        // A local `struct` / `impl` / `type` declares a name this judgement
        // would resolve in the module's scope, where it means something else or
        // nothing.
        if leading.iter().any(|s| matches!(s, ast::Stmt::Item(_))) {
            return ArgClass::Opaque(OpaqueReason::Inference);
        }
        let bound: Vec<String> = leading
            .iter()
            .filter_map(|s| match s {
                ast::Stmt::Let(l) => Some(pattern_binding_names(&l.pattern)),
                _ => None,
            })
            .flatten()
            .collect();
        self.with_bindings(scope, bound, |s, scope| match last {
            ast::Stmt::Expr(stmt) => s.synth(&stmt.expr, scope),
            ast::Stmt::Match(m) => s.synth_match(m, scope),
            ast::Stmt::If(if_stmt) => s.synth_branches(
                &if_stmt.condition,
                &if_stmt.then_block,
                if_stmt.else_block.as_ref(),
                scope,
            ),
            ast::Stmt::Return(_)
            | ast::Stmt::TaskReturn(_)
            | ast::Stmt::Break(_)
            | ast::Stmt::Continue(_)
            | ast::Stmt::Error(_) => ArgClass::Opaque(OpaqueReason::Inference),
            ast::Stmt::Let(_)
            | ast::Stmt::While(_)
            | ast::Stmt::For(_)
            | ast::Stmt::ForOf(_)
            | ast::Stmt::Loop(_)
            | ast::Stmt::Assert(_)
            | ast::Stmt::LabeledBlock(_)
            | ast::Stmt::Item(_) => ArgClass::Exact(TypeTable::UNIT),
        })
    }

    /// Run `f` with `names` shadowed, so an identifier the argument binds for
    /// itself is not answered from the call site's scope.
    fn with_bindings<R>(
        &mut self,
        scope: &mut SynthScope<'_>,
        names: Vec<String>,
        f: impl FnOnce(&mut Self, &mut SynthScope<'_>) -> R,
    ) -> R {
        let depth = scope.shadowed.len();
        scope.shadowed.extend(names);
        let result = f(self, scope);
        scope.shadowed.truncate(depth);
        result
    }

    /// The least upper bound — what an `if` / `match` knows when its branches
    /// disagree. Widening keeps the invariant: the result's set contains both
    /// branches' sets. When neither side contains the other, two exact types
    /// sharing a head widen to that head, and everything else widens to the
    /// top.
    fn join_classes(&self, left: ArgClass, right: ArgClass) -> ArgClass {
        if self.class_contains(&left, &right) {
            return left;
        }
        if self.class_contains(&right, &left) {
            return right;
        }
        if let (ArgClass::Exact(a), ArgClass::Exact(b)) = (&left, &right) {
            let tt = self.tysys.type_table.borrow();
            let head = tt.fq_base_type_name(*a);
            if head == tt.fq_base_type_name(*b) {
                return ArgClass::Head(head);
            }
        }
        ArgClass::Opaque(OpaqueReason::Inference)
    }

    /// Whether `outer`'s denoted set contains `inner`'s — the ordering the
    /// join widens along.
    fn class_contains(&self, outer: &ArgClass, inner: &ArgClass) -> bool {
        if outer == inner {
            return true;
        }
        match (outer, inner) {
            (ArgClass::Opaque(_), _) => true,
            // `class_admits` waves through a parameter that still mentions a
            // type parameter, which is right for admission and wrong here:
            // containment must not be claimed for a type that is not yet one.
            (_, ArgClass::Exact(t)) => {
                !self.tysys.type_table.borrow().contains_type_param(*t)
                    && self.class_admits(*t, outer)
            }
            // An integer literal admits the float types too, so its set is the
            // wider of the two.
            (ArgClass::IntLit, ArgClass::FloatLit) => true,
            _ => false,
        }
    }

    /// A resolved type's class: exact when closed, its head when only the head
    /// is known, open when neither. The single funnel every rule returns
    /// through, so "`Exact` means closed" holds in one place.
    ///
    /// A type carrying an unsolved inference hole is *not* closed even though
    /// it names a constructor: `let Some(b) = opt` binds `b` at `?` until the
    /// argument position pins it.
    fn class_of_type(&self, type_id: TypeId) -> ArgClass {
        if type_id == TypeTable::UNKNOWN || type_id == TypeTable::ERROR {
            return ArgClass::Opaque(OpaqueReason::Unresolved);
        }
        let open = self.tysys.type_table.borrow().contains_type_param(type_id)
            || self.type_has_infer_hole(type_id);
        if !open {
            return ArgClass::Exact(type_id);
        }
        let tt = self.tysys.type_table.borrow();
        match tt.get(type_id) {
            ResolvedType::GenericInstance { .. } | ResolvedType::GenericResource { .. } => {
                ArgClass::Head(tt.fq_base_type_name(type_id))
            }
            _ => ArgClass::Opaque(OpaqueReason::Inference),
        }
    }

    /// The class of a written type — a cast target, which is the documented
    /// escape from an ambiguous call, so it must handle generic spellings.
    fn synth_type(&mut self, ty: &ast::Type) -> ArgClass {
        let resolved = self.resolve_type(ty);
        self.class_of_type(resolved)
    }

    /// A named struct literal's type: exact for a non-generic struct the table
    /// has interned, its head for a generic one — `Pair { … }` elaborates to
    /// `Pair<i32, i64>`, and the declaration's own `TypeId` is not that type.
    /// A name that is not a known type resolves to nothing, so it claims no
    /// head either.
    fn synth_named_type(&self, name: &str) -> ArgClass {
        if let Some(primitive) = TypeTable::primitive_by_name(name) {
            return ArgClass::Exact(primitive);
        }
        let Some(def) = self.decl_key_or_local(name) else {
            return ArgClass::Opaque(OpaqueReason::Unresolved);
        };
        let generic = self
            .lookup_struct_fields_of_decl(def)
            .is_some_and(|info| !info.type_param_type_ids.is_empty());
        if generic {
            return ArgClass::Head(FqTypeName::of_head(self.tysys.resolutions.defs(), def));
        }
        let found = self
            .tysys
            .type_table
            .borrow()
            .find_struct_type(crate::tir::StructDef::Decl(def));
        if let Some(type_id) = found {
            return self.class_of_type(type_id);
        }
        if self
            .tysys
            .is_known_type_name(self.tysys.resolutions.defs().name(def))
        {
            return ArgClass::Head(FqTypeName::of_head(self.tysys.resolutions.defs(), def));
        }
        ArgClass::Opaque(OpaqueReason::Unresolved)
    }
}
