//! Function body translation — converts TIR expressions and statements to WIR instructions.
//!
//! This is the core of the `tir_to_wir` phase, translating each TIR function body
//! into a sequence of WIR instructions.

use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt,
    TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::wir::{CanonicalIntrinsic, WirInstr, WirName, WirType, WirTypeDef, WirTypeId};

use super::context::WirContext;

/// Recursively collect variable names from Let statements.
///
/// These names are gathered eagerly from the statement tree and preferred
/// when present; any missing entries are then backfilled from
/// `tir_func.locals[idx].name` (for example, slots created in expression
/// contexts the walker doesn't recurse into, or by optimizer passes that
/// allocate locals without emitting a `Let`).
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
            // Build local-name map: params first, then `Let` statement
            // names (which carry the most descriptive identifiers — `?`
            // temps, hoisted-buf names, and so on). `tir_func.locals`
            // backfills entries that no `Let` shadows, covering parameter
            // slots without a body Let, slots created in expression
            // contexts the walker doesn't recurse into, and pre-lower
            // function bodies that haven't been desugared yet.
            let mut local_names = IndexMap::default();
            for param in &tir_func.params {
                local_names.insert(param.local_index, param.name.clone());
            }
            collect_let_names(&mut local_names, &body.stmts);
            for (idx, local) in tir_func.locals.iter().enumerate() {
                let key = u32::try_from(idx).unwrap();
                local_names.entry(key).or_insert_with(|| local.name.clone());
            }

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
pub(super) struct LabelEntry {
    /// Label name from TIR (for labeled blocks).
    pub(super) label: Option<String>,
    /// True if this is the outer block wrapping a loop (target for unlabeled break).
    pub(super) is_loop_break: bool,
    /// True if this is a loop instruction (target for continue).
    pub(super) is_loop_continue: bool,
}

/// Translator state for a single function.
pub(super) struct FunctionTranslator<'a, 'b> {
    pub(super) ctx: &'a mut WirContext<'b>,
    pub(super) type_table: &'a TypeTable,
    pub(super) tir_func: &'a TirFunction,
    /// Stack of Wasm block scopes for computing br depths.
    pub(super) label_stack: Vec<LabelEntry>,
    /// Counter for generating unique match scrutinee local names.
    pub(super) match_counter: u32,
    /// Counter for generating unique temporary local names.
    pub(super) local_counter: u32,
    /// Map from local index to variable name (built from params + Let stmts).
    pub(super) local_names: IndexMap<u32, String>,
    /// Set of local indices declared as immutable (`let`, not `let mut`).
    /// Used to skip unnecessary value copies when an immutable binding
    /// is initialized from another immutable local.
    pub(super) immutable_locals: IndexSet<u32>,
}

impl FunctionTranslator<'_, '_> {
    /// Get the WIR local name for a given local index.
    /// Uses the TIR variable name if available, otherwise falls back to `__local_N`.
    ///
    /// WIR locals are looked up by name during codegen (`current_locals` is
    /// keyed by name in `codegen::emit::resolve_local`), so any two locals
    /// that share a name would clobber each other's entry and silently
    /// mis-resolve. The disambiguation rules here mirror
    /// `wir_build::functions`'s construction of `WirFunction::param_names`:
    ///
    /// - When two params share a name (e.g. a synthesised closure's
    ///   implicit `self: &__Closure` env collides with an explicit
    ///   `self`-named param forwarded from a source method), every such
    ///   param's name is suffixed with `_{local_index}`.
    /// - A non-param local that shadows a param keeps the original
    ///   collision-resolution shape: the param keeps its raw name and the
    ///   non-param gets the `_{index}` suffix. This avoids renaming params
    ///   just because a `let self = ...` happens to shadow them in the
    ///   body.
    pub(super) fn local_name(&self, index: u32) -> String {
        let Some(name) = self.local_names.get(&index) else {
            return format!("__local_{index}");
        };

        let is_param = self.tir_func.params.iter().any(|p| p.local_index == index);
        if is_param {
            // Duplicate PARAM names are unambiguous in TIR (each carries its
            // local_index) but share a single bucket in WIR's name-keyed
            // `current_locals`, so suffix every collision with the index.
            let param_count = self
                .tir_func
                .params
                .iter()
                .filter(|p| {
                    self.local_names
                        .get(&p.local_index)
                        .is_some_and(|n| n == name)
                })
                .count();
            if param_count > 1 {
                return format!("{name}_{index}");
            }
            return name.clone();
        }

        // Non-param: shadow a param-or-let by suffixing the non-param.
        let total = self.local_names.values().filter(|n| *n == name).count();
        if total > 1 {
            format!("{name}_{index}")
        } else {
            name.clone()
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
        } else if !self.tir_func.locals.is_empty() {
            // `locals` is indexed absolutely (entries 0..param_count are
            // params, entries param_count.. are non-param locals), matching
            // DeclareLocal generation.
            if let Some(local) = self.tir_func.locals.get(index as usize) {
                self.wir_type(local.type_id)
            } else {
                WirType::I32
            }
        } else {
            WirType::I32
        }
    }

    /// Shorthand for `self.ctx.type_id_to_wir_type(self.type_table, type_id)`.
    pub(super) fn wir_type(&self, type_id: TypeId) -> WirType {
        self.ctx.type_id_to_wir_type(self.type_table, type_id)
    }

    /// Look up the WIR type of a struct field.
    pub(super) fn struct_field_wir_type(
        &self,
        struct_type_id: &WirTypeId,
        field_name: &str,
    ) -> WirType {
        if let Some(WirTypeDef::Struct(st)) = self.ctx.types.get(struct_type_id.index() as usize)
            && let Some(f) = st.fields.iter().find(|f| f.name == field_name)
        {
            return f.ty.clone();
        }
        WirType::I32
    }

    /// Look up the element WIR type of an array type.
    pub(super) fn array_element_wir_type(&self, array_type_id: &WirTypeId) -> WirType {
        if let Some(WirTypeDef::Array(at)) = self.ctx.types.get(array_type_id.index() as usize) {
            return at.element_type.clone();
        }
        WirType::I32
    }

    /// Build a `StructNew` instruction, wrapping each field value with `RefAsNonNull`
    /// where the struct definition declares a non-nullable reference field.
    pub(super) fn struct_new(&self, type_id: WirTypeId, fields: Vec<WirInstr>) -> WirInstr {
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
        // `locals` may only contain body locals (not params) or it may be empty
        // for functions that haven't been through the lower phase's local allocation.
        // Fall back to scanning Let statements to discover locals.
        let param_count = self.tir_func.params.len();
        if self.tir_func.locals.is_empty() {
            // Scan block for Let declarations to discover local types
            self.declare_locals_from_stmts(&mut instrs, &block.stmts);
        } else {
            for (i, local) in self.tir_func.locals.iter().enumerate() {
                // Skip entries that correspond to params (they're already declared)
                if i < param_count {
                    continue;
                }
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, local.type_id);
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
    pub(super) fn translate_stmts(&mut self, stmts: &[TirStmt]) -> Vec<WirInstr> {
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
    pub(super) fn translate_stmts_as_value(&mut self, stmts: &[TirStmt]) -> Vec<WirInstr> {
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
                // In a value-producing block, a final statement that does not push
                // a value to the Wasm stack means the enclosing typed block must be
                // exited exclusively via labeled `break`/`return` from inside the
                // statement.  The normal fall-through at the block's `end` is therefore
                // unreachable; append `unreachable` so the Wasm validator accepts the
                // typed block even when it has no stack value at the `end`.
                //
                // This covers all non-value-producing last statements:
                //  - explicit divergence (Return, Br, BrTable, Unreachable)
                //  - void blocks / loops whose only exits are outer labeled breaks
                //    (e.g. a TIR `loop {}` translated to `Block{result:None,[Loop{…}]}`)
                //  - any other void WIR instruction that should never reach this point
                //    in well-typed TIR
                let needs_unreachable = is_last && !instr.produces_stack_value();
                instrs.push(instr);
                if needs_unreachable {
                    instrs.push(WirInstr::Unreachable);
                }
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
    pub(super) fn translate_expr_as_value(&mut self, expr: &TirExpr) -> WirInstr {
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
                ..
            } => {
                // `immutable_locals` used to feed the WIR-level `is_source_immutable`
                // shortcut; keep the tracking for the residual reader
                // (`wir_build::value_copy::build_value_copy` no longer needs it but
                // removing the field is follow-up cleanup).
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
                    // Value-copy wrappers are materialized at the TIR level by
                    // `lower::value_copy`; the translation here is a plain
                    // LocalSet. `skip_value_copy` is still respected upstream
                    // (the inserter leaves the value unwrapped).
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
    pub(super) fn translate_expr(&mut self, expr: &TirExpr) -> WirInstr {
        let instr = self.translate_expr_inner(expr);
        if expr.type_id == TypeTable::NEVER {
            WirInstr::Seq(vec![instr, WirInstr::Unreachable])
        } else {
            instr
        }
    }

    fn translate_expr_inner(&mut self, expr: &TirExpr) -> WirInstr {
        match &expr.kind {
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
                    if matches!(self.type_table.get(inner), ResolvedType::Unknown) {
                        panic!(
                            "[WIR] Null with unresolved Option inner type (type_id={:?})",
                            expr.type_id
                        );
                    }
                    self.translate_variant_construct(
                        expr.type_id, // variant_type
                        1,            // case_index: None is case 1
                        "None",
                        None, // no payload
                        expr.type_id,
                    )
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

            TirExprKind::Local { index, .. } => {
                // Unit and Never locals have no Wasm representation. For Unit
                // there is nothing to push. For Never the local declaration
                // was skipped (its initializer diverges); the surrounding
                // `translate_expr` wrapper appends `Unreachable` so the local
                // value never materializes — emit a placeholder `Nop`.
                if expr.type_id == TypeTable::UNIT || expr.type_id == TypeTable::NEVER {
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
                    .map(|a| self.translate_expr(&a.expr))
                    .collect();

                if let Some(func_id) = self.resolve_function_ref(func) {
                    WirInstr::Call {
                        func_id,
                        args: translated_args,
                    }
                } else {
                    panic!(
                        "[WIR] unresolved Call: name={:?} builtin={:?}",
                        func.name.clone(),
                        builtin
                    );
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
                        translated_args.push(self.translate_expr(&arg.expr));
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

            TirExprKind::StructLiteral { fields, .. } => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, expr.type_id);
                let WirType::Ref { type_id, .. } = wir_type else {
                    panic!(
                        "[WIR] StructLiteral expected Ref WirType, got {wir_type:?} (type_id={:?})",
                        expr.type_id
                    );
                };
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
            }

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
                let WirType::Ref { type_id, .. } = wir_type else {
                    panic!(
                        "[WIR] FieldAccess receiver expected Ref WirType, got {wir_type:?} (field={field_name}, type_id={:?})",
                        receiver.type_id
                    );
                };
                let result_ty = self.struct_field_wir_type(&type_id, field_name);
                WirInstr::StructGet {
                    type_id,
                    field_name: field_name.clone(),
                    expr: Box::new(recv),
                    result_ty,
                }
            }

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
                        // Value-copy wrappers for Assign targets are inserted by the
                        // TIR `lower::value_copy` pass; no WIR-level wrapping here.
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
                        let WirType::Ref { type_id, .. } = wir_type else {
                            panic!(
                                "[WIR] FieldAccess assignment expected Ref receiver, got {wir_type:?} (field={field_name}, type_id={:?})",
                                receiver.type_id
                            );
                        };
                        self.struct_set(type_id, field_name.clone(), recv, val)
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

            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                // Type casts become appropriate conversion instructions
                self.translate_cast(inner, inner.type_id, *target_type)
            }

            TirExprKind::Block(block) => {
                let body = if expr.type_id == TypeTable::UNIT {
                    self.translate_stmts(&block.stmts)
                } else {
                    self.translate_stmts_as_value(&block.stmts)
                };
                WirInstr::Seq(body)
            }

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

            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => self.translate_match(scrutinee, arms, expr.type_id),

            TirExprKind::Index {
                expr: array_expr,
                index: index_expr,
            } => self.translate_index(array_expr, index_expr),

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
                let Some(type_id) = wir_type_id else {
                    panic!(
                        "[WIR] TupleLiteral could not resolve a tuple struct type (expr type_id={:?}, elements={})",
                        expr.type_id,
                        elements.len()
                    );
                };
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
            }

            TirExprKind::TupleSpread { .. }
            | TirExprKind::TupleZip { .. }
            | TirExprKind::TypePackExpansion { .. } => {
                panic!(
                    "TupleSpread/TupleZip/TypePackExpansion should have been expanded during monomorphization"
                )
            }

            TirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => self.translate_switch(scrutinee, *min_value, arms, default, expr.type_id),

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

            TirExprKind::Closure { .. } => {
                panic!(
                    "[WIR] Closure should be lowered to StructLiteral or ClosureToCanonical before WIR build"
                );
            }
            TirExprKind::Capture { .. } => {
                panic!("[WIR] Capture should be lowered to FieldAccess before WIR build");
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

            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }

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
}
