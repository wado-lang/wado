use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::name::{ModuleSource, mangle_generic_name};
use crate::tir::{
    MonomorphInfo, PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirField,
    TirFunction, TirModule, TirStmt, TirStmtKind, TirStruct, TirStructField, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::token::Span;

pub(super) struct BoxLowerer {
    /// Mapping from inner `TypeId` to Box<T> struct type ID.
    /// e.g., `TypeTable::I32` → `TypeId` for Struct("Box<i32>")
    box_struct_types: IndexMap<TypeId, TypeId>,
    /// Set of all Box<T> struct type IDs (for fast lookup).
    box_type_ids: IndexSet<TypeId>,
    /// Generated Box<T> struct definitions to add to the module.
    pub(super) generated_structs: Vec<TirStruct>,
    /// Module source for registering Box types in the type table.
    /// Set from `TypeTable::box_module_source` (registered via `#[comp_feature("box")]`
    /// on `struct Box<T>` in the prelude).
    box_module_source: ModuleSource,
    /// Struct fields indexed by (name, `module_source`) for deref assign expansion.
    pub(super) struct_fields_map: IndexMap<(String, ModuleSource), Vec<TirField>>,
    /// Variant names from all modules, used to identify `GenericInstance` types
    /// that are variants and need boxing.
    pub(super) variant_names: IndexSet<String>,
}

impl BoxLowerer {
    pub(super) fn new(box_module_source: ModuleSource) -> Self {
        Self {
            box_struct_types: IndexMap::default(),
            box_type_ids: IndexSet::default(),
            generated_structs: Vec::new(),
            box_module_source,
            struct_fields_map: IndexMap::default(),
            variant_names: IndexSet::default(),
        }
    }

    /// Get or create a Box<T> struct type for the given inner type.
    fn get_or_create_box_type(
        &mut self,
        inner_type_id: TypeId,
        type_table: &mut TypeTable,
    ) -> TypeId {
        if let Some(&box_type) = self.box_struct_types.get(&inner_type_id) {
            return box_type;
        }

        // Create the Box struct type name: e.g., "Box<i32>"
        let inner_name = type_table.mangle_type_name(inner_type_id);
        let struct_name = mangle_generic_name("Box", &[inner_name]);

        // Register under the Box definition's module source (from #[comp_feature("box")]).
        let struct_type_id = type_table.make_monomorphized_struct(
            struct_name.clone(),
            self.box_module_source.clone(),
            "Box".to_string(),
        );

        // Create the TirStruct definition with a single `value` field
        let tir_struct = TirStruct {
            name: struct_name,
            module_source: self.box_module_source.clone(),
            is_pub: true,
            type_params: Vec::new(),
            monomorph_info: Some(MonomorphInfo {
                generic_name: "Box".to_string(),
                impl_type_args: vec![inner_type_id],
                method_type_args: vec![],
                is_blanket: false,
            }),
            fields: vec![TirField {
                name: "value".to_string(),
                is_pub: false,
                type_id: inner_type_id,
                index: 0,
                span: Span::new(0, 0, 0, 0),
                is_hidden: false,
                serde_rename: None,
                serde_default: false,
                default_expr: None,
            }],
            span: Span::new(0, 0, 0, 0),
            serde_rename_all: None,
        };

        self.generated_structs.push(tir_struct);
        self.box_struct_types.insert(inner_type_id, struct_type_id);
        self.box_type_ids.insert(struct_type_id);

        struct_type_id
    }

    /// Get the inner (value) `TypeId` for a Box struct type, if it is one.
    fn get_box_inner_type(&self, type_id: TypeId) -> Option<TypeId> {
        for (&inner, &box_type) in &self.box_struct_types {
            if box_type == type_id {
                return Some(inner);
            }
        }
        None
    }

    /// Check if a type is a variant (either directly or as a `GenericInstance` of a variant).
    fn is_variant_type(&self, type_id: TypeId, type_table: &TypeTable) -> bool {
        match type_table.get(type_id) {
            ResolvedType::Variant { .. } => true,
            ResolvedType::GenericInstance { name, .. } => {
                self.variant_names.contains(name.as_str())
            }
            _ => false,
        }
    }

    /// Look up struct fields for a given `TypeId` via the type table.
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

    /// Expand `*ref = value` for non-box struct types into field-by-field assignments.
    ///
    /// After `transform_block`, any remaining `Assign { target: Deref(..) }` nodes
    /// are for non-primitive types (structs, String). This pass expands them into:
    ///   let __`deref_ref_N` = `ref_expr`;
    ///   let __`deref_val_N` = `value_expr`;
    ///   __`deref_ref_N.field0` = __`deref_val_N.field0`;
    ///   __`deref_ref_N.field1` = __`deref_val_N.field1`;
    ///   ...
    fn expand_deref_assigns_in_block(
        &self,
        block: &mut TirBlock,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
        type_table: &TypeTable,
    ) {
        let mut new_stmts: Vec<TirStmt> = Vec::with_capacity(block.stmts.len());

        for stmt in std::mem::take(&mut block.stmts) {
            // Recurse into nested blocks first
            new_stmts.push(stmt);
            let stmt = new_stmts.last_mut().unwrap();
            self.expand_deref_assigns_in_stmt(stmt, local_count, local_types, type_table);

            // Check if this stmt is Expr(Assign { target: Deref(..), value })
            let should_expand = matches!(
                &stmt.kind,
                TirStmtKind::Expr(expr) if matches!(
                    &expr.kind,
                    TirExprKind::Assign { target, .. }
                    if matches!(&target.kind, TirExprKind::Unary { op: TirUnaryOp::Deref, .. })
                )
            );

            if !should_expand {
                continue;
            }

            // Extract the assign components and save type info before moves
            let TirStmtKind::Expr(expr) = &mut stmt.kind else {
                continue;
            };
            let TirExprKind::Assign { target, value } = &mut expr.kind else {
                continue;
            };
            let TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: ref_expr,
            } = &mut target.kind
            else {
                continue;
            };

            // Determine the inner struct type from the ref type
            let inner_type_id = match type_table.get(ref_expr.type_id) {
                ResolvedType::MutRef(inner) => *inner,
                // Ref should have been caught by the immutable check
                _ => continue,
            };

            // Look up struct fields
            let Some(fields) = self.get_struct_fields(inner_type_id, type_table) else {
                continue;
            };

            if fields.is_empty() {
                continue;
            }

            // Save type IDs before destructive moves
            let ref_type_id = ref_expr.type_id;
            let span = expr.span;

            // Allocate temp locals
            let ref_local_idx = *local_count;
            *local_count += 1;
            local_types.push(ref_type_id);

            let val_local_idx = *local_count;
            *local_count += 1;
            local_types.push(inner_type_id);

            // Take ownership of ref_expr and value
            let ref_owned = std::mem::replace(
                ref_expr.as_mut(),
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
            );
            let val_owned = std::mem::replace(
                value.as_mut(),
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
            );

            // Remove the original stmt (we just pushed it) and replace with expansion
            new_stmts.pop();

            // let __deref_ref = ref_expr
            new_stmts.push(TirStmt {
                kind: TirStmtKind::Let {
                    local_index: ref_local_idx,
                    name: format!("__deref_ref_{ref_local_idx}"),
                    type_id: ref_type_id,
                    is_mut: false,
                    is_reactive: false,
                    value: ref_owned,
                    skip_value_copy: false,
                },
                span,
            });

            // let __deref_val = value_expr
            new_stmts.push(TirStmt {
                kind: TirStmtKind::Let {
                    local_index: val_local_idx,
                    name: format!("__deref_val_{val_local_idx}"),
                    type_id: inner_type_id,
                    is_mut: false,
                    is_reactive: false,
                    value: val_owned,
                    skip_value_copy: false,
                },
                span,
            });

            // For each field: __deref_ref.field_i = __deref_val.field_i
            for field in &fields {
                let ref_local = TirExpr::new(
                    TirExprKind::Local {
                        index: ref_local_idx,
                        name: format!("__deref_ref_{ref_local_idx}"),
                    },
                    ref_type_id,
                    span,
                );
                let val_local = TirExpr::new(
                    TirExprKind::Local {
                        index: val_local_idx,
                        name: format!("__deref_val_{val_local_idx}"),
                    },
                    inner_type_id,
                    span,
                );

                let assign_target = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(ref_local),
                        field_index: field.index,
                        field_name: field.name.clone(),
                    },
                    field.type_id,
                    span,
                );
                let assign_value = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(val_local),
                        field_index: field.index,
                        field_name: field.name.clone(),
                    },
                    field.type_id,
                    span,
                );

                new_stmts.push(TirStmt {
                    kind: TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Assign {
                            target: Box::new(assign_target),
                            value: Box::new(assign_value),
                        },
                        field.type_id,
                        span,
                    )),
                    span,
                });
            }
        }

        block.stmts = new_stmts;
    }

    /// Recurse into nested blocks within a statement for deref assign expansion.
    fn expand_deref_assigns_in_stmt(
        &self,
        stmt: &mut TirStmt,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
        type_table: &TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.expand_deref_assigns_in_block(
                    then_block,
                    local_count,
                    local_types,
                    type_table,
                );
                if let Some(else_block) = else_block {
                    self.expand_deref_assigns_in_block(
                        else_block,
                        local_count,
                        local_types,
                        type_table,
                    );
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.expand_deref_assigns_in_block(body, local_count, local_types, type_table);
            }
            TirStmtKind::IfLet {
                then_block,
                else_block,
                ..
            } => {
                self.expand_deref_assigns_in_block(
                    then_block,
                    local_count,
                    local_types,
                    type_table,
                );
                if let Some(else_block) = else_block {
                    self.expand_deref_assigns_in_block(
                        else_block,
                        local_count,
                        local_types,
                        type_table,
                    );
                }
            }
            _ => {}
        }
    }

    /// Transform expressions in a module (called after type table setup).
    ///
    /// This is the per-module phase: transforms function bodies, impl methods,
    /// and global initializers. Also injects generated Box structs into the module.
    pub(super) fn lower_module_exprs(&mut self, module: &mut TirModule) {
        // Transform expressions in all functions.
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            self.transform_function(&mut func, &module.type_table);
        }

        // Transform impl method bodies
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                self.transform_function(method, &module.type_table);
            }
        }

        // Transform global initializers
        {
            let type_table = module.type_table.borrow();
            for global in &mut module.globals {
                self.transform_expr(&mut global.initializer, &IndexSet::default(), &type_table);
            }
        }

        // Box structs are injected into the module separately
        // (see lower::lower)
    }

    /// Scan the type table to find which primitives need Box types.
    pub(super) fn create_needed_box_types(&mut self, type_table: &mut TypeTable) {
        // Collect base TypeIds that need boxing, plus newtypes.
        // Boxing is required for:
        // - Primitives (except i128/u128 which are already GC types)
        // - Variant types (subtype hierarchy prevents field-by-field deref assignment)
        let mut needs_box_base: IndexSet<TypeId> = IndexSet::default();

        for type_id in type_table.iter_type_ids().collect::<Vec<_>>() {
            match type_table.get(type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    let is_prim = matches!(type_table.get(inner), ResolvedType::Primitive(p)
                        if !matches!(p, PrimitiveType::I128 | PrimitiveType::U128));
                    let is_variant = self.is_variant_type(inner, type_table);
                    let needs_box = is_prim || is_variant;
                    if needs_box {
                        needs_box_base.insert(inner);
                    }
                }
                _ => {}
            }
        }

        // Create Box<T> struct types for each type that needs boxing
        for base_type_id in needs_box_base {
            self.get_or_create_box_type(base_type_id, type_table);
        }
    }

    /// Rewrite type table entries: Ref(primitive) → Box struct, MutRef(primitive) → Box struct.
    ///
    /// Note: Option(primitive) is NOT rewritten here. The type table keeps `Option(primitive)`
    /// so that codegen and pattern matching can still see the original inner type. The lower
    /// pass transforms variant expressions (`VariantConstruct`) to wrap/unwrap Box structs,
    /// while codegen handles the type mapping from `Option(primitive)` to a nullable Box reference.
    pub(super) fn rewrite_types(&mut self, type_table: &mut TypeTable) {
        // Collect entries to rewrite (can't mutate while iterating)
        let mut replacements: Vec<(TypeId, ResolvedType)> = Vec::new();

        for type_id in type_table.iter_type_ids().collect::<Vec<_>>() {
            match type_table.get(type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    if let Some(&box_type_id) = self.box_struct_types.get(&inner) {
                        // Replace Ref(primitive) with the Box struct type
                        replacements.push((type_id, type_table.get(box_type_id).clone()));
                    }
                }
                _ => {}
            }
        }

        for (type_id, new_type) in &replacements {
            type_table.replace_type(*type_id, new_type.clone());
        }

        // Add all rewritten TypeIds to box_type_ids so that Deref/Assign
        // handlers can recognize them as Box types.
        for (type_id, _) in replacements {
            self.box_type_ids.insert(type_id);
        }
    }

    /// Transform a function's body to use Box<T> struct operations.
    fn transform_function(&self, func: &mut TirFunction, type_table_rc: &Rc<RefCell<TypeTable>>) {
        let type_table = type_table_rc.borrow();

        // Update local_types for address-taken primitive locals
        let address_taken = func.address_taken_locals.clone();
        for &local_idx in &address_taken {
            let local_type_id = func.local_types[local_idx as usize];
            if let Some(&box_type_id) = self.box_struct_types.get(&local_type_id) {
                func.local_types[local_idx as usize] = box_type_id;
            }
        }

        // Transform the function body
        if let Some(body) = &mut func.body {
            self.transform_block(body, &address_taken, &type_table);
        }

        // Expand non-box deref assignments (*ref = value) to field-by-field assignments.
        // After transform_block, any remaining Assign { target: Deref(..) } are for
        // non-primitive struct types. Expand them using the struct fields map.
        if let Some(body) = &mut func.body {
            self.expand_deref_assigns_in_block(
                body,
                &mut func.local_count,
                &mut func.local_types,
                &type_table,
            );
        }

        // Keep `address_taken_locals` populated past lowering: downstream
        // optimization passes (e.g. `field_forward`) use it as a stable
        // "this local was ever address-taken in source" signal that
        // survives optimizer iterations even after the syntactic `&x`
        // markers in the body have been inlined/elided.
    }

    /// Transform a block of statements.
    fn transform_block(
        &self,
        block: &mut TirBlock,
        address_taken: &IndexSet<u32>,
        type_table: &TypeTable,
    ) {
        for stmt in &mut block.stmts {
            self.transform_stmt(stmt, address_taken, type_table);
        }
    }

    /// Transform a single statement.
    fn transform_stmt(
        &self,
        stmt: &mut TirStmt,
        address_taken: &IndexSet<u32>,
        type_table: &TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let {
                local_index,
                value,
                type_id,
                ..
            } => {
                // First transform the value expression
                self.transform_expr(value, address_taken, type_table);

                // For address-taken primitive locals, wrap the initial value in Box<T>
                if address_taken.contains(local_index)
                    && let Some(&box_type_id) = self.box_struct_types.get(type_id)
                {
                    let original_value = std::mem::replace(
                        value,
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, stmt.span),
                    );
                    let box_struct_name =
                        if let ResolvedType::Struct { name, .. } = type_table.get(box_type_id) {
                            name.clone()
                        } else {
                            panic!("Box type should be a struct");
                        };
                    *value = TirExpr::new(
                        TirExprKind::StructLiteral {
                            struct_type: box_type_id,
                            struct_name: box_struct_name,
                            fields: vec![TirStructField {
                                name: "value".to_string(),
                                value: original_value,
                                field_index: 0,
                            }],
                        },
                        box_type_id,
                        stmt.span,
                    );
                    // Update the Let's type_id to Box<T>
                    *type_id = box_type_id;
                }
            }
            TirStmtKind::Expr(expr) => {
                self.transform_expr(expr, address_taken, type_table);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                self.transform_expr(expr, address_taken, type_table);
            }
            TirStmtKind::Return { value: None } => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.transform_expr(condition, address_taken, type_table);
                self.transform_block(then_block, address_taken, type_table);
                if let Some(else_block) = else_block {
                    self.transform_block(else_block, address_taken, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.transform_block(body, address_taken, type_table);
            }
            TirStmtKind::Break {
                value: Some(expr), ..
            } => {
                self.transform_expr(expr, address_taken, type_table);
            }
            TirStmtKind::Break { value: None, .. } | TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.transform_block(block, address_taken, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.transform_expr(scrutinee, address_taken, type_table);
                self.transform_block(then_block, address_taken, type_table);
                if let Some(else_block) = else_block {
                    self.transform_block(else_block, address_taken, type_table);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.transform_expr(value, address_taken, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    /// Transform a single expression.
    ///
    /// This is the core of the boxing lowering. It handles:
    /// 1. `Unary(Ref/MutRef, expr)` for primitives → `StructLiteral(Box<T>)`
    /// 2. `Unary(Ref/MutRef, Local)` for address-taken → just `Local` (Box IS the ref)
    /// 3. `Unary(Deref, expr)` on Box types → `FieldAccess(.value)`
    /// 4. `Local { index }` for address-taken → `FieldAccess(Local, .value)`
    /// 5. `Assign { target: Local, value }` for address-taken → assign to `.value`
    /// 6. `Assign { target: Deref(..), value }` for primitives → assign to `.value`
    /// 7. `VariantConstruct { Option, Some, primitive }` → wrap payload in Box
    fn transform_expr(
        &self,
        expr: &mut TirExpr,
        address_taken: &IndexSet<u32>,
        type_table: &TypeTable,
    ) {
        // Recursively transform sub-expressions first (bottom-up)
        match &mut expr.kind {
            TirExprKind::Binary { left, right, .. } => {
                self.transform_expr(left, address_taken, type_table);
                self.transform_expr(right, address_taken, type_table);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.transform_expr(&mut arg.expr, address_taken, type_table);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.transform_expr(receiver, address_taken, type_table);
                for arg in args {
                    self.transform_expr(&mut arg.expr, address_taken, type_table);
                }
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::Index { expr: e, index, .. } => {
                self.transform_expr(e, address_taken, type_table);
                self.transform_expr(index, address_taken, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.transform_expr(condition, address_taken, type_table);
                self.transform_block(&mut *then_branch, address_taken, type_table);
                if let Some(else_branch) = else_branch {
                    self.transform_block(else_branch, address_taken, type_table);
                }
            }
            TirExprKind::Match { expr: e, arms } => {
                self.transform_expr(e, address_taken, type_table);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.transform_expr(guard, address_taken, type_table);
                    }
                    self.transform_expr(&mut arm.body, address_taken, type_table);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.transform_expr(&mut field.value, address_taken, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.transform_expr(elem, address_taken, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.transform_expr(body, address_taken, type_table);
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.transform_expr(callee, address_taken, type_table);
                for arg in args {
                    self.transform_expr(arg, address_taken, type_table);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.transform_expr(functor, address_taken, type_table);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload) = payload {
                    self.transform_expr(payload, address_taken, type_table);
                }
            }
            TirExprKind::VariantTag { expr: inner } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::VariantPayload { expr: inner, .. } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::VariantTest { expr: inner, .. } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.transform_expr(value, address_taken, type_table);
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.transform_expr(arg, address_taken, type_table);
                }
            }
            TirExprKind::Block(block) => {
                self.transform_block(block, address_taken, type_table);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.transform_block(block, address_taken, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.transform_expr(scrutinee, address_taken, type_table);
                for arm in arms {
                    self.transform_block(arm, address_taken, type_table);
                }
                self.transform_block(default, address_taken, type_table);
            }
            // Leaf nodes: no sub-expressions to transform
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Unit
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            // Assign and Unary are handled specially below (before recursion for some cases)
            TirExprKind::Assign { .. } | TirExprKind::Unary { .. } => {
                // Handled below
            }
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }

        // Now handle the boxing-specific transformations (top-down after sub-expressions)
        let span = expr.span;

        match &mut expr.kind {
            TirExprKind::Unary { op, expr: inner } => {
                // First recursively transform the inner expression
                // (need to handle address-taken locals BEFORE general Ref/Deref)
                self.transform_expr(inner, address_taken, type_table);

                match op {
                    TirUnaryOp::Ref | TirUnaryOp::MutRef => {
                        // Case 1: &local / &mut local where local is address-taken
                        // → just the local (the Box IS the reference)
                        if let TirExprKind::FieldAccess {
                            expr: box_local,
                            field_name,
                            ..
                        } = &inner.kind
                        {
                            // After address-taken local transformation, reads become
                            // FieldAccess(Local, .value). Taking a ref to that should
                            // just return the Box (the Local).
                            if field_name == "value"
                                && let TirExprKind::Local { index, .. } = &box_local.kind
                                && address_taken.contains(index)
                            {
                                let local_expr = (**box_local).clone();
                                *expr = local_expr;
                                return;
                            }
                        }

                        // Case 2: &primitive_expr / &mut primitive_expr
                        // → Box<T> { value: expr }
                        let inner_type_id = inner.type_id;
                        if let Some(&box_type_id) = self.box_struct_types.get(&inner_type_id) {
                            let box_struct_name = if let ResolvedType::Struct { name, .. } =
                                type_table.get(box_type_id)
                            {
                                name.clone()
                            } else {
                                panic!("Box type should be a struct");
                            };

                            let inner_owned = std::mem::replace(
                                inner.as_mut(),
                                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                            );

                            expr.kind = TirExprKind::StructLiteral {
                                struct_type: box_type_id,
                                struct_name: box_struct_name,
                                fields: vec![TirStructField {
                                    name: "value".to_string(),
                                    value: inner_owned,
                                    field_index: 0,
                                }],
                            };
                            expr.type_id = box_type_id;
                        }
                        // For non-primitive refs (structs, arrays, etc.), no change needed
                    }
                    TirUnaryOp::Deref => {
                        // Case 3: *ref_to_primitive → FieldAccess(.value)
                        // After type rewriting, ref types are Box<T> struct types
                        let inner_type_id = inner.type_id;
                        if self.box_type_ids.contains(&inner_type_id) {
                            let inner_type = self.get_box_inner_type(inner_type_id);
                            let result_type = inner_type.unwrap_or(expr.type_id);

                            let inner_owned = std::mem::replace(
                                inner.as_mut(),
                                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                            );

                            expr.kind = TirExprKind::FieldAccess {
                                expr: Box::new(inner_owned),
                                field_index: 0,
                                field_name: "value".to_string(),
                            };
                            expr.type_id = result_type;
                        }
                        // For non-primitive refs, Deref is a no-op in Wasm (transparent)
                    }
                    _ => {}
                } // Already handled sub-expression recursion
            }

            TirExprKind::Assign { target, value } => {
                self.transform_expr(value, address_taken, type_table);

                match &mut target.kind {
                    // Assign to address-taken local: x = val → x.value = val
                    TirExprKind::Local { index, name } => {
                        if address_taken.contains(index)
                            && self.box_struct_types.contains_key(&target.type_id)
                        {
                            let local_idx = *index;
                            let local_name = name.clone();
                            let box_type_id = *self
                                .box_struct_types
                                .get(&target.type_id)
                                .expect("address-taken local should have box type");
                            let local_expr = TirExpr::new(
                                TirExprKind::Local {
                                    index: local_idx,
                                    name: local_name,
                                },
                                box_type_id,
                                span,
                            );
                            target.kind = TirExprKind::FieldAccess {
                                expr: Box::new(local_expr),
                                field_index: 0,
                                field_name: "value".to_string(),
                            };
                            // target.type_id stays as the primitive type (the value's type)
                        } else {
                            self.transform_expr(target, address_taken, type_table);
                        }
                    }
                    // Assign through deref: *ref = val → ref.value = val
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: ref_expr,
                    } => {
                        self.transform_expr(ref_expr, address_taken, type_table);
                        let ref_type = ref_expr.type_id;
                        if self.box_type_ids.contains(&ref_type) {
                            let ref_owned = std::mem::replace(
                                ref_expr.as_mut(),
                                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                            );
                            let result_type =
                                self.get_box_inner_type(ref_type).unwrap_or(target.type_id);
                            target.kind = TirExprKind::FieldAccess {
                                expr: Box::new(ref_owned),
                                field_index: 0,
                                field_name: "value".to_string(),
                            };
                            target.type_id = result_type;
                        }
                    }
                    _ => {
                        self.transform_expr(target, address_taken, type_table);
                    }
                } // Already handled sub-expression recursion
            }

            TirExprKind::Local { index, name } => {
                if address_taken.contains(index) {
                    let original_type = expr.type_id;
                    if let Some(&box_type_id) = self.box_struct_types.get(&original_type) {
                        // Transform: Local { index } → FieldAccess(Local { index }, .value)
                        let local_expr = TirExpr::new(
                            TirExprKind::Local {
                                index: *index,
                                name: name.clone(),
                            },
                            box_type_id,
                            span,
                        );
                        expr.kind = TirExprKind::FieldAccess {
                            expr: Box::new(local_expr),
                            field_index: 0,
                            field_name: "value".to_string(),
                        };
                        // expr.type_id stays as the primitive type
                    }
                }
            }

            // Option now uses SubtypeHierarchy — no Box wrapping needed for
            // VariantConstruct("Some").
            _ => {}
        }
    }
}
