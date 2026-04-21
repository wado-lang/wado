//! Function instantiation: creating concrete functions from generic definitions,
//! collecting instantiation sites, variadic expansion, and method call resolution.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::{LocalMethodName, MethodName, ModuleSource, mangle_generic_name};
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBinaryOp, TirBlock,
    TirExpr, TirExprKind, TirFunction, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirTemplatePart, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use super::state::Monomorphizer;
use super::{generic_function_key, module_source_for_trait_impl};

/// Lower remaining comparison operators on non-primitive types in all module functions.
pub fn lower_comparisons_in_module(
    module: &mut TirModule,
    trait_method_locations: &IndexMap<String, ModuleSource>,
) {
    let type_table_rc = module.type_table.clone();
    let module_source = module.module_source.clone();

    struct ComparisonLowerer<'a> {
        trait_method_locations: &'a IndexMap<String, ModuleSource>,
        current_module_source: &'a ModuleSource,
        type_table: &'a std::rc::Rc<std::cell::RefCell<TypeTable>>,
    }

    impl TirMutVisitor for ComparisonLowerer<'_> {
        fn visit_expr(&mut self, expr: &mut TirExpr) {
            self.walk_expr(expr);

            if let TirExprKind::Binary { op, left, right } = &mut expr.kind
                && matches!(
                    *op,
                    TirBinaryOp::Eq
                        | TirBinaryOp::NotEq
                        | TirBinaryOp::Lt
                        | TirBinaryOp::Gt
                        | TirBinaryOp::LtEq
                        | TirBinaryOp::GtEq
                )
                && let Some(new_kind) = try_lower_comparison(
                    self.trait_method_locations,
                    self.current_module_source,
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

    let mut lowerer = ComparisonLowerer {
        trait_method_locations,
        current_module_source: &module_source,
        type_table: &type_table_rc,
    };

    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(mut body) = func.body.take() {
            lowerer.visit_block(&mut body);
            func.body = Some(body);
        }
    }

    for global in &mut module.globals {
        lowerer.visit_expr(&mut global.initializer);
    }
}

/// Collects function instantiation sites by traversing TIR with `TirRefVisitor`.
///
/// Replaces the manual recursive traversal in `collect_func_instantiation_sites_in_*`
/// with visitor-based traversal. The visitor's `walk_expr`/`walk_stmt` handles
/// recursion into all TIR node kinds automatically, so only Call and `MethodCall`
/// need custom handling.
pub(super) struct InstantiationCollector<'a> {
    pub mono: &'a mut Monomorphizer,
    pub generic_functions: &'a IndexMap<String, Rc<RefCell<TirFunction>>>,
    pub type_table: &'a TypeTable,
}

impl TirRefVisitor for InstantiationCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        // Handle Call and MethodCall instantiation logic
        self.mono.collect_func_instantiation_sites_in_expr(
            expr,
            self.generic_functions,
            self.type_table,
        );
        // Recurse into all sub-expressions via walk_expr
        self.walk_expr(expr);
    }
}

struct LocalIndexRewriter {
    old_idx: u32,
    new_idx: u32,
}

impl TirMutVisitor for LocalIndexRewriter {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Local { index, .. } = &mut expr.kind
            && *index == self.old_idx
        {
            *index = self.new_idx;
        }
        self.walk_expr(expr);
    }

    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        if let TirStmtKind::Let { local_index, .. } = &mut stmt.kind
            && *local_index == self.old_idx
        {
            *local_index = self.new_idx;
        }
        self.walk_stmt(stmt);
    }

    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        if let TirPattern::Binding { local_index, .. } = pattern
            && *local_index == self.old_idx
        {
            *local_index = self.new_idx;
        }
        self.walk_pattern(pattern);
    }
}

impl Monomorphizer {
    /// Collect function instantiation sites from Call/MethodCall expressions
    pub fn collect_function_instantiation_sites(
        &mut self,
        module: &TirModule,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    ) {
        let type_table = module.type_table.borrow();
        let mut collector = InstantiationCollector {
            mono: self,
            generic_functions,
            type_table: &type_table,
        };
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Skip generic functions - their bodies contain TypeParam references that
            // would incorrectly queue instantiations with TypeParam TypeIds instead of
            // concrete types. We only scan concrete functions; generic function bodies
            // are scanned after instantiation in Phase 9.
            // Effect-only params don't count as generic.
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                continue;
            }
            if let Some(body) = &func.body {
                collector.visit_block(body);
            }
        }

        // Also scan global variable initializers for function instantiation sites
        for global in &module.globals {
            collector.visit_expr(&global.initializer);
        }
    }

    pub fn collect_func_instantiation_sites_in_expr(
        &mut self,
        expr: &TirExpr,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
    ) {
        match &expr.kind {
            TirExprKind::Call {
                func,
                type_args,
                args,
                ..
            } => {
                let qualified_func_name =
                    generic_function_key(func.is_method(), &func.module_source, &func.name);
                // Check if this is a call to a generic function with explicit type args
                if !type_args.is_empty() && generic_functions.contains_key(&qualified_func_name) {
                    let key = InstantiationKey {
                        name: qualified_func_name,
                        module_source: func.module_source.clone(),
                        impl_type_args: vec![],
                        method_type_args: type_args.clone(),
                        method_info: func.method_info.clone(),
                    };
                    let mangled = self.function_instantiation_name(&key, type_table);
                    self.try_queue_function(key, mangled);
                }
                // Also check if this is a static method call on a monomorphized struct
                // (formerly StaticCall). Use method_info metadata to get struct/method name.
                if let FunctionRef {
                    method_info: Some(info),
                    monomorph_info: Some(monomorph),
                    ..
                } = func
                    && (!monomorph.impl_type_args.is_empty()
                        || !monomorph.method_type_args.is_empty())
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
                        if let Some(generic_func_rc) = generic_functions.get(&generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            // impl_type_args and method_type_args are now separate.
                            // For variadic impls, impl_type_args may be empty — extract
                            // from the struct name if needed.
                            let impl_type_args = if monomorph.impl_type_args.is_empty()
                                && !generic_func.impl_type_params.is_empty()
                                && info.struct_name != info.base_struct_name
                            {
                                type_table
                                    .find_type_args_by_mangled_name(&info.struct_name)
                                    .unwrap_or_default()
                            } else {
                                monomorph.impl_type_args.clone()
                            };
                            let method_type_args = monomorph.method_type_args.clone();
                            let total = impl_type_args.len() + method_type_args.len();
                            if total >= generic_func.impl_type_params.len() {
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name,
                                    module_source: func.module_source.clone(),
                                    impl_type_args: impl_type_args.clone(),
                                    method_type_args,
                                    method_info,
                                };
                                let mangled = self.method_instantiation_name(
                                    &key,
                                    type_table,
                                    impl_type_args.len(),
                                );
                                self.try_queue_function(key, mangled);
                            }
                            break;
                        }
                    }
                }
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        &arg.expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                type_args,
                args,
                ..
            } => {
                // Extract method name from method_info or fall back to function name
                let method_name = method_func
                    .method_info
                    .clone()
                    .map(|info| info.method_name)
                    .unwrap_or_else(|| method_func.name.clone());
                // Check if this is a method call with explicit type args
                if !type_args.is_empty() {
                    // Get the struct name from the receiver type
                    if let Some(struct_name) =
                        self.get_struct_name_from_type(receiver.type_id, type_table)
                    {
                        // Try both inherent method and trait method formats
                        let trait_name_opt = method_func
                            .method_info
                            .clone()
                            .and_then(|info| info.trait_name);
                        let mut names_to_try: Vec<(String, Option<String>)> = vec![(
                            MethodName::format_local(&struct_name, None, &method_name),
                            None,
                        )];
                        if let Some(ref tn) = trait_name_opt {
                            names_to_try.push((
                                MethodName::format_local(&struct_name, Some(tn), &method_name),
                                Some(tn.clone()),
                            ));
                        }

                        let mut found = false;
                        for (full_method_name, tn) in &names_to_try {
                            if let Some(gf) = generic_functions.get(full_method_name) {
                                let method_info =
                                    gf.borrow().method_info.clone().unwrap_or_else(|| {
                                        LocalMethodName::new(
                                            struct_name.clone(),
                                            tn.clone(),
                                            method_name.clone(),
                                        )
                                    });
                                let key = InstantiationKey {
                                    name: full_method_name.clone(),
                                    module_source: method_func.module_source.clone(),
                                    impl_type_args: vec![],
                                    method_type_args: type_args.clone(),
                                    method_info: Some(method_info),
                                };
                                let mangled = self.method_instantiation_name(
                                    &key, type_table,
                                    0, // non-generic struct: no impl type params
                                );
                                self.try_queue_function(key, mangled);
                                found = true;
                                break;
                            }
                        }
                        // Handle "double generics": method call with type_args on a monomorphized generic struct
                        // e.g., c.transform::<i64>(100) where c: Container<i32> and transform<U>
                        // Also handles GenericInstance receivers (e.g., Option<i32>)
                        if !found {
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
                                let mut dg_names: Vec<(String, Option<String>)> = vec![(
                                    MethodName::format_local(&base_struct, None, &method_name),
                                    None,
                                )];
                                if let Some(ref tn) = trait_name_opt {
                                    dg_names.push((
                                        MethodName::format_local(
                                            &base_struct,
                                            Some(tn),
                                            &method_name,
                                        ),
                                        Some(tn.clone()),
                                    ));
                                    // For ref-type impls, also try "&^Trait::method"
                                    if let Some(ref info) = method_func.method_info
                                        && info.base_struct_name != base_struct
                                    {
                                        dg_names.push((
                                            MethodName::format_local(
                                                &info.base_struct_name,
                                                Some(tn),
                                                &method_name,
                                            ),
                                            Some(tn.clone()),
                                        ));
                                    }
                                }

                                for (generic_method_name, tn) in &dg_names {
                                    if let Some(generic_func_rc) =
                                        generic_functions.get(generic_method_name)
                                    {
                                        let generic_func = generic_func_rc.borrow();
                                        if impl_type_args.len()
                                            >= generic_func.impl_type_params.len()
                                        {
                                            let method_info = LocalMethodName::new(
                                                base_struct,
                                                tn.clone(),
                                                method_name.clone(),
                                            );
                                            let key = InstantiationKey {
                                                name: generic_method_name.clone(),
                                                module_source: method_func.module_source.clone(),
                                                impl_type_args: impl_type_args.clone(),
                                                method_type_args: type_args.clone(),
                                                method_info: Some(method_info),
                                            };
                                            let mangled = self.method_instantiation_name(
                                                &key,
                                                type_table,
                                                impl_type_args.len(),
                                            );
                                            self.try_queue_function(key, mangled);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Also check if the receiver is a monomorphized generic struct
                // e.g., c.get() where c: Counter<i32>, or arr.push() where arr: Array<fn(i32)->i32>
                let struct_info = self.get_struct_info_from_type(receiver.type_id, type_table);
                if let Some((base_struct, impl_type_args)) = struct_info
                    && !impl_type_args.is_empty()
                {
                    // Try both regular method and trait method formats
                    // Method names to try: BaseStruct::method, BaseStruct^Trait::method (from method_info)
                    let mut names_to_try = Vec::new();
                    // For ref-type impls (e.g., impl Trait for &Array<T>),
                    // try the ref struct name FIRST so it takes priority
                    let is_ref_blanket_impl = if let Some(ref info) =
                        method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                        && info.base_struct_name != base_struct
                    {
                        names_to_try.push(MethodName::format_local(
                            &info.base_struct_name,
                            Some(trait_name),
                            &method_name,
                        ));
                        true
                    } else {
                        false
                    };
                    names_to_try.push(MethodName::format_local(&base_struct, None, &method_name));
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                    {
                        names_to_try.push(MethodName::format_local(
                            &base_struct,
                            Some(trait_name),
                            &method_name,
                        ));
                    }

                    for generic_method_name in &names_to_try {
                        if let Some(generic_func_rc) = generic_functions.get(generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            // For true ref blanket impls (e.g., impl<T> Inspect for &T),
                            // the impl_type_args should be the full inner type of the ref,
                            // not the inner struct's own type args.
                            // e.g., for &Array<i32>, we need [Array<i32>], not [i32].
                            //
                            // But for specific ref impls (e.g., impl<T> IntoIterator for &Array<T>),
                            // the impl_type_args from get_struct_info_from_type are correct as-is.
                            // Distinguish by checking the generic function's self param:
                            //   - &T blanket: self is &&TypeParam → for-type is TypeParam
                            //   - &Array<T>:  self is &&GenericInstance → for-type is GenericInstance
                            let effective_impl_type_args = if is_ref_blanket_impl
                                && generic_method_name == &names_to_try[0]
                            {
                                let is_true_ref_blanket = generic_func
                                    .params
                                    .first()
                                    .map(|p| {
                                        let mut t = p.type_id;
                                        for _ in 0..2 {
                                            match type_table.get(t) {
                                                ResolvedType::Ref(inner)
                                                | ResolvedType::MutRef(inner) => t = *inner,
                                                _ => break,
                                            }
                                        }
                                        matches!(type_table.get(t), ResolvedType::TypeParam { .. })
                                    })
                                    .unwrap_or(false);
                                if is_true_ref_blanket {
                                    match type_table.get(receiver.type_id) {
                                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                                            vec![*inner]
                                        }
                                        _ => impl_type_args.clone(),
                                    }
                                } else {
                                    impl_type_args.clone()
                                }
                            } else {
                                impl_type_args.clone()
                            };
                            // Check if method has its own type params (double generics)
                            let has_method_type_params = generic_func.has_real_type_params();
                            // Queue if we have at least enough impl type args.
                            // impl_type_args may be longer than impl_type_params when the impl
                            // fixes some struct type params to concrete types
                            // (e.g., `impl Trait for Foo<Array<String>, V>` where only V is free).
                            if effective_impl_type_args.len() >= generic_func.impl_type_params.len()
                            {
                                let method_type_args_for_key =
                                    if has_method_type_params && !type_args.is_empty() {
                                        type_args.clone()
                                    } else {
                                        vec![]
                                    };
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name.clone(),
                                    module_source: method_func.module_source.clone(),
                                    impl_type_args: effective_impl_type_args.clone(),
                                    method_type_args: method_type_args_for_key,
                                    method_info,
                                };
                                let mangled = self.method_instantiation_name(
                                    &key,
                                    type_table,
                                    effective_impl_type_args.len(),
                                );
                                self.try_queue_function(key, mangled);
                                break;
                            }
                        }
                    }
                }

                // Also handle already-monomorphized structs via reverse lookup
                // e.g., c.add(10) where c: Container<i32>
                // Use get_struct_name_from_type to properly unwrap reference types (&T, &mut T)
                if let Some(struct_name) =
                    self.get_struct_name_from_type(receiver.type_id, type_table)
                    && let Some(struct_key) = self.structs.mangled_to_key.get(&struct_name)
                {
                    let base_struct = &struct_key.name;
                    let impl_type_args = struct_key.impl_type_args.clone();

                    let mut names_to_try =
                        vec![MethodName::format_local(base_struct, None, &method_name)];
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                    {
                        names_to_try.push(MethodName::format_local(
                            base_struct,
                            Some(trait_name),
                            &method_name,
                        ));
                        // For ref-type impls (e.g., impl Trait for &Array<T>),
                        // the template function is registered under "&^Trait::method"
                        if info.base_struct_name != *base_struct {
                            names_to_try.push(MethodName::format_local(
                                &info.base_struct_name,
                                Some(trait_name),
                                &method_name,
                            ));
                        }
                    }

                    for generic_method_name in names_to_try {
                        if let Some(generic_func_rc) = generic_functions.get(&generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            let has_method_type_params = generic_func.has_real_type_params();
                            if impl_type_args.len() >= generic_func.impl_type_params.len() {
                                let method_type_args_for_key =
                                    if has_method_type_params && !type_args.is_empty() {
                                        type_args.clone()
                                    } else {
                                        vec![]
                                    };
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name.clone(),
                                    module_source: method_func.module_source.clone(),
                                    impl_type_args: impl_type_args.clone(),
                                    method_type_args: method_type_args_for_key,
                                    method_info,
                                };
                                let mangled = self.method_instantiation_name(
                                    &key,
                                    type_table,
                                    impl_type_args.len(),
                                );
                                self.try_queue_function(key, mangled);
                                break;
                            }
                        }
                    }
                }

                // Blanket impl fallback: if the FunctionRef has monomorph_info from a
                // blanket impl that matches a generic function template, queue the
                // instantiation using that template function.
                if let FunctionRef {
                    monomorph_info: Some(mono),
                    ..
                } = method_func
                    && mono.is_blanket
                    && let Some(generic_func_rc) = generic_functions.get(&mono.generic_name)
                {
                    let generic_func = generic_func_rc.borrow();
                    let method_info = generic_func.method_info.clone();
                    // impl_type_args and method_type_args are now separate in MonomorphInfo.
                    // For blanket impls, impl_type_args contains the concrete receiver type.
                    // method_type_args comes from the MethodCall's type_args field.
                    // If method_type_args is empty but the callee has type params, infer from args.
                    let impl_ta = mono.impl_type_args.clone();
                    let method_ta = if !type_args.is_empty() {
                        type_args.clone()
                    } else if mono.method_type_args.is_empty()
                        && generic_func.has_real_type_params()
                    {
                        // Infer method type args from argument types
                        let method_params = &generic_func.params;
                        let mut inferred_method_args = Vec::new();
                        for param in &generic_func.type_params {
                            let param_idx =
                                generic_func.impl_type_params.len() as u32 + param.index;
                            let mut inferred = None;
                            for (pi, mp) in method_params.iter().enumerate().skip(1) {
                                let inner = match type_table.get(mp.type_id) {
                                    ResolvedType::Ref(t) | ResolvedType::MutRef(t) => *t,
                                    _ => mp.type_id,
                                };
                                if matches!(type_table.get(inner), ResolvedType::TypeParam { index, .. } if *index == param_idx)
                                {
                                    let arg_idx = pi - 1;
                                    if let Some(arg) = args.get(arg_idx) {
                                        let mut arg_type = arg.expr.type_id;
                                        while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                            type_table.get(arg_type).clone()
                                        {
                                            arg_type = t;
                                        }
                                        if let ResolvedType::GenericInstance {
                                            name,
                                            type_args: ta,
                                            ..
                                        } = type_table.get(arg_type)
                                            && name == "Box"
                                            && ta.len() == 1
                                        {
                                            arg_type = ta[0];
                                        }
                                        inferred = Some(arg_type);
                                    }
                                    break;
                                }
                            }
                            if let Some(tid) = inferred {
                                inferred_method_args.push(tid);
                            }
                        }
                        inferred_method_args
                    } else {
                        mono.method_type_args.clone()
                    };
                    let key = InstantiationKey {
                        name: mono.generic_name.clone(),
                        module_source: method_func.module_source.clone(),
                        impl_type_args: impl_ta,
                        method_type_args: method_ta,
                        method_info,
                    };
                    let impl_type_params_count = generic_func.impl_type_params.len();
                    let mangled = self.method_instantiation_name_inner(
                        &key,
                        type_table,
                        impl_type_params_count,
                        &generic_func.impl_type_params,
                    );
                    self.try_queue_function(key, mangled);
                }

                // Tuple variadic impl: receiver is a tuple type, method is on "Tuple"
                // (e.g., Tuple^Eq::eq from variadic `impl<..T: Eq> Eq for [..T]`)
                // The resolver already creates monomorph_info with the generic name and
                // impl_type_args (the concrete tuple element types).
                // TODO: LocalMethodName.struct_name does not carry module_source,
                // so we cannot use TypeTable::is_tuple_type here. This is safe because
                // only built-in tuples use TUPLE_TYPE_NAME as struct_name in method info.
                if let Some(ref info) = method_func.method_info
                    && info.struct_name == TypeTable::TUPLE_TYPE_NAME
                {
                    let mono = method_func.monomorph_info.as_ref();
                    let generic_name = mono.map(|m| m.generic_name.clone()).unwrap_or_else(|| {
                        MethodName::format_local(
                            TypeTable::TUPLE_TYPE_NAME,
                            info.trait_name.as_deref(),
                            &info.method_name,
                        )
                    });
                    let impl_type_args =
                        mono.map(|m| m.impl_type_args.clone()).unwrap_or_else(|| {
                            let mut receiver_type = receiver.type_id;
                            while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                                type_table.get(receiver_type).clone()
                            {
                                receiver_type = inner;
                            }
                            type_table.as_tuple(receiver_type).unwrap_or_default()
                        });
                    {
                        if let Some(generic_func_rc) = generic_functions.get(&generic_name) {
                            let generic_func = generic_func_rc.borrow();
                            let method_info = generic_func.method_info.clone();
                            let impl_type_params_count = generic_func.impl_type_params.len();
                            let impl_type_params: Vec<crate::tir::TirTypeParam> =
                                generic_func.impl_type_params.clone();
                            drop(generic_func);
                            let key = InstantiationKey {
                                name: generic_name,
                                module_source: method_func.module_source.clone(),
                                impl_type_args,
                                method_type_args: vec![],
                                method_info,
                            };
                            let mangled = self.method_instantiation_name_inner(
                                &key,
                                type_table,
                                impl_type_params_count,
                                &impl_type_params,
                            );
                            self.try_queue_function(key, mangled);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Instantiate a generic function with concrete type arguments
    ///
    /// Note: `instantiate_function` is separate from `instantiate_method`
    pub fn instantiate_function(
        &mut self,
        generic: &TirFunction,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirFunction> {
        let mangled_name = self.functions.instantiated.get(key)?.clone();

        // Build substitution map: type param index -> concrete type
        // Include both method-level type params AND impl block type params
        let mut substitution: IndexMap<u32, TypeId> = IndexMap::default();

        // Add impl block type params from key.impl_type_args
        let non_pack_impl_params_count = generic
            .impl_type_params
            .iter()
            .filter(|p| !p.is_pack)
            .count();
        for param in &generic.impl_type_params {
            if param.is_pack {
                // Variadic pack: map the pack index to a tuple of the impl-level type args,
                // excluding non-pack impl params.
                let pack_args_count = key
                    .impl_type_args
                    .len()
                    .saturating_sub(non_pack_impl_params_count);
                let pack_args: Vec<TypeId> = key
                    .impl_type_args
                    .iter()
                    .take(pack_args_count)
                    .copied()
                    .collect();
                let pack_type = type_table.make_tuple(pack_args);
                substitution.insert(param.index, pack_type);
            } else if let Some(&arg) = key.impl_type_args.get(param.index as usize) {
                substitution.insert(param.index, arg);
            }
        }

        // Add method-level type params from key.method_type_args
        let offset = generic.impl_type_params.len() as u32;
        for (param, &arg) in generic.type_params.iter().zip(key.method_type_args.iter()) {
            substitution.insert(offset + param.index, arg);
        }

        // Substitute types in parameters
        let params: Vec<TirParam> = generic
            .params
            .iter()
            .map(|param| TirParam {
                name: param.name.clone(),
                type_id: self.substitute_type(param.type_id, &substitution, type_table),
                local_index: param.local_index,
                is_mut: param.is_mut,
                span: param.span,
                default_expr: None,
            })
            .collect();

        // Substitute return type
        let return_type = self.substitute_type(generic.return_type, &substitution, type_table);

        // Substitute types in local_types
        let mut local_types: Vec<TypeId> = generic
            .local_types
            .iter()
            .map(|&t| self.substitute_type(t, &substitution, type_table))
            .collect();

        // Clone and substitute types in body
        let mut local_count = generic.local_count;
        self.current_impl_type_param_count = generic.impl_type_params.len();
        self.current_impl_struct_name = generic
            .method_info
            .as_ref()
            .map(|info| info.base_struct_name.clone())
            .unwrap_or_default();
        let body = generic.body.as_ref().map(|b| {
            let mut new_body = b.clone();
            self.substitute_types_in_block(
                &mut new_body,
                &substitution,
                type_table,
                &mut local_count,
                &mut local_types,
            );
            // Fixup TypePackExpansion: allocate separate locals for each expanded element
            Self::fixup_pack_expansion_locals(&mut new_body, &mut local_count, &mut local_types);
            new_body
        });

        Some(TirFunction {
            module_source: key.module_source.clone(),
            is_async: generic.is_async,
            name: mangled_name,
            is_pub: generic.is_pub,
            is_export: generic.is_export, // Inherit from generic
            type_params: vec![],          // Concrete function has no type params
            impl_type_params: vec![],     // Already monomorphized, no impl type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                impl_type_args: key.impl_type_args.clone(),
                method_type_args: key.method_type_args.clone(),
                is_blanket: false,
            }),
            // Update method_info with mangled struct name including impl type args
            // and method type args (from the method's own type params)
            method_info: generic.method_info.as_ref().map(|info| {
                let impl_type_arg_names: Vec<String> = key
                    .impl_type_args
                    .iter()
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                let method_type_arg_names: Vec<String> = key
                    .method_type_args
                    .iter()
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                // Blanket impl: struct name IS the type param (e.g., "I").
                // Replace it with the concrete type name instead of appending type args.
                let is_blanket = generic
                    .impl_type_params
                    .iter()
                    .any(|p| p.name == info.base_struct_name);
                if is_blanket && !impl_type_arg_names.is_empty() {
                    let base = type_table.base_type_name(key.impl_type_args[0]);
                    info.with_substituted_struct_name(&impl_type_arg_names[0], &base)
                } else {
                    info.with_type_args(&impl_type_arg_names, &method_type_arg_names)
                }
            }),
            params,
            return_type,
            effects: generic.effects.clone(),
            stores: generic.stores.clone(),
            body,
            span: generic.span,
            local_count,
            local_types,
            address_taken_locals: generic.address_taken_locals.clone(),
            stores_aliased_locals: generic.stores_aliased_locals.clone(),
            // Scratch local fields - computed by lower phase (after monomorphization)
            is_cm_binding: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: generic.inline_hint,
            comp_features: generic.comp_features,
            export_name: generic.export_name.clone(),
            allocator_tag: generic.allocator_tag.clone(),
        })
    }

    /// Substitute type parameters in a block
    pub fn substitute_types_in_block(
        &self,
        block: &mut TirBlock,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        let has_variadic = block
            .stmts
            .iter()
            .any(|s| matches!(&s.kind, TirStmtKind::VariadicForOf { .. }));
        if has_variadic {
            let old_stmts = std::mem::take(&mut block.stmts);
            for mut stmt in old_stmts {
                if let TirStmtKind::VariadicForOf { .. } = &stmt.kind {
                    block.stmts.extend(self.expand_variadic_for_of(
                        &mut stmt,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    ));
                } else {
                    self.substitute_types_in_stmt(
                        &mut stmt,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                    block.stmts.push(stmt);
                }
            }
        } else {
            for stmt in &mut block.stmts {
                self.substitute_types_in_stmt(
                    stmt,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
        }
    }

    fn substitute_types_in_stmt(
        &self,
        stmt: &mut TirStmt,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                *type_id = self.substitute_type(*type_id, substitution, type_table);
                self.substitute_types_in_expr(
                    value,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirStmtKind::Expr(expr) => {
                self.substitute_types_in_expr(
                    expr,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.substitute_types_in_expr(
                        expr,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.substitute_types_in_expr(
                    condition,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                self.substitute_types_in_block(
                    then_block,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                if let Some(else_blk) = else_block {
                    self.substitute_types_in_block(
                        else_blk,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirStmtKind::Loop { body } => {
                self.substitute_types_in_block(
                    body,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.substitute_types_in_expr(
                        v,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.substitute_types_in_block(
                    block,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.substitute_types_in_expr(
                    scrutinee,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                self.substitute_types_in_pattern(pattern, substitution, type_table);
                self.substitute_types_in_block(
                    then_block,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                if let Some(else_blk) = else_block {
                    self.substitute_types_in_block(
                        else_blk,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.substitute_types_in_pattern(pattern, substitution, type_table);
                self.substitute_types_in_expr(
                    value,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded in substitute_types_in_block")
            }
        }
    }

    fn substitute_types_in_expr(
        &self,
        expr: &mut TirExpr,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        // Substitute the expression's own type
        expr.type_id = self.substitute_type(expr.type_id, substitution, type_table);

        // Recurse into sub-expressions
        match &mut expr.kind {
            TirExprKind::Call {
                func: call_func,
                type_args,
                args,
                ..
            } => {
                // Substitute type args themselves
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                // For static method calls (formerly StaticCall), also update the func name
                // by delegating to the StaticCall substitution logic below via a flag.
                let is_static_method = call_func.method_info.is_some();
                if is_static_method {
                    // Inline the StaticCall substitution logic
                    if !substitution.is_empty()
                        && let Some(info) = call_func.method_info.clone()
                    {
                        let old_func_name = call_func.name.clone();
                        let module_source = call_func.module_source.clone();

                        // Derive substituted type args from the callee's own monomorph_info.
                        // The callee's monomorph_info records which type params the callee
                        // uses (impl-level and method-level). We substitute through the
                        // outer function's substitution map to resolve any type params.
                        //
                        // This is the single source of truth for the callee's type args.
                        // We must NOT use the outer substitution map's entries directly
                        // as type args — those are the outer function's type params, not
                        // the callee's.
                        let (sub_impl_type_args, sub_method_type_args) = if let FunctionRef {
                            monomorph_info: Some(mi),
                            ..
                        } = &*call_func
                        {
                            (
                                mi.impl_type_args
                                    .iter()
                                    .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                    .collect::<Vec<_>>(),
                                mi.method_type_args
                                    .iter()
                                    .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                    .collect::<Vec<_>>(),
                            )
                        } else if self.current_impl_type_param_count > 0
                            && info.struct_name == info.base_struct_name
                            && info.base_struct_name == self.current_impl_struct_name
                        {
                            // The callee has no monomorph_info, but the outer function
                            // has impl-level type params AND the callee's struct matches
                            // the outer impl's struct (e.g., we're inside
                            // `impl<K,V> TreeMap<K,V>` and calling `TreeMap::new()`).
                            // The resolver didn't annotate the bare struct reference with
                            // type args, so we derive them from the outer substitution's
                            // impl-level entries.
                            let mut sorted_entries: Vec<_> = substitution.iter().collect();
                            sorted_entries.sort_by_key(|(idx, _)| **idx);
                            let impl_args: Vec<TypeId> = sorted_entries
                                .iter()
                                .take(self.current_impl_type_param_count)
                                .map(|(_, tid)| **tid)
                                .collect();
                            (impl_args, vec![])
                        } else {
                            (vec![], vec![])
                        };

                        // Build the new method info with substituted type names.
                        let mut new_info = if info.is_type_param_receiver {
                            // The struct name is a type parameter (e.g., T^Ord::cmp).
                            // Look up the concrete type from the outer substitution.
                            let mut sorted_entries: Vec<_> = substitution.iter().collect();
                            sorted_entries.sort_by_key(|(idx, _)| **idx);
                            if sorted_entries.is_empty() {
                                info.clone()
                            } else {
                                let concrete_tid = *sorted_entries[0].1;
                                let type_name = type_table.mangle_type_name(concrete_tid);
                                let base = type_table.base_type_name(concrete_tid);
                                info.with_substituted_struct_name(&type_name, &base)
                            }
                        } else {
                            // Apply the callee's own substituted type args.
                            let impl_names: Vec<String> = sub_impl_type_args
                                .iter()
                                .map(|&tid| type_table.mangle_type_name(tid))
                                .collect();
                            let method_names: Vec<String> = sub_method_type_args
                                .iter()
                                .map(|&tid| type_table.mangle_type_name(tid))
                                .collect();
                            if impl_names.is_empty() && method_names.is_empty() {
                                info.clone()
                            } else {
                                info.with_type_args(&impl_names, &method_names)
                            }
                        };
                        // For type param receivers, also update method type args if they
                        // contain type params that need substitution (e.g., R → FixedReader
                        // in a default trait method body calling Self::read::<R>(r)).
                        // The with_substituted_struct_name above only replaces the struct
                        // name, not the method type args.
                        if info.is_type_param_receiver
                            && let FunctionRef {
                                monomorph_info: Some(mi),
                                ..
                            } = &*call_func
                        {
                            let substituted_method_args: Vec<TypeId> = mi
                                .method_type_args
                                .iter()
                                .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                .collect();
                            let any_changed = substituted_method_args
                                .iter()
                                .zip(mi.method_type_args.iter())
                                .any(|(a, b)| a != b);
                            if any_changed {
                                new_info.method_type_args = substituted_method_args
                                    .iter()
                                    .map(|&tid| type_table.mangle_type_name(tid))
                                    .collect();
                            }
                        }
                        let new_func_name = new_info.to_mangled_name();

                        if new_func_name != old_func_name {
                            if info.is_type_param_receiver {
                                let mut sorted_entries: Vec<_> = substitution.iter().collect();
                                sorted_entries.sort_by_key(|(idx, _)| **idx);
                                let concrete_module = self
                                    .functions
                                    .trait_method_locations
                                    .get(&new_func_name)
                                    .cloned()
                                    .or_else(|| {
                                        let concrete_type_id = sorted_entries[0].1;
                                        module_source_for_trait_impl(type_table, *concrete_type_id)
                                    });
                                let new_monomorph = if new_info.method_type_args.is_empty() {
                                    None
                                } else {
                                    let base_info = LocalMethodName::new(
                                        new_info.base_struct_name.clone(),
                                        new_info.trait_name.clone(),
                                        new_info.method_name.clone(),
                                    );
                                    let generic_name = base_info.to_mangled_name();
                                    let method_type_arg_tids: Vec<TypeId> = if let FunctionRef {
                                        monomorph_info: Some(mi),
                                        ..
                                    } = &*call_func
                                    {
                                        mi.method_type_args
                                            .iter()
                                            .map(|&tid| {
                                                self.substitute_type(tid, substitution, type_table)
                                            })
                                            .collect()
                                    } else {
                                        Vec::new()
                                    };
                                    let concrete_type_id = *sorted_entries[0].1;
                                    let impl_type_arg_tids: Vec<TypeId> = type_table
                                        .generic_type_args(concrete_type_id)
                                        .unwrap_or_default();
                                    Some(MonomorphInfo {
                                        generic_name,
                                        impl_type_args: impl_type_arg_tids,
                                        method_type_args: method_type_arg_tids,
                                        is_blanket: false,
                                    })
                                };
                                // When the call still needs monomorphization (new_monomorph is Some),
                                // use the current module source because instantiate_function will
                                // add the concrete function to this module. When monomorphization
                                // is complete (None), use the concrete module where the impl lives.
                                let resolved_module = if new_monomorph.is_some() {
                                    self.current_module_source.clone()
                                } else {
                                    concrete_module.unwrap_or_else(|| module_source.clone())
                                };
                                *call_func = FunctionRef {
                                    module_source: resolved_module,
                                    name: new_func_name,
                                    monomorph_info: new_monomorph,
                                    method_info: Some(new_info),
                                };
                            } else {
                                let monomorph_info = Some(MonomorphInfo {
                                    generic_name: old_func_name,
                                    impl_type_args: sub_impl_type_args,
                                    method_type_args: sub_method_type_args,
                                    is_blanket: false,
                                });
                                *call_func = FunctionRef {
                                    module_source,
                                    name: new_func_name,
                                    monomorph_info,
                                    method_info: Some(new_info),
                                };
                            }
                        }
                    }
                }
                for arg in args {
                    self.substitute_types_in_expr(
                        &mut arg.expr,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                type_args,
                args,
                ..
            } => {
                self.substitute_types_in_expr(
                    receiver,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                for arg in &mut *args {
                    self.substitute_types_in_expr(
                        &mut arg.expr,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }

                // Capture trait info before substitution clears is_type_param_receiver
                let type_param_trait_info = method_func.method_info.as_ref().and_then(|i| {
                    if i.is_type_param_receiver {
                        Some((i.trait_name.clone(), i.method_name.clone()))
                    } else {
                        None
                    }
                });

                self.resolve_method_call_substitution(
                    method_func,
                    receiver.type_id,
                    substitution,
                    type_table,
                );

                // Primitives use direct Wasm instructions, not trait methods.
                // Convert back to a binary op when monomorphization concretized to a primitive.
                if let Some((trait_name_before, method_name_before)) = type_param_trait_info {
                    let mut recv_inner = receiver.type_id;
                    while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                        type_table.get(recv_inner).clone()
                    {
                        recv_inner = t;
                    }
                    if type_table.is_primitive_like(recv_inner)
                        && let Some(binary_op) = trait_method_to_binary_op(
                            trait_name_before.as_deref(),
                            &method_name_before,
                        )
                    {
                        // Unwrap receiver: &T -> T (strip the Ref wrapper)
                        let left = if let TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: inner,
                        } = &receiver.kind
                        {
                            (**inner).clone()
                        } else {
                            (**receiver).clone()
                        };
                        // Unwrap first arg: &T -> T
                        let right = if let Some(arg) = args.first()
                            && let TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: inner,
                            } = &arg.expr.kind
                        {
                            (**inner).clone()
                        } else if let Some(arg) = args.first() {
                            arg.expr.clone()
                        } else {
                            unreachable!()
                        };
                        let result_type =
                            if matches!(binary_op, TirBinaryOp::Eq | TirBinaryOp::NotEq) {
                                TypeTable::BOOL
                            } else {
                                left.type_id
                            };
                        expr.kind = TirExprKind::Binary {
                            op: binary_op,
                            left: Box::new(left),
                            right: Box::new(right),
                        };
                        expr.type_id = result_type;
                    }
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.substitute_types_in_expr(
                        arg,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::Binary { op, left, right } => {
                self.substitute_types_in_expr(
                    left,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                self.substitute_types_in_expr(
                    right,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                // After type substitution, comparison operators on non-primitive
                // types must be lowered to Eq::eq / Ord::cmp method calls.
                if matches!(
                    *op,
                    TirBinaryOp::Eq
                        | TirBinaryOp::NotEq
                        | TirBinaryOp::Lt
                        | TirBinaryOp::Gt
                        | TirBinaryOp::LtEq
                        | TirBinaryOp::GtEq
                ) && let Some(new_kind) = try_lower_comparison(
                    &self.functions.trait_method_locations,
                    &self.current_module_source,
                    expr.span,
                    *op,
                    left,
                    right,
                    type_table,
                ) {
                    expr.kind = new_kind;
                }
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.substitute_types_in_expr(
                    inner,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::Block(block) => {
                self.substitute_types_in_block(
                    block,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.substitute_types_in_expr(
                    condition,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                self.substitute_types_in_block(
                    then_branch,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                if let Some(else_blk) = else_branch {
                    self.substitute_types_in_block(
                        else_blk,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                // First pass: substitute types in all elements (skip TypePackExpansion —
                // those are expanded with per-element substitution in the second pass)
                for elem in elements.iter_mut() {
                    if !matches!(elem.kind, TirExprKind::TypePackExpansion { .. }) {
                        self.substitute_types_in_expr(
                            elem,
                            substitution,
                            type_table,
                            local_count,
                            local_types,
                        );
                    }
                }
                // Second pass: expand TupleSpread and TypePackExpansion nodes
                let has_expansion = elements.iter().any(|e| {
                    matches!(
                        e.kind,
                        TirExprKind::TupleSpread { .. } | TirExprKind::TypePackExpansion { .. }
                    )
                });
                if has_expansion {
                    let old_elements = std::mem::take(elements);
                    for elem in old_elements {
                        if let TirExprKind::TupleSpread { ref expr } = elem.kind {
                            if let Some(inner_elems) = type_table.as_tuple(expr.type_id) {
                                for (i, &elem_type) in inner_elems.iter().enumerate() {
                                    elements.push(TirExpr::new(
                                        TirExprKind::FieldAccess {
                                            expr: expr.clone(),
                                            field_index: i as u32,
                                            field_name: i.to_string(),
                                        },
                                        elem_type,
                                        elem.span,
                                    ));
                                }
                            } else {
                                // Single-type spread (not a tuple), keep as-is
                                elements.push(*expr.clone());
                            }
                        } else if let TirExprKind::TypePackExpansion {
                            ref call_expr,
                            pack_type_id,
                        } = elem.kind
                        {
                            // Expand type pack: for each concrete type in the pack,
                            // clone the expression and substitute with per-element types.
                            let pack_index = match type_table.get(pack_type_id) {
                                ResolvedType::TypePack { index, .. } => *index,
                                _ => 0,
                            };
                            let concrete_pack =
                                self.substitute_type(pack_type_id, substitution, type_table);
                            let pack_elems = type_table
                                .as_tuple(concrete_pack)
                                .unwrap_or_else(|| vec![concrete_pack]);
                            for &elem_type in &pack_elems {
                                let mut elem_call = call_expr.as_ref().clone();
                                // Per-element substitution: pack → single element type.
                                // This correctly rewrites the static call (T::method → i32::method)
                                // and the expression's own type (TypePack → i32).
                                let mut elem_sub = substitution.clone();
                                elem_sub.insert(pack_index, elem_type);
                                self.substitute_types_in_expr(
                                    &mut elem_call,
                                    &elem_sub,
                                    type_table,
                                    local_count,
                                    local_types,
                                );
                                // Fix up Return statements: the per-element substitution
                                // incorrectly maps pack types in return positions.
                                // Compute per-element-substituted return types from the
                                // original call_expr and replace with full-sub versions.
                                for wrong_type in
                                    Self::collect_return_value_types(call_expr.as_ref())
                                {
                                    let wrong =
                                        self.substitute_type(wrong_type, &elem_sub, type_table);
                                    let correct =
                                        self.substitute_type(wrong_type, substitution, type_table);
                                    if wrong != correct {
                                        Self::fixup_return_types_in_expr(
                                            &mut elem_call,
                                            wrong,
                                            correct,
                                            type_table,
                                        );
                                    }
                                }
                                elements.push(elem_call);
                            }
                        } else {
                            elements.push(elem);
                        }
                    }
                    // Rebuild the tuple type from expanded element types
                    let new_elem_types: Vec<TypeId> = elements.iter().map(|e| e.type_id).collect();
                    expr.type_id = type_table.make_tuple(new_elem_types);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.substitute_types_in_expr(
                    target,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                self.substitute_types_in_expr(
                    value,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                *target_type = self.substitute_type(*target_type, substitution, type_table);
                self.substitute_types_in_expr(
                    inner,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.substitute_types_in_expr(
                    inner,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::TupleSpread { expr: inner } => {
                self.substitute_types_in_expr(
                    inner,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::TupleZip { expr: zip_inner } => {
                self.substitute_types_in_expr(
                    zip_inner,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                // After substitution, expand to transposed TupleLiteral.
                // Inner expr type: [[A0, A1, ...], [B0, B1, ...], ...]
                // Result: [[A0, B0, ...], [A1, B1, ...], ...]
                let inner_expr = zip_inner.as_ref().clone();
                let span = expr.span;
                let outer_elems = match type_table.as_tuple(inner_expr.type_id) {
                    Some(elems) => elems,
                    None => return,
                };
                let inner_arities: Vec<Vec<TypeId>> = outer_elems
                    .iter()
                    .filter_map(|e| type_table.as_tuple(*e))
                    .collect();
                if inner_arities.is_empty() || inner_arities.len() != outer_elems.len() {
                    return;
                }
                let arity = inner_arities[0].len();
                let num_rows = outer_elems.len();
                let mut col_exprs = Vec::with_capacity(arity);
                for col in 0..arity {
                    let mut row_exprs = Vec::with_capacity(num_rows);
                    for (row, row_types) in inner_arities.iter().enumerate() {
                        let row_access = TirExpr::new(
                            TirExprKind::FieldAccess {
                                expr: Box::new(inner_expr.clone()),
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
                    let col_tuple_type = type_table.make_tuple(col_types);
                    col_exprs.push(TirExpr::new(
                        TirExprKind::TupleLiteral {
                            elements: row_exprs,
                        },
                        col_tuple_type,
                        span,
                    ));
                }
                // Compute the correct transposed type from the column tuple types
                let transposed_types: Vec<TypeId> = col_exprs.iter().map(|e| e.type_id).collect();
                let transposed_type = type_table.make_tuple(transposed_types);
                expr.kind = TirExprKind::TupleLiteral {
                    elements: col_exprs,
                };
                expr.type_id = transposed_type;
            }
            TirExprKind::TypePackExpansion {
                call_expr,
                pack_type_id,
            } => {
                // Don't substitute inside call_expr here — it's expanded in TupleLiteral.
                // But do substitute the pack_type_id so we can look it up later.
                *pack_type_id = self.substitute_type(*pack_type_id, substitution, type_table);
                // Note: call_expr substitution happens during TupleLiteral expansion
                // with per-element substitutions. We still need to handle it if somehow
                // encountered outside TupleLiteral context.
                self.substitute_types_in_expr(
                    call_expr,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::Index { expr: array, index } => {
                self.substitute_types_in_expr(
                    array,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                self.substitute_types_in_expr(
                    index,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.substitute_types_in_expr(
                    scrutinee,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                for arm in arms {
                    self.substitute_types_in_pattern(&mut arm.pattern, substitution, type_table);
                    if let Some(guard) = &mut arm.guard {
                        self.substitute_types_in_expr(
                            guard,
                            substitution,
                            type_table,
                            local_count,
                            local_types,
                        );
                    }
                    self.substitute_types_in_expr(
                        &mut arm.body,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::Closure {
                params,
                body,
                captures,
                ..
            } => {
                for (_, type_id) in params {
                    *type_id = self.substitute_type(*type_id, substitution, type_table);
                }
                for cap in captures {
                    cap.type_id = self.substitute_type(cap.type_id, substitution, type_table);
                }
                self.substitute_types_in_expr(
                    body,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                // First substitute field expressions (which will update expr.type_id)
                for field in fields {
                    self.substitute_types_in_expr(
                        &mut field.value,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }

                // Then substitute struct_type
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);

                // Important: expr.type_id has already been substituted (line 1605 above)
                // Use it to get the correct struct type and name
                // This handles the case where struct_type is a plain Struct but expr.type_id
                // has been properly substituted to the monomorphized version
                if expr.type_id != *struct_type {
                    *struct_type = expr.type_id;
                }

                // Update struct_name to match the (possibly monomorphized) struct_type
                match type_table.get(*struct_type) {
                    ResolvedType::Struct { name, .. } => {
                        struct_name.clone_from(name);
                    }
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        if type_args.is_empty() && !substitution.is_empty() {
                            // GenericInstance with empty type_args in a substitution context
                            // Build the name using the substitution map
                            let mut sorted_entries: Vec<_> = substitution.iter().collect();
                            sorted_entries.sort_by_key(|(idx, _)| **idx);
                            let args: Vec<String> = sorted_entries
                                .iter()
                                .map(|(_, tid)| type_table.mangle_type_name(**tid))
                                .collect();
                            *struct_name = mangle_generic_name(name, &args);
                        } else {
                            // For generic instances like Container<i32>, compute the mangled name
                            let args: Vec<String> = type_args
                                .iter()
                                .map(|arg| type_table.mangle_type_name(*arg))
                                .collect();
                            *struct_name = mangle_generic_name(name, &args);
                        }
                    }
                    _ => {}
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.substitute_types_in_expr(
                    callee,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                for arg in args {
                    self.substitute_types_in_expr(
                        arg,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::ClosureToCanonical {
                functor,
                target_fn_type,
                ..
            } => {
                self.substitute_types_in_expr(
                    functor,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                *target_fn_type = self.substitute_type(*target_fn_type, substitution, type_table);
            }
            TirExprKind::VariantConstruct {
                variant_type,
                payload,
                ..
            } => {
                *variant_type = self.substitute_type(*variant_type, substitution, type_table);
                let original_payload_type = payload.as_ref().map(|p| p.type_id);
                if let Some(payload_expr) = payload {
                    self.substitute_types_in_expr(
                        payload_expr,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
                // After substitution, if variant_type is still a bare Variant (from
                // generic library code), convert it to a GenericInstance using the
                // payload type as type arg (e.g., Option + &mut Node<String> → Option<&mut Node<String>>).
                // Only promote if the payload type was actually changed by substitution,
                // indicating the variant is generic. Non-generic variants like
                // `Shape { Circle(f64), Point }` have concrete payload types that aren't
                // affected by substitution and should NOT be promoted to GenericInstance.
                if let ResolvedType::Variant { ref name, .. } =
                    type_table.get(*variant_type).clone()
                    && let Some(payload_expr) = payload
                    && original_payload_type.is_some_and(|orig| orig != payload_expr.type_id)
                {
                    // Use make_option for Option to ensure canonical module_source
                    let new_id = if name == "Option" {
                        type_table.make_option(payload_expr.type_id)
                    } else {
                        let module_source = if let ResolvedType::Variant { module_source, .. } =
                            type_table.get(*variant_type)
                        {
                            module_source.clone()
                        } else {
                            unreachable!()
                        };
                        type_table.make_generic_instance(
                            name.clone(),
                            module_source,
                            vec![payload_expr.type_id],
                        )
                    };
                    *variant_type = new_id;
                    expr.type_id = new_id;
                }
                // Unit cases (None) will be handled by the translator's fallback
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.substitute_types_in_block(
                    block,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.substitute_types_in_expr(
                    value,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::VariantTag { expr } => {
                self.substitute_types_in_expr(
                    expr,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.substitute_types_in_expr(
                    expr,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            TirExprKind::VariantPayload {
                expr, payload_type, ..
            } => {
                self.substitute_types_in_expr(
                    expr,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.substitute_types_in_expr(
                    scrutinee,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                for arm in arms {
                    self.substitute_types_in_block(
                        arm,
                        substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
                self.substitute_types_in_block(
                    default,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
            }
            // Literals and other simple expressions
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::Local { .. } => {}
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.substitute_types_in_expr(
                            inner,
                            substitution,
                            type_table,
                            local_count,
                            local_types,
                        );
                    }
                }
            }
        }
    }

    /// Resolve method call name substitution during monomorphization.
    ///
    /// When a generic function body contains a method call like `Array<T>::len`,
    /// this resolves it to the concrete `Array<i32>::len` after type substitution.
    /// Handles three cases:
    /// 1. Type-param receiver (e.g., `T^Ord::cmp` → `i32^Ord::cmp`)
    /// 2. Generic struct receiver (e.g., `Array<T>::len` → `Array<i32>::len`)
    /// 3. Non-generic receiver (no change needed)
    fn resolve_method_call_substitution(
        &self,
        method_func: &mut FunctionRef,
        receiver_type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        if substitution.is_empty() {
            return;
        }
        let Some(info) = method_func.method_info.clone() else {
            return;
        };

        // Check if the struct actually needs type arg substitution.
        // Skip for non-generic structs (e.g., String::push_str from template strings)
        // that happen to appear inside a generic impl block.
        let has_explicit_type_params = info.struct_name != info.base_struct_name;
        let receiver_is_generic = {
            let mut base = receiver_type_id;
            while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                type_table.get(base).clone()
            {
                base = inner;
            }
            // Unwrap Newtype to check if the underlying base type is generic
            let effective = match type_table.get(base) {
                ResolvedType::Newtype { base_type, .. } => *base_type,
                _ => base,
            };
            matches!(
                type_table.get(effective),
                ResolvedType::GenericInstance { .. }
                    | ResolvedType::GenericResource { .. }
                    | ResolvedType::BuiltinArray(_)
                    | ResolvedType::Struct {
                        is_monomorphized: true,
                        ..
                    }
            )
        };
        let needs_struct_type_args =
            has_explicit_type_params || info.is_type_param_receiver || receiver_is_generic;

        let old_func_name = method_func.name.clone();
        let module_source = method_func.module_source.clone();

        // Build type args from substitution
        let mut sorted_entries: Vec<_> = substitution.iter().collect();
        sorted_entries.sort_by_key(|(idx, _)| **idx);
        let type_names: Vec<String> = sorted_entries
            .iter()
            .map(|(_, tid)| type_table.mangle_type_name(**tid))
            .collect();
        let type_args: Vec<TypeId> = sorted_entries.iter().map(|(_, tid)| **tid).collect();

        // Compute the new method info with concrete type names.
        // If the struct is a type param (e.g., T^Ord::cmp), substitute the struct
        // name directly instead of adding type args.
        let new_info = if info.is_type_param_receiver && !type_names.is_empty() {
            // Use the (already-substituted) receiver type to find the concrete name.
            let mut inner = receiver_type_id;
            while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) = type_table.get(inner).clone()
            {
                inner = t;
            }
            // For newtypes/flags: first try the newtype's own name (e.g., "Meters"),
            // then fall back to the base type name (e.g., "f64") if no direct impl exists.
            let own_mangled = type_table.mangle_type_name(inner);
            let own_base = type_table.base_type_name(inner);
            let candidate = info.with_substituted_struct_name(&own_mangled, &own_base);
            let candidate_name = candidate.to_mangled_name();
            if self
                .functions
                .trait_method_locations
                .contains_key(&candidate_name)
            {
                candidate
            } else {
                let mangled = type_table.mangle_type_name_resolving_newtypes(inner);
                let base = type_table.base_type_name(inner);
                info.with_substituted_struct_name(&mangled, &base)
            }
        } else if needs_struct_type_args {
            // Derive the struct name from the already-substituted receiver type.
            // Resolve through newtypes so that e.g. MyArray<i32>::len → Array<i32>::len.
            let mut recv_inner = receiver_type_id;
            while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                type_table.get(recv_inner).clone()
            {
                recv_inner = t;
            }
            let recv_mangled = type_table.mangle_type_name_resolving_newtypes(recv_inner);
            let recv_base = type_table.base_type_name(recv_inner);
            let mut new_info = info.with_substituted_struct_name(&recv_mangled, &recv_base);
            // For ref-type impls (e.g., impl IntoIterator for &Array<T>), preserve
            // the ref base_struct_name ("&" or "&mut") so that the monomorphizer
            // selects the correct generic function template
            // ("&^IntoIterator::into_iter" instead of "Array^IntoIterator::into_iter").
            if info.is_ref_impl {
                new_info.base_struct_name.clone_from(&info.base_struct_name);
            }
            new_info
        } else {
            info.clone()
        };
        let new_func_name = new_info.to_mangled_name();

        if new_func_name == old_func_name {
            return;
        }

        if info.is_type_param_receiver {
            // Type param receiver: redirect to a concrete method (e.g., T^Ord::cmp → i32^Ord::cmp)
            let concrete_module = self
                .functions
                .trait_method_locations
                .get(&new_func_name)
                .cloned()
                .or_else(|| {
                    let mut inner = receiver_type_id;
                    while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                        type_table.get(inner).clone()
                    {
                        inner = t;
                    }
                    module_source_for_trait_impl(type_table, inner)
                });

            // Determine if this is a blanket impl method.
            // - Direct concrete method: found in trait_method_locations → monomorph_info = None
            // - Generic impl method: receiver has type_args → handled by receiver scan → None
            // - Blanket impl method: neither → is_blanket = true
            let receiver_has_type_args = {
                let mut inner = receiver_type_id;
                while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                    type_table.get(inner).clone()
                {
                    inner = t;
                }
                matches!(
                    type_table.get(inner),
                    ResolvedType::GenericInstance {
                        type_args: args, ..
                    } if !args.is_empty()
                ) || matches!(type_table.get(inner), ResolvedType::BuiltinArray(_))
            };
            let monomorph_info = if self
                .functions
                .trait_method_locations
                .contains_key(&new_func_name)
                || receiver_has_type_args
            {
                None
            } else {
                // Blanket impl: choose the right generic_name for lookup.
                // For associated type projections (S::SeqSerializer^...), use new_func_name.
                // Otherwise, use old_func_name which preserves the type param name.
                let blanket_name = if old_func_name
                    .split('^')
                    .next()
                    .is_some_and(|struct_part| struct_part.contains("::"))
                {
                    new_func_name.clone()
                } else {
                    old_func_name
                };
                Some(MonomorphInfo {
                    generic_name: blanket_name,
                    impl_type_args: type_args,
                    method_type_args: vec![],
                    is_blanket: true,
                })
            };
            *method_func = FunctionRef {
                module_source: concrete_module.unwrap_or_else(|| module_source.clone()),
                name: new_func_name,
                monomorph_info,
                method_info: Some(new_info),
            };
        } else {
            // Normal monomorphization (e.g., Array<T>::len → Array<i32>::len)
            let (existing_generic_name, existing_impl_ta, existing_method_ta, existing_is_blanket) =
                match method_func {
                    FunctionRef {
                        monomorph_info: Some(mi),
                        ..
                    } => (
                        Some(mi.generic_name.clone()),
                        Some(mi.impl_type_args.clone()),
                        Some(mi.method_type_args.clone()),
                        mi.is_blanket,
                    ),
                    _ => (None, None, None, false),
                };
            // For blanket impl calls, substitute the existing type_args rather than
            // building from the enclosing substitution map.
            let final_impl_ta = if existing_is_blanket {
                if let Some(args) = existing_impl_ta {
                    args.iter()
                        .map(|&tid| self.substitute_type(tid, substitution, type_table))
                        .collect()
                } else {
                    type_args
                }
            } else {
                type_args
            };
            let final_method_ta = existing_method_ta.unwrap_or_default();
            let monomorph_info = Some(MonomorphInfo {
                generic_name: existing_generic_name.unwrap_or(old_func_name),
                impl_type_args: final_impl_ta,
                method_type_args: final_method_ta,
                is_blanket: existing_is_blanket,
            });
            *method_func = FunctionRef {
                module_source,
                name: new_func_name,
                monomorph_info,
                method_info: Some(new_info),
            };
        }
    }

    /// Expand a `VariadicForOf` TIR node into concrete unrolled blocks.
    ///
    /// After type substitution resolves `TypePack` to a concrete tuple, this generates
    /// the same structure as the resolver's `resolve_tuple_for_of`.
    fn expand_variadic_for_of(
        &self,
        stmt: &mut TirStmt,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) -> Vec<TirStmt> {
        let span = stmt.span;
        let TirStmtKind::VariadicForOf {
            iterable,
            binding_name,
            binding_local,
            is_mut,
            body,
            unique_id,
        } = &mut stmt.kind
        else {
            unreachable!()
        };

        // Substitute types in the iterable to get the concrete tuple type
        self.substitute_types_in_expr(iterable, substitution, type_table, local_count, local_types);

        // Get the concrete tuple elements
        let iterable_type = iterable.type_id;
        let elements = type_table.as_tuple(iterable_type).unwrap_or_else(|| {
            panic!(
                "VariadicForOf: expected concrete Tuple after substitution, got {:?}",
                type_table.get(iterable_type)
            );
        });

        // Find the TypePack index in the substitution map so we can override it per element
        let pack_index = {
            let mut found = None;
            for (&idx, &tid) in substitution {
                if tid == iterable_type || type_table.is_tuple(tid) {
                    found = Some(idx);
                    break;
                }
            }
            found
        };

        let uid = *unique_id;
        let temp_name = format!("__tuple_{uid}");
        let binding_local_idx = *binding_local;
        let b_name = binding_name.clone();
        let b_mut = *is_mut;

        // Allocate a dedicated temp local for the tuple (the original binding_local
        // will be reused per-iteration below with distinct types per element).
        let temp_local_idx = *local_count;
        *local_count += 1;
        local_types.push(iterable_type);

        let mut outer_stmts = Vec::new();

        // let __tuple_N = iterable;
        outer_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: temp_name.clone(),
                local_index: temp_local_idx,
                is_mut: false,
                is_reactive: false,
                type_id: iterable_type,
                value: iterable.clone(),
                skip_value_copy: false,
            },
            span,
        ));

        // Track locals defined across iterations to detect conflicts
        let mut seen_locals: IndexSet<u32> = IndexSet::default();

        // For each element, create: { let v = __tuple_N.i; body }
        for (i, &elem_type) in elements.iter().enumerate() {
            let mut iter_stmts = Vec::new();

            // Allocate a unique binding local per iteration (each element has a different type)
            let iter_binding = *local_count;
            *local_count += 1;
            local_types.push(elem_type);

            let tuple_ref = TirExpr::new(
                TirExprKind::Local {
                    index: temp_local_idx,
                    name: temp_name.clone(),
                },
                iterable_type,
                span,
            );
            let field = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(tuple_ref),
                    field_index: i as u32,
                    field_name: i.to_string(),
                },
                elem_type,
                span,
            );

            iter_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: b_name.clone(),
                    local_index: iter_binding,
                    is_mut: b_mut,
                    is_reactive: false,
                    type_id: elem_type,
                    value: field,
                    skip_value_copy: false,
                },
                span,
            ));

            // Clone the body and substitute types with per-element substitution.
            // Override the TypePack → elem_type (instead of TypePack → tuple type)
            let mut elem_body = body.clone();
            if let Some(pack_idx) = pack_index {
                // Detect destructured zip patterns: body starts with Let stmts
                // whose values are FieldAccess on the binding local
                // (e.g., `let a = binding.0; let b = binding.1;`).
                let destruct_count = body
                    .stmts
                    .iter()
                    .take_while(|s| {
                        if let TirStmtKind::Let { value, .. } = &s.kind
                            && let TirExprKind::FieldAccess { expr: inner, .. } = &value.kind
                            && let TirExprKind::Local { index, .. } = &inner.kind
                        {
                            return *index == binding_local_idx;
                        }
                        false
                    })
                    .count();

                if destruct_count > 0 {
                    // For destructured zip patterns, substitute_type's Tuple splicing
                    // corrupts the pair type Tuple([TypePack, TypePack]) → Tuple([T0,T1,...,T0,T1,...]).
                    // Instead, generate fresh destructured Let stmts with correct field types
                    // from elem_type, and only substitute the remaining user body stmts.
                    let pair_fields = type_table
                        .as_tuple(elem_type)
                        .unwrap_or_else(|| vec![elem_type]);

                    // The body_pack_type is the individual field type (e.g., [i32, i32] → i32).
                    // For non-tuple field types this is just the field type itself.
                    let body_pack_type = pair_fields[0];

                    // Replace the destructured stmts with fresh ones that have correct types.
                    let mut fresh_destruct_stmts = Vec::with_capacity(destruct_count);
                    for (j, orig_stmt) in elem_body.stmts[..destruct_count].iter().enumerate() {
                        if let TirStmtKind::Let {
                            name,
                            local_index,
                            is_mut,
                            is_reactive,
                            value: orig_value,
                            ..
                        } = &orig_stmt.kind
                        {
                            let field_type = pair_fields.get(j).copied().unwrap_or(body_pack_type);
                            // Update local_types for this local
                            let idx = *local_index as usize;
                            if idx < local_types.len() {
                                local_types[idx] = field_type;
                            }
                            let binding_ref = TirExpr::new(
                                TirExprKind::Local {
                                    index: binding_local_idx,
                                    name: b_name.clone(),
                                },
                                elem_type,
                                span,
                            );
                            let field_access = TirExpr::new(
                                TirExprKind::FieldAccess {
                                    expr: Box::new(binding_ref),
                                    field_index: if let TirExprKind::FieldAccess {
                                        field_index,
                                        ..
                                    } = &orig_value.kind
                                    {
                                        *field_index
                                    } else {
                                        j as u32
                                    },
                                    field_name: j.to_string(),
                                },
                                field_type,
                                span,
                            );
                            fresh_destruct_stmts.push(TirStmt::new(
                                TirStmtKind::Let {
                                    name: name.clone(),
                                    local_index: *local_index,
                                    is_mut: *is_mut,
                                    is_reactive: *is_reactive,
                                    type_id: field_type,
                                    value: field_access,
                                    skip_value_copy: false,
                                },
                                span,
                            ));
                        }
                    }

                    // Extract only the user body stmts (after destructuring)
                    let user_stmts: Vec<TirStmt> =
                        elem_body.stmts.drain(destruct_count..).collect();
                    let mut user_body = TirBlock::new(user_stmts, span);

                    // Substitute user body with field type (not pair type)
                    let mut elem_substitution = substitution.clone();
                    elem_substitution.insert(pack_idx, body_pack_type);
                    self.substitute_types_in_block(
                        &mut user_body,
                        &elem_substitution,
                        type_table,
                        local_count,
                        local_types,
                    );

                    // Reassemble: fresh destructured stmts + substituted user body
                    elem_body.stmts = fresh_destruct_stmts;
                    elem_body.stmts.extend(user_body.stmts);
                } else {
                    let mut elem_substitution = substitution.clone();
                    elem_substitution.insert(pack_idx, elem_type);
                    self.substitute_types_in_block(
                        &mut elem_body,
                        &elem_substitution,
                        type_table,
                        local_count,
                        local_types,
                    );
                }
            } else {
                // Fallback: substitute with original and rewrite manually
                self.substitute_types_in_block(
                    &mut elem_body,
                    substitution,
                    type_table,
                    local_count,
                    local_types,
                );
                self.rewrite_variadic_binding_types(
                    &mut elem_body,
                    binding_local_idx,
                    elem_type,
                    type_table,
                );
            }
            // Populate type_args on MethodCall nodes that have inferred generic params.
            // Inside variadic for-of, method calls like `seq.element(&v)` have empty type_args
            // because T was inferred from a TypePack at resolution time. Now that types are
            // concrete, fill in type_args so the monomorphizer can instantiate the generic method.
            Self::infer_method_call_type_args(&mut elem_body, type_table);

            // Rewrite binding_local_idx → iter_binding in the body
            for s in &mut elem_body.stmts {
                Self::rewrite_local_index_in_stmt(s, binding_local_idx, iter_binding);
            }

            // Reallocate body locals that conflict with previous iterations.
            // This handles temporaries from `?` expansion (__qm_v, __qm_e) and other
            // locals that share the same index across cloned iteration bodies.
            let mut body_locals: Vec<u32> = Vec::new();
            Self::collect_locals_in_block(&elem_body, &mut body_locals);
            for body_local in body_locals {
                if !seen_locals.insert(body_local) {
                    // This local was already used by a previous iteration — reallocate
                    let new_idx = *local_count;
                    *local_count += 1;
                    let body_local_type = Self::find_local_type_in_block(&elem_body, body_local)
                        .unwrap_or(
                            local_types
                                .get(body_local as usize)
                                .copied()
                                .unwrap_or(TypeTable::UNIT),
                        );
                    local_types.push(body_local_type);
                    for s in &mut elem_body.stmts {
                        Self::rewrite_local_index_in_stmt(s, body_local, new_idx);
                    }
                }
            }

            iter_stmts.extend(elem_body.stmts);

            let label = format!("__tuple_iter_{uid}_{i}");
            outer_stmts.push(TirStmt::new(
                TirStmtKind::LabeledBlock {
                    label,
                    block: TirBlock::new(iter_stmts, span),
                },
                span,
            ));
        }

        let outer_label = format!("__tuple_for_of_{uid}");
        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: outer_label,
                block: TirBlock::new(outer_stmts, span),
            },
            span,
        )]
    }

    /// After variadic for-of expansion, method calls that had inferred type params
    /// (empty `type_args`) need their `type_args` populated from the concrete argument types.
    /// e.g., `seq.element(&v)` where element<T: Serialize> — infer T from the arg type.
    fn infer_method_call_type_args(block: &mut TirBlock, type_table: &TypeTable) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                TirStmtKind::Expr(expr) => {
                    Self::infer_method_call_type_args_in_expr(expr, type_table);
                }
                TirStmtKind::Let { value, .. } => {
                    Self::infer_method_call_type_args_in_expr(value, type_table);
                }
                TirStmtKind::LabeledBlock { block, .. } => {
                    Self::infer_method_call_type_args(block, type_table);
                }
                TirStmtKind::If {
                    condition,
                    then_block,
                    else_block,
                    ..
                } => {
                    Self::infer_method_call_type_args_in_expr(condition, type_table);
                    Self::infer_method_call_type_args(then_block, type_table);
                    if let Some(eb) = else_block {
                        Self::infer_method_call_type_args(eb, type_table);
                    }
                }
                _ => {}
            }
        }
    }

    fn infer_method_call_type_args_in_expr(expr: &mut TirExpr, type_table: &TypeTable) {
        match &mut expr.kind {
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => {
                Self::infer_method_call_type_args_in_expr(receiver, type_table);
                for arg in args.iter_mut() {
                    Self::infer_method_call_type_args_in_expr(&mut arg.expr, type_table);
                }
                // If type_args is empty and the method has monomorph_info or method_info
                // suggesting it's generic, infer type_args from argument types.
                if type_args.is_empty()
                    && let Some(ref info) = func.method_info
                {
                    // Check if method_type_args is empty (meaning inferred type args)
                    if info.method_type_args.is_empty() {
                        // Try to infer from first non-self arg's inner type.
                        // For element<T: Serialize>(&mut self, value: &T), the first
                        // arg is &T, so T is the inner type of the first arg.
                        if let Some(first_arg) = args.first() {
                            let mut arg_type = first_arg.expr.type_id;
                            // Unwrap references
                            while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                type_table.get(arg_type).clone()
                            {
                                arg_type = t;
                            }
                            // Unwrap Box<T> (auto-boxed primitive ref)
                            if let ResolvedType::GenericInstance {
                                name,
                                type_args: ta,
                                ..
                            } = type_table.get(arg_type)
                                && name == "Box"
                                && ta.len() == 1
                            {
                                arg_type = ta[0];
                            }
                            // Only set if arg_type is concrete (not a type param)
                            // AND not already present in monomorph_info.type_args.
                            // If it's already there, it's an impl type arg (e.g.,
                            // Array<String>::push where T=String comes from the
                            // impl, not a method-level type param). Adding it again
                            // would cause double-counting in instantiation.
                            let is_concrete = !matches!(
                                type_table.get(arg_type),
                                ResolvedType::TypeParam { .. }
                                    | ResolvedType::TypePack { .. }
                                    | ResolvedType::Unknown
                            );
                            // Check if the inferred type is already an impl type arg
                            // of the receiver's struct. If so, it's not a method-level
                            // type arg and should not be added (to avoid double-counting
                            // in instantiation). e.g., Array<String>::push infers
                            // String from the arg, but String is the impl type arg.
                            let receiver_impl_type_args = {
                                let mut base = receiver.type_id;
                                while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                    type_table.get(base).clone()
                                {
                                    base = t;
                                }
                                match type_table.get(base) {
                                    ResolvedType::GenericInstance { type_args: ta, .. } => {
                                        ta.clone()
                                    }
                                    ResolvedType::BuiltinArray(elem) => vec![*elem],
                                    _ => vec![],
                                }
                            };
                            let is_impl_type_arg = receiver_impl_type_args.contains(&arg_type);
                            if is_concrete && !is_impl_type_arg {
                                type_args.push(arg_type);
                            }
                        }
                    }
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::infer_method_call_type_args(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::infer_method_call_type_args_in_expr(condition, type_table);
                Self::infer_method_call_type_args(then_branch, type_table);
                if let Some(eb) = else_branch {
                    Self::infer_method_call_type_args(eb, type_table);
                }
            }
            _ => {}
        }
    }

    /// Rewrite types in the body of a variadic for-of iteration.
    fn rewrite_variadic_binding_types(
        &self,
        block: &mut TirBlock,
        binding_local: u32,
        elem_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        for stmt in &mut block.stmts {
            self.rewrite_variadic_types_in_stmt(stmt, binding_local, elem_type, type_table);
        }
    }

    fn rewrite_variadic_types_in_stmt(
        &self,
        stmt: &mut TirStmt,
        binding_local: u32,
        elem_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.rewrite_variadic_types_in_expr(value, binding_local, elem_type, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.rewrite_variadic_types_in_expr(expr, binding_local, elem_type, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.rewrite_variadic_types_in_expr(expr, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.rewrite_variadic_types_in_expr(
                    condition,
                    binding_local,
                    elem_type,
                    type_table,
                );
                self.rewrite_variadic_binding_types(
                    then_block,
                    binding_local,
                    elem_type,
                    type_table,
                );
                if let Some(eb) = else_block {
                    self.rewrite_variadic_binding_types(eb, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.rewrite_variadic_binding_types(body, binding_local, elem_type, type_table);
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                self.rewrite_variadic_binding_types(block, binding_local, elem_type, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.rewrite_variadic_types_in_expr(
                    scrutinee,
                    binding_local,
                    elem_type,
                    type_table,
                );
                self.rewrite_variadic_binding_types(
                    then_block,
                    binding_local,
                    elem_type,
                    type_table,
                );
                if let Some(eb) = else_block {
                    self.rewrite_variadic_binding_types(eb, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.rewrite_variadic_types_in_expr(value, binding_local, elem_type, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.rewrite_variadic_types_in_expr(v, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::Continue
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::VariadicForOf { .. } => {}
        }
    }

    fn rewrite_variadic_types_in_expr(
        &self,
        expr: &mut TirExpr,
        binding_local: u32,
        elem_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } => {
                if *index == binding_local {
                    expr.type_id = elem_type;
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                self.rewrite_variadic_types_in_expr(receiver, binding_local, elem_type, type_table);
                for arg in args.iter_mut() {
                    self.rewrite_variadic_types_in_expr(
                        &mut arg.expr,
                        binding_local,
                        elem_type,
                        type_table,
                    );
                }
                // If the receiver uses the binding local, update the method's struct name
                if Self::expr_uses_local(receiver, binding_local)
                    && let Some(info) = &mut func.method_info
                {
                    let type_name = type_table.mangle_type_name(elem_type);
                    let base_name = type_table.base_type_name(elem_type);
                    *info = info.with_substituted_struct_name(&type_name, &base_name);
                    func.name = info.to_mangled_name();
                    if let Some(ms) = module_source_for_trait_impl(type_table, elem_type) {
                        func.module_source = ms;
                    }
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in args.iter_mut() {
                    self.rewrite_variadic_types_in_expr(
                        &mut arg.expr,
                        binding_local,
                        elem_type,
                        type_table,
                    );
                }
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => {
                self.rewrite_variadic_types_in_expr(inner, binding_local, elem_type, type_table);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.rewrite_variadic_binding_types(block, binding_local, elem_type, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.rewrite_variadic_types_in_expr(
                    condition,
                    binding_local,
                    elem_type,
                    type_table,
                );
                self.rewrite_variadic_binding_types(
                    then_branch,
                    binding_local,
                    elem_type,
                    type_table,
                );
                if let Some(eb) = else_branch {
                    self.rewrite_variadic_binding_types(eb, binding_local, elem_type, type_table);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.rewrite_variadic_types_in_expr(left, binding_local, elem_type, type_table);
                self.rewrite_variadic_types_in_expr(right, binding_local, elem_type, type_table);
            }
            TirExprKind::Assign { target, value } => {
                self.rewrite_variadic_types_in_expr(target, binding_local, elem_type, type_table);
                self.rewrite_variadic_types_in_expr(value, binding_local, elem_type, type_table);
            }
            TirExprKind::Block(block) => {
                self.rewrite_variadic_binding_types(block, binding_local, elem_type, type_table);
            }
            _ => {
                // Literals, FuncRef, GlobalVarGet, etc. — no rewriting needed
            }
        }
    }

    fn expr_uses_local(expr: &TirExpr, local_index: u32) -> bool {
        match &expr.kind {
            TirExprKind::Local { index, .. } => *index == local_index,
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => Self::expr_uses_local(inner, local_index),
            TirExprKind::StructLiteral { fields, .. } => fields
                .iter()
                .any(|f| Self::expr_uses_local(&f.value, local_index)),
            _ => false,
        }
    }

    /// Fix up local variable indices in expanded `TypePackExpansion` elements.
    ///
    /// When `[..T::method()?]` is expanded, the `?` operator's match expression
    /// creates local variables (`__qm_v`, `__qm_e`). All expanded elements share
    /// the same local indices but need different types. This allocates new locals
    /// for each element to avoid type conflicts.
    fn fixup_pack_expansion_locals(
        block: &mut TirBlock,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        for stmt in &mut block.stmts {
            Self::fixup_pack_expansion_locals_in_stmt(stmt, local_count, local_types);
        }
    }

    fn fixup_pack_expansion_locals_in_stmt(
        stmt: &mut TirStmt,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                Self::fixup_pack_expansion_locals_in_expr(expr, local_count, local_types);
            }
            TirStmtKind::Let { value, .. } => {
                Self::fixup_pack_expansion_locals_in_expr(value, local_count, local_types);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::fixup_pack_expansion_locals_in_expr(condition, local_count, local_types);
                Self::fixup_pack_expansion_locals(then_block, local_count, local_types);
                if let Some(eb) = else_block {
                    Self::fixup_pack_expansion_locals(eb, local_count, local_types);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                Self::fixup_pack_expansion_locals(body, local_count, local_types);
            }
            _ => {}
        }
    }

    fn fixup_pack_expansion_locals_in_expr(
        expr: &mut TirExpr,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        match &mut expr.kind {
            TirExprKind::TupleLiteral { elements } if elements.len() > 1 => {
                // Collect local definitions from each element.
                // If multiple elements define the same local, allocate new locals.
                let mut first_seen_locals: IndexSet<u32> = IndexSet::default();
                for (elem_idx, elem) in elements.iter_mut().enumerate() {
                    let mut locals_in_elem: Vec<u32> = Vec::new();
                    Self::collect_locals_in_expr(elem, &mut locals_in_elem);
                    if elem_idx == 0 {
                        // Update local_types for element 0's locals from expression types
                        for &local_idx in &locals_in_elem {
                            if let Some(correct_type) =
                                Self::find_local_type_in_expr(elem, local_idx)
                                && let Some(entry) = local_types.get_mut(local_idx as usize)
                            {
                                *entry = correct_type;
                            }
                        }
                        first_seen_locals.extend(locals_in_elem);
                    } else {
                        // Reallocate locals shared with previous elements;
                        // for new locals, update local_types from the expression's
                        // actual types (pattern bindings have correct per-element types
                        // but local_types may have wrong types from pack substitution).
                        let mut new_locals: Vec<u32> = Vec::new();
                        for old_idx in &locals_in_elem {
                            if first_seen_locals.contains(old_idx) {
                                let new_idx = *local_count;
                                *local_count += 1;
                                let local_type = Self::find_local_type_in_expr(elem, *old_idx)
                                    .unwrap_or(
                                        local_types
                                            .get(*old_idx as usize)
                                            .copied()
                                            .unwrap_or(TypeTable::UNIT),
                                    );
                                local_types.push(local_type);
                                Self::rewrite_local_index_in_expr(elem, *old_idx, new_idx);
                            } else {
                                if let Some(correct_type) =
                                    Self::find_local_type_in_expr(elem, *old_idx)
                                    && let Some(entry) = local_types.get_mut(*old_idx as usize)
                                {
                                    *entry = correct_type;
                                }
                                new_locals.push(*old_idx);
                            }
                        }
                        first_seen_locals.extend(new_locals);
                    }
                }
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    Self::fixup_pack_expansion_locals_in_expr(
                        &mut arg.expr,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::fixup_pack_expansion_locals(block, local_count, local_types);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::fixup_pack_expansion_locals_in_expr(scrutinee, local_count, local_types);
                for arm in arms {
                    Self::fixup_pack_expansion_locals_in_expr(
                        &mut arm.body,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            }
            | TirExprKind::FieldAccess { expr: p, .. }
            | TirExprKind::Cast { expr: p, .. }
            | TirExprKind::Unary { expr: p, .. } => {
                Self::fixup_pack_expansion_locals_in_expr(p, local_count, local_types);
            }
            TirExprKind::Binary { left, right, .. }
            | TirExprKind::Assign {
                target: left,
                value: right,
            } => {
                Self::fixup_pack_expansion_locals_in_expr(left, local_count, local_types);
                Self::fixup_pack_expansion_locals_in_expr(right, local_count, local_types);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    Self::fixup_pack_expansion_locals_in_expr(
                        &mut f.value,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::fixup_pack_expansion_locals_in_expr(condition, local_count, local_types);
                Self::fixup_pack_expansion_locals(then_branch, local_count, local_types);
                if let Some(eb) = else_branch {
                    Self::fixup_pack_expansion_locals(eb, local_count, local_types);
                }
            }
            TirExprKind::Index { expr: array, index } => {
                Self::fixup_pack_expansion_locals_in_expr(array, local_count, local_types);
                Self::fixup_pack_expansion_locals_in_expr(index, local_count, local_types);
            }
            _ => {}
        }
    }

    /// Collect all local indices that are defined (via Let or pattern binding) inside an expression.
    fn collect_locals_in_expr(expr: &TirExpr, locals: &mut Vec<u32>) {
        match &expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_locals_in_expr(scrutinee, locals);
                for arm in arms {
                    Self::collect_locals_in_pattern(&arm.pattern, locals);
                    if let Some(guard) = &arm.guard {
                        Self::collect_locals_in_expr(guard, locals);
                    }
                    Self::collect_locals_in_expr(&arm.body, locals);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::collect_locals_in_block(block, locals);
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    Self::collect_locals_in_expr(&arg.expr, locals);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::collect_locals_in_expr(elem, locals);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            }
            | TirExprKind::FieldAccess { expr: p, .. }
            | TirExprKind::Cast { expr: p, .. }
            | TirExprKind::Unary { expr: p, .. } => {
                Self::collect_locals_in_expr(p, locals);
            }
            TirExprKind::Binary { left, right, .. }
            | TirExprKind::Assign {
                target: left,
                value: right,
            } => {
                Self::collect_locals_in_expr(left, locals);
                Self::collect_locals_in_expr(right, locals);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    Self::collect_locals_in_expr(&f.value, locals);
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::collect_locals_in_expr(condition, locals);
                Self::collect_locals_in_block(then_branch, locals);
                if let Some(eb) = else_branch {
                    Self::collect_locals_in_block(eb, locals);
                }
            }
            TirExprKind::Index { expr: array, index } => {
                Self::collect_locals_in_expr(array, locals);
                Self::collect_locals_in_expr(index, locals);
            }
            _ => {}
        }
    }

    fn collect_locals_in_block(block: &TirBlock, locals: &mut Vec<u32>) {
        for stmt in &block.stmts {
            match &stmt.kind {
                TirStmtKind::Let { local_index, .. } => {
                    locals.push(*local_index);
                }
                TirStmtKind::Expr(expr) => Self::collect_locals_in_expr(expr, locals),
                TirStmtKind::Return { value: Some(v) } => Self::collect_locals_in_expr(v, locals),
                TirStmtKind::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    Self::collect_locals_in_expr(condition, locals);
                    Self::collect_locals_in_block(then_block, locals);
                    if let Some(eb) = else_block {
                        Self::collect_locals_in_block(eb, locals);
                    }
                }
                TirStmtKind::LabeledBlock { block, .. } | TirStmtKind::Loop { body: block } => {
                    Self::collect_locals_in_block(block, locals);
                }
                _ => {}
            }
        }
    }

    fn collect_locals_in_pattern(pattern: &TirPattern, locals: &mut Vec<u32>) {
        match pattern {
            TirPattern::Binding { local_index, .. } => {
                locals.push(*local_index);
            }
            TirPattern::Variant { bindings, .. } => {
                for b in bindings {
                    Self::collect_locals_in_pattern(b, locals);
                }
            }
            TirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    Self::collect_locals_in_pattern(p, locals);
                }
            }
            TirPattern::Struct { fields, .. } => {
                for f in fields {
                    Self::collect_locals_in_pattern(&f.pattern, locals);
                }
            }
            _ => {}
        }
    }

    /// Find the type of a local variable definition inside an expression.
    fn find_local_type_in_expr(expr: &TirExpr, local_idx: u32) -> Option<TypeId> {
        match &expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                if let Some(t) = Self::find_local_type_in_expr(scrutinee, local_idx) {
                    return Some(t);
                }
                for arm in arms {
                    if let Some(t) = Self::find_local_type_in_pattern(&arm.pattern, local_idx) {
                        return Some(t);
                    }
                    if let Some(t) = Self::find_local_type_in_expr(&arm.body, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::find_local_type_in_block(block, local_idx)
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    if let Some(t) = Self::find_local_type_in_expr(&arg.expr, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    if let Some(t) = Self::find_local_type_in_expr(elem, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            }
            | TirExprKind::FieldAccess { expr: p, .. }
            | TirExprKind::Cast { expr: p, .. }
            | TirExprKind::Unary { expr: p, .. } => Self::find_local_type_in_expr(p, local_idx),
            TirExprKind::Binary { left, right, .. }
            | TirExprKind::Assign {
                target: left,
                value: right,
            } => Self::find_local_type_in_expr(left, local_idx)
                .or_else(|| Self::find_local_type_in_expr(right, local_idx)),
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    if let Some(t) = Self::find_local_type_in_expr(&f.value, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if let Some(t) = Self::find_local_type_in_expr(condition, local_idx) {
                    return Some(t);
                }
                if let Some(t) = Self::find_local_type_in_block(then_branch, local_idx) {
                    return Some(t);
                }
                if let Some(eb) = else_branch {
                    return Self::find_local_type_in_block(eb, local_idx);
                }
                None
            }
            _ => None,
        }
    }

    fn find_local_type_in_block(block: &TirBlock, local_idx: u32) -> Option<TypeId> {
        for stmt in &block.stmts {
            match &stmt.kind {
                TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } if *local_index == local_idx => return Some(*type_id),
                TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                    if let Some(t) = Self::find_local_type_in_expr(expr, local_idx) {
                        return Some(t);
                    }
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    if let Some(t) = Self::find_local_type_in_block(then_block, local_idx) {
                        return Some(t);
                    }
                    if let Some(eb) = else_block
                        && let Some(t) = Self::find_local_type_in_block(eb, local_idx)
                    {
                        return Some(t);
                    }
                }
                TirStmtKind::LabeledBlock { block, .. } | TirStmtKind::Loop { body: block } => {
                    if let Some(t) = Self::find_local_type_in_block(block, local_idx) {
                        return Some(t);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn find_local_type_in_pattern(pattern: &TirPattern, local_idx: u32) -> Option<TypeId> {
        match pattern {
            TirPattern::Binding {
                local_index,
                type_id,
                ..
            } if *local_index == local_idx => Some(*type_id),
            TirPattern::Variant { bindings, .. } => {
                for b in bindings {
                    if let Some(t) = Self::find_local_type_in_pattern(b, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    if let Some(t) = Self::find_local_type_in_pattern(p, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirPattern::Struct { fields, .. } => {
                for f in fields {
                    if let Some(t) = Self::find_local_type_in_pattern(&f.pattern, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Collect all unique `type_ids` from Return statement values in an expression.
    /// These are the GENERIC types before any substitution, used to compute
    /// wrong/correct type pairs for pack expansion fixup.
    fn collect_return_value_types(expr: &TirExpr) -> IndexSet<TypeId> {
        let mut types = IndexSet::default();
        Self::collect_return_value_types_in_expr(expr, &mut types);
        types
    }

    fn collect_return_value_types_in_expr(expr: &TirExpr, types: &mut IndexSet<TypeId>) {
        match &expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_return_value_types_in_expr(scrutinee, types);
                for arm in arms {
                    Self::collect_return_value_types_in_expr(&arm.body, types);
                }
            }
            TirExprKind::Block(block) => {
                for stmt in &block.stmts {
                    match &stmt.kind {
                        TirStmtKind::Return { value: Some(v) } => {
                            Self::collect_return_value_type_ids(v, types);
                        }
                        TirStmtKind::Expr(e) => {
                            Self::collect_return_value_types_in_expr(e, types);
                        }
                        TirStmtKind::If {
                            then_block,
                            else_block,
                            ..
                        } => {
                            for s in &then_block.stmts {
                                if let TirStmtKind::Return { value: Some(v) } = &s.kind {
                                    Self::collect_return_value_type_ids(v, types);
                                }
                            }
                            if let Some(eb) = else_block {
                                for s in &eb.stmts {
                                    if let TirStmtKind::Return { value: Some(v) } = &s.kind {
                                        Self::collect_return_value_type_ids(v, types);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_return_value_type_ids(expr: &TirExpr, types: &mut IndexSet<TypeId>) {
        types.insert(expr.type_id);
        if let TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } = &expr.kind
        {
            types.insert(*variant_type);
            if let Some(p) = payload {
                Self::collect_return_value_type_ids(p, types);
            }
        }
    }

    /// Fix up Return statements inside a type pack expansion element.
    ///
    /// The per-element substitution turns `[..T]` into `[elem_type]` (a single-element
    /// tuple) in Return statements, but it should be `concrete_pack` (the full tuple).
    /// This walks the expression tree and replaces the wrong type in Return values.
    fn fixup_return_types_in_expr(
        expr: &mut TirExpr,
        wrong_type: TypeId,
        correct_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        match &mut expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::fixup_return_types_in_expr(scrutinee, wrong_type, correct_type, type_table);
                for arm in arms {
                    Self::fixup_return_types_in_expr(
                        &mut arm.body,
                        wrong_type,
                        correct_type,
                        type_table,
                    );
                }
            }
            TirExprKind::Block(block) => {
                Self::fixup_return_types_in_block(block, wrong_type, correct_type, type_table);
            }
            _ => {}
        }
    }

    fn fixup_return_types_in_block(
        block: &mut TirBlock,
        wrong_type: TypeId,
        correct_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                TirStmtKind::Return { value: Some(value) } => {
                    Self::fixup_return_value_type(value, wrong_type, correct_type, type_table);
                }
                TirStmtKind::Expr(expr) => {
                    Self::fixup_return_types_in_expr(expr, wrong_type, correct_type, type_table);
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    Self::fixup_return_types_in_block(
                        then_block,
                        wrong_type,
                        correct_type,
                        type_table,
                    );
                    if let Some(else_blk) = else_block {
                        Self::fixup_return_types_in_block(
                            else_blk,
                            wrong_type,
                            correct_type,
                            type_table,
                        );
                    }
                }
                TirStmtKind::LabeledBlock { block, .. } | TirStmtKind::Loop { body: block } => {
                    Self::fixup_return_types_in_block(block, wrong_type, correct_type, type_table);
                }
                _ => {}
            }
        }
    }

    /// Replace the wrong single-element tuple type with the correct full tuple type
    /// inside a Return statement's value expression (recursively in `type_ids`).
    fn fixup_return_value_type(
        expr: &mut TirExpr,
        wrong_type: TypeId,
        correct_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        expr.type_id =
            Self::replace_type_in_generic(expr.type_id, wrong_type, correct_type, type_table);
        match &mut expr.kind {
            TirExprKind::VariantConstruct {
                variant_type,
                payload,
                ..
            } => {
                *variant_type = Self::replace_type_in_generic(
                    *variant_type,
                    wrong_type,
                    correct_type,
                    type_table,
                );
                if let Some(p) = payload {
                    Self::fixup_return_value_type(p, wrong_type, correct_type, type_table);
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::fixup_return_value_type(
                        &mut arg.expr,
                        wrong_type,
                        correct_type,
                        type_table,
                    );
                }
            }
            _ => {}
        }
    }

    /// Replace `old_type` with `new_type` inside generic instances.
    /// For example, Result<[i32], String> → Result<[i32,String,bool], String>
    /// when `old_type` = [i32] and `new_type` = [i32,String,bool].
    fn replace_type_in_generic(
        type_id: TypeId,
        old_type: TypeId,
        new_type: TypeId,
        type_table: &mut TypeTable,
    ) -> TypeId {
        if type_id == old_type {
            return new_type;
        }
        match type_table.get(type_id).clone() {
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| Self::replace_type_in_generic(arg, old_type, new_type, type_table))
                    .collect();
                if new_args == type_args {
                    type_id
                } else {
                    type_table.intern(ResolvedType::GenericInstance {
                        name,
                        module_source,
                        type_args: new_args,
                    })
                }
            }
            _ => type_id,
        }
    }

    fn rewrite_local_index_in_stmt(stmt: &mut TirStmt, old_idx: u32, new_idx: u32) {
        LocalIndexRewriter { old_idx, new_idx }.visit_stmt(stmt);
    }

    fn rewrite_local_index_in_expr(expr: &mut TirExpr, old_idx: u32, new_idx: u32) {
        LocalIndexRewriter { old_idx, new_idx }.visit_expr(expr);
    }
}

/// Convert `==`/`!=`/`<`/`>`/`<=`/`>=` on Struct/Variant/GenericInstance types to
/// `Eq::eq` / `Ord::cmp` method calls. Returns `None` for primitives.
fn try_lower_comparison(
    trait_method_locations: &IndexMap<String, ModuleSource>,
    current_module_source: &ModuleSource,
    span: crate::token::Span,
    op: TirBinaryOp,
    left: &TirExpr,
    right: &TirExpr,
    type_table: &mut TypeTable,
) -> Option<TirExprKind> {
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
            if TypeTable::is_tuple_type(name, module_source) {
                // Tuple Eq/Ord are provided by variadic impls in core:prelude/tuple.wado
                // and already lowered to method calls by the resolver.
                return None;
            }
            let args: Vec<String> = type_args
                .iter()
                .map(|&t| type_table.mangle_type_name(t))
                .collect();
            (name.clone(), args, Some(module_source.clone()))
        }
        _ => return None,
    };

    let make_ref = |e: &TirExpr, tt: &mut TypeTable| -> TirExpr {
        let ref_type = tt.intern(ResolvedType::Ref(e.type_id));
        TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: Box::new(e.clone()),
            },
            ref_type,
            span,
        )
    };

    let resolve_module = |mangled: &str, type_mod: Option<ModuleSource>| -> ModuleSource {
        trait_method_locations
            .get(mangled)
            .cloned()
            .or(type_mod)
            .unwrap_or_else(|| current_module_source.clone())
    };

    if matches!(op, TirBinaryOp::Eq | TirBinaryOp::NotEq) {
        let receiver = make_ref(left, type_table);
        let arg_ref = make_ref(right, type_table);
        let method_info =
            LocalMethodName::new(base_struct_name, Some("Eq".to_string()), "eq".to_string())
                .with_struct_type_args(&impl_type_args);
        let mangled_name = method_info.to_mangled_name();
        let method_module = resolve_module(&mangled_name, type_module_source);

        let method_call = TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: method_module,
                name: mangled_name,
                monomorph_info: None,
                method_info: Some(method_info),
            },
            vec![],
            vec![CallArg::new(arg_ref, false)],
        );

        if op == TirBinaryOp::NotEq {
            return Some(TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(TirExpr::new(method_call, TypeTable::BOOL, span)),
            });
        }
        return Some(method_call);
    }

    if matches!(
        op,
        TirBinaryOp::Lt | TirBinaryOp::Gt | TirBinaryOp::LtEq | TirBinaryOp::GtEq
    ) {
        let receiver = make_ref(left, type_table);
        let arg_ref = make_ref(right, type_table);
        let ordering_type_id = type_table.intern(ResolvedType::Enum {
            name: "Ordering".to_string(),
            module_source: ModuleSource::prelude(),
        });
        let method_info =
            LocalMethodName::new(base_struct_name, Some("Ord".to_string()), "cmp".to_string())
                .with_struct_type_args(&impl_type_args);
        let mangled_name = method_info.to_mangled_name();
        let method_module = resolve_module(&mangled_name, type_module_source);

        let cmp_call = TirExpr::new(
            TirExprKind::method_call(
                Box::new(receiver),
                FunctionRef {
                    module_source: method_module,
                    name: mangled_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                vec![],
                vec![CallArg::new(arg_ref, false)],
            ),
            ordering_type_id,
            span,
        );

        let (compare_op, case_name, case_index): (TirBinaryOp, &str, u32) = match op {
            TirBinaryOp::Lt => (TirBinaryOp::Eq, "Less", 0),
            TirBinaryOp::Gt => (TirBinaryOp::Eq, "Greater", 2),
            TirBinaryOp::LtEq => (TirBinaryOp::NotEq, "Greater", 2),
            TirBinaryOp::GtEq => (TirBinaryOp::NotEq, "Less", 0),
            _ => unreachable!(),
        };

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

/// Convert a trait method name to a TIR binary operator, if applicable.
/// Used when a type-param-receiver method call is monomorphized to a primitive type.
fn trait_method_to_binary_op(trait_name: Option<&str>, method_name: &str) -> Option<TirBinaryOp> {
    match (trait_name, method_name) {
        (Some("Add"), "add") => Some(TirBinaryOp::Add),
        (Some("Sub"), "sub") => Some(TirBinaryOp::Sub),
        (Some("Mul"), "mul") => Some(TirBinaryOp::Mul),
        (Some("Div"), "div") => Some(TirBinaryOp::Div),
        (Some("Rem"), "rem") => Some(TirBinaryOp::Mod),
        (Some("BitAnd"), "bitand") => Some(TirBinaryOp::BitAnd),
        (Some("BitOr"), "bitor") => Some(TirBinaryOp::BitOr),
        (Some("BitXor"), "bitxor") => Some(TirBinaryOp::BitXor),
        (Some("Shl"), "shl") => Some(TirBinaryOp::Shl),
        (Some("Shr"), "shr") => Some(TirBinaryOp::Shr),
        (Some("Eq"), "eq") => Some(TirBinaryOp::Eq),
        _ => None,
    }
}
