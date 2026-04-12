//! Function body translation — converts TIR expressions and statements to WIR instructions.
//!
//! This is the core of the `tir_to_wir` phase, translating each TIR function body
//! into a sequence of WIR instructions.

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    CallArg, FunctionRef, PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind,
    TirFunction, TirLiteralPattern, TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::wir::{
    CanonicalIntrinsic, CmFuturePayload, CmScalarType, CmStreamPayload, WirFuncId, WirInstr,
    WirName, WirType, WirTypeDef, WirTypeId,
};

use super::context::WirContext;

/// Classification of a TIR primitive type by the Wasm numeric type family
/// it is represented as, together with signedness for integer types.
///
/// Used by binary / unary op dispatch to pick the correct WIR instruction
/// (e.g., `I32Add` vs `I64Add`, `I32DivU` vs `I32DivS`) without repeatedly
/// matching on individual `PrimitiveType` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimitiveKind {
    /// `i8`, `i16`, `i32` — represented as Wasm `i32`, signed.
    I32Signed,
    /// `u8`, `u16`, `u32`, `bool`, `char` — represented as Wasm `i32`, unsigned.
    I32Unsigned,
    /// `i64` — represented as Wasm `i64`, signed.
    I64Signed,
    /// `u64` — represented as Wasm `i64`, unsigned.
    I64Unsigned,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
    /// Anything else — reference types or primitives not handled by
    /// scalar binop/unop dispatch (e.g., `i128`, `u128`, `v128`).
    Other,
}

impl PrimitiveKind {
    /// Classify a `TypeId`'s underlying primitive.
    fn from_type_id(type_table: &TypeTable, type_id: TypeId) -> Self {
        match type_table.get(type_id) {
            ResolvedType::Primitive(p) => Self::from_primitive(*p),
            _ => Self::Other,
        }
    }

    /// Classify a `PrimitiveType` value.
    fn from_primitive(p: PrimitiveType) -> Self {
        match p {
            PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 => Self::I32Signed,
            PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::Bool
            | PrimitiveType::Char => Self::I32Unsigned,
            PrimitiveType::I64 => Self::I64Signed,
            PrimitiveType::U64 => Self::I64Unsigned,
            PrimitiveType::F32 => Self::F32,
            PrimitiveType::F64 => Self::F64,
            PrimitiveType::I128 | PrimitiveType::U128 | PrimitiveType::V128 => Self::Other,
        }
    }

    /// True for `I64Signed` / `I64Unsigned`.
    fn is_i64(self) -> bool {
        matches!(self, Self::I64Signed | Self::I64Unsigned)
    }

    /// True for unsigned integer families (`I32Unsigned`, `I64Unsigned`).
    fn is_unsigned(self) -> bool {
        matches!(self, Self::I32Unsigned | Self::I64Unsigned)
    }
}

/// Helper macro for unary f64 builtins.
macro_rules! unary_f64 {
    ($self:expr, $args:expr, $variant:path) => {{
        let o = $self.translate_expr(&$args[0].expr);
        Some($variant(Box::new(o)))
    }};
}

/// Helper macro for binary f64 builtins.
macro_rules! binary_f64 {
    ($self:expr, $args:expr, $variant:path) => {{
        let l = $self.translate_expr(&$args[0].expr);
        let r = $self.translate_expr(&$args[1].expr);
        Some($variant(Box::new(l), Box::new(r)))
    }};
}

/// Helper macro for unary f32 builtins.
macro_rules! unary_f32 {
    ($self:expr, $args:expr, $variant:path) => {{
        let o = $self.translate_expr(&$args[0].expr);
        Some($variant(Box::new(o)))
    }};
}

/// Helper macro for binary f32 builtins.
macro_rules! binary_f32 {
    ($self:expr, $args:expr, $variant:path) => {{
        let l = $self.translate_expr(&$args[0].expr);
        let r = $self.translate_expr(&$args[1].expr);
        Some($variant(Box::new(l), Box::new(r)))
    }};
}

/// Extract a compile-time constant i32 from a TIR expression (for SIMD lane indices).
fn extract_i32_const(expr: &TirExpr) -> u8 {
    match &expr.kind {
        TirExprKind::IntLiteral { value, .. } => *value as u8,
        _ => panic!("SIMD lane index must be a constant integer literal"),
    }
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
            TirStmtKind::IfLet {
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

    let type_table = &*ctx.package.type_table.borrow();
    for functor in &ctx.package.closure_functors {
        let module_source = &functor.module_source;
        let functor_key = (module_source.clone(), functor.id);
        if ctx.closure_wrapper_funcs.contains_key(&functor_key) {
            continue;
        }

        // Look up the __call func_id, scoped to the correct module.
        // If __call was removed by DCE (closure never used), skip this functor entirely.
        // This check must come before type lookups since DCE may have removed the
        // functor's types from the TypeTable.
        let functor_name = &functor.struct_name;
        let call_method_fq = format!("{module_source}/{functor_name}::__call");
        let call_func_id = match ctx.func_map.get(&call_method_fq).cloned() {
            Some(id) => id,
            None => continue,
        };

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
        let user_params_clone = user_params.clone();
        let (fn_type_id, _) = ctx.get_or_create_canonical_closure_type(user_params, result_wirs);

        // Get functor struct type ID
        let functor_wir_type = ctx.type_id_to_wir_type(type_table, functor.ref_type_id);
        let functor_struct_type_id = match &functor_wir_type {
            WirType::Ref { type_id, .. } => type_id.clone(),
            _ => continue,
        };

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
                        result_ty: WirType::AbstractRef {
                            heap_type: crate::wir::WirAbstractHeapType::Struct,
                            nullable: true,
                        },
                    }),
                }),
            },
        ];

        let mut call_args = vec![WirInstr::LocalGet {
            name: typed_env_local,
            result_ty: functor_wir_type.clone(),
        }];
        for i in 0..user_param_count {
            call_args.push(WirInstr::LocalGet {
                name: format!("__p{i}"),
                result_ty: user_params_clone[i].clone(),
            });
        }

        let call_instr = WirInstr::Call {
            func_id: call_func_id,
            args: call_args,
        };
        if has_result {
            body.push(WirInstr::Return {
                value: Some(Box::new(call_instr)),
            });
        } else {
            body.push(call_instr);
        }

        let mut param_names = vec![env_local];
        for i in 0..user_param_count {
            param_names.push(format!("__p{i}"));
        }

        // Include module source in wrapper name for debuggability
        let global_id = ctx.closure_wrapper_funcs.len();
        let wrapper_name = format!("{module_source}/__closure_wrapper_{global_id}");
        let wrapper_fq = format!("closure/{wrapper_name}");

        let func = WirFunction {
            name: WirName { fq: wrapper_fq },
            type_id: fn_type_id,
            param_names,
            body: Some(body),
            meta: crate::wir::WirMeta::default(),
            generic_origin: None,
            effects: Vec::new(),
            stores: Vec::new(),
            comp_features: 0,
            export_name: None,
        };

        let func_id = ctx.register_function(func);
        ctx.closure_wrapper_funcs.insert(functor_key, func_id);
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
            let mut local_names = IndexMap::default();
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
                    immutable_locals: IndexSet::default(),
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

    /// Build a `LocalGet` with the WIR type resolved from a TIR local index.
    fn local_get(&self, index: u32) -> WirInstr {
        let name = self.local_name(index);
        let result_ty = self.local_wir_type(index);
        WirInstr::LocalGet { name, result_ty }
    }

    /// Resolve the WIR type of a TIR local variable by index.
    fn local_wir_type(&self, index: u32) -> WirType {
        let param_count = self.tir_func.params.len();
        if (index as usize) < param_count {
            let type_id = self.tir_func.params[index as usize].type_id;
            self.wir_type(type_id)
        } else if !self.tir_func.local_types.is_empty() {
            // local_types is indexed absolutely (entries 0..param_count are params,
            // entries param_count.. are non-param locals), matching DeclareLocal generation.
            if let Some(&type_id) = self.tir_func.local_types.get(index as usize) {
                self.wir_type(type_id)
            } else {
                WirType::I32
            }
        } else {
            WirType::I32
        }
    }

    /// Shorthand for `self.ctx.type_id_to_wir_type(self.type_table, type_id)`.
    fn wir_type(&self, type_id: TypeId) -> WirType {
        self.ctx.type_id_to_wir_type(self.type_table, type_id)
    }

    /// Look up the WIR type of a struct field.
    fn struct_field_wir_type(&self, struct_type_id: &WirTypeId, field_name: &str) -> WirType {
        if let Some(WirTypeDef::Struct(st)) = self.ctx.types.get(struct_type_id.index() as usize)
            && let Some(f) = st.fields.iter().find(|f| f.name == field_name)
        {
            return f.ty.clone();
        }
        WirType::I32
    }

    /// Look up the element WIR type of an array type.
    fn array_element_wir_type(&self, array_type_id: &WirTypeId) -> WirType {
        if let Some(WirTypeDef::Array(at)) = self.ctx.types.get(array_type_id.index() as usize) {
            return at.element_type.clone();
        }
        WirType::I32
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
            ResolvedType::Struct { base_name, .. } => {
                // Box<T> types are GC reference cells for primitive boxing.
                // They should share the heap object on assignment, not deep-copy.
                // Identified by base_name set during monomorphization / boxing pass.
                base_name.as_deref() != Some("Box")
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // Box<T> types are GC reference cells for primitive boxing.
                if name == "Box" {
                    return false;
                }
                // Empty tuples (unit-like) don't need value copy.
                if TypeTable::is_tuple_type(name, module_source) && type_args.is_empty() {
                    return false;
                }
                true
            }
            ResolvedType::Variant { .. } => true,
            _ => false,
        }
    }

    /// Check if an expression is "fresh" (doesn't need value copy).
    /// Fresh values are newly created and don't alias existing data,
    /// so they can be used directly without copying.
    fn is_fresh_value(expr: &TirExpr) -> bool {
        Self::is_fresh_in_context(expr, &IndexSet::default())
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
            | TirExprKind::TupleSpread { .. }
            | TirExprKind::TupleZip { .. }
            | TirExprKind::TypePackExpansion { .. }
            | TirExprKind::Null => true,

            // All call variants return fresh values (callee constructs the return value)
            TirExprKind::Call { .. }
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

            // Field access on a fresh struct — the struct is unaliased,
            // so the extracted field is not shared.
            TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::is_fresh_in_context(inner, fresh_locals)
            }

            // Variant payload extraction from a fresh variant — the variant
            // is unaliased, so the payload is not shared.
            TirExprKind::VariantPayload { expr: inner, .. } => {
                Self::is_fresh_in_context(inner, fresh_locals)
            }

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
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => self.is_source_immutable(inner),
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
            type_id: wir_tid,
            nullable,
        } = wir_type
        {
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
                TirStmtKind::IfLet {
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
    /// Also handles statement-level If/IfLet as value-producing when they're the
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
                    // For UNIT-typed expressions, all paths exit via break/return,
                    // so the fall-through is dead code — mark it explicitly so the
                    // Wasm validator knows the enclosing value-block's `end` is
                    // unreachable (void intermediate blocks don't push the expected
                    // typed result to the outer block's type stack).
                    if expr.type_id == TypeTable::UNIT {
                        instrs.push(WirInstr::Unreachable);
                    }
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
                // Statement-level IfLet with else can produce a value
                if let TirStmtKind::IfLet {
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
            // A Loop that always exits via a labeled `break` to an outer block never
            // falls through in Wado, but the Wasm `loop` instruction itself can fall
            // through. Add `unreachable` so the Wasm validator knows the fallthrough
            // path of the enclosing value-block is dead.
            if is_last && matches!(stmt.kind, TirStmtKind::Loop { .. }) {
                instrs.push(WirInstr::Unreachable);
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
            TirStmtKind::IfLet {
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
            TirStmtKind::IfLet {
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
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.translate_let_pattern(pattern, value)
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
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
            TirExprKind::IntLiteral { value, .. } => match self.type_table.get(expr.type_id) {
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
                    WirInstr::I64Const(*value as i64)
                }
                _ => WirInstr::I32Const(*value as i32),
            },
            TirExprKind::FloatLiteral { value, .. } => match self.type_table.get(expr.type_id) {
                ResolvedType::Primitive(PrimitiveType::F32) => WirInstr::F32Const(*value as f32),
                _ => WirInstr::F64Const(*value),
            },
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
                // For Option types, construct a None variant struct.
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
                    // Used by CM bindings for local initialization before conditional assignment.
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
                    self.local_get(*index)
                }
            }
            TirExprKind::FuncRef {
                module_source,
                name,
            } => {
                // FuncRef should have been converted to a Closure by the closure lowering pass.
                // If we reach here, it means a FuncRef survived lowering (e.g., external function
                // not in the module's func_sigs). This is a compiler bug.
                panic!(
                    "FuncRef '{module_source}::{name}' was not converted to a Closure during lowering"
                );
            }
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                let global_name = self.make_global_name(module_source, name);
                WirInstr::GlobalGet {
                    name: WirName { fq: global_name },
                    result_ty: self.wir_type(expr.type_id),
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
                    name: WirName { fq: global_name },
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
                let result = self.translate_binary_op(op, l, r, left.type_id);
                // Truncate sub-i32 arithmetic/bitwise results to the correct width.
                // Comparisons and logical ops return bool (i32 0/1), so skip those.
                if !matches!(
                    op,
                    TirBinaryOp::Eq
                        | TirBinaryOp::NotEq
                        | TirBinaryOp::Lt
                        | TirBinaryOp::LtEq
                        | TirBinaryOp::Gt
                        | TirBinaryOp::GtEq
                        | TirBinaryOp::And
                        | TirBinaryOp::Or
                        | TirBinaryOp::RefEq
                        | TirBinaryOp::RefNotEq
                ) && let ResolvedType::Primitive(prim) = self.type_table.get(left.type_id)
                {
                    return Self::truncate_to_sub_i32(result, prim);
                }
                result
            }

            // === Unary Operations ===
            TirExprKind::Unary { op, expr: inner } => match op {
                TirUnaryOp::Ref | TirUnaryOp::MutRef => self.translate_expr(inner),
                TirUnaryOp::Deref => self.translate_expr(inner),
                _ => {
                    let o = Box::new(self.translate_expr(inner));
                    let result = self.translate_unary_op(op, o, inner.type_id);
                    // Truncate sub-i32 results for Neg and BitNot.
                    if matches!(op, TirUnaryOp::Neg | TirUnaryOp::BitNot)
                        && let ResolvedType::Primitive(prim) = self.type_table.get(inner.type_id)
                    {
                        return Self::truncate_to_sub_i32(result, prim);
                    }
                    result
                }
            },

            // === Function Calls ===
            TirExprKind::Call { func, args, .. } => {
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

                // Static method: canonical dispatch (e.g., Stream::new, WaitableSet::new)
                if let Some(canonical) = func.method_info.clone().and_then(|m| m.cm_name)
                    && let Some(instr) = self.try_translate_canonical_static_method(
                        &canonical,
                        func,
                        args,
                        expr.type_id,
                    )
                {
                    return instr;
                }

                let translated_args: Vec<WirInstr> = args
                    .iter()
                    .filter(|a| a.expr.type_id != TypeTable::UNIT)
                    .map(|a| {
                        let translated = self.translate_expr(&a.expr);
                        self.maybe_value_copy_if_mut(&a.expr, translated, a.is_mut)
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
                for arg in args {
                    if arg.expr.type_id != TypeTable::UNIT {
                        let translated = self.translate_expr(&arg.expr);
                        translated_args
                            .push(self.maybe_value_copy_if_mut(&arg.expr, translated, arg.is_mut));
                    }
                }

                if let Some(func_id) = self.resolve_function_ref(func) {
                    WirInstr::Call {
                        func_id,
                        args: translated_args,
                    }
                } else if let Some(mi) = func.method_info.clone() {
                    panic!(
                        "[WIR] unresolved MethodCall: name={:?} method_info={:?}",
                        func.name.clone(),
                        mi
                    );
                } else {
                    panic!("[WIR] unresolved MethodCall: name={:?}", func.name.clone());
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
                    let result_ty = self.struct_field_wir_type(&type_id, field_name);
                    WirInstr::StructGet {
                        type_id,
                        field_name: field_name.clone(),
                        expr: Box::new(recv),
                        result_ty,
                    }
                } else {
                    WirInstr::Unreachable
                }
            }

            // === Assignment ===
            TirExprKind::Assign { target, value } => {
                let val = self.translate_expr(value);
                match &target.kind {
                    TirExprKind::Local { index, name } => {
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
                        // SROA rewrites struct-field writes (`s.field = v`) to local
                        // writes (`__sroa_s_field = v`). The original StructSet stores
                        // the GC reference as-is without a defensive copy, so the
                        // rewritten local write must also skip value_copy to preserve
                        // the same reference-sharing semantics.
                        let val = if name.starts_with("__sroa_") {
                            val
                        } else {
                            self.maybe_value_copy(value, val)
                        };
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
                let wir_type_id = match &wir_type {
                    WirType::Ref { type_id, .. } => Some(type_id.clone()),
                    _ if elements.len() >= 2 => {
                        // Tuple types created in CM binding synthesis may have
                        // TypeIds from a different module's type_table, causing
                        // type_id_to_wir_type to return I32 or AbstractRef instead
                        // of Ref. Fall back to matching by element WIR types.
                        self.ctx
                            .find_tuple_type_for_elements(self.type_table, elements)
                            .or_else(|| {
                                self.ctx
                                    .define_tuple_struct_for_elements(self.type_table, elements)
                            })
                    }
                    _ => None,
                };
                if let Some(type_id) = wir_type_id {
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

            TirExprKind::TupleSpread { .. }
            | TirExprKind::TupleZip { .. }
            | TirExprKind::TypePackExpansion { .. } => {
                panic!(
                    "TupleSpread/TupleZip/TypePackExpansion should have been expanded during monomorphization"
                )
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
                        result_ty: WirType::I32,
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
                // Look up in WASI imports (registered by register_imports from TIR imports)
                let func_id = if let Some(func_id) =
                    self.ctx.func_map.get(&format!("wasi/{local_name}"))
                {
                    func_id.clone()
                } else {
                    // Not pre-registered — lazily register as a canonical intrinsic.
                    // This handles canonical imports (e.g., "task-return") that may not
                    // be in TIR imports but are needed by CM binding synthesis.
                    let params: Vec<WirType> = args
                        .iter()
                        .map(|a| self.ctx.type_id_to_wir_type(self.type_table, a.type_id))
                        .collect();
                    let results =
                        if expr.type_id == TypeTable::UNIT || expr.type_id == TypeTable::NEVER {
                            vec![]
                        } else {
                            vec![self.ctx.type_id_to_wir_type(self.type_table, expr.type_id)]
                        };
                    let intrinsic = CanonicalIntrinsic::from_import_name(local_name)
                        .unwrap_or_else(|| panic!("unknown canonical intrinsic: {local_name}"));
                    // Future-related canonicals with default payload from from_import_name
                    // are NOT registered here. They must be registered via CM method dispatch
                    // with the correct CmFuturePayload. If a builtin calls future-drop-readable
                    // etc., the func_map entry from import registration is used directly.
                    if intrinsic.future_payload().is_some() {
                        // Look up the pre-registered import function
                        self.ctx
                            .func_map
                            .get(&format!("wasi/{local_name}"))
                            .cloned()
                            .unwrap_or_else(|| {
                                // No pre-registered import; fall back to ensure_canonical
                                self.ctx.ensure_canonical(intrinsic, params, results)
                            })
                    } else {
                        self.ctx.ensure_canonical(intrinsic, params, results)
                    }
                };
                WirInstr::Call {
                    func_id,
                    args: translated_args,
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
                closure_module,
            } => self.translate_closure_to_canonical(
                functor,
                *functor_id,
                *target_fn_type,
                closure_module,
            ),

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
        let kind = PrimitiveKind::from_type_id(self.type_table, left_type_id);

        match op {
            TirBinaryOp::Add => match kind {
                PrimitiveKind::F64 => WirInstr::F64Add(left, right),
                PrimitiveKind::F32 => WirInstr::F32Add(left, right),
                k if k.is_i64() => WirInstr::I64Add(left, right),
                _ => WirInstr::I32Add(left, right),
            },
            TirBinaryOp::Sub => match kind {
                PrimitiveKind::F64 => WirInstr::F64Sub(left, right),
                PrimitiveKind::F32 => WirInstr::F32Sub(left, right),
                k if k.is_i64() => WirInstr::I64Sub(left, right),
                _ => WirInstr::I32Sub(left, right),
            },
            TirBinaryOp::Mul => match kind {
                PrimitiveKind::F64 => WirInstr::F64Mul(left, right),
                PrimitiveKind::F32 => WirInstr::F32Mul(left, right),
                k if k.is_i64() => WirInstr::I64Mul(left, right),
                _ => WirInstr::I32Mul(left, right),
            },
            TirBinaryOp::Div => match kind {
                PrimitiveKind::F64 => WirInstr::F64Div(left, right),
                PrimitiveKind::F32 => WirInstr::F32Div(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64DivU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64DivS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32DivU(left, right),
                _ => WirInstr::I32DivS(left, right),
            },
            TirBinaryOp::Mod => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64RemU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64RemS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32RemU(left, right),
                _ => WirInstr::I32RemS(left, right),
            },
            TirBinaryOp::Eq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Eq(left, right),
                PrimitiveKind::F32 => WirInstr::F32Eq(left, right),
                k if k.is_i64() => WirInstr::I64Eq(left, right),
                _ => WirInstr::I32Eq(left, right),
            },
            TirBinaryOp::NotEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Ne(left, right),
                PrimitiveKind::F32 => WirInstr::F32Ne(left, right),
                k if k.is_i64() => WirInstr::I64Ne(left, right),
                _ => WirInstr::I32Ne(left, right),
            },
            TirBinaryOp::Lt => match kind {
                PrimitiveKind::F64 => WirInstr::F64Lt(left, right),
                PrimitiveKind::F32 => WirInstr::F32Lt(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64LtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LtU(left, right),
                _ => WirInstr::I32LtS(left, right),
            },
            TirBinaryOp::LtEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Le(left, right),
                PrimitiveKind::F32 => WirInstr::F32Le(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64LeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LeU(left, right),
                _ => WirInstr::I32LeS(left, right),
            },
            TirBinaryOp::Gt => match kind {
                PrimitiveKind::F64 => WirInstr::F64Gt(left, right),
                PrimitiveKind::F32 => WirInstr::F32Gt(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64GtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GtU(left, right),
                _ => WirInstr::I32GtS(left, right),
            },
            TirBinaryOp::GtEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Ge(left, right),
                PrimitiveKind::F32 => WirInstr::F32Ge(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64GeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GeU(left, right),
                _ => WirInstr::I32GeS(left, right),
            },
            TirBinaryOp::And | TirBinaryOp::BitAnd => {
                if kind.is_i64() {
                    WirInstr::I64And(left, right)
                } else {
                    WirInstr::I32And(left, right)
                }
            }
            TirBinaryOp::Or | TirBinaryOp::BitOr => {
                if kind.is_i64() {
                    WirInstr::I64Or(left, right)
                } else {
                    WirInstr::I32Or(left, right)
                }
            }
            TirBinaryOp::BitXor => {
                if kind.is_i64() {
                    WirInstr::I64Xor(left, right)
                } else {
                    WirInstr::I32Xor(left, right)
                }
            }
            TirBinaryOp::Shl => {
                if kind.is_i64() {
                    WirInstr::I64Shl(left, right)
                } else {
                    WirInstr::I32Shl(left, right)
                }
            }
            TirBinaryOp::Shr => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64ShrU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64ShrS(left, right),
                k if k.is_unsigned() => WirInstr::I32ShrU(left, right),
                _ => WirInstr::I32ShrS(left, right),
            },
            TirBinaryOp::RefEq => WirInstr::RefEq(left, right),
            TirBinaryOp::RefNotEq => WirInstr::I32Eqz(Box::new(WirInstr::RefEq(left, right))),
        }
    }

    /// Translate a unary operation to WIR.
    fn translate_unary_op(
        &self,
        op: &TirUnaryOp,
        operand: Box<WirInstr>,
        operand_type_id: TypeId,
    ) -> WirInstr {
        let kind = PrimitiveKind::from_type_id(self.type_table, operand_type_id);

        match op {
            TirUnaryOp::Neg => match kind {
                PrimitiveKind::F64 => WirInstr::F64Neg(operand),
                PrimitiveKind::F32 => WirInstr::F32Neg(operand),
                k if k.is_i64() => WirInstr::I64Sub(Box::new(WirInstr::I64Const(0)), operand),
                _ => WirInstr::I32Sub(Box::new(WirInstr::I32Const(0)), operand),
            },
            TirUnaryOp::Not => WirInstr::I32Eqz(operand),
            TirUnaryOp::BitNot => {
                if kind.is_i64() {
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
        if let TirExprKind::IntLiteral { value, .. } = &inner.kind
            && matches!(
                self.type_table.get(to_type),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
            )
        {
            return WirInstr::I64Const(*value as i64);
        }

        let inner_instr = self.translate_expr(inner);
        let from = self.type_table.get(from_type);
        let to = self.type_table.get(to_type);

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
        resolved_info.struct_name.clone_from(&base_name);
        resolved_info.base_struct_name = base_name;
        let mangled = resolved_info.to_mangled_name();
        let fq = format!("{module_source}/{mangled}");
        self.ctx.func_map.get(&fq).cloned()
    }

    /// Resolve a newtype name to the ultimate base struct/primitive name.
    /// Returns `None` if the name is not a newtype.
    fn resolve_newtype_to_base_struct_name(&self, name: &str) -> Option<String> {
        self.type_table
            .get_newtype_ultimate_base_name(name)
            .map(str::to_owned)
    }

    /// Translate a builtin intrinsic call to a WIR instruction.
    ///
    /// Returns `Some(instr)` for instruction-builtins (Wasm instructions),
    /// `None` for import-builtins (handled as regular function calls).
    fn translate_builtin_call(
        &mut self,
        builtin_name: &str,
        args: &[CallArg],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        match builtin_name {
            // === Memory Load Instructions ===
            "builtin::i32_load" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Load {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load8_u" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Load8U {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load16_u" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Load16U {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                })
            }

            "builtin::i64_load" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Load {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                })
            }

            // === Memory Store Instructions ===
            "builtin::i32_store" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32Store {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store8" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32Store8 {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store16" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32Store16 {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i64_store" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
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
                let len = self.translate_expr(&args[0].expr);
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
                let arr = self.translate_expr(&args[0].expr);
                Some(WirInstr::ArrayLen(Box::new(arr)))
            }
            "builtin::array_get_u8" => {
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                self.ctx
                    .array_type_by_name
                    .get("u8")
                    .map(|type_id| WirInstr::ArrayGetU {
                        type_id: type_id.clone(),
                        array: Box::new(arr),
                        index: Box::new(idx),
                        result_ty: WirType::I32,
                    })
            }
            "builtin::array_set_u8" => {
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                let val = self.translate_expr(&args[2].expr);
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
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayGet {
                        type_id: type_id.clone(),
                        array: Box::new(arr),
                        index: Box::new(idx),
                        result_ty: self.array_element_wir_type(&type_id),
                    })
                } else {
                    None
                }
            }
            "builtin::array_set" => {
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                let val = self.translate_expr(&args[2].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
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
                let dst = self.translate_expr(&args[0].expr);
                let dst_offset = self.translate_expr(&args[1].expr);
                let src = self.translate_expr(&args[2].expr);
                let src_offset = self.translate_expr(&args[3].expr);
                let len = self.translate_expr(&args[4].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
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
                let arr = self.translate_expr(&args[0].expr);
                let offset = self.translate_expr(&args[1].expr);
                let val = self.translate_expr(&args[2].expr);
                let len = self.translate_expr(&args[3].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
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

            // === Reference Identity ===
            "builtin::ref_eq" => {
                let l = self.translate_expr(&args[0].expr);
                let r = self.translate_expr(&args[1].expr);
                Some(WirInstr::RefEq(Box::new(l), Box::new(r)))
            }

            // === Bitwise/Integer ===
            "builtin::i32_and" => {
                let l = self.translate_expr(&args[0].expr);
                let r = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32And(Box::new(l), Box::new(r)))
            }
            "builtin::i32_eqz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Eqz(Box::new(o)))
            }
            "builtin::i32_clz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Clz(Box::new(o)))
            }
            "builtin::i64_clz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Clz(Box::new(o)))
            }
            "builtin::i32_ctz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Ctz(Box::new(o)))
            }
            "builtin::i64_ctz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Ctz(Box::new(o)))
            }
            "builtin::i32_popcnt" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Popcnt(Box::new(o)))
            }
            "builtin::i64_popcnt" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Popcnt(Box::new(o)))
            }

            // === Reinterpretation ===
            "builtin::i64_reinterpret_f64" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64ReinterpretF64(Box::new(o)))
            }
            "builtin::f64_reinterpret_i64" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64ReinterpretI64(Box::new(o)))
            }
            "builtin::i32_reinterpret_f32" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32ReinterpretF32(Box::new(o)))
            }
            "builtin::f32_reinterpret_i32" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32ReinterpretI32(Box::new(o)))
            }

            "builtin::v128_const" => {
                // The argument is an i128 literal interpreted as v128 bit pattern
                let o = self.translate_expr(&args[0].expr);
                // Extract the constant value - it should be an IntLiteral
                if let WirInstr::I32Const(v) = &o {
                    Some(WirInstr::V128Const(i128::from(*v)))
                } else if let WirInstr::I64Const(v) = &o {
                    Some(WirInstr::V128Const(i128::from(*v)))
                } else {
                    Some(WirInstr::V128Const(0))
                }
            }
            "builtin::v128_not" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::V128Not(Box::new(a)))
            }
            "builtin::v128_and" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128And(Box::new(a), Box::new(b)))
            }
            "builtin::v128_or" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128Or(Box::new(a), Box::new(b)))
            }
            "builtin::v128_xor" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128Xor(Box::new(a), Box::new(b)))
            }
            "builtin::v128_bitselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::V128Bitselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i8x16_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Splat(Box::new(a)))
            }
            "builtin::i8x16_extract_lane_s" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ExtractLaneS(lane, Box::new(a)))
            }
            "builtin::i8x16_extract_lane_u" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ExtractLaneU(lane, Box::new(a)))
            }
            "builtin::i8x16_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I8x16ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i8x16_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Add(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Neg(Box::new(a)))
            }
            "builtin::i8x16_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_lt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LtU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_gt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GtU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_le_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LeU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_ge_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GeU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_swizzle" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Swizzle(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Splat(Box::new(a)))
            }
            "builtin::i16x8_extract_lane_s" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtractLaneS(lane, Box::new(a)))
            }
            "builtin::i16x8_extract_lane_u" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtractLaneU(lane, Box::new(a)))
            }
            "builtin::i16x8_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I16x8ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i16x8_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Add(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Mul(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Neg(Box::new(a)))
            }
            "builtin::i16x8_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_lt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LtU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_gt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GtU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_le_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LeU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_ge_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GeU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Splat(Box::new(a)))
            }
            "builtin::i32x4_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtractLane(lane, Box::new(a)))
            }
            "builtin::i32x4_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I32x4ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i32x4_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Add(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Mul(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Neg(Box::new(a)))
            }
            "builtin::i32x4_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_lt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LtU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_gt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GtU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_le_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LeU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_ge_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GeU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Splat(Box::new(a)))
            }
            "builtin::i64x2_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtractLane(lane, Box::new(a)))
            }
            "builtin::i64x2_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I64x2ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i64x2_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Add(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Mul(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Neg(Box::new(a)))
            }
            "builtin::i64x2_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Splat(Box::new(a)))
            }
            "builtin::f32x4_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4ExtractLane(lane, Box::new(a)))
            }
            "builtin::f32x4_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::F32x4ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::f32x4_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Add(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Sub(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Mul(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_div" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Div(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Neg(Box::new(a)))
            }
            "builtin::f32x4_sqrt" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Sqrt(Box::new(a)))
            }
            "builtin::f32x4_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Abs(Box::new(a)))
            }
            "builtin::f32x4_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Eq(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Ne(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_lt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Lt(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_gt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Gt(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_le" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Le(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_ge" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Ge(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Min(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Max(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Splat(Box::new(a)))
            }
            "builtin::f64x2_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2ExtractLane(lane, Box::new(a)))
            }
            "builtin::f64x2_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::F64x2ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::f64x2_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Add(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Sub(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Mul(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_div" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Div(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Neg(Box::new(a)))
            }
            "builtin::f64x2_sqrt" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Sqrt(Box::new(a)))
            }
            "builtin::f64x2_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Abs(Box::new(a)))
            }
            "builtin::f64x2_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Eq(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Ne(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_lt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Lt(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_gt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Gt(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_le" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Le(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_ge" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Ge(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Min(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Max(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Abs(Box::new(a)))
            }
            "builtin::i8x16_add_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16AddSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_add_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16AddSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_sub_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16SubSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_sub_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16SubSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_min_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MinS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_min_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MinU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_max_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MaxS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_max_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MaxU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_avgr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16AvgrU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16AllTrue(Box::new(a)))
            }
            "builtin::i8x16_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Bitmask(Box::new(a)))
            }
            "builtin::i8x16_narrow_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16NarrowI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_narrow_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16NarrowI16x8U(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_popcnt" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Popcnt(Box::new(a)))
            }
            "builtin::i16x8_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Abs(Box::new(a)))
            }
            "builtin::i16x8_add_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8AddSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_add_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8AddSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_sub_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8SubSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_sub_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8SubSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_min_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MinS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_min_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MinU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_max_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MaxS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_max_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MaxU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_avgr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8AvgrU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8AllTrue(Box::new(a)))
            }
            "builtin::i16x8_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Bitmask(Box::new(a)))
            }
            "builtin::i16x8_narrow_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8NarrowI32x4S(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_narrow_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8NarrowI32x4U(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extend_low_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendLowI8x16S(Box::new(a)))
            }
            "builtin::i16x8_extend_high_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendHighI8x16S(Box::new(a)))
            }
            "builtin::i16x8_extend_low_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendLowI8x16U(Box::new(a)))
            }
            "builtin::i16x8_extend_high_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendHighI8x16U(Box::new(a)))
            }
            "builtin::i16x8_extmul_low_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulLowI8x16S(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extmul_high_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulHighI8x16S(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extmul_low_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulLowI8x16U(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extmul_high_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulHighI8x16U(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extadd_pairwise_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtAddPairwiseI8x16S(Box::new(a)))
            }
            "builtin::i16x8_extadd_pairwise_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtAddPairwiseI8x16U(Box::new(a)))
            }
            "builtin::i16x8_q15mulr_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Q15MulrSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Abs(Box::new(a)))
            }
            "builtin::i32x4_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4AllTrue(Box::new(a)))
            }
            "builtin::i32x4_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Bitmask(Box::new(a)))
            }
            "builtin::i32x4_min_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MinS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_min_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MinU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_max_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MaxS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_max_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MaxU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_dot_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4DotI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extend_low_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendLowI16x8S(Box::new(a)))
            }
            "builtin::i32x4_extend_high_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendHighI16x8S(Box::new(a)))
            }
            "builtin::i32x4_extend_low_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendLowI16x8U(Box::new(a)))
            }
            "builtin::i32x4_extend_high_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendHighI16x8U(Box::new(a)))
            }
            "builtin::i32x4_extmul_low_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulLowI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extmul_high_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulHighI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extmul_low_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulLowI16x8U(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extmul_high_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulHighI16x8U(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extadd_pairwise_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtAddPairwiseI16x8S(Box::new(a)))
            }
            "builtin::i32x4_extadd_pairwise_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtAddPairwiseI16x8U(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF32x4S(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF32x4U(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f64x2_s_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF64x2SZero(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f64x2_u_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF64x2UZero(Box::new(a)))
            }
            "builtin::i64x2_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Abs(Box::new(a)))
            }
            "builtin::i64x2_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2AllTrue(Box::new(a)))
            }
            "builtin::i64x2_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Bitmask(Box::new(a)))
            }
            "builtin::i64x2_extend_low_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendLowI32x4S(Box::new(a)))
            }
            "builtin::i64x2_extend_high_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendHighI32x4S(Box::new(a)))
            }
            "builtin::i64x2_extend_low_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendLowI32x4U(Box::new(a)))
            }
            "builtin::i64x2_extend_high_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendHighI32x4U(Box::new(a)))
            }
            "builtin::i64x2_extmul_low_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulLowI32x4S(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_extmul_high_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulHighI32x4S(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_extmul_low_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulLowI32x4U(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_extmul_high_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulHighI32x4U(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_ceil" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Ceil(Box::new(a)))
            }
            "builtin::f32x4_floor" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Floor(Box::new(a)))
            }
            "builtin::f32x4_trunc" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Trunc(Box::new(a)))
            }
            "builtin::f32x4_nearest" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Nearest(Box::new(a)))
            }
            "builtin::f32x4_pmin" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4PMin(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_pmax" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4PMax(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_convert_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4ConvertI32x4S(Box::new(a)))
            }
            "builtin::f32x4_convert_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4ConvertI32x4U(Box::new(a)))
            }
            "builtin::f32x4_demote_f64x2_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4DemoteF64x2Zero(Box::new(a)))
            }
            "builtin::f64x2_ceil" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Ceil(Box::new(a)))
            }
            "builtin::f64x2_floor" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Floor(Box::new(a)))
            }
            "builtin::f64x2_trunc" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Trunc(Box::new(a)))
            }
            "builtin::f64x2_nearest" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Nearest(Box::new(a)))
            }
            "builtin::f64x2_pmin" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2PMin(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_pmax" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2PMax(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_convert_low_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2ConvertLowI32x4S(Box::new(a)))
            }
            "builtin::f64x2_convert_low_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2ConvertLowI32x4U(Box::new(a)))
            }
            "builtin::f64x2_promote_low_f32x4" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2PromoteLowF32x4(Box::new(a)))
            }
            "builtin::v128_andnot" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128AndNot(Box::new(a), Box::new(b)))
            }
            "builtin::v128_any_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::V128AnyTrue(Box::new(a)))
            }
            "builtin::i8x16_relaxed_swizzle" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16RelaxedSwizzle(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I8x16RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i16x8_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I16x8RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i32x4_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I32x4RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i64x2_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I64x2RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f32x4_relaxed_madd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F32x4RelaxedMadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f32x4_relaxed_nmadd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F32x4RelaxedNmadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f64x2_relaxed_madd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F64x2RelaxedMadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f64x2_relaxed_nmadd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F64x2RelaxedNmadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f32x4_relaxed_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4RelaxedMin(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_relaxed_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4RelaxedMax(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_relaxed_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2RelaxedMin(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_relaxed_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2RelaxedMax(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_relaxed_trunc_f32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF32x4S(Box::new(a)))
            }
            "builtin::i32x4_relaxed_trunc_f32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF32x4U(Box::new(a)))
            }
            "builtin::i32x4_relaxed_trunc_f64x2_s_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF64x2SZero(Box::new(a)))
            }
            "builtin::i32x4_relaxed_trunc_f64x2_u_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF64x2UZero(Box::new(a)))
            }
            "builtin::i16x8_relaxed_q15mulr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8RelaxedQ15mulrS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_relaxed_dot_i8x16_i7x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8RelaxedDotI8x16I7x16S(
                    Box::new(a),
                    Box::new(b),
                ))
            }
            "builtin::i32x4_relaxed_dot_i8x16_i7x16_add_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I32x4RelaxedDotI8x16I7x16AddS(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }

            // === Memory ===
            "builtin::memory_grow" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::MemoryGrow(Box::new(o)))
            }
            "builtin::memory_size" => Some(WirInstr::MemorySize),
            "builtin::memory_fill" => {
                let dst = self.translate_expr(&args[0].expr);
                let value = self.translate_expr(&args[1].expr);
                let len = self.translate_expr(&args[2].expr);
                Some(WirInstr::MemoryFill {
                    dst: Box::new(dst),
                    value: Box::new(value),
                    len: Box::new(len),
                })
            }

            // === Control ===
            "builtin::unreachable" => Some(WirInstr::Unreachable),
            "builtin::likely" | "builtin::unlikely" => {
                let likely = builtin_name == "builtin::likely";
                let expr = self.translate_expr(&args[0].expr);
                Some(WirInstr::BranchHint {
                    likely,
                    expr: Box::new(expr),
                })
            }
            "builtin::select" => {
                let cond = self.translate_expr(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let b = self.translate_expr(&args[2].expr);
                let result_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[1].expr.type_id);
                Some(WirInstr::Select {
                    condition: Box::new(cond),
                    if_true: Box::new(a),
                    if_false: Box::new(b),
                    ty: Some(result_type),
                })
            }

            // === Multi-value 128-bit integer operations ===
            "builtin::i64_add128" => {
                let a_lo = Box::new(self.translate_expr(&args[0].expr));
                let a_hi = Box::new(self.translate_expr(&args[1].expr));
                let b_lo = Box::new(self.translate_expr(&args[2].expr));
                let b_hi = Box::new(self.translate_expr(&args[3].expr));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Add128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_sub128" => {
                let a_lo = Box::new(self.translate_expr(&args[0].expr));
                let a_hi = Box::new(self.translate_expr(&args[1].expr));
                let b_lo = Box::new(self.translate_expr(&args[2].expr));
                let b_hi = Box::new(self.translate_expr(&args[3].expr));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Sub128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_mul_wide_u" => {
                let a = Box::new(self.translate_expr(&args[0].expr));
                let b = Box::new(self.translate_expr(&args[1].expr));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideU(a, b), result_type_id))
            }
            "builtin::i64_mul_wide_s" => {
                let a = Box::new(self.translate_expr(&args[0].expr));
                let b = Box::new(self.translate_expr(&args[1].expr));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideS(a, b), result_type_id))
            }

            // === No-op casts ===
            "builtin::i32_as_char" => Some(self.translate_expr(&args[0].expr)),

            // === WASI call indirects ===
            "builtin::call_indirect_stdout_write_via_stream"
            | "builtin::call_indirect_stderr_write_via_stream" => {
                // Wado uses stackful async: canon lower without async flag,
                // so sync lower returns the result directly.
                let is_stderr = builtin_name.contains("stderr");
                let wasi_func_name = if is_stderr {
                    "wasi:cli/Stderr::write_via_stream"
                } else {
                    "wasi:cli/Stdout::write_via_stream"
                };
                let key = format!("wasi/{wasi_func_name}");
                if let Some(func_id) = self.ctx.func_map.get(&key).cloned() {
                    let call_args: Vec<WirInstr> =
                        args.iter().map(|a| self.translate_expr(&a.expr)).collect();
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

    /// Map a primitive type to its CM scalar type.
    ///
    /// Returns `Some(CmScalarType::S32)` for i32, `Some(CmScalarType::U8)` for u8, etc.
    /// Returns `None` for non-CM-scalar types (f32, f64 are included as float32/float64).
    fn primitive_to_cm_scalar(prim: &PrimitiveType) -> Option<CmScalarType> {
        match prim {
            PrimitiveType::I8 => Some(CmScalarType::S8),
            PrimitiveType::I16 => Some(CmScalarType::S16),
            PrimitiveType::I32 => Some(CmScalarType::S32),
            PrimitiveType::I64 => Some(CmScalarType::S64),
            PrimitiveType::U8 => Some(CmScalarType::U8),
            PrimitiveType::U16 => Some(CmScalarType::U16),
            PrimitiveType::U32 => Some(CmScalarType::U32),
            PrimitiveType::U64 => Some(CmScalarType::U64),
            PrimitiveType::F32 => Some(CmScalarType::F32),
            PrimitiveType::F64 => Some(CmScalarType::F64),
            PrimitiveType::Bool => Some(CmScalarType::Bool),
            PrimitiveType::Char => Some(CmScalarType::Char),
            _ => None,
        }
    }

    /// Get the CM future payload type from `MonomorphInfo` (for static methods like `Future::new`).
    fn cm_future_payload_from_monomorph(&self, func: &FunctionRef) -> CmFuturePayload {
        if let Some(ref info) = func.monomorph_info
            && !info.impl_type_args.is_empty()
        {
            return self.classify_future_payload(info.impl_type_args[0]);
        }
        CmFuturePayload::Trailers
    }

    /// Get the CM future payload type for a Future/FutureWritable receiver.
    fn cm_future_payload(&self, receiver_type_id: TypeId) -> CmFuturePayload {
        // Receiver is &Future<T> or &FutureWritable<T> — unwrap the reference
        let inner_type_id = match self.type_table.get(receiver_type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => receiver_type_id,
        };
        if let ResolvedType::GenericResource { type_args, .. } = self.type_table.get(inner_type_id)
            && !type_args.is_empty()
        {
            return self.classify_future_payload(type_args[0]);
        }
        CmFuturePayload::Trailers
    }

    /// Classify a future's type argument into the CM future payload category.
    ///
    /// - Primitive scalar → `Scalar(s)`
    /// - `Result<(), E>` (unit Ok type) → `Transmission` (HTTP transmission result)
    /// - Anything else → `Trailers` (HTTP trailers pattern)
    fn classify_future_payload(&self, type_arg: TypeId) -> CmFuturePayload {
        match self.type_table.get(type_arg) {
            ResolvedType::Primitive(prim) => {
                if let Some(scalar) = Self::primitive_to_cm_scalar(prim) {
                    return CmFuturePayload::Scalar(scalar);
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Result" && type_args.len() >= 2 => {
                if matches!(self.type_table.get(type_args[0]), ResolvedType::Unit) {
                    // Determine ErrorCode source from the error type's package
                    let source = self.error_code_source(type_args[1]);
                    return CmFuturePayload::Transmission(source);
                }
            }
            _ => {}
        }
        CmFuturePayload::Trailers
    }

    /// Determine the WASI package source for an `ErrorCode` type.
    ///
    /// Extracts the package name from the type's `ModuleSource::Wasi { interface }`
    /// (e.g. `"http/types.wado"` → `"http"`). Falls back to `"cli"` if the error
    /// type is not a WASI-defined enum or variant.
    fn error_code_source(&self, error_type_id: TypeId) -> String {
        let module_source = match self.type_table.get(error_type_id) {
            ResolvedType::Enum { module_source, .. }
            | ResolvedType::Variant { module_source, .. } => module_source,
            _ => return "cli".to_string(),
        };
        if let crate::name::ModuleSource::Wasi { interface } = module_source {
            // interface format is "{package}/..." (e.g., "http/types.wado")
            interface.split('/').next().unwrap_or("cli").to_string()
        } else {
            "cli".to_string()
        }
    }

    /// Dispatch canonical resource methods based on `#[cm("...")]` attribute.
    /// Returns `Some(WirInstr)` if the method has a canonical name and was handled.
    ///
    /// Most CM methods are now handled by synthesis (rewritten to `CmRawCall` or
    /// internal binding calls). Only future operations remain here because they
    /// require payload-parameterized canonical imports.
    fn try_translate_canonical_method(
        &mut self,
        receiver: &TirExpr,
        func: &FunctionRef,
        args: &[CallArg],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        let cm_name = func.method_info.clone()?.cm_name.as_ref()?.clone();
        let handle = self.translate_expr(receiver);
        match cm_name.as_str() {
            // Future operations: payload-parameterized canonical imports
            "future-read" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_future_read(handle, result_type_id, payload))
            }
            "future-write" => {
                let value_arg = self.translate_expr(&args[0].expr);
                let value_type_id = args[0].expr.type_id;
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_future_write(handle, value_arg, value_type_id, payload))
            }
            "future-cancel-read" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureCancelRead(payload), handle))
            }
            "future-cancel-write" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureCancelWrite(payload), handle))
            }
            "future-drop-readable" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureDropReadable(payload), handle))
            }
            "future-drop-writable" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureDropWritable(payload), handle))
            }

            other => {
                eprintln!("[WIR] unhandled canonical method: {other}");
                None
            }
        }
    }

    /// Dispatch canonical resource static methods (e.g., `Stream::new`, `Future::new`).
    /// Returns `Some(WirInstr)` if the canonical name was handled.
    ///
    /// Most static CM methods are now handled by synthesis. Only stream/future-new
    /// remain here because they require i64→tuple splitting with proper GC type casting.
    fn try_translate_canonical_static_method(
        &mut self,
        canonical: &str,
        func: &FunctionRef,
        _args: &[CallArg],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        match canonical {
            "stream-new" => Some(self.emit_stream_or_future_new(
                false,
                CmFuturePayload::Trailers,
                result_type_id,
            )),
            "future-new" => {
                let payload = self.cm_future_payload_from_monomorph(func);
                Some(self.emit_stream_or_future_new(true, payload, result_type_id))
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
    fn emit_drop_handle(&mut self, intrinsic: CanonicalIntrinsic, handle: WirInstr) -> WirInstr {
        let func_id = self
            .ctx
            .ensure_canonical(intrinsic, vec![WirType::I32], vec![]);
        WirInstr::Call {
            func_id,
            args: vec![handle],
        }
    }

    /// Emit `stream-new()` or `future-new()` → split i64 into [`rx_i32`, `tx_i32`] tuple.
    fn emit_stream_or_future_new(
        &mut self,
        is_future: bool,
        payload: CmFuturePayload,
        result_type_id: TypeId,
    ) -> WirInstr {
        let intrinsic = if is_future {
            CanonicalIntrinsic::FutureNew(payload)
        } else {
            CanonicalIntrinsic::StreamNew(CmStreamPayload::U8)
        };
        let func_id = self
            .ctx
            .ensure_canonical(intrinsic, vec![], vec![WirType::I64]);

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
        let get_low = WirInstr::I32WrapI64(Box::new(WirInstr::LocalGet {
            name: temp.clone(),
            result_ty: WirType::I64,
        }));
        let get_high = WirInstr::I32WrapI64(Box::new(WirInstr::I64ShrU(
            Box::new(WirInstr::LocalGet {
                name: temp,
                result_ty: WirType::I64,
            }),
            Box::new(WirInstr::I64Const(32)),
        )));

        let mut instrs = vec![declare, set_temp];
        instrs.push(self.struct_new(type_id, vec![get_low, get_high]));
        WirInstr::Seq(instrs)
    }

    /// Emit `future-read(handle)` → `Option<T>`.
    ///
    /// Currently only supports `T` = `Result<Option<Trailers>, ErrorCode>` (8-byte CM payload).
    /// The synthesis:
    /// 1. Allocate 8 bytes via realloc
    /// 2. Call `future-read(handle, ptr)` → `raw_result`
    /// 3. If BLOCKED (`0xFFFF_FFFF`), wait via waitable-set, re-read result
    /// 4. Extract status = result & 0xF
    /// 5. If COMPLETED (status 0): return `Option::Some(value)` — value read from ptr
    /// 6. If DROPPED (status 1): return `Option::None`
    /// 7. Free linear memory
    ///
    /// TODO: implement general CM lifting for arbitrary T values.
    /// Currently hardcoded for the `Ok(None)` pattern (all-zeros payload = 8 bytes).
    fn emit_future_read(
        &mut self,
        handle: WirInstr,
        result_type_id: TypeId,
        payload: CmFuturePayload,
    ) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            return WirInstr::Unreachable;
        };
        // future-read: (handle: i32, ptr: i32) -> i32
        let future_read_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::FutureRead(payload),
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        let ws_new_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetNew,
            vec![],
            vec![WirType::I32],
        );
        let w_join_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableJoin,
            vec![WirType::I32, WirType::I32],
            vec![],
        );
        let ws_wait_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetWait,
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let handle_name = format!("__fr_handle_{suffix}");
        let ptr_name = format!("__fr_ptr_{suffix}");
        let result_name = format!("__fr_result_{suffix}");
        let evt_ptr_name = format!("__fr_evtptr_{suffix}");

        let mut instrs = vec![];

        // Declare locals
        for (name, ty) in [
            (&handle_name, WirType::I32),
            (&ptr_name, WirType::I32),
            (&result_name, WirType::I32),
        ] {
            instrs.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        // Save handle
        instrs.push(WirInstr::LocalSet {
            name: handle_name.clone(),
            value: Box::new(handle),
        });

        // Allocate buffer for the CM payload.
        // Use 40 bytes (same as future-write) to cover the worst-case CM layout.
        const BUF_SIZE: i32 = 40;
        const BUF_ALIGN: i32 = 8;

        instrs.push(WirInstr::LocalSet {
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
        });

        // result = future-read(handle, ptr)
        instrs.push(WirInstr::LocalSet {
            name: result_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: future_read_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                        result_ty: WirType::I32,
                    },
                    WirInstr::LocalGet {
                        name: ptr_name.clone(),
                        result_ty: WirType::I32,
                    },
                ],
            }),
        });

        // canon future.read returns pack_copy_result:
        //   0xFFFF_FFFF (BLOCKED): future not ready, wait using the FUTURE handle
        //   (count << 4) | status:
        //     status 0 = COMPLETED: payload written to buffer → Some(lifted_value)
        //     status 1 = DROPPED: writer dropped → None
        //     status 2 = CANCELLED
        //   For futures, count is always 0, so COMPLETED = 0, DROPPED = 1.
        let ws_drop_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetDrop,
            vec![WirType::I32],
            vec![],
        );
        // Case 1: BLOCKED (0xFFFF_FFFF) → wait on future handle
        instrs.push(WirInstr::If {
            condition: Box::new(WirInstr::I32Eq(
                Box::new(WirInstr::LocalGet {
                    name: result_name.clone(),
                    result_ty: WirType::I32,
                }),
                Box::new(WirInstr::I32Const(-1)),
            )),
            result: None,
            then_body: {
                let evt = evt_ptr_name;
                let ws_name = format!("__fr_ws_{suffix}");
                vec![
                    WirInstr::DeclareLocal {
                        name: ws_name.clone(),
                        ty: WirType::I32,
                    },
                    WirInstr::DeclareLocal {
                        name: evt.clone(),
                        ty: WirType::I32,
                    },
                    // ws = waitable_set_new()
                    WirInstr::LocalSet {
                        name: ws_name.clone(),
                        value: Box::new(WirInstr::Call {
                            func_id: ws_new_id,
                            args: vec![],
                        }),
                    },
                    // waitable_join(FUTURE_HANDLE, ws) — wait on the future itself
                    WirInstr::Call {
                        func_id: w_join_id.clone(),
                        args: vec![
                            WirInstr::LocalGet {
                                name: handle_name.clone(),
                                result_ty: WirType::I32,
                            },
                            WirInstr::LocalGet {
                                name: ws_name.clone(),
                                result_ty: WirType::I32,
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
                                name: ws_name.clone(),
                                result_ty: WirType::I32,
                            },
                            WirInstr::LocalGet {
                                name: evt.clone(),
                                result_ty: WirType::I32,
                            },
                        ],
                    })),
                    // Free event buffer
                    WirInstr::Drop(Box::new(WirInstr::Call {
                        func_id: realloc_id.clone(),
                        args: vec![
                            WirInstr::LocalGet {
                                name: evt,
                                result_ty: WirType::I32,
                            },
                            WirInstr::I32Const(8),
                            WirInstr::I32Const(4),
                            WirInstr::I32Const(0),
                        ],
                    })),
                    // Unjoin future handle from waitable set before dropping it.
                    // waitable.join(handle, 0) removes the child relationship.
                    WirInstr::Call {
                        func_id: w_join_id,
                        args: vec![
                            WirInstr::LocalGet {
                                name: handle_name,
                                result_ty: WirType::I32,
                            },
                            WirInstr::I32Const(0),
                        ],
                    },
                    // Drop waitable set
                    WirInstr::Call {
                        func_id: ws_drop_id,
                        args: vec![WirInstr::LocalGet {
                            name: ws_name,
                            result_ty: WirType::I32,
                        }],
                    },
                    // After wait completes, the data transfer is done and the
                    // future handle is consumed.  Do NOT retry future-read;
                    // just mark the result as COMPLETED (0) so the payload
                    // lifter reads from the buffer.
                    WirInstr::LocalSet {
                        name: result_name.clone(),
                        value: Box::new(WirInstr::I32Const(0)),
                    },
                ]
            },
            else_body: None,
        });

        // pack_copy_result status:
        //   status 0 = COMPLETED → payload written to buffer → Some(lifted_value)
        //   status 1 = DROPPED → writer dropped → None
        //   After BLOCKED wait, result is set to 0 (COMPLETED).
        let option_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);

        // Build the Some payload by lifting T from linear memory.
        let some_payload = self.lift_future_read_payload(result_type_id, &ptr_name);
        let some_variant =
            self.build_variant_case_wir(result_type_id, 0, "Some", Some(some_payload));
        let none_variant = self.build_variant_case_wir(result_type_id, 1, "None", None);

        self.local_counter += 1;
        let option_result_name = format!("__fr_opt_{}", self.local_counter);
        instrs.push(WirInstr::DeclareLocal {
            name: option_result_name.clone(),
            ty: option_wir_type.clone(),
        });

        // option_result = if (result & 0xF) == 0 { Some(lifted_value) } else { None }
        instrs.push(WirInstr::LocalSet {
            name: option_result_name.clone(),
            value: Box::new(WirInstr::If {
                condition: Box::new(WirInstr::I32Eq(
                    Box::new(WirInstr::I32And(
                        Box::new(WirInstr::LocalGet {
                            name: result_name,
                            result_ty: WirType::I32,
                        }),
                        Box::new(WirInstr::I32Const(0xF)),
                    )),
                    Box::new(WirInstr::I32Const(0)),
                )),
                result: Some(option_wir_type.clone()),
                then_body: vec![some_variant],
                else_body: Some(vec![none_variant]),
            }),
        });

        // Free payload buffer (after reading from it in the if-then branch)
        instrs.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet {
                    name: ptr_name.clone(),
                    result_ty: WirType::I32,
                },
                WirInstr::I32Const(BUF_SIZE),
                WirInstr::I32Const(BUF_ALIGN),
                WirInstr::I32Const(0),
            ],
        })));

        instrs.push(WirInstr::LocalGet {
            name: option_result_name,
            result_ty: option_wir_type,
        });

        WirInstr::Seq(instrs)
    }

    /// Lift the T value from the CM payload buffer for `Future::read`.
    ///
    /// `option_type_id` is `Option<T>`. We extract T and lift it from memory at `ptr_name`.
    ///
    /// Currently supports:
    /// - `T = Result<Option<own<resource>>, ErrorCode>` — the trailers pattern
    ///   CM layout: [`result_disc:i32`, `option_disc:i32`, handle:i32]
    ///   Ok(None) = [0, 0, 0], Ok(Some(h)) = [0, 1, h]
    ///   Err(...) → trap (not yet implemented)
    fn lift_future_read_payload(&mut self, option_type_id: TypeId, ptr_name: &str) -> WirInstr {
        // option_type_id = Option<T>; extract T
        let Some(inner_t) = self.type_table.as_option(option_type_id) else {
            return WirInstr::Unreachable;
        };

        // Check if T is a scalar numeric type (i32, i64, f32, f64, etc.)
        if let Some(load_instr) = self.lift_cm_scalar(inner_t, ptr_name) {
            return load_instr;
        }

        // Check if T is Result<Ok, Err>
        if let ResolvedType::GenericInstance {
            name, type_args, ..
        } = self.type_table.get(inner_t)
            && name == "Result"
            && type_args.len() == 2
        {
            let ok_type_id = type_args[0];

            // Check if Ok type is Option<R> (the trailers pattern)
            if let Some(inner_resource_type_id) = self.type_table.as_option(ok_type_id) {
                return self.lift_result_option_resource(
                    inner_t,
                    ok_type_id,
                    inner_resource_type_id,
                    ptr_name,
                );
            }

            // Check if Ok type is () (the transmission pattern: Result<(), E>)
            if matches!(self.type_table.get(ok_type_id), ResolvedType::Unit) {
                return self.lift_result_unit(inner_t, ptr_name);
            }
        }

        // Fallback: unsupported T type — emit unreachable
        // (will trap at runtime if this code path is reached)
        WirInstr::Unreachable
    }

    /// Lift a scalar numeric value from CM linear memory at `ptr_name + 0`.
    ///
    /// Returns `Some(WirInstr)` for CM number types (i8–i64, u8–u64, f32, f64,
    /// bool, char), `None` otherwise.
    fn lift_cm_scalar(&self, type_id: TypeId, ptr_name: &str) -> Option<WirInstr> {
        let ResolvedType::Primitive(prim) = self.type_table.get(type_id) else {
            return None;
        };
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.to_string(),
            result_ty: WirType::I32,
        };
        let instr = match prim {
            PrimitiveType::I8 | PrimitiveType::U8 | PrimitiveType::Bool => WirInstr::I32Load8U {
                offset: 0,
                align: 0,
                addr: Box::new(ptr()),
            },
            PrimitiveType::I16 | PrimitiveType::U16 => WirInstr::I32Load {
                offset: 0,
                align: 1,
                addr: Box::new(ptr()),
            },
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char => WirInstr::I32Load {
                offset: 0,
                align: 2,
                addr: Box::new(ptr()),
            },
            PrimitiveType::I64 | PrimitiveType::U64 => WirInstr::I64Load {
                offset: 0,
                align: 3,
                addr: Box::new(ptr()),
            },
            _ => return None,
        };
        Some(instr)
    }

    /// Lift `Result<Option<R>, E>` from CM linear memory.
    ///
    /// CM layout at ptr:
    /// - offset 0: result discriminant (i32) — 0=Ok, 1=Err
    /// - offset 4: option discriminant (i32) — 0=None, 1=Some (when Ok)
    /// - offset 8: resource handle (i32) (when Ok+Some)
    ///
    /// Returns the constructed `Result` variant value.
    fn lift_result_option_resource(
        &mut self,
        result_type_id: TypeId,
        option_type_id: TypeId,
        _resource_type_id: TypeId,
        ptr_name: &str,
    ) -> WirInstr {
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.to_string(),
            result_ty: WirType::I32,
        };

        // Read result discriminant at offset 0
        let result_disc = WirInstr::I32Load {
            offset: 0,
            align: 2,
            addr: Box::new(ptr()),
        };

        // Build Ok branch: lift Option<R> from offset 4
        let option_disc = WirInstr::I32Load {
            offset: 4,
            align: 2,
            addr: Box::new(ptr()),
        };

        // Build Option::None (for the resource)
        let inner_none = self.build_variant_case_wir(option_type_id, 1, "None", None);

        // Build Option::Some(handle) — read handle at offset 8
        let handle_val = WirInstr::I32Load {
            offset: 8,
            align: 2,
            addr: Box::new(ptr()),
        };
        let inner_some = self.build_variant_case_wir(option_type_id, 0, "Some", Some(handle_val));

        // Option<R> = if option_disc == 0 { None } else { Some(handle) }
        let option_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, option_type_id);
        let lifted_option = WirInstr::If {
            condition: Box::new(WirInstr::I32Eqz(Box::new(option_disc))),
            result: Some(option_wir_type),
            then_body: vec![inner_none],
            else_body: Some(vec![inner_some]),
        };

        // Result::Ok(lifted_option)
        let result_ok = self.build_variant_case_wir(result_type_id, 0, "Ok", Some(lifted_option));

        // Result::Err — trap for now (ErrorCode lifting not yet implemented)
        let result_err = WirInstr::Unreachable;

        // Result = if result_disc == 0 { Ok(...) } else { Err (trap) }
        let result_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        WirInstr::If {
            condition: Box::new(WirInstr::I32Eqz(Box::new(result_disc))),
            result: Some(result_wir_type),
            then_body: vec![result_ok],
            else_body: Some(vec![result_err]),
        }
    }

    /// Lift `Result<(), E>` from CM linear memory.
    ///
    /// CM layout at ptr:
    /// - offset 0: result discriminant (i32) — 0=Ok, 1=Err
    ///
    /// Returns `Result::Ok(())` when discriminant is 0, traps on Err.
    fn lift_result_unit(&mut self, result_type_id: TypeId, ptr_name: &str) -> WirInstr {
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.to_string(),
            result_ty: WirType::I32,
        };

        let result_disc = WirInstr::I32Load {
            offset: 0,
            align: 2,
            addr: Box::new(ptr()),
        };

        let result_ok = self.build_variant_case_wir(result_type_id, 0, "Ok", None);

        // Err branch: trap (ErrorCode lifting not yet implemented for this pattern)
        let result_err = WirInstr::Unreachable;

        let result_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        WirInstr::If {
            condition: Box::new(WirInstr::I32Eqz(Box::new(result_disc))),
            result: Some(result_wir_type),
            then_body: vec![result_ok],
            else_body: Some(vec![result_err]),
        }
    }

    /// Emit WIR for `FutureWritable<T>::write(value)`.
    ///
    /// Dispatches to a type-specific emitter based on `value_type_id`:
    /// - Scalar numeric types (i8–i64, u8–u64, bool, char): `emit_future_write_scalar`
    /// - `Result<Option<R>, E>::Ok(null)` pattern: `emit_future_write_ok_none`
    fn emit_future_write(
        &mut self,
        handle: WirInstr,
        value: WirInstr,
        value_type_id: TypeId,
        payload: CmFuturePayload,
    ) -> WirInstr {
        // Check if the value type is a scalar numeric type
        if let ResolvedType::Primitive(
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64
            | PrimitiveType::Bool
            | PrimitiveType::Char,
        ) = self.type_table.get(value_type_id)
        {
            return self.emit_future_write_scalar(handle, value, value_type_id, payload);
        }

        // Fallback: Result<Option<R>, E>::Ok(null) pattern (trailers)
        self.emit_future_write_ok_none(handle)
    }

    /// Emit WIR to write a scalar numeric value into a `FutureWritable` handle.
    ///
    /// Allocates a CM buffer, stores the value, and calls `future-write` with
    /// `async` canonical option. If the operation returns BLOCKED, the buffer is
    /// intentionally kept alive — the reader will copy the data and complete
    /// the write on the same thread. The buffer is freed only on immediate
    /// completion (non-BLOCKED).
    fn emit_future_write_scalar(
        &mut self,
        handle: WirInstr,
        value: WirInstr,
        value_type_id: TypeId,
        payload: CmFuturePayload,
    ) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            return WirInstr::Unreachable;
        };
        let future_write_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::FutureWrite(payload),
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let ptr_name = format!("__fw_write_ptr_{suffix}");
        let result_name = format!("__fw_result_{suffix}");

        // Determine buffer size/alignment from the primitive type
        let ResolvedType::Primitive(prim) = self.type_table.get(value_type_id) else {
            return WirInstr::Unreachable;
        };
        let (buf_size, buf_align): (i32, i32) = match prim {
            PrimitiveType::I8 | PrimitiveType::U8 | PrimitiveType::Bool => (1, 1),
            PrimitiveType::I16 | PrimitiveType::U16 => (2, 2),
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char => (4, 4),
            PrimitiveType::I64 | PrimitiveType::U64 => (8, 8),
            _ => return WirInstr::Unreachable,
        };

        let mut seq = vec![];

        // Declare locals
        for (name, ty) in [(&ptr_name, WirType::I32), (&result_name, WirType::I32)] {
            seq.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        // Allocate buffer
        seq.push(WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: realloc_id,
                args: vec![
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(buf_align),
                    WirInstr::I32Const(buf_size),
                ],
            }),
        });

        // Store value at ptr
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.clone(),
            result_ty: WirType::I32,
        };
        let store_instr = match prim {
            PrimitiveType::I8 | PrimitiveType::U8 | PrimitiveType::Bool => WirInstr::I32Store8 {
                offset: 0,
                align: 0,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            PrimitiveType::I16 | PrimitiveType::U16 => WirInstr::I32Store16 {
                offset: 0,
                align: 1,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char => WirInstr::I32Store {
                offset: 0,
                align: 2,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            PrimitiveType::I64 | PrimitiveType::U64 => WirInstr::I64Store {
                offset: 0,
                align: 3,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            _ => return WirInstr::Unreachable,
        };
        seq.push(store_instr);

        // result = future_write(handle, ptr)
        // With `async` canonical option, BLOCKED means the data is pending and
        // the reader will pick it up. We drop the result — the buffer stays alive
        // until the reader copies the data on the same thread.
        seq.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: future_write_id,
            args: vec![
                handle,
                WirInstr::LocalGet {
                    name: ptr_name,
                    result_ty: WirType::I32,
                },
            ],
        })));

        // NOTE: We intentionally do NOT free the buffer here. When the canonical
        // `async` option returns BLOCKED, the CM runtime holds a reference to the
        // buffer. The reader will copy from it before this thread continues.
        // The buffer is leaked but this is acceptable for small scalar payloads.

        WirInstr::Seq(seq)
    }

    /// Emit the BLOCKED handling pattern for `future-write`.
    ///
    /// If `result == -1` (BLOCKED), creates a waitable-set, joins the handle,
    /// waits for the reader to consume the value, then frees the event buffer.
    fn emit_future_write_blocked_wait(
        &self,
        handle_name: &str,
        result_name: &str,
        evt_name: &str,
        realloc_id: &WirFuncId,
        ws_new_id: &WirFuncId,
        w_join_id: &WirFuncId,
        ws_wait_id: &WirFuncId,
    ) -> WirInstr {
        WirInstr::If {
            condition: Box::new(WirInstr::I32Eq(
                Box::new(WirInstr::LocalGet {
                    name: result_name.to_string(),
                    result_ty: WirType::I32,
                }),
                Box::new(WirInstr::I32Const(-1)),
            )),
            result: None,
            then_body: vec![
                WirInstr::DeclareLocal {
                    name: evt_name.to_string(),
                    ty: WirType::I32,
                },
                WirInstr::LocalSet {
                    name: result_name.to_string(),
                    value: Box::new(WirInstr::Call {
                        func_id: ws_new_id.clone(),
                        args: vec![],
                    }),
                },
                WirInstr::Call {
                    func_id: w_join_id.clone(),
                    args: vec![
                        WirInstr::LocalGet {
                            name: handle_name.to_string(),
                            result_ty: WirType::I32,
                        },
                        WirInstr::LocalGet {
                            name: result_name.to_string(),
                            result_ty: WirType::I32,
                        },
                    ],
                },
                WirInstr::LocalSet {
                    name: evt_name.to_string(),
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
                    func_id: ws_wait_id.clone(),
                    args: vec![
                        WirInstr::LocalGet {
                            name: result_name.to_string(),
                            result_ty: WirType::I32,
                        },
                        WirInstr::LocalGet {
                            name: evt_name.to_string(),
                            result_ty: WirType::I32,
                        },
                    ],
                })),
                WirInstr::Drop(Box::new(WirInstr::Call {
                    func_id: realloc_id.clone(),
                    args: vec![
                        WirInstr::LocalGet {
                            name: evt_name.to_string(),
                            result_ty: WirType::I32,
                        },
                        WirInstr::I32Const(8),
                        WirInstr::I32Const(4),
                        WirInstr::I32Const(0),
                    ],
                })),
            ],
            else_body: None,
        }
    }

    /// Emit WIR to write `Ok(None)` (8 zero bytes) into a `FutureWritable` handle,
    /// then free the temporary buffer.
    ///
    /// Hardcoded encoding for `Result<Option<Trailers>, ErrorCode>::Ok(null)`:
    ///   - 4 bytes at offset 0: Ok discriminant (0)
    ///   - 4 bytes at offset 4: None discriminant (0)
    ///
    /// If `future-write` returns BLOCKED (`0xFFFF_FFFF`), waits via waitable-set
    /// for the reader to consume the value before continuing.
    fn emit_future_write_ok_none(&mut self, handle: WirInstr) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            return WirInstr::Unreachable;
        };
        let future_write_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::FutureWrite(CmFuturePayload::Trailers),
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        let ws_new_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetNew,
            vec![],
            vec![WirType::I32],
        );
        let w_join_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableJoin,
            vec![WirType::I32, WirType::I32],
            vec![],
        );
        let ws_wait_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetWait,
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let ptr_name = format!("__fw_write_ptr_{suffix}");
        let handle_name = format!("__fw_handle_{suffix}");
        let result_name = format!("__fw_result_{suffix}");
        let evt_name = format!("__fw_evt_{suffix}");

        // The buffer must be large enough for the full CM layout of
        // result<option<own<trailers>>, error-code>. ErrorCode is a large variant
        // whose biggest cases contain option<string> (12 bytes) or dns-error-payload
        // (option<string> + option<u16> = 16 bytes). We allocate 40 bytes to cover
        // the worst case and zero-initialize the entire buffer, since Ok(None) is
        // represented as all-zeros.
        const BUF_SIZE: i32 = 40;
        const BUF_ALIGN: i32 = 8;

        let mut seq = vec![];

        // Declare locals
        for (name, ty) in [
            (&ptr_name, WirType::I32),
            (&handle_name, WirType::I32),
            (&result_name, WirType::I32),
        ] {
            seq.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        // Save handle
        seq.push(WirInstr::LocalSet {
            name: handle_name.clone(),
            value: Box::new(handle),
        });

        // Allocate buffer
        seq.push(WirInstr::LocalSet {
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
        });

        // Zero-initialize the entire buffer using i64 stores (8 bytes each).
        for i in 0..(BUF_SIZE / 8) {
            seq.push(WirInstr::I64Store {
                offset: u64::from((i * 8).cast_unsigned()),
                align: 3,
                addr: Box::new(WirInstr::LocalGet {
                    name: ptr_name.clone(),
                    result_ty: WirType::I32,
                }),
                value: Box::new(WirInstr::I64Const(0)),
            });
        }

        // result = future_write(handle, ptr)
        seq.push(WirInstr::LocalSet {
            name: result_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: future_write_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                        result_ty: WirType::I32,
                    },
                    WirInstr::LocalGet {
                        name: ptr_name.clone(),
                        result_ty: WirType::I32,
                    },
                ],
            }),
        });

        // If BLOCKED (0xFFFF_FFFF), wait via waitable-set for the reader to consume
        seq.push(self.emit_future_write_blocked_wait(
            &handle_name,
            &result_name,
            &evt_name,
            &realloc_id,
            &ws_new_id,
            &w_join_id,
            &ws_wait_id,
        ));

        // Free the payload buffer after future_write completes.
        seq.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet {
                    name: ptr_name,
                    result_ty: WirType::I32,
                },
                WirInstr::I32Const(BUF_SIZE),
                WirInstr::I32Const(BUF_ALIGN),
                WirInstr::I32Const(0),
            ],
        })));

        WirInstr::Seq(seq)
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
        let repr_result_ty = self.struct_field_wir_type(&array_struct_type, "repr");
        let raw_arr = WirInstr::StructGet {
            type_id: array_struct_type,
            field_name: "repr".to_string(),
            expr: Box::new(arr),
            result_ty: repr_result_ty,
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

        let elem_result_ty = self.array_element_wir_type(&raw_type);
        let get_instr = if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::Bool)
        ) {
            WirInstr::ArrayGetU {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                result_ty: elem_result_ty,
            }
        } else if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::I16)
        ) {
            WirInstr::ArrayGetS {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                result_ty: elem_result_ty,
            }
        } else {
            WirInstr::ArrayGet {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                result_ty: elem_result_ty,
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

            let repr_result_ty = self.struct_field_wir_type(&array_struct_type, "repr");
            let raw_arr = WirInstr::StructGet {
                type_id: array_struct_type,
                field_name: "repr".to_string(),
                expr: Box::new(arr),
                result_ty: repr_result_ty,
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

        // The br_table switch generates wrapper blocks around each arm body.
        // arm[i]'s body ends up inside (num_arms - i) wrapper blocks, and the
        // default body inside (num_arms + 1) blocks. We must push dummy label
        // entries so that break/continue inside arm bodies compute correct br
        // depths.

        // Translate default body (wrapped in num_arms + 1 blocks)
        let default_block_count = num_arms + 1;
        for _ in 0..default_block_count {
            self.label_stack.push(LabelEntry {
                label: None,
                is_loop_break: false,
                is_loop_continue: false,
            });
        }
        let default_body = if has_result {
            self.translate_stmts_as_value(&default.stmts)
        } else {
            self.translate_stmts(&default.stmts)
        };
        for _ in 0..default_block_count {
            self.label_stack.pop();
        }

        // Translate arm bodies (arm[i] wrapped in num_arms - i blocks)
        let arm_bodies: Vec<Vec<WirInstr>> = arms
            .iter()
            .enumerate()
            .map(|(i, arm)| {
                let block_count = num_arms - i;
                for _ in 0..block_count {
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                }
                let body = if has_result {
                    self.translate_stmts_as_value(&arm.stmts)
                } else {
                    self.translate_stmts(&arm.stmts)
                };
                for _ in 0..block_count {
                    self.label_stack.pop();
                }
                body
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

    /// Translate a `LetDestructure` (tuple destructuring) statement.
    /// Evaluates the tuple expression, stores it in a temp local,
    /// then binds each element to its pattern binding local.
    fn translate_let_pattern(&mut self, pattern: &TirPattern, value: &TirExpr) -> Option<WirInstr> {
        let value_instr = self.translate_expr(value);
        let value_instr = self.maybe_value_copy(value, value_instr);

        match pattern {
            TirPattern::Tuple(patterns, _) => {
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
                            let field_name_str = format!("{i}");
                            let field_result_ty =
                                self.struct_field_wir_type(type_id, &field_name_str);
                            instrs.push(WirInstr::LocalSet {
                                name: local_name,
                                value: Box::new(WirInstr::StructGet {
                                    type_id: type_id.clone(),
                                    field_name: field_name_str,
                                    expr: Box::new(WirInstr::LocalGet {
                                        name: temp_name.clone(),
                                        result_ty: wir_type.clone(),
                                    }),
                                    result_ty: field_result_ty,
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

        // Pre-compute the wasm If nesting depth for each arm body.
        // Each non-irrefutable arm generates a WirInstr::If (guarded arms generate 2).
        // The arm at source index s will be nested inside all Ifs from arms 0..s,
        // so we need to push dummy label entries to make break/continue compute
        // correct br depths.
        let mut if_depths = Vec::with_capacity(arms.len());
        {
            let mut depth = 0u32;
            for arm in arms {
                let is_irrefutable = matches!(
                    arm.pattern,
                    TirPattern::Wildcard
                        | TirPattern::Binding { .. }
                        | TirPattern::Struct { .. }
                        | TirPattern::Tuple(_, _)
                ) && arm.guard.is_none();
                // When the pattern is trivially true (Binding/Wildcard) and a guard
                // is present, we fold into a single If instead of nested 2-level If,
                // so only count 1 depth instead of 2.
                let pattern_trivially_true = matches!(
                    arm.pattern,
                    TirPattern::Wildcard | TirPattern::Binding { .. }
                );
                if !is_irrefutable {
                    depth += 1;
                    if arm.guard.is_some() && !pattern_trivially_true {
                        depth += 1; // guarded arms with non-trivial pattern create an extra inner If
                    }
                }
                if_depths.push(depth);
            }
        }

        for (reverse_idx, arm) in arms.iter().rev().enumerate() {
            let source_idx = arms.len() - 1 - reverse_idx;
            let if_nesting = if_depths[source_idx];

            let body_instrs = {
                // Push dummy label entries for the match's If nesting so that
                // break/continue inside arm bodies compute correct br depths.
                for _ in 0..if_nesting {
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                }
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
                    // values on the Wasm stack. Guard with `produces_stack_value()` to
                    // avoid emitting `drop` after instructions that produce no value
                    // (e.g. `Block{result: None}` from LabeledBlock fusion).
                    if arm.body.type_id != TypeTable::UNIT
                        && arm.body.type_id != TypeTable::NEVER
                        && instr.produces_stack_value()
                    {
                        WirInstr::Drop(Box::new(instr))
                    } else {
                        instr
                    }
                };
                instrs.push(body);
                // Note: `translate_expr` already appends `unreachable` for
                // `never`-typed arm bodies, so no extra push is needed here.
                for _ in 0..if_nesting {
                    self.label_stack.pop();
                }
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
                    | TirPattern::Tuple(_, _)
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
                //
                // Optimization: when the pattern condition is trivially true (i.e., the
                // pattern is irrefutable like Binding/Wildcard), fold the guard into a
                // single If to avoid cloning `result` (which causes 2^N tree explosion
                // for many guarded arms, e.g., string match with N branches).
                let pattern_is_trivially_true = matches!(&condition, WirInstr::I32Const(1));
                if pattern_is_trivially_true {
                    // Pattern always matches — just use the guard as the sole condition.
                    // Emit pattern bindings before the guard expression so that bound
                    // variables (e.g., `__lit_N`) are available when evaluating the guard
                    // (e.g., `__lit_N.eq("str")`). Since the pattern is irrefutable
                    // (Binding/Wildcard), these bindings are safe to emit unconditionally.
                    // We embed bindings into the condition via Seq so that the If is the
                    // top-level instruction (required for value-producing match expressions).
                    let mut bind_instrs = Vec::new();
                    self.emit_pattern_bindings(
                        &arm.pattern,
                        &scrut_local_name,
                        scrutinee.type_id,
                        &mut bind_instrs,
                    );
                    let guard_expr = self.translate_expr(guard);
                    let condition_with_bindings = if bind_instrs.is_empty() {
                        guard_expr
                    } else {
                        bind_instrs.push(guard_expr);
                        WirInstr::Seq(bind_instrs)
                    };
                    result = WirInstr::If {
                        condition: Box::new(condition_with_bindings),
                        result: result_wir_type.clone(),
                        then_body: body_instrs,
                        else_body: Some(vec![result]),
                    };
                } else {
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
                }
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
                    result_ty: self.wir_type(scrut_type),
                };
                self.translate_literal_pattern_condition(lit, scrut_get, scrut_type)
            }
            TirPattern::Enum { case_index, .. } => {
                // Enum: compare i32 discriminant
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
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
                    result_ty: self.wir_type(scrut_type),
                };

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
                                            result_ty: WirType::I32,
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
                                    // Case type not found (unit payload): fall back to discriminant check
                                    let wir_type =
                                        self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                                    if let WirType::Ref { type_id, .. } = wir_type {
                                        WirInstr::I32Eq(
                                            Box::new(WirInstr::StructGet {
                                                type_id,
                                                field_name: "discriminant".to_string(),
                                                expr: Box::new(scrut_get),
                                                result_ty: WirType::I32,
                                            }),
                                            Box::new(WirInstr::I32Const(case.index as i32)),
                                        )
                                    } else {
                                        WirInstr::I32Const(0)
                                    }
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
            TirPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => {
                let scrut_get = || WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
                };
                let is_i64 = matches!(
                    self.type_table.get(scrut_type),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                if is_i64 {
                    let start_const = WirInstr::I64Const(*start as i64);
                    let end_const = WirInstr::I64Const(*end as i64);
                    let ge = if *is_unsigned {
                        WirInstr::I64GeU(Box::new(scrut_get()), Box::new(start_const))
                    } else {
                        WirInstr::I64GeS(Box::new(scrut_get()), Box::new(start_const))
                    };
                    let upper = if *inclusive {
                        if *is_unsigned {
                            WirInstr::I64LeU(Box::new(scrut_get()), Box::new(end_const))
                        } else {
                            WirInstr::I64LeS(Box::new(scrut_get()), Box::new(end_const))
                        }
                    } else if *is_unsigned {
                        WirInstr::I64LtU(Box::new(scrut_get()), Box::new(end_const))
                    } else {
                        WirInstr::I64LtS(Box::new(scrut_get()), Box::new(end_const))
                    };
                    WirInstr::I32And(Box::new(ge), Box::new(upper))
                } else {
                    let start_const = WirInstr::I32Const(*start as i32);
                    let end_const = WirInstr::I32Const(*end as i32);
                    let ge = if *is_unsigned {
                        WirInstr::I32GeU(Box::new(scrut_get()), Box::new(start_const))
                    } else {
                        WirInstr::I32GeS(Box::new(scrut_get()), Box::new(start_const))
                    };
                    let upper = if *inclusive {
                        if *is_unsigned {
                            WirInstr::I32LeU(Box::new(scrut_get()), Box::new(end_const))
                        } else {
                            WirInstr::I32LeS(Box::new(scrut_get()), Box::new(end_const))
                        }
                    } else if *is_unsigned {
                        WirInstr::I32LtU(Box::new(scrut_get()), Box::new(end_const))
                    } else {
                        WirInstr::I32LtS(Box::new(scrut_get()), Box::new(end_const))
                    };
                    WirInstr::I32And(Box::new(ge), Box::new(upper))
                }
            }
            TirPattern::Tuple(_, _) | TirPattern::Struct { .. } => {
                // Tuple/struct patterns: always irrefutable
                WirInstr::I32Const(1)
            }
            TirPattern::Or(alternatives) => {
                // Or pattern: combine conditions with logical OR
                let mut result = WirInstr::I32Const(0);
                for alt in alternatives {
                    let cond = self.translate_pattern_condition(alt, scrut_local, scrut_type);
                    result = WirInstr::I32Or(Box::new(result), Box::new(cond));
                }
                result
            }
            TirPattern::ConstantValue { .. } => {
                panic!(
                    "ConstantValue pattern should have been lowered to binding + guard before WIR translation"
                );
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
                    panic!("string literal patterns should be lowered before WIR translation")
                }
            }
        }
    }

    /// Emit pattern bindings (local.set for bound variables).
    ///
    /// For or-patterns, conditionally extracts bindings from whichever alternative matched.
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
                        result_ty: self.wir_type(scrut_type),
                    }),
                });
            }
            TirPattern::Variant {
                variant_name,
                bindings,
                enum_type,
                payload_type,
            } => {
                if bindings.is_empty() {
                    return;
                }

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
                                result_ty: self.wir_type(scrut_type),
                            }),
                        }),
                    });
                    // Extract each payload binding via struct.get from the cast local
                    for (i, binding) in bindings.iter().enumerate() {
                        let payload_field_name = format!("payload_{i}");
                        let payload_result_ty =
                            self.struct_field_wir_type(&case_type_id, &payload_field_name);
                        let payload_get = WirInstr::StructGet {
                            type_id: case_type_id.clone(),
                            field_name: payload_field_name,
                            expr: Box::new(WirInstr::LocalGet {
                                name: cast_local.clone(),
                                result_ty: WirType::Ref {
                                    type_id: case_type_id.clone(),
                                    nullable: false,
                                },
                            }),
                            result_ty: payload_result_ty,
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
                            let needs_boxing = if let WirType::Ref {
                                type_id: binding_tid,
                                ..
                            } = &binding_wir
                            {
                                match payload_field_wir.as_ref() {
                                    Some(WirType::Ref {
                                        type_id: payload_tid,
                                        ..
                                    }) => {
                                        // Boxing needed if binding type differs from
                                        // payload type (e.g., binding expects Box<Inner>
                                        // but payload is ref Inner for variant payloads)
                                        binding_tid != payload_tid
                                    }
                                    Some(WirType::AbstractRef { .. }) => false,
                                    Some(_) => true,
                                    None => false,
                                }
                            } else {
                                false
                            };
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
                            // Skip local.set for unit-typed bindings (no Wasm local exists)
                            if !matches!(binding_wir, WirType::Unit) {
                                instrs.push(WirInstr::LocalSet {
                                    name: self.local_name(*local_index),
                                    value: Box::new(value),
                                });
                            }
                        } else if !matches!(
                            binding,
                            TirPattern::Wildcard
                                | TirPattern::Literal(_)
                                | TirPattern::Enum { .. }
                                | TirPattern::ConstantValue { .. }
                                | TirPattern::Range { .. }
                        ) {
                            // Compound sub-pattern (Tuple, Struct, Variant, Or):
                            // extract the payload into a temp local and recurse into
                            // emit_pattern_bindings to handle arbitrarily nested
                            // destructuring.
                            let payload_tid = *payload_type;
                            let payload_wir =
                                self.ctx.type_id_to_wir_type(self.type_table, payload_tid);
                            self.local_counter += 1;
                            let temp_name = format!("__variant_payload_{}", self.local_counter);
                            instrs.push(WirInstr::DeclareLocal {
                                name: temp_name.clone(),
                                ty: payload_wir,
                            });
                            instrs.push(WirInstr::LocalSet {
                                name: temp_name.clone(),
                                value: Box::new(payload_get),
                            });
                            self.emit_pattern_bindings(binding, &temp_name, payload_tid, instrs);
                        }
                    }
                } else {
                    // Fallback: just copy the scrutinee (won't be type-correct for payload)
                    for binding in bindings {
                        if let TirPattern::Binding {
                            local_index,
                            type_id,
                            ..
                        } = binding
                        {
                            let wir = self.ctx.type_id_to_wir_type(self.type_table, *type_id);
                            // Skip unit-typed bindings (no Wasm local exists for unit)
                            if !matches!(wir, WirType::Unit) {
                                instrs.push(WirInstr::LocalSet {
                                    name: self.local_name(*local_index),
                                    value: Box::new(WirInstr::LocalGet {
                                        name: scrut_local.to_string(),
                                        result_ty: self.wir_type(scrut_type),
                                    }),
                                });
                            }
                        }
                    }
                }
            }
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {
                // No bindings needed
            }
            TirPattern::Tuple(sub_patterns, _) => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                if let WirType::Ref { ref type_id, .. } = wir_type {
                    let element_types = self.type_table.as_tuple(scrut_type).unwrap_or_default();
                    for (i, sub_pattern) in sub_patterns.iter().enumerate() {
                        let field_name_str = format!("{i}");
                        let field_result_ty = self.struct_field_wir_type(type_id, &field_name_str);
                        let field_get = WirInstr::StructGet {
                            type_id: type_id.clone(),
                            field_name: field_name_str,
                            expr: Box::new(WirInstr::LocalGet {
                                name: scrut_local.to_string(),
                                result_ty: wir_type.clone(),
                            }),
                            result_ty: field_result_ty,
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
                if let WirType::Ref { ref type_id, .. } = wir_type {
                    for field in fields {
                        let field_result_ty =
                            self.struct_field_wir_type(type_id, &field.field_name);
                        let field_get = WirInstr::StructGet {
                            type_id: type_id.clone(),
                            field_name: field.field_name.clone(),
                            expr: Box::new(WirInstr::LocalGet {
                                name: scrut_local.to_string(),
                                result_ty: wir_type.clone(),
                            }),
                            result_ty: field_result_ty,
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
            TirPattern::Or(alternatives) => {
                // Or patterns: emit bindings for each alternative, guarded by its condition.
                // For alternatives with only wildcards (no real bindings), skip entirely.
                let has_any_bindings = alternatives.iter().any(pattern_has_bindings);
                if !has_any_bindings {
                    return;
                }
                // Emit conditional binding extraction: check each alternative and
                // emit bindings from the one that matches.
                // Build a nested if-else chain from the inside out.
                let mut result: Option<WirInstr> = None;
                for alt in alternatives.iter().rev() {
                    let cond = self.translate_pattern_condition(alt, scrut_local, scrut_type);
                    let mut body = Vec::new();
                    self.emit_pattern_bindings(alt, scrut_local, scrut_type, &mut body);
                    let else_body = result.map(|r| vec![r]);
                    result = Some(WirInstr::If {
                        condition: Box::new(cond),
                        result: None,
                        then_body: body,
                        else_body,
                    });
                }
                if let Some(if_instr) = result {
                    instrs.push(if_instr);
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

    /// Resolve the `TypeId` of a struct field by name.
    fn resolve_struct_field_type(&self, struct_type: TypeId, field_name: &str) -> TypeId {
        if let ResolvedType::Struct {
            name,
            module_source,
            ..
        } = self.type_table.get(struct_type)
        {
            for s in &self.ctx.package.structs {
                if s.module_source == *module_source && s.name == *name {
                    for f in &s.fields {
                        if f.name == field_name {
                            return f.type_id;
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

        let fq = format!("{variant_module_source}//{variant_name}");

        // Look up case-specific struct type
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

    /// Build a variant case value directly from WIR instructions (no TIR needed).
    ///
    /// Used by canonical method synthesis to construct `Option::Some/None` and similar
    /// variant values without going through TIR expression translation.
    fn build_variant_case_wir(
        &self,
        variant_type_id: TypeId,
        case_index: u32,
        case_name: &str,
        payload: Option<WirInstr>,
    ) -> WirInstr {
        let (variant_name, variant_module_source) = match self.type_table.get(variant_type_id) {
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

        let fq = format!("{variant_module_source}//{variant_name}");
        let case_fq = format!("{fq}::{case_name}");

        if let Some(case_type_id) = self.ctx.type_map.get(&case_fq).cloned() {
            let mut fields = vec![WirInstr::I32Const(case_index as i32)];
            if let Some(payload_instr) = payload {
                fields.push(payload_instr);
            }
            self.struct_new(case_type_id, fields)
        } else {
            let wir_type = self
                .ctx
                .type_id_to_wir_type(self.type_table, variant_type_id);
            if let WirType::Ref { type_id, .. } = wir_type {
                let mut fields = vec![WirInstr::I32Const(case_index as i32)];
                if let Some(payload_instr) = payload {
                    fields.push(payload_instr);
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
                            result_ty: WirType::I32,
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
                            result_ty: WirType::I32,
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
                    result_ty: WirType::I32,
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

        // Look up variant type info
        if let Some(variant_type_id) = self.ctx.type_map.get(&fq)
            && let crate::wir::WirTypeDef::Variant(vt) =
                &self.ctx.types[variant_type_id.index() as usize]
        {
            // ref.cast to the case struct, then struct.get the payload field
            if let Some(case) = vt.cases.get(case_index as usize) {
                let case_fq = format!("{fq}::{}", case.name);
                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq) {
                    let cast = WirInstr::RefCast {
                        type_id: case_type_id.clone(),
                        nullable: false,
                        expr: Box::new(val),
                    };
                    let payload_result_ty = self.struct_field_wir_type(case_type_id, "payload_0");
                    return WirInstr::StructGet {
                        type_id: case_type_id.clone(),
                        field_name: "payload_0".to_string(),
                        expr: Box::new(cast),
                        result_ty: payload_result_ty,
                    };
                }
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
                ty: callee_ref_type.clone(),
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
        let env_result_ty = self.struct_field_wir_type(&closure_struct_type_id, "env");
        let env_arg = WirInstr::StructGet {
            type_id: closure_struct_type_id.clone(),
            field_name: "env".to_string(),
            expr: Box::new(WirInstr::LocalGet {
                name: temp_name.clone(),
                result_ty: callee_ref_type.clone(),
            }),
            result_ty: env_result_ty,
        };

        let mut call_args = vec![env_arg];
        for arg in args {
            let translated = self.translate_expr(arg);
            call_args.push(self.maybe_value_copy(arg, translated));
        }

        // func_ref = struct.get $closure "func"
        let func_result_ty = self.struct_field_wir_type(&closure_struct_type_id, "func");
        let func_ref = WirInstr::StructGet {
            type_id: closure_struct_type_id,
            field_name: "func".to_string(),
            expr: Box::new(WirInstr::LocalGet {
                name: temp_name,
                result_ty: callee_ref_type,
            }),
            result_ty: func_result_ty,
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
        closure_module: &ModuleSource,
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

        // Look up the pre-registered wrapper function for this functor.
        // Use closure_module (the module where the closure was defined) for the lookup,
        // not self.module_source (which may differ after cross-module inlining).
        let functor_key = (closure_module.clone(), functor_id);
        let wrapper_func_id = if let Some(id) = self.ctx.closure_wrapper_funcs.get(&functor_key) {
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

fn pattern_has_bindings(pattern: &TirPattern) -> bool {
    match pattern {
        TirPattern::Binding { .. } => true,
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => false,
        TirPattern::Variant { bindings, .. } => bindings.iter().any(pattern_has_bindings),
        TirPattern::Tuple(subs, _) => subs.iter().any(pattern_has_bindings),
        TirPattern::Struct { fields, .. } => {
            fields.iter().any(|f| pattern_has_bindings(&f.pattern))
        }
        TirPattern::Or(alts) => alts.iter().any(pattern_has_bindings),
    }
}
