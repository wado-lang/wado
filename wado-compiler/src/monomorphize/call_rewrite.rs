//! Post-monomorphization rewrites: function call rewriting to monomorphized names.

use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::name::MethodName;
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBlock, TirExpr,
    TirExprKind, TirLocal, TirModule, TirStmt, TirStmtKind, TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use super::generic_function_key;
use super::module_source_for_trait_impl;
use super::state::Monomorphizer;

/// Strip `&`/`&mut` and `Newtype` and return the underlying type's home
/// module, if any. Used as the disambiguation hint for
/// `TraitEnv::impl_module_for` and as the candidate module for inherent-
/// method lookups; newtypes inherit their base type's inherent methods, so
/// peeling them lines the hint up with where the inherited template lives.
fn receiver_module_hint(tt: &TypeTable, tid: TypeId) -> Option<ModuleSource> {
    let mut tid = tid;
    loop {
        match tt.get(tid) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => tid = *inner,
            ResolvedType::Newtype { base_type, .. } => tid = *base_type,
            _ => return module_source_for_trait_impl(tt, tid),
        }
    }
}

impl Monomorphizer {
    /// Try to find a queued instantiation, allowing `key.module_source` to be
    /// an approximate hint. Mirrors `lookup_template_with_trait_fallback` in
    /// `func_inst.rs`: when the literal `(module_source, name, ...)` key
    /// misses, consult `TraitEnv::impl_module_for` over the listed struct
    /// candidates to find the impl block's home, then retry the lookup with
    /// that module. For inherent (non-trait) methods, also try the
    /// `type_module_hint` directly — inherent impls live with their type.
    /// Returns `(matched_key, mangled_name)` so the caller can rewrite the
    /// call site to point at the same module the queued instantiation
    /// landed in.
    fn lookup_instantiation_with_trait_fallback(
        &self,
        mut key: InstantiationKey,
        struct_candidates: &[&str],
        type_module_hint: Option<&ModuleSource>,
    ) -> Option<(InstantiationKey, String)> {
        if let Some(mangled) = self.lookup_function_instantiation(&key) {
            let mangled = mangled.clone();
            return Some((key, mangled));
        }
        let trait_name = key
            .method_info
            .as_ref()
            .and_then(|i| i.base_trait_name.as_ref().or(i.trait_name.as_ref()))
            .cloned();
        if let Some(trait_name) = trait_name {
            for candidate in struct_candidates {
                if let Some(impl_module) = self.functions.trait_env.impl_module_for(
                    candidate,
                    &trait_name,
                    type_module_hint,
                ) {
                    key.module_source = impl_module.clone();
                    if let Some(mangled) = self.lookup_function_instantiation(&key) {
                        let mangled = mangled.clone();
                        return Some((key, mangled));
                    }
                }
            }
            // Blanket impl fallback: dispatch through `impl<I: Bound> Trait for I`
            // isn't keyed by struct name. The queued instantiation lives in the
            // blanket's home module, looked up by trait name only.
            if let Some(impl_module) = self
                .functions
                .trait_env
                .blanket_impl_module_for_trait(&trait_name, type_module_hint)
            {
                key.module_source = impl_module.clone();
                if let Some(mangled) = self.lookup_function_instantiation(&key) {
                    let mangled = mangled.clone();
                    return Some((key, mangled));
                }
            }
        } else if let Some(type_module) = type_module_hint {
            // Inherent methods: the impl block lives in the receiver type's
            // own module (`impl<T> Array<T> { fn len(&self) -> i32 ... }`).
            key.module_source = type_module.clone();
            if let Some(mangled) = self.lookup_function_instantiation(&key) {
                let mangled = mangled.clone();
                return Some((key, mangled));
            }
        }
        None
    }
}

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
                // Sync `locals` types with Let statement types
                Self::sync_local_types_from_lets(&body, &mut func.locals);
                // Update all Local expression types based on `locals`
                Self::update_local_expr_types(&mut body, &func.locals);
                func.body = Some(body);
            }
        }

        // Rewrite function calls in global variable initializers
        for global in &mut module.globals {
            rewriter.visit_expr(&mut global.initializer);
        }
    }

    /// Sync `locals` types with the let statements they were derived from.
    ///
    /// Only walks into statement-level blocks (If/Loop/LabeledBlock/IfLet), not
    /// into expression blocks, since closures have their own local scope.
    fn sync_local_types_from_lets(block: &TirBlock, locals: &mut [TirLocal]) {
        struct SyncVisitor<'a> {
            locals: &'a mut [TirLocal],
        }
        impl TirRefVisitor for SyncVisitor<'_> {
            fn visit_stmt(&mut self, stmt: &TirStmt) {
                if let TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } = &stmt.kind
                    && let Some(local) = self.locals.get_mut(*local_index as usize)
                {
                    local.type_id = *type_id;
                }
                self.walk_stmt(stmt);
            }
            fn visit_expr(&mut self, _expr: &TirExpr) {
                // Don't recurse into expressions — only statement-level blocks matter
            }
        }
        SyncVisitor { locals }.visit_block(block);
    }

    /// Update all Local expression types based on the function's `locals`.
    fn update_local_expr_types(block: &mut TirBlock, locals: &[TirLocal]) {
        struct LocalTypeUpdater<'a> {
            locals: &'a [TirLocal],
        }
        impl TirMutVisitor for LocalTypeUpdater<'_> {
            fn visit_expr(&mut self, expr: &mut TirExpr) {
                if let TirExprKind::Local { index, .. } = &expr.kind
                    && let Some(local) = self.locals.get(*index as usize)
                {
                    expr.type_id = local.type_id;
                }
                // Closures have their own local scope, don't update with parent's locals
                if matches!(expr.kind, TirExprKind::Closure { .. }) {
                    return;
                }
                self.walk_expr(expr);
            }
        }
        LocalTypeUpdater { locals }.visit_block(block);
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
            let qualified_func_key =
                generic_function_key(func.is_method(), &func.module_source, &func.name);
            let qualified_func_name = qualified_func_key.1;

            // Resolve type_args: use explicit ones, or infer from args for implicit generic calls.
            // Only attempt inference for free functions (not method calls), since method calls
            // have their own rewriting logic below.
            let effective_type_args: Option<Vec<TypeId>> = if !type_args.is_empty() {
                Some(type_args.clone())
            } else if func.method_info.is_none() && func.monomorph_info.is_none() {
                self.infer_instantiated_type_args(
                    &qualified_func_name,
                    &original_method_info,
                    args,
                    type_table,
                )
            } else {
                None
            };

            if let Some(inferred_args) = effective_type_args {
                let key = InstantiationKey {
                    name: qualified_func_name,
                    module_source: func.module_source.clone(),
                    impl_type_args: vec![],
                    method_type_args: inferred_args,
                    method_info: original_method_info.clone(),
                };
                if let Some(mangled) = self.lookup_function_instantiation(&key) {
                    *func = FunctionRef {
                        module_source: key.module_source.clone(),
                        name: mangled.clone(),
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: original_func_name,
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args.clone(),
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
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
                let candidates: Vec<&str> = vec![&info.base_struct_name, &info.struct_name];
                for generic_method_name in names_to_try {
                    let key = InstantiationKey {
                        name: generic_method_name.clone(),
                        module_source: func.module_source.clone(),
                        impl_type_args: monomorph.impl_type_args.clone(),
                        method_type_args: monomorph.method_type_args.clone(),
                        method_info: Some(info.clone()),
                    };
                    if let Some((key, mangled)) =
                        self.lookup_instantiation_with_trait_fallback(key, &candidates, None)
                    {
                        let original_method_info = func.method_info.clone();
                        *func = FunctionRef {
                            module_source: key.module_source.clone(),
                            name: mangled,
                            monomorph_info: Some(MonomorphInfo {
                                generic_name: generic_method_name,
                                impl_type_args: key.impl_type_args.clone(),
                                method_type_args: key.method_type_args,
                                is_blanket: false,
                            }),
                            method_info: original_method_info,
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

            let info_ref = method_func.method_info.as_ref();
            let candidates: Vec<&str> = if let Some(info) = info_ref {
                vec![&info.base_struct_name, &info.struct_name, &struct_name]
            } else {
                vec![&struct_name]
            };
            let receiver_module = receiver_module_hint(type_table, receiver.type_id);
            let mut rewritten = false;
            for (full_method_name, _tn) in &names_to_try {
                let key = InstantiationKey {
                    name: full_method_name.clone(),
                    module_source: method_func.module_source.clone(),
                    impl_type_args: vec![],
                    method_type_args: type_args.clone(),
                    method_info: method_func.method_info.clone(),
                };
                if let Some((key, mangled)) = self.lookup_instantiation_with_trait_fallback(
                    key,
                    &candidates,
                    receiver_module.as_ref(),
                ) {
                    let original_method_info = method_func.method_info.clone();
                    *method_func = FunctionRef {
                        module_source: key.module_source.clone(),
                        name: mangled,
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: full_method_name.clone(),
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args,
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
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

                    let info_ref = method_func.method_info.as_ref();
                    let dg_candidates: Vec<&str> = if let Some(info) = info_ref {
                        vec![&info.base_struct_name, &info.struct_name, &base_struct]
                    } else {
                        vec![&base_struct]
                    };
                    let dg_receiver_module = receiver_module_hint(type_table, receiver.type_id);
                    for (generic_method_name, _tn) in &dg_names {
                        let combined_key = InstantiationKey {
                            name: generic_method_name.clone(),
                            module_source: method_func.module_source.clone(),
                            impl_type_args: impl_type_args.clone(),
                            method_type_args: type_args.clone(),
                            method_info: method_func.method_info.clone(),
                        };
                        if let Some((combined_key, mangled)) = self
                            .lookup_instantiation_with_trait_fallback(
                                combined_key,
                                &dg_candidates,
                                dg_receiver_module.as_ref(),
                            )
                        {
                            let original_method_info = method_func.method_info.clone();
                            *method_func = FunctionRef {
                                module_source: combined_key.module_source.clone(),
                                name: mangled,
                                monomorph_info: Some(MonomorphInfo {
                                    generic_name: generic_method_name.clone(),
                                    impl_type_args: combined_key.impl_type_args.clone(),
                                    method_type_args: combined_key.method_type_args.clone(),
                                    is_blanket: false,
                                }),
                                method_info: original_method_info,
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
        //
        // Skip this branch when the call carries a blanket-impl
        // `monomorph_info` (synthesized refs like `self.data.inspect(f)` for
        // `data: &Array<i32>`). The impl_type_args derived from the
        // receiver's GenericInstance (`[i32]`) are NOT what the blanket
        // instantiation was queued with (`[Array<i32>]`), so any match here
        // would route the call to the inner-type's impl
        // (`Array<i32>^Inspect`) and drop the leading `&` produced by the
        // ref blanket body. Let the dedicated blanket-fallback branch below
        // handle these — it uses `mono.impl_type_args` which carries the
        // full ref-inner type.
        else if let Some((base_struct, impl_type_args)) =
            self.get_struct_info_from_type(receiver.type_id, type_table)
            && !impl_type_args.is_empty()
            && !method_func
                .monomorph_info
                .as_ref()
                .is_some_and(|m| m.is_blanket)
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
                        module_source: method_func.module_source.clone(),
                        impl_type_args: impl_type_args.clone(),
                        method_type_args: vec![],
                        method_info: None,
                    });
                }
                let trait_method_name =
                    MethodName::format_local(&base_struct, Some(trait_name), &method_name);
                possible_keys.push(InstantiationKey {
                    name: trait_method_name,
                    module_source: method_func.module_source.clone(),
                    impl_type_args: impl_type_args.clone(),
                    method_type_args: vec![],
                    method_info: None,
                });
            }
            // Also try regular method format
            possible_keys.push(InstantiationKey {
                name: MethodName::format_local(&base_struct, None, &method_name),
                module_source: method_func.module_source.clone(),
                impl_type_args,
                method_type_args: vec![],
                method_info: None,
            });

            let info_ref = method_func.method_info.as_ref();
            let pk_candidates: Vec<&str> = if let Some(info) = info_ref {
                vec![&info.base_struct_name, &info.struct_name, &base_struct]
            } else {
                vec![&base_struct]
            };
            let pk_receiver_module = receiver_module_hint(type_table, receiver.type_id);
            for mut key in possible_keys {
                // Help the trait_env fallback by carrying the call's method_info on
                // the lookup key. `InstantiationKey` equality ignores `method_info`,
                // so this only feeds the fallback's trait-name extraction; it does
                // not perturb the literal hashmap lookup.
                if key.method_info.is_none() {
                    key.method_info = method_func.method_info.clone();
                }
                if let Some((key, mangled)) = self.lookup_instantiation_with_trait_fallback(
                    key,
                    &pk_candidates,
                    pk_receiver_module.as_ref(),
                ) {
                    // Preserve original method_info
                    let original_method_info = method_func.method_info.clone();
                    *method_func = FunctionRef {
                        module_source: key.module_source.clone(),
                        name: mangled,
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: key.name.clone(),
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args,
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
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
                    module_source: method_func.module_source.clone(),
                    impl_type_args: mono.impl_type_args.clone(),
                    method_type_args: mono.method_type_args.clone(),
                    method_info: method_func.method_info.clone(),
                };
                let candidates: Vec<&str> = if let Some(info) = method_func.method_info.as_ref() {
                    vec![&info.base_struct_name, &info.struct_name]
                } else {
                    Vec::new()
                };
                let blanket_receiver_module = receiver_module_hint(type_table, receiver.type_id);
                self.lookup_instantiation_with_trait_fallback(
                    key,
                    &candidates,
                    blanket_receiver_module.as_ref(),
                )
                .map(|(k, mangled)| {
                    (
                        mangled,
                        mono.generic_name.clone(),
                        mono.impl_type_args.clone(),
                        mono.method_type_args.clone(),
                        k.module_source,
                    )
                })
            } else {
                None
            };
            if let Some((mangled, generic_name, impl_ta, method_ta, ms)) = blanket_lookup {
                let original_method_info = method_func.method_info.clone();
                *method_func = FunctionRef {
                    module_source: ms,
                    name: mangled,
                    monomorph_info: Some(MonomorphInfo {
                        generic_name,
                        impl_type_args: impl_ta,
                        method_type_args: method_ta,
                        is_blanket: true,
                    }),
                    method_info: original_method_info,
                };
            }
        }
        // Tuple variadic impl: rewrite Tuple^Eq::eq → Tuple<i32,i32,i32>^Eq::eq
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
            let impl_type_args = mono.map(|m| m.impl_type_args.clone()).unwrap_or_else(|| {
                let mut receiver_type = receiver.type_id;
                while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(receiver_type).clone()
                {
                    receiver_type = inner;
                }
                type_table.as_tuple(receiver_type).unwrap_or_default()
            });
            {
                let key = InstantiationKey {
                    name: generic_name.clone(),
                    module_source: method_func.module_source.clone(),
                    impl_type_args: impl_type_args.clone(),
                    method_type_args: vec![],
                    method_info: method_func.method_info.clone(),
                };
                let candidates: Vec<&str> =
                    vec![TypeTable::TUPLE_TYPE_NAME, &info.base_struct_name];
                let tuple_receiver_module = receiver_module_hint(type_table, receiver.type_id);
                if let Some((key, mangled)) = self.lookup_instantiation_with_trait_fallback(
                    key,
                    &candidates,
                    tuple_receiver_module.as_ref(),
                ) {
                    let original_method_info = method_func.method_info.clone();
                    *method_func = FunctionRef {
                        module_source: key.module_source,
                        name: mangled,
                        monomorph_info: Some(MonomorphInfo {
                            generic_name,
                            impl_type_args,
                            method_type_args: vec![],
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
                    };
                }
            }
        }
    }

    /// For implicit generic calls (where `type_args` is empty but the callee was instantiated),
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
