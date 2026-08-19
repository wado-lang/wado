use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName};
use crate::tir::FunctionRef;
use crate::tir::{
    CallArg, PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirField,
    TirFunction, TirLiteralPattern, TirLocal, TirMatchArm, TirPattern, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Coerce a by-value `value` to a `&T` / `&mut T` pattern binding.
///
/// Match ergonomics binds a reference to a value scrutinee. Pattern
/// lowering runs after `boxing::prepare_types`, so the binding's
/// reference type may have been redefined to a `Box<T>` struct —
/// in that case we materialise a `Box{value}` literal directly
/// rather than a `Ref` / `MutRef` unary the fold would have to
/// rewrite afterwards. A `value` that is already a reference is
/// returned unchanged.
fn coerce_value_to_binding(
    value: TirExpr,
    binding_type: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> TirExpr {
    let value_is_ref = matches!(
        type_table.get(value.type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    ) || type_table.box_payload_of(value.type_id).is_some();
    if value_is_ref {
        return value;
    }
    match type_table.get(binding_type) {
        ResolvedType::Ref(_) => TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: Box::new(value),
            },
            binding_type,
            span,
        ),
        ResolvedType::MutRef(_) => TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: Box::new(value),
            },
            binding_type,
            span,
        ),
        // A `&primitive` / `&variant` / `&fn` binding type that boxing
        // has redefined to its `Box<T>` struct: build the struct literal.
        ResolvedType::Struct { def, type_args }
            if type_table.box_payload_of(binding_type).is_some() =>
        {
            let struct_name = type_table.struct_rendered_name(*def, type_args);
            TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type: binding_type,
                    struct_name,
                    fields: vec![crate::tir::TirStructField {
                        name: "value".to_string(),
                        value,
                        field_index: 0,
                    }],
                },
                binding_type,
                span,
            )
        }
        _ => value,
    }
}

/// Peel `Ref` / `MutRef` and `Box<T>` wrappers off `expr`, returning the
/// unwrapped expression and its type — a `Deref` for the former, a `.value`
/// field access for the latter. `boxing::prepare_types` redefines every
/// `Ref(boxable)` to its `Box<T>`, so post-boxing a reference scrutinee arrives
/// box-shaped; pre-boxing the registry is empty and this is a plain `Ref` peel.
fn peel_refs_and_box(
    mut expr: TirExpr,
    mut type_id: TypeId,
    type_table: &TypeTable,
    span: Span,
) -> (TirExpr, TypeId) {
    loop {
        match type_table.get(type_id) {
            ResolvedType::Ref(t) | ResolvedType::MutRef(t) => {
                let t = *t;
                expr = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: Box::new(expr),
                    },
                    t,
                    span,
                );
                type_id = t;
            }
            _ => match type_table.box_payload_of(type_id) {
                Some(payload) => {
                    expr = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(expr),
                            field_index: 0,
                            field_name: "value".to_string(),
                        },
                        payload,
                        span,
                    );
                    type_id = payload;
                }
                None => return (expr, type_id),
            },
        }
    }
}

/// Package-level facts pattern lowering needs, gathered once so
/// [`Lowering::lower_function`] can run inside the translator's per-function
/// walk. Lowering rewrites `LetDestructure` / `IfLet` into `Let` + `Match`
/// chains and expands or-patterns. It runs after all of `lower::plan`, so
/// scrutinees and synthesised references alike arrive box-shaped.
pub struct Lowering {
    /// Map from the `variant` declaration to its (`case_name`, `case_index`)
    /// pairs.
    variant_case_map: IndexMap<crate::defs::DefId, Vec<(String, u32)>>,
    /// Map from a struct type's head-and-args to its field definitions.
    struct_fields_map: IndexMap<(crate::tir::StructDef, Vec<TypeId>), Vec<TirField>>,
    /// Canonical stdlib name of the `Eq` trait.
    eq_trait_name: crate::name::FqTraitName,
    /// Canonical stdlib name of the `String` struct.
    string_struct_name: FqTypeName,
    /// Immutable globals with a bare integer-literal initializer, keyed by `(module, name)`.
    const_int_globals: IndexMap<(ModuleSource, String), i128>,
}

impl Lowering {
    /// Gather the package-level maps once, before the translator's
    /// per-function walk begins.
    pub fn new(flat: &FlatPackage) -> Self {
        let mut variant_case_map: IndexMap<crate::defs::DefId, Vec<(String, u32)>> =
            IndexMap::default();
        for variant in &flat.variants {
            let cases: Vec<(String, u32)> = variant
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index))
                .collect();
            variant_case_map.insert(variant.def, cases);
        }

        let mut struct_fields_map: IndexMap<(crate::tir::StructDef, Vec<TypeId>), Vec<TirField>> =
            IndexMap::default();
        for s in &flat.structs {
            struct_fields_map.insert((s.def, s.type_args.clone()), s.fields.clone());
        }

        let mut const_int_globals: IndexMap<(ModuleSource, String), i128> = IndexMap::default();
        for g in &flat.globals {
            // A `global mut` can change, and a deferred global holds a
            // placeholder rather than its value — neither is a constant.
            if !g.wado_mutable
                && let Some(declared) = g.init.declared()
                && let TirExprKind::IntLiteral { value, .. } = &declared.kind
            {
                const_int_globals.insert(
                    (g.module_source.clone(), g.name.clone()),
                    i128::from(*value),
                );
            }
        }

        let type_table = flat.type_table.borrow();
        let eq_trait_name = type_table.compiler_trait_fq(crate::compiler_item::CompilerItem::Eq);
        let string_struct_name =
            type_table.compiler_struct_fq_name(crate::compiler_item::CompilerItem::String);
        Self {
            variant_case_map,
            struct_fields_map,
            eq_trait_name,
            string_struct_name,
            const_int_globals,
        }
    }

    /// Lower the patterns in one function's body in place.
    pub fn lower_function(&self, func: &mut TirFunction, type_table: &TypeTable) {
        let Some(mut body) = func.body.take() else {
            return;
        };
        // Take ownership of the locals to avoid borrow conflicts.
        let local_count = func.local_count;
        let locals = std::mem::take(&mut func.locals);

        let mut lowerer = PatternLowerer::new(
            local_count,
            locals,
            self.eq_trait_name.clone(),
            self.string_struct_name.clone(),
            &self.variant_case_map,
            &self.struct_fields_map,
            &self.const_int_globals,
        );
        lowerer.lower_block(&mut body, type_table);

        let (new_count, new_locals) = lowerer.into_parts();
        func.local_count = new_count;
        func.locals = new_locals;
        func.body = Some(body);
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
    eq_trait_name: crate::name::FqTraitName,
    /// Canonical stdlib name of the `String` struct, resolved through
    /// the same registry so the receiver-type slot of the synthesised
    /// `String^Eq::eq` `LocalMethodName` tracks renames too.
    string_struct_name: FqTypeName,
    /// Map from the `variant` declaration to a list of (`case_name`,
    /// `case_index`) pairs; the scrutinee's type names the declaration.
    variant_case_map: &'a IndexMap<crate::defs::DefId, Vec<(String, u32)>>,
    /// Map from a struct type's head-and-args to its field definitions.
    struct_fields_map: &'a IndexMap<(crate::tir::StructDef, Vec<TypeId>), Vec<TirField>>,
    /// Immutable integer-literal globals; see `Lowering::const_int_globals`.
    const_int_globals: &'a IndexMap<(ModuleSource, String), i128>,
    /// Locals bound to a scrutinee here. Their `Let` goes through the fold, so
    /// each already reads a place nothing can write.
    owned_temps: IndexSet<u32>,
}

/// A projection chain rooted at a local. Anything else is a temporary the
/// expression alone holds.
fn is_place(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::Local { .. } => true,
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref,
            expr: inner,
        } => is_place(inner),
        _ => false,
    }
}

/// Such a binding aliases the place it reads, so it must read one nothing can
/// write.
fn binds_by_value(pattern: &TirPattern, type_table: &TypeTable) -> bool {
    match pattern {
        TirPattern::Binding { type_id, .. } => {
            crate::lower::plan::value_copy::needs_value_copy(*type_id, type_table)
        }
        TirPattern::Tuple(sub, _)
        | TirPattern::Variant { bindings: sub, .. }
        | TirPattern::Or(sub) => sub.iter().any(|p| binds_by_value(p, type_table)),
        TirPattern::Struct { fields, .. } => fields
            .iter()
            .any(|f| binds_by_value(&f.pattern, type_table)),
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => false,
    }
}

/// The type a compound pattern's temp holds: the scrutinee's, match-ergonomic
/// references peeled. Shared with the value-copy seed walk, which mints no temp
/// of its own and would otherwise spell the peel a second time.
pub(crate) fn pattern_temp_type(
    pattern: &TirPattern,
    value_type: TypeId,
    type_table: &TypeTable,
) -> TypeId {
    if !matches!(pattern, TirPattern::Tuple(_, _) | TirPattern::Struct { .. }) {
        return value_type;
    }
    let mut current = value_type;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = *type_table.get(current) {
        current = inner;
    }
    current
}

impl<'a> PatternLowerer<'a> {
    fn new(
        local_count: u32,
        locals: Vec<TirLocal>,
        eq_trait_name: crate::name::FqTraitName,
        string_struct_name: FqTypeName,
        variant_case_map: &'a IndexMap<crate::defs::DefId, Vec<(String, u32)>>,
        struct_fields_map: &'a IndexMap<(crate::tir::StructDef, Vec<TypeId>), Vec<TirField>>,
        const_int_globals: &'a IndexMap<(ModuleSource, String), i128>,
    ) -> Self {
        Self {
            local_count,
            locals,
            temp_counter: 0,
            eq_trait_name,
            string_struct_name,
            variant_case_map,
            struct_fields_map,
            const_int_globals,
            owned_temps: IndexSet::default(),
        }
    }

    /// Look up the case index for a case of `def`.
    fn get_case_index(&self, def: crate::defs::DefId, case_name: &str) -> Option<u32> {
        self.variant_case_map
            .get(&def)
            .and_then(|cases| cases.iter().find(|(name, _)| name == case_name))
            .map(|(_, index)| *index)
    }

    /// Look up struct field definitions by `type_id`
    fn get_struct_fields(&self, type_id: TypeId, type_table: &TypeTable) -> Option<Vec<TirField>> {
        match type_table.get(type_id) {
            ResolvedType::Struct { def, type_args } => self
                .struct_fields_map
                .get(&(*def, type_args.clone()))
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
            TirPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                let (start, end, inclusive) = (*start, *end, *inclusive);
                let temp_index = self.alloc_local(elem_type);
                let temp_name = format!("__range_{temp_index}");

                let cond = self.range_condition(
                    temp_index, &temp_name, elem_type, start, end, inclusive, span,
                );
                conditions.push(cond);

                *sub = TirPattern::Binding {
                    name: temp_name,
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
                    peel_refs_and_box(local, elem_type, type_table, span).0
                };

                // Generate VariantTest condition
                let variant_def = match type_table.get(*enum_type) {
                    ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => {
                        type_table.nominal_def(*enum_type)
                    }
                    _ => None,
                };

                if let Some(def) = variant_def
                    && let Some(case_index) = self.get_case_index(def, variant_name)
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
                    peel_refs_and_box(local, elem_type, type_table, span).0
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
                let enum_local = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: temp_name,
                    },
                    pattern_type,
                    span,
                );
                let (enum_expr, _) = peel_refs_and_box(enum_local, pattern_type, type_table, span);
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
                let variant_local = TirExpr::new(
                    TirExprKind::Local {
                        index: temp_index,
                        name: temp_name,
                    },
                    pattern_type,
                    span,
                );
                let (variant_expr, _) =
                    peel_refs_and_box(variant_local, pattern_type, type_table, span);

                let variant_def = match type_table.get(*enum_type) {
                    ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => {
                        type_table.nominal_def(*enum_type)
                    }
                    _ => None,
                };
                let case_index_opt =
                    variant_def.and_then(|def| self.get_case_index(def, variant_name));

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
            TirPattern::Or(alternatives) => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__or_{temp_index}");
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

                // Alternatives are tried in order, first match wins, so each
                // carries its own copy of the continuation — that is what
                // re-tests the rest of the pattern per alternative.
                let mut checks =
                    TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, span);
                for alternative in alternatives.iter().rev() {
                    let bound = TirExpr::new(
                        TirExprKind::Local {
                            index: temp_index,
                            name: temp_name.clone(),
                        },
                        pattern_type,
                        span,
                    );
                    let check = self.build_pattern_check(
                        alternative,
                        bound,
                        pattern_type,
                        span,
                        type_table,
                        continuation.clone(),
                    );
                    checks = TirExpr::new(
                        TirExprKind::Binary {
                            op: TirBinaryOp::Or,
                            left: Box::new(check),
                            right: Box::new(checks),
                        },
                        TypeTable::BOOL,
                        span,
                    );
                }

                let block = TirBlock::new(
                    vec![let_stmt, TirStmt::new(TirStmtKind::Expr(checks), span)],
                    span,
                );
                TirExpr::new(TirExprKind::Block(block), TypeTable::BOOL, span)
            }
            TirPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                let temp_index = self.alloc_local(pattern_type);
                let temp_name = format!("__range_{temp_index}");
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
                let range_cond = self.range_condition(
                    temp_index,
                    &temp_name,
                    pattern_type,
                    *start,
                    *end,
                    *inclusive,
                    span,
                );
                let and_expr = TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::And,
                        left: Box::new(range_cond),
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
        }
    }

    /// Build a range condition: `local >= start && local <(=) end`. A `char`
    /// range carries its bounds as code points, so they go back to char
    /// literals to keep the comparison well-typed.
    #[allow(clippy::too_many_arguments)]
    fn range_condition(
        &self,
        local_index: u32,
        local_name: &str,
        local_type: TypeId,
        start: i128,
        end: i128,
        inclusive: bool,
        span: Span,
    ) -> TirExpr {
        let local = || {
            TirExpr::new(
                TirExprKind::Local {
                    index: local_index,
                    name: local_name.to_string(),
                },
                local_type,
                span,
            )
        };
        let bound = |value: i128| {
            if local_type == TypeTable::CHAR {
                let code = u32::try_from(value).ok().and_then(char::from_u32);
                TirExpr::new(
                    TirExprKind::CharLiteral(code.expect("a char range bound is a code point")),
                    TypeTable::CHAR,
                    span,
                )
            } else {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: value as u64,
                        repr: value.to_string(),
                    },
                    local_type,
                    span,
                )
            }
        };
        let compare = |op, right| {
            TirExpr::new(
                TirExprKind::Binary {
                    op,
                    left: Box::new(local()),
                    right: Box::new(right),
                },
                TypeTable::BOOL,
                span,
            )
        };
        let upper = if inclusive {
            TirBinaryOp::LtEq
        } else {
            TirBinaryOp::Lt
        };
        TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::And,
                left: Box::new(compare(TirBinaryOp::GtEq, bound(start))),
                right: Box::new(compare(upper, bound(end))),
            },
            TypeTable::BOOL,
            span,
        )
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

        // For String, use a method call to String^Eq::eq
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
            TirStmtKind::Let {
                value,
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                skip_value_copy,
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
                        skip_value_copy,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Expr(mut expr) => {
                // Two reasons: `labeled_block_fusion` keys on the
                // `(Let, Match)` pair and stops firing on an inline scrutinee,
                // and an owning arm binding needs a scrutinee nothing can
                // write. A place scrutinee needs neither: the arms project it
                // where it lies, and fusion's producers are calls. Hoisting one
                // made `match *r` deep-copy what `match r` reads in place.
                if let TirExprKind::Match {
                    expr: scrutinee,
                    arms,
                    ..
                } = &mut expr.kind
                    && (!is_place(scrutinee)
                        || self.needs_owned_scrutinee(scrutinee, arms, type_table))
                {
                    let temp_let = self.bind_scrutinee_to_temp(scrutinee, type_table, stmt.span);
                    out.push(temp_let);
                }
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

    /// Emit the `Let` statement for a `TirPattern::Binding` destructure
    /// target. The binding local's current type is authoritative —
    /// `boxing` may have promoted an address-taken local to `Box<T>`
    /// after the pattern's recorded `type_id` was set — and the value
    /// is coerced to it (match ergonomics: a `&T` / `&mut T` binding
    /// can capture a by-value `T`).
    fn emit_binding_let(
        &self,
        name: &str,
        local_index: u32,
        is_mut: bool,
        value: TirExpr,
        span: Span,
        type_table: &TypeTable,
        out: &mut Vec<TirStmt>,
    ) {
        let binding_type = type_table.get_local_type(local_index, &self.locals);
        let value = coerce_value_to_binding(value, binding_type, type_table, span);
        out.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.to_string(),
                local_index,
                is_mut,
                is_reactive: false,
                type_id: binding_type,
                value,
                skip_value_copy: false,
            },
            span,
        ));
    }

    /// An arm binding that takes ownership of a writable place would alias it.
    /// The temp's `Let` asks the fold for the copy.
    fn needs_owned_scrutinee(
        &self,
        scrutinee: &TirExpr,
        arms: &[TirMatchArm],
        type_table: &TypeTable,
    ) -> bool {
        if let TirExprKind::Local { index, .. } = &scrutinee.kind
            && self.owned_temps.contains(index)
        {
            return false;
        }
        self.place_is_writable(scrutinee, type_table)
            && arms
                .iter()
                .any(|arm| binds_by_value(&arm.pattern, type_table))
    }

    /// A projection through a reference is writable whatever it is rooted at:
    /// an immutable local still holds a `&mut`. What is not a place is a
    /// temporary this body alone holds; whether *that* aliases the caller's
    /// storage is decided where it escapes, not here.
    fn place_is_writable(&self, expr: &TirExpr, type_table: &TypeTable) -> bool {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self
                .locals
                .get(*index as usize)
                .is_none_or(|local| local.is_mut),
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref,
                expr: inner,
            } => {
                matches!(
                    type_table.get(inner.type_id),
                    ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                ) || self.place_is_writable(inner, type_table)
            }
            _ => false,
        }
    }

    /// Move `scrutinee` into a temp, leaving a read of it in place. The caller
    /// puts the returned `Let` wherever its position allows a statement.
    fn bind_scrutinee_to_temp(
        &mut self,
        scrutinee: &mut TirExpr,
        type_table: &TypeTable,
        span: Span,
    ) -> TirStmt {
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, scrutinee.span);
        let mut hoisted = std::mem::replace(scrutinee, placeholder);
        self.lower_expr(&mut hoisted, type_table);
        let type_id = hoisted.type_id;
        let scrutinee_span = hoisted.span;
        let temp = self.alloc_local(type_id);
        let name = format!("__match_{temp}");
        self.owned_temps.insert(temp);
        *scrutinee = TirExpr::new(
            TirExprKind::Local {
                index: temp,
                name: name.clone(),
            },
            type_id,
            scrutinee_span,
        );
        TirStmt::new(
            TirStmtKind::Let {
                name,
                local_index: temp,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: hoisted,
                skip_value_copy: false,
            },
            span,
        )
    }

    /// The temp a compound pattern's projections read. The fold decides its
    /// copy, and the per-binding `Let`s share its storage rather than ask
    /// again.
    fn emit_pattern_temp_let(
        &mut self,
        value: TirExpr,
        span: Span,
        out: &mut Vec<TirStmt>,
    ) -> (u32, String) {
        let local_index = self.alloc_local(value.type_id);
        let name = self.next_temp_name();
        let type_id = value.type_id;
        self.owned_temps.insert(local_index);
        out.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.clone(),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: false,
            },
            span,
        ));
        (local_index, name)
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

        let target = pattern_temp_type(pattern, value.type_id, type_table);
        let mut value = value;
        while value.type_id != target {
            let (ResolvedType::Ref(inner) | ResolvedType::MutRef(inner)) =
                *type_table.get(value.type_id)
            else {
                unreachable!("pattern_temp_type peels references only")
            };
            value = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: Box::new(value),
                },
                inner,
                span,
            );
        }
        let value = value;

        match pattern {
            TirPattern::Tuple(sub_patterns, _) => {
                let (tuple_temp_index, tuple_temp_name) =
                    self.emit_pattern_temp_let(value, span, out);

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
                name, local_index, ..
            } => {
                self.emit_binding_let(name, *local_index, is_mut, value, span, type_table, out);
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
                let (variant_temp_index, variant_temp_name) =
                    self.emit_pattern_temp_let(value, span, out);

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
                let (struct_temp_index, struct_temp_name) =
                    self.emit_pattern_temp_let(value, span, out);

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
                name, local_index, ..
            } => {
                self.emit_binding_let(name, *local_index, is_mut, value, span, type_table, out);
            }
            TirPattern::Tuple(sub_patterns, _) => {
                // Nested tuple - allocate temp and recurse
                let (tuple_temp_index, tuple_temp_name) =
                    self.emit_pattern_temp_let(value, span, out);

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
                    let (variant_temp_index, variant_temp_name) =
                        self.emit_pattern_temp_let(value, span, out);

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
                let (struct_temp_index, struct_temp_name) =
                    self.emit_pattern_temp_let(value, span, out);

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
        let span = expr.span;
        // A `Match` in expression position has no statement slot for the temp
        // its owning arm bindings need, so it grows a block to hold one.
        let mut scrutinee_temp: Option<TirStmt> = None;
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
                if self.needs_owned_scrutinee(scrutinee, arms, type_table) {
                    scrutinee_temp = Some(self.bind_scrutinee_to_temp(scrutinee, type_table, span));
                } else {
                    self.lower_expr(scrutinee, type_table);
                }
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

                // Match ergonomics: peel Ref / MutRef / Box off the scrutinee
                let scrut_span = scrutinee.span;
                let scrut_type = scrutinee.type_id;
                let old = std::mem::replace(
                    scrutinee.as_mut(),
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, scrut_span),
                );
                let (peeled, _) = peel_refs_and_box(old, scrut_type, type_table, scrut_span);
                *scrutinee.as_mut() = peeled;

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

                for arm in arms.iter_mut() {
                    if arm.guard.is_some() {
                        continue;
                    }
                    let TirPattern::ConstantValue { expr: const_expr } = &arm.pattern else {
                        continue;
                    };
                    let TirExprKind::GlobalVarGet {
                        module_source,
                        name,
                    } = &const_expr.kind
                    else {
                        continue;
                    };
                    let Some(&value) = self
                        .const_int_globals
                        .get(&(module_source.clone(), name.clone()))
                    else {
                        continue;
                    };
                    let unsigned = matches!(
                        type_table.get(scrutinee_type_id),
                        ResolvedType::Primitive(
                            PrimitiveType::U8
                                | PrimitiveType::U16
                                | PrimitiveType::U32
                                | PrimitiveType::U64
                        )
                    );
                    arm.pattern = if unsigned {
                        TirPattern::Literal(TirLiteralPattern::U128(value as u128))
                    } else {
                        TirPattern::Literal(TirLiteralPattern::I128(value))
                    };
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

                // Lifting `mut` variant-payload bindings into explicit
                // `Let mut original = fresh_local` statements is now
                // owned by `lower::plan::lift_mut::lift_mut_match_bindings`,
                // a stand-alone sub-pass that runs between `closure`
                // and `value_copy`. Keeping it out of this pre-pass is
                // the prerequisite for moving pattern lowering itself
                // into the translator (Phase 10 Step 2b) without making
                // `value_copy` pattern-aware.
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
            | TirExprKind::TupleLen { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                self.lower_expr(inner, type_table);
            }
            TirExprKind::VariadicTupleComprehension { .. } => {
                unreachable!(
                    "VariadicTupleComprehension should be expanded during monomorphization"
                )
            }
            TirExprKind::Call { args, .. } => {
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
                // A closure's `FunctionContext` starts at `next_local: 0`, so any
                // temp allocated while descending must live in that namespace —
                // otherwise closure lowering builds a `local_types` table where
                // the outer index collides with a real closure local. The scope
                // state is `params + body_locals`, discarded after the visit.
                let saved_count = self.local_count;
                let saved_locals = std::mem::take(&mut self.locals);
                self.local_count = (params.len() + body_locals.len()) as u32;
                self.locals = params
                    .iter()
                    .map(|(name, ty)| TirLocal {
                        name: name.clone(),
                        type_id: *ty,
                        is_mut: false,
                        span: crate::token::Span::default(),
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
        if let Some(temp_let) = scrutinee_temp {
            let matched =
                std::mem::replace(expr, TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span));
            let type_id = matched.type_id;
            *expr = TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![temp_let, TirStmt::new(TirStmtKind::Expr(matched), span)],
                    span,
                )),
                type_id,
                span,
            );
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
