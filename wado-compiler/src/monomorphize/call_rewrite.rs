//! Post-monomorphization rewrites: function call rewriting to monomorphized names.

use crate::name::MethodName;
use crate::name::LocalMethodName;
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBlock, TirExpr,
    TirExprKind, TirModule, TirStmt, TirStmtKind, TirTemplatePart, TypeId, TypeTable,
};
use crate::tir_visitor::TirMutVisitor;

use super::generic_function_key;
use super::state::Monomorphizer;

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
            func,
            type_args,
            args,
            ..
        } = &mut expr.kind
        {
            let original_func_name = func.name.clone();
            let original_method_info = func.method_info.clone();
            let qualified_func_name =
                generic_function_key(func.is_method(), &func.module_source, &func.name);

            // Resolve type_args: use explicit ones, or infer from args for implicit generic calls.
            // Only attempt inference for free functions (not method calls), since method calls
            // have their own rewriting logic below.
            let effective_type_args: Option<Vec<TypeId>> = if !type_args.is_empty() {
                Some(type_args.clone())
            } else if func.method_info.is_none() && func.monomorph_info.is_none() {
                self.infer_instantiated_type_args(&qualified_func_name, &original_method_info, args, type_table)
            } else {
                None
            };

            if let Some(inferred_args) = effective_type_args {
                let key = InstantiationKey {
                    name: qualified_func_name.clone(),
                    impl_type_args: vec![],
                    method_type_args: inferred_args,
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

    /// For implicit generic calls (where type_args is empty but the callee was instantiated),
    /// try to find the matching instantiation by checking all instantiated variants of the callee.
    fn infer_instantiated_type_args(
        &self,
        qualified_func_name: &str,
        method_info: &Option<LocalMethodName>,
        args: &[CallArg],
        type_table: &TypeTable,
    ) -> Option<Vec<TypeId>> {
        // Look for any instantiation whose name matches and whose arg types match
        for (key, _mangled) in &self.functions.instantiated {
            if key.name != qualified_func_name {
                continue;
            }
            if key.method_info != *method_info {
                continue;
            }
            if key.method_type_args.is_empty() {
                continue;
            }
            // Found a candidate. Check if it's the right one by verifying
            // that no type params remain in the args (concrete call).
            // For single-instantiation cases (the common case), this is sufficient.
            let all_args_concrete = args
                .iter()
                .all(|a| !type_table.contains_type_param(a.expr.type_id));
            if all_args_concrete {
                return Some(key.method_type_args.clone());
            }
        }
        None
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
