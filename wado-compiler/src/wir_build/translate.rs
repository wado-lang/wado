//! Function body translation — converts TIR expressions and statements to WIR instructions.
//!
//! This is the core of the `tir_to_wir` phase, translating each TIR function body
//! into a sequence of WIR instructions.

use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind,
    TirFunction, TirLiteralPattern, TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::wir::{WirInstr, WirName, WirType, WirTypeDef, WirTypeId};
use indexmap::{IndexMap, IndexSet};

use super::context::WirContext;

/// Helper macro for unary f64 builtins.
macro_rules! unary_f64 {
    ($self:expr, $args:expr, $variant:path) => {{
        let o = $self.translate_expr(&$args[0]);
        Some($variant(Box::new(o)))
    }};
}

/// Helper macro for binary f64 builtins.
macro_rules! binary_f64 {
    ($self:expr, $args:expr, $variant:path) => {{
        let l = $self.translate_expr(&$args[0]);
        let r = $self.translate_expr(&$args[1]);
        Some($variant(Box::new(l), Box::new(r)))
    }};
}

/// Helper macro for unary f32 builtins.
macro_rules! unary_f32 {
    ($self:expr, $args:expr, $variant:path) => {{
        let o = $self.translate_expr(&$args[0]);
        Some($variant(Box::new(o)))
    }};
}

/// Helper macro for binary f32 builtins.
macro_rules! binary_f32 {
    ($self:expr, $args:expr, $variant:path) => {{
        let l = $self.translate_expr(&$args[0]);
        let r = $self.translate_expr(&$args[1]);
        Some($variant(Box::new(l), Box::new(r)))
    }};
}

/// Recursively collect variable names from Let statements.
fn collect_let_names(names: &mut IndexMap<u32, String>, stmts: &[TirStmt]) {
    for stmt in stmts {
        match &stmt.kind {
            TirStmtKind::Let {
                name, local_index, ..
            } => {
                names.insert(*local_index, name.clone());
            }
            TirStmtKind::Loop { body } => {
                collect_let_names(names, &body.stmts);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                collect_let_names(names, &then_block.stmts);
                if let Some(eb) = else_block {
                    collect_let_names(names, &eb.stmts);
                }
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                collect_let_names(names, &then_block.stmts);
                if let Some(eb) = else_block {
                    collect_let_names(names, &eb.stmts);
                }
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                collect_let_names(names, &block.stmts);
            }
            _ => {}
        }
    }
}

/// Register canonical closure wrapper functions for all closure functors.
/// Must be called before `translate_function_bodies` so wrappers are available
/// for `ClosureToCanonical` references.
pub fn register_closure_wrappers(ctx: &mut WirContext<'_>) {
    use crate::wir::{WirFunction, WirName, WirType};

    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
        for functor in &tir_mod.closure_functors {
            if ctx.closure_wrapper_funcs.contains_key(&functor.id) {
                continue;
            }

            // Get the __call method's param/result types (excluding self)
            let call_func = functor.call_method.borrow();
            let user_param_count = call_func.params.len() - 1; // skip self
            let user_params: Vec<WirType> = call_func
                .params
                .iter()
                .skip(1) // skip self parameter
                .map(|p| ctx.type_id_to_wir_type(type_table, p.type_id))
                .collect();
            let result_wirs: Vec<WirType> = if call_func.return_type == crate::tir::TypeTable::UNIT
                || call_func.return_type == crate::tir::TypeTable::NEVER
            {
                vec![]
            } else {
                vec![ctx.type_id_to_wir_type(type_table, call_func.return_type)]
            };
            let has_result = !result_wirs.is_empty();

            // Get canonical func type
            let (fn_type_id, _) =
                ctx.get_or_create_canonical_closure_type(user_params, result_wirs);

            // Get functor struct type ID
            let functor_wir_type = ctx.type_id_to_wir_type(type_table, functor.ref_type_id);
            let functor_struct_type_id = match &functor_wir_type {
                WirType::Ref { type_id, .. } => type_id.clone(),
                _ => continue,
            };

            // Look up the __call func_id
            let functor_name = &functor.struct_name;
            let call_method_suffix = format!("/{functor_name}::__call");
            let call_func_id = ctx
                .func_map
                .iter()
                .find(|(k, _)| k.ends_with(&call_method_suffix))
                .map(|(_, v)| v.clone());

            // Build wrapper function body
            let env_local = "__env".to_string();
            let typed_env_local = "__typed_env".to_string();

            let mut body = vec![
                WirInstr::DeclareLocal {
                    name: typed_env_local.clone(),
                    ty: WirType::Ref {
                        type_id: functor_struct_type_id.clone(),
                        nullable: false,
                    },
                },
                WirInstr::LocalSet {
                    name: typed_env_local.clone(),
                    value: Box::new(WirInstr::RefCast {
                        type_id: functor_struct_type_id,
                        nullable: false,
                        expr: Box::new(WirInstr::LocalGet {
                            name: env_local.clone(),
                        }),
                    }),
                },
            ];

            let mut call_args = vec![WirInstr::LocalGet {
                name: typed_env_local,
            }];
            for i in 0..user_param_count {
                call_args.push(WirInstr::LocalGet {
                    name: format!("__p{i}"),
                });
            }

            if let Some(call_fid) = call_func_id {
                let call_instr = WirInstr::Call {
                    func_id: call_fid,
                    args: call_args,
                };
                if has_result {
                    body.push(WirInstr::Return {
                        value: Some(Box::new(call_instr)),
                    });
                } else {
                    body.push(call_instr);
                }
            } else {
                body.push(WirInstr::Unreachable);
            }

            let mut param_names = vec![env_local];
            for i in 0..user_param_count {
                param_names.push(format!("__p{i}"));
            }

            let wrapper_name = format!("__closure_wrapper_{}", functor.id);
            let wrapper_fq = format!("closure//{wrapper_name}");

            let func = WirFunction {
                name: WirName {
                    display: wrapper_name,
                    fq: wrapper_fq,
                },
                type_id: fn_type_id,
                param_names,
                body: Some(body),
                meta: crate::wir::WirMeta::default(),
                generic_origin: None,
                effects: Vec::new(),
                comp_features: 0,
                export_name: None,
            };

            let func_id = ctx.register_function(func);
            ctx.closure_wrapper_funcs.insert(functor.id, func_id);
        }
    }
}

/// Translate all pending function bodies from TIR to WIR instructions.
pub fn translate_function_bodies(ctx: &mut WirContext<'_>) {
    let pending: Vec<_> = std::mem::take(&mut ctx.pending_bodies);

    for pending_body in &pending {
        let tir_func = pending_body.tir_func.borrow();
        let type_table = pending_body.type_table.borrow();

        if let Some(ref body) = tir_func.body {
            // Build local name map from params
            let mut local_names = IndexMap::new();
            for param in &tir_func.params {
                local_names.insert(param.local_index, param.name.clone());
            }
            // Pre-scan Let statements to collect variable names
            collect_let_names(&mut local_names, &body.stmts);

            // Translate inside a nested block so the translator (and its reborrow of ctx)
            // is dropped before we write back to ctx.functions below.
            let wir_body = {
                let mut translator = FunctionTranslator {
                    ctx: &mut *ctx,
                    type_table: &type_table,
                    tir_func: &tir_func,
                    label_stack: Vec::new(),
                    match_counter: 0,
                    local_counter: 0,
                    local_names,
                    immutable_locals: IndexSet::new(),
                };
                translator.translate_block(body)
            };
            drop(type_table);
            drop(tir_func);
            ctx.functions[pending_body.wir_func_index].body = Some(wir_body);
        }
    }
}

/// Tracks a Wasm block scope in the label stack for computing br depths.
struct LabelEntry {
    /// Label name from TIR (for labeled blocks).
    label: Option<String>,
    /// True if this is the outer block wrapping a loop (target for unlabeled break).
    is_loop_break: bool,
    /// True if this is a loop instruction (target for continue).
    is_loop_continue: bool,
}

/// Translator state for a single function.
struct FunctionTranslator<'a, 'b> {
    ctx: &'a mut WirContext<'b>,
    type_table: &'a TypeTable,
    tir_func: &'a TirFunction,
    /// Stack of Wasm block scopes for computing br depths.
    label_stack: Vec<LabelEntry>,
    /// Counter for generating unique match scrutinee local names.
    match_counter: u32,
    /// Counter for generating unique temporary local names.
    local_counter: u32,
    /// Map from local index to variable name (built from params + Let stmts).
    local_names: IndexMap<u32, String>,
    /// Set of local indices declared as immutable (`let`, not `let mut`).
    /// Used to skip unnecessary value copies when an immutable binding
    /// is initialized from another immutable local.
    immutable_locals: IndexSet<u32>,
}

impl FunctionTranslator<'_, '_> {
    /// Get the WIR local name for a given local index.
    /// Uses the TIR variable name if available, otherwise falls back to `__local_N`.
    ///
    /// Parameters keep their original names (matching `WirFunction::param_names`).
    /// Non-parameter locals that shadow a parameter name get an `_{index}` suffix.
    fn local_name(&self, index: u32) -> String {
        if let Some(name) = self.local_names.get(&index) {
            let count = self.local_names.values().filter(|n| *n == name).count();
            if count > 1 {
                // Check if this index belongs to a parameter — params keep
                // their original names so they match `WirFunction::param_names`.
                let is_param = self.tir_func.params.iter().any(|p| p.local_index == index);
                if is_param {
                    name.clone()
                } else {
                    format!("{name}_{index}")
                }
            } else {
                name.clone()
            }
        } else {
            format!("__local_{index}")
        }
    }

    /// Build a `StructNew` instruction, wrapping each field value with `RefAsNonNull`
    /// where the struct definition declares a non-nullable reference field.
    fn struct_new(&self, type_id: WirTypeId, fields: Vec<WirInstr>) -> WirInstr {
        let fields = self.cast_nonnull_fields(&type_id, fields);
        WirInstr::StructNew { type_id, fields }
    }

    /// Build a `StructSet` instruction, wrapping the value with `RefAsNonNull`
    /// if the target field is a non-nullable reference.
    fn struct_set(
        &self,
        type_id: WirTypeId,
        field_name: String,
        expr: WirInstr,
        value: WirInstr,
    ) -> WirInstr {
        let value = if self.is_field_nonnull_ref(&type_id, &field_name) {
            WirInstr::RefAsNonNull(Box::new(value))
        } else {
            value
        };
        WirInstr::StructSet {
            type_id,
            field_name,
            expr: Box::new(expr),
            value: Box::new(value),
        }
    }

    /// Wrap each field value with `RefAsNonNull` where the struct definition
    /// declares a non-nullable reference field.
    fn cast_nonnull_fields(&self, type_id: &WirTypeId, fields: Vec<WirInstr>) -> Vec<WirInstr> {
        let idx = type_id.index() as usize;
        if idx < self.ctx.types.len()
            && let WirTypeDef::Struct(st) = &self.ctx.types[idx]
        {
            fields
                .into_iter()
                .enumerate()
                .map(|(i, instr)| {
                    if st.fields.get(i).is_some_and(|f| f.ty.is_nonnull_ref()) {
                        WirInstr::RefAsNonNull(Box::new(instr))
                    } else {
                        instr
                    }
                })
                .collect()
        } else {
            fields
        }
    }

    /// Check if a named field of a struct type is a non-nullable reference.
    fn is_field_nonnull_ref(&self, type_id: &WirTypeId, field_name: &str) -> bool {
        let idx = type_id.index() as usize;
        if idx < self.ctx.types.len()
            && let WirTypeDef::Struct(st) = &self.ctx.types[idx]
        {
            st.fields
                .iter()
                .any(|f| f.name == field_name && f.ty.is_nonnull_ref())
        } else {
            false
        }
    }

    /// Check if a type requires value copy (struct, array, tuple, variant, option).
    fn needs_value_copy(&self, type_id: TypeId) -> bool {
        match self.type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                // Internal Box<T> types are GC reference cells for primitive boxing.
                // They should share the heap object on assignment, not deep-copy.
                let is_box = name.starts_with("Box<") && module_source.is_core_internal();
                !is_box
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => {
                // Internal Box<T> types are GC reference cells for primitive boxing.
                // They should share the heap object on assignment, not deep-copy.
                !(name == "Box" && module_source.is_core_internal())
            }
            ResolvedType::Variant { .. } => true,
            ResolvedType::Tuple(elements) => !elements.is_empty(),
            _ => false,
        }
    }

    /// Check if an expression is "fresh" (doesn't need value copy).
    /// Fresh values are newly created and don't alias existing data,
    /// so they can be used directly without copying.
    fn is_fresh_value(expr: &TirExpr) -> bool {
        Self::is_fresh_in_context(expr, &IndexSet::new())
    }

    /// Check if an expression is fresh, considering locals known to hold fresh values.
    ///
    /// `fresh_locals` tracks local variable indices that were assigned fresh values
    /// within the enclosing block scope. This enables copy elision for patterns like:
    /// ```text
    /// __seq_lit: {
    ///     let mut __b = Array { ... };   // __b is fresh
    ///     break __seq_lit: *__b;          // *__b is fresh (deref of fresh local)
    /// }
    /// ```
    fn is_fresh_in_context(expr: &TirExpr, fresh_locals: &IndexSet<u32>) -> bool {
        match &expr.kind {
            // Literals always produce fresh values
            TirExprKind::StringLiteral(_)
            | TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::Null => true,

            // All call variants return fresh values (callee constructs the return value)
            TirExprKind::Call { .. }
            | TirExprKind::StaticCall { .. }
            | TirExprKind::MethodCall { .. }
            | TirExprKind::CmRawCall { .. }
            | TirExprKind::IndirectCall { .. } => true,

            // ClosureToCanonical creates a fresh closure struct
            TirExprKind::ClosureToCanonical { .. } => true,

            // Variant/enum constructors produce fresh values
            TirExprKind::VariantConstruct { .. } | TirExprKind::EnumConstruct { .. } => true,

            // Local variable reference: fresh if the local is known to hold a fresh value
            TirExprKind::Local { index, .. } => fresh_locals.contains(index),

            // Deref of a fresh value is still fresh (e.g., *self where self is fresh)
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => Self::is_fresh_in_context(inner, fresh_locals),

            // Labeled blocks: fresh if every break value targeting this label is fresh,
            // tracking which locals hold fresh values within the block
            TirExprKind::LabeledBlock { label, block, .. } => {
                Self::block_breaks_are_fresh(label, block, fresh_locals)
            }

            // Everything else is not fresh (field access, index, etc.)
            _ => false,
        }
    }

    /// Check if every `break label(value)` in a block has a fresh value.
    ///
    /// Tracks locals assigned fresh values within the block, enabling copy
    /// elision for inlined builder patterns where the break value references
    /// a locally-created object.
    fn block_breaks_are_fresh(label: &str, block: &TirBlock, parent_fresh: &IndexSet<u32>) -> bool {
        let mut found = false;
        let mut fresh_locals = parent_fresh.clone();
        if Self::scan_block_for_breaks(label, block, &mut found, &mut fresh_locals) {
            found
        } else {
            false
        }
    }

    /// Recursively scan a block for `Break` targeting `label`.
    /// Tracks fresh locals and returns `false` if any break value is not fresh.
    fn scan_block_for_breaks(
        label: &str,
        block: &TirBlock,
        found: &mut bool,
        fresh_locals: &mut IndexSet<u32>,
    ) -> bool {
        for stmt in &block.stmts {
            if !Self::scan_stmt_for_breaks(label, stmt, found, fresh_locals) {
                return false;
            }
        }
        true
    }

    fn scan_stmt_for_breaks(
        label: &str,
        stmt: &TirStmt,
        found: &mut bool,
        fresh_locals: &mut IndexSet<u32>,
    ) -> bool {
        match &stmt.kind {
            // Track locals assigned fresh values
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                if Self::is_fresh_in_context(value, fresh_locals) {
                    fresh_locals.insert(*local_index);
                }
                true
            }
            TirStmtKind::Break {
                label: Some(l),
                value: Some(v),
            } if l == label => {
                *found = true;
                Self::is_fresh_in_context(v, fresh_locals)
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                if !Self::scan_block_for_breaks(label, then_block, found, fresh_locals) {
                    return false;
                }
                if let Some(eb) = else_block
                    && !Self::scan_block_for_breaks(label, eb, found, fresh_locals)
                {
                    return false;
                }
                true
            }
            TirStmtKind::Loop { body } => {
                Self::scan_block_for_breaks(label, body, found, fresh_locals)
            }
            TirStmtKind::Expr(expr) => Self::scan_expr_for_breaks(label, expr, found, fresh_locals),
            _ => true,
        }
    }

    fn scan_expr_for_breaks(
        label: &str,
        expr: &TirExpr,
        found: &mut bool,
        fresh_locals: &mut IndexSet<u32>,
    ) -> bool {
        match &expr.kind {
            TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
                Self::scan_block_for_breaks(label, block, found, fresh_locals)
            }
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                if !Self::scan_block_for_breaks(label, then_branch, found, fresh_locals) {
                    return false;
                }
                if let Some(eb) = else_branch
                    && !Self::scan_block_for_breaks(label, eb, found, fresh_locals)
                {
                    return false;
                }
                true
            }
            _ => true,
        }
    }

    /// Wrap a translated value instruction in `ValueCopy` if needed.
    fn maybe_value_copy(&self, value: &TirExpr, translated: WirInstr) -> WirInstr {
        if self.needs_value_copy(value.type_id) && !Self::is_fresh_value(value) {
            self.build_value_copy(value.type_id, translated)
        } else {
            translated
        }
    }

    /// Copy an argument only if the corresponding callee parameter is `mut`.
    ///
    /// When a parameter is not `mut`, the callee cannot reassign it, write to its
    /// fields, or call `&mut self` methods on it — so the caller's value is safe
    /// without a defensive copy.
    fn maybe_value_copy_if_mut(
        &self,
        value: &TirExpr,
        translated: WirInstr,
        is_mut: bool,
    ) -> WirInstr {
        if is_mut {
            self.maybe_value_copy(value, translated)
        } else {
            translated
        }
    }

    /// Check if a source expression's root local is immutable.
    /// Returns true when the expression is a Local or `FieldAccess` chain
    /// whose root local is in the immutable set. In that case, sharing
    /// the value without deep-copying is safe because neither the
    /// destination (non-mut let) nor the source can be mutated.
    fn is_source_immutable(&self, expr: &TirExpr) -> bool {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.immutable_locals.contains(index),
            TirExprKind::FieldAccess { expr: inner, .. } => self.is_source_immutable(inner),
            _ => false,
        }
    }

    /// Build a `ValueCopy` instruction for the given type.
    /// Uses the WIR type ID to identify the struct, and builds a shallow copy descriptor.
    /// Only `Option<T>` values can be null; plain struct/variant types are always non-null.
    fn build_value_copy(&self, type_id: TypeId, expr: WirInstr) -> WirInstr {
        use crate::wir::{WirCopyField, WirCopyType};
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, type_id);
        if let WirType::Ref {
            type_id: wir_tid, ..
        } = wir_type
        {
            // With SubtypeHierarchy, Option values are non-null refs (None is a valid struct)
            // TODO: Future optimization: NullableRef Option would need nullable=true here
            let nullable = false;

            // Variants use pass-through copy (immutable structs in the rec group)
            if self.ctx.is_variant_type(&wir_tid) {
                return WirInstr::ValueCopy {
                    type_id: wir_tid,
                    source_type: WirCopyType::Variant { cases: Vec::new() },
                    expr: Box::new(expr),
                    nullable,
                };
            }
            // Look up the WIR struct type to get field count
            let field_count = self.ctx.get_struct_field_count(&wir_tid);
            let copy_fields: Vec<WirCopyField> = (0..field_count)
                .map(|i| WirCopyField {
                    index: i,
                    needs_copy: false, // Shallow copy (field-by-field)
                    copy_type: None,
                })
                .collect();
            WirInstr::ValueCopy {
                type_id: wir_tid,
                source_type: WirCopyType::Struct {
                    fields: copy_fields,
                },
                expr: Box::new(expr),
                nullable,
            }
        } else {
            expr
        }
    }

    /// Build the qualified global name.
    fn make_global_name(&self, module_source: &crate::name::ModuleSource, name: &str) -> String {
        if module_source.is_entry_point() {
            format!("global:{name}")
        } else {
            let module_path = module_source.to_path();
            format!("global:{}::{name}", module_path.join("::"))
        }
    }

    /// Translate the top-level function body: declares locals and translates statements.
    fn translate_block(&mut self, block: &TirBlock) -> Vec<WirInstr> {
        let mut instrs = Vec::new();

        // Declare local variables.
        // `local_types` may only contain body locals (not params) or it may be empty
        // for functions that haven't been through the lower phase's local allocation.
        // Fall back to scanning Let statements to discover locals.
        let param_count = self.tir_func.params.len();
        if self.tir_func.local_types.is_empty() {
            // Scan block for Let declarations to discover local types
            self.declare_locals_from_stmts(&mut instrs, &block.stmts);
        } else {
            for (i, &local_type_id) in self.tir_func.local_types.iter().enumerate() {
                // Skip entries that correspond to params (they're already declared)
                if i < param_count {
                    continue;
                }
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, local_type_id);
                // Skip unit-type locals (unit has no Wasm representation)
                if matches!(wir_type, WirType::Unit) {
                    continue;
                }
                let idx = u32::try_from(i).unwrap();
                let local_name = self.local_name(idx);
                instrs.push(WirInstr::DeclareLocal {
                    name: local_name,
                    ty: wir_type,
                });
            }
        }

        // Translate statements
        instrs.extend(self.translate_stmts(&block.stmts));

        instrs
    }

    /// Scan statements recursively to discover Let declarations and emit `DeclareLocal`.
    /// Used when `local_types` is empty (for functions from library modules).
    fn declare_locals_from_stmts(&self, instrs: &mut Vec<WirInstr>, stmts: &[TirStmt]) {
        for stmt in stmts {
            match &stmt.kind {
                TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } => {
                    // Skip params (they are already declared via param_names)
                    let param_count = u32::try_from(self.tir_func.params.len()).unwrap();
                    if *local_index >= param_count {
                        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, *type_id);
                        // Skip unit-type locals (unit has no Wasm representation)
                        if !matches!(wir_type, WirType::Unit) {
                            let local_name = self.local_name(*local_index);
                            instrs.push(WirInstr::DeclareLocal {
                                name: local_name,
                                ty: wir_type,
                            });
                        }
                    }
                }
                TirStmtKind::Loop { body } => {
                    self.declare_locals_from_stmts(instrs, &body.stmts);
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    self.declare_locals_from_stmts(instrs, &then_block.stmts);
                    if let Some(eb) = else_block {
                        self.declare_locals_from_stmts(instrs, &eb.stmts);
                    }
                }
                TirStmtKind::IfPattern {
                    then_block,
                    else_block,
                    ..
                } => {
                    self.declare_locals_from_stmts(instrs, &then_block.stmts);
                    if let Some(eb) = else_block {
                        self.declare_locals_from_stmts(instrs, &eb.stmts);
                    }
                }
                TirStmtKind::LabeledBlock { block, .. } => {
                    self.declare_locals_from_stmts(instrs, &block.stmts);
                }
                _ => {}
            }
        }
    }

    /// Translate a list of TIR statements to WIR instructions (no local declarations).
    fn translate_stmts(&mut self, stmts: &[TirStmt]) -> Vec<WirInstr> {
        let mut instrs = Vec::new();
        for stmt in stmts {
            if let Some(instr) = self.translate_stmt(stmt) {
                instrs.push(instr);
            }
        }
        instrs
    }

    /// Translate statements where the last expression produces the block's value.
    ///
    /// Used for if-expression branches and labeled-block-expression bodies.
    /// The last `Expr` statement is NOT dropped; it stays on the Wasm stack as the result.
    /// Also handles statement-level If/IfPattern as value-producing when they're the
    /// last statement (TIR stores these as statements, not expressions).
    fn translate_stmts_as_value(&mut self, stmts: &[TirStmt]) -> Vec<WirInstr> {
        let mut instrs = Vec::new();
        let len = stmts.len();
        for (i, stmt) in stmts.iter().enumerate() {
            let is_last = i + 1 == len;
            if is_last {
                // Last statement: if it's an Expr, translate without drop
                if let TirStmtKind::Expr(expr) = &stmt.kind {
                    let instr = self.translate_expr_as_value(expr);
                    instrs.push(instr);
                    // Note: `translate_expr` already appends `unreachable` for
                    // `never`-typed expressions, so no extra push is needed here.
                    continue;
                }
                // Statement-level If with else can produce a value
                if let TirStmtKind::If {
                    condition,
                    then_block,
                    else_block: Some(else_block),
                    ..
                } = &stmt.kind
                    && let Some(result_type) = self.infer_stmts_result_type(&then_block.stmts)
                {
                    let cond = self.translate_expr(condition);
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                    let then_body = self.translate_stmts_as_value(&then_block.stmts);
                    let else_body = Some(self.translate_stmts_as_value(&else_block.stmts));
                    self.label_stack.pop();
                    instrs.push(WirInstr::If {
                        condition: Box::new(cond),
                        result: Some(result_type),
                        then_body,
                        else_body,
                    });
                    continue;
                }
                // Statement-level IfPattern with else can produce a value
                if let TirStmtKind::IfPattern {
                    scrutinee,
                    then_block,
                    else_block: Some(else_block),
                    ..
                } = &stmt.kind
                    && let Some(result_type) = self.infer_stmts_result_type(&then_block.stmts)
                {
                    let scrut = self.translate_expr(scrutinee);
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                    let then_body = self.translate_stmts_as_value(&then_block.stmts);
                    let else_body = Some(self.translate_stmts_as_value(&else_block.stmts));
                    self.label_stack.pop();
                    instrs.push(WirInstr::If {
                        condition: Box::new(scrut),
                        result: Some(result_type),
                        then_body,
                        else_body,
                    });
                    continue;
                }
            }
            if let Some(instr) = self.translate_stmt(stmt) {
                instrs.push(instr);
            }
        }
        instrs
    }

    /// Infer the WIR result type from the last statement in a list.
    /// Returns `Some(type)` if the last statement can produce a value, `None` otherwise.
    fn infer_stmts_result_type(&self, stmts: &[TirStmt]) -> Option<WirType> {
        stmts.last().and_then(|stmt| match &stmt.kind {
            TirStmtKind::Expr(expr) => {
                if expr.type_id != TypeTable::UNIT && expr.type_id != TypeTable::NEVER {
                    Some(self.ctx.type_id_to_wir_type(self.type_table, expr.type_id))
                } else {
                    None
                }
            }
            TirStmtKind::If {
                then_block,
                else_block: Some(_),
                ..
            } => self.infer_stmts_result_type(&then_block.stmts),
            TirStmtKind::IfPattern {
                then_block,
                else_block: Some(_),
                ..
            } => self.infer_stmts_result_type(&then_block.stmts),
            _ => None,
        })
    }

    /// Translate an expression in "value position" — the result stays on the Wasm stack.
    ///
    /// Handles cases where TIR assigns UNIT type to expressions that actually produce
    /// values in a given context (e.g., nested if expressions, chained assignments).
    fn translate_expr_as_value(&mut self, expr: &TirExpr) -> WirInstr {
        // If the expression already has a non-UNIT type, translate normally
        if expr.type_id != TypeTable::UNIT {
            return self.translate_expr(expr);
        }

        match &expr.kind {
            // If expression with UNIT type but value-producing branches
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                if let Some(result_type) = self.infer_stmts_result_type(&then_branch.stmts) {
                    let cond = self.translate_expr(condition);
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                    let then_body = self.translate_stmts_as_value(&then_branch.stmts);
                    let else_body = else_branch
                        .as_ref()
                        .map(|b| self.translate_stmts_as_value(&b.stmts));
                    self.label_stack.pop();
                    return WirInstr::If {
                        condition: Box::new(cond),
                        result: Some(result_type),
                        then_body,
                        else_body,
                    };
                }
                self.translate_expr(expr)
            }
            // Block with UNIT type but value-producing last expression
            TirExprKind::Block(block) => {
                if self.infer_stmts_result_type(&block.stmts).is_some() {
                    let body = self.translate_stmts_as_value(&block.stmts);
                    WirInstr::Seq(body)
                } else {
                    self.translate_expr(expr)
                }
            }
            _ => self.translate_expr(expr),
        }
    }

    /// Translate a TIR statement to a WIR instruction.
    fn translate_stmt(&mut self, stmt: &TirStmt) -> Option<WirInstr> {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                value,
                is_mut,
                skip_value_copy,
                ..
            } => {
                // Track immutable locals for value-copy elision on subsequent bindings.
                if !is_mut {
                    self.immutable_locals.insert(*local_index);
                }
                let value_instr = self.translate_expr(value);
                // If the initializer diverges (`never`), no value reaches the stack,
                // so LocalSet would be invalid. `translate_expr` already appends
                // `unreachable` for `never`-typed expressions, so just emit the
                // diverging instruction; the local is declared but never assigned.
                if value.type_id == TypeTable::NEVER {
                    Some(value_instr)
                } else if value.type_id == TypeTable::UNIT {
                    // Unit-type locals have no Wasm representation; just emit
                    // the init expression for its side effects (usually Nop).
                    Some(value_instr)
                } else {
                    let local_name = self.local_name(*local_index);
                    // Skip deep value-copy when safe:
                    // 1. LICM-hoisted variables (skip_value_copy flag set by optimizer)
                    // 2. Immutable binding from an immutable source (no mutation possible)
                    let can_skip_copy =
                        *skip_value_copy || (!is_mut && self.is_source_immutable(value));
                    let value_instr = if can_skip_copy {
                        value_instr
                    } else {
                        self.maybe_value_copy(value, value_instr)
                    };
                    Some(WirInstr::LocalSet {
                        name: local_name,
                        value: Box::new(value_instr),
                    })
                }
            }
            TirStmtKind::Expr(expr) => {
                let instr = self.translate_expr(expr);
                // If the expression has a non-unit type, drop it.
                // Exception: assignments and global-var-sets produce void WIR instructions
                // (LocalSet/StructSet/ArraySet/GlobalSet), so don't wrap them in Drop.
                let is_void_instr = matches!(
                    &expr.kind,
                    TirExprKind::Assign { .. } | TirExprKind::GlobalVarSet { .. }
                );
                if !is_void_instr
                    && expr.type_id != TypeTable::UNIT
                    && expr.type_id != TypeTable::NEVER
                {
                    Some(WirInstr::Drop(Box::new(instr)))
                } else {
                    Some(instr)
                }
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    let value_instr = self.translate_expr(expr);
                    Some(WirInstr::Return {
                        value: Some(Box::new(value_instr)),
                    })
                } else {
                    Some(WirInstr::Return { value: None })
                }
            }
            TirStmtKind::Loop { body } => {
                // Generate: block { loop { <body>; br 0; } }
                // The outer block is for break, the inner loop is for continue.
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: true,
                    is_loop_continue: false,
                });
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: true,
                });
                let mut body_instrs = self.translate_stmts(&body.stmts);
                // Unconditional back-edge: br 0 to loop header
                body_instrs.push(WirInstr::Br { depth: 0 });
                self.label_stack.pop(); // pop loop
                self.label_stack.pop(); // pop outer block
                Some(WirInstr::Block {
                    label: None,
                    result: None,
                    body: vec![WirInstr::Loop {
                        label: None,
                        body: body_instrs,
                    }],
                })
            }
            TirStmtKind::Break { label, value } => {
                let depth = self.compute_break_depth(label.as_deref());
                if let Some(val) = value {
                    let val_instr = self.translate_expr(val);
                    Some(WirInstr::Seq(vec![val_instr, WirInstr::Br { depth }]))
                } else {
                    Some(WirInstr::Br { depth })
                }
            }
            TirStmtKind::Continue => {
                let depth = self.compute_continue_depth();
                Some(WirInstr::Br { depth })
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                let cond = self.translate_expr(condition);
                // Push a label entry for the if block scope
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let then_body = self.translate_stmts(&then_block.stmts);
                let else_body = else_block.as_ref().map(|b| self.translate_stmts(&b.stmts));
                self.label_stack.pop();
                Some(WirInstr::If {
                    condition: Box::new(cond),
                    result: None,
                    then_body,
                    else_body,
                })
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                // Translate pattern matching as a simple if on the scrutinee
                let scrut = self.translate_expr(scrutinee);
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let then_body = self.translate_stmts(&then_block.stmts);
                let else_body = else_block.as_ref().map(|b| self.translate_stmts(&b.stmts));
                self.label_stack.pop();
                // For now, generate a placeholder - actual pattern matching needs more work
                Some(WirInstr::If {
                    condition: Box::new(scrut),
                    result: None,
                    then_body,
                    else_body,
                })
            }
            TirStmtKind::LabeledBlock { label, block } => {
                self.label_stack.push(LabelEntry {
                    label: Some(label.clone()),
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let body_instrs = self.translate_stmts(&block.stmts);
                self.label_stack.pop();
                Some(WirInstr::Block {
                    label: Some(label.clone()),
                    result: None,
                    body: body_instrs,
                })
            }
            TirStmtKind::LetPattern { pattern, value, .. } => {
                self.translate_let_pattern(pattern, value)
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    /// Translate a TIR expression to a WIR instruction.
    ///
    /// When the expression has type `never` (bottom type), the returned instruction
    /// diverges.  The `Seq([instr, Unreachable])` wrapper tells the Wasm validator
    /// that any subsequent type expectations in the same block are vacuously satisfied,
    /// so `never`-typed sub-expressions can appear in any value position (binary
    /// operands, struct fields, array elements, function arguments, …).
    fn translate_expr(&mut self, expr: &TirExpr) -> WirInstr {
        let instr = self.translate_expr_inner(expr);
        if expr.type_id == TypeTable::NEVER {
            WirInstr::Seq(vec![instr, WirInstr::Unreachable])
        } else {
            instr
        }
    }

    fn translate_expr_inner(&mut self, expr: &TirExpr) -> WirInstr {
        match &expr.kind {
            // === Literals ===
            TirExprKind::IntLiteral { value, .. } => {
                // Resolve newtypes to their base primitive type
                let base_type_id = self.type_table.get_ultimate_base_type(expr.type_id);
                match self.type_table.get(base_type_id) {
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
                        WirInstr::I64Const(*value as i64)
                    }
                    _ => WirInstr::I32Const(*value as i32),
                }
            }
            TirExprKind::FloatLiteral { value, .. } => {
                // Resolve newtypes to their base primitive type
                let base_type_id = self.type_table.get_ultimate_base_type(expr.type_id);
                match self.type_table.get(base_type_id) {
                    ResolvedType::Primitive(PrimitiveType::F32) => {
                        WirInstr::F32Const(*value as f32)
                    }
                    _ => WirInstr::F64Const(*value),
                }
            }
            TirExprKind::BoolLiteral(value) => WirInstr::I32Const(i32::from(*value)),
            TirExprKind::CharLiteral(c) => WirInstr::I32Const(*c as i32),
            TirExprKind::StringLiteral(s) => {
                // String literals are constructed from data segments
                self.translate_string_literal(s)
            }
            TirExprKind::BytesLiteral(b) => {
                // Bytes literals are constructed as Array<u8> from data segments
                self.translate_bytes_literal(b)
            }
            TirExprKind::Null => {
                // For Option types, construct a None variant struct (SubtypeHierarchy)
                // TODO: Future NullableRef optimization would use RefNull for Option too
                if let Some(inner) = self.type_table.as_option(expr.type_id) {
                    // If the inner type is unresolved (UNKNOWN), use unreachable
                    // since we can't construct a concrete variant struct without
                    // knowing the type. This only happens in error recovery paths.
                    if matches!(self.type_table.get(inner), ResolvedType::Unknown) {
                        WirInstr::Unreachable
                    } else {
                        self.translate_variant_construct(
                            expr.type_id, // variant_type
                            1,            // case_index: None is case 1
                            "None",
                            None, // no payload
                            expr.type_id,
                        )
                    }
                } else {
                    // Non-Option null: emit ref.null as a placeholder value.
                    // Used by CM adapters for local initialization before conditional assignment.
                    WirInstr::RefNull {
                        heap_type: crate::wir::WirAbstractHeapType::None,
                    }
                }
            }
            TirExprKind::Unit => {
                // Unit has no value; use nop
                WirInstr::Nop
            }

            // === Variables ===
            TirExprKind::Local { index, .. } => {
                // Unit-type locals have no Wasm representation
                if expr.type_id == TypeTable::UNIT {
                    WirInstr::Nop
                } else {
                    WirInstr::LocalGet {
                        name: self.local_name(*index),
                    }
                }
            }
            TirExprKind::Global {
                module_source,
                name,
            } => {
                // Global function references — currently emitting as i32 const placeholder
                // (TirExprKind::Global is for function references, not global variables)
                let full_name = if module_source.is_entry_point() {
                    name.clone()
                } else {
                    format!("{module_source}::{name}")
                };
                if let Some(func_id) = self.ctx.func_map.get(&full_name) {
                    WirInstr::I32Const(func_id.index() as i32)
                } else {
                    WirInstr::I32Const(0)
                }
            }
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                let global_name = self.make_global_name(module_source, name);
                WirInstr::GlobalGet {
                    name: WirName {
                        display: name.clone(),
                        fq: global_name,
                    },
                }
            }
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => {
                let global_name = self.make_global_name(module_source, name);
                let val = self.translate_expr(value);
                WirInstr::GlobalSet {
                    name: WirName {
                        display: name.clone(),
                        fq: global_name,
                    },
                    value: Box::new(val),
                }
            }

            // === Binary Operations ===
            TirExprKind::Binary { op, left, right } => {
                // Short-circuit logical operators: defer right-side evaluation
                if matches!(op, TirBinaryOp::And) {
                    let l = self.translate_expr(left);
                    let r = self.translate_expr(right);
                    // if left { right } else { 0 }
                    return WirInstr::If {
                        condition: Box::new(l),
                        result: Some(WirType::I32),
                        then_body: vec![r],
                        else_body: Some(vec![WirInstr::I32Const(0)]),
                    };
                }
                if matches!(op, TirBinaryOp::Or) {
                    let l = self.translate_expr(left);
                    let r = self.translate_expr(right);
                    // if left { 1 } else { right }
                    return WirInstr::If {
                        condition: Box::new(l),
                        result: Some(WirType::I32),
                        then_body: vec![WirInstr::I32Const(1)],
                        else_body: Some(vec![r]),
                    };
                }
                let l = Box::new(self.translate_expr(left));
                let r = Box::new(self.translate_expr(right));
                self.translate_binary_op(op, l, r, left.type_id)
            }

            // === Unary Operations ===
            TirExprKind::Unary { op, expr: inner } => match op {
                TirUnaryOp::Ref | TirUnaryOp::MutRef => self.translate_expr(inner),
                TirUnaryOp::Deref => self.translate_expr(inner),
                _ => {
                    let o = Box::new(self.translate_expr(inner));
                    self.translate_unary_op(op, o, inner.type_id)
                }
            },

            // === Function Calls ===
            TirExprKind::Call {
                func,
                args,
                param_is_mut,
                ..
            } => {
                // Check for instruction-builtins first
                let builtin = func
                    .builtin_name()
                    .or_else(|| func.monomorphized_builtin_name());
                if let Some(ref builtin_name) = builtin
                    && let Some(instr) =
                        self.translate_builtin_call(builtin_name, args, expr.type_id)
                {
                    return instr;
                }

                let translated_args: Vec<WirInstr> = args
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.type_id != TypeTable::UNIT)
                    .map(|(i, a)| {
                        let translated = self.translate_expr(a);
                        let is_mut = param_is_mut.get(i).copied().unwrap_or(true);
                        self.maybe_value_copy_if_mut(a, translated, is_mut)
                    })
                    .collect();

                if let Some(func_id) = self.resolve_function_ref(func) {
                    WirInstr::Call {
                        func_id,
                        args: translated_args,
                    }
                } else {
                    eprintln!(
                        "[WIR] unresolved Call: name={:?} builtin={:?}",
                        func.name.clone(),
                        builtin
                    );
                    WirInstr::Unreachable
                }
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                param_is_mut,
                ..
            } => {
                // Canonical resource method dispatch: uses #[canonical("...")] from types.wado
                if let Some(instr) =
                    self.try_translate_canonical_method(receiver, func, args, expr.type_id)
                {
                    return instr;
                }

                let mut translated_args: Vec<WirInstr> = Vec::new();
                // Receiver is always included (self/&self/&mut self is never unit).
                // Receivers are always reference types — do not copy them.
                translated_args.push(self.translate_expr(receiver));
                // params[0] is self; args[i] corresponds to params[i+1]
                for (i, arg) in args.iter().enumerate() {
                    if arg.type_id != TypeTable::UNIT {
                        let translated = self.translate_expr(arg);
                        let is_mut = param_is_mut.get(i).copied().unwrap_or(true);
                        translated_args.push(self.maybe_value_copy_if_mut(arg, translated, is_mut));
                    }
                }

                if let Some(func_id) = self.resolve_function_ref(func) {
                    WirInstr::Call {
                        func_id,
                        args: translated_args,
                    }
                } else {
                    if let Some(mi) = func.method_info.clone() {
                        eprintln!(
                            "[WIR] unresolved MethodCall: name={:?} method_info={:?}",
                            func.name.clone(),
                            mi
                        );
                    } else {
                        eprintln!("[WIR] unresolved MethodCall: name={:?}", func.name.clone());
                    }
                    WirInstr::Unreachable
                }
            }
            TirExprKind::StaticCall {
                func,
                args,
                param_is_mut,
                ..
            } => {
                // Canonical static resource method dispatch (e.g., Stream::new, WaitableSet::new)
                if let Some(canonical) = func.method_info.clone().and_then(|m| m.canonical_name)
                    && let Some(instr) =
                        self.try_translate_canonical_static_method(&canonical, args, expr.type_id)
                {
                    return instr;
                }

                let translated_args: Vec<WirInstr> = args
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.type_id != TypeTable::UNIT)
                    .map(|(i, a)| {
                        let translated = self.translate_expr(a);
                        let is_mut = param_is_mut.get(i).copied().unwrap_or(true);
                        self.maybe_value_copy_if_mut(a, translated, is_mut)
                    })
                    .collect();

                if let Some(func_id) = self.resolve_function_ref(func) {
                    WirInstr::Call {
                        func_id,
                        args: translated_args,
                    }
                } else {
                    eprintln!("[WIR] unresolved StaticCall: name={:?}", func.name.clone());
                    WirInstr::Unreachable
                }
            }

            // === Struct Literal ===
            TirExprKind::StructLiteral { fields, .. } => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, expr.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    // Unit-typed fields have no Wasm representation; skip them.
                    let non_unit_fields: Vec<_> = fields
                        .iter()
                        .filter(|f| {
                            !matches!(
                                self.ctx
                                    .type_id_to_wir_type(self.type_table, f.value.type_id),
                                WirType::Unit
                            )
                        })
                        .collect();
                    let field_instrs: Vec<WirInstr> = non_unit_fields
                        .iter()
                        .map(|f| self.translate_expr(&f.value))
                        .collect();
                    self.struct_new(type_id, field_instrs)
                } else {
                    WirInstr::Unreachable
                }
            }

            // === Field Access ===
            TirExprKind::FieldAccess {
                expr: receiver,
                field_name,
                ..
            } => {
                // If the field's result type is unit, emit only the receiver
                // for side effects and return Nop — unit has no Wasm representation.
                if expr.type_id == TypeTable::UNIT {
                    let recv = self.translate_expr(receiver);
                    return WirInstr::Seq(vec![WirInstr::Drop(Box::new(recv))]);
                }
                let recv = self.translate_expr(receiver);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, receiver.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    WirInstr::StructGet {
                        type_id,
                        field_name: field_name.clone(),
                        expr: Box::new(recv),
                    }
                } else {
                    WirInstr::Unreachable
                }
            }

            // === Assignment ===
            TirExprKind::Assign { target, value } => {
                let val = self.translate_expr(value);
                match &target.kind {
                    TirExprKind::Local { index, .. } => {
                        // Unit-type locals have no Wasm representation
                        if target.type_id == TypeTable::UNIT {
                            return val;
                        }
                        // If the value is a LocalSet from nested chained assignment
                        // (e.g., `h = i = 42`), convert it to LocalTee so it leaves
                        // the assigned value on the stack for the outer assignment.
                        let val = match val {
                            WirInstr::LocalSet {
                                name: inner_name,
                                value: inner_val,
                            } => WirInstr::LocalTee {
                                name: inner_name,
                                value: inner_val,
                            },
                            other => other,
                        };
                        let val = self.maybe_value_copy(value, val);
                        WirInstr::LocalSet {
                            name: self.local_name(*index),
                            value: Box::new(val),
                        }
                    }
                    TirExprKind::FieldAccess {
                        expr: receiver,
                        field_name: _,
                        ..
                    } if target.type_id == TypeTable::UNIT => {
                        // Unit-typed field assignment: the field has no Wasm
                        // representation. Emit the receiver for side effects (then
                        // drop the ref), and emit val for side effects (it produces
                        // nothing because unit has no Wasm representation).
                        let recv = self.translate_expr(receiver);
                        WirInstr::Seq(vec![val, WirInstr::Drop(Box::new(recv))])
                    }
                    TirExprKind::FieldAccess {
                        expr: receiver,
                        field_name,
                        ..
                    } => {
                        let recv = self.translate_expr(receiver);
                        let wir_type = self
                            .ctx
                            .type_id_to_wir_type(self.type_table, receiver.type_id);
                        if let WirType::Ref { type_id, .. } = wir_type {
                            self.struct_set(type_id, field_name.clone(), recv, val)
                        } else {
                            WirInstr::Unreachable
                        }
                    }
                    TirExprKind::Index {
                        expr: array_expr,
                        index: index_expr,
                    } => self.translate_index_assign(array_expr, index_expr, val),
                    _ => {
                        // Unhandled assignment target
                        WirInstr::Drop(Box::new(val))
                    }
                }
            }

            // === Cast ===
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                // Type casts become appropriate conversion instructions
                self.translate_cast(inner, inner.type_id, *target_type)
            }

            // === Block ===
            TirExprKind::Block(block) => {
                let body = if expr.type_id == TypeTable::UNIT {
                    self.translate_stmts(&block.stmts)
                } else {
                    self.translate_stmts_as_value(&block.stmts)
                };
                WirInstr::Seq(body)
            }

            // === If Expression ===
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.translate_expr(condition);
                let has_result = expr.type_id != TypeTable::UNIT;
                self.label_stack.push(LabelEntry {
                    label: None,
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let then_body = if has_result {
                    self.translate_stmts_as_value(&then_branch.stmts)
                } else {
                    self.translate_stmts(&then_branch.stmts)
                };
                let else_body = else_branch.as_ref().map(|b| {
                    if has_result {
                        self.translate_stmts_as_value(&b.stmts)
                    } else {
                        self.translate_stmts(&b.stmts)
                    }
                });
                self.label_stack.pop();
                let result_type = if has_result {
                    Some(self.ctx.type_id_to_wir_type(self.type_table, expr.type_id))
                } else {
                    None
                };
                WirInstr::If {
                    condition: Box::new(cond),
                    result: result_type,
                    then_body,
                    else_body,
                }
            }

            // === Match Expression ===
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => self.translate_match(scrutinee, arms, expr.type_id),

            // === Index ===
            TirExprKind::Index {
                expr: array_expr,
                index: index_expr,
            } => self.translate_index(array_expr, index_expr),

            // === Tuple Literal ===
            TirExprKind::TupleLiteral { elements } => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, expr.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    // Unit-typed elements have no Wasm representation; skip them.
                    let non_unit_elements: Vec<_> = elements
                        .iter()
                        .filter(|e| {
                            !matches!(
                                self.ctx.type_id_to_wir_type(self.type_table, e.type_id),
                                WirType::Unit
                            )
                        })
                        .collect();
                    let field_instrs: Vec<WirInstr> = non_unit_elements
                        .iter()
                        .map(|e| self.translate_expr(e))
                        .collect();
                    self.struct_new(type_id, field_instrs)
                } else {
                    WirInstr::Unreachable
                }
            }

            // === Switch (lowered pattern matching) ===
            TirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => self.translate_switch(scrutinee, *min_value, arms, default, expr.type_id),

            // === Variant Operations (lowered) ===
            TirExprKind::VariantTag { expr: inner } => {
                // Get discriminant field from variant base type
                let val = self.translate_expr(inner);
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    WirInstr::StructGet {
                        type_id,
                        field_name: "discriminant".to_string(),
                        expr: Box::new(val),
                    }
                } else {
                    WirInstr::I32Const(0)
                }
            }
            TirExprKind::VariantTest {
                expr: inner,
                case_index,
                case_name: _,
            } => self.translate_variant_test(inner, *case_index),
            TirExprKind::VariantPayload {
                expr: inner,
                case_index,
                payload_type: _,
            } => self.translate_variant_payload(inner, *case_index),
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => self.translate_variant_construct(
                *variant_type,
                *case_index,
                case_name,
                payload.as_deref(),
                expr.type_id,
            ),
            TirExprKind::EnumConstruct { case_index, .. } => WirInstr::I32Const(*case_index as i32),

            // === CM Raw Call ===
            TirExprKind::CmRawCall {
                local_name, args, ..
            } => {
                let translated_args: Vec<WirInstr> =
                    args.iter().map(|a| self.translate_expr(a)).collect();
                // Look up in WASI imports
                if let Some(func_id) = self.ctx.func_map.get(&format!("wasi/{local_name}")) {
                    WirInstr::Call {
                        func_id: func_id.clone(),
                        args: translated_args,
                    }
                } else {
                    eprintln!("[WIR] unresolved CmRawCall: local_name={local_name}");
                    WirInstr::Unreachable
                }
            }

            // === Closure ===
            TirExprKind::Closure { .. } => {
                // Closure should be lowered to StructLiteral or ClosureToCanonical
                // before reaching this point. If it's still here, emit unreachable.
                WirInstr::Unreachable
            }
            TirExprKind::Capture { .. } => {
                // Capture should be lowered to FieldAccess before reaching this point.
                WirInstr::Unreachable
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.translate_indirect_call(callee, args, expr.type_id)
            }
            TirExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
            } => self.translate_closure_to_canonical(functor, *functor_id, *target_fn_type),

            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should have been expanded before WIR build")
            }

            // === Labeled Block Expression ===
            TirExprKind::LabeledBlock { label, block, .. } => {
                let has_result = expr.type_id != TypeTable::UNIT;
                self.label_stack.push(LabelEntry {
                    label: Some(label.clone()),
                    is_loop_break: false,
                    is_loop_continue: false,
                });
                let body = if has_result {
                    self.translate_stmts_as_value(&block.stmts)
                } else {
                    self.translate_stmts(&block.stmts)
                };
                self.label_stack.pop();
                let result_type = if expr.type_id == TypeTable::UNIT {
                    None
                } else {
                    Some(self.ctx.type_id_to_wir_type(self.type_table, expr.type_id))
                };
                WirInstr::Block {
                    label: Some(label.clone()),
                    result: result_type,
                    body,
                }
            }
        }
    }

    /// Compute the `br` depth for a break statement.
    ///
    /// For labeled break: finds the block with the matching label.
    /// For unlabeled break: finds the outer block wrapping the innermost loop.
    fn compute_break_depth(&self, label: Option<&str>) -> u32 {
        for (i, entry) in self.label_stack.iter().rev().enumerate() {
            if let Some(target_label) = label {
                if entry.label.as_deref() == Some(target_label) {
                    return u32::try_from(i).unwrap();
                }
            } else if entry.is_loop_break {
                return u32::try_from(i).unwrap();
            }
        }
        // Fallback: depth 0 (should not happen with correct TIR)
        0
    }

    /// Compute the `br` depth for a continue statement.
    ///
    /// Finds the innermost loop instruction in the label stack.
    fn compute_continue_depth(&self) -> u32 {
        for (i, entry) in self.label_stack.iter().rev().enumerate() {
            if entry.is_loop_continue {
                return u32::try_from(i).unwrap();
            }
        }
        // Fallback: depth 0 (should not happen with correct TIR)
        0
    }

    /// Translate a string literal to WIR instructions.
    ///
    /// Creates a String struct from a passive data segment:
    ///   `array.new_data` $`u8_array`, $`data_idx` (offset=0, len=bytes)
    ///   struct.new $String (repr=array, used=len)
    fn translate_string_literal(&self, s: &str) -> WirInstr {
        let byte_len = s.len();

        // Look up u8 array type
        let u8_array_type = self.ctx.array_type_by_name.get("u8").cloned();

        // Look up String struct type
        let string_struct_name =
            crate::name::StructName::new(crate::name::ModuleSource::string(), "String".to_string());
        let string_type = self.ctx.struct_type_map.get(&string_struct_name).cloned();

        let (Some(array_type_id), Some(string_type_id)) = (u8_array_type, string_type) else {
            // Types not registered — fall back to unreachable
            return WirInstr::Unreachable;
        };

        if byte_len == 0 {
            // Empty string: array.new_default + struct.new
            self.struct_new(
                string_type_id,
                vec![
                    WirInstr::ArrayNewDefault {
                        type_id: array_type_id,
                        len: Box::new(WirInstr::I32Const(0)),
                    },
                    WirInstr::I32Const(0),
                ],
            )
        } else {
            // Non-empty string: look up data segment
            let data_index = self.ctx.string_literal_map.get(s).copied().unwrap_or(0);
            let len_i32 = i32::try_from(byte_len).unwrap_or(0);

            self.struct_new(
                string_type_id,
                vec![
                    WirInstr::ArrayNewData {
                        type_id: array_type_id,
                        data_index,
                        offset: Box::new(WirInstr::I32Const(0)),
                        len: Box::new(WirInstr::I32Const(len_i32)),
                    },
                    WirInstr::I32Const(len_i32),
                ],
            )
        }
    }

    /// Translate a bytes literal to WIR instructions.
    ///
    /// Creates an `Array<u8>` struct from a passive data segment:
    ///   `array.new_data` $`u8_array`, $`data_idx` (offset=0, len=bytes)
    ///   struct.new $`Array<u8>` (repr=array, used=len)
    fn translate_bytes_literal(&self, b: &[u8]) -> WirInstr {
        let byte_len = b.len();

        // Look up u8 GC array type
        let u8_array_type = self.ctx.array_type_by_name.get("u8").cloned();

        // Look up Array<u8> wrapper struct type
        let mangled =
            crate::name::mangle_generic_name("Array", std::slice::from_ref(&"u8".to_string()));
        let array_struct_name =
            crate::name::StructName::new(crate::name::ModuleSource::prelude(), mangled);
        let array_struct_type = self.ctx.struct_type_map.get(&array_struct_name).cloned();

        let (Some(gc_array_type_id), Some(struct_type_id)) = (u8_array_type, array_struct_type)
        else {
            return WirInstr::Unreachable;
        };

        if byte_len == 0 {
            self.struct_new(
                struct_type_id,
                vec![
                    WirInstr::ArrayNewDefault {
                        type_id: gc_array_type_id,
                        len: Box::new(WirInstr::I32Const(0)),
                    },
                    WirInstr::I32Const(0),
                ],
            )
        } else {
            let data_index = self.ctx.bytes_literal_map.get(b).copied().unwrap_or(0);
            let len_i32 = i32::try_from(byte_len).unwrap_or(0);

            self.struct_new(
                struct_type_id,
                vec![
                    WirInstr::ArrayNewData {
                        type_id: gc_array_type_id,
                        data_index,
                        offset: Box::new(WirInstr::I32Const(0)),
                        len: Box::new(WirInstr::I32Const(len_i32)),
                    },
                    WirInstr::I32Const(len_i32),
                ],
            )
        }
    }

    /// Translate a binary operation to WIR.
    fn translate_binary_op(
        &self,
        op: &TirBinaryOp,
        left: Box<WirInstr>,
        right: Box<WirInstr>,
        left_type_id: TypeId,
    ) -> WirInstr {
        // Resolve newtypes to their base primitive type
        let base_type_id = self.type_table.get_ultimate_base_type(left_type_id);
        let is_i64 = matches!(
            self.type_table.get(base_type_id),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        let is_f64 = matches!(
            self.type_table.get(base_type_id),
            ResolvedType::Primitive(PrimitiveType::F64)
        );
        let is_f32 = matches!(
            self.type_table.get(base_type_id),
            ResolvedType::Primitive(PrimitiveType::F32)
        );
        let is_unsigned = matches!(
            self.type_table.get(base_type_id),
            ResolvedType::Primitive(
                PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64
            )
        );

        match op {
            TirBinaryOp::Add => {
                if is_f64 {
                    WirInstr::F64Add(left, right)
                } else if is_f32 {
                    WirInstr::F32Add(left, right)
                } else if is_i64 {
                    WirInstr::I64Add(left, right)
                } else {
                    WirInstr::I32Add(left, right)
                }
            }
            TirBinaryOp::Sub => {
                if is_f64 {
                    WirInstr::F64Sub(left, right)
                } else if is_f32 {
                    WirInstr::F32Sub(left, right)
                } else if is_i64 {
                    WirInstr::I64Sub(left, right)
                } else {
                    WirInstr::I32Sub(left, right)
                }
            }
            TirBinaryOp::Mul => {
                if is_f64 {
                    WirInstr::F64Mul(left, right)
                } else if is_f32 {
                    WirInstr::F32Mul(left, right)
                } else if is_i64 {
                    WirInstr::I64Mul(left, right)
                } else {
                    WirInstr::I32Mul(left, right)
                }
            }
            TirBinaryOp::Div => {
                if is_f64 {
                    WirInstr::F64Div(left, right)
                } else if is_f32 {
                    WirInstr::F32Div(left, right)
                } else if is_i64 {
                    if is_unsigned {
                        WirInstr::I64DivU(left, right)
                    } else {
                        WirInstr::I64DivS(left, right)
                    }
                } else if is_unsigned {
                    WirInstr::I32DivU(left, right)
                } else {
                    WirInstr::I32DivS(left, right)
                }
            }
            TirBinaryOp::Mod => {
                if is_i64 {
                    if is_unsigned {
                        WirInstr::I64RemU(left, right)
                    } else {
                        WirInstr::I64RemS(left, right)
                    }
                } else if is_unsigned {
                    WirInstr::I32RemU(left, right)
                } else {
                    WirInstr::I32RemS(left, right)
                }
            }
            TirBinaryOp::Eq => {
                if is_f64 {
                    WirInstr::F64Eq(left, right)
                } else if is_f32 {
                    WirInstr::F32Eq(left, right)
                } else if is_i64 {
                    WirInstr::I64Eq(left, right)
                } else {
                    WirInstr::I32Eq(left, right)
                }
            }
            TirBinaryOp::NotEq => {
                if is_f64 {
                    WirInstr::F64Ne(left, right)
                } else if is_f32 {
                    WirInstr::F32Ne(left, right)
                } else if is_i64 {
                    WirInstr::I64Ne(left, right)
                } else {
                    WirInstr::I32Ne(left, right)
                }
            }
            TirBinaryOp::Lt => {
                if is_f64 {
                    WirInstr::F64Lt(left, right)
                } else if is_f32 {
                    WirInstr::F32Lt(left, right)
                } else if is_i64 {
                    if is_unsigned {
                        WirInstr::I64LtU(left, right)
                    } else {
                        WirInstr::I64LtS(left, right)
                    }
                } else if is_unsigned {
                    WirInstr::I32LtU(left, right)
                } else {
                    WirInstr::I32LtS(left, right)
                }
            }
            TirBinaryOp::LtEq => {
                if is_f64 {
                    WirInstr::F64Le(left, right)
                } else if is_f32 {
                    WirInstr::F32Le(left, right)
                } else if is_i64 {
                    if is_unsigned {
                        WirInstr::I64LeU(left, right)
                    } else {
                        WirInstr::I64LeS(left, right)
                    }
                } else if is_unsigned {
                    WirInstr::I32LeU(left, right)
                } else {
                    WirInstr::I32LeS(left, right)
                }
            }
            TirBinaryOp::Gt => {
                if is_f64 {
                    WirInstr::F64Gt(left, right)
                } else if is_f32 {
                    WirInstr::F32Gt(left, right)
                } else if is_i64 {
                    if is_unsigned {
                        WirInstr::I64GtU(left, right)
                    } else {
                        WirInstr::I64GtS(left, right)
                    }
                } else if is_unsigned {
                    WirInstr::I32GtU(left, right)
                } else {
                    WirInstr::I32GtS(left, right)
                }
            }
            TirBinaryOp::GtEq => {
                if is_f64 {
                    WirInstr::F64Ge(left, right)
                } else if is_f32 {
                    WirInstr::F32Ge(left, right)
                } else if is_i64 {
                    if is_unsigned {
                        WirInstr::I64GeU(left, right)
                    } else {
                        WirInstr::I64GeS(left, right)
                    }
                } else if is_unsigned {
                    WirInstr::I32GeU(left, right)
                } else {
                    WirInstr::I32GeS(left, right)
                }
            }
            TirBinaryOp::And => {
                if is_i64 {
                    WirInstr::I64And(left, right)
                } else {
                    WirInstr::I32And(left, right)
                }
            }
            TirBinaryOp::Or => {
                if is_i64 {
                    WirInstr::I64Or(left, right)
                } else {
                    WirInstr::I32Or(left, right)
                }
            }
            TirBinaryOp::BitAnd => {
                if is_i64 {
                    WirInstr::I64And(left, right)
                } else {
                    WirInstr::I32And(left, right)
                }
            }
            TirBinaryOp::BitOr => {
                if is_i64 {
                    WirInstr::I64Or(left, right)
                } else {
                    WirInstr::I32Or(left, right)
                }
            }
            TirBinaryOp::BitXor => {
                if is_i64 {
                    WirInstr::I64Xor(left, right)
                } else {
                    WirInstr::I32Xor(left, right)
                }
            }
            TirBinaryOp::Shl => {
                if is_i64 {
                    WirInstr::I64Shl(left, right)
                } else {
                    WirInstr::I32Shl(left, right)
                }
            }
            TirBinaryOp::Shr => {
                if is_i64 {
                    if is_unsigned {
                        WirInstr::I64ShrU(left, right)
                    } else {
                        WirInstr::I64ShrS(left, right)
                    }
                } else if is_unsigned {
                    WirInstr::I32ShrU(left, right)
                } else {
                    WirInstr::I32ShrS(left, right)
                }
            }
        }
    }

    /// Translate a unary operation to WIR.
    fn translate_unary_op(
        &self,
        op: &TirUnaryOp,
        operand: Box<WirInstr>,
        operand_type_id: TypeId,
    ) -> WirInstr {
        // Resolve newtypes to their base primitive type
        let base_type_id = self.type_table.get_ultimate_base_type(operand_type_id);
        let is_i64 = matches!(
            self.type_table.get(base_type_id),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        let is_f64 = matches!(
            self.type_table.get(base_type_id),
            ResolvedType::Primitive(PrimitiveType::F64)
        );
        let is_f32 = matches!(
            self.type_table.get(base_type_id),
            ResolvedType::Primitive(PrimitiveType::F32)
        );

        match op {
            TirUnaryOp::Neg => {
                if is_f64 {
                    WirInstr::F64Neg(operand)
                } else if is_f32 {
                    WirInstr::F32Neg(operand)
                } else if is_i64 {
                    WirInstr::I64Sub(Box::new(WirInstr::I64Const(0)), operand)
                } else {
                    WirInstr::I32Sub(Box::new(WirInstr::I32Const(0)), operand)
                }
            }
            TirUnaryOp::Not => WirInstr::I32Eqz(operand),
            TirUnaryOp::BitNot => {
                if is_i64 {
                    WirInstr::I64Xor(operand, Box::new(WirInstr::I64Const(-1)))
                } else {
                    WirInstr::I32Xor(operand, Box::new(WirInstr::I32Const(-1)))
                }
            }
            // Ref/MutRef/Deref handled above in translate_expr
            TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref => {
                WirInstr::Seq(vec![*operand])
            }
        }
    }

    /// Wrap an i32-producing instruction with sub-32-bit truncation if the
    /// target type is narrower than i32.
    fn truncate_to_sub_i32(instr: WirInstr, target: &PrimitiveType) -> WirInstr {
        match target {
            PrimitiveType::I8 => WirInstr::I32Extend8S(Box::new(instr)),
            PrimitiveType::U8 => {
                WirInstr::I32And(Box::new(instr), Box::new(WirInstr::I32Const(0xFF)))
            }
            PrimitiveType::I16 => WirInstr::I32Extend16S(Box::new(instr)),
            PrimitiveType::U16 => {
                WirInstr::I32And(Box::new(instr), Box::new(WirInstr::I32Const(0xFFFF)))
            }
            _ => instr,
        }
    }

    /// Translate a type cast.
    fn translate_cast(&mut self, inner: &TirExpr, from_type: TypeId, to_type: TypeId) -> WirInstr {
        // Optimize: IntLiteral cast to i64/u64 → emit I64Const directly to avoid i32 truncation
        let to_base = self.type_table.get_ultimate_base_type(to_type);
        if let TirExprKind::IntLiteral { value, .. } = &inner.kind
            && matches!(
                self.type_table.get(to_base),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
            )
        {
            return WirInstr::I64Const(*value as i64);
        }

        let inner_instr = self.translate_expr(inner);
        // Resolve newtypes to their base types for cast operations
        let from_base = self.type_table.get_ultimate_base_type(from_type);
        let from = self.type_table.get(from_base);
        let to = self.type_table.get(to_base);

        // Numeric casts: extension/conversion mode is determined by the source
        // type's signedness. Signed sources sign-extend, unsigned sources zero-extend.
        match (from, to) {
            // i32-like signed → i64/u64: sign-extend
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8,
                ),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64),
            ) => WirInstr::I64ExtendI32S(Box::new(inner_instr)),
            // i32-like unsigned → i64/u64: zero-extend
            (
                ResolvedType::Primitive(
                    PrimitiveType::U32
                    | PrimitiveType::U16
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64),
            ) => WirInstr::I64ExtendI32U(Box::new(inner_instr)),
            // i64/u64 → i32-like: wrap (truncate lower 32 bits)
            (
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I32
                    | PrimitiveType::U32
                    | PrimitiveType::I16
                    | PrimitiveType::U16
                    | PrimitiveType::I8
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char),
                ),
            ) => {
                let wrapped = WirInstr::I32WrapI64(Box::new(inner_instr));
                Self::truncate_to_sub_i32(wrapped, to_prim)
            }
            // i32-like signed → f64
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8,
                ),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI32S(Box::new(inner_instr)),
            // i32-like unsigned → f64
            (
                ResolvedType::Primitive(
                    PrimitiveType::U32
                    | PrimitiveType::U16
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI32U(Box::new(inner_instr)),
            // i32-like signed → f32
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8,
                ),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI32S(Box::new(inner_instr)),
            // i32-like unsigned → f32
            (
                ResolvedType::Primitive(
                    PrimitiveType::U32
                    | PrimitiveType::U16
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI32U(Box::new(inner_instr)),
            // i64 → f64 (signed)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI64S(Box::new(inner_instr)),
            // u64 → f64 (unsigned)
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI64U(Box::new(inner_instr)),
            // i64 → f32 (signed)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI64S(Box::new(inner_instr)),
            // u64 → f32 (unsigned)
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI64U(Box::new(inner_instr)),
            // f64 → signed i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8),
                ),
            ) => {
                let truncated = WirInstr::I32TruncF64S(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f64 → unsigned i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::U32 | PrimitiveType::U16 | PrimitiveType::U8),
                ),
            ) => {
                let truncated = WirInstr::I32TruncF64U(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f64 → i64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => WirInstr::I64TruncF64S(Box::new(inner_instr)),
            // f64 → u64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => WirInstr::I64TruncF64U(Box::new(inner_instr)),
            // f32 → signed i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8),
                ),
            ) => {
                let truncated = WirInstr::I32TruncF32S(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f32 → unsigned i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::U32 | PrimitiveType::U16 | PrimitiveType::U8),
                ),
            ) => {
                let truncated = WirInstr::I32TruncF32U(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f32 → i64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => WirInstr::I64TruncF32S(Box::new(inner_instr)),
            // f32 → u64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => WirInstr::I64TruncF32U(Box::new(inner_instr)),
            // f64 ↔ f32
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32DemoteF64(Box::new(inner_instr)),
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64PromoteF32(Box::new(inner_instr)),
            // Same-Wasm-size narrowing (e.g., i32 → u8, u32 → i16)
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32
                    | PrimitiveType::U32
                    | PrimitiveType::I16
                    | PrimitiveType::U16
                    | PrimitiveType::I8
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I8
                    | PrimitiveType::U8
                    | PrimitiveType::I16
                    | PrimitiveType::U16),
                ),
            ) => Self::truncate_to_sub_i32(inner_instr, to_prim),
            _ => {
                // For other casts (struct casts, etc.), just pass through
                inner_instr
            }
        }
    }

    /// Resolve a TIR `FunctionRef` to a `WirFuncId`.
    fn resolve_function_ref(
        &self,
        func_ref: &crate::tir::FunctionRef,
    ) -> Option<crate::wir::WirFuncId> {
        let module_source = &func_ref.module_source;
        let name = &func_ref.name;

        // Try direct name lookup
        let fq = format!("{module_source}/{name}");
        if let Some(id) = self.ctx.func_map.get(&fq) {
            return Some(id.clone());
        }
        // Try alias registered during import collection (builtin/{func_name})
        let alias = format!("builtin/{name}");
        if let Some(id) = self.ctx.func_map.get(&alias) {
            return Some(id.clone());
        }
        // Try with method info
        if let Some(method_info) = &func_ref.method_info {
            let mangled = method_info.to_mangled_name();
            let fq2 = format!("{module_source}/{mangled}");
            if let Some(id) = self.ctx.func_map.get(&fq2) {
                return Some(id.clone());
            }
            // Newtype fallback: if struct name is a newtype, try the base type name
            if let Some(id) = self.resolve_newtype_method(module_source, method_info) {
                return Some(id);
            }
        }
        // Try suffix match
        let suffix = format!("/{name}");
        for key in self.ctx.func_map.keys() {
            if key.ends_with(&suffix) {
                return self.ctx.func_map.get(key).cloned();
            }
        }
        None
    }

    /// Try to resolve a method call on a newtype by substituting the base type name.
    /// For example, `Location::sum` → `Point::sum` when `type Location = Point`.
    /// Follows the newtype chain for chained newtypes (C → B → A → Point).
    fn resolve_newtype_method(
        &self,
        module_source: &crate::name::ModuleSource,
        method_info: &crate::name::LocalMethodName,
    ) -> Option<crate::wir::WirFuncId> {
        let struct_name = &method_info.base_struct_name;
        // Find a Newtype in the type table with this name
        let base_name = self.resolve_newtype_to_base_struct_name(struct_name)?;
        // Build a new method name with the base type's struct name
        let mut resolved_info = method_info.clone();
        resolved_info.struct_name = base_name.clone();
        resolved_info.base_struct_name = base_name;
        let mangled = resolved_info.to_mangled_name();
        let fq = format!("{module_source}/{mangled}");
        if let Some(id) = self.ctx.func_map.get(&fq) {
            return Some(id.clone());
        }
        // Try suffix match with the resolved name
        let suffix = format!("/{mangled}");
        for key in self.ctx.func_map.keys() {
            if key.ends_with(&suffix) {
                return self.ctx.func_map.get(key).cloned();
            }
        }
        None
    }

    /// Resolve a newtype name to the ultimate base struct/primitive name.
    /// Returns `None` if the name is not a newtype.
    fn resolve_newtype_to_base_struct_name(&self, name: &str) -> Option<String> {
        // Search the type table for a Newtype with the given name
        for type_id in self.type_table.iter_type_ids() {
            if let ResolvedType::Newtype {
                name: newtype_name,
                base_type,
                ..
            } = self.type_table.get(type_id)
                && newtype_name == name
            {
                // Follow the chain to the ultimate base type
                let ultimate = self.type_table.get_ultimate_base_type(*base_type);
                return Some(self.type_table.type_name(ultimate));
            }
        }
        None
    }

    /// Translate a builtin intrinsic call to a WIR instruction.
    ///
    /// Returns `Some(instr)` for instruction-builtins (Wasm instructions),
    /// `None` for import-builtins (handled as regular function calls).
    fn translate_builtin_call(
        &mut self,
        builtin_name: &str,
        args: &[TirExpr],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        match builtin_name {
            // === Memory Load Instructions ===
            "builtin::i32_load" => {
                let addr = self.translate_expr(&args[0]);
                Some(WirInstr::I32Load {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load8_u" => {
                let addr = self.translate_expr(&args[0]);
                Some(WirInstr::I32Load8U {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load16_u" => {
                let addr = self.translate_expr(&args[0]);
                Some(WirInstr::I32Load16U {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                })
            }

            "builtin::i64_load" => {
                let addr = self.translate_expr(&args[0]);
                Some(WirInstr::I64Load {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                })
            }

            // === Memory Store Instructions ===
            "builtin::i32_store" => {
                let addr = self.translate_expr(&args[0]);
                let val = self.translate_expr(&args[1]);
                Some(WirInstr::I32Store {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store8" => {
                let addr = self.translate_expr(&args[0]);
                let val = self.translate_expr(&args[1]);
                Some(WirInstr::I32Store8 {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store16" => {
                let addr = self.translate_expr(&args[0]);
                let val = self.translate_expr(&args[1]);
                Some(WirInstr::I32Store16 {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i64_store" => {
                let addr = self.translate_expr(&args[0]);
                let val = self.translate_expr(&args[1]);
                Some(WirInstr::I64Store {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }

            // === Array GC Instructions ===
            "builtin::array_new" => {
                // array.new_default: creates a new array of the given element type
                let len = self.translate_expr(&args[0]);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, result_type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayNewDefault {
                        type_id,
                        len: Box::new(len),
                    })
                } else {
                    None
                }
            }
            "builtin::array_len" => {
                let arr = self.translate_expr(&args[0]);
                Some(WirInstr::ArrayLen(Box::new(arr)))
            }
            "builtin::array_get_u8" => {
                let arr = self.translate_expr(&args[0]);
                let idx = self.translate_expr(&args[1]);
                self.ctx
                    .array_type_by_name
                    .get("u8")
                    .map(|type_id| WirInstr::ArrayGetU {
                        type_id: type_id.clone(),
                        array: Box::new(arr),
                        index: Box::new(idx),
                    })
            }
            "builtin::array_set_u8" => {
                let arr = self.translate_expr(&args[0]);
                let idx = self.translate_expr(&args[1]);
                let val = self.translate_expr(&args[2]);
                self.ctx
                    .array_type_by_name
                    .get("u8")
                    .map(|type_id| WirInstr::ArraySet {
                        type_id: type_id.clone(),
                        array: Box::new(arr),
                        index: Box::new(idx),
                        value: Box::new(val),
                    })
            }
            "builtin::array_get" => {
                let arr = self.translate_expr(&args[0]);
                let idx = self.translate_expr(&args[1]);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayGet {
                        type_id,
                        array: Box::new(arr),
                        index: Box::new(idx),
                    })
                } else {
                    None
                }
            }
            "builtin::array_set" => {
                let arr = self.translate_expr(&args[0]);
                let idx = self.translate_expr(&args[1]);
                let val = self.translate_expr(&args[2]);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArraySet {
                        type_id,
                        array: Box::new(arr),
                        index: Box::new(idx),
                        value: Box::new(val),
                    })
                } else {
                    None
                }
            }
            "builtin::array_copy" => {
                let dst = self.translate_expr(&args[0]);
                let dst_offset = self.translate_expr(&args[1]);
                let src = self.translate_expr(&args[2]);
                let src_offset = self.translate_expr(&args[3]);
                let len = self.translate_expr(&args[4]);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayCopy {
                        dest_type_id: type_id.clone(),
                        src_type_id: type_id,
                        dest: Box::new(dst),
                        dest_offset: Box::new(dst_offset),
                        src: Box::new(src),
                        src_offset: Box::new(src_offset),
                        len: Box::new(len),
                    })
                } else {
                    None
                }
            }
            "builtin::array_fill" => {
                let arr = self.translate_expr(&args[0]);
                let offset = self.translate_expr(&args[1]);
                let val = self.translate_expr(&args[2]);
                let len = self.translate_expr(&args[3]);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayFill {
                        type_id,
                        array: Box::new(arr),
                        offset: Box::new(offset),
                        value: Box::new(val),
                        len: Box::new(len),
                    })
                } else {
                    None
                }
            }

            // === Float Math (single-instruction) ===
            "builtin::f64_abs" => unary_f64!(self, args, WirInstr::F64Abs),
            "builtin::f64_ceil" => unary_f64!(self, args, WirInstr::F64Ceil),
            "builtin::f64_floor" => unary_f64!(self, args, WirInstr::F64Floor),
            "builtin::f64_trunc" => unary_f64!(self, args, WirInstr::F64Trunc),
            "builtin::f64_nearest" => unary_f64!(self, args, WirInstr::F64Nearest),
            "builtin::f64_sqrt" => unary_f64!(self, args, WirInstr::F64Sqrt),
            "builtin::f64_min" => binary_f64!(self, args, WirInstr::F64Min),
            "builtin::f64_max" => binary_f64!(self, args, WirInstr::F64Max),
            "builtin::f64_copysign" => binary_f64!(self, args, WirInstr::F64Copysign),
            "builtin::f32_abs" => unary_f32!(self, args, WirInstr::F32Abs),
            "builtin::f32_ceil" => unary_f32!(self, args, WirInstr::F32Ceil),
            "builtin::f32_floor" => unary_f32!(self, args, WirInstr::F32Floor),
            "builtin::f32_trunc" => unary_f32!(self, args, WirInstr::F32Trunc),
            "builtin::f32_nearest" => unary_f32!(self, args, WirInstr::F32Nearest),
            "builtin::f32_sqrt" => unary_f32!(self, args, WirInstr::F32Sqrt),
            "builtin::f32_min" => binary_f32!(self, args, WirInstr::F32Min),
            "builtin::f32_max" => binary_f32!(self, args, WirInstr::F32Max),
            "builtin::f32_copysign" => binary_f32!(self, args, WirInstr::F32Copysign),

            // === Bitwise/Integer ===
            "builtin::i32_and" => {
                let l = self.translate_expr(&args[0]);
                let r = self.translate_expr(&args[1]);
                Some(WirInstr::I32And(Box::new(l), Box::new(r)))
            }
            "builtin::i32_eqz" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I32Eqz(Box::new(o)))
            }
            "builtin::i32_clz" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I32Clz(Box::new(o)))
            }
            "builtin::i64_clz" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I64Clz(Box::new(o)))
            }
            "builtin::i32_ctz" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I32Ctz(Box::new(o)))
            }
            "builtin::i64_ctz" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I64Ctz(Box::new(o)))
            }
            "builtin::i32_popcnt" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I32Popcnt(Box::new(o)))
            }
            "builtin::i64_popcnt" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I64Popcnt(Box::new(o)))
            }

            // === Reinterpretation ===
            "builtin::i64_reinterpret_f64" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I64ReinterpretF64(Box::new(o)))
            }
            "builtin::f64_reinterpret_i64" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::F64ReinterpretI64(Box::new(o)))
            }
            "builtin::i32_reinterpret_f32" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::I32ReinterpretF32(Box::new(o)))
            }
            "builtin::f32_reinterpret_i32" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::F32ReinterpretI32(Box::new(o)))
            }

            // === Memory ===
            "builtin::memory_grow" => {
                let o = self.translate_expr(&args[0]);
                Some(WirInstr::MemoryGrow(Box::new(o)))
            }
            "builtin::memory_size" => Some(WirInstr::MemorySize),

            // === Control ===
            "builtin::unreachable" => Some(WirInstr::Unreachable),
            "builtin::likely" | "builtin::unlikely" => {
                let likely = builtin_name == "builtin::likely";
                let expr = self.translate_expr(&args[0]);
                Some(WirInstr::BranchHint {
                    likely,
                    expr: Box::new(expr),
                })
            }
            "builtin::select" => {
                let cond = self.translate_expr(&args[0]);
                let a = self.translate_expr(&args[1]);
                let b = self.translate_expr(&args[2]);
                let result_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[1].type_id);
                Some(WirInstr::Select {
                    condition: Box::new(cond),
                    if_true: Box::new(a),
                    if_false: Box::new(b),
                    ty: Some(result_type),
                })
            }

            // === Multi-value 128-bit integer operations ===
            "builtin::i64_add128" => {
                let a_lo = Box::new(self.translate_expr(&args[0]));
                let a_hi = Box::new(self.translate_expr(&args[1]));
                let b_lo = Box::new(self.translate_expr(&args[2]));
                let b_hi = Box::new(self.translate_expr(&args[3]));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Add128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_sub128" => {
                let a_lo = Box::new(self.translate_expr(&args[0]));
                let a_hi = Box::new(self.translate_expr(&args[1]));
                let b_lo = Box::new(self.translate_expr(&args[2]));
                let b_hi = Box::new(self.translate_expr(&args[3]));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Sub128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_mul_wide_u" => {
                let a = Box::new(self.translate_expr(&args[0]));
                let b = Box::new(self.translate_expr(&args[1]));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideU(a, b), result_type_id))
            }
            "builtin::i64_mul_wide_s" => {
                let a = Box::new(self.translate_expr(&args[0]));
                let b = Box::new(self.translate_expr(&args[1]));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideS(a, b), result_type_id))
            }

            // === No-op casts ===
            "builtin::i32_as_char" => Some(self.translate_expr(&args[0])),

            // === WASI call indirects ===
            "builtin::call_indirect_stdout_write_via_stream"
            | "builtin::call_indirect_stderr_write_via_stream" => {
                // These pass the argument and add i32.const 2048 (buffer_size),
                // then call the appropriate WASI write_via_stream function.
                let is_stderr = builtin_name.contains("stderr");
                let wasi_func_name = if is_stderr {
                    "wasi:cli/Stderr::write_via_stream"
                } else {
                    "wasi:cli/Stdout::write_via_stream"
                };
                let key = format!("wasi/{wasi_func_name}");
                if let Some(func_id) = self.ctx.func_map.get(&key).cloned() {
                    let mut call_args: Vec<WirInstr> =
                        args.iter().map(|a| self.translate_expr(a)).collect();
                    call_args.push(WirInstr::I32Const(2048));
                    Some(WirInstr::Call {
                        func_id,
                        args: call_args,
                    })
                } else {
                    None
                }
            }

            // Not an instruction-builtin; fall through to function call resolution
            _ => None,
        }
    }

    /// Wrap a multi-value [i64, i64] instruction in a tuple struct.
    /// The Wasm instruction pushes two i64s on the stack; we wrap them
    /// in a `StructNew` for the result tuple type [i64, i64].
    fn wrap_multivalue_i64(&self, instr: WirInstr, result_type_id: TypeId) -> WirInstr {
        let wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        if let WirType::Ref { type_id, .. } = wir_type {
            // The multi-value instr pushes two i64s, then StructNew wraps them
            WirInstr::MultiValueStructNew {
                type_id,
                instr: Box::new(instr),
            }
        } else {
            // Fallback: just emit the instruction (shouldn't happen)
            instr
        }
    }

    // =========================================================================
    // Canonical resource method dispatch
    // =========================================================================

    /// Dispatch canonical resource methods based on `#[canonical("...")]` attribute.
    /// Returns `Some(WirInstr)` if the method has a canonical name and was handled.
    fn try_translate_canonical_method(
        &mut self,
        receiver: &TirExpr,
        func: &FunctionRef,
        args: &[TirExpr],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        let canonical_name = func.method_info.clone()?.canonical_name.as_ref()?.clone();
        let handle = self.translate_expr(receiver);
        match canonical_name.as_str() {
            // === Stream instance methods ===
            "stream-read" => {
                let max_arg = self.translate_expr(&args[0]);
                Some(self.emit_stream_read(handle, max_arg, result_type_id))
            }
            "stream-write" => {
                let data_arg = self.translate_expr(&args[0]);
                Some(self.emit_stream_write(handle, data_arg))
            }
            "stream-drop-readable" => Some(self.emit_drop_handle("stream-drop-readable", handle)),
            "stream-drop-writable" => Some(self.emit_drop_handle("stream-drop-writable", handle)),

            // === Future instance methods ===
            "future-read" => Some(self.emit_future_read(handle, result_type_id)),
            "future-write" => Some(self.emit_future_write_ok_none(handle)),
            "future-drop-readable" => Some(self.emit_drop_handle("future-drop-readable", handle)),
            "future-drop-writable" => Some(self.emit_drop_handle("future-drop-writable", handle)),

            // === WaitableSet instance methods ===
            "waitable-set-wait" => Some(self.emit_waitable_set_wait(handle, result_type_id)),
            "waitable-set-poll" => Some(self.emit_waitable_set_poll(handle, result_type_id)),
            "waitable-set-drop" => Some(self.emit_drop_handle("waitable-set-drop", handle)),

            // === Subtask instance methods ===
            "subtask-drop" => Some(self.emit_drop_handle("subtask-drop", handle)),
            "waitable-join" => {
                let set_arg = self.translate_expr(&args[0]);
                Some(self.emit_waitable_join(handle, set_arg))
            }

            // === ErrorContext instance methods ===
            "error-context-debug-message" => {
                panic!("not yet implemented: error-context-debug-message synthesis")
            }
            "error-context-drop" => Some(self.emit_drop_handle("error-context-drop", handle)),

            other => {
                eprintln!("[WIR] unhandled canonical method: {other}");
                None
            }
        }
    }

    /// Dispatch canonical resource static methods (e.g., `Stream::new`, `WaitableSet::new`).
    /// Returns `Some(WirInstr)` if the canonical name was handled.
    fn try_translate_canonical_static_method(
        &mut self,
        canonical: &str,
        args: &[TirExpr],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        match canonical {
            "stream-new" => Some(self.emit_stream_or_future_new(false, result_type_id)),
            "future-new" => Some(self.emit_stream_or_future_new(true, result_type_id)),
            "waitable-set-new" => Some(self.emit_waitable_set_new()),
            "error-context-new" => {
                let _ = args; // suppress unused warning
                panic!("not yet implemented: error-context-new synthesis")
            }
            _ => None,
        }
    }

    /// Emit a canonical drop call: `{canonical}(handle)`.
    ///
    /// Used for all CM resource drop operations:
    /// `stream-drop-readable`, `stream-drop-writable`,
    /// `future-drop-readable`, `future-drop-writable`,
    /// `waitable-set-drop`, `subtask-drop`, `error-context-drop`.
    fn emit_drop_handle(&mut self, canonical: &str, handle: WirInstr) -> WirInstr {
        let func_id = self
            .ctx
            .ensure_canonical(canonical, vec![WirType::I32], vec![]);
        WirInstr::Call {
            func_id,
            args: vec![handle],
        }
    }

    /// Emit `stream-new()` or `future-new()` → split i64 into [`rx_i32`, `tx_i32`] tuple.
    fn emit_stream_or_future_new(&mut self, is_future: bool, result_type_id: TypeId) -> WirInstr {
        let canonical = if is_future {
            "future-new"
        } else {
            "stream-new"
        };
        let func_id = self
            .ctx
            .ensure_canonical(canonical, vec![], vec![WirType::I64]);

        // Resolve the result tuple type for StructNew
        let wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        let type_id = match wir_type {
            WirType::Ref { type_id, .. } => type_id,
            _ => return WirInstr::Unreachable,
        };

        // Declare a temp local, call import → i64, split into [rx, tx] tuple
        self.local_counter += 1;
        let temp = format!("__pair_temp_{}", self.local_counter);
        let declare = WirInstr::DeclareLocal {
            name: temp.clone(),
            ty: WirType::I64,
        };
        let call = WirInstr::Call {
            func_id,
            args: vec![],
        };
        let set_temp = WirInstr::LocalSet {
            name: temp.clone(),
            value: Box::new(call),
        };
        let get_low = WirInstr::I32WrapI64(Box::new(WirInstr::LocalGet { name: temp.clone() }));
        let get_high = WirInstr::I32WrapI64(Box::new(WirInstr::I64ShrU(
            Box::new(WirInstr::LocalGet { name: temp }),
            Box::new(WirInstr::I64Const(32)),
        )));

        let mut instrs = vec![declare, set_temp];
        instrs.push(self.struct_new(type_id, vec![get_low, get_high]));
        WirInstr::Seq(instrs)
    }

    /// Emit `waitable-set-new()` → i32 (waitable set handle).
    fn emit_waitable_set_new(&mut self) -> WirInstr {
        let func_id = self
            .ctx
            .ensure_canonical("waitable-set-new", vec![], vec![WirType::I32]);
        WirInstr::Call {
            func_id,
            args: vec![],
        }
    }

    /// Emit `waitable-join(subtask_handle, waitable_set_handle)`.
    ///
    /// `waitable.join` is void — it adds the subtask to the set for monitoring.
    /// The waitable token identifying the subtask in `WaitEvent.handle` is the
    /// subtask handle itself (first argument). We save it before the void call
    /// and return it so callers of `Subtask::join() -> Waitable` get the token.
    fn emit_waitable_join(&mut self, handle: WirInstr, set_arg: WirInstr) -> WirInstr {
        let func_id =
            self.ctx
                .ensure_canonical("waitable-join", vec![WirType::I32, WirType::I32], vec![]);
        self.local_counter += 1;
        let suffix = self.local_counter;
        let handle_name = format!("__wj_handle_{suffix}");
        WirInstr::Seq(vec![
            WirInstr::DeclareLocal {
                name: handle_name.clone(),
                ty: WirType::I32,
            },
            WirInstr::LocalSet {
                name: handle_name.clone(),
                value: Box::new(handle),
            },
            WirInstr::Call {
                func_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                    },
                    set_arg,
                ],
            },
            WirInstr::LocalGet { name: handle_name },
        ])
    }

    /// Emit `waitable-set-wait(ws_handle)` → `WaitEvent` struct.
    fn emit_waitable_set_wait(&mut self, _handle: WirInstr, _result_type_id: TypeId) -> WirInstr {
        panic!("not yet implemented: waitable-set-wait synthesis (WaitEvent struct lowering)")
    }

    /// Emit `waitable-set-poll(ws_handle)` → Option<WaitEvent>.
    fn emit_waitable_set_poll(&mut self, _handle: WirInstr, _result_type_id: TypeId) -> WirInstr {
        panic!("not yet implemented: waitable-set-poll synthesis (Option<WaitEvent> lowering)")
    }

    /// Emit `future-read(handle)` → Option<T>.
    fn emit_future_read(&mut self, _handle: WirInstr, _result_type_id: TypeId) -> WirInstr {
        panic!("not yet implemented: future-read synthesis (Option<T> lowering)")
    }

    /// Emit WIR to write `Ok(None)` (8 zero bytes) into a `FutureWritable` handle,
    /// then free the temporary buffer.
    ///
    /// Hardcoded encoding for `Result<Option<Trailers>, ErrorCode>::Ok(null)`:
    ///   - 4 bytes at offset 0: Ok discriminant (0)
    ///   - 4 bytes at offset 4: None discriminant (0)
    ///
    /// TODO: implement general CM lowering for arbitrary T values.
    fn emit_future_write_ok_none(&mut self, handle: WirInstr) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            return WirInstr::Unreachable;
        };
        let future_write_id = self.ctx.ensure_canonical(
            "future-write",
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let ptr_name = format!("__fw_write_ptr_{}", self.local_counter);

        // The buffer must be large enough for the full CM layout of
        // result<option<own<trailers>>, error-code>. ErrorCode is a large variant
        // whose biggest cases contain option<string> (12 bytes) or dns-error-payload
        // (option<string> + option<u16> = 16 bytes). We allocate 40 bytes to cover
        // the worst case and zero-initialize the entire buffer, since Ok(None) is
        // represented as all-zeros.
        const BUF_SIZE: i32 = 40;
        const BUF_ALIGN: i32 = 8;

        let declare_ptr = WirInstr::DeclareLocal {
            name: ptr_name.clone(),
            ty: WirType::I32,
        };
        let alloc_ptr = WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: realloc_id.clone(),
                args: vec![
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(BUF_ALIGN),
                    WirInstr::I32Const(BUF_SIZE),
                ],
            }),
        };

        // Zero-initialize the entire buffer using i64 stores (8 bytes each).
        let mut seq = vec![declare_ptr, alloc_ptr];
        for i in 0..(BUF_SIZE / 8) {
            seq.push(WirInstr::I64Store {
                offset: u64::from((i * 8).cast_unsigned()),
                align: 3,
                addr: Box::new(WirInstr::LocalGet {
                    name: ptr_name.clone(),
                }),
                value: Box::new(WirInstr::I64Const(0)),
            });
        }

        // Drop result of future_write(handle, ptr)
        seq.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: future_write_id,
            args: vec![
                handle,
                WirInstr::LocalGet {
                    name: ptr_name.clone(),
                },
            ],
        })));
        // Free the buffer after future_write.
        seq.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet { name: ptr_name },
                WirInstr::I32Const(BUF_SIZE),
                WirInstr::I32Const(BUF_ALIGN),
                WirInstr::I32Const(0),
            ],
        })));

        WirInstr::Seq(seq)
    }

    /// Emit WIR for `Stream<u8>::read(max) -> Array<u8>`.
    ///
    /// 1. Allocate `max` bytes in linear memory
    /// 2. Call `stream_read(rx, ptr, max)` → `raw_result`
    /// 3. If BLOCKED (`0xFFFF_FFFF`), wait via waitable-set, re-read result
    /// 4. Extract count = `raw_result` >> 4
    /// 5. Copy from linear memory to GC array
    /// 6. Free linear memory
    /// 7. Wrap in Array<u8> struct
    fn emit_stream_read(
        &mut self,
        handle: WirInstr,
        max_arg: WirInstr,
        result_type_id: TypeId,
    ) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            return WirInstr::Unreachable;
        };
        let stream_read_id = self.ctx.ensure_canonical(
            "stream-read",
            vec![WirType::I32, WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        let ws_new_id = self
            .ctx
            .ensure_canonical("waitable-set-new", vec![], vec![WirType::I32]);
        let w_join_id =
            self.ctx
                .ensure_canonical("waitable-join", vec![WirType::I32, WirType::I32], vec![]);
        let ws_wait_id = self.ctx.ensure_canonical(
            "waitable-set-wait",
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        self.local_counter += 1;
        let suffix = self.local_counter;
        let ptr_name = format!("__sr_ptr_{suffix}");
        let max_name = format!("__sr_max_{suffix}");
        let result_name = format!("__sr_result_{suffix}");
        let handle_name = format!("__sr_handle_{suffix}");
        let count_name = format!("__sr_count_{suffix}");
        let evt_ptr_name = format!("__sr_evtptr_{suffix}");
        let repr_name = format!("__sr_repr_{suffix}");
        let idx_name = format!("__sr_idx_{suffix}");

        let mut instrs = vec![];

        // Declare locals
        for (name, ty) in [
            (&handle_name, WirType::I32),
            (&max_name, WirType::I32),
            (&ptr_name, WirType::I32),
            (&result_name, WirType::I32),
            (&count_name, WirType::I32),
        ] {
            instrs.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        // Save handle and max
        instrs.push(WirInstr::LocalSet {
            name: handle_name.clone(),
            value: Box::new(handle),
        });
        instrs.push(WirInstr::LocalSet {
            name: max_name.clone(),
            value: Box::new(max_arg),
        });

        // ptr = realloc(0, 0, 1, max)
        instrs.push(WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: realloc_id.clone(),
                args: vec![
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(1),
                    WirInstr::LocalGet {
                        name: max_name.clone(),
                    },
                ],
            }),
        });

        // result = stream_read(handle, ptr, max)
        instrs.push(WirInstr::LocalSet {
            name: result_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: stream_read_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                    },
                    WirInstr::LocalGet {
                        name: ptr_name.clone(),
                    },
                    WirInstr::LocalGet {
                        name: max_name.clone(),
                    },
                ],
            }),
        });

        // If result == 0xFFFF_FFFF (BLOCKED), wait via waitable-set
        instrs.push(WirInstr::If {
            condition: Box::new(WirInstr::I32Eq(
                Box::new(WirInstr::LocalGet {
                    name: result_name.clone(),
                }),
                Box::new(WirInstr::I32Const(-1)), // 0xFFFF_FFFF as i32
            )),
            result: None,
            then_body: {
                let evt = evt_ptr_name.clone();
                vec![
                    WirInstr::DeclareLocal {
                        name: evt.clone(),
                        ty: WirType::I32,
                    },
                    // ws = waitable_set_new() — reuse result_name as temp
                    WirInstr::LocalSet {
                        name: result_name.clone(),
                        value: Box::new(WirInstr::Call {
                            func_id: ws_new_id,
                            args: vec![],
                        }),
                    },
                    // waitable_join(handle, ws)
                    WirInstr::Call {
                        func_id: w_join_id,
                        args: vec![
                            WirInstr::LocalGet {
                                name: handle_name.clone(),
                            },
                            WirInstr::LocalGet {
                                name: result_name.clone(),
                            },
                        ],
                    },
                    // evt_ptr = realloc(0, 0, 4, 8)
                    WirInstr::LocalSet {
                        name: evt.clone(),
                        value: Box::new(WirInstr::Call {
                            func_id: realloc_id.clone(),
                            args: vec![
                                WirInstr::I32Const(0),
                                WirInstr::I32Const(0),
                                WirInstr::I32Const(4),
                                WirInstr::I32Const(8),
                            ],
                        }),
                    },
                    // waitable_set_wait(ws, evt_ptr)
                    WirInstr::Drop(Box::new(WirInstr::Call {
                        func_id: ws_wait_id,
                        args: vec![
                            WirInstr::LocalGet {
                                name: result_name.clone(),
                            },
                            WirInstr::LocalGet { name: evt.clone() },
                        ],
                    })),
                    // result = i32.load(evt_ptr + 4)
                    WirInstr::LocalSet {
                        name: result_name.clone(),
                        value: Box::new(WirInstr::I32Load {
                            offset: 4,
                            align: 2,
                            addr: Box::new(WirInstr::LocalGet { name: evt.clone() }),
                        }),
                    },
                    // Free event buffer
                    WirInstr::Drop(Box::new(WirInstr::Call {
                        func_id: realloc_id.clone(),
                        args: vec![
                            WirInstr::LocalGet { name: evt },
                            WirInstr::I32Const(8),
                            WirInstr::I32Const(4),
                            WirInstr::I32Const(0),
                        ],
                    })),
                ]
            },
            else_body: None,
        });

        // count = result >> 4
        instrs.push(WirInstr::LocalSet {
            name: count_name.clone(),
            value: Box::new(WirInstr::I32ShrU(
                Box::new(WirInstr::LocalGet {
                    name: result_name.clone(),
                }),
                Box::new(WirInstr::I32Const(4)),
            )),
        });

        // Resolve the Array<u8> struct type and its repr field's array type
        let array_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        let array_struct_type_id = match array_wir_type {
            WirType::Ref { type_id, .. } => type_id,
            _ => return WirInstr::Unreachable,
        };
        let repr_array_type_id = match &self.ctx.types[array_struct_type_id.index() as usize] {
            WirTypeDef::Struct(s) => match &s.fields[0].ty {
                WirType::Ref { type_id, .. } => type_id.clone(),
                _ => return WirInstr::Unreachable,
            },
            _ => return WirInstr::Unreachable,
        };

        // repr = array.new_default $i32_array count
        instrs.push(WirInstr::DeclareLocal {
            name: repr_name.clone(),
            ty: WirType::Ref {
                type_id: repr_array_type_id.clone(),
                nullable: false,
            },
        });
        instrs.push(WirInstr::LocalSet {
            name: repr_name.clone(),
            value: Box::new(WirInstr::ArrayNewDefault {
                type_id: repr_array_type_id.clone(),
                len: Box::new(WirInstr::LocalGet {
                    name: count_name.clone(),
                }),
            }),
        });

        // Loop: repr[i] = i32.load8_u(ptr + i) for i in 0..count
        instrs.push(WirInstr::DeclareLocal {
            name: idx_name.clone(),
            ty: WirType::I32,
        });
        instrs.push(WirInstr::LocalSet {
            name: idx_name.clone(),
            value: Box::new(WirInstr::I32Const(0)),
        });
        instrs.push(WirInstr::Block {
            label: Some(format!("__sr_brk_{suffix}")),
            result: None,
            body: vec![WirInstr::Loop {
                label: Some(format!("__sr_lp_{suffix}")),
                body: vec![
                    // br_if $break (i >= count)
                    WirInstr::BrIf {
                        depth: 1,
                        condition: Box::new(WirInstr::I32GeU(
                            Box::new(WirInstr::LocalGet {
                                name: idx_name.clone(),
                            }),
                            Box::new(WirInstr::LocalGet {
                                name: count_name.clone(),
                            }),
                        )),
                    },
                    // repr[i] = i32.load8_u(ptr + i)
                    WirInstr::ArraySet {
                        type_id: repr_array_type_id,
                        array: Box::new(WirInstr::LocalGet {
                            name: repr_name.clone(),
                        }),
                        index: Box::new(WirInstr::LocalGet {
                            name: idx_name.clone(),
                        }),
                        value: Box::new(WirInstr::I32Load8U {
                            offset: 0,
                            align: 0,
                            addr: Box::new(WirInstr::I32Add(
                                Box::new(WirInstr::LocalGet {
                                    name: ptr_name.clone(),
                                }),
                                Box::new(WirInstr::LocalGet {
                                    name: idx_name.clone(),
                                }),
                            )),
                        }),
                    },
                    // i += 1
                    WirInstr::LocalSet {
                        name: idx_name.clone(),
                        value: Box::new(WirInstr::I32Add(
                            Box::new(WirInstr::LocalGet {
                                name: idx_name.clone(),
                            }),
                            Box::new(WirInstr::I32Const(1)),
                        )),
                    },
                    // br $loop
                    WirInstr::Br { depth: 0 },
                ],
            }],
        });

        // Free linear memory: realloc(ptr, max, 1, 0)
        instrs.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet { name: ptr_name },
                WirInstr::LocalGet { name: max_name },
                WirInstr::I32Const(1),
                WirInstr::I32Const(0),
            ],
        })));

        // Create Array<u8> struct: { repr, used: count }
        instrs.push(self.struct_new(
            array_struct_type_id,
            vec![
                WirInstr::LocalGet { name: repr_name },
                WirInstr::LocalGet { name: count_name },
            ],
        ));

        WirInstr::Seq(instrs)
    }

    /// Emit WIR for `StreamWritable<u8>::write(data: Array<u8>)`.
    ///
    /// 1. Lower Array<u8> to linear memory via `cm_lower_array_u8`
    /// 2. Call `stream-write(tx, ptr, len)` → status
    /// 3. Handle BLOCKED via waitable-set
    /// 4. Free linear memory
    fn emit_stream_write(&mut self, handle: WirInstr, data_arg: WirInstr) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            return WirInstr::Unreachable;
        };
        let stream_write_id = self.ctx.ensure_canonical(
            "stream-write",
            vec![WirType::I32, WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        let Some(cm_lower_id) = self
            .ctx
            .func_map
            .get("core:internal/cm_lower_array_u8")
            .cloned()
        else {
            return WirInstr::Unreachable;
        };
        let ws_new_id = self
            .ctx
            .ensure_canonical("waitable-set-new", vec![], vec![WirType::I32]);
        let w_join_id =
            self.ctx
                .ensure_canonical("waitable-join", vec![WirType::I32, WirType::I32], vec![]);
        let ws_wait_id = self.ctx.ensure_canonical(
            "waitable-set-wait",
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let handle_name = format!("__sw_handle_{suffix}");
        let packed_name = format!("__sw_packed_{suffix}");
        let ptr_name = format!("__sw_ptr_{suffix}");
        let len_name = format!("__sw_len_{suffix}");
        let result_name = format!("__sw_result_{suffix}");
        let evt_name = format!("__sw_evt_{suffix}");

        let mut instrs = vec![];

        // Declare locals
        for (name, ty) in [
            (&handle_name, WirType::I32),
            (&ptr_name, WirType::I32),
            (&len_name, WirType::I32),
            (&result_name, WirType::I32),
        ] {
            instrs.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }
        instrs.push(WirInstr::DeclareLocal {
            name: packed_name.clone(),
            ty: WirType::I64,
        });

        // Save handle
        instrs.push(WirInstr::LocalSet {
            name: handle_name.clone(),
            value: Box::new(handle),
        });

        // packed = cm_lower_array_u8(data) → i64 (ptr | (len << 32))
        instrs.push(WirInstr::LocalSet {
            name: packed_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: cm_lower_id,
                args: vec![data_arg],
            }),
        });

        // ptr = packed as i32
        instrs.push(WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::I32WrapI64(Box::new(WirInstr::LocalGet {
                name: packed_name.clone(),
            }))),
        });
        // len = (packed >> 32) as i32
        instrs.push(WirInstr::LocalSet {
            name: len_name.clone(),
            value: Box::new(WirInstr::I32WrapI64(Box::new(WirInstr::I64ShrU(
                Box::new(WirInstr::LocalGet {
                    name: packed_name.clone(),
                }),
                Box::new(WirInstr::I64Const(32)),
            )))),
        });

        // result = stream_write(handle, ptr, len)
        instrs.push(WirInstr::LocalSet {
            name: result_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: stream_write_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                    },
                    WirInstr::LocalGet {
                        name: ptr_name.clone(),
                    },
                    WirInstr::LocalGet {
                        name: len_name.clone(),
                    },
                ],
            }),
        });

        // If result == BLOCKED (-1), wait via waitable-set
        instrs.push(WirInstr::If {
            condition: Box::new(WirInstr::I32Eq(
                Box::new(WirInstr::LocalGet {
                    name: result_name.clone(),
                }),
                Box::new(WirInstr::I32Const(-1)),
            )),
            result: None,
            then_body: vec![
                WirInstr::DeclareLocal {
                    name: evt_name.clone(),
                    ty: WirType::I32,
                },
                WirInstr::LocalSet {
                    name: result_name.clone(),
                    value: Box::new(WirInstr::Call {
                        func_id: ws_new_id,
                        args: vec![],
                    }),
                },
                WirInstr::Call {
                    func_id: w_join_id,
                    args: vec![
                        WirInstr::LocalGet {
                            name: handle_name.clone(),
                        },
                        WirInstr::LocalGet {
                            name: result_name.clone(),
                        },
                    ],
                },
                WirInstr::LocalSet {
                    name: evt_name.clone(),
                    value: Box::new(WirInstr::Call {
                        func_id: realloc_id.clone(),
                        args: vec![
                            WirInstr::I32Const(0),
                            WirInstr::I32Const(0),
                            WirInstr::I32Const(4),
                            WirInstr::I32Const(8),
                        ],
                    }),
                },
                WirInstr::Drop(Box::new(WirInstr::Call {
                    func_id: ws_wait_id,
                    args: vec![
                        WirInstr::LocalGet {
                            name: result_name.clone(),
                        },
                        WirInstr::LocalGet {
                            name: evt_name.clone(),
                        },
                    ],
                })),
                WirInstr::Drop(Box::new(WirInstr::Call {
                    func_id: realloc_id.clone(),
                    args: vec![
                        WirInstr::LocalGet {
                            name: evt_name.clone(),
                        },
                        WirInstr::I32Const(8),
                        WirInstr::I32Const(4),
                        WirInstr::I32Const(0),
                    ],
                })),
            ],
            else_body: None,
        });

        // Free linear memory: realloc(ptr, len, 1, 0)
        instrs.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet { name: ptr_name },
                WirInstr::LocalGet { name: len_name },
                WirInstr::I32Const(1),
                WirInstr::I32Const(0),
            ],
        })));

        WirInstr::Seq(instrs)
    }

    /// Translate array index read: `arr[i]`
    fn translate_index(&mut self, array_expr: &TirExpr, index_expr: &TirExpr) -> WirInstr {
        let arr = self.translate_expr(array_expr);
        let idx = self.translate_expr(index_expr);

        // Unwrap reference types
        let base_type_id = match self.type_table.get(array_expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => array_expr.type_id,
        };

        if let Some(element_type_id) = self.type_table.as_array(base_type_id) {
            self.build_array_get(arr, idx, base_type_id, element_type_id)
        } else {
            // Not an array type — fallback
            WirInstr::Unreachable
        }
    }

    /// Build an array.get instruction sequence.
    /// Given an Array<T> struct ref, extracts the repr field and does the appropriate get.
    fn build_array_get(
        &self,
        arr: WirInstr,
        idx: WirInstr,
        array_type_id: TypeId,
        element_type_id: TypeId,
    ) -> WirInstr {
        // Get the Array<T> struct WirType
        let array_struct_wir = self.ctx.type_id_to_wir_type(self.type_table, array_type_id);
        let WirType::Ref {
            type_id: array_struct_type,
            ..
        } = array_struct_wir
        else {
            return WirInstr::Unreachable;
        };

        // Get the raw GC array type
        let elem_name = self.type_table.mangle_type_name(element_type_id);
        let raw_array_type = self
            .ctx
            .array_type_by_name
            .get(&elem_name)
            .or_else(|| self.ctx.array_type_map.get(&element_type_id))
            .cloned();
        let Some(raw_type) = raw_array_type else {
            return WirInstr::Unreachable;
        };

        // StructGet field "repr" (field 0) to get raw array
        let raw_arr = WirInstr::StructGet {
            type_id: array_struct_type,
            field_name: "repr".to_string(),
            expr: Box::new(arr),
        };

        // Determine appropriate array get instruction based on element type
        let elem_resolved = self.type_table.get(element_type_id);
        let is_ref = matches!(
            elem_resolved,
            ResolvedType::GenericInstance { .. }
                | ResolvedType::Struct { .. }
                | ResolvedType::Function { .. }
                | ResolvedType::Ref(_)
                | ResolvedType::MutRef(_)
                | ResolvedType::Variant { .. }
        );

        let get_instr = if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::Bool)
        ) {
            WirInstr::ArrayGetU {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
            }
        } else if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::I16)
        ) {
            WirInstr::ArrayGetS {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
            }
        } else {
            WirInstr::ArrayGet {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
            }
        };

        // For reference element types, convert nullable to non-null
        if is_ref {
            WirInstr::RefAsNonNull(Box::new(get_instr))
        } else {
            get_instr
        }
    }

    /// Translate array index assignment: `arr[i] = val`
    fn translate_index_assign(
        &mut self,
        array_expr: &TirExpr,
        index_expr: &TirExpr,
        val: WirInstr,
    ) -> WirInstr {
        let arr = self.translate_expr(array_expr);
        let idx = self.translate_expr(index_expr);

        let base_type_id = match self.type_table.get(array_expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => array_expr.type_id,
        };

        if let Some(element_type_id) = self.type_table.as_array(base_type_id) {
            let array_struct_wir = self.ctx.type_id_to_wir_type(self.type_table, base_type_id);
            let WirType::Ref {
                type_id: array_struct_type,
                ..
            } = array_struct_wir
            else {
                return WirInstr::Drop(Box::new(val));
            };

            let elem_name = self.type_table.mangle_type_name(element_type_id);
            let raw_array_type = self
                .ctx
                .array_type_by_name
                .get(&elem_name)
                .or_else(|| self.ctx.array_type_map.get(&element_type_id))
                .cloned();
            let Some(raw_type) = raw_array_type else {
                return WirInstr::Drop(Box::new(val));
            };

            let raw_arr = WirInstr::StructGet {
                type_id: array_struct_type,
                field_name: "repr".to_string(),
                expr: Box::new(arr),
            };

            WirInstr::ArraySet {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                value: Box::new(val),
            }
        } else {
            WirInstr::Drop(Box::new(val))
        }
    }

    /// Translate array literal: `[1, 2, 3]`
    /// Translate map literal: `{ a: 1, b: 2 }` coerced to `TreeMap<String, V>`.
    ///
    /// Generates WIR equivalent to:
    ///   let mut __map = `TreeMap::`<String, `V>::new()`;
    ///   __map["a"] = 1;
    ///   __map["b"] = 2;
    ///   __map
    /// Translate switch expression using `br_table`.
    fn translate_switch(
        &mut self,
        scrutinee: &TirExpr,
        min_value: i64,
        arms: &[TirBlock],
        default: &TirBlock,
        result_type: TypeId,
    ) -> WirInstr {
        let has_result = result_type != TypeTable::UNIT && result_type != TypeTable::NEVER;
        let result_wir_type = if has_result {
            Some(self.ctx.type_id_to_wir_type(self.type_table, result_type))
        } else {
            None
        };

        // Translate scrutinee and adjust for min_value
        let scrut = self.translate_expr(scrutinee);
        let is_i64 = matches!(
            self.type_table.get(scrutinee.type_id),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );

        let adjusted = if min_value != 0 {
            if is_i64 {
                WirInstr::I32WrapI64(Box::new(WirInstr::I64Sub(
                    Box::new(scrut),
                    Box::new(WirInstr::I64Const(min_value)),
                )))
            } else {
                WirInstr::I32Sub(
                    Box::new(scrut),
                    Box::new(WirInstr::I32Const(min_value as i32)),
                )
            }
        } else if is_i64 {
            WirInstr::I32WrapI64(Box::new(scrut))
        } else {
            scrut
        };

        let num_arms = arms.len();

        // br_table targets: target[i] = i + 1 (depth to arm[i]'s wrapper block)
        // Block nesting (innermost to outermost): default, arm[0], arm[1], ..., arm[n-1], result
        // From br_table: depth 0 = default block, depth i+1 = arm[i]'s block
        let targets: Vec<u32> = (1..=num_arms as u32).collect();
        let default_target = 0u32; // Default block is innermost

        let br_table = WirInstr::BrTable {
            index: Box::new(adjusted),
            targets,
            default: default_target,
        };

        // Translate default body
        let default_body = if has_result {
            self.translate_stmts_as_value(&default.stmts)
        } else {
            self.translate_stmts(&default.stmts)
        };

        // Translate arm bodies
        let arm_bodies: Vec<Vec<WirInstr>> = arms
            .iter()
            .map(|arm| {
                if has_result {
                    self.translate_stmts_as_value(&arm.stmts)
                } else {
                    self.translate_stmts(&arm.stmts)
                }
            })
            .collect();

        // Build from innermost out:
        // block $default { br_table }; default_body; br N
        let mut current = vec![WirInstr::Block {
            label: None,
            result: None,
            body: vec![br_table],
        }];
        current.extend(default_body);
        if num_arms > 0 {
            current.push(WirInstr::Br {
                depth: num_arms as u32,
            });
        }

        // For each arm, wrap in a block
        for (i, arm_body) in arm_bodies.into_iter().enumerate() {
            let mut next = vec![WirInstr::Block {
                label: None,
                result: None,
                body: current,
            }];
            next.extend(arm_body);
            let remaining = num_arms - 1 - i;
            if remaining > 0 {
                next.push(WirInstr::Br {
                    depth: remaining as u32,
                });
            }
            current = next;
        }

        // Outer result block
        WirInstr::Block {
            label: None,
            result: result_wir_type,
            body: current,
        }
    }

    /// Translate a `LetPattern` (tuple destructuring) statement.
    /// Evaluates the tuple expression, stores it in a temp local,
    /// then binds each element to its pattern binding local.
    fn translate_let_pattern(&mut self, pattern: &TirPattern, value: &TirExpr) -> Option<WirInstr> {
        let value_instr = self.translate_expr(value);
        let value_instr = self.maybe_value_copy(value, value_instr);

        match pattern {
            TirPattern::Tuple(patterns) => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, value.type_id);
                if let WirType::Ref { ref type_id, .. } = wir_type {
                    let mut instrs = Vec::new();

                    // Declare and assign a temp local for the tuple
                    let temp_name = format!("__let_pattern_{}", self.match_counter);
                    self.match_counter += 1;
                    instrs.push(WirInstr::DeclareLocal {
                        name: temp_name.clone(),
                        ty: wir_type.clone(),
                    });
                    instrs.push(WirInstr::LocalSet {
                        name: temp_name.clone(),
                        value: Box::new(value_instr),
                    });

                    // Bind each element
                    for (i, sub_pattern) in patterns.iter().enumerate() {
                        if let TirPattern::Binding { local_index, .. } = sub_pattern {
                            let local_name = self.local_name(*local_index);
                            instrs.push(WirInstr::LocalSet {
                                name: local_name,
                                value: Box::new(WirInstr::StructGet {
                                    type_id: type_id.clone(),
                                    field_name: format!("{i}"),
                                    expr: Box::new(WirInstr::LocalGet {
                                        name: temp_name.clone(),
                                    }),
                                }),
                            });
                        }
                        // Wildcard patterns: skip (no binding)
                    }

                    Some(WirInstr::Seq(instrs))
                } else {
                    // Non-ref tuple type — shouldn't happen for real tuples
                    None
                }
            }
            TirPattern::Binding { local_index, .. } => {
                // Simple binding (not a tuple destructure)
                let local_name = self.local_name(*local_index);
                Some(WirInstr::LocalSet {
                    name: local_name,
                    value: Box::new(value_instr),
                })
            }
            _ => None,
        }
    }

    /// Translate match expression as nested if-else chain.
    fn translate_match(
        &mut self,
        scrutinee: &TirExpr,
        arms: &[TirMatchArm],
        result_type: TypeId,
    ) -> WirInstr {
        let has_result = result_type != TypeTable::UNIT && result_type != TypeTable::NEVER;
        let result_wir_type = if has_result {
            Some(self.ctx.type_id_to_wir_type(self.type_table, result_type))
        } else {
            None
        };

        // Store scrutinee in a local to avoid re-evaluation
        let scrut = self.translate_expr(scrutinee);
        let match_id = self.match_counter;
        self.match_counter += 1;
        let scrut_local_name = format!("__match_scrut_{match_id}");
        let scrut_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, scrutinee.type_id);

        // Build the if-else chain from inside out (last arm first)
        let mut result = WirInstr::Unreachable; // fallback: non-exhaustive

        for arm in arms.iter().rev() {
            let body_instrs = {
                let mut instrs = Vec::new();
                // Bind pattern variables
                self.emit_pattern_bindings(
                    &arm.pattern,
                    &scrut_local_name,
                    scrutinee.type_id,
                    &mut instrs,
                );
                let body = if has_result {
                    self.translate_expr_as_value(&arm.body)
                } else {
                    let instr = self.translate_expr(&arm.body);
                    // If the arm body produces a non-unit value (e.g. after inlining
                    // transforms a Block into a bare call), drop it to avoid leaving
                    // values on the Wasm stack.
                    if arm.body.type_id != TypeTable::UNIT && arm.body.type_id != TypeTable::NEVER {
                        WirInstr::Drop(Box::new(instr))
                    } else {
                        instr
                    }
                };
                instrs.push(body);
                // Note: `translate_expr` already appends `unreachable` for
                // `never`-typed arm bodies, so no extra push is needed here.
                instrs
            };

            let condition = self.translate_pattern_condition(
                &arm.pattern,
                &scrut_local_name,
                scrutinee.type_id,
            );

            // For irrefutable patterns (wildcard, binding, struct), just use the body
            let is_irrefutable = matches!(
                arm.pattern,
                TirPattern::Wildcard
                    | TirPattern::Binding { .. }
                    | TirPattern::Struct { .. }
                    | TirPattern::Tuple(_)
            );

            if is_irrefutable && arm.guard.is_none() {
                // This arm always matches — it becomes the fallback
                if body_instrs.len() == 1 {
                    result = body_instrs.into_iter().next().unwrap();
                } else {
                    result = WirInstr::Seq(body_instrs);
                }
            } else if let Some(guard) = &arm.guard {
                // Guard present: use nested if to avoid eager evaluation.
                // Outer if checks the pattern condition; inner if checks the guard.
                // This prevents pattern bindings (ref.cast etc.) from executing
                // when the pattern doesn't match.
                let mut inner_then = Vec::new();
                self.emit_pattern_bindings(
                    &arm.pattern,
                    &scrut_local_name,
                    scrutinee.type_id,
                    &mut inner_then,
                );
                let guard_expr = self.translate_expr(guard);
                // Inner if: check guard, run body or fall through to remaining arms
                let inner_if = WirInstr::If {
                    condition: Box::new(guard_expr),
                    result: result_wir_type.clone(),
                    then_body: body_instrs,
                    else_body: Some(vec![result.clone()]),
                };
                inner_then.push(inner_if);
                // Outer if: check pattern condition
                result = WirInstr::If {
                    condition: Box::new(condition),
                    result: result_wir_type.clone(),
                    then_body: inner_then,
                    else_body: Some(vec![result]),
                };
            } else {
                let then_body = body_instrs;
                let else_body = Some(vec![result]);
                result = WirInstr::If {
                    condition: Box::new(condition),
                    result: result_wir_type.clone(),
                    then_body,
                    else_body,
                };
            }
        }

        // Wrap everything: declare local, set, then the if-else chain
        WirInstr::Seq(vec![
            WirInstr::DeclareLocal {
                name: scrut_local_name.clone(),
                ty: scrut_wir_type,
            },
            WirInstr::LocalSet {
                name: scrut_local_name,
                value: Box::new(scrut),
            },
            result,
        ])
    }

    /// Generate a condition expression for a pattern.
    /// Returns an i32 (0 or 1) indicating whether the pattern matches.
    fn translate_pattern_condition(
        &self,
        pattern: &TirPattern,
        scrut_local: &str,
        scrut_type: TypeId,
    ) -> WirInstr {
        match pattern {
            TirPattern::Wildcard | TirPattern::Binding { .. } => {
                WirInstr::I32Const(1) // always matches
            }
            TirPattern::Literal(lit) => {
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                };
                self.translate_literal_pattern_condition(lit, scrut_get, scrut_type)
            }
            TirPattern::Enum { case_index, .. } => {
                // Enum: compare i32 discriminant
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                };
                WirInstr::I32Eq(
                    Box::new(scrut_get),
                    Box::new(WirInstr::I32Const(*case_index as i32)),
                )
            }
            TirPattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                // For variant patterns, use ref.test on the case type
                // or discriminant comparison for unit cases
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                };

                // Option is now handled as a regular variant (SubtypeHierarchy)
                // TODO: Future optimization: NullableRef Option would use RefIsNull here

                // Look up variant type info to find the case WirTypeId
                let (var_name, var_module) = match self.type_table.get(scrut_type) {
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    } => (name.clone(), module_source.clone()),
                    ResolvedType::GenericInstance {
                        name,
                        module_source,
                        type_args,
                        ..
                    } => {
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| self.type_table.mangle_type_name(*t))
                            .collect();
                        (
                            crate::name::mangle_generic_name(name, &type_arg_names),
                            module_source.clone(),
                        )
                    }
                    _ => return WirInstr::I32Const(0),
                };
                let fq = format!("{var_module}//{var_name}");

                // Look up variant case info
                if let Some(variant_type_id) = self.ctx.type_map.get(&fq) {
                    // Find the case by name
                    if let crate::wir::WirTypeDef::Variant(vt) =
                        &self.ctx.types[variant_type_id.index() as usize]
                    {
                        if let Some(case) = vt.cases.iter().find(|c| c.name == *variant_name) {
                            if case.payload.is_empty() && bindings.is_empty() {
                                // Unit variant: check discriminant
                                let wir_type =
                                    self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                                if let WirType::Ref { type_id, .. } = wir_type {
                                    WirInstr::I32Eq(
                                        Box::new(WirInstr::StructGet {
                                            type_id,
                                            field_name: "discriminant".to_string(),
                                            expr: Box::new(scrut_get),
                                        }),
                                        Box::new(WirInstr::I32Const(case.index as i32)),
                                    )
                                } else {
                                    WirInstr::I32Const(0)
                                }
                            } else {
                                // Payload variant: use ref.test on case subtype
                                let case_fq = format!("{fq}::{variant_name}");
                                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq) {
                                    WirInstr::RefTest {
                                        type_id: case_type_id.clone(),
                                        nullable: false,
                                        expr: Box::new(scrut_get),
                                    }
                                } else {
                                    WirInstr::I32Const(0)
                                }
                            }
                        } else {
                            WirInstr::I32Const(0)
                        }
                    } else {
                        WirInstr::I32Const(0)
                    }
                } else {
                    WirInstr::I32Const(0)
                }
            }
            TirPattern::Tuple(_) | TirPattern::Struct { .. } => {
                // Tuple/struct patterns: always irrefutable
                WirInstr::I32Const(1)
            }
        }
    }

    /// Generate a condition for a literal pattern.
    fn translate_literal_pattern_condition(
        &self,
        lit: &TirLiteralPattern,
        scrut_get: WirInstr,
        scrut_type: TypeId,
    ) -> WirInstr {
        let is_i64 = matches!(
            self.type_table.get(scrut_type),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        match lit {
            TirLiteralPattern::I128(val) => {
                if is_i64 {
                    WirInstr::I64Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I64Const(*val as i64)),
                    )
                } else {
                    WirInstr::I32Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I32Const(*val as i32)),
                    )
                }
            }
            TirLiteralPattern::U128(val) => {
                if is_i64 {
                    WirInstr::I64Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I64Const(*val as i64)),
                    )
                } else {
                    WirInstr::I32Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I32Const(*val as i32)),
                    )
                }
            }
            TirLiteralPattern::Bool(val) => WirInstr::I32Eq(
                Box::new(scrut_get),
                Box::new(WirInstr::I32Const(i32::from(*val))),
            ),
            TirLiteralPattern::Char(val) => WirInstr::I32Eq(
                Box::new(scrut_get),
                Box::new(WirInstr::I32Const(*val as i32)),
            ),
            TirLiteralPattern::String(_) | TirLiteralPattern::Null => {
                // String/null patterns: use ref.eq or ref.is_null
                if matches!(lit, TirLiteralPattern::Null) {
                    WirInstr::RefIsNull(Box::new(scrut_get))
                } else {
                    // String comparison: not yet handled, return false
                    WirInstr::I32Const(0)
                }
            }
        }
    }

    /// Emit pattern bindings (local.set for bound variables).
    fn emit_pattern_bindings(
        &mut self,
        pattern: &TirPattern,
        scrut_local: &str,
        scrut_type: TypeId,
        instrs: &mut Vec<WirInstr>,
    ) {
        match pattern {
            TirPattern::Binding { local_index, .. } => {
                instrs.push(WirInstr::LocalSet {
                    name: self.local_name(*local_index),
                    value: Box::new(WirInstr::LocalGet {
                        name: scrut_local.to_string(),
                    }),
                });
            }
            TirPattern::Variant {
                variant_name,
                bindings,
                enum_type,
                ..
            } => {
                if bindings.is_empty() {
                    return;
                }

                // Option is now handled as a regular variant (SubtypeHierarchy)
                // TODO: Future optimization: NullableRef Option would extract from nullable ref

                // Look up the variant type to find the case WirTypeId
                let (var_name, var_module) = match self.type_table.get(*enum_type) {
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    } => (name.clone(), module_source.clone()),
                    ResolvedType::GenericInstance {
                        name,
                        module_source,
                        type_args,
                        ..
                    } => {
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| self.type_table.mangle_type_name(*t))
                            .collect();
                        (
                            crate::name::mangle_generic_name(name, &type_arg_names),
                            module_source.clone(),
                        )
                    }
                    _ => return,
                };
                let fq = format!("{var_module}//{var_name}");
                let case_fq = format!("{fq}::{variant_name}");

                // Try to get the case type for ref.cast + struct.get
                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq).cloned() {
                    // Use a temp local to hold the cast result (avoids repeated ref.cast)
                    self.local_counter += 1;
                    let cast_local = format!("__cast_{}", self.local_counter);
                    instrs.push(WirInstr::DeclareLocal {
                        name: cast_local.clone(),
                        ty: WirType::Ref {
                            type_id: case_type_id.clone(),
                            nullable: false,
                        },
                    });
                    instrs.push(WirInstr::LocalSet {
                        name: cast_local.clone(),
                        value: Box::new(WirInstr::RefCast {
                            type_id: case_type_id.clone(),
                            nullable: false,
                            expr: Box::new(WirInstr::LocalGet {
                                name: scrut_local.to_string(),
                            }),
                        }),
                    });
                    // Extract each payload binding via struct.get from the cast local
                    for (i, binding) in bindings.iter().enumerate() {
                        let payload_get = WirInstr::StructGet {
                            type_id: case_type_id.clone(),
                            field_name: format!("payload_{i}"),
                            expr: Box::new(WirInstr::LocalGet {
                                name: cast_local.clone(),
                            }),
                        };
                        if let TirPattern::Binding {
                            local_index,
                            type_id,
                            ..
                        } = binding
                        {
                            // Check the local's actual type (which may have been
                            // promoted to Box<T> by the address-taken boxing pass)
                            // rather than the pattern binding's original type_id.
                            let local_type_id =
                                if (*local_index as usize) < self.tir_func.local_types.len() {
                                    self.tir_func.local_types[*local_index as usize]
                                } else {
                                    *type_id
                                };
                            let binding_wir =
                                self.ctx.type_id_to_wir_type(self.type_table, local_type_id);
                            let payload_field_wir =
                                self.get_case_payload_wir_type(&case_type_id, i);
                            let needs_boxing = matches!(binding_wir, WirType::Ref { .. })
                                && payload_field_wir.as_ref().is_some_and(|t| {
                                    !matches!(t, WirType::Ref { .. } | WirType::AbstractRef { .. })
                                });
                            let value = if needs_boxing {
                                if let WirType::Ref {
                                    type_id: box_tid, ..
                                } = &binding_wir
                                {
                                    WirInstr::StructNew {
                                        type_id: box_tid.clone(),
                                        fields: vec![payload_get],
                                    }
                                } else {
                                    payload_get
                                }
                            } else {
                                payload_get
                            };
                            instrs.push(WirInstr::LocalSet {
                                name: self.local_name(*local_index),
                                value: Box::new(value),
                            });
                        } else if let TirPattern::Tuple(sub_patterns) = binding {
                            // Tuple payload: payload_i is a ref to a tuple struct.
                            // Extract each tuple field into the corresponding local.
                            if let Some(tuple_type_id) =
                                self.get_case_payload_ref_type(&case_type_id, i)
                            {
                                self.local_counter += 1;
                                let tuple_local = format!("__tuple_payload_{}", self.local_counter);
                                instrs.push(WirInstr::DeclareLocal {
                                    name: tuple_local.clone(),
                                    ty: WirType::Ref {
                                        type_id: tuple_type_id.clone(),
                                        nullable: false,
                                    },
                                });
                                instrs.push(WirInstr::LocalSet {
                                    name: tuple_local.clone(),
                                    value: Box::new(payload_get),
                                });
                                for (j, sub) in sub_patterns.iter().enumerate() {
                                    if let TirPattern::Binding { local_index, .. } = sub {
                                        instrs.push(WirInstr::LocalSet {
                                            name: self.local_name(*local_index),
                                            value: Box::new(WirInstr::StructGet {
                                                type_id: tuple_type_id.clone(),
                                                field_name: format!("{j}"),
                                                expr: Box::new(WirInstr::LocalGet {
                                                    name: tuple_local.clone(),
                                                }),
                                            }),
                                        });
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Fallback: just copy the scrutinee (won't be type-correct for payload)
                    for binding in bindings {
                        if let TirPattern::Binding { local_index, .. } = binding {
                            instrs.push(WirInstr::LocalSet {
                                name: self.local_name(*local_index),
                                value: Box::new(WirInstr::LocalGet {
                                    name: scrut_local.to_string(),
                                }),
                            });
                        }
                    }
                }
            }
            TirPattern::Wildcard | TirPattern::Literal(_) | TirPattern::Enum { .. } => {
                // No bindings needed
            }
            TirPattern::Tuple(sub_patterns) => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                if let WirType::Ref { type_id, .. } = wir_type {
                    let element_types =
                        if let ResolvedType::Tuple(elements) = self.type_table.get(scrut_type) {
                            elements.clone()
                        } else {
                            vec![]
                        };
                    for (i, sub_pattern) in sub_patterns.iter().enumerate() {
                        let field_get = WirInstr::StructGet {
                            type_id: type_id.clone(),
                            field_name: format!("{i}"),
                            expr: Box::new(WirInstr::LocalGet {
                                name: scrut_local.to_string(),
                            }),
                        };
                        match sub_pattern {
                            TirPattern::Binding { local_index, .. } => {
                                instrs.push(WirInstr::LocalSet {
                                    name: self.local_name(*local_index),
                                    value: Box::new(field_get),
                                });
                            }
                            TirPattern::Wildcard => {}
                            _ => {
                                // Nested pattern: store in temp and recurse
                                self.local_counter += 1;
                                let temp_name = format!("__tuple_elem_{}", self.local_counter);
                                let elem_type =
                                    element_types.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                                let elem_wir_type =
                                    self.ctx.type_id_to_wir_type(self.type_table, elem_type);
                                instrs.push(WirInstr::DeclareLocal {
                                    name: temp_name.clone(),
                                    ty: elem_wir_type,
                                });
                                instrs.push(WirInstr::LocalSet {
                                    name: temp_name.clone(),
                                    value: Box::new(field_get),
                                });
                                self.emit_pattern_bindings(
                                    sub_pattern,
                                    &temp_name,
                                    elem_type,
                                    instrs,
                                );
                            }
                        }
                    }
                }
            }
            TirPattern::Struct { fields, .. } => {
                // Emit field bindings for struct patterns in match arms
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                if let WirType::Ref { type_id, .. } = wir_type {
                    for field in fields {
                        let field_get = WirInstr::StructGet {
                            type_id: type_id.clone(),
                            field_name: field.field_name.clone(),
                            expr: Box::new(WirInstr::LocalGet {
                                name: scrut_local.to_string(),
                            }),
                        };
                        match &field.pattern {
                            TirPattern::Binding { local_index, .. } => {
                                instrs.push(WirInstr::LocalSet {
                                    name: self.local_name(*local_index),
                                    value: Box::new(field_get),
                                });
                            }
                            TirPattern::Wildcard => {}
                            _ => {
                                // For nested patterns, store in a temp and recurse
                                self.local_counter += 1;
                                let temp_name = format!("__struct_field_{}", self.local_counter);
                                let field_type =
                                    self.resolve_struct_field_type(scrut_type, &field.field_name);
                                let field_wir_type =
                                    self.ctx.type_id_to_wir_type(self.type_table, field_type);
                                instrs.push(WirInstr::DeclareLocal {
                                    name: temp_name.clone(),
                                    ty: field_wir_type,
                                });
                                instrs.push(WirInstr::LocalSet {
                                    name: temp_name.clone(),
                                    value: Box::new(field_get),
                                });
                                self.emit_pattern_bindings(
                                    &field.pattern,
                                    &temp_name,
                                    field_type,
                                    instrs,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Look up a variant case struct's payload field type, extracting the inner
    /// ref type ID. For `payload_i` of a tuple type, this returns the `WirTypeId`
    /// of the tuple struct.
    fn get_case_payload_wir_type(
        &self,
        case_type_id: &crate::wir::WirTypeId,
        payload_index: usize,
    ) -> Option<WirType> {
        let type_def = self.ctx.types.get(case_type_id.index() as usize)?;
        if let crate::wir::WirTypeDef::Struct(s) = type_def {
            let field = s.fields.get(payload_index + 1)?;
            return Some(field.ty.clone());
        }
        None
    }

    fn get_case_payload_ref_type(
        &self,
        case_type_id: &crate::wir::WirTypeId,
        payload_index: usize,
    ) -> Option<crate::wir::WirTypeId> {
        let type_def = self.ctx.types.get(case_type_id.index() as usize)?;
        if let crate::wir::WirTypeDef::Struct(s) = type_def {
            // Fields are: [discriminant, payload_0, payload_1, ...]
            let field = s.fields.get(payload_index + 1)?;
            if let WirType::Ref { type_id, .. } = &field.ty {
                return Some(type_id.clone());
            }
        }
        None
    }

    /// Resolve the `TypeId` of a struct field by name.
    fn resolve_struct_field_type(&self, struct_type: TypeId, field_name: &str) -> TypeId {
        if let ResolvedType::Struct {
            name,
            module_source,
            ..
        } = self.type_table.get(struct_type)
        {
            for module in self.ctx.project.tir_modules.values() {
                if module.module_source == *module_source {
                    for s in &module.structs {
                        if s.name == *name {
                            for f in &s.fields {
                                if f.name == field_name {
                                    return f.type_id;
                                }
                            }
                        }
                    }
                }
            }
        }
        TypeTable::UNKNOWN
    }

    /// Translate variant construction: `Shape::Circle(5.0)`
    fn translate_variant_construct(
        &mut self,
        variant_type: TypeId,
        case_index: u32,
        case_name: &str,
        payload: Option<&TirExpr>,
        result_type: TypeId,
    ) -> WirInstr {
        // Get the variant name and module source
        let (variant_name, variant_module_source) = match self.type_table.get(variant_type) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_name(*t))
                    .collect();
                (
                    crate::name::mangle_generic_name(name, &type_arg_names),
                    module_source.clone(),
                )
            }
            _ => return WirInstr::Unreachable,
        };

        // Option is now handled as a regular variant (SubtypeHierarchy)
        // TODO: Future optimization: NullableRef Option would pass-through/null here

        // Look up case-specific struct type
        let fq = format!("{variant_module_source}//{variant_name}");
        let case_fq = format!("{fq}::{case_name}");

        if let Some(case_type_id) = self.ctx.type_map.get(&case_fq).cloned() {
            // Build struct.new for the case type: (tag, payload?)
            let mut fields = vec![WirInstr::I32Const(case_index as i32)];
            if let Some(payload_expr) = payload {
                fields.push(self.translate_expr(payload_expr));
            }
            self.struct_new(case_type_id, fields)
        } else {
            // Fallback: try the base variant type
            let wir_type = self.ctx.type_id_to_wir_type(self.type_table, result_type);
            if let WirType::Ref { type_id, .. } = wir_type {
                let mut fields = vec![WirInstr::I32Const(case_index as i32)];
                if let Some(payload_expr) = payload {
                    fields.push(self.translate_expr(payload_expr));
                }
                self.struct_new(type_id, fields)
            } else {
                WirInstr::Unreachable
            }
        }
    }

    /// Translate variant test: check if variant is of a specific case.
    fn translate_variant_test(&mut self, inner: &TirExpr, case_index: u32) -> WirInstr {
        let val = self.translate_expr(inner);

        // Look up variant type info
        let (var_name, var_module) = match self.type_table.get(inner.type_id) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_name(*t))
                    .collect();
                (
                    crate::name::mangle_generic_name(name, &type_arg_names),
                    module_source.clone(),
                )
            }
            _ => {
                // Non-variant: compare discriminant directly
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    return WirInstr::I32Eq(
                        Box::new(WirInstr::StructGet {
                            type_id,
                            field_name: "discriminant".to_string(),
                            expr: Box::new(val),
                        }),
                        Box::new(WirInstr::I32Const(case_index as i32)),
                    );
                }
                return WirInstr::I32Const(0);
            }
        };

        let fq = format!("{var_module}//{var_name}");

        // Check if this case has a payload
        if let Some(variant_type_id) = self.ctx.type_map.get(&fq)
            && let crate::wir::WirTypeDef::Variant(vt) =
                &self.ctx.types[variant_type_id.index() as usize]
            && let Some(case) = vt.cases.get(case_index as usize)
        {
            if case.payload.is_empty() {
                // Unit variant: check discriminant
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    return WirInstr::I32Eq(
                        Box::new(WirInstr::StructGet {
                            type_id,
                            field_name: "discriminant".to_string(),
                            expr: Box::new(val),
                        }),
                        Box::new(WirInstr::I32Const(case_index as i32)),
                    );
                }
            } else {
                // Payload variant: use ref.test
                let case_fq = format!("{fq}::{}", case.name);
                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq) {
                    return WirInstr::RefTest {
                        type_id: case_type_id.clone(),
                        nullable: false,
                        expr: Box::new(val),
                    };
                }
            }
        }

        // Fallback: compare discriminant
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
        if let WirType::Ref { type_id, .. } = wir_type {
            WirInstr::I32Eq(
                Box::new(WirInstr::StructGet {
                    type_id,
                    field_name: "discriminant".to_string(),
                    expr: Box::new(val),
                }),
                Box::new(WirInstr::I32Const(case_index as i32)),
            )
        } else {
            WirInstr::I32Const(0)
        }
    }

    /// Translate variant payload extraction.
    fn translate_variant_payload(&mut self, inner: &TirExpr, case_index: u32) -> WirInstr {
        let val = self.translate_expr(inner);

        // Look up variant type info
        let (var_name, var_module) = match self.type_table.get(inner.type_id) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_name(*t))
                    .collect();
                (
                    crate::name::mangle_generic_name(name, &type_arg_names),
                    module_source.clone(),
                )
            }
            _ => return val,
        };

        let fq = format!("{var_module}//{var_name}");

        // Look up case info to get the case type
        if let Some(variant_type_id) = self.ctx.type_map.get(&fq)
            && let crate::wir::WirTypeDef::Variant(vt) =
                &self.ctx.types[variant_type_id.index() as usize]
            && let Some(case) = vt.cases.get(case_index as usize)
        {
            let case_fq = format!("{fq}::{}", case.name);
            if let Some(case_type_id) = self.ctx.type_map.get(&case_fq) {
                // ref.cast to case type, then struct.get field 1 (payload)
                let cast = WirInstr::RefCast {
                    type_id: case_type_id.clone(),
                    nullable: false,
                    expr: Box::new(val),
                };
                return WirInstr::StructGet {
                    type_id: case_type_id.clone(),
                    field_name: "payload_0".to_string(),
                    expr: Box::new(cast),
                };
            }
        }

        // Fallback
        val
    }

    /// Translate `IndirectCall { callee, args }` to `call_ref` through canonical closure.
    fn translate_indirect_call(
        &mut self,
        callee: &TirExpr,
        args: &[TirExpr],
        result_type: TypeId,
    ) -> WirInstr {
        let callee_wir = self.translate_expr(callee);

        // Look up the Function type to get param/result info
        let fn_type = self.type_table.get(callee.type_id);
        let (param_types, return_type) = match fn_type {
            crate::tir::ResolvedType::Function {
                params,
                return_type,
                ..
            } => (params.clone(), *return_type),
            _ => return WirInstr::Unreachable,
        };

        // Compute canonical closure types directly by signature key
        let param_wirs: Vec<WirType> = param_types
            .iter()
            .map(|p| self.ctx.type_id_to_wir_type(self.type_table, *p))
            .collect();
        let result_wirs: Vec<WirType> =
            if return_type == TypeTable::UNIT || return_type == TypeTable::NEVER {
                vec![]
            } else {
                vec![self.ctx.type_id_to_wir_type(self.type_table, return_type)]
            };
        let key = WirContext::canonical_closure_key(&param_wirs, &result_wirs);
        let (fn_type_id, closure_struct_type_id) =
            if let Some((ftid, stid)) = self.ctx.canonical_closure_types.get(&key) {
                (ftid.clone(), stid.clone())
            } else {
                return WirInstr::Unreachable;
            };

        // Generate a temp local for the callee as canonical closure struct ref
        let temp_name = format!("__indirect_call_{}", self.local_counter);
        self.local_counter += 1;
        let callee_ref_type = WirType::Ref {
            type_id: closure_struct_type_id.clone(),
            nullable: false,
        };

        // Build: declare temp, cast callee from abstract structref to canonical closure,
        // store, extract env + args + funcref, call_ref
        let mut stmts = vec![
            WirInstr::DeclareLocal {
                name: temp_name.clone(),
                ty: callee_ref_type,
            },
            WirInstr::LocalSet {
                name: temp_name.clone(),
                value: Box::new(WirInstr::RefCast {
                    type_id: closure_struct_type_id.clone(),
                    nullable: false,
                    expr: Box::new(callee_wir),
                }),
            },
        ];

        // Build args: env, then user args
        let env_arg = WirInstr::StructGet {
            type_id: closure_struct_type_id.clone(),
            field_name: "env".to_string(),
            expr: Box::new(WirInstr::LocalGet {
                name: temp_name.clone(),
            }),
        };

        let mut call_args = vec![env_arg];
        for arg in args {
            let translated = self.translate_expr(arg);
            call_args.push(self.maybe_value_copy(arg, translated));
        }

        // func_ref = struct.get $closure "func"
        let func_ref = WirInstr::StructGet {
            type_id: closure_struct_type_id,
            field_name: "func".to_string(),
            expr: Box::new(WirInstr::LocalGet { name: temp_name }),
        };

        let call_ref = WirInstr::CallRef {
            type_id: fn_type_id,
            func_ref: Box::new(func_ref),
            args: call_args,
        };

        if result_type == TypeTable::UNIT || result_type == TypeTable::NEVER {
            stmts.push(call_ref);
            WirInstr::Seq(stmts)
        } else {
            // Need to return the call result as the block value
            stmts.push(call_ref);
            let result_wir = self.ctx.type_id_to_wir_type(self.type_table, result_type);
            WirInstr::Block {
                label: None,
                result: Some(result_wir),
                body: stmts,
            }
        }
    }

    /// Translate `ClosureToCanonical` — convert a functor struct to canonical closure.
    fn translate_closure_to_canonical(
        &mut self,
        functor: &TirExpr,
        functor_id: u32,
        target_fn_type: TypeId,
    ) -> WirInstr {
        let functor_instr = self.translate_expr(functor);

        // Look up the canonical closure struct type for the target function type
        let fn_resolved = self.type_table.get(target_fn_type);
        let (param_types, return_type) = match fn_resolved {
            crate::tir::ResolvedType::Function {
                params,
                return_type,
                ..
            } => (params.clone(), *return_type),
            _ => return WirInstr::Unreachable,
        };

        let param_wirs: Vec<WirType> = param_types
            .iter()
            .map(|p| self.ctx.type_id_to_wir_type(self.type_table, *p))
            .collect();
        let result_wirs: Vec<WirType> =
            if return_type == TypeTable::UNIT || return_type == TypeTable::NEVER {
                vec![]
            } else {
                vec![self.ctx.type_id_to_wir_type(self.type_table, return_type)]
            };

        // Get canonical closure type
        let key = WirContext::canonical_closure_key(&param_wirs, &result_wirs);
        let struct_type_id = if let Some((_, stid)) = self.ctx.canonical_closure_types.get(&key) {
            stid.clone()
        } else {
            return WirInstr::Unreachable;
        };

        // Look up the pre-registered wrapper function for this functor
        let wrapper_func_id = if let Some(id) = self.ctx.closure_wrapper_funcs.get(&functor_id) {
            id.clone()
        } else {
            return WirInstr::Unreachable;
        };

        // Build: CanonicalClosure { env: functor_as_structref, func: ref.func $wrapper }
        self.struct_new(
            struct_type_id,
            vec![
                functor_instr,
                WirInstr::RefFunc {
                    func_id: wrapper_func_id,
                },
            ],
        )
    }
}
