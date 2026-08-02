//! Method call and static method call resolution.

use super::trait_env::ImplTargetKey;
use crate::ast::{self, AstId, Item};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, Receiver, RefKind};
use crate::tir::{
    FunctionRef, MonomorphInfo, ResolvedType, SubstitutionContext, TirExpr, TirExprKind, TypeId,
    TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::callee::StaticMethodRef;
use super::method_lookup::MethodInferenceInput;
use super::reflect::ScalarReflectSpec;
use super::types::{FunctionContext, MethodInfo, MethodOwner, TypeError};

/// A static call named the way [symbol notation] writes it — the receiver's
/// type arguments included (`List<i32>::with_capacity`). Rendering only the
/// head collapsed `Take<A>::take` and `Take<B>::take` onto one string, so a
/// diagnostic naming both could not tell them apart.
///
/// [symbol notation]: ../../../docs/wep-2026-06-14-symbol-notation.md
fn static_call_symbol_name(static_call: &ast::StaticMethodCallExpr) -> String {
    let mut name = String::new();
    crate::unparse::unparse_type_into(&static_call.target_type, &mut name);
    name.push_str("::");
    name.push_str(&static_call.method);
    name
}

/// Inputs to [`Elaborator::resolve_method_call_with`], the TIR-level method-call
/// dispatcher. The AST-driven [`Elaborator::resolve_method_call`] is a thin
/// wrapper that resolves the receiver / type args / args from the
/// [`ast::MethodCallExpr`] and forwards here; sites that synthesise method
/// calls without a backing AST node (e.g. the for-of loop's `into_iter()` /
/// `next()` dispatches) call this directly with their already-resolved
/// receiver and an empty `args` slice.
///
/// `method_id == None` signals a synthetic call: no use→def edge is
/// recorded against any AST node for the method name token. This is what
/// keeps internal helper calls out of LSP jump-to-definition.
///
/// `call_id == None` likewise suppresses recording the dispatch decision
/// in [`super::sem::TypeAnnotations::method_dispatch`]: the future
/// `reify` pass (Stage 5 of WEP 2026-05-26) only walks source-level
/// `MethodCallExpr` nodes, so a synthesised call has no AST id under which
/// to file an entry.
pub(super) struct MethodCallInput<'a> {
    pub receiver: TirExpr,
    /// The receiver's source AST when the call comes from user syntax.
    /// `resolve_ident` leaves placeholder TIR at annotate time, so the
    /// `&mut self` receiver-mutability check walks this instead. `None`
    /// for synthetic dispatches (for-of desugaring), whose receivers are
    /// compiler-owned locals.
    pub receiver_ast: Option<&'a ast::Expr>,
    pub method_name: &'a str,
    pub method_id: Option<AstId>,
    pub call_id: Option<AstId>,
    pub type_args: Vec<TypeId>,
    /// Per-position `_` mask for `type_args` (see `call::turbofish_holes`).
    /// Empty when the caller supplied no `_` placeholders (synthetic callers
    /// and fully-explicit turbofish), which leaves inference untriggered.
    pub type_arg_holes: Vec<bool>,
    pub args: &'a [ast::Expr],
    pub expected_type: Option<TypeId>,
    pub span: Span,
    /// The trait a qualified call named (`Alpha::describe(&x)`), constraining
    /// which impl may be picked — the escape hatch for a method name two
    /// traits share (WEP 2026-07-31). `None` for an ordinary `x.m()`, whose
    /// candidates span every trait implemented for the receiver.
    pub required_trait: Option<super::types::RequiredTrait>,
}

/// Result of [`Elaborator::resolve_method_call_with`]: the typed
/// placeholder plus, on successful dispatch, the receiver-adjustment
/// inputs and resolved target a synthetic caller (for-of's `into_iter()`
/// / `next()`, whose `call_id == None` skips `record_method_dispatch`)
/// needs to record the decision its own way. `None` when a short-circuit
/// path returned early or method lookup failed.
pub(super) struct MethodCallOutcome {
    pub expr: TirExpr,
    pub dispatch: Option<(ast::SelfKind, bool, FunctionRef)>,
    /// The resolved signature, for a caller that suppressed
    /// `record_method_dispatch` with `call_id: None` and files its own record.
    /// The qualified-call path files a *static* dispatch, which needs the same
    /// facts: without them its arguments lose their defaults, their `is_mut`
    /// shape, and the expected types an unannotated closure argument infers
    /// from.
    pub signature: Option<MethodSignatureFacts>,
}

pub(super) struct MethodSignatureFacts {
    pub param_is_mut: Vec<bool>,
    pub param_names: Vec<String>,
    pub param_defaults: Vec<Option<ast::Expr>>,
    pub param_types: Vec<TypeId>,
    pub self_kind: ast::SelfKind,
}

impl MethodCallOutcome {
    fn no_dispatch(expr: TirExpr) -> Self {
        Self {
            expr,
            dispatch: None,
            signature: None,
        }
    }
}

use super::util::placeholder;

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Check for IndexMut desugaring: container[i].method() where method needs &mut self
        // We need to detect this BEFORE resolving the receiver, because resolve_index
        // would otherwise generate Index::index instead of IndexMut::index_mut
        if let ast::Expr::Index(index_expr) = &method_call.receiver
            && let Some(result) =
                self.try_resolve_index_mut_method_call(index_expr, method_call, ctx)
        {
            return result.type_id;
        }

        let receiver = placeholder(
            self.resolve_expr(&method_call.receiver, ctx, None),
            method_call.receiver.span(),
        );

        // A `_` resolves to UNKNOWN here; its position is recorded in the hole
        // mask below so the dispatch fills it from inference.
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        // Build the mask only for the `_` case; an empty vec (no allocation)
        // marks "no holes" for the fully-explicit common path.
        let type_arg_holes = if super::call::turbofish_has_hole(&method_call.type_args) {
            super::call::turbofish_holes(&method_call.type_args)
        } else {
            Vec::new()
        };

        self.resolve_method_call_with(
            MethodCallInput {
                receiver,
                receiver_ast: Some(&method_call.receiver),
                method_name: &method_call.method,
                method_id: Some(method_call.method_id),
                call_id: Some(method_call.id),
                type_args,
                type_arg_holes,
                args: &method_call.args,
                expected_type,
                span: method_call.span,
                required_trait: None,
            },
            ctx,
        )
        .expr
        .type_id
    }

    /// Dispatch a method call from an already-resolved receiver TIR. See
    /// [`MethodCallInput`] for the contract.
    pub(super) fn resolve_method_call_with(
        &mut self,
        input: MethodCallInput<'_>,
        ctx: &mut FunctionContext,
    ) -> MethodCallOutcome {
        let MethodCallInput {
            mut receiver,
            receiver_ast,
            method_name,
            method_id,
            call_id,
            type_args,
            type_arg_holes,
            args: args_ast,
            expected_type,
            span,
            required_trait,
        } = input;
        // A qualified call names one trait, so the inherent-method step — a
        // different namespace — is skipped; only that trait's impls may
        // answer. The ref-impl priority step still runs (with the filter):
        // it is trait-impl lookup too, and skipping it would send
        // `IntoIterator::into_iter(&list)` to the base type's impl where
        // `(&list).into_iter()` selects `impl IntoIterator for &List<T>`.
        let required_trait = required_trait.as_ref();
        // NOTE: args are resolved later (after method lookup) to enable literal coercion
        // using the method's parameter types as expected types.

        // Shallow per-argument classes for argument-directed selection among
        // one trait's argument lists (WEP 2026-07-31 phase 3). Computed before
        // lookup — cheap, side-effect-free — because selection must run before
        // any candidate's signature shapes the arguments.
        let probe_classes: Vec<super::method_lookup::ProbeClass> = args_ast
            .iter()
            .map(|a| self.probe_arg_class(a, ctx))
            .collect();

        // Base (non-ref) type for method lookup. `mut`: deferred-inference may
        // concretise the receiver below.
        let mut base_type_id = self.tysys.get_base_type(receiver.type_id);

        // Get struct name and module source from base type
        // The struct_module is where the struct is defined (and inherent methods live)
        let (struct_name, struct_module) = match self.tysys.type_table.borrow().get(base_type_id) {
            ResolvedType::Struct {
                decl_name: name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            // Primitive types have impl blocks in core:prelude/primitive
            ResolvedType::Primitive(_) => (
                self.tysys
                    .type_table
                    .borrow()
                    .mangle_type_name(base_type_id),
                ModuleSource::primitive(),
            ),
            // Unit type () has impl blocks in core:prelude/primitive
            ResolvedType::Unit => (
                TypeTable::UNIT_TYPE_NAME.to_string(),
                ModuleSource::primitive(),
            ),
            // Enum types - use enum name and its defining module
            ResolvedType::Enum {
                name,
                module_source,
            } => (name.clone(), module_source.clone()),
            // Generic resource types (Future<T>, Stream<T>, etc.) - use resource name and module
            ResolvedType::GenericResource {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            // Newtype/Flags - use the type's own name and defining module
            ResolvedType::Newtype {
                name,
                module_source,
                ..
            }
            | ResolvedType::Flags {
                name,
                module_source,
            } => (name.clone(), module_source.clone()),
            // Raw GC array `Array<T>`: inherent methods live in
            // `impl Array<T>` (core:prelude/array.wado), keyed by "Array".
            ResolvedType::BuiltinArray(_) => (
                TypeTable::ARRAY_TYPE_NAME.to_string(),
                ModuleSource::array(),
            ),
            _ => (
                self.tysys
                    .type_table
                    .borrow()
                    .mangle_type_name(base_type_id),
                self.current_module_source.clone(),
            ),
        };

        // Extract receiver type args for generic types (used for resolving associated types)
        let type_args_source_id = {
            let tt = self.tysys.type_table.borrow();
            if matches!(tt.get(base_type_id), ResolvedType::Newtype { .. }) {
                tt.get_ultimate_base_type(base_type_id)
            } else {
                base_type_id
            }
        };
        let receiver_type_args_for_trait: Option<Vec<TypeId>> = match self
            .tysys
            .type_table
            .borrow()
            .get(type_args_source_id)
            .clone()
        {
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. }
                if !type_args.is_empty() =>
            {
                Some(type_args)
            }
            // The raw GC array `Array<T>` carries its element as a single
            // type arg, so a trait method's associated types (e.g.
            // `IntoIterator::Iter` / `Item` for `impl IntoIterator for
            // Array<T>`) resolve against `[elem]` just like a generic
            // container's.
            ResolvedType::BuiltinArray(elem) => Some(vec![elem]),
            _ => None,
        };

        let mut method_info: Option<MethodInfo> = None;
        let mut trait_name: Option<String> = None;
        let mut trait_impl_module_source: Option<ModuleSource> = None;
        let mut blanket_type_param: Option<String> = None;
        let mut trait_impl_struct_name: Option<FqTypeName> = None;
        let mut matched_impl_struct_name: Option<String> = None;
        // `Some` when the ref-priority path below adopts a `&T` / `&mut T` impl,
        // so `base_struct_name` (then `"&"` / `"&mut"`) keys back to its typed
        // `Receiver::Ref` without re-inspecting the string.
        let mut matched_ref_kind: Option<RefKind> = None;

        // If receiver is a reference type, try ref-type trait impls first.
        // e.g., impl IntoIterator for &List<T> takes priority over impl IntoIterator for List<T>.
        // Only specific ref impls are preferred (not blanket impls like impl Inspect for &T).
        {
            let is_ref = matches!(
                self.tysys.type_table.borrow().get(receiver.type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            );
            if is_ref {
                let ref_kind = RefKind::from_resolved(
                    &self.tysys.type_table.borrow().get(receiver.type_id).clone(),
                )
                .expect("ref classify");
                let result = self.find_trait_method_for_type(
                    &ImplTargetKey::Ref(ref_kind),
                    method_name,
                    &struct_module,
                    receiver_type_args_for_trait.as_deref(),
                    Some(base_type_id),
                    span,
                    required_trait,
                    Some(&probe_classes),
                );
                // Only use ref-type impls that target a concrete container type
                // (e.g., impl IntoIterator for &List<T>), NOT blanket ref impls
                // (e.g., impl Inspect for &T where the inner type is just a type param).
                if let Some(trait_match) = result
                    && !trait_match.is_blanket_ref_impl
                {
                    matched_impl_struct_name = Some(trait_match.impl_struct_name.clone());
                    trait_impl_struct_name = Some(trait_match.impl_struct_fq);
                    matched_ref_kind = Some(ref_kind);
                    trait_name = Some(trait_match.trait_name);
                    let mut info = trait_match.method_info;
                    info.is_ref_impl = true;
                    method_info = Some(info);
                    trait_impl_module_source = Some(trait_match.impl_module_source);
                    blanket_type_param = trait_match.blanket_type_param;
                }
            }
        }

        // Look up method info based on receiver type (inherent + base type trait methods)
        if method_info.is_none() && required_trait.is_none() {
            method_info = self.lookup_method_info(receiver.type_id, method_name);
        }

        // Fall back to base type trait methods
        if method_info.is_none()
            && let Some(trait_match) = self.find_trait_method_for_type(
                &self.impl_target_of(base_type_id, &crate::name::DeclName::new(&struct_name)),
                method_name,
                &struct_module,
                receiver_type_args_for_trait.as_deref(),
                Some(base_type_id),
                span,
                required_trait,
                Some(&probe_classes),
            )
        {
            matched_impl_struct_name = Some(trait_match.impl_struct_name.clone());
            if trait_match.impl_struct_name != struct_name {
                trait_impl_struct_name = Some(trait_match.impl_struct_fq);
            }
            trait_name = Some(trait_match.trait_name);
            method_info = Some(trait_match.method_info);
            trait_impl_module_source = Some(trait_match.impl_module_source);
            blanket_type_param = trait_match.blanket_type_param;
        }

        // If still not found and receiver is a TypeParam, try trait bounds
        // e.g., T: Ord -> look up cmp() in Ord trait declaration
        if method_info.is_none() {
            let type_param_name = {
                let resolved = self.tysys.type_table.borrow().get(base_type_id).clone();
                if let ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } =
                    resolved
                {
                    Some(name)
                } else {
                    None
                }
            };
            if let Some(name) = type_param_name
                && let Some(bounds) = self
                    .annotate_ctx
                    .trait_ctx
                    .type_param_bounds
                    .get(&name)
                    .cloned()
                && let Some((found_trait, info)) = {
                    // A qualified call names one bound, so the others are not
                    // competitors — without this filter the collision it exists
                    // to resolve is still reported inside a generic body, and
                    // the first bound answers regardless of which was named.
                    // Bounds are compared as declarations, so a same-named
                    // trait from another module does not answer for the one
                    // the call named.
                    let bound_names: Vec<String> = bounds
                        .iter()
                        .map(|b| b.name.clone())
                        .filter(|n| {
                            required_trait.is_none_or(|w| self.trait_decl_key_in_frame(n) == w.decl)
                        })
                        .collect();
                    self.find_method_in_trait_bounds(&bound_names, method_name, base_type_id, span)
                }
            {
                trait_name = Some(found_trait);
                method_info = Some(info);
            }
        }

        // If still not found and receiver is an AssocTypeProjection, try its bounds
        // e.g., S::SeqSerializer: SerializeSeq -> look up element() in SerializeSeq
        if method_info.is_none() {
            let assoc_bounds = {
                let resolved = self.tysys.type_table.borrow().get(base_type_id).clone();
                if let ResolvedType::AssocTypeProjection { bounds, .. } = resolved {
                    if bounds.is_empty() {
                        None
                    } else {
                        Some(bounds)
                    }
                } else {
                    None
                }
            };
            if let Some(bounds) = assoc_bounds
                && let Some((found_trait, info)) = {
                    let bounds: Vec<String> = bounds
                        .into_iter()
                        .filter(|n| {
                            required_trait.is_none_or(|w| self.trait_decl_key_in_frame(n) == w.decl)
                        })
                        .collect();
                    self.find_method_in_trait_bounds(&bounds, method_name, base_type_id, span)
                }
            {
                trait_name = Some(found_trait);
                method_info = Some(info);
            }
        }

        // Get method info (error if method not found)
        // Track whether the lookup actually found a real method. The
        // error-recovery branch below fabricates a placeholder MethodInfo
        // so resolution can continue past a diagnostic, but the
        // FunctionRef we then build mangles a non-existent method
        // against the receiver's struct module — we MUST NOT record that
        // as a successful dispatch in `sem.types.method_dispatch`, or
        // Stage 5 reify would try to lower a call to a function that
        // does not exist.
        let method_found = method_info.is_some();
        let MethodInfo {
            impl_offset: sig_impl_offset,
            method_ast_id: dispatched_method_ast_id,
            mut return_type,
            self_kind,
            param_types,
            param_is_mut: _,
            owner,
            cm_name,
            is_ref_impl,
            method_type_param_ids: _,
            impl_module: inherent_impl_module,
            from_concrete_impl,
            param_defaults,
            param_names,
            consumes_self,
        } = if let Some(info) = method_info {
            info
        } else {
            let type_name = self.tysys.type_table.borrow().type_name(base_type_id);
            let _ = self.emit(TypeError::MethodNotFound {
                type_name,
                method_name: method_name.to_string(),
                hint: String::new(),
                span,
            });
            // Default to Unknown type for error recovery
            MethodInfo {
                impl_offset: None,
                method_ast_id: None,
                return_type: TypeTable::UNKNOWN,
                self_kind: ast::SelfKind::Ref,
                param_types: vec![],
                param_is_mut: vec![],
                owner: MethodOwner::Receiver,
                cm_name: None,
                is_ref_impl: false,
                method_type_param_ids: vec![],
                impl_module: None,
                from_concrete_impl: false,
                param_defaults: vec![],
                param_names: vec![],
                consumes_self: false,
            }
        };

        // Tuple.len() is a compile-time constant — return immediately without a function call.
        if method_name == "len" && self.tysys.type_table.borrow().is_tuple(base_type_id) {
            // A tuple whose type still contains a `..T` pack has an arity that is
            // only known after monomorphization. Defer folding to a literal so it
            // is not frozen at the (wrong) unsubstituted pack count.
            if self.type_contains_pack(base_type_id) {
                return MethodCallOutcome::no_dispatch(TirExpr::new(
                    TirExprKind::TupleLen {
                        expr: Box::new(receiver),
                    },
                    TypeTable::I32,
                    span,
                ));
            }
            let len = self
                .tysys
                .type_table
                .borrow()
                .as_tuple(base_type_id)
                .unwrap()
                .len() as i64;
            return MethodCallOutcome::no_dispatch(TirExpr::new(
                TirExprKind::IntLiteral {
                    value: len as u64,
                    repr: len.to_string(),
                },
                TypeTable::I32,
                span,
            ));
        }

        // Tuple.zip() transposes a tuple-of-tuples.
        // [[A0, A1], [B0, B1]].zip() → [[A0, B0], [A1, B1]]
        if method_name == "zip" && self.tysys.type_table.borrow().is_tuple(base_type_id) {
            let has_type_pack = self.type_contains_pack(base_type_id);
            if has_type_pack {
                // TypePack present: defer expansion to monomorphization.
                return MethodCallOutcome::no_dispatch(TirExpr::new(
                    TirExprKind::TupleZip {
                        expr: Box::new(receiver),
                    },
                    return_type,
                    span,
                ));
            }
            // Concrete tuples: expand inline now.
            let outer_elems = self
                .tysys
                .type_table
                .borrow()
                .as_tuple(base_type_id)
                .unwrap();
            let inner_arities: Vec<Vec<TypeId>> = outer_elems
                .iter()
                .map(|e| self.tysys.type_table.borrow().as_tuple(*e).unwrap())
                .collect();
            let arity = inner_arities[0].len();
            let num_rows = outer_elems.len();
            let mut col_exprs = Vec::with_capacity(arity);
            for col in 0..arity {
                let mut row_exprs = Vec::with_capacity(num_rows);
                for (row, row_types) in inner_arities.iter().enumerate() {
                    let row_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(receiver.clone()),
                            field_index: row as u32,
                            field_name: row.to_string(),
                        },
                        outer_elems[row],
                        span,
                    );
                    let cell = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(row_access),
                            field_index: col as u32,
                            field_name: col.to_string(),
                        },
                        row_types[col],
                        span,
                    );
                    row_exprs.push(cell);
                }
                let col_types: Vec<TypeId> = inner_arities.iter().map(|row| row[col]).collect();
                let col_tuple_type = self.tysys.type_table.borrow_mut().make_tuple(col_types);
                col_exprs.push(TirExpr::new(
                    TirExprKind::TupleLiteral {
                        elements: row_exprs,
                    },
                    col_tuple_type,
                    span,
                ));
            }
            return MethodCallOutcome::no_dispatch(TirExpr::new(
                TirExprKind::TupleLiteral {
                    elements: col_exprs,
                },
                return_type,
                span,
            ));
        }

        // Static methods (no self parameter) cannot be called with instance method syntax.
        // e.g., `obj.static_method()` should be `Type::static_method()` instead.
        if self_kind == ast::SelfKind::None {
            let type_name = self.tysys.type_table.borrow().type_name(base_type_id);
            let _ = self.emit(TypeError::MethodNotFound {
                type_name: type_name.clone(),
                method_name: method_name.to_string(),
                hint: format!(
                    "'{method_name}' is a static method; use {type_name}::{method_name}() instead"
                ),
                span,
            });
            return MethodCallOutcome::no_dispatch(TirExpr::new(
                TirExprKind::Unit,
                TypeTable::ERROR,
                span,
            ));
        }

        // Type check method arguments against expected parameter types (newtype-aware)
        // If method was inherited from a newtype's base type, substitute base->newtype in params
        let expected_param_types: Vec<TypeId> = if let Some(base_type_id) = owner.inherited() {
            // Get the newtype that the method is being called on
            let newtype_id = self.tysys.get_base_type(receiver.type_id);
            // Substitute base type with newtype in all parameter types
            param_types
                .iter()
                .map(|&ty| {
                    self.tysys
                        .substitute_newtype_in_type(ty, base_type_id, newtype_id)
                })
                .collect()
        } else {
            param_types
        };

        // Resolve arguments with coercion using method parameter types
        let mut args: Vec<TirExpr> = args_ast
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = expected_param_types.get(i).copied();
                placeholder(self.resolve_expr(arg, ctx, expected_type), arg.span())
            })
            .collect();

        // Pad missing trailing args with declared parameter defaults.
        // Earlier-parameter references inside a default (e.g. `fn f(w, h = w)`)
        // are handled by substituting the caller's arg ASTs for those parameter
        // names before resolving, mirroring the free-function path in
        // `pad_args_with_defaults`.
        if args.len() < expected_param_types.len() && !param_defaults.is_empty() {
            let mut subs: crate::hashmap::IndexMap<String, ast::Expr> =
                crate::hashmap::IndexMap::default();
            for (i, arg_ast) in args_ast.iter().enumerate() {
                if let Some(name) = param_names.get(i) {
                    subs.insert(name.clone(), arg_ast.clone());
                }
            }
            for i in args.len()..expected_param_types.len() {
                let Some(Some(default_ast)) = param_defaults.get(i) else {
                    break;
                };
                let expected_type = expected_param_types[i];
                let mut default_expr = default_ast.clone();
                default_expr.substitute_idents(&subs);
                let resolved = self.resolve_expr(&default_expr, ctx, Some(expected_type));
                args.push(placeholder(resolved, default_expr.span()));
                if let Some(name) = param_names.get(i) {
                    subs.insert(name.clone(), default_expr);
                }
            }
        }

        // Check each argument against expected parameter type
        for (i, (arg, &expected_type)) in
            args.iter_mut().zip(expected_param_types.iter()).enumerate()
        {
            // Pin a deferred hole that rode a prior binding into this argument
            // (`let v = gen()?; out.push(v)`) against the parameter type.
            if self.type_has_infer_hole(arg.type_id) && self.hole_pinnable_against(expected_type) {
                self.solve_infer_holes_against(arg.type_id, expected_type);
                arg.type_id = self.apply_infer_holes(arg.type_id);
            }
            let arg_span = args_ast.get(i).map_or(span, super::ast::Expr::span);
            self.typecheck(arg.type_id, expected_type, arg_span);
        }

        // Substitute return type for inherited newtype methods
        // e.g., Point::clone_point() -> Point becomes Location::clone_point() -> Location
        if let Some(base_type_id) = owner.inherited() {
            let newtype_id = self.tysys.get_base_type(receiver.type_id);
            return_type =
                self.tysys
                    .substitute_newtype_in_type(return_type, base_type_id, newtype_id);
        }

        // Address-taken tracking for an implicit `&mut self` borrow on a
        // primitive local receiver is owned by reify (`reify.rs` method-call
        // arm marks `address_taken_locals` on the TIR it emits); the combined
        // walk no longer computes it now that `resolve_ident` returns a
        // placeholder.

        if self_kind == ast::SelfKind::MutRef && !is_ref_impl {
            self.check_mut_receiver(&receiver, receiver_ast, method_name, span, ctx);
        }

        // Adjust receiver based on what the method expects (self_kind)
        receiver = self.adjust_receiver_for_self_kind(receiver, self_kind, is_ref_impl, span);

        // Build unified substitution context for double generics
        // Type param indices are assigned as follows:
        // - Impl type params (from struct): 0, 1, 2, ...
        // - Method type params: offset, offset+1, ... (where offset = impl_type_params.len())
        let mut subst_ctx = SubstitutionContext::new();
        let mut impl_offset = 0u32;

        // A concrete-instantiation impl (`impl List<u8>`) registers no impl type
        // params and resolves its `self`/param types to the concrete
        // instantiation, so the receiver's type args are NOT substitution
        // params. Method-level type params therefore start at index 0; mapping
        // the receiver args here would clash with them (e.g. bind a method `T`
        // at index 0 to the receiver's `u8`).
        if from_concrete_impl {
            // impl_offset stays 0; method type params occupy 0.. .
        } else if trait_name.is_none() {
            // First, add impl-level type args from receiver's generic type (use base type)
            // IMPORTANT: Skip this for trait methods because find_trait_method_for_type already
            // resolved the return type using associated type bindings. Adding impl_args here would
            // incorrectly substitute TypeParams from the OUTER context (e.g., TreeMap's K, V) that
            // happen to have the same indices as this impl's type params (e.g., List's T).
            match self.tysys.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance {
                    type_args: receiver_type_args,
                    ..
                }
                | ResolvedType::GenericResource {
                    type_args: receiver_type_args,
                    ..
                } if !receiver_type_args.is_empty() => {
                    impl_offset = receiver_type_args.len() as u32;
                    subst_ctx = subst_ctx.with_impl_args(&receiver_type_args);
                }
                // The raw GC array `Array<T>` carries a single impl-level type
                // arg (its element type), exactly like `Container<T>`. Without
                // this, an inherent `impl Array<T>` method's return type keeps
                // its `T` unsubstituted (e.g. `fn first(&self) -> T` or
                // `fn slice(&self) -> Slice<T>`), so the caller sees a bare
                // `T`. Stdlib impls dodge this only because they resolve via the
                // loaded-module path in `lookup_method_info`, which substitutes
                // during type resolution; a user-defined `impl Array<T>` is
                // registered locally and needs the substitution here.
                ResolvedType::BuiltinArray(elem) => {
                    impl_offset = 1;
                    subst_ctx = subst_ctx.with_impl_args(&[elem]);
                }
                _ => {}
            }
        } else {
            // For trait methods, just compute impl_offset for method type args
            match self.tysys.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance { type_args, .. }
                | ResolvedType::GenericResource { type_args, .. }
                    if !type_args.is_empty() =>
                {
                    impl_offset = type_args.len() as u32;
                }
                ResolvedType::BuiltinArray(_) => {
                    impl_offset = 1;
                }
                _ => {}
            }
        }

        // The digest's numbering wins: the derivations above count receiver
        // arguments, which overshoots when one of them is concrete.
        if let Some(offset) = sig_impl_offset {
            impl_offset = offset;
        }

        // Inference runs when the turbofish is omitted entirely or carries an
        // explicit `_` placeholder; in the latter case the inferred holes are
        // merged into the explicit args, which always win.
        let has_hole = type_arg_holes.iter().any(|&h| h);
        let (method_type_args, reuse_params) = if type_args.is_empty() || has_hole {
            let inferred = self.infer_method_type_args(MethodInferenceInput {
                receiver_type: receiver.type_id,
                method_name,
                impl_offset,
                param_types: &expected_param_types,
                args: &args,
                raw_args: args_ast,
                decl_return_type: return_type,
                expected_return_type: expected_type,
                trait_name: trait_name.as_deref(),
                span,
            });
            if type_args.is_empty() {
                (inferred.type_args, inferred.bound_check_params)
            } else {
                let mut merged = type_args;
                super::call::merge_turbofish_type_args(
                    &mut merged,
                    &type_arg_holes,
                    &inferred.type_args,
                );
                (merged, inferred.bound_check_params)
            }
        } else {
            (type_args, None)
        };

        if !method_type_args.is_empty() {
            subst_ctx = subst_ctx.with_method_args(&method_type_args, impl_offset);
            // Enforce the method's type-arg bounds (shared rule); a violating
            // concrete arg would otherwise trap WIR build. Hole args are skipped
            // and re-checked in `finalize_infer_holes`. Reuse the params
            // `infer_method_type_args` already looked up; the explicit-turbofish
            // path (no inference) falls back to a fresh lookup.
            match reuse_params {
                Some(params) => self.enforce_type_arg_bounds(&params, &method_type_args, span),
                None => self.check_method_type_arg_bounds(
                    &struct_name,
                    &struct_module,
                    method_name,
                    trait_name.as_deref(),
                    &method_type_args,
                    span,
                ),
            }
        }

        // Apply unified substitution
        if !subst_ctx.is_empty() {
            return_type =
                subst_ctx.substitute(return_type, &mut self.tysys.type_table.borrow_mut());
        }

        // Deferred-inference solve point: a hole that flowed in from an
        // uninferred generic receiver (`p.get()` in `p.get().unwrap()`) is
        // solved against this call's expected type and concretised *before* the
        // mangling/recording below embeds the receiver type in a name a later
        // TypeId sweep could not fix.
        if let Some(expected) = expected_type
            && (self.type_has_infer_hole(return_type) || self.type_has_infer_hole(receiver.type_id))
        {
            self.solve_infer_holes_against(return_type, expected);
            receiver.type_id = self.apply_infer_holes(receiver.type_id);
            return_type = self.apply_infer_holes(return_type);
            base_type_id = self.tysys.get_base_type(receiver.type_id);
        }
        // A hole may still ride the receiver (a deep chain's intermediate call,
        // `gen().keep().unwrap()`): the recorded name embeds `Type<?hole>`, but
        // the monomorphizer rebuilds names from the receiver type, which the
        // module-end sweep concretises once the hole is solved further out.

        // Re-coerce literal-number args and typecheck each arg against the substituted
        // parameter type. This catches inference conflicts such as
        // `h.two_method<T>(1 as i64, 2 as i32)` where `T` cannot be both `i64` and `i32`.
        // The pre-inference typecheck at line ~380 only sees TypeParam (a wildcard),
        // so the conflict must be caught after substitution.
        if !method_type_args.is_empty() {
            let substituted_param_types: Vec<TypeId> = expected_param_types
                .iter()
                .map(|&t| subst_ctx.substitute(t, &mut self.tysys.type_table.borrow_mut()))
                .collect();
            self.recoerce_literal_args(args_ast, &mut args, &substituted_param_types);
            for (i, arg) in args.iter().enumerate() {
                if let Some(&expected) = substituted_param_types.get(i) {
                    self.typecheck(
                        arg.type_id,
                        expected,
                        args_ast.get(i).map_or(span, super::ast::Expr::span),
                    );
                }
            }
        }

        // Get struct name and monomorph info from base type for mangled method name.
        // For inherited methods (Newtype/Flags), use the actual implementation type's name,
        // since the function is defined on the base type (e.g., Point::sum, not Location::sum).
        let method_impl_type_id = owner.declaring(base_type_id);
        let (
            mut receiver_struct_name,
            mut base_struct_name,
            impl_type_arg_names,
            receiver_type_args,
        ) = match self
            .tysys
            .type_table
            .borrow()
            .get(method_impl_type_id)
            .clone()
        {
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source,
            }
            | ResolvedType::GenericResource {
                name,
                type_args,
                module_source,
            } => {
                // Qualify the base and the arguments alike, so a concrete-generic
                // impl's method name matches its definition (issue #1348). A
                // tuple carries the tuple head, not a declared one, so it keeps
                // the `[a,b]` spelling every other namespace gives it.
                let type_arg_names: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                    .collect();
                let base = if TypeTable::is_tuple_type(&name) {
                    FqTypeName::tuple(Vec::new())
                } else {
                    FqTypeName::declared(&module_source, &name)
                };
                let mangled = base.clone().with_args(type_arg_names.clone());
                (mangled, base, type_arg_names, Some(type_args))
            }
            // The raw GC array splits like a generic instance: the receiver
            // name is the full `Array<T>` spelling, but the method-owner base
            // name is "Array" (matching `impl Array<T>`'s registration).
            ResolvedType::BuiltinArray(elem) => {
                let arg_name = self.tysys.type_table.borrow().fq_type_name(elem);
                let base = self.qualified_receiver_name(TypeTable::ARRAY_TYPE_NAME);
                let mangled = base.clone().with_args(vec![arg_name.clone()]);
                (mangled, base, vec![arg_name], Some(vec![elem]))
            }
            // Named by its declaring module: a bare head names no definition,
            // and re-resolution would peel past the impl to the base.
            ResolvedType::Newtype {
                name,
                module_source,
                ..
            } if matched_impl_struct_name.as_deref() == Some(name.as_str()) => {
                let base = FqTypeName::declared(&module_source, &name);
                (base.clone(), base, vec![], None)
            }
            // A generic newtype's stored `name` is the display form, baking
            // arguments into the head (`MyArray<i32>`); split it there.
            ResolvedType::Newtype {
                name,
                module_source,
                ..
            } if name.contains('<') => {
                let type_args = {
                    let tt = self.tysys.type_table.borrow();
                    let ultimate = tt.get_ultimate_base_type(method_impl_type_id);
                    tt.generic_type_args(ultimate).unwrap_or_default()
                };
                let head =
                    FqTypeName::declared(&module_source, crate::name::split_base_name(&name));
                let type_arg_names: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                    .collect();
                let mangled = head.clone().with_args(type_arg_names.clone());
                (mangled, head, type_arg_names, Some(type_args))
            }
            _ => {
                let name = self
                    .tysys
                    .type_table
                    .borrow()
                    .fq_type_name(method_impl_type_id);
                let head = name.head_only();
                (name, head, vec![], None)
            }
        };

        // For trait methods found through the newtype chain, override with the actual impl struct.
        // E.g., `loc.describe()` where `loc: Location`, `impl Describable for Point` →
        // use "Point" so the call resolves to "Point^Describable::describe".
        if let Some(impl_name) = trait_impl_struct_name {
            receiver_struct_name.clone_from(&impl_name);
            base_struct_name = impl_name;
        }

        let mangled_method_name =
            MethodName::format_local(&receiver_struct_name, trait_name.as_deref(), method_name);

        // Build monomorph_info for method calls on generic types or with method type args
        let monomorph_info = if from_concrete_impl {
            // A method from a concrete instantiation impl (`impl List<u8>`) is a
            // per-instantiation concrete function (`List<u8>::method`), not an
            // impl-level template. If the method has NO type params of its own,
            // it is fully concrete — call it directly (no monomorph_info) so
            // cross-module inclusion / DCE / WIR resolution handle it and
            // distinct instantiations stay distinct. If it DOES have method-level
            // type params (`impl List<u8> { fn f<T>() }`), it still needs
            // monomorphization over those — keyed by the per-instantiation
            // template name with NO impl type args (the receiver is concrete).
            if method_type_args.is_empty() {
                None
            } else {
                let generic_name = MethodName::format_local(
                    &receiver_struct_name,
                    trait_name.as_deref(),
                    method_name,
                );
                Some(MonomorphInfo {
                    generic_name,
                    impl_type_args: vec![],
                    method_type_args: method_type_args.clone(),
                    is_blanket: false,
                })
            }
        } else if let Some(ref blanket_param) = blanket_type_param {
            // For blanket impls, the template function uses the type param name (e.g., "I").
            // The call site uses the concrete receiver (e.g., "ListIter<i32>").
            // monomorph_info maps from the concrete name back to the template.
            let generic_name = MethodName::format_local(
                &FqTypeName::binder(blanket_param),
                trait_name.as_deref(),
                method_name,
            );
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: vec![base_type_id],
                method_type_args: vec![],
                is_blanket: true,
            })
        } else if receiver_type_args.is_some() || !method_type_args.is_empty() {
            let generic_name = MethodName::format_local(&base_struct_name, None, method_name);
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: receiver_type_args.unwrap_or_default(),
                method_type_args: method_type_args.clone(),
                is_blanket: false,
            })
        } else {
            None
        };

        // Convert method type args to string names for method_info
        // Use inferred type args if available, otherwise use explicit type args
        let method_type_arg_names: Vec<String> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().mangle_type_name(*t))
            .collect();

        // Build method_info with base struct name, then apply impl and method type args
        let is_type_param_receiver = matches!(
            self.tysys.type_table.borrow().get(base_type_id),
            ResolvedType::TypeParam { .. }
                | ResolvedType::TypePack { .. }
                | ResolvedType::AssocTypeProjection { .. }
        );
        let base_receiver = match matched_ref_kind {
            Some(kind) => Receiver::Ref(kind),
            None => Receiver::Type(base_struct_name.clone()),
        };
        let base_target = match matched_ref_kind {
            Some(kind) => ImplTargetKey::Ref(kind),
            // `impl_target_of` falls back on a written name, so hand it the
            // declaration name rather than the module-qualified head.
            None => self.impl_target_of(base_type_id, &base_struct_name.decl_name()),
        };
        let param_is_mut = self.lookup_method_param_is_mut(&base_target, method_name);
        let mut method_info =
            LocalMethodName::of(base_receiver, trait_name, method_name.to_string())
                .with_type_args(&impl_type_arg_names, &method_type_arg_names);
        method_info.is_type_param_receiver = is_type_param_receiver;
        method_info.is_ref_impl = is_ref_impl;
        method_info.cm_name = cm_name;

        // `module_source` is the body's home module. The body lives:
        //   1. In the trait-impl block's module for cross-module trait impls
        //      (e.g. `impl Display for String` in `core:prelude/format`).
        //   2. In the *base* type's module when the method was inherited
        //      through a newtype (`type MyArray<T> = List<T>`; `arr.len()`
        //      reaches `List::len` in `core:prelude/array`, not the
        //      newtype's module).
        //   3. In the receiver type's module otherwise — inherent methods
        //      live alongside the type they're declared on.
        let method_module_source = trait_impl_module_source
            // Inherent methods: the body lives in the module that declares the
            // `impl` block, which may differ from the receiver type's module
            // (a user-written `impl List<u8>` on the prelude `List`). Prefer
            // that so cross-module inherent impls resolve.
            .or_else(|| inherent_impl_module.clone())
            .or_else(|| {
                owner.inherited().and_then(|base_id| {
                    match self.tysys.type_table.borrow().get(base_id) {
                        ResolvedType::Struct { module_source, .. }
                        | ResolvedType::GenericInstance { module_source, .. }
                        | ResolvedType::Enum { module_source, .. }
                        | ResolvedType::Variant { module_source, .. }
                        | ResolvedType::Newtype { module_source, .. }
                        | ResolvedType::Flags { module_source, .. }
                        | ResolvedType::GenericResource { module_source, .. } => {
                            Some(module_source.clone())
                        }
                        ResolvedType::Primitive(_) | ResolvedType::Unit => {
                            Some(ModuleSource::primitive())
                        }
                        ResolvedType::BuiltinArray(_) => Some(ModuleSource::array()),
                        _ => None,
                    }
                })
            })
            .unwrap_or_else(|| struct_module.clone());

        // Record use->def for jump-to-definition on the method name token.
        // Synthetic call sites (e.g. for-of's `.into_iter()` / `.next()`) pass
        // `method_id == None` so no edge is recorded — the call has no
        // source-level method name to navigate from.
        // The target is the declaration dispatch selected, carried on its
        // signature. A name scan cannot stand in: two impls on one type can
        // declare the same method, and only dispatch knows which answered.
        if let (Some(method_id), Some(def_id)) = (method_id, dispatched_method_ast_id) {
            self.record_reference_to_def(method_id, def_id);
        }

        let func = FunctionRef {
            module_source: method_module_source,
            name: mangled_method_name,
            monomorph_info,
            method_info: Some(method_info),
        };

        // Stage 4 of WEP 2026-05-26: record the dispatch decision so the
        // future `reify` pass can emit the same `MethodCall` TIR without
        // re-running trait lookup / method-name mangling. Skipped when:
        //  - `call_id == None` (synthetic call: for-of's `.into_iter()`
        //    / `.next()`),
        //  - The early-returning short-circuits above (tuple `.len()` /
        //    `.zip()`, static-method-as-instance error) returned before
        //    reaching here, or
        //  - Method lookup failed and we are in the error-recovery
        //    placeholder path (`method_found == false`).
        // Only the trait-qualified caller reads the signature facts back
        // (`required_trait` is its marker); ordinary method calls skip the
        // four vector clones, incl. deep default-expression ASTs.
        let signature = (method_found && required_trait.is_some()).then(|| MethodSignatureFacts {
            param_is_mut: param_is_mut.clone(),
            param_names: param_names.clone(),
            param_defaults: param_defaults.clone(),
            param_types: expected_param_types.clone(),
            self_kind,
        });
        let dispatch = if method_found {
            self.record_method_dispatch(
                call_id,
                &func,
                self_kind,
                is_ref_impl,
                param_is_mut,
                param_names,
                param_defaults,
                return_type,
                method_type_args,
                consumes_self,
            );
            // The `method_found` gate keeps the error-recovery placeholder
            // from leaking into the returned dispatch.
            Some((self_kind, is_ref_impl, func))
        } else {
            None
        };

        // Reify rebuilds the `MethodCall` TIR from the recorded dispatch;
        // the walk projects only the result type. `receiver` and `args`
        // were resolved above for their fact-recording side effects.
        MethodCallOutcome {
            expr: placeholder(return_type, span),
            dispatch,
            signature,
        }
    }

    /// Whether `Trait::method` names a trait's instance method, making a call
    /// on it the trait-qualified (UFCS) form `Trait::method(recv, args…)`
    /// (WEP 2026-07-31). A trait's *static* method is not included: it has no
    /// receiver argument to bind `Self` from.
    pub(super) fn is_trait_instance_method(&self, trait_name: &str, method_name: &str) -> bool {
        let declared = self.declared_trait_name(trait_name);
        self.tysys
            .trait_env
            .find_trait_decl_key(&declared)
            .is_some()
            && self
                .trait_sig_by_name(&declared)
                .and_then(|sig| sig.method(method_name))
                .is_some_and(|m| m.sig.self_kind != ast::SelfKind::None)
    }

    /// `Trait::method(recv, args…)` — the receiver is the first argument, so
    /// dispatch is the ordinary method-call path with the named trait as a
    /// constraint on which impl may answer.
    pub(super) fn resolve_trait_qualified_call(
        &mut self,
        trait_name: &str,
        method_name: &str,
        call: &ast::CallExpr,
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let required = super::types::RequiredTrait {
            decl: self.trait_decl_key_in_frame(trait_name),
            args: None,
            display: self.declared_trait_name(trait_name),
        };
        let type_args: Vec<TypeId> = call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        // The method name is the second segment of `Trait::method`; the edge
        // for jump-to-definition is recorded against it.
        let method_id = match &call.callee {
            ast::Expr::Ident(ident) => ident.segments.get(1).map(|seg| seg.id),
            _ => None,
        };
        self.resolve_trait_qualified_call_parts(
            required,
            method_name,
            &call.args,
            type_args,
            call.id,
            method_id,
            call.span,
            expected_type,
            ctx,
        )
    }

    /// The shared engine behind both qualified spellings: the bare
    /// `Trait::method(recv, …)` ident form, and the trait-turbofish
    /// `Take::<A>::take(recv, …)` static form whose `required_trait` carries
    /// the resolved trait arguments and thereby pins one argument list.
    #[allow(clippy::too_many_arguments)]
    fn resolve_trait_qualified_call_parts(
        &mut self,
        required_trait: super::types::RequiredTrait,
        method_name: &str,
        args: &[ast::Expr],
        type_args: Vec<TypeId>,
        call_id: AstId,
        // The method-name token, for the use→def edge.
        method_id: Option<AstId>,
        span: Span,
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let Some((receiver_ast, rest)) = args.split_first() else {
            let _ = self.emit(TypeError::TraitQualifiedCallNeedsReceiver {
                trait_name: required_trait.display,
                method: method_name.to_string(),
                span,
            });
            return TypeTable::ERROR;
        };
        let trait_display = required_trait.display.clone();
        let receiver_type = self.resolve_expr(receiver_ast, ctx, None);
        // `call_id: None` — the dispatcher would file the decision under
        // `method_dispatch`, which reify only reads for a `MethodCallExpr`
        // node. A qualified call spells its receiver's mode itself (`&x` for
        // `&self`), so no receiver adjustment is owed and the call is an
        // ordinary one whose first argument happens to be the receiver: the
        // decision is recorded as a static dispatch, which reify's `Call` arm
        // already replays.
        let outcome = self.resolve_method_call_with(
            MethodCallInput {
                receiver: placeholder(receiver_type, receiver_ast.span()),
                receiver_ast: Some(receiver_ast),
                method_name,
                method_id,
                call_id: None,
                type_args: type_args.clone(),
                type_arg_holes: vec![],
                args: rest,
                expected_type,
                span,
                required_trait: Some(required_trait),
            },
            ctx,
        );
        if let Some(sig) = &outcome.signature {
            self.check_trait_qualified_receiver_mode(
                &trait_display,
                method_name,
                sig.self_kind,
                receiver_type,
                receiver_ast.span(),
            );
        }
        if let (Some((_, _, function_ref)), Some(sig)) = (outcome.dispatch, outcome.signature) {
            // The receiver occupies slot 0 of the static shape, so every
            // per-parameter list gains a leading entry for it. It is spelled at
            // the call site and never omitted, hence no default; it is `mut`
            // exactly when the method takes `&mut self`.
            let mut param_is_mut = vec![sig.self_kind == ast::SelfKind::MutRef];
            param_is_mut.extend(sig.param_is_mut);
            let mut param_defaults: Vec<(String, Option<ast::Expr>)> =
                vec![("self".to_string(), None)];
            param_defaults.extend(sig.param_names.into_iter().zip(sig.param_defaults));
            let mut param_types = vec![receiver_type];
            param_types.extend(sig.param_types);
            // An unannotated closure argument infers its parameter types from
            // this; without it the closure's functor is generated with
            // `unknown` params and dropped before codegen.
            self.record_call_param_types(call_id, param_types);
            self.sem.types.static_method_dispatch.insert(
                call_id,
                super::sem::types::StaticMethodDispatch {
                    function_ref,
                    param_is_mut,
                    type_args,
                    param_defaults,
                    self_in_args: true,
                },
            );
        }
        outcome.expr.type_id
    }

    /// The receiver of a qualified call spells its own mode (WEP 2026-07-31);
    /// enforce that the spelling agrees with the method's `self` parameter.
    /// Without this, a by-value receiver against `&mut self` mutates a copy
    /// and silently drops the change. A `&mut` receiver still answers a
    /// `&self` method — the one reference coercion the language has.
    fn check_trait_qualified_receiver_mode(
        &mut self,
        trait_name: &str,
        method: &str,
        self_kind: ast::SelfKind,
        receiver_type: TypeId,
        span: Span,
    ) {
        if receiver_type == TypeTable::ERROR || receiver_type == TypeTable::UNKNOWN {
            return;
        }
        let resolved = self.tysys.type_table.borrow().get(receiver_type).clone();
        let is_ref = matches!(resolved, ResolvedType::Ref(_));
        let is_mut_ref = matches!(resolved, ResolvedType::MutRef(_));
        let (expected, spelled) = match self_kind {
            ast::SelfKind::Value if is_ref || is_mut_ref => ("self", "value"),
            ast::SelfKind::Ref if !(is_ref || is_mut_ref) => ("&self", "&value"),
            ast::SelfKind::MutRef if !is_mut_ref => ("&mut self", "&mut value"),
            _ => return,
        };
        let _ = self.emit(TypeError::TraitQualifiedReceiverMode {
            trait_name: trait_name.to_string(),
            method: method.to_string(),
            expected: expected.to_string(),
            spelled: spelled.to_string(),
            span,
        });
    }

    /// Resolve a static method call: `List::<i32>::with_capacity(100)` or `Point::origin()`
    pub(super) fn resolve_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        // ReflectStruct trait-qualified static call: `ReflectStruct::<T>::members()` /
        // `type_name()`. `ReflectStruct` is a (sealed) trait, not a type, so
        // `target_type` would not resolve — intercept and route to the
        // concrete `T`'s synthesized `T^ReflectStruct::method`. This is the only
        // spelling for ReflectStruct metadata; a bare `T::members()` never
        // resolves, so struct namespaces stay clean.
        if let ast::Type::Generic(g) = &static_call.target_type
            && self.is_reflect_trait_call(&g.name, &static_call.method)
            && let Some(self_ty_ast) = g.args.first()
        {
            let self_ty = self.resolve_type(self_ty_ast);
            return self.resolve_reflect_static_call(self_ty, static_call, ctx);
        }

        // `ReflectVariant::<T>::…` — the variant analog of the interception
        // above (WEP 2026-06-13 §3d).
        if let ast::Type::Generic(g) = &static_call.target_type
            && self.is_reflect_variant_trait_call(&g.name, &static_call.method)
            && let Some(self_ty_ast) = g.args.first()
        {
            let self_ty = self.resolve_type(self_ty_ast);
            return self.resolve_reflect_variant_static_call(self_ty, static_call, ctx);
        }

        // `ReflectEnum::<T>::…` / `ReflectFlags::<T>::…` — the scalar-kind
        // analogs (WEP 2026-06-13 §3b / §3c), sharing one resolver.
        if let ast::Type::Generic(g) = &static_call.target_type {
            for spec in [ScalarReflectSpec::ENUM, ScalarReflectSpec::FLAGS] {
                if self.is_reflect_scalar_trait_call(spec, &g.name, &static_call.method)
                    && let Some(self_ty_ast) = g.args.first()
                {
                    let self_ty = self.resolve_type(self_ty_ast);
                    return self.resolve_reflect_scalar_static_call(
                        spec,
                        self_ty,
                        static_call,
                        ctx,
                    );
                }
            }
        }

        // Resolve the target type first to get struct name for parameter type lookup
        let target_type_id = self.resolve_type(&static_call.target_type);

        // `Tag::<Point>::tag()` where `Tag` is a trait resolves to no type;
        // unreported it types `unknown` and lowering builds an invalid module.
        if target_type_id == TypeTable::UNKNOWN
            && let ast::Type::Generic(g) = &static_call.target_type
            && self.tysys.trait_env.find_trait_decl_key(&g.name).is_some()
        {
            // `Take::<A>::take(recv, …)` — the trait-turbofish qualified call
            // (WEP 2026-07-31): the turbofish pins one argument list by the
            // *types* its arguments resolve to, so an aliased spelling still
            // names the impl written under the original name; the head
            // resolves past any `use … as` alias to its declaration. Gated on
            // the turbofish matching the trait's declared arity: on a
            // zero-parameter trait the turbofish cannot be trait arguments
            // (`Shape::<Sq>::area` writes the receiver — a pre-existing
            // misuse), so that shape keeps its unknown-function error.
            if self.is_trait_instance_method(&g.name, &static_call.method)
                && self
                    .find_trait_decl_type_params(&self.declared_trait_name(&g.name))
                    .is_some_and(|params| !params.is_empty() && params.len() == g.args.len())
            {
                let declared_head = self.declared_trait_name(&g.name);
                let trait_args: Vec<TypeId> = g.args.iter().map(|a| self.resolve_type(a)).collect();
                let args_spelled: Vec<String> =
                    g.args.iter().map(|a| self.get_type_name_full(a)).collect();
                let required = super::types::RequiredTrait {
                    decl: self.trait_decl_key_in_frame(&g.name),
                    args: Some(trait_args),
                    display: format!("{declared_head}<{}>", args_spelled.join(", ")),
                };
                let method_type_args: Vec<TypeId> = static_call
                    .type_args
                    .iter()
                    .map(|ty| self.resolve_type(ty))
                    .collect();
                return self.resolve_trait_qualified_call_parts(
                    required,
                    &static_call.method.clone(),
                    &static_call.args,
                    method_type_args,
                    static_call.id,
                    Some(static_call.method_id),
                    static_call.span,
                    None,
                    ctx,
                );
            }
            let _ = self.emit(TypeError::UnknownFunction {
                name: static_call_symbol_name(static_call),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        // Extract struct name AND canonical decl key for parameter type
        // lookup (follow newtypes to base). The canonical key disambiguates
        // two modules' same-named structs whose static methods both live in
        // the global `StaticMethodIndex`.
        let (struct_name_for_lookup, struct_key_for_lookup) =
            self.static_receiver_struct_key(target_type_id);

        // Look up parameter types for coercion. Thread the canonical
        // receiver key (from the resolved target type) so that two
        // modules' same-named structs each route to their own impl.
        let mut param_types = struct_name_for_lookup
            .as_ref()
            .map(|name| {
                self.lookup_static_method_param_types_keyed(
                    name,
                    &static_call.method,
                    struct_key_for_lookup.as_ref(),
                )
            })
            .unwrap_or_default();

        // Literal preselect for a conversion call (WEP 2026-07-31 phase 4):
        // choose the impl before the argument is elaborated, so the expected
        // type comes from the selected impl instead of whichever the
        // name-keyed index returns first — the circular ordering this WEP
        // diagnoses. The name hint below then finds the same impl.
        if static_call.args.len() == 1
            && let Some(recv_name) = struct_name_for_lookup.clone()
            && self.try_conversion_preselect(
                &recv_name,
                &static_call.method,
                &static_call.args[0],
                static_call.span,
                ctx,
                &mut param_types,
            )
        {
            return TypeTable::ERROR;
        }

        // Looked up once, reused for arg padding and the recorded dispatch fact.
        let static_method_defaults: Vec<(String, Option<ast::Expr>)> = struct_name_for_lookup
            .as_ref()
            .map(|name| {
                self.lookup_static_method_param_defaults_keyed(
                    name,
                    &static_call.method,
                    struct_key_for_lookup.as_ref(),
                )
            })
            .unwrap_or_default();

        // For generic variant constructors (e.g., Option::<List<u8>>::Some([])),
        // compute substituted payload type so literal coercion works on first resolve.
        if param_types.is_empty() {
            let generic_data = {
                let resolved = self.tysys.type_table.borrow().get(target_type_id).clone();
                if let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args: instance_type_args,
                } = resolved
                {
                    Some((name, module_source, instance_type_args))
                } else {
                    None
                }
            };
            if let Some((name, module_source, instance_type_args)) = generic_data
                && let Some(variant_info) =
                    self.lookup_variant_case_in(&name, &module_source).cloned()
                && let Some((_, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
            {
                let payload_is_unit = matches!(
                    self.tysys.type_table.borrow().get(case_data.payload),
                    ResolvedType::Unit
                );
                if !payload_is_unit {
                    let mut payload_type = case_data.payload;
                    if !instance_type_args.is_empty() {
                        payload_type =
                            self.substitute_type_params(payload_type, &instance_type_args);
                    }
                    param_types.push(payload_type);
                }
            }
        }

        // Resolve method-level type arguments
        let method_type_args: Vec<TypeId> = static_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Not folded into `lookup_static_method_param_types`: variant
        // constructors need its answer to stay empty.
        {
            let has_type_args = matches!(&static_call.target_type, ast::Type::Generic(_))
                || !method_type_args.is_empty();
            if has_type_args
                && !param_types.is_empty()
                && let Some(name) = struct_name_for_lookup.as_deref()
                && let Some(sig) = self.static_method_sig(name, &static_call.method)
            {
                let declaring_args: Vec<TypeId> = match &static_call.target_type {
                    ast::Type::Generic(g) => g.args.iter().map(|t| self.resolve_type(t)).collect(),
                    _ => vec![],
                };
                let instantiated = sig.instantiate_call(
                    &self.tysys.type_table,
                    &declaring_args,
                    &method_type_args,
                );
                let first_value = sig.first_value_param().min(instantiated.param_types.len());
                for (param_type, &instantiated_type) in param_types
                    .iter_mut()
                    .zip(&instantiated.param_types[first_value..])
                {
                    *param_type = instantiated_type;
                }
            }
        }

        // Resolve arguments with expected types for coercion
        let mut args: Vec<TirExpr> = static_call
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let expected_type = param_types.get(i).copied();
                placeholder(self.resolve_expr(a, ctx, expected_type), a.span())
            })
            .collect();

        // Pad omitted trailing arguments with declared parameter defaults.
        // Variant / flags constructors carry no defaults, so the arg-count
        // checks below are unaffected.
        if args.len() < param_types.len() && !static_method_defaults.is_empty() {
            let defaults = &static_method_defaults;
            let mut subs: crate::hashmap::IndexMap<String, ast::Expr> =
                crate::hashmap::IndexMap::default();
            for (i, arg_ast) in static_call.args.iter().enumerate() {
                if let Some((pname, _)) = defaults.get(i) {
                    subs.insert(pname.clone(), arg_ast.clone());
                }
            }
            for i in args.len()..param_types.len() {
                let Some((pname, Some(default_ast))) = defaults.get(i) else {
                    break;
                };
                let expected_type = param_types[i];
                let mut default_expr = default_ast.clone();
                default_expr.substitute_idents(&subs);
                let resolved = self.resolve_expr(&default_expr, ctx, Some(expected_type));
                args.push(placeholder(resolved, default_expr.span()));
                subs.insert(pname.clone(), default_expr);
            }
        }

        // Option::Some and Option::None are handled by the generic variant
        // construction path below (line ~686). No special case needed.

        // Handle flags type static methods: none() and all()
        {
            let flags_name = match self.tysys.type_table.borrow().get(target_type_id).clone() {
                ResolvedType::Flags { ref name, .. } => Some(name.clone()),
                _ => None,
            };
            if let Some(ref name) = flags_name
                && let Some(flags_info) = self.lookup_flags_case(name).cloned()
            {
                match static_call.method.as_str() {
                    "none" => {
                        if !args.is_empty() {
                            let _ = self.emit(TypeError::ArgumentCountMismatch {
                                expected: 0,
                                found: args.len(),
                                span: static_call.span,
                            });
                            return TypeTable::ERROR;
                        }
                        return flags_info.type_id;
                    }
                    "all" => {
                        if !args.is_empty() {
                            let _ = self.emit(TypeError::ArgumentCountMismatch {
                                expected: 0,
                                found: args.len(),
                                span: static_call.span,
                            });
                            return TypeTable::ERROR;
                        }
                        return flags_info.type_id;
                    }
                    _ => {}
                }
            }
        }

        // Handle custom variant construction: Shape::Circle(5.0) or MyVariant::Unit
        if let ResolvedType::Variant {
            name,
            module_source,
        } = self.tysys.type_table.borrow().get(target_type_id).clone()
        {
            // Look up the variant case info
            if let Some(variant_info) = self.lookup_variant_case_in(&name, &module_source) {
                // Find the case by name
                if let Some((_case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                {
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.tysys.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        let _ = self.emit(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TypeTable::ERROR;
                    }

                    // Stage 7-B: reify rebuilds the `VariantConstruct` from
                    // the AST + variant info; the combined walk projects only
                    // the result type.
                    return target_type_id;
                }
                // If no matching case, fall through to general method lookup
                // (e.g., trait methods like `AppError::from(e)`)
            }
        }

        // Handle generic variant construction: Result::<i32, String>::Ok(42)
        let generic_name = {
            let tt = self.tysys.type_table.borrow();
            if let ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } = tt.get(target_type_id)
            {
                Some((name.clone(), module_source.clone()))
            } else {
                None
            }
        };
        if let Some((name, module_source)) = generic_name {
            // Check if the base type is a variant
            if let Some(variant_info) = self.lookup_variant_case_in(&name, &module_source).cloned()
            {
                // This is a generic variant like Result<T, E>
                // Find the case by name
                if let Some((_case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                    .map(|(i, c)| (i, c.clone()))
                {
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.tysys.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        let _ = self.emit(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TypeTable::ERROR;
                    }

                    // Refine `_` placeholders in the turbofish (`Result::<_,
                    // MyErr>::Ok(7)`): infer the hole slots from the payload
                    // while the explicit args stay pinned. Without holes the
                    // explicitly-resolved `target_type_id` is already complete.
                    let has_target_hole = matches!(
                        &static_call.target_type,
                        ast::Type::Generic(g) if super::call::turbofish_has_hole(&g.args)
                    );
                    let result_type = if has_target_hole {
                        let target_holes = match &static_call.target_type {
                            ast::Type::Generic(g) => super::call::turbofish_holes(&g.args),
                            _ => Vec::new(),
                        };
                        let explicit_args = match self.tysys.type_table.borrow().get(target_type_id)
                        {
                            ResolvedType::GenericInstance { type_args, .. } => type_args.clone(),
                            _ => Vec::new(),
                        };
                        {
                            let inferred = self.tysys.infer_variant_type_args(
                                &self.annotate_ctx,
                                &name,
                                &variant_info,
                                &case_data,
                                args.first(),
                                None,
                                &explicit_args,
                                &target_holes,
                            );
                            self.defer_uninferable_variant(
                                inferred,
                                &name,
                                &variant_info,
                                static_call.span,
                            )
                        }
                    } else {
                        target_type_id
                    };

                    // Check payload type against the variant case's payload
                    // type, substituted with the (possibly refined) type args.
                    if !args.is_empty() {
                        let result_args = match self.tysys.type_table.borrow().get(result_type) {
                            ResolvedType::GenericInstance { type_args, .. } => {
                                Some(type_args.clone())
                            }
                            _ => None,
                        };
                        let expected_payload = match result_args {
                            Some(args_vec) => {
                                Some(self.substitute_type_params(case_data.payload, &args_vec))
                            }
                            None => param_types.first().copied(),
                        };
                        if let Some(expected_type) = expected_payload {
                            let span = static_call
                                .args
                                .first()
                                .map_or(static_call.span, super::ast::Expr::span);
                            self.typecheck(args[0].type_id, expected_type, span);
                        }
                    }

                    // Stage 7-B: reify rebuilds the `VariantConstruct` from
                    // the AST + variant info; the combined walk projects only
                    // the result type. The payload was already resolved (and
                    // typechecked) above for its fact-recording side effects.
                    return result_type;
                }
                // If no matching case, fall through to general method lookup
                // (e.g., trait methods like `Result::<T, E>::from(e)`)
            }
        }

        // Handle From<T>::from calls resolved via bodyless `impl From<T> for Type;`
        // The synthesized function doesn't exist during resolution, so we generate the call inline.
        if static_call.method == "from"
            && args.len() == 1
            && self.has_from_synthesis_request(&static_call.target_type, &args[0].type_id)
        {
            return self
                .resolve_from_call(
                    target_type_id,
                    args[0].type_id,
                    args.into_iter().next().unwrap(),
                    static_call.span,
                    static_call.id,
                )
                .type_id;
        }

        // Reflexive identity: From<T> for T — return the value unchanged.
        if static_call.method == "from" && args.len() == 1 && args[0].type_id == target_type_id {
            return args.into_iter().next().unwrap().type_id;
        }

        // Newtype From conversions: From<Base> for Newtype and From<Newtype> for Base.
        // Newtypes share the same representation as their base type, so this is a Cast.
        if static_call.method == "from" && args.len() == 1 {
            let arg_type = args[0].type_id;
            let base_of_target = self
                .tysys
                .type_table
                .borrow()
                .get_newtype_base(target_type_id);
            let base_of_arg = self.tysys.type_table.borrow().get_newtype_base(arg_type);
            if base_of_target == Some(arg_type) || base_of_arg == Some(target_type_id) {
                // Stage 7-B: reify rebuilds the newtype `Cast`; the combined
                // walk projects only the result type.
                return target_type_id;
            }
        }

        let (struct_name, struct_module, mangled_struct_name, struct_type_args) = match self
            .tysys
            .type_table
            .borrow()
            .get(target_type_id)
        {
            ResolvedType::Struct {
                decl_name: name,
                module_source,
                ..
            }
            | ResolvedType::Resource {
                name,
                module_source,
            } => (
                name.clone(),
                module_source.clone(),
                FqTypeName::declared(module_source, name),
                vec![],
            ),
            // Generic resource types (Future<T>, Stream<T>, etc.) - handle like generic structs
            // for static method resolution: use the base name and type args for substitution.
            ResolvedType::GenericResource {
                name,
                module_source,
                type_args,
            } => {
                let type_arg_names: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                    .collect();
                let mangled = FqTypeName::declared(module_source, name).with_args(type_arg_names);
                (
                    name.clone(),
                    module_source.clone(),
                    mangled,
                    type_args.clone(),
                )
            }
            ResolvedType::Primitive(prim) => (
                prim.as_str().to_string(),
                ModuleSource::primitive(),
                FqTypeName::builtin(prim.as_str()),
                vec![],
            ),
            ResolvedType::BuiltinArray(elem) => {
                let elem = *elem;
                let arg = self.tysys.type_table.borrow().fq_type_name(elem);
                (
                    TypeTable::ARRAY_TYPE_NAME.to_string(),
                    ModuleSource::array(),
                    FqTypeName::builtin(TypeTable::ARRAY_TYPE_NAME).with_args(vec![arg]),
                    vec![elem],
                )
            }
            ResolvedType::Enum {
                name,
                module_source,
            }
            | ResolvedType::Variant {
                name,
                module_source,
            } => (
                name.clone(),
                module_source.clone(),
                FqTypeName::declared(module_source, name),
                vec![],
            ),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                let args: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                    .collect();
                (
                    name.clone(),
                    module_source.clone(),
                    FqTypeName::declared(module_source, name).with_args(args),
                    type_args.clone(),
                )
            }
            ResolvedType::Newtype {
                name,
                module_source,
                base_type,
            } => {
                // First try the newtype's own name (for methods defined via `impl NewtypeName`)
                let newtype_name = name.clone();
                let newtype_module = module_source.clone();

                // Check if the newtype itself has the static method
                if self.has_static_method_direct(&newtype_name, &static_call.method) {
                    let fq = FqTypeName::declared(&newtype_module, &newtype_name);
                    (newtype_name, newtype_module, fq, vec![])
                } else {
                    // Fall back to the base type for inherited methods
                    match self.tysys.type_table.borrow().get(*base_type).clone() {
                        ResolvedType::Struct {
                            decl_name: name,
                            module_source,
                            ..
                        } => {
                            let fq = FqTypeName::declared(&module_source, &name);
                            (name, module_source, fq, vec![])
                        }
                        ResolvedType::GenericInstance {
                            name,
                            module_source,
                            type_args,
                        } => {
                            let args: Vec<FqTypeName> = type_args
                                .iter()
                                .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                                .collect();
                            let fq = FqTypeName::declared(&module_source, &name).with_args(args);
                            (name, module_source, fq, type_args)
                        }
                        ResolvedType::Newtype {
                            base_type: inner_base,
                            ..
                        } => {
                            let mut current = inner_base;
                            loop {
                                match self.tysys.type_table.borrow().get(current).clone() {
                                    ResolvedType::Struct {
                                        decl_name: name,
                                        module_source,
                                        ..
                                    } => {
                                        let fq = FqTypeName::declared(&module_source, &name);
                                        break (name, module_source, fq, vec![]);
                                    }
                                    ResolvedType::Newtype {
                                        base_type: next, ..
                                    } => current = next,
                                    _ => {
                                        let fq =
                                            FqTypeName::declared(&newtype_module, &newtype_name);
                                        break (newtype_name, newtype_module, fq, vec![]);
                                    }
                                }
                            }
                        }
                        ResolvedType::Primitive(prim) => (
                            prim.as_str().to_string(),
                            ModuleSource::primitive(),
                            FqTypeName::builtin(prim.as_str()),
                            vec![],
                        ),
                        _ => {
                            let fq = FqTypeName::declared(&newtype_module, &newtype_name);
                            (newtype_name, newtype_module, fq, vec![])
                        }
                    }
                }
            }
            ResolvedType::Flags {
                name,
                module_source,
            } => {
                // First try the flags' own name, then fall back to u32
                let flags_name = name.clone();
                let flags_module = module_source.clone();
                if self.has_static_method_direct(&flags_name, &static_call.method) {
                    let fq = FqTypeName::declared(&flags_module, &flags_name);
                    (flags_name, flags_module, fq, vec![])
                } else {
                    (
                        "u32".to_string(),
                        ModuleSource::primitive(),
                        FqTypeName::builtin("u32"),
                        vec![],
                    )
                }
            }
            // The target names no struct-like type: a trait, an undeclared
            // name, a turbofish on a non-generic.
            _ => {
                let _ = self.emit(TypeError::UnknownFunction {
                    name: static_call_symbol_name(static_call),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            }
        };

        // Find trait name: if the static method belongs to a trait impl, include the
        // trait name in the mangled function name so WIR can resolve it correctly.
        // For From/TryFrom, disambiguate by matching the first argument's type.
        let arg_type_hint = if (static_call.method == "from" || static_call.method == "try_from")
            && args.len() == 1
        {
            Some(self.tysys.type_table.borrow().type_name(args[0].type_id))
        } else {
            None
        };
        // Keep the whole selection: its trait names the mangled function and
        // its `method_id` is the declaration this call resolved to, which the
        // use→def edge below is recorded against.
        let selected = self.locate_static_method_impl(
            &struct_name,
            &static_call.method,
            arg_type_hint.as_deref(),
        );
        let trait_name_opt = selected.as_ref().and_then(|r| r.trait_name.clone());

        // The expected type that shaped the argument came from
        // `lookup_static_method_param_types_keyed`, which keys on (receiver,
        // method) alone — with two conversion impls it can be a different
        // impl's than the one the argument's type then selects. Left alone the
        // mangled name loses its trait and reaches WIR build unresolved, so the
        // disagreement is reported here instead of ICE-ing there.
        if trait_name_opt.is_none()
            && let Some(arg_type) = arg_type_hint.as_deref()
            && !self.has_inherent_static_method(&struct_name, &static_call.method)
            && self.report_unmatched_conversion(
                &struct_name,
                &static_call.method,
                arg_type,
                static_call.span,
            )
        {
            return TypeTable::ERROR;
        }

        let mangled_func_name = MethodName::format_local(
            &mangled_struct_name,
            trait_name_opt.as_deref(),
            &static_call.method,
        );

        let method_ref = StaticMethodRef::new(
            struct_module.clone(),
            struct_name.clone(),
            static_call.method.clone(),
            trait_name_opt.clone(),
            selected.as_ref().and_then(|r| r.method_id),
        );

        // Look up return type
        let mut return_type =
            self.lookup_static_method_return_type(&method_ref, &mangled_func_name);

        // A value blanket indexes statics under its receiver *param* name, so
        // the concrete receiver's own bucket misses.
        if return_type == TypeTable::UNKNOWN
            && let Some(resolved) = self.resolve_blanket_static_method(
                target_type_id,
                &static_call.method,
                static_call.id,
                &method_type_args,
                &static_method_defaults,
            )
        {
            return resolved;
        }

        // Emit a compile error if the static method was not found anywhere
        if return_type == TypeTable::UNKNOWN {
            let _ = self.emit(TypeError::UnknownFunction {
                name: static_call_symbol_name(static_call),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        // Substitute type parameters in the return type using SubstitutionContext
        {
            let mut subst_ctx = SubstitutionContext::new();
            if !struct_type_args.is_empty() {
                subst_ctx = subst_ctx.with_impl_args(&struct_type_args);
            }
            let impl_offset = struct_type_args.len() as u32;
            if !method_type_args.is_empty() {
                subst_ctx = subst_ctx.with_method_args(&method_type_args, impl_offset);
            }
            if !subst_ctx.is_empty() {
                return_type =
                    subst_ctx.substitute(return_type, &mut self.tysys.type_table.borrow_mut());
            }
        }

        // Build monomorph_info for generic instantiations
        let monomorph_info = if struct_type_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            let generic_name = MethodName::format_local(
                &self.qualified_receiver_name(&struct_name),
                trait_name_opt.as_deref(),
                &static_call.method,
            );
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: struct_type_args.clone(),
                method_type_args: method_type_args.clone(),
                is_blanket: false,
            })
        };

        let method_type_arg_names: Vec<String> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().mangle_type_name(*t))
            .collect();
        let impl_only_type_arg_names: Vec<FqTypeName> = struct_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
            .collect();

        let param_is_mut = struct_name_for_lookup
            .as_deref()
            .map(|name| self.lookup_static_method_param_is_mut(name, &static_call.method))
            .unwrap_or_default();

        // Build method_info with base struct name and trait name (if applicable)
        let mut method_info = LocalMethodName::new(
            self.qualified_receiver_name(&struct_name),
            trait_name_opt,
            static_call.method.clone(),
        )
        .with_type_args(&impl_only_type_arg_names, &method_type_arg_names);

        // Propagate #[cm("...")] from resource static methods for CM binding synthesis.
        method_info.cm_name =
            self.lookup_resource_static_cm(&struct_name, &struct_module, &static_call.method);

        // Record use->def for jump-to-definition on the method name token.
        // The selection knows which impl answered — two conversion impls on
        // one type declare the same `from`, so a name lookup cannot tell them
        // apart. It covers trait impls only; an inherent static has no
        // selection and reaches the index instead.
        if let Some(method_ast_id) = selected
            .as_ref()
            .and_then(|r| r.method_id)
            .or_else(|| self.static_method_decl_id(&struct_name, &static_call.method))
        {
            self.record_reference_to_def(static_call.method_id, method_ast_id);
        }

        let func_ref = FunctionRef {
            module_source: struct_module,
            name: mangled_func_name,
            monomorph_info,
            method_info: Some(method_info),
        };

        // Stage 5 (WEP 2026-05-26): record the resolved static-method
        // call so reify reproduces the same `FunctionRef` (mangled name,
        // monomorph info, and `cm_name` for CM binding synthesis) without
        // re-resolving the target type — reify's from-scratch resolution
        // fails on imported / CM generic targets (`Future::<T>::new`,
        // `Result::<…>::Ok`), yielding an empty struct name. Keyed on the
        // `StaticMethodCallExpr`'s own `AstId`; variant-ctor turbofish
        // shapes are handled by reify before this fact is consulted.
        let key = static_call.id;
        self.sem.types.static_method_dispatch.insert(
            key,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut,
                type_args: method_type_args,
                param_defaults: static_method_defaults,
                self_in_args: false,
            },
        );

        // Stage 7-B: reify rebuilds the static-method `Call` TIR from the
        // recorded `static_method_dispatch` + resolved args; the combined
        // walk projects only the result type. `args` was resolved above for
        // its fact-recording side effects.
        return_type
    }

    /// Resolve a `static` trait method reached through a value blanket impl
    /// (`impl<T: Bound> Trait for T`). The blanket has no per-type home, so its
    /// statics are indexed under the receiver param name (`T`) and the concrete
    /// receiver's bucket never sees them. Select the blanket whose bounds the
    /// receiver satisfies and dispatch to its template, the way an instance
    /// method reached through the same impl already does.
    pub(super) fn resolve_blanket_static_method(
        &mut self,
        receiver_type_id: TypeId,
        method: &str,
        call_id: AstId,
        method_type_args: &[TypeId],
        static_method_defaults: &[(String, Option<ast::Expr>)],
    ) -> Option<TypeId> {
        let (trait_name, blanket_param, blanket_module) =
            self.find_blanket_static_method(receiver_type_id, method)?;

        let template_name = MethodName::format_local(
            &FqTypeName::binder(&blanket_param),
            Some(&trait_name),
            method,
        );
        let method_ref = StaticMethodRef::new(
            blanket_module.clone(),
            blanket_param,
            method.to_string(),
            Some(trait_name.clone()),
            None,
        );
        let template_return = self.lookup_static_method_return_type(&method_ref, &template_name);
        if template_return == TypeTable::UNKNOWN {
            return None;
        }
        // The template is written against the blanket param, so `-> Self` /
        // `-> T` lands on the receiver at the call site.
        let return_type = SubstitutionContext::new()
            .with_impl_args(&[receiver_type_id])
            .substitute(template_return, &mut self.tysys.type_table.borrow_mut());

        let receiver_arg_name = self
            .tysys
            .type_table
            .borrow()
            .fq_type_name(receiver_type_id);
        let method_type_arg_names: Vec<String> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().mangle_type_name(*t))
            .collect();
        let method_info = LocalMethodName::new(
            self.tysys.fq_receiver_head(receiver_type_id),
            Some(trait_name),
            method.to_string(),
        )
        .with_type_args(&[receiver_arg_name], &method_type_arg_names);

        let func_ref = FunctionRef {
            module_source: blanket_module,
            name: method_info.to_mangled_name(),
            monomorph_info: Some(MonomorphInfo {
                generic_name: template_name,
                impl_type_args: vec![receiver_type_id],
                method_type_args: method_type_args.to_vec(),
                is_blanket: true,
            }),
            method_info: Some(method_info),
        };
        self.sem.types.static_method_dispatch.insert(
            call_id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut: Vec::new(),
                type_args: method_type_args.to_vec(),
                param_defaults: static_method_defaults.to_vec(),
                self_in_args: false,
            },
        );

        Some(return_type)
    }

    /// The value blanket impl carrying a static `method_name` whose receiver
    /// bounds `receiver_type_id` satisfies, as `(trait, receiver param, module)`.
    fn find_blanket_static_method(
        &mut self,
        receiver_type_id: TypeId,
        method_name: &str,
    ) -> Option<(String, String, ModuleSource)> {
        let candidates: Vec<(String, String, ModuleSource, Vec<String>)> = self
            .tysys
            .trait_env
            .blanket_impls
            .iter()
            .flat_map(|(trait_name, impls)| impls.iter().map(move |b| (trait_name, b)))
            .filter(|(_, b)| b.receiver == super::trait_env::BlanketReceiver::Value)
            // Reading the impl header instead would also match an instance
            // method of the same name.
            .filter(|(_, b)| {
                self.tysys
                    .trait_env
                    .static_method_index
                    .get(&(b.module.clone(), b.param.clone()))
                    .is_some_and(|entries| entries.iter().any(|e| e.name == method_name))
            })
            .map(|(trait_name, b)| {
                (
                    trait_name.clone(),
                    b.param.clone(),
                    b.module.clone(),
                    b.bounds.clone(),
                )
            })
            .collect();

        candidates
            .into_iter()
            .find(|(_, _, _, bounds)| {
                bounds.iter().all(|bound| {
                    self.tysys.type_implements_trait(
                        &self.annotate_ctx,
                        &self.type_lookup(),
                        receiver_type_id,
                        bound,
                    )
                })
            })
            .map(|(trait_name, param, module, _)| (trait_name, param, module))
    }

    /// Look up `#[cm("...")]` for a static (no-self) method on a resource type in a module.
    fn lookup_resource_static_cm(
        &self,
        struct_name: &str,
        struct_module: &ModuleSource,
        method_name: &str,
    ) -> Option<String> {
        let (_, _, decl_id, _) = self
            .tysys
            .trait_env
            .resource_static_method_index
            .get(&(struct_module.clone(), struct_name.to_string()))?
            .iter()
            .find(|(name, ..)| name == method_name)?;
        self.tysys
            .signatures
            .resource_method_sig(*decl_id, method_name)?
            .cm_name
            .clone()
    }

    /// Check if a static method exists directly for a given type name (no newtype fallback).
    fn has_static_method_direct(&self, struct_name: &str, method_name: &str) -> bool {
        let qualified = self.qualified_receiver_name(struct_name);
        let mangled = MethodName::format_local(&qualified, None, method_name);
        if self.sem.decls.function_return_types.contains_key(&mangled) {
            return true;
        }
        // Also check with trait-qualified name
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name) {
            let trait_mangled =
                MethodName::format_local(&qualified, Some(&trait_name), method_name);
            if self
                .sem
                .decls
                .function_return_types
                .contains_key(&trait_mangled)
            {
                return true;
            }
        }
        // Every impl block on the type, inherent and trait alike.
        let keys = self
            .tysys
            .trait_env
            .all_impl_keys(&self.impl_target(struct_name));
        self.keys_declare_static_method(&keys, method_name)
    }

    /// Look up static method return type based on struct name and method name
    pub(super) fn lookup_static_method_return_type(
        &mut self,
        method_ref: &StaticMethodRef,
        mangled_func_name: &str,
    ) -> TypeId {
        let struct_name = method_ref.type_name.as_str();
        let method_name = method_ref.method_name.as_str();
        // First check locally registered function_return_types
        if let Some(&return_type) = self.sem.decls.function_return_types.get(mangled_func_name) {
            return return_type;
        }

        // Also try with just StructName::method (for non-generic types)
        let simple_name = MethodName::format_local(
            &self.qualified_receiver_name(struct_name),
            None,
            method_name,
        );
        if let Some(&return_type) = self.sem.decls.function_return_types.get(&simple_name) {
            return return_type;
        }

        // Try with trait-qualified name (StructName^TraitName::method)
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name) {
            let trait_mangled = MethodName::format_local(
                &self.qualified_receiver_name(struct_name),
                Some(&trait_name),
                method_name,
            );
            if let Some(&return_type) = self.sem.decls.function_return_types.get(&trait_mangled) {
                return return_type;
            }
        }

        // Search via pre-built index (handles impls defined outside the struct's defining module).
        // Canonicalise the bare `struct_name` through the call site's import context so the
        // canonical decl key disambiguates two modules' same-named static methods.
        let static_key = self.canonical_decl_key(struct_name);
        // The decl pass already resolved this signature in the impl's own
        // frame — impl and method type params interned, `Self` bound to the
        // impl target, the impl module's imports in scope. Re-deriving all of
        // that here is what the digest exists to avoid.
        let indexed_return = self
            .tysys
            .trait_env
            .static_method_index
            .get(&static_key)
            .and_then(|methods| methods.iter().find(|e| e.name == method_name))
            .and_then(|e| self.tysys.signatures.method_sig(e.method_id))
            .map(|sig| sig.decl.return_type.unwrap_or(TypeTable::UNIT));
        if let Some(return_type) = indexed_return {
            return return_type;
        }

        // Search resource declarations via pre-built index. Same canonical
        // key disambiguation as the inherent-impl path above. The decl pass
        // resolved these in the resource's own frame, so a generic resource's
        // `Option<T>` is already a `TypeParam` here.
        let indexed_resource_return = self
            .tysys
            .trait_env
            .resource_static_method_index
            .get(&static_key)
            .and_then(|methods| {
                methods
                    .iter()
                    .find(|(name, ..)| name == method_name)
                    .and_then(|(name, _, item_id, _)| {
                        let sig = self.tysys.signatures.resource_method_sig(*item_id, name)?;
                        Some(sig.decl.return_type.unwrap_or(TypeTable::UNIT))
                    })
            });
        if let Some(return_type) = indexed_resource_return {
            return return_type;
        }

        // Auto-derived `Default::default()` returns the struct type itself.
        if method_name == "default"
            && let Some(struct_type) = self
                .tysys
                .auto_derive_default_struct_type(&self.type_lookup(), struct_name)
        {
            return struct_type;
        }

        // Fall back to a trait default method body. When
        // `impl Trait for Type` does not override a static method that the
        // trait provides a default for, concrete `Type::method()` calls
        // must still resolve — this mirrors how generic dispatch
        // (`T::method()`) already reaches the trait default via
        // `find_method_type_param_names`.
        if let Some(trait_name) = self.find_static_method_trait(struct_name, method_name)
            && let Some(default_method) = self
                .trait_sig_by_name(&trait_name)
                .and_then(|sig| sig.method(method_name))
                .filter(|m| m.default_body.is_some() && m.sig.self_kind == ast::SelfKind::None)
                .cloned()
        {
            let mut scope = self.enter_inherited_type_param_scope();
            let self_type_id = scope.resolve_named_type(struct_name, Span::default(), false);
            let result = default_method
                .sig
                .instantiate_call(&scope.tysys.type_table, &[self_type_id], &[])
                .return_type;
            drop(scope);
            return result;
        }

        TypeTable::UNKNOWN
    }

    /// Look up static method parameter types for coercion.
    ///
    /// Sets up impl-level and method-level type parameters in scope so that
    /// generic parameter types resolve to `TypeParam(...)` instead of `Unknown`.
    /// Callers can then substitute these with concrete types after inference.
    pub(super) fn lookup_static_method_param_types(
        &mut self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<TypeId> {
        self.lookup_static_method_param_types_keyed(struct_name, method_name, None)
    }

    /// Like [`Self::lookup_static_method_param_types`] but takes a pre-
    /// resolved canonical receiver key. Used by call sites that already
    /// resolved the target type to a `TypeId` and extracted its
    /// `(module_source, name)`: when the caller is on a same-name struct
    /// in another module, the bare-name `struct_name` would canonicalise
    /// against a global "first matching name" bucket and pick the wrong
    /// impl. The explicit key bypasses that ambiguity.
    pub(super) fn lookup_static_method_param_types_keyed(
        &mut self,
        struct_name: &str,
        method_name: &str,
        static_key_hint: Option<&crate::elaborator::trait_env::DeclKey>,
    ) -> Vec<TypeId> {
        // O(1) lookup via pre-built static method index. The index is
        // keyed by the receiver's canonical decl key so two same-named
        // structs in different modules each resolve to their own
        // bucket. Prefer the caller's pre-resolved key when available
        // (it threads through the `TypeId`'s module source and so
        // distinguishes `CounterA::make` from `CounterB::make` even
        // though both alias the same bare name `"Counter"`).
        let static_key = static_key_hint
            .cloned()
            .unwrap_or_else(|| self.canonical_decl_key(struct_name));
        // Carry the impl's defining module out of the index alongside
        // the AST so the per-param elaborator can swap into its perspective —
        // a static method's signature references types the impl module
        // imports, not the caller's.
        // Static methods take no receiver, so the digest's canonical form —
        // impl type params left abstract — is already the answer.
        let indexed = self
            .tysys
            .trait_env
            .static_method_index
            .get(&static_key)
            .and_then(|methods| methods.iter().find(|e| e.name == method_name))
            .and_then(|e| self.tysys.signatures.method_sig(e.method_id))
            .map(|sig| sig.decl.param_types[sig.first_value_param()..].to_vec());
        if let Some(param_types) = indexed {
            return param_types;
        }

        Vec::new()
    }

    /// Resolve a static-method receiver `TypeId` to its `(struct_name,
    /// decl_key)` for impl / parameter lookups: follow newtypes to the base,
    /// map flags to `u32` and builtin arrays to `core:array`.
    pub(super) fn static_receiver_struct_key(
        &self,
        target_type_id: TypeId,
    ) -> (
        Option<String>,
        Option<crate::elaborator::trait_env::DeclKey>,
    ) {
        let key: Option<crate::elaborator::trait_env::DeclKey> = {
            let mut current_type = target_type_id;
            loop {
                match self.tysys.type_table.borrow().get(current_type).clone() {
                    ResolvedType::Struct {
                        decl_name: name,
                        module_source,
                        ..
                    }
                    | ResolvedType::GenericInstance {
                        name,
                        module_source,
                        ..
                    } => break Some((module_source, name)),
                    ResolvedType::Newtype { base_type, .. } => current_type = base_type,
                    ResolvedType::Flags { .. } => {
                        current_type = TypeTable::U32;
                    }
                    ResolvedType::BuiltinArray(_) => {
                        break Some((
                            ModuleSource::array(),
                            TypeTable::ARRAY_TYPE_NAME.to_string(),
                        ));
                    }
                    _ => break None,
                }
            }
        };
        let name = key.as_ref().map(|(_, n)| n.clone());
        (name, key)
    }

    /// Default-value expressions for a static method's non-self parameters, in
    /// the same order as [`Self::lookup_static_method_param_types_keyed`].
    /// Returns `(param_name, default_expr)` pairs; `default_expr` is `None` for
    /// parameters without a declared default.
    pub(super) fn lookup_static_method_param_defaults_keyed(
        &mut self,
        struct_name: &str,
        method_name: &str,
        static_key_hint: Option<&crate::elaborator::trait_env::DeclKey>,
    ) -> Vec<(String, Option<ast::Expr>)> {
        let static_key = static_key_hint
            .cloned()
            .unwrap_or_else(|| self.canonical_decl_key(struct_name));
        // Names and defaults come out of the same record, so their order
        // matches the parameter types by construction.
        let indexed = self
            .tysys
            .trait_env
            .static_method_index
            .get(&static_key)
            .and_then(|methods| methods.iter().find(|e| e.name == method_name))
            .and_then(|e| self.tysys.signatures.method_sig(e.method_id))
            .map(|sig| crate::elaborator::sig::Param::named_defaults(&sig.params));
        if let Some(defaults) = indexed {
            return defaults;
        }

        Vec::new()
    }

    /// Impl blocks on `struct_name`, current-module-first. `all_impl_index` is
    /// already in global order, so the partition needs no per-call sort.
    fn impl_blocks_for_type<'b>(&'b self, type_key: &ImplTargetKey) -> Vec<&'b ast::ImplBlock> {
        let Some(keys) = self.tysys.trait_env.all_impl_index.get(type_key) else {
            return Vec::new();
        };
        let mut current: Vec<&ast::ImplBlock> = Vec::new();
        let mut others: Vec<&ast::ImplBlock> = Vec::new();
        for key in keys {
            let Some(Item::Impl(impl_block)) = self
                .loaded_modules
                .get(&key.0)
                .and_then(|m| m.item_by_id(key.1))
            else {
                continue;
            };
            if key.0 == self.current_module_source {
                current.push(impl_block);
            } else {
                others.push(impl_block);
            }
        }
        current.extend(others);
        current
    }

    /// Look up whether each non-self parameter of an instance method is `mut`.
    /// Returns empty vec (conservative) for unknown methods.
    fn lookup_method_param_is_mut(&self, type_key: &ImplTargetKey, method_name: &str) -> Vec<bool> {
        for impl_block in self.impl_blocks_for_type(type_key) {
            for method in &impl_block.methods {
                if method.name == method_name {
                    return method
                        .params
                        .iter()
                        .filter(|p| p.self_kind == ast::SelfKind::None)
                        .map(|p| p.is_mut)
                        .collect();
                }
            }
        }
        Vec::new()
    }

    /// Look up whether each parameter of a static method is `mut`.
    /// Returns empty vec (conservative) for unknown methods.
    pub(super) fn lookup_static_method_param_is_mut(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<bool> {
        let type_target = self.impl_target(struct_name);
        for impl_block in self.impl_blocks_for_type(&type_target) {
            for method in &impl_block.methods {
                let has_self = method
                    .params
                    .iter()
                    .any(|p| p.self_kind != ast::SelfKind::None);
                if method.name == method_name && !has_self {
                    return method.params.iter().map(|p| p.is_mut).collect();
                }
            }
        }
        Vec::new()
    }

    /// Find the trait name for a static method on a struct, if the method belongs to a trait impl.
    /// Returns `None` for inherent static methods, `Some(trait_name)` for trait static methods.
    pub(super) fn has_from_synthesis_request(
        &self,
        target_type: &ast::Type,
        arg_type_id: &crate::tir::TypeId,
    ) -> bool {
        let target_name = Self::get_type_name_static(target_type);
        let arg_type_name = self.tysys.type_table.borrow().type_name(*arg_type_id);
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        self.tysys.trait_env.impl_headers.values().any(|header| {
            if !header.is_synthesize_request {
                return false;
            }
            let Some(trait_type) = &header.trait_type else {
                return false;
            };
            if header.trait_name.as_deref() != Some(from_trait_name.as_str())
                || Self::get_type_name_static(&header.ty) != target_name
            {
                return false;
            }
            matches!(trait_type, ast::Type::Generic(generic)
                if generic.args.len() == 1
                    && self.get_type_name_full(&generic.args[0]) == arg_type_name)
        })
    }

    pub(super) fn find_static_method_trait(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<String> {
        self.locate_static_method_impl(struct_name, method_name, None)
            .and_then(|r| r.trait_name)
    }

    /// Which conversion trait a static `from` / `try_from` call names.
    pub(super) fn conversion_trait_name(&self, method_name: &str) -> String {
        if method_name == "try_from" {
            "TryFrom".to_string()
        } else {
            self.tysys
                .type_table
                .borrow()
                .compiler_trait_name(crate::compiler_item::CompilerItem::From)
                .to_string()
        }
    }

    /// Report why a conversion call's argument matched no impl, when the
    /// receiver's conversion impls explain it: a blanket impl this path
    /// cannot instantiate, or concrete impls none of which accept the
    /// argument's type. Returns whether an error was emitted — the caller
    /// then stops instead of building an unresolvable mangled name (an ICE
    /// at WIR build).
    pub(super) fn report_unmatched_conversion(
        &mut self,
        struct_name: &str,
        method_name: &str,
        arg_type: &str,
        span: Span,
    ) -> bool {
        let (candidates, has_blanket) = self.conversion_impl_survey(struct_name, method_name);
        if has_blanket {
            let _ = self.emit(TypeError::UnsupportedBlanketConversion {
                trait_name: self.conversion_trait_name(method_name),
                receiver: struct_name.to_string(),
                method: method_name.to_string(),
                arg_type: arg_type.to_string(),
                span,
            });
            return true;
        }
        if !candidates.is_empty() {
            let _ = self.emit(TypeError::NoMatchingTraitArgument {
                trait_name: self.conversion_trait_name(method_name),
                receiver: struct_name.to_string(),
                method: method_name.to_string(),
                arg_type: arg_type.to_string(),
                candidates: candidates.into_iter().map(|c| c.spelling).collect(),
                span,
            });
            return true;
        }
        false
    }

    /// The shared preselect entry for a one-argument conversion call
    /// (`Wrapper::from(42)`, in either its static-call or plain-call
    /// spelling): `Selected` installs the chosen impl's source type as the
    /// argument's expected type; `Ambiguous` reports and returns `true` so
    /// the caller stops.
    pub(super) fn try_conversion_preselect(
        &mut self,
        recv_name: &str,
        method_name: &str,
        arg: &ast::Expr,
        span: Span,
        ctx: &mut FunctionContext,
        param_types: &mut Vec<TypeId>,
    ) -> bool {
        if (method_name != "from" && method_name != "try_from")
            || self.has_inherent_static_method(recv_name, method_name)
        {
            return false;
        }
        let probe = self.probe_arg_class(arg, ctx);
        match self.conversion_preselect(recv_name, method_name, &probe) {
            ConversionPreselect::Selected(source) => {
                *param_types = vec![source];
                false
            }
            ConversionPreselect::Ambiguous(candidates) => {
                let _ = self.emit(TypeError::AmbiguousConversionArgument {
                    receiver: recv_name.to_string(),
                    method: method_name.to_string(),
                    candidates,
                    span,
                });
                true
            }
            ConversionPreselect::Pass => false,
        }
    }

    /// Whether any impl block among `keys` declares `method_name` taking no
    /// receiver. Headers name the methods and the signature digest says
    /// whether each takes `self`, so the question is answered without an
    /// impl-block AST — and keyed canonically, so two modules' same-named
    /// types cannot answer for each other.
    fn keys_declare_static_method(
        &self,
        keys: &[(ModuleSource, crate::ast::AstId)],
        method_name: &str,
    ) -> bool {
        keys.iter().any(|key| {
            self.tysys
                .trait_env
                .impl_headers
                .get(key)
                .into_iter()
                .flat_map(|header| header.methods.iter())
                .any(|m| {
                    m.name == method_name
                        && self
                            .tysys
                            .signatures
                            .method_sig(m.ast_id)
                            .is_some_and(|sig| sig.self_kind == ast::SelfKind::None)
                })
        })
    }

    /// Whether an inherent impl (`impl Type { … }`) declares a no-self method
    /// of this name. A conversion-call guard needs the distinction: a trait
    /// lookup returning `None` is a failure only when no inherent static can
    /// answer instead.
    pub(super) fn has_inherent_static_method(&self, struct_name: &str, method_name: &str) -> bool {
        let keys = self
            .tysys
            .trait_env
            .inherent_impl_keys(&self.impl_target(struct_name));
        self.keys_declare_static_method(&keys, method_name)
    }

    /// The literal preselect over a receiver's conversion impls
    /// (WEP 2026-07-31 phase 4) — this DOES decide calls: `Selected` /
    /// `Ambiguous` short-circuit resolution.
    ///
    /// Selection must run before the argument is elaborated: the expected
    /// type that shapes a literal comes from the selected impl, and picking
    /// it afterwards is the circular ordering the WEP diagnoses. Only the
    /// literal classes participate — a concrete argument's resolved type
    /// already selects deterministically through the name hint, and `Admit`
    /// arguments carry their own type.
    ///
    /// Admissibility is [`Elaborator::probe_admits`] over each impl's
    /// *resolved* source type — the same table argument-directed selection
    /// uses — so an integer newtype admits an integer literal here exactly as
    /// it does there. A spelling table would under-admit newtypes, and
    /// under-admission selects wrongly (the forbidden direction).
    pub(super) fn conversion_preselect(
        &mut self,
        struct_name: &str,
        method_name: &str,
        probe: &super::method_lookup::ProbeClass,
    ) -> ConversionPreselect {
        use super::method_lookup::ProbeClass;
        if !matches!(
            probe,
            ProbeClass::IntLit | ProbeClass::FloatLit | ProbeClass::StrLit
        ) {
            return ConversionPreselect::Pass;
        }
        let (candidates, _has_blanket) = self.conversion_impl_survey(struct_name, method_name);
        let admitted: Vec<ConversionCandidate> = candidates
            .into_iter()
            .filter(|c| {
                c.source != TypeTable::UNKNOWN
                    && c.source != TypeTable::ERROR
                    && self.probe_admits(c.source, probe)
            })
            .collect();
        match admitted.as_slice() {
            [] => ConversionPreselect::Pass,
            [only] => ConversionPreselect::Selected(only.source),
            _ => ConversionPreselect::Ambiguous(admitted.into_iter().map(|c| c.spelling).collect()),
        }
    }

    /// The source types the receiver's conversion impls accept
    /// (`From<String>` beside `From<i64>`), each with its spelling (for
    /// diagnostics) and its type resolved in the impl's own frame (for
    /// admissibility), in candidate order, plus whether a blanket conversion
    /// impl exists. It walks the impls directly rather than sharing
    /// [`Self::locate_static_method_impl`]'s early-return traversal, because
    /// its consumers need the full candidate list.
    pub(super) fn conversion_impl_survey(
        &mut self,
        struct_name: &str,
        method_name: &str,
    ) -> (Vec<ConversionCandidate>, bool) {
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let mut gathered: Vec<(ast::Type, ModuleSource, String)> = Vec::new();
        let mut seen_spellings: Vec<String> = Vec::new();
        let mut has_blanket = false;
        {
            let mut collect = |s: &Self, impl_block: &ast::ImplBlock, module: &ModuleSource| {
                let Some(trait_type) = impl_block.trait_type.as_ref() else {
                    return;
                };
                let base = Self::get_type_name_static(trait_type);
                if Self::get_type_name_static(&impl_block.ty) != struct_name
                    || (base != from_trait_name && base != "TryFrom")
                    || !impl_block.methods.iter().any(|m| m.name == method_name)
                {
                    return;
                }
                if let ast::Type::Generic(g) = trait_type
                    && let Some(arg) = g.args.first()
                {
                    // A source mentioning one of the impl's type parameters is
                    // a blanket: it accepts (a family of) everything, its
                    // presence means the trait-less path can resolve the call
                    // through the blanket resolver, and it is never an
                    // unmatched alternative worth listing.
                    if ast_type_mentions_param(arg, &impl_block.type_params) {
                        has_blanket = true;
                        return;
                    }
                    // Full spelling with the head un-aliased, so the
                    // alternatives read `List<i32>`, not a bare `List`.
                    let head = Self::get_type_name_static(arg);
                    let head = s.import_original_name(&head, module);
                    let mut rendered = String::new();
                    crate::unparse::unparse_type_into(arg, &mut rendered);
                    let resolved = match rendered.split_once('<') {
                        Some((_, args)) => format!("{head}<{args}"),
                        None => head,
                    };
                    // The current module's impls are in the impl index too, so
                    // the two passes below see each of them twice. Coherence
                    // forbids two impls of one conversion, so a repeat is
                    // always the same impl seen again.
                    if !seen_spellings.contains(&resolved) {
                        seen_spellings.push(resolved.clone());
                        gathered.push((arg.clone(), module.clone(), resolved));
                    }
                }
            };

            for item in self.current_module_items {
                if let Item::Impl(impl_block) = item {
                    collect(self, impl_block, &self.current_module_source);
                }
            }
            if let Some(entries) = self
                .tysys
                .trait_env
                .impl_index
                .get(&self.impl_target(struct_name))
            {
                for (module_source, item_id) in entries {
                    if let Some(module) = self.loaded_modules.get(module_source)
                        && let Some(Item::Impl(impl_block)) = module.item_by_id(*item_id)
                    {
                        collect(self, impl_block, module_source);
                    }
                }
            }
        }

        // Resolve each source in its impl's frame, so a private or aliased
        // name means what the impl wrote. Recording is suppressed: these are
        // (possibly foreign) declaration nodes, not uses at this call site.
        let candidates = gathered
            .into_iter()
            .map(|(arg, module, spelling)| {
                let source = self.with_reference_recording_suppressed(|s| {
                    s.with_module_perspective_for(&module, |s2| s2.resolve_type(&arg))
                });
                ConversionCandidate { spelling, source }
            })
            .collect();
        (candidates, has_blanket)
    }

    /// Locate a static trait method impl, returning the resolved identity
    /// (`module`, `type_name`, `method_name`, `trait_name`). Used so that
    /// `FunctionRef` gets the correct `module_source` — especially when a
    /// user defines `impl From<MyType> for i32` in the entry module (or
    /// another module), so DCE and WIR building can find it.
    /// The original (un-aliased) name `name` resolves to *within `module`* — its
    /// `use { Original as name }` original, or `name` itself when not aliased.
    /// Resolving in the impl's own module (not the call site) makes `From`-impl
    /// matching independent of whatever alias the caller uses for the source
    /// type.
    fn import_original_name(&self, name: &str, module: &ModuleSource) -> String {
        let fallback = || name.to_string();
        if module == &self.current_module_source {
            return self
                .sem
                .imports
                .import_original_names
                .get(name)
                .cloned()
                .unwrap_or_else(fallback);
        }
        let scope = self.tysys.trait_env.import_scope(module);
        scope
            .original_names
            .get(name)
            .cloned()
            .unwrap_or_else(fallback)
    }

    pub(super) fn locate_static_method_impl(
        &self,
        struct_name: &str,
        method_name: &str,
        arg_type_name: Option<&str>,
    ) -> Option<StaticMethodRef> {
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let is_from_or_try_from =
            |base: &str| -> bool { base == from_trait_name || base == "TryFrom" };
        let resolve_trait_name = |trait_type: &ast::Type| -> String {
            let base = Self::get_type_name_static(trait_type);
            if is_from_or_try_from(&base) {
                self.get_type_name_full(trait_type)
            } else {
                base
            }
        };

        let matches_arg_type = |trait_type: &ast::Type,
                                impl_module: &ModuleSource,
                                type_params: &[ast::GenericParam]|
         -> bool {
            let Some(expected) = arg_type_name else {
                return true;
            };
            let base = Self::get_type_name_static(trait_type);
            if is_from_or_try_from(&base)
                && let ast::Type::Generic(g) = trait_type
                && let Some(arg) = g.args.first()
            {
                // A blanket source (`impl<T: Display> From<T> for Wrapper`)
                // is deliberately NOT matched here: baking its unsubstituted
                // `From<T>` spelling into the mangled name defeats the
                // instantiation `resolve_blanket_static_method` performs.
                // Rejecting it sends the call down the trait-less path, where
                // the blanket resolver picks it up.
                if let ast::Type::Named(n) = arg
                    && type_params.iter().any(|p| p.name == n.name)
                {
                    return false;
                }
                // Un-alias the impl's source-type head *in the impl's module*
                // before comparing: `impl From<ClockInstant>` (where
                // `ClockInstant` is `use { Instant as ClockInstant }`) must match
                // a call whose argument's real name is `Instant`, regardless of
                // the alias the caller used. The verbatim name would miss the
                // impl and fall back to a (non-existent) inherent `Type::from`.
                let head = Self::get_type_name_static(arg);
                let head = self.import_original_name(&head, impl_module);
                let expected_head = expected.split('<').next().unwrap_or(expected);
                if head != expected_head {
                    return false;
                }
                // A bare-head argument spelling is fully compared already. A
                // generic one must match its arguments too (whitespace
                // ignored), or two impls sharing a head (`From<List<i32>>`
                // beside `From<List<String>>`) both answer and the first one
                // wins wrongly. Nested aliasing can make the spellings
                // disagree and miss an impl — the name-based hint mechanism's
                // ceiling; TypeId matching is the replacement
                // (WEP 2026-07-31 phase 4).
                if !expected.contains('<') {
                    return true;
                }
                let mut rendered = String::new();
                crate::unparse::unparse_type_into(arg, &mut rendered);
                let full: String = match rendered.split_once('<') {
                    Some((_, args)) => format!("{head}<{args}"),
                    None => head,
                };
                let strip = |t: &str| t.replace(' ', "");
                return strip(&full) == strip(expected);
            }
            !is_from_or_try_from(&base)
        };

        // Returns the trait the impl names and the node declaring the method
        // there — the identity of what this selection picked, so a caller
        // recording a use→def edge names the impl the argument chose rather
        // than the receiver's first same-named method.
        let check_impl =
            |impl_block: &ast::ImplBlock, impl_module: &ModuleSource| -> Option<(String, AstId)> {
                let trait_type = impl_block.trait_type.as_ref()?;
                if Self::get_type_name_static(&impl_block.ty) != struct_name
                    || !matches_arg_type(trait_type, impl_module, &impl_block.type_params)
                {
                    return None;
                }
                for method in &impl_block.methods {
                    let has_self = method
                        .params
                        .iter()
                        .any(|p| p.self_kind != ast::SelfKind::None);
                    if method.name == method_name && !has_self {
                        return Some((resolve_trait_name(trait_type), method.id));
                    }
                }
                // Fall back to the trait declaration's default methods: when
                // `impl Trait for Type` does not override a defaulted static
                // method, the trait still provides the body, so `Type::method`
                // (called concretely, not via a generic bound) must resolve to
                // the trait's default. This mirrors how generic dispatch
                // (`T::method()`) already finds default methods in
                // `find_method_type_param_names`.
                let trait_name_base = Self::get_type_name_static(trait_type);
                if let Some(method) = self
                    .trait_sig_by_name(&trait_name_base)
                    .and_then(|sig| sig.method(method_name))
                    && method.default_body.is_some()
                    && method.sig.self_kind == ast::SelfKind::None
                {
                    return Some((resolve_trait_name(trait_type), method.sig.ast_id));
                }
                None
            };

        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some((trait_name, method_id)) =
                    check_impl(impl_block, &self.current_module_source)
            {
                return Some(StaticMethodRef::new(
                    self.current_module_source.clone(),
                    struct_name,
                    method_name,
                    Some(trait_name),
                    Some(method_id),
                ));
            }
        }

        // Use trait_env.impl_index for O(1) lookup instead of scanning all modules
        if let Some(entries) = self
            .tysys
            .trait_env
            .impl_index
            .get(&self.impl_target(struct_name))
        {
            for (module_source, item_id) in entries {
                if let Some(module) = self.loaded_modules.get(module_source)
                    && let Some(Item::Impl(impl_block)) = module.item_by_id(*item_id)
                    && let Some((trait_name, method_id)) = check_impl(impl_block, module_source)
                {
                    return Some(StaticMethodRef::new(
                        module_source.clone(),
                        struct_name,
                        method_name,
                        Some(trait_name),
                        Some(method_id),
                    ));
                }
            }
        }

        if method_name == "default"
            && self
                .tysys
                .auto_derive_default_struct_type(&self.type_lookup(), struct_name)
                .is_some()
        {
            let default_trait_name = self
                .tysys
                .type_table
                .borrow()
                .compiler_trait_name(crate::compiler_item::CompilerItem::Default)
                .to_string();
            let module_source = self.find_struct_module_source(struct_name);
            self.tysys
                .type_table
                .borrow_mut()
                .record_bound_driven_synth_request(
                    struct_name,
                    &module_source,
                    &default_trait_name,
                );
            return Some(StaticMethodRef::new(
                module_source,
                struct_name,
                method_name,
                Some(default_trait_name),
                None,
            ));
        }

        None
    }

    /// Get the operator trait and method name for a binary operator.
    pub(super) fn is_static_method(&self, struct_name: &str, method_name: &str) -> bool {
        let mangled_name = MethodName::format_local(
            &self.qualified_receiver_name(struct_name),
            None,
            method_name,
        );

        // Check if it's registered in function_return_types (static methods are registered there)
        if self
            .sem
            .decls
            .function_return_types
            .contains_key(&mangled_name)
        {
            return true;
        }

        // O(1) lookup via pre-built static method index (impl blocks).
        // Canonicalise so a same-named struct in another module doesn't
        // accidentally claim this name.
        let static_key = self.canonical_decl_key(struct_name);
        if let Some(methods) = self.tysys.trait_env.static_method_index.get(&static_key)
            && methods.iter().any(|e| e.name == method_name)
        {
            return true;
        }

        // The index holds only what `TraitEnv::build` classified as a static
        // method; ask the headers directly for the rest.
        if self.keys_declare_static_method(
            &self.tysys.trait_env.all_impl_keys(&self.impl_target(struct_name)),
            method_name,
        ) {
            return true;
        }

        // O(1) lookup via pre-built resource static method index.
        // Same canonical-key disambiguation.
        if let Some(methods) = self
            .tysys
            .trait_env
            .resource_static_method_index
            .get(&static_key)
            && methods.iter().any(|(name, ..)| name == method_name)
        {
            return true;
        }

        // For newtypes/flags, check if the base type has the static method
        if let Some(newtype_id) = self.lookup_newtype(struct_name) {
            let base_name = match self.tysys.type_table.borrow().get(newtype_id).clone() {
                ResolvedType::Newtype { base_type, .. } => {
                    Some(self.tysys.get_ultimate_base_struct_name(base_type))
                }
                ResolvedType::Flags { .. } => Some("u32".to_string()),
                _ => None,
            };
            if let Some(base_name) = base_name
                && self.is_static_method(&base_name, method_name)
            {
                return true;
            }
        }

        // Auto-derived `Default::default()` for structs whose fields all have
        // default expressions. No user impl exists (previous checks would have
        // caught it), but `synthesis::traits` will emit the body.
        if method_name == "default"
            && self
                .tysys
                .auto_derive_default_struct_type(&self.type_lookup(), struct_name)
                .is_some()
        {
            return true;
        }

        // `ReflectStruct` metadata is reachable only through the trait-qualified form
        // `ReflectStruct::<T>::method()` (see `resolve_call`), never as a bare
        // `T::method()` static method — that keeps struct namespaces clean.

        // Defaulted trait method: when `impl Trait for Type` does not
        // override a static method that the trait provides a default for,
        // `Type::method` must still resolve. `locate_static_method_impl`
        // applies the same fallback to find the trait name and module.
        if self
            .locate_static_method_impl(struct_name, method_name, None)
            .is_some()
        {
            return true;
        }

        false
    }

    /// Resolve a static method call from a qualified name like `Point::origin()`
    pub(super) fn resolve_static_method_call_from_qualified(
        &mut self,
        struct_name: &str,
        method_name: &str,
        args: &[TirExpr],
        impl_type_args: &[TypeId],
        method_type_args: &[TypeId],
        call_id: AstId,
        span: Span,
        _ctx: &mut FunctionContext,
    ) -> TirExpr {
        // The call site may refer to the receiver type through a
        // `use { Counter as CounterA }` alias. Resolve the alias to its
        // canonical declaration name so the mangled TIR function
        // (`Counter::make`) can be found at WIR-build time — that name
        // is keyed by the *original* `Counter`, not the local alias.
        // The other lookups below still consume `struct_name` as-is and
        // canonicalise internally via `Elaborator::canonical_decl_key`.
        // Rebuilt from the canonical key, not the local alias.
        let qualified_struct_name = self.qualified_receiver_name(struct_name);
        let mangled_func_name_owned =
            MethodName::format_local(&qualified_struct_name, None, method_name);
        let mangled_func_name = mangled_func_name_owned.as_str();
        // For newtypes, check if the newtype itself has the method first,
        // then fall back to the base type's static method
        let mut newtype_dispatch: Option<(TypeId, TypeId, Vec<TypeId>)> = None;
        // The written name keys the impl indices; the fq form names the method.
        let (actual_struct_name, actual_struct_fq, actual_mangled_name) = if let Some(newtype_id) =
            self.lookup_newtype(struct_name)
        {
            // First check if the newtype itself has this static method
            if self.has_static_method_direct(struct_name, method_name) {
                (
                    struct_name.to_string(),
                    qualified_struct_name,
                    mangled_func_name.to_string(),
                )
            } else {
                let base_type_id = match self.tysys.type_table.borrow().get(newtype_id).clone() {
                    ResolvedType::Newtype { .. } => Some(
                        self.tysys
                            .type_table
                            .borrow()
                            .get_ultimate_base_type(newtype_id),
                    ),
                    _ => None,
                };
                let base_name = base_type_id
                    .map(|b| self.tysys.get_ultimate_base_struct_name(b))
                    .or_else(|| match self.tysys.type_table.borrow().get(newtype_id) {
                        ResolvedType::Flags { .. } => Some("u32".to_string()),
                        _ => None,
                    });
                if let (Some(base_name), Some(base_type_id)) = (base_name.clone(), base_type_id) {
                    let base_args = match self.tysys.type_table.borrow().get(base_type_id) {
                        ResolvedType::GenericInstance { type_args, .. }
                        | ResolvedType::GenericResource { type_args, .. } => type_args.clone(),
                        ResolvedType::BuiltinArray(elem) => vec![*elem],
                        _ => vec![],
                    };
                    newtype_dispatch = Some((newtype_id, base_type_id, base_args));
                    let base_fq = self.tysys.fq_receiver_head(base_type_id);
                    let mangled = MethodName::format_local(&base_fq, None, method_name);
                    (base_name, base_fq, mangled)
                } else if let Some(base_name) = base_name {
                    let base_fq = self.qualified_receiver_name(&base_name);
                    let mangled = MethodName::format_local(&base_fq, None, method_name);
                    (base_name, base_fq, mangled)
                } else {
                    (
                        struct_name.to_string(),
                        qualified_struct_name,
                        mangled_func_name.to_string(),
                    )
                }
            }
        } else {
            (
                struct_name.to_string(),
                qualified_struct_name,
                mangled_func_name.to_string(),
            )
        };

        let impl_type_args_owned: Vec<TypeId> = match &newtype_dispatch {
            Some((_, _, base_args)) if impl_type_args.is_empty() && !base_args.is_empty() => {
                base_args.clone()
            }
            _ => impl_type_args.to_vec(),
        };
        let impl_type_args = impl_type_args_owned.as_slice();

        // Find trait name and the module where the impl block lives.
        // For From/TryFrom, disambiguate by matching the first argument's type so that
        // user-defined `impl From<MyType> for i32` is resolved to its actual defining module
        // rather than the default `ModuleSource::primitive()` for `i32`.
        let arg_type_hint =
            if (method_name == "from" || method_name == "try_from") && args.len() == 1 {
                Some(self.tysys.type_table.borrow().type_name(args[0].type_id))
            } else {
                None
            };
        let resolved = self.locate_static_method_impl(
            &actual_struct_name,
            method_name,
            arg_type_hint.as_deref(),
        );
        // The expected type that shaped the argument came from
        // `lookup_static_method_param_types_keyed`, which keys on (receiver,
        // method) alone — with two conversion impls it can be a different
        // impl's than the one the argument's type then selects. Left alone the
        // mangled name loses its trait and reaches WIR build unresolved, so the
        // disagreement is reported here instead of ICE-ing there.
        if resolved.is_none()
            && let Some(arg_type) = arg_type_hint.as_deref()
            && !self.has_inherent_static_method(&actual_struct_name, method_name)
            && self.report_unmatched_conversion(&actual_struct_name, method_name, arg_type, span)
        {
            return placeholder(TypeTable::ERROR, span);
        }

        let method_ref = resolved.unwrap_or_else(|| {
            StaticMethodRef::new(
                self.find_struct_module_source(&actual_struct_name),
                &actual_struct_name,
                method_name,
                None,
                None,
            )
        });

        // Use trait-qualified mangled name if this is a trait method
        let final_mangled_name = if let Some(ref trait_name) = method_ref.trait_name {
            MethodName::format_local(&actual_struct_fq, Some(trait_name), method_name)
        } else {
            actual_mangled_name
        };

        // Look up return type using the actual struct name
        let mut return_type =
            self.lookup_static_method_return_type(&method_ref, &final_mangled_name);

        // Substitute impl-level + method-level type parameters in return type.
        // `lookup_static_method_return_type` registers impl params at indices
        // 0..impl_count and method params at indices impl_count..total, so a
        // single flat substitution list `[impl_args.., method_args..]` lines
        // up correctly with `substitute_type_params` (which substitutes by index).
        if !impl_type_args.is_empty() || !method_type_args.is_empty() {
            let mut combined = impl_type_args.to_vec();
            combined.extend_from_slice(method_type_args);
            return_type = self.substitute_type_params(return_type, &combined);
        }

        if let Some((newtype_id, base_type_id, _)) = newtype_dispatch
            && return_type == base_type_id
        {
            return_type = newtype_id;
        }

        // Build monomorph_info for impl-level and/or method-level generic instantiation
        let monomorph_info = if impl_type_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            Some(MonomorphInfo {
                generic_name: final_mangled_name.clone(),
                impl_type_args: impl_type_args.to_vec(),
                method_type_args: method_type_args.to_vec(),
                is_blanket: false,
            })
        };

        let param_is_mut = self.lookup_static_method_param_is_mut(&actual_struct_name, method_name);

        let param_defaults =
            self.lookup_static_method_param_defaults_keyed(&actual_struct_name, method_name, None);

        // Propagate #[cm("...")] from resource static methods
        let cm_name =
            self.lookup_resource_static_cm(&actual_struct_name, &method_ref.module, method_name);

        let StaticMethodRef {
            module: struct_module,
            trait_name: trait_name_opt,
            ..
        } = method_ref;

        let func_ref = FunctionRef {
            module_source: struct_module,
            name: final_mangled_name,
            monomorph_info,
            method_info: Some({
                let mut m =
                    LocalMethodName::new(actual_struct_fq, trait_name_opt, method_name.to_string());
                m.cm_name = cm_name;
                m
            }),
        };

        // Stage 7-B: record the static-method dispatch decision (formerly
        // recovered by the caller from the built `Call` TIR) so reify can
        // reproduce the same `Call` shape without re-running impl lookup,
        // mangled-name construction, or monomorph-info shaping. The per-arg
        // `is_mut` flags match what the old `CallArg`s carried.
        let param_is_mut: Vec<bool> = args
            .iter()
            .zip(param_is_mut.iter().copied().chain(std::iter::repeat(false)))
            .map(|(_, is_mut)| is_mut)
            .collect();
        self.sem.types.static_method_dispatch.insert(
            call_id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut,
                type_args: vec![],
                param_defaults,
                self_in_args: false,
            },
        );

        placeholder(return_type, span)
    }
}

/// See [`Elaborator::conversion_preselect`].
pub(super) enum ConversionPreselect {
    /// Exactly one conversion impl admits the literal: elaborate the argument
    /// against this source type, and the name hint then finds the same impl.
    Selected(TypeId),
    /// Several impls admit the literal — a literal never selects, so the call
    /// is reported with the admitted alternatives.
    Ambiguous(Vec<String>),
    /// The preselect does not apply (non-literal argument, no admitted
    /// candidate, or an unresolvable source type): the existing path decides.
    Pass,
}

/// One non-blanket conversion impl's source type: the spelling for
/// diagnostics, the resolved type for admissibility. See
/// [`Elaborator::conversion_impl_survey`].
pub(super) struct ConversionCandidate {
    pub(super) spelling: String,
    pub(super) source: TypeId,
}

/// Whether an AST type syntactically mentions one of `params`. Shapes the
/// walk does not descend into count as mentioning, so a caller skipping
/// resolution for open types never resolves one by mistake.
fn ast_type_mentions_param(ty: &ast::Type, params: &[ast::GenericParam]) -> bool {
    match ty {
        ast::Type::Named(n) => params.iter().any(|p| p.name == n.name),
        ast::Type::Generic(g) => {
            params.iter().any(|p| p.name == g.name)
                || g.args.iter().any(|a| ast_type_mentions_param(a, params))
        }
        ast::Type::Tuple(elems) => elems.iter().any(|e| ast_type_mentions_param(e, params)),
        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
            ast_type_mentions_param(inner, params)
        }
        _ => !params.is_empty(),
    }
}
