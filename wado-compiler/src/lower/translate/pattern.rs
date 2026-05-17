use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;

use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::FunctionRef;
use crate::tir::{
    CallArg, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirField,
    TirLiteralPattern, TirLocal, TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable, block_result_type,
};
use crate::token::Span;

/// Pattern lowering: a TIR-mutating pass that rewrites
/// `LetDestructure` / `IfLet` into explicit `Let` + `If` chains and
/// expands or-patterns in `Match` arms. Runs as the last TIR-touching
/// pass before the translator walks TIR → NIR.
pub fn lower(flat: &mut FlatPackage) {
    // Build a map keyed by (variant_name, module_source). The
    // module_source axis is required so that two modules each
    // declaring a variant with the same name keep their case
    // tables distinct — pattern lookup resolves the
    // module_source from the scrutinee's resolved type.
    let mut variant_case_map: IndexMap<(String, ModuleSource), Vec<(String, u32)>> =
        IndexMap::default();
    for variant in &flat.variants {
        let cases: Vec<(String, u32)> = variant
            .cases
            .iter()
            .map(|c| (c.name.clone(), c.index))
            .collect();
        variant_case_map.insert((variant.name.clone(), variant.module_source.clone()), cases);
    }

    // Build struct fields map from module structs. The key uses the
    // struct's own `module_source` so that two modules each declaring
    // a struct with the same name keep their field tables distinct —
    // pattern lookup resolves the `module_source` from the scrutinee's
    // resolved type.
    let mut struct_fields_map: IndexMap<(String, ModuleSource), Vec<TirField>> =
        IndexMap::default();
    for s in &flat.structs {
        struct_fields_map.insert((s.name.clone(), s.module_source.clone()), s.fields.clone());
    }

    let type_table = flat.type_table.borrow();
    let eq_trait_name = type_table
        .compiler_items()
        .trait_name(crate::compiler_item::CompilerItem::Eq)
        .to_string();
    let string_struct_name = type_table
        .compiler_items()
        .struct_name(crate::compiler_item::CompilerItem::String)
        .to_string();
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(mut body) = func.body.take() {
            // Take ownership of the values to avoid borrow conflicts
            let local_count = func.local_count;
            let locals = std::mem::take(&mut func.locals);

            let mut lowerer = PatternLowerer::new(
                local_count,
                locals,
                eq_trait_name.clone(),
                string_struct_name.clone(),
                &variant_case_map,
                &struct_fields_map,
            );
            lowerer.lower_block(&mut body, &type_table);

            // Put the values back
            let (new_count, new_locals) = lowerer.into_parts();
            func.local_count = new_count;
            func.locals = new_locals;
            func.body = Some(body);
        }
    }
}

/// Pattern lowering context - tracks local allocation for a function
struct PatternLowerer<'a> {
    local_count: u32,
    locals: Vec<TirLocal>,
    temp_counter: u32,
    /// Canonical stdlib name of the `Eq` trait. Resolved once from the
    /// compiler-item registry so synthesised `String^Eq::eq` calls
    /// follow stdlib renames without falling back to a hard-coded
    /// `"Eq"` literal.
    eq_trait_name: String,
    /// Canonical stdlib name of the `String` struct, resolved through
    /// the same registry so the receiver-type slot of the synthesised
    /// `String^Eq::eq` `LocalMethodName` tracks renames too.
    string_struct_name: String,
    /// Map from (`variant_name`, `module_source`) to a list of
    /// (`case_name`, `case_index`) pairs. The `module_source` axis
    /// is required because Wado allows two modules to each declare a
    /// variant with the same name; lookup resolves the source from
    /// the scrutinee's `ResolvedType::Variant.module_source` (or
    /// `GenericInstance.module_source`).
    variant_case_map: &'a IndexMap<(String, ModuleSource), Vec<(String, u32)>>,
    /// Map from (`struct_name`, `module_source`) to field definitions
    struct_fields_map: &'a IndexMap<(String, ModuleSource), Vec<TirField>>,
}

impl<'a> PatternLowerer<'a> {
    fn new(
        local_count: u32,
        locals: Vec<TirLocal>,
        eq_trait_name: String,
        string_struct_name: String,
        variant_case_map: &'a IndexMap<(String, ModuleSource), Vec<(String, u32)>>,
        struct_fields_map: &'a IndexMap<(String, ModuleSource), Vec<TirField>>,
    ) -> Self {
        Self {
            local_count,
            locals,
            temp_counter: 0,
            eq_trait_name,
            string_struct_name,
            variant_case_map,
            struct_fields_map,
        }
    }

    /// Look up the case index for a variant case by (variant name,
    /// module source, case name). The module source is mandatory to
    /// distinguish two modules' same-named variants; the legacy
    /// name-only lookup silently overwrote one module's cases with
    /// the other's and produced spurious "Unknown case" panics on
    /// `if let` patterns whose target was the overwritten variant.
    fn get_case_index(
        &self,
        variant_name: &str,
        module_source: &ModuleSource,
        case_name: &str,
    ) -> Option<u32> {
        self.variant_case_map
            .get(&(variant_name.to_string(), module_source.clone()))
            .and_then(|cases| cases.iter().find(|(name, _)| name == case_name))
            .map(|(_, index)| *index)
    }

    /// Look up struct field definitions by `type_id`
    fn get_struct_fields(&self, type_id: TypeId, type_table: &TypeTable) -> Option<Vec<TirField>> {
        match type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => self
                .struct_fields_map
                .get(&(name.clone(), module_source.clone()))
                .cloned(),
            _ => None,
        }
    }

    /// Consume the lowerer and return the final local count and locals.
    fn into_parts(self) -> (u32, Vec<TirLocal>) {
        (self.local_count, self.locals)
    }

    /// Allocate a new local and return its index. Pattern temps are
    /// synthesised — they have no source-level name, so the produced slot
    /// uses the `__local_N` convention.
    fn alloc_local(&mut self, type_id: TypeId) -> u32 {
        let index = self.local_count;
        self.local_count += 1;
        self.locals.push(TirLocal::synth(index, type_id, false));
        index
    }

    /// Generate a unique temp local name
    fn next_temp_name(&mut self) -> String {
        let name = format!("__pattern_temp_{}", self.temp_counter);
        self.temp_counter += 1;
        name
    }

    /// Walk a pattern and replace every `mut`-binding leaf whose target
    /// local is in `is_mut == true`. For each, allocate a fresh
    /// immutable temp local, rewrite the pattern leaf to point at the
    /// temp, and return a `Let { is_mut: true, value: Local(temp) }`
    /// statement that copies the temp into the original mutable local.
    ///
    /// The resulting `Let` value is wrapped by
    /// `lower::plan::value_copy::insert`, so the mutable binding gets a
    /// fresh copy of the scrutinee payload instead of aliasing it. This
    /// preserves the value semantics the previous `lower_if_pattern_*`
    /// helpers provided for `IfLet`'s mutable bindings.
    fn hoist_mut_bindings(&mut self, pattern: &mut TirPattern, span: Span) -> Vec<TirStmt> {
        let mut prepend = Vec::new();
        self.hoist_mut_bindings_in_pattern(pattern, span, &mut prepend);
        prepend
    }

    fn hoist_mut_bindings_in_pattern(
        &mut self,
        pattern: &mut TirPattern,
        span: Span,
        out: &mut Vec<TirStmt>,
    ) {
        match pattern {
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                let orig_index = *local_index;
                let orig_type = *type_id;
                let local_is_mut = self
                    .locals
                    .get(orig_index as usize)
                    .map(|l| l.is_mut)
                    .unwrap_or(false);
                if !local_is_mut {
                    return;
                }
                let temp_index = self.alloc_local(orig_type);
                let temp_name = self.next_temp_name();
                let orig_name = std::mem::replace(name, temp_name.clone());
                *local_index = temp_index;
                *type_id = orig_type;
                let temp_local = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: temp_name,
                    },
                    orig_type,
                    span,
                );
                out.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: orig_name,
                        local_index: orig_index,
                        is_mut: true,
                        is_reactive: false,
                        type_id: orig_type,
                        value: temp_local,
                        skip_value_copy: false,
                    },
                    span,
                ));
            }
            TirPattern::Tuple(sub_patterns, _) => {
                for sub in sub_patterns {
                    self.hoist_mut_bindings_in_pattern(sub, span, out);
                }
            }
            TirPattern::Variant { bindings, .. } => {
                for sub in bindings {
                    self.hoist_mut_bindings_in_pattern(sub, span, out);
                }
            }
            TirPattern::Struct { fields, .. } => {
                for f in fields {
                    self.hoist_mut_bindings_in_pattern(&mut f.pattern, span, out);
                }
            }
            TirPattern::Or(alternatives) => {
                // `Or` alternatives share the same arm body, so each
                // alternative's `mut` binding refers to the same target
                // local. Lift through each alternative; the `Let`s
                // emitted use the same destination local across
                // alternatives, which is the same shape the resolver
                // produced for the non-or case.
                for alt in alternatives {
                    self.hoist_mut_bindings_in_pattern(alt, span, out);
                }
            }
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {}
        }
    }

    /// Check if this is a multi-value builtin call that should not be lowered.
    /// Codegen has a special optimization for these patterns.
    fn is_multivalue_builtin_pattern(
        &self,
        pattern: &TirPattern,
        value: &TirExpr,
        type_table: &TypeTable,
    ) -> bool {
        // Pattern must be a flat tuple with only Binding or Wildcard
        let patterns = match pattern {
            TirPattern::Tuple(patterns, _) => patterns,
            _ => return false,
        };

        // Verify all sub-patterns are simple bindings or wildcards
        for p in patterns {
            match p {
                TirPattern::Binding { .. } | TirPattern::Wildcard => {}
                _ => return false,
            }
        }

        // Check if value is a builtin call
        let is_builtin_call = match &value.kind {
            TirExprKind::Call { func: func_ref, .. } => func_ref.module_source.is_core_builtin(),
            _ => false,
        };

        if !is_builtin_call {
            return false;
        }

        // Check if return type is a tuple (multi-value return)
        let elem_types = match type_table.as_tuple(value.type_id) {
            Some(types) => types,
            None => return false,
        };

        // Verify pattern length matches tuple element count
        patterns.len() == elem_types.len()
    }

    /// Check if a pattern contains any refutable sub-patterns that need extraction.
    ///
    /// A "refutable sub-pattern" is a pattern nested inside a compound that may fail
    /// to match at runtime (literals, variants, enums, constants, ranges, or-patterns).
    /// Pure destructurings such as `Some([a, b])` or `{ x, y }` return false — they
    /// are handled by the simpler non-extracting lowering paths.
    fn pattern_has_refutable_sub_patterns(pattern: &TirPattern) -> bool {
        match pattern {
            TirPattern::Tuple(sub_patterns, _) => {
                sub_patterns.iter().any(Self::pattern_is_refutable)
            }
            TirPattern::Struct { fields, .. } => fields
                .iter()
                .any(|f| Self::pattern_is_refutable(&f.pattern)),
            TirPattern::Variant { bindings, .. } => bindings.iter().any(Self::pattern_is_refutable),
            _ => false,
        }
    }

    /// Whether the given pattern may fail to match at runtime.
    fn pattern_is_refutable(pattern: &TirPattern) -> bool {
        match pattern {
            TirPattern::Wildcard | TirPattern::Binding { .. } => false,
            TirPattern::Literal(_)
            | TirPattern::Variant { .. }
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. }
            | TirPattern::Or(_) => true,
            TirPattern::Tuple(sub_patterns, _) => {
                sub_patterns.iter().any(Self::pattern_is_refutable)
            }
            TirPattern::Struct { fields, .. } => fields
                .iter()
                .any(|f| Self::pattern_is_refutable(&f.pattern)),
        }
    }

    /// Extract refutable sub-patterns (literals, variants, enums) from a match arm's
    /// tuple/struct pattern into guard conditions.
    ///
    /// Transforms:
    ///   `[a, 10] => body`  →  `[a, __lit_0] && __lit_0 == 10 => body`
    ///   `[Bool(x), Bool(y)] => body`  →  `[__v_0, __v_1] && variant_test(__v_0, Bool) && variant_test(__v_1, Bool) => { let x = payload(__v_0); let y = payload(__v_1); body }`
    fn extract_refutable_sub_patterns(
        &mut self,
        arm: &mut TirMatchArm,
        scrutinee_type: TypeId,
        type_table: &TypeTable,
    ) {
        if !Self::pattern_has_refutable_sub_patterns(&arm.pattern) {
            return;
        }

        let span = arm.span;
        let mut conditions: Vec<TirExpr> = Vec::new();
        let mut body_prefix_stmts: Vec<TirStmt> = Vec::new();

        match &mut arm.pattern {
            TirPattern::Tuple(sub_patterns, _) => {
                let element_types = type_table
                    .as_tuple(scrutinee_type)
                    .unwrap_or_else(|| vec![TypeTable::UNKNOWN; sub_patterns.len()]);

                for (i, sub) in sub_patterns.iter_mut().enumerate() {
                    let elem_type = element_types.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                    self.extract_refutable_sub_pattern(
                        sub,
                        elem_type,
                        span,
                        type_table,
                        &mut conditions,
                        &mut body_prefix_stmts,
                    );
                }
            }
            TirPattern::Struct { fields, .. } => {
                let struct_fields_info = self.get_struct_fields(scrutinee_type, type_table);

                for field in fields.iter_mut() {
                    let field_type = struct_fields_info
                        .as_ref()
                        .and_then(|info| {
                            info.iter()
                                .find(|f| f.name == field.field_name)
                                .map(|f| f.type_id)
                        })
                        .unwrap_or(TypeTable::UNKNOWN);
                    self.extract_refutable_sub_pattern(
                        &mut field.pattern,
                        field_type,
                        span,
                        type_table,
                        &mut conditions,
                        &mut body_prefix_stmts,
                    );
                }
            }
            TirPattern::Variant {
                bindings,
                payload_type,
                ..
            } => {
                let payload_type = *payload_type;
                for binding in bindings.iter_mut() {
                    self.extract_refutable_sub_pattern(
                        binding,
                        payload_type,
                        span,
                        type_table,
                        &mut conditions,
                        &mut body_prefix_stmts,
                    );
                }
            }
            _ => {}
        }

        if conditions.is_empty() && body_prefix_stmts.is_empty() {
            return;
        }

        // Combine all conditions with &&
        let combined_conditions: Option<TirExpr> = if conditions.is_empty() {
            None
        } else {
            Some(
                conditions
                    .into_iter()
                    .reduce(|acc, cond| {
                        TirExpr::new(
                            TirExprKind::Binary {
                                op: TirBinaryOp::And,
                                left: Box::new(acc),
                                right: Box::new(cond),
                            },
                            TypeTable::BOOL,
                            span,
                        )
                    })
                    .unwrap(),
            )
        };

        let existing_guard = arm.guard.take();

        // Build the "inner" guard: `{ body_prefix_stmts; existing_guard_or_true }`.
        // This ensures that extracted bindings (e.g. variant payloads) are set
        // BEFORE the user's guard expression evaluates, so guards can reference
        // bindings from refutable sub-patterns (e.g. `[Some(a), Some(b)] && a > b`).
        // Since locals are function-scoped in WASM, bindings set here persist
        // into the arm body.
        let inner_guard: Option<TirExpr> = if body_prefix_stmts.is_empty() {
            existing_guard
        } else {
            let final_bool = existing_guard.unwrap_or_else(|| {
                TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span)
            });
            let mut block_stmts = body_prefix_stmts;
            block_stmts.push(TirStmt::new(TirStmtKind::Expr(final_bool), span));
            Some(TirExpr::new(
                TirExprKind::Block(TirBlock::new(block_stmts, span)),
                TypeTable::BOOL,
                span,
            ))
        };

        // Combine conditions with the inner guard.
        arm.guard = match (combined_conditions, inner_guard) {
            (Some(c), Some(i)) => Some(TirExpr::new(
                TirExprKind::Binary {
                    op: TirBinaryOp::And,
                    left: Box::new(c),
                    right: Box::new(i),
                },
                TypeTable::BOOL,
                span,
            )),
            (Some(c), None) => Some(c),
            (None, Some(i)) => Some(i),
            (None, None) => None,
        };
    }

    /// Extract a single refutable sub-pattern, replacing it with a binding and
    /// adding conditions and body prefix statements as needed.
    fn extract_refutable_sub_pattern(
        &mut self,
        sub: &mut TirPattern,
        elem_type: TypeId,
        span: Span,
        type_table: &TypeTable,
        conditions: &mut Vec<TirExpr>,
        body_prefix_stmts: &mut Vec<TirStmt>,
    ) {
        match sub {
            TirPattern::Literal(lit) => {
                let temp_index = self.alloc_local(elem_type);

                let cond = self.literal_eq_condition(temp_index, elem_type, lit, span);
                conditions.push(cond);

                *sub = TirPattern::Binding {
                    name: format!("__lit_{temp_index}"),
                    local_index: temp_index,
                    type_id: elem_type,
                };
            }
            TirPattern::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => {
                let temp_index = self.alloc_local(elem_type);
                let temp_name = format!("__variant_{temp_index}");

                // If the element is a reference, deref to get the variant value
                let variant_expr = {
                    let local = TirExpr::new(
                        TirExprKind::Local {
                            index: temp_index,
                            name: temp_name.clone(),
                        },
                        elem_type,
                        span,
                    );
                    let mut inner = elem_type;
                    let mut expr = local;
                    while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) = type_table.get(inner)
                    {
                        let t = *t;
                        expr = TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Deref,
                                expr: Box::new(expr),
                            },
                            t,
                            span,
                        );
                        inner = t;
                    }
                    expr
                };

                // Generate VariantTest condition
                let variant_type_info = match type_table.get(*enum_type) {
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    }
                    | ResolvedType::GenericInstance {
                        name,
                        module_source,
                        ..
                    } => Some((name.clone(), module_source.clone())),
                    _ => None,
                };

                if let Some((ref vt_name, ref vt_module)) = variant_type_info
                    && let Some(case_index) = self.get_case_index(vt_name, vt_module, variant_name)
                {
                    let cond = TirExpr::new(
                        TirExprKind::VariantTest {
                            expr: Box::new(variant_expr.clone()),
                            case_index,
                            case_name: variant_name.clone(),
                        },
                        TypeTable::BOOL,
                        span,
                    );
                    conditions.push(cond);

                    // Generate payload extraction for the arm body
                    if let Some(binding) = bindings.first() {
                        let payload_expr = TirExpr::new(
                            TirExprKind::VariantPayload {
                                expr: Box::new(variant_expr),
                                case_index,
                                payload_type: *payload_type,
                            },
                            *payload_type,
                            span,
                        );

                        match binding {
                            TirPattern::Binding {
                                name,
                                local_index,
                                type_id,
                            } => {
                                body_prefix_stmts.push(TirStmt::new(
                                    TirStmtKind::Let {
                                        name: name.clone(),
                                        local_index: *local_index,
                                        is_mut: false,
                                        is_reactive: false,
                                        type_id: *type_id,
                                        value: payload_expr,
                                        skip_value_copy: false,
                                    },
                                    span,
                                ));
                            }
                            _ => {
                                // For more complex payload patterns (e.g. tuple),
                                // use lower_pattern_to_lets
                                self.lower_pattern_to_lets(
                                    binding,
                                    false,
                                    payload_expr,
                                    span,
                                    body_prefix_stmts,
                                    type_table,
                                );
                            }
                        }
                    }
                }

                *sub = TirPattern::Binding {
                    name: temp_name,
                    local_index: temp_index,
                    type_id: elem_type,
                };
            }
            TirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => {
                let temp_index = self.alloc_local(elem_type);
                let temp_name = format!("__enum_{temp_index}");

                // If the element is a reference, deref to get the enum value
                let enum_expr = {
                    let local = TirExpr::new(
                        TirExprKind::Local {
                            index: temp_index,
                            name: temp_name.clone(),
                        },
                        elem_type,
                        span,
                    );
                    let mut inner = elem_type;
                    let mut expr = local;
                    while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) = type_table.get(inner)
                    {
                        let t = *t;
                        expr = TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Deref,
                                expr: Box::new(expr),
                            },
                            t,
                            span,
                        );
                        inner = t;
                    }
                    expr
                };

                // Generate enum discriminant comparison
                let cond = TirExpr::new(
                    TirExprKind::Binary {
                        left: Box::new(enum_expr),
                        op: TirBinaryOp::Eq,
                        right: Box::new(TirExpr::new(
                            TirExprKind::EnumConstruct {
                                enum_type: *enum_type,
                                case_index: *case_index,
                                case_name: case_name.clone(),
                            },
                            *enum_type,
                            span,
                        )),
                    },
                    TypeTable::BOOL,
                    span,
                );
                conditions.push(cond);

                *sub = TirPattern::Binding {
                    name: temp_name,
                    local_index: temp_index,
                    type_id: elem_type,
                };
            }
            TirPattern::ConstantValue { expr: const_expr } => {
                let temp_index = self.alloc_local(elem_type);
                let local_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: format!("__const_{temp_index}"),
                    },
                    elem_type,
                    span,
                );
                let cond = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::Eq,
                        left: Box::new(local_expr),
                        right: const_expr.clone(),
                    },
                    TypeTable::BOOL,
                    span,
                );
                conditions.push(cond);
                *sub = TirPattern::Binding {
                    name: format!("__const_{temp_index}"),
                    local_index: temp_index,
                    type_id: elem_type,
                };
            }
            TirPattern::Struct { .. } | TirPattern::Tuple(..) | TirPattern::Or(..) => {
                // Compound sub-patterns (struct/tuple/or inside variant payload or
                // another compound). These may themselves contain refutable sub-patterns
                // (e.g. `Branch([Leaf(a), Leaf(b)])`), so we use `build_pattern_check`
                // to recursively generate the full check expression including both
                // variant tag tests and payload extractions. The whole thing becomes
                // a single bool condition in the guard.
                let temp_index = self.alloc_local(elem_type);
                let temp_name = format!("__compound_{temp_index}");
                let temp_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: temp_name.clone(),
                    },
                    elem_type,
                    span,
                );
                let original = std::mem::replace(sub, TirPattern::Wildcard);
                let true_literal =
                    TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span);
                let check = self.build_pattern_check(
                    &original,
                    temp_expr,
                    elem_type,
                    span,
                    type_table,
                    true_literal,
                );
                conditions.push(check);
                *sub = TirPattern::Binding {
                    name: temp_name,
                    local_index: temp_index,
                    type_id: elem_type,
                };
            }
            _ => {}
        }
    }

    /// Build a boolean `TirExpr` that checks whether `value` matches `pattern`.
    /// If the pattern matches, any inner bindings are set and then `continuation`
    /// is evaluated as the final result. If the pattern does not match, the
    /// expression short-circuits to false.
    ///
    /// This is a CPS-style helper used for recursive refutable extraction in
    /// nested compound patterns (e.g. `Branch([Leaf(a), Leaf(b)])`).
    fn build_pattern_check(
        &mut self,
        pattern: &TirPattern,
        value: TirExpr,
        pattern_type: TypeId,
        span: Span,
        type_table: &TypeTable,
        continuation: TirExpr,
    ) -> TirExpr {
        match pattern {
            TirPattern::Wildcard => {
                // Evaluate value for side effects, then continuation.
                let drop_stmt = TirStmt::new(TirStmtKind::Expr(value), span);
                let cont_stmt = TirStmt::new(TirStmtKind::Expr(continuation), span);
                let block = TirBlock::new(vec![drop_stmt, cont_stmt], span);
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: *local_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: *type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                let cont_stmt = TirStmt::new(TirStmtKind::Expr(continuation), span);
                let block = TirBlock::new(vec![let_stmt, cont_stmt], span);
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Literal(lit) => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__lit_{temp_index}");
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: temp_name,
                        local_index: temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: pattern_type,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                let eq_cond = self.literal_eq_condition(temp_index, pattern_type, lit, span);
                let and_expr = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::And,
                        left: Box::new(eq_cond),
                        right: Box::new(continuation),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let block = TirBlock::new(
                    vec![let_stmt, TirStmt::new(TirStmtKind::Expr(and_expr), span)],
                    span,
                );
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::ConstantValue { expr: const_expr } => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__const_{temp_index}");
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: temp_name.clone(),
                        local_index: temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: pattern_type,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                let local_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: temp_name,
                    },
                    pattern_type,
                    span,
                );
                let eq_cond = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::Eq,
                        left: Box::new(local_expr),
                        right: const_expr.clone(),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let and_expr = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::And,
                        left: Box::new(eq_cond),
                        right: Box::new(continuation),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let block = TirBlock::new(
                    vec![let_stmt, TirStmt::new(TirStmtKind::Expr(and_expr), span)],
                    span,
                );
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__enum_{temp_index}");
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: temp_name.clone(),
                        local_index: temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: pattern_type,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                let mut enum_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: temp_name,
                    },
                    pattern_type,
                    span,
                );
                let mut inner = pattern_type;
                while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) = type_table.get(inner) {
                    let t = *t;
                    enum_expr = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(enum_expr),
                        },
                        t,
                        span,
                    );
                    inner = t;
                }
                let eq_cond = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::Eq,
                        left: Box::new(enum_expr),
                        right: Box::new(TirExpr::new(
                            TirExprKind::EnumConstruct {
                                enum_type: *enum_type,
                                case_index: *case_index,
                                case_name: case_name.clone(),
                            },
                            *enum_type,
                            span,
                        )),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let and_expr = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::And,
                        left: Box::new(eq_cond),
                        right: Box::new(continuation),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let block = TirBlock::new(
                    vec![let_stmt, TirStmt::new(TirStmtKind::Expr(and_expr), span)],
                    span,
                );
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__variant_{temp_index}");
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: temp_name.clone(),
                        local_index: temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: pattern_type,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                let mut variant_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: temp_name,
                    },
                    pattern_type,
                    span,
                );
                let mut inner = pattern_type;
                while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) = type_table.get(inner) {
                    let t = *t;
                    variant_expr = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(variant_expr),
                        },
                        t,
                        span,
                    );
                    inner = t;
                }

                let variant_type_info = match type_table.get(*enum_type) {
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    }
                    | ResolvedType::GenericInstance {
                        name,
                        module_source,
                        ..
                    } => Some((name.clone(), module_source.clone())),
                    _ => None,
                };
                let case_index_opt = variant_type_info
                    .as_ref()
                    .and_then(|(vt, ms)| self.get_case_index(vt, ms, variant_name));

                let Some(case_index) = case_index_opt else {
                    // Variant info not found; fall back to letting value be bound and
                    // continue with the continuation unconditionally.
                    let cont_stmt = TirStmt::new(TirStmtKind::Expr(continuation), span);
                    let block = TirBlock::new(vec![let_stmt, cont_stmt], span);
                    return TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span);
                };

                let variant_test = TirExpr::new(
                    TirExprKind::VariantTest {
                        expr: Box::new(variant_expr.clone()),
                        case_index,
                        case_name: variant_name.clone(),
                    },
                    TypeTable::BOOL,
                    span,
                );

                // Build the inner continuation that performs payload extraction
                // (if any) and then runs the outer continuation.
                let inner_cont = if let Some(binding) = bindings.first() {
                    if matches!(binding, TirPattern::Wildcard) {
                        continuation
                    } else {
                        let payload_expr = TirExpr::new(
                            TirExprKind::VariantPayload {
                                expr: Box::new(variant_expr),
                                case_index,
                                payload_type: *payload_type,
                            },
                            *payload_type,
                            span,
                        );
                        self.build_pattern_check(
                            binding,
                            payload_expr,
                            *payload_type,
                            span,
                            type_table,
                            continuation,
                        )
                    }
                } else {
                    continuation
                };

                let and_expr = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::And,
                        left: Box::new(variant_test),
                        right: Box::new(inner_cont),
                    },
                    TypeTable::BOOL,
                    span,
                );
                let block = TirBlock::new(
                    vec![let_stmt, TirStmt::new(TirStmtKind::Expr(and_expr), span)],
                    span,
                );
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Tuple(sub_patterns, _) => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__tup_{temp_index}");
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: temp_name.clone(),
                        local_index: temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: pattern_type,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );

                let elem_types = type_table
                    .as_tuple(pattern_type)
                    .unwrap_or_else(|| vec![TypeTable::UNKNOWN; sub_patterns.len()]);

                // Fold right: innermost check's continuation is the user continuation,
                // outer checks wrap around it.
                let mut current = continuation;
                for (i, sub) in sub_patterns.iter().enumerate().rev() {
                    let elem_type = elem_types.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                    let project = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: temp_index,
                                    name: temp_name.clone(),
                                },
                                pattern_type,
                                span,
                            )),
                            field_index: i as u32,
                            field_name: i.to_string(),
                        },
                        elem_type,
                        span,
                    );
                    current = self
                        .build_pattern_check(sub, project, elem_type, span, type_table, current);
                }

                let block = TirBlock::new(
                    vec![let_stmt, TirStmt::new(TirStmtKind::Expr(current), span)],
                    span,
                );
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Struct { fields, .. } => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__struct_{temp_index}");
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: temp_name.clone(),
                        local_index: temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: pattern_type,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );

                let struct_fields_info = self.get_struct_fields(pattern_type, type_table);

                let mut current = continuation;
                for field in fields.iter().rev() {
                    let field_type = struct_fields_info
                        .as_ref()
                        .and_then(|info| {
                            info.iter()
                                .find(|f| f.name == field.field_name)
                                .map(|f| f.type_id)
                        })
                        .unwrap_or(TypeTable::UNKNOWN);
                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: temp_index,
                                    name: temp_name.clone(),
                                },
                                pattern_type,
                                span,
                            )),
                            field_index: field.field_index,
                            field_name: field.field_name.clone(),
                        },
                        field_type,
                        span,
                    );
                    current = self.build_pattern_check(
                        &field.pattern,
                        field_access,
                        field_type,
                        span,
                        type_table,
                        current,
                    );
                }

                let block = TirBlock::new(
                    vec![let_stmt, TirStmt::new(TirStmtKind::Expr(current), span)],
                    span,
                );
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Or(_) | TirPattern::Range { .. } => {
                // Nested Or/Range patterns are not currently supported by this path.
                // They are handled at the top level via the existing match lowering.
                panic!("unsupported nested pattern in refutable extraction: {pattern:?}");
            }
        }
    }

    /// Build an equality condition: `local == literal_value`
    fn literal_eq_condition(
        &self,
        local_index: u32,
        local_type: TypeId,
        lit: &TirLiteralPattern,
        span: Span,
    ) -> TirExpr {
        let local_expr = TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name: format!("__lit_{local_index}"),
            },
            local_type,
            span,
        );

        let literal_expr = match lit {
            TirLiteralPattern::I128(val) => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: *val as u64,
                    repr: val.to_string(),
                },
                local_type,
                span,
            ),
            TirLiteralPattern::U128(val) => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: *val as u64,
                    repr: val.to_string(),
                },
                local_type,
                span,
            ),
            TirLiteralPattern::Bool(val) => {
                TirExpr::new(TirExprKind::BoolLiteral(*val), TypeTable::BOOL, span)
            }
            TirLiteralPattern::Char(val) => {
                TirExpr::new(TirExprKind::CharLiteral(*val), TypeTable::CHAR, span)
            }
            TirLiteralPattern::String(val) => {
                TirExpr::new(TirExprKind::StringLiteral(val.clone()), local_type, span)
            }
            TirLiteralPattern::Null => TirExpr::new(TirExprKind::Null, TypeTable::UNKNOWN, span),
        };

        // For String, use MethodCall to String^Eq::eq
        if matches!(lit, TirLiteralPattern::String(_)) {
            return self.string_eq_call(local_expr, literal_expr, local_type, span);
        }

        // For primitives, use binary ==
        TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::Eq,
                left: Box::new(local_expr),
                right: Box::new(literal_expr),
            },
            TypeTable::BOOL,
            span,
        )
    }

    /// Build a `String^Eq::eq(&self, &other)` method call expression.
    fn string_eq_call(
        &self,
        receiver: TirExpr,
        other: TirExpr,
        string_type: TypeId,
        span: Span,
    ) -> TirExpr {
        // Eq::eq expects (&self, &Self) — both receiver and argument are &String.
        // The WIR translate phase handles ref wrapping for method calls,
        // so we pass the values directly and let translate handle self-kind adjustment.
        // However, the arg explicitly needs &String since that's the method signature.
        let method_info = LocalMethodName::new(
            self.string_struct_name.clone(),
            Some(self.eq_trait_name.clone()),
            "eq".to_string(),
        );
        let mangled_name = method_info.to_mangled_name();
        TirExpr::new(
            TirExprKind::method_call(
                Box::new(receiver),
                FunctionRef {
                    module_source: ModuleSource::string(),
                    name: mangled_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                vec![],
                vec![CallArg::new(
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(other),
                        },
                        string_type, // &String, but type_id here is approximate
                        span,
                    ),
                    false,
                )],
            ),
            TypeTable::BOOL,
            span,
        )
    }

    /// Lower patterns in a block
    fn lower_block(&mut self, block: &mut TirBlock, type_table: &TypeTable) {
        // Process statements, potentially expanding LetDestructure into multiple statements
        let mut new_stmts = Vec::with_capacity(block.stmts.len());

        for stmt in std::mem::take(&mut block.stmts) {
            self.lower_stmt(stmt, &mut new_stmts, type_table);
        }

        block.stmts = new_stmts;
    }

    /// Lower a statement, potentially expanding it into multiple statements
    fn lower_stmt(&mut self, stmt: TirStmt, out: &mut Vec<TirStmt>, type_table: &TypeTable) {
        match stmt.kind {
            TirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => {
                // Don't lower multi-value builtin calls - codegen has a special optimization for them
                if self.is_multivalue_builtin_pattern(&pattern, &value, type_table) {
                    let mut value = value;
                    self.lower_expr(&mut value, type_table);
                    out.push(TirStmt::new(
                        TirStmtKind::LetDestructure {
                            pattern,
                            is_mut,
                            value,
                        },
                        stmt.span,
                    ));
                } else {
                    // Lower LetDestructure to explicit Let statements
                    self.lower_let_pattern(&pattern, is_mut, value, stmt.span, out, type_table);
                }
            }
            TirStmtKind::IfLet {
                mut scrutinee,
                pattern,
                mut then_block,
                else_block,
            } => {
                // Lower expressions in scrutinee first.
                self.lower_expr(&mut scrutinee, type_table);

                // Irrefutable binding/wildcard: `if let x = expr { body }`
                // is just a let-binding (or side-effect evaluation) followed
                // by the body. Generating a two-arm Match here would leave
                // the wildcard arm reachable in shape only — codegen would
                // emit dead code for it.
                if matches!(pattern, TirPattern::Binding { .. } | TirPattern::Wildcard) {
                    self.lower_let_pattern(&pattern, false, scrutinee, stmt.span, out, type_table);
                    self.lower_block(&mut then_block, type_table);
                    out.extend(then_block.stmts);
                    return;
                }

                // ConstantValue at the top level: `if let C = expr` reduces
                // to a direct equality `expr == C`. The Match path could
                // express it as `match expr { C => then, _ => else }`, but
                // the binary-comparison form skips a Match-shaped lowering
                // for what is fundamentally a single boolean test.
                if let TirPattern::ConstantValue { expr: const_expr } = &pattern {
                    let condition = TirExpr::new(
                        TirExprKind::Binary {
                            op: TirBinaryOp::Eq,
                            left: Box::new(scrutinee),
                            right: const_expr.clone(),
                        },
                        TypeTable::BOOL,
                        stmt.span,
                    );
                    self.lower_block(&mut then_block, type_table);
                    out.push(TirStmt::new(
                        TirStmtKind::If {
                            condition,
                            then_block,
                            else_block,
                        },
                        stmt.span,
                    ));
                    return;
                }

                // Refutable patterns (Variant / Enum / Struct / Tuple /
                // literal sub-patterns / ...): rewrite to a two-arm
                // `match` and let the Match path handle preprocessing
                // (or-pattern expansion, ref deref, refutable sub-pattern
                // extraction). After preprocessing the Match flows
                // through to NIR Match, where `wir_build::pattern_match`
                // generates the VariantTest / VariantPayload / etc. chain.
                //
                // The arm body types follow each branch's own
                // [`block_result_type`]; the Match's `type_id` follows
                // the resolver's rule for IfLet-as-value (both branches
                // agree, or one diverges as `NEVER`). For an IfLet with
                // no else, the surrounding `Block::block_result_type`
                // already collapses to `Unit`, so the Match is `Unit`
                // and the then-branch value is discarded.
                let span = stmt.span;

                // Hoist `mut`-binding pattern leaves into immutable temps
                // and rebind via prepended `Let` statements in the then
                // block. `wir_build::pattern_match` stores the payload to
                // the binding's local without consulting value-copy, so
                // a mutable binding on a value-semantic type would alias
                // the scrutinee payload. Routing through a `Let` lets
                // `lower::plan::value_copy::insert` wrap the value the
                // same way the old `lower_if_pattern_option` did.
                let mut pattern = pattern;
                let mut then_block = then_block;
                let prepend = self.hoist_mut_bindings(&mut pattern, span);
                if !prepend.is_empty() {
                    let mut stmts = prepend;
                    stmts.extend(then_block.stmts);
                    then_block.stmts = stmts;
                }

                let then_type = block_result_type(&then_block);
                let else_type = else_block.as_ref().map(block_result_type);

                let then_body = TirExpr::new(TirExprKind::Block(then_block), then_type, span);
                let else_body = match else_block {
                    Some(b) => TirExpr::new(TirExprKind::Block(b), else_type.unwrap(), span),
                    None => TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                };

                let match_type = match else_type {
                    Some(et) if then_type == et => then_type,
                    Some(et) if then_type == TypeTable::NEVER => et,
                    Some(et) if et == TypeTable::NEVER => then_type,
                    _ => TypeTable::UNIT,
                };

                let mut match_expr = TirExpr::new(
                    TirExprKind::Match {
                        expr: Box::new(scrutinee),
                        arms: vec![
                            TirMatchArm {
                                pattern,
                                guard: None,
                                body: then_body,
                                span,
                            },
                            TirMatchArm {
                                pattern: TirPattern::Wildcard,
                                guard: None,
                                body: else_body,
                                span,
                            },
                        ],
                    },
                    match_type,
                    span,
                );
                self.lower_expr(&mut match_expr, type_table);
                out.push(TirStmt::new(TirStmtKind::Expr(match_expr), stmt.span));
            }
            TirStmtKind::Let {
                value,
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                ..
            } => {
                // Lower expressions inside the Let value
                let mut value = value;
                self.lower_expr(&mut value, type_table);
                out.push(TirStmt::new(
                    TirStmtKind::Let {
                        name,
                        local_index,
                        is_mut,
                        is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Expr(mut expr) => {
                self.lower_expr(&mut expr, type_table);
                out.push(TirStmt::new(TirStmtKind::Expr(expr), stmt.span));
            }
            TirStmtKind::Return { value } => {
                let value = value.map(|mut v| {
                    self.lower_expr(&mut v, type_table);
                    v
                });
                out.push(TirStmt::new(TirStmtKind::Return { value }, stmt.span));
            }
            TirStmtKind::If {
                condition,
                mut then_block,
                mut else_block,
            } => {
                let mut condition = condition;
                self.lower_expr(&mut condition, type_table);
                self.lower_block(&mut then_block, type_table);
                if let Some(ref mut else_blk) = else_block {
                    self.lower_block(else_blk, type_table);
                }
                out.push(TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Loop { mut body } => {
                self.lower_block(&mut body, type_table);
                out.push(TirStmt::new(TirStmtKind::Loop { body }, stmt.span));
            }
            TirStmtKind::LabeledBlock { label, mut block } => {
                self.lower_block(&mut block, type_table);
                out.push(TirStmt::new(
                    TirStmtKind::LabeledBlock { label, block },
                    stmt.span,
                ));
            }
            TirStmtKind::Break { label, value } => {
                let value = value.map(|mut v| {
                    self.lower_expr(&mut v, type_table);
                    v
                });
                out.push(TirStmt::new(TirStmtKind::Break { label, value }, stmt.span));
            }
            TirStmtKind::Continue => {
                out.push(TirStmt::new(TirStmtKind::Continue, stmt.span));
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    /// Lower `LetDestructure` to explicit Let statements
    fn lower_let_pattern(
        &mut self,
        pattern: &TirPattern,
        is_mut: bool,
        value: TirExpr,
        span: Span,
        out: &mut Vec<TirStmt>,
        type_table: &TypeTable,
    ) {
        // First, lower any expressions inside the value
        let mut value = value;
        self.lower_expr(&mut value, type_table);

        // Match ergonomics for let patterns: if the value is a reference type
        // but the pattern is a compound (tuple/struct), deref the value first.
        let value = match pattern {
            TirPattern::Tuple(_, _) | TirPattern::Struct { .. } => {
                let mut current = value;
                loop {
                    match type_table.get(current.type_id).clone() {
                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                            current = TirExpr::new(
                                TirExprKind::Unary {
                                    op: TirUnaryOp::Deref,
                                    expr: Box::new(current),
                                },
                                inner,
                                span,
                            );
                        }
                        _ => break current,
                    }
                }
            }
            _ => value,
        };

        match pattern {
            TirPattern::Tuple(sub_patterns, _) => {
                // Allocate temp local for the tuple
                let tuple_temp_index = self.alloc_local(value.type_id);
                let tuple_temp_name = self.next_temp_name();

                // Create Let for the tuple
                let tuple_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: tuple_temp_name.clone(),
                        local_index: tuple_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(tuple_let);

                // Get element types
                let elem_types = type_table
                    .as_tuple(type_table.get_local_type(tuple_temp_index, &self.locals))
                    .unwrap_or_else(|| vec![TypeTable::UNKNOWN; sub_patterns.len()]);

                // Project each element via FieldAccess. SROA / DCE later
                // elide the temp + struct.new when the temp doesn't escape,
                // so we don't force tuple destructures through a heap
                // allocation.
                for (i, (sub_pattern, elem_type)) in
                    sub_patterns.iter().zip(elem_types.iter()).enumerate()
                {
                    let project = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: tuple_temp_index,
                                    name: tuple_temp_name.clone(),
                                },
                                type_table.get_local_type(tuple_temp_index, &self.locals),
                                span,
                            )),
                            field_index: i as u32,
                            field_name: i.to_string(),
                        },
                        *elem_type,
                        span,
                    );

                    self.lower_pattern_to_lets(sub_pattern, is_mut, project, span, out, type_table);
                }
            }
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                // Match ergonomics: if binding type is &T or &mut T but value
                // is a non-reference T, wrap in Ref/MutRef.
                let value = {
                    let binding_resolved = type_table.get(*type_id).clone();
                    let value_is_ref = matches!(
                        type_table.get(value.type_id),
                        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                    );
                    if value_is_ref {
                        value
                    } else {
                        match binding_resolved {
                            ResolvedType::Ref(_) => TirExpr::new(
                                TirExprKind::Unary {
                                    op: TirUnaryOp::Ref,
                                    expr: Box::new(value),
                                },
                                *type_id,
                                span,
                            ),
                            ResolvedType::MutRef(_) => TirExpr::new(
                                TirExprKind::Unary {
                                    op: TirUnaryOp::MutRef,
                                    expr: Box::new(value),
                                },
                                *type_id,
                                span,
                            ),
                            _ => value,
                        }
                    }
                };
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: *local_index,
                        is_mut,
                        is_reactive: false,
                        type_id: *type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(let_stmt);
            }
            TirPattern::Wildcard => {
                // Evaluate value for side effects but discard
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
            TirPattern::Variant {
                bindings,
                payload_type,
                ..
            } => {
                // For variant patterns in LetDestructure, extract payload
                // Allocate temp for variant
                let variant_temp_index = self.alloc_local(value.type_id);
                let variant_temp_name = self.next_temp_name();

                let variant_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: variant_temp_name.clone(),
                        local_index: variant_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(variant_let);

                // If there are bindings, extract payload
                if let Some(binding) = bindings.first() {
                    let payload_expr = TirExpr::new(
                        TirExprKind::VariantPayload {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: variant_temp_index,
                                    name: variant_temp_name,
                                },
                                type_table.get_local_type(variant_temp_index, &self.locals),
                                span,
                            )),
                            case_index: 0, // Will be refined when we have more info
                            payload_type: *payload_type,
                        },
                        *payload_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        binding,
                        is_mut,
                        payload_expr,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Struct { fields, .. } => {
                // Allocate temp local for the struct
                let struct_temp_index = self.alloc_local(value.type_id);
                let struct_temp_name = self.next_temp_name();

                let struct_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: struct_temp_name.clone(),
                        local_index: struct_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(struct_let);

                // Get field type info from struct definition
                let struct_fields_info = self.get_struct_fields(
                    type_table.get_local_type(struct_temp_index, &self.locals),
                    type_table,
                );

                for field in fields {
                    let field_type = struct_fields_info
                        .as_ref()
                        .and_then(|info| {
                            info.iter()
                                .find(|f| f.name == field.field_name)
                                .map(|f| f.type_id)
                        })
                        .unwrap_or(TypeTable::UNKNOWN);

                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: struct_temp_index,
                                    name: struct_temp_name.clone(),
                                },
                                type_table.get_local_type(struct_temp_index, &self.locals),
                                span,
                            )),
                            field_index: field.field_index,
                            field_name: field.field_name.clone(),
                        },
                        field_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        &field.pattern,
                        is_mut,
                        field_access,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {
                // Literal/Enum/ConstantValue/Range patterns don't bind anything, just evaluate for side effects
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
            TirPattern::Or(alternatives) => {
                // Or patterns in let-destructure: use first alternative's bindings
                if let Some(first) = alternatives.first() {
                    self.lower_pattern_to_lets(first, is_mut, value, span, out, type_table);
                }
            }
        }
    }

    /// Helper to lower a pattern to Let statements given an already-evaluated value
    fn lower_pattern_to_lets(
        &mut self,
        pattern: &TirPattern,
        is_mut: bool,
        value: TirExpr,
        span: Span,
        out: &mut Vec<TirStmt>,
        type_table: &TypeTable,
    ) {
        match pattern {
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                // Match ergonomics: if binding type is &T or &mut T but value type
                // is a non-reference T, wrap value in a Ref/MutRef operation.
                let value = {
                    let binding_resolved = type_table.get(*type_id).clone();
                    let value_is_ref = matches!(
                        type_table.get(value.type_id),
                        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                    );
                    if value_is_ref {
                        value
                    } else {
                        match binding_resolved {
                            ResolvedType::Ref(_) => TirExpr::new(
                                TirExprKind::Unary {
                                    op: TirUnaryOp::Ref,
                                    expr: Box::new(value),
                                },
                                *type_id,
                                span,
                            ),
                            ResolvedType::MutRef(_) => TirExpr::new(
                                TirExprKind::Unary {
                                    op: TirUnaryOp::MutRef,
                                    expr: Box::new(value),
                                },
                                *type_id,
                                span,
                            ),
                            _ => value,
                        }
                    }
                };
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: *local_index,
                        is_mut,
                        is_reactive: false,
                        type_id: *type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(let_stmt);
            }
            TirPattern::Tuple(sub_patterns, _) => {
                // Nested tuple - allocate temp and recurse
                let tuple_temp_index = self.alloc_local(value.type_id);
                let tuple_temp_name = self.next_temp_name();

                let tuple_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: tuple_temp_name.clone(),
                        local_index: tuple_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(tuple_let);

                let elem_types = type_table
                    .as_tuple(type_table.get_local_type(tuple_temp_index, &self.locals))
                    .unwrap_or_else(|| vec![TypeTable::UNKNOWN; sub_patterns.len()]);

                for (i, (sub_pattern, elem_type)) in
                    sub_patterns.iter().zip(elem_types.iter()).enumerate()
                {
                    let project = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: tuple_temp_index,
                                    name: tuple_temp_name.clone(),
                                },
                                type_table.get_local_type(tuple_temp_index, &self.locals),
                                span,
                            )),
                            field_index: i as u32,
                            field_name: i.to_string(),
                        },
                        *elem_type,
                        span,
                    );

                    self.lower_pattern_to_lets(sub_pattern, is_mut, project, span, out, type_table);
                }
            }
            TirPattern::Wildcard => {
                // Discard value — emit as expression statement. The WIR
                // translation wraps non-unit Expr statements in Drop to
                // consume stack values. For unit-typed expressions (e.g.,
                // Ok(_) payload in Result<(), E>), the expression should
                // not produce a value on the wasm stack.
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
            TirPattern::Variant {
                bindings,
                payload_type,
                ..
            } => {
                if let Some(binding) = bindings.first()
                    // Skip payload extraction for unit-type wildcards — there is
                    // no payload to extract, and the VariantPayload WIR translation
                    // would emit a struct.get that leaves a dangling value on stack.
                    && !(*payload_type == TypeTable::UNIT
                        && matches!(binding, TirPattern::Wildcard))
                {
                    // Allocate temp and extract payload
                    let variant_temp_index = self.alloc_local(value.type_id);
                    let variant_temp_name = self.next_temp_name();

                    let variant_let = TirStmt::new(
                        TirStmtKind::Let {
                            name: variant_temp_name.clone(),
                            local_index: variant_temp_index,
                            is_mut: false,
                            is_reactive: false,
                            type_id: value.type_id,
                            value,
                            skip_value_copy: false,
                        },
                        span,
                    );
                    out.push(variant_let);

                    let payload_expr = TirExpr::new(
                        TirExprKind::VariantPayload {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: variant_temp_index,
                                    name: variant_temp_name,
                                },
                                type_table.get_local_type(variant_temp_index, &self.locals),
                                span,
                            )),
                            case_index: 0,
                            payload_type: *payload_type,
                        },
                        *payload_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        binding,
                        is_mut,
                        payload_expr,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Struct { fields, .. } => {
                // Nested struct - allocate temp and recurse
                let struct_temp_index = self.alloc_local(value.type_id);
                let struct_temp_name = self.next_temp_name();

                let struct_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: struct_temp_name.clone(),
                        local_index: struct_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(struct_let);

                let struct_fields_info = self.get_struct_fields(
                    type_table.get_local_type(struct_temp_index, &self.locals),
                    type_table,
                );

                for field in fields {
                    let field_type = struct_fields_info
                        .as_ref()
                        .and_then(|info| {
                            info.iter()
                                .find(|f| f.name == field.field_name)
                                .map(|f| f.type_id)
                        })
                        .unwrap_or(TypeTable::UNKNOWN);

                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: struct_temp_index,
                                    name: struct_temp_name.clone(),
                                },
                                type_table.get_local_type(struct_temp_index, &self.locals),
                                span,
                            )),
                            field_index: field.field_index,
                            field_name: field.field_name.clone(),
                        },
                        field_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        &field.pattern,
                        is_mut,
                        field_access,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {
                // Just evaluate for side effects (no bindings)
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
            TirPattern::Or(alternatives) => {
                // Or patterns in lets: use first alternative's bindings
                if let Some(first) = alternatives.first() {
                    self.lower_pattern_to_lets(first, is_mut, value, span, out, type_table);
                }
            }
        }
    }

    /// Lower expressions (recurse into sub-expressions)
    fn lower_expr(&mut self, expr: &mut TirExpr, type_table: &TypeTable) {
        match &mut expr.kind {
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.lower_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.lower_expr(condition, type_table);
                self.lower_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.lower_block(else_blk, type_table);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                // First, recursively lower sub-expressions
                self.lower_expr(scrutinee, type_table);
                for arm in arms.iter_mut() {
                    if let Some(guard) = &mut arm.guard {
                        self.lower_expr(guard, type_table);
                    }
                    self.lower_expr(&mut arm.body, type_table);
                }

                // Expand or-patterns: `A | B => body` becomes `A => body, B => body`
                let mut expanded_arms = Vec::new();
                for arm in arms.drain(..) {
                    if let TirPattern::Or(alternatives) = arm.pattern {
                        for alt in alternatives {
                            expanded_arms.push(TirMatchArm {
                                pattern: alt,
                                guard: arm.guard.clone(),
                                body: arm.body.clone(),
                                span: arm.span,
                            });
                        }
                    } else {
                        expanded_arms.push(arm);
                    }
                }
                *arms = expanded_arms;

                // Match ergonomics: insert deref if scrutinee is Ref/MutRef
                while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(scrutinee.type_id)
                {
                    let inner = *inner;
                    let span = scrutinee.span;
                    let old = std::mem::replace(
                        scrutinee.as_mut(),
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                    );
                    *scrutinee.as_mut() = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(old),
                        },
                        inner,
                        span,
                    );
                }

                // Extract literal sub-patterns from tuple/struct patterns into guards
                let scrutinee_type_id = scrutinee.type_id;
                for arm in arms.iter_mut() {
                    self.extract_refutable_sub_patterns(arm, scrutinee_type_id, type_table);
                }

                // Lower top-level string literal patterns into binding + guard
                for arm in arms.iter_mut() {
                    if matches!(
                        &arm.pattern,
                        TirPattern::Literal(TirLiteralPattern::String(_))
                    ) {
                        let span = arm.span;
                        let temp_index = self.alloc_local(scrutinee_type_id);
                        let lit = match &arm.pattern {
                            TirPattern::Literal(lit) => lit.clone(),
                            _ => unreachable!(),
                        };
                        let cond =
                            self.literal_eq_condition(temp_index, scrutinee_type_id, &lit, span);
                        arm.pattern = TirPattern::Binding {
                            name: format!("__lit_{temp_index}"),
                            local_index: temp_index,
                            type_id: scrutinee_type_id,
                        };
                        arm.guard = Some(match arm.guard.take() {
                            Some(existing) => TirExpr::new(
                                TirExprKind::Binary {
                                    op: TirBinaryOp::And,
                                    left: Box::new(cond),
                                    right: Box::new(existing),
                                },
                                TypeTable::BOOL,
                                span,
                            ),
                            None => cond,
                        });
                    }
                }

                // Lower top-level constant value patterns into binding + guard
                for arm in arms.iter_mut() {
                    if let TirPattern::ConstantValue { expr: const_expr } = &arm.pattern {
                        let span = arm.span;
                        let temp_index = self.alloc_local(scrutinee_type_id);
                        let local_expr = TirExpr::new(
                            TirExprKind::Local {
                                index: temp_index,
                                name: format!("__const_{temp_index}"),
                            },
                            scrutinee_type_id,
                            span,
                        );
                        let cond = TirExpr::new(
                            TirExprKind::Binary {
                                op: TirBinaryOp::Eq,
                                left: Box::new(local_expr),
                                right: const_expr.clone(),
                            },
                            TypeTable::BOOL,
                            span,
                        );
                        arm.pattern = TirPattern::Binding {
                            name: format!("__const_{temp_index}"),
                            local_index: temp_index,
                            type_id: scrutinee_type_id,
                        };
                        arm.guard = Some(match arm.guard.take() {
                            Some(existing) => TirExpr::new(
                                TirExprKind::Binary {
                                    op: TirBinaryOp::And,
                                    left: Box::new(cond),
                                    right: Box::new(existing),
                                },
                                TypeTable::BOOL,
                                span,
                            ),
                            None => cond,
                        });
                    }
                }

                // Switch conversion (dense integer / enum `Match` →
                // `br_table`) is performed in the TIR → NIR translator
                // (`lower::translate::switch`).
            }
            TirExprKind::Binary { left, right, .. } => {
                self.lower_expr(left, type_table);
                self.lower_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                self.lower_expr(inner, type_table);
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    self.lower_expr(&mut arg.expr, type_table);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.lower_expr(arg, type_table);
                }
            }
            TirExprKind::Index { expr: arr, index }
            | TirExprKind::Assign {
                target: arr,
                value: index,
            } => {
                self.lower_expr(arr, type_table);
                self.lower_expr(index, type_table);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.lower_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.lower_expr(elem, type_table);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.lower_expr(callee, type_table);
                for arg in args {
                    self.lower_expr(arg, type_table);
                }
            }
            TirExprKind::Closure {
                params,
                body_locals,
                body,
                ..
            } => {
                // Closures have their own local-index namespace — the
                // resolver builds each closure's `FunctionContext` with a
                // fresh `next_local: 0`, so the body's `Local` / `Let`
                // indices are independent of the outer function's.
                // Pattern lowering must honour that: any temp it allocates
                // while descending into the body has to live in the
                // closure's namespace, otherwise the closure-functor
                // lowering pass (`lower/closure.rs`) builds a `local_types`
                // table where the temp's outer-scoped index collides with
                // a real closure-scoped local, producing a closure body
                // whose `LocalSet` targets the wrong slot.
                //
                // The closure carries its parameter list and the types of
                // its body-level let-bindings (`body_locals`); the
                // closure-scope state is their concatenation. Temps
                // pattern lowering allocates while descending grow the
                // local maps for the duration of the visit, then the
                // closure-functor lowering pass re-collects them from the
                // body's `Let`s via `collect_locals_from_block`, so we
                // discard the updated state on the way out.
                let saved_count = self.local_count;
                let saved_locals = std::mem::take(&mut self.locals);
                self.local_count = (params.len() + body_locals.len()) as u32;
                self.locals = params
                    .iter()
                    .map(|(name, ty)| TirLocal {
                        name: name.clone(),
                        type_id: *ty,
                        is_mut: false,
                    })
                    .chain(body_locals.iter().cloned())
                    .collect();

                self.lower_expr(body, type_table);

                self.local_count = saved_count;
                self.locals = saved_locals;
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.lower_expr(p, type_table);
                }
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.lower_expr(value, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.lower_expr(scrutinee, type_table);
                for arm in arms {
                    self.lower_block(arm, type_table);
                }
                self.lower_block(default, type_table);
            }
            // Terminals
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }
}

/// Helper trait extension for `TypeTable` to get local type
trait TypeTableExt {
    fn get_local_type(&self, index: u32, locals: &[TirLocal]) -> TypeId;
}

impl TypeTableExt for TypeTable {
    fn get_local_type(&self, index: u32, locals: &[TirLocal]) -> TypeId {
        locals
            .get(index as usize)
            .map(|l| l.type_id)
            .unwrap_or(TypeTable::UNKNOWN)
    }
}
