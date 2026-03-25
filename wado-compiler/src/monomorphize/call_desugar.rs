//! Post-monomorphization rewrites: function call rewriting to monomorphized names,
//! and comparison operator desugaring to trait method calls.

use std::cell::RefCell;
use std::rc::Rc;

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBinaryOp, TirBlock,
    TirExpr, TirExprKind, TirModule, TirStmt, TirStmtKind, TirTemplatePart, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::tir_visitor::TirMutVisitor;
use crate::token::Span;

use super::state::Monomorphizer;
use super::generic_function_key;

impl Monomorphizer {
    /// Rewrite function calls in all functions to use monomorphized names
    pub(super) fn rewrite_function_calls_in_module(&self, module: &mut TirModule) {
        let type_table = module.type_table.borrow();
        let mut rewriter = CallRewriter {
            mono: self,
            type_table: &type_table,
        };

        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(mut body) = func.body.take() {
                rewriter.visit_block(&mut body);
                // Sync local_types with Let statement types
                Self::sync_local_types_from_lets(&body, &mut func.local_types);
                // Update all Local expression types based on local_types
                Self::update_local_expr_types(&mut body, &func.local_types);
                func.body = Some(body);
            }
        }

        // Rewrite function calls in global variable initializers
        for global in &mut module.globals {
            rewriter.visit_expr(&mut global.initializer);
        }
    }

    /// Sync `local_types` array from Let statements that may have been updated
    fn sync_local_types_from_lets(block: &TirBlock, local_types: &mut [TypeId]) {
        for stmt in &block.stmts {
            match &stmt.kind {
                TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } => {
                    if let Some(local_type) = local_types.get_mut(*local_index as usize) {
                        *local_type = *type_id;
                    }
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    Self::sync_local_types_from_lets(then_block, local_types);
                    if let Some(else_blk) = else_block {
                        Self::sync_local_types_from_lets(else_blk, local_types);
                    }
                }
                TirStmtKind::Loop { body } => {
                    Self::sync_local_types_from_lets(body, local_types);
                }
                _ => {}
            }
        }
    }

    /// Update all Local expression types based on `local_types` array
    fn update_local_expr_types(block: &mut TirBlock, local_types: &[TypeId]) {
        for stmt in &mut block.stmts {
            Self::update_local_expr_types_in_stmt(stmt, local_types);
        }
    }

    fn update_local_expr_types_in_stmt(stmt: &mut TirStmt, local_types: &[TypeId]) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                Self::update_local_expr_types_in_expr(value, local_types);
            }
            TirStmtKind::Expr(expr) => {
                Self::update_local_expr_types_in_expr(expr, local_types);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    Self::update_local_expr_types_in_expr(expr, local_types);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::update_local_expr_types_in_expr(condition, local_types);
                Self::update_local_expr_types(then_block, local_types);
                if let Some(else_blk) = else_block {
                    Self::update_local_expr_types(else_blk, local_types);
                }
            }
            TirStmtKind::Loop { body } => {
                Self::update_local_expr_types(body, local_types);
            }
            _ => {}
        }
    }

    fn update_local_expr_types_in_expr(expr: &mut TirExpr, local_types: &[TypeId]) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } => {
                if let Some(&local_type) = local_types.get(*index as usize) {
                    expr.type_id = local_type;
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::update_local_expr_types_in_expr(&mut arg.expr, local_types);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::update_local_expr_types_in_expr(receiver, local_types);
                for arg in args {
                    Self::update_local_expr_types_in_expr(&mut arg.expr, local_types);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::update_local_expr_types_in_expr(left, local_types);
                Self::update_local_expr_types_in_expr(right, local_types);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                Self::update_local_expr_types_in_expr(inner, local_types);
            }
            TirExprKind::Block(block) => {
                Self::update_local_expr_types(block, local_types);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::update_local_expr_types_in_expr(condition, local_types);
                Self::update_local_expr_types(then_branch, local_types);
                if let Some(else_blk) = else_branch {
                    Self::update_local_expr_types(else_blk, local_types);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::update_local_expr_types_in_expr(elem, local_types);
                }
            }
            TirExprKind::Index { expr, index } => {
                Self::update_local_expr_types_in_expr(expr, local_types);
                Self::update_local_expr_types_in_expr(index, local_types);
            }
            TirExprKind::Assign { target, value } => {
                Self::update_local_expr_types_in_expr(target, local_types);
                Self::update_local_expr_types_in_expr(value, local_types);
            }
            TirExprKind::Match { expr, arms } => {
                Self::update_local_expr_types_in_expr(expr, local_types);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        Self::update_local_expr_types_in_expr(guard, local_types);
                    }
                    Self::update_local_expr_types_in_expr(&mut arm.body, local_types);
                }
            }
            TirExprKind::Closure { .. } => {
                // Closures have their own local scope, don't update with parent's local_types
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::update_local_expr_types_in_expr(&mut field.value, local_types);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::update_local_expr_types_in_expr(callee, local_types);
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                Self::update_local_expr_types_in_expr(functor, local_types);
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        Self::update_local_expr_types_in_expr(inner, local_types);
                    }
                }
            }
            _ => {}
        }
    }
    fn rewrite_call_expr(&self, expr: &mut TirExpr, type_table: &TypeTable) {
        if let TirExprKind::Call {
            func, type_args, ..
        } = &mut expr.kind
        {
            let original_func_name = func.name.clone();
            let original_method_info = func.method_info.clone();
            let qualified_func_name =
                generic_function_key(func.is_method(), &func.module_source, &func.name);
            // If this is a generic call, rewrite to monomorphized name
            if !type_args.is_empty() {
                let key = InstantiationKey {
                    name: qualified_func_name,
                    impl_type_args: vec![],
                    method_type_args: type_args.clone(),
                    method_info: original_method_info.clone(),
                };
                if let Some(mangled) = self.functions.instantiated.get(&key) {
                    *func = FunctionRef {
                        module_source: self.current_module_source.clone(),
                        name: mangled.clone(),
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: original_func_name,
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args.clone(),
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
                        is_cm_binding: false,
                    };

                    if let ResolvedType::TypeParam { index, .. } = type_table.get(expr.type_id)
                        && let Some(&concrete) = key.method_type_args.get(*index as usize)
                    {
                        expr.type_id = concrete;
                    }

                    type_args.clear();
                }
            }
            // Also handle static method calls (formerly StaticCall) that need rewriting
            if let FunctionRef {
                monomorph_info: Some(monomorph),
                method_info: Some(info),
                ..
            } = func
                && (!monomorph.impl_type_args.is_empty() || !monomorph.method_type_args.is_empty())
            {
                let mut names_to_try = vec![MethodName::format_local(
                    &info.base_struct_name,
                    info.trait_name.as_deref(),
                    &info.method_name,
                )];
                if info.struct_name != info.base_struct_name {
                    names_to_try.push(MethodName::format_local(
                        &info.struct_name,
                        info.trait_name.as_deref(),
                        &info.method_name,
                    ));
                }
                for generic_method_name in names_to_try {
                    let key = InstantiationKey {
                        name: generic_method_name.clone(),
                        impl_type_args: monomorph.impl_type_args.clone(),
                        method_type_args: monomorph.method_type_args.clone(),
                        method_info: Some(info.clone()),
                    };
                    if let Some(mangled) = self.functions.instantiated.get(&key) {
                        let original_method_info = func.method_info.clone();
                        *func = FunctionRef {
                            module_source: self.current_module_source.clone(),
                            name: mangled.clone(),
                            monomorph_info: Some(MonomorphInfo {
                                generic_name: generic_method_name,
                                impl_type_args: key.impl_type_args.clone(),
                                method_type_args: key.method_type_args.clone(),
                                is_blanket: false,
                            }),
                            method_info: original_method_info,
                            is_cm_binding: false,
                        };
                        break;
                    }
                }
            }
        }
    }

    fn rewrite_method_call_expr(&self, expr: &mut TirExpr, type_table: &TypeTable) {
        let TirExprKind::MethodCall {
            receiver,
            func: method_func,
            type_args,
            ..
        } = &mut expr.kind
        else {
            return;
        };

        // Extract method name from method_info or fall back to function name
        let method_name = method_func
            .method_info
            .clone()
            .map(|info| info.method_name)
            .unwrap_or_else(|| method_func.name.clone());
        // If this is a generic method call, rewrite to monomorphized name
        if !type_args.is_empty()
            && let Some(struct_name) = self.get_struct_name_from_type(receiver.type_id, type_table)
        {
            // Try both inherent method and trait method formats
            let trait_name_opt = method_func
                .method_info
                .clone()
                .and_then(|info| info.trait_name);
            let mut names_to_try = vec![(
                MethodName::format_local(&struct_name, None, &method_name),
                None::<String>,
            )];
            if let Some(ref tn) = trait_name_opt {
                names_to_try.push((
                    MethodName::format_local(&struct_name, Some(tn), &method_name),
                    Some(tn.clone()),
                ));
            }

            let mut rewritten = false;
            for (full_method_name, _tn) in &names_to_try {
                let key = InstantiationKey {
                    name: full_method_name.clone(),
                    impl_type_args: vec![],
                    method_type_args: type_args.clone(),
                    method_info: None,
                };
                if let Some(mangled) = self.functions.instantiated.get(&key) {
                    let original_method_info = method_func.method_info.clone();
                    *method_func = FunctionRef {
                        module_source: self.current_module_source.clone(),
                        name: mangled.clone(),
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: full_method_name.clone(),
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args.clone(),
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
                        is_cm_binding: false,
                    };
                    type_args.clear();
                    rewritten = true;
                    break;
                }
            }
            // Handle "double generics": method on monomorphized generic struct
            // e.g., c.transform::<i64>() where c: Container<i32>
            // Also handles GenericInstance receivers (e.g., Option<i32>)
            if !rewritten {
                let base_info = self
                    .structs
                    .mangled_to_key
                    .get(&struct_name)
                    .map(|k| (k.name.clone(), k.impl_type_args.clone()))
                    .or_else(|| {
                        self.get_struct_info_from_type(receiver.type_id, type_table)
                            .filter(|(_, args)| !args.is_empty())
                    });
                if let Some((base_struct, impl_type_args)) = base_info {
                    // Try both inherent and trait method formats
                    let mut dg_names = vec![(
                        MethodName::format_local(&base_struct, None, &method_name),
                        None::<String>,
                    )];
                    if let Some(ref tn) = trait_name_opt {
                        dg_names.push((
                            MethodName::format_local(&base_struct, Some(tn), &method_name),
                            Some(tn.clone()),
                        ));
                    }

                    for (generic_method_name, _tn) in &dg_names {
                        let combined_key = InstantiationKey {
                            name: generic_method_name.clone(),
                            impl_type_args: impl_type_args.clone(),
                            method_type_args: type_args.clone(),
                            method_info: None,
                        };
                        if let Some(mangled) = self.functions.instantiated.get(&combined_key) {
                            let original_method_info = method_func.method_info.clone();
                            *method_func = FunctionRef {
                                module_source: self.current_module_source.clone(),
                                name: mangled.clone(),
                                monomorph_info: Some(MonomorphInfo {
                                    generic_name: generic_method_name.clone(),
                                    impl_type_args: combined_key.impl_type_args.clone(),
                                    method_type_args: combined_key.method_type_args.clone(),
                                    is_blanket: false,
                                }),
                                method_info: original_method_info,
                                is_cm_binding: false,
                            };
                            type_args.clear();

                            if let ResolvedType::TypeParam { index, .. } =
                                type_table.get(expr.type_id)
                            {
                                let impl_count = impl_type_args.len() as u32;
                                if *index < impl_count {
                                    if let Some(&concrete) =
                                        combined_key.impl_type_args.get(*index as usize)
                                    {
                                        expr.type_id = concrete;
                                    }
                                } else {
                                    let method_index = (*index - impl_count) as usize;
                                    if let Some(&concrete) =
                                        combined_key.method_type_args.get(method_index)
                                    {
                                        expr.type_id = concrete;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
        // Also handle case where type_args is empty but receiver is a GenericInstance
        // e.g., nums.index_value(0) where nums: Triple<i32>
        else if let Some((base_struct, impl_type_args)) =
            self.get_struct_info_from_type(receiver.type_id, type_table)
            && !impl_type_args.is_empty()
        {
            // Try trait method name format first (e.g., Triple^IndexValue::index_value)
            let mut possible_keys = Vec::new();
            if let Some(ref info) = method_func.method_info.clone()
                && let Some(ref trait_name) = info.trait_name
            {
                // For ref-type impls, try the ref struct name first (e.g., "&^IntoIterator::into_iter")
                if info.base_struct_name != base_struct {
                    possible_keys.push(InstantiationKey {
                        name: MethodName::format_local(
                            &info.base_struct_name,
                            Some(trait_name),
                            &method_name,
                        ),
                        impl_type_args: impl_type_args.clone(),
                        method_type_args: vec![],
                        method_info: None,
                    });
                }
                let trait_method_name =
                    MethodName::format_local(&base_struct, Some(trait_name), &method_name);
                possible_keys.push(InstantiationKey {
                    name: trait_method_name,
                    impl_type_args: impl_type_args.clone(),
                    method_type_args: vec![],
                    method_info: None,
                });
            }
            // Also try regular method format
            possible_keys.push(InstantiationKey {
                name: MethodName::format_local(&base_struct, None, &method_name),
                impl_type_args,
                method_type_args: vec![],
                method_info: None,
            });

            for key in possible_keys {
                if let Some(mangled) = self.functions.instantiated.get(&key) {
                    // Preserve original method_info
                    let original_method_info = method_func.method_info.clone();
                    *method_func = FunctionRef {
                        module_source: self.current_module_source.clone(),
                        name: mangled.clone(),
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: key.name.clone(),
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args.clone(),
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
                        is_cm_binding: false,
                    };
                    break;
                }
            }
        }
        // Blanket impl fallback: if the FunctionRef has monomorph_info from a
        // blanket impl, rewrite to the monomorphized function name.
        {
            let blanket_lookup = if let FunctionRef {
                monomorph_info: Some(mono),
                ..
            } = &*method_func
                && mono.is_blanket
            {
                let key = InstantiationKey {
                    name: mono.generic_name.clone(),
                    impl_type_args: mono.impl_type_args.clone(),
                    method_type_args: mono.method_type_args.clone(),
                    method_info: None,
                };
                self.functions.instantiated.get(&key).map(|mangled| {
                    (
                        mangled.clone(),
                        mono.generic_name.clone(),
                        mono.impl_type_args.clone(),
                        mono.method_type_args.clone(),
                    )
                })
            } else {
                None
            };
            if let Some((mangled, generic_name, impl_ta, method_ta)) = blanket_lookup {
                let original_method_info = method_func.method_info.clone();
                *method_func = FunctionRef {
                    module_source: self.current_module_source.clone(),
                    name: mangled,
                    monomorph_info: Some(MonomorphInfo {
                        generic_name,
                        impl_type_args: impl_ta,
                        method_type_args: method_ta,
                        is_blanket: true,
                    }),
                    method_info: original_method_info,
                    is_cm_binding: false,
                };
            }
        }
    }
    /// Try to desugar a comparison operator to a trait method call.
    ///
    /// This handles comparison operators on struct types that have `Eq` or `Ord`
    /// trait implementations. During initial resolution, generic type parameters
    /// can't be desugared because the concrete type isn't known. After type
    /// substitution during monomorphization, we can now desugar these operators.
    ///
    /// Returns `Some(new_kind)` if the binary expression should be replaced,
    /// or `None` if it should remain as is (for primitives).
    pub(super) fn try_desugar_comparison(
        &self,
        span: Span,
        op: TirBinaryOp,
        left: &TirExpr,
        right: &TirExpr,
        type_table: &mut TypeTable,
    ) -> Option<TirExprKind> {
        // Get the base struct name and type args from the operand type
        let operand_type = type_table.get(left.type_id);
        let (base_struct_name, impl_type_args, type_module_source): (
            String,
            Vec<String>,
            Option<ModuleSource>,
        ) = match operand_type {
            ResolvedType::Struct {
                name,
                module_source,
                base_name,
                ..
            } => {
                let struct_name = base_name.as_deref().unwrap_or(name).to_string();
                (struct_name, vec![], Some(module_source.clone()))
            }
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), vec![], Some(module_source.clone())),
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source,
                ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                (name.clone(), args, Some(module_source.clone()))
            }
            // Primitives don't use trait-based comparison
            _ => return None,
        };

        // Handle Eq trait (== and !=)
        if matches!(op, TirBinaryOp::Eq | TirBinaryOp::NotEq) {
            let needs_negation = op == TirBinaryOp::NotEq;

            // Create receiver with reference (trait methods take &self)
            let receiver_ref_type = type_table.intern(ResolvedType::Ref(left.type_id));
            let receiver = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(left.clone()),
                },
                receiver_ref_type,
                span,
            );

            // Create argument with reference (other: &Self)
            let arg_ref_type = type_table.intern(ResolvedType::Ref(right.type_id));
            let arg_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(right.clone()),
                },
                arg_ref_type,
                span,
            );

            let method_info =
                LocalMethodName::new(base_struct_name, Some("Eq".to_string()), "eq".to_string())
                    .with_struct_type_args(&impl_type_args);
            let mangled_name = method_info.to_mangled_name();

            // Resolve the module where the trait impl lives.
            // First check trait_method_locations (populated during cross-module collection),
            // then fall back to the type's own module_source (impl is in same module as type).
            let method_module_source = self
                .functions
                .trait_method_locations
                .get(&mangled_name)
                .cloned()
                .or(type_module_source)
                .unwrap_or_else(|| self.current_module_source.clone());

            let method_call = TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef {
                    module_source: method_module_source,
                    name: mangled_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                    is_cm_binding: false,
                },
                type_args: vec![],
                args: vec![CallArg::new(arg_ref, false)],
            };

            if needs_negation {
                let bool_type =
                    type_table.intern(ResolvedType::Primitive(crate::tir::PrimitiveType::Bool));
                return Some(TirExprKind::Unary {
                    op: TirUnaryOp::Not,
                    expr: Box::new(TirExpr::new(method_call, bool_type, span)),
                });
            }
            return Some(method_call);
        }

        // Handle Ord trait (<, >, <=, >=)
        // Ord::cmp returns Ordering enum with discriminants: Less=0, Equal=1, Greater=2
        if matches!(
            op,
            TirBinaryOp::Lt | TirBinaryOp::Gt | TirBinaryOp::LtEq | TirBinaryOp::GtEq
        ) {
            // Create receiver with reference (trait methods take &self)
            let receiver_ref_type = type_table.intern(ResolvedType::Ref(left.type_id));
            let receiver = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(left.clone()),
                },
                receiver_ref_type,
                span,
            );

            // Create argument with reference (other: &Self)
            let arg_ref_type = type_table.intern(ResolvedType::Ref(right.type_id));
            let arg_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(right.clone()),
                },
                arg_ref_type,
                span,
            );

            // Get Ordering type for cmp return value
            let ordering_type_id = type_table.intern(ResolvedType::Enum {
                name: "Ordering".to_string(),
                module_source: ModuleSource::prelude(),
            });

            let method_info =
                LocalMethodName::new(base_struct_name, Some("Ord".to_string()), "cmp".to_string())
                    .with_struct_type_args(&impl_type_args);
            let mangled_name = method_info.to_mangled_name();

            // Resolve the module where the trait impl lives.
            let ord_method_module_source = self
                .functions
                .trait_method_locations
                .get(&mangled_name)
                .cloned()
                .or(type_module_source)
                .unwrap_or_else(|| self.current_module_source.clone());

            let cmp_call = TirExpr::new(
                TirExprKind::MethodCall {
                    receiver: Box::new(receiver),
                    func: FunctionRef {
                        module_source: ord_method_module_source,
                        name: mangled_name,
                        monomorph_info: None,
                        method_info: Some(method_info),
                        is_cm_binding: false,
                    },
                    type_args: vec![],
                    args: vec![CallArg::new(arg_ref, false)],
                },
                ordering_type_id,
                span,
            );

            // Determine comparison operator and Ordering variant:
            // < : cmp(a, b) == Ordering::Less
            // > : cmp(a, b) == Ordering::Greater
            // <= : cmp(a, b) != Ordering::Greater
            // >= : cmp(a, b) != Ordering::Less
            let (compare_op, case_name, case_index): (TirBinaryOp, &str, u32) = match op {
                TirBinaryOp::Lt => (TirBinaryOp::Eq, "Less", 0),
                TirBinaryOp::Gt => (TirBinaryOp::Eq, "Greater", 2),
                TirBinaryOp::LtEq => (TirBinaryOp::NotEq, "Greater", 2),
                TirBinaryOp::GtEq => (TirBinaryOp::NotEq, "Less", 0),
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
                span,
            );

            return Some(TirExprKind::Binary {
                op: compare_op,
                left: Box::new(cmp_call),
                right: Box::new(ordering_variant),
            });
        }

        None
    }
    /// Desugar comparison operators on non-primitive types in all functions.
    ///
    /// This is needed for non-generic functions (where `substitute_types_in_expr` is
    /// never called) that use `==`, `!=`, `<`, etc. on struct/variant types.
    /// Without this pass, those operators fall through to the codegen's `I32Eq` fallback,
    /// which is wrong for GC reference types (variants, structs with custom Eq).
    pub(super) fn desugar_comparisons_in_module(&self, module: &mut TirModule) {
        let type_table_rc = module.type_table.clone();
        let mut desugarer = ComparisonDesugarer {
            mono: self,
            type_table: &type_table_rc,
        };
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(mut body) = func.body.take() {
                desugarer.visit_block(&mut body);
                func.body = Some(body);
            }
        }
    }
}

struct CallRewriter<'a> {
    mono: &'a Monomorphizer,
    type_table: &'a TypeTable,
}

impl TirMutVisitor for CallRewriter<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        self.walk_stmt(stmt);
        // Update the Let's type_id if it was a type parameter that got substituted
        if let TirStmtKind::Let { value, type_id, .. } = &mut stmt.kind
            && self.type_table.contains_type_param(*type_id)
            && !self.type_table.contains_type_param(value.type_id)
        {
            *type_id = value.type_id;
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &expr.kind {
            TirExprKind::Call { .. } => {
                self.mono.rewrite_call_expr(expr, self.type_table);
            }
            TirExprKind::MethodCall { .. } => {
                self.mono.rewrite_method_call_expr(expr, self.type_table);
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

struct ComparisonDesugarer<'a> {
    mono: &'a Monomorphizer,
    type_table: &'a Rc<RefCell<TypeTable>>,
}

impl TirMutVisitor for ComparisonDesugarer<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // Recurse into children first, then desugar this node
        self.walk_expr(expr);

        if let TirExprKind::Binary { op, left, right } = &mut expr.kind
            && let Some(new_kind) = self.mono.try_desugar_comparison(
                expr.span,
                *op,
                left,
                right,
                &mut self.type_table.borrow_mut(),
            )
        {
            expr.kind = new_kind;
        }
    }
}
