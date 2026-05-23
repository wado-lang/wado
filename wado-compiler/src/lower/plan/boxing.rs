//! Type-table and parameter-shadow preparation for the boxing
//! scheme. Body-level rewrites live in the TIR → NIR fold
//! ([`crate::lower::translate`]).

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::mangle_generic_name;
use crate::tir::{
    MonomorphInfo, PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirField, TirLocal,
    TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField, TypeId, TypeTable,
};
use crate::token::Span;

/// Type-level boxing facts produced by [`prepare_types`].
pub struct BoxPlan {
    /// Inner `TypeId` → canonical `Box<T>` struct `TypeId`.
    pub box_struct_types: IndexMap<TypeId, TypeId>,
    /// Every `TypeId` now denoting a `Box<T>` struct: the canonical
    /// wrapper ids *and* the redefined `Ref` / `MutRef` ids.
    pub box_type_ids: IndexSet<TypeId>,
}

impl BoxPlan {
    /// Reverse lookup: `Box<T>` `TypeId` → `T`.
    pub fn get_box_inner_type(&self, type_id: TypeId) -> Option<TypeId> {
        self.box_struct_types
            .iter()
            .find_map(|(inner, box_type)| (*box_type == type_id).then_some(*inner))
    }
}

/// Prepare the type table for boxing: a type-table-only pass.
///
/// It mints a `Box<T>` struct type for every `&primitive` / `&variant`
/// / `&fn` reference in the program, redefines the corresponding
/// `Ref` / `MutRef` `TypeIds` to those struct types (so composite types
/// that transitively contain a boxed reference follow automatically),
/// and appends the generated struct definitions to `flat.structs`.
///
/// It does not touch any function body — body lowering is driven
/// separately from the returned [`BoxPlan`].
pub fn prepare_types(flat: &mut FlatPackage) -> BoxPlan {
    let box_module_source = flat
        .type_table
        .borrow()
        .compiler_items()
        .struct_module(crate::compiler_item::CompilerItem::Box)
        .cloned()
        .unwrap_or_else(ModuleSource::prelude);
    let mut builder = TypeBuilder::new(box_module_source);

    for v in &flat.variants {
        builder.variant_names.insert(v.name.clone());
    }

    {
        let mut type_table = flat.type_table.borrow_mut();
        builder.create_needed_box_types(&mut type_table);
        builder.rewrite_types(&mut type_table);
    }

    flat.structs.append(&mut builder.generated_structs);
    BoxPlan {
        box_struct_types: builder.box_struct_types,
        box_type_ids: builder.box_type_ids,
    }
}

/// Per function: for address-taken parameters, allocate a `Box`
/// shadow local + prelude `Let` + remap reads; for non-param
/// address-taken locals, retag the declaration to its box type.
/// Function bodies are otherwise untouched.
pub fn shadow_params(flat: &mut FlatPackage, plan: &BoxPlan) {
    let type_table = flat.type_table.clone();
    let type_table = type_table.borrow();
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        shadow_one_function(&mut func, plan, &type_table);
    }
}

fn shadow_one_function(func: &mut crate::tir::TirFunction, plan: &BoxPlan, type_table: &TypeTable) {
    let address_taken = func.address_taken_locals.clone();
    let param_count = u32::try_from(func.params.len()).unwrap();

    // (param_idx, shadow_local_idx, box_type_id, original_type_id, name)
    let mut shadowed_params: Vec<(u32, u32, TypeId, TypeId, String)> = Vec::new();
    let mut effective_address_taken = address_taken.clone();

    for &local_idx in &address_taken {
        let local_type_id = func.locals[local_idx as usize].type_id;
        let Some(&box_type_id) = plan.box_struct_types.get(&local_type_id) else {
            continue;
        };
        if local_idx < param_count {
            let shadow_idx = func.local_count;
            func.local_count += 1;
            let name = func.params[local_idx as usize].name.clone();
            func.locals.push(TirLocal {
                name: format!("__boxed_param_{local_idx}"),
                type_id: box_type_id,
                is_mut: false,
            });
            effective_address_taken.swap_remove(&local_idx);
            effective_address_taken.insert(shadow_idx);
            shadowed_params.push((local_idx, shadow_idx, box_type_id, local_type_id, name));
        } else {
            // Retag the declaration so `convert_local` sees the box
            // type without the fold having to consult `BoxPlan`.
            func.locals[local_idx as usize].type_id = box_type_id;
        }
    }

    if !shadowed_params.is_empty()
        && let Some(body) = &mut func.body
    {
        let mut remap: IndexMap<u32, u32> = IndexMap::default();
        for (param_idx, shadow_idx, _, _, _) in &shadowed_params {
            remap.insert(*param_idx, *shadow_idx);
        }
        remap_locals_in_block(body, &remap);

        let mut prelude_stmts: Vec<TirStmt> = Vec::with_capacity(shadowed_params.len());
        for (param_idx, shadow_idx, box_type_id, original_type_id, name) in &shadowed_params {
            let box_struct_name =
                if let ResolvedType::Struct { name, .. } = type_table.get(*box_type_id) {
                    name.clone()
                } else {
                    panic!("Box type should be a struct");
                };
            let span = func.span;
            let param_read = TirExpr::new(
                TirExprKind::Local {
                    index: *param_idx,
                    name: name.clone(),
                },
                *original_type_id,
                span,
            );
            let wrap = TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type: *box_type_id,
                    struct_name: box_struct_name,
                    fields: vec![TirStructField {
                        name: "value".to_string(),
                        value: param_read,
                        field_index: 0,
                    }],
                },
                *box_type_id,
                span,
            );
            prelude_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: format!("__boxed_param_{param_idx}"),
                    local_index: *shadow_idx,
                    is_mut: false,
                    is_reactive: false,
                    type_id: *box_type_id,
                    value: wrap,
                    skip_value_copy: true,
                },
                span,
            ));
        }
        body.stmts.splice(0..0, prelude_stmts);
    }

    // Persist the effective set so downstream optimization passes
    // (`field_forward`, etc.) see shadow locals in place of the
    // original param indices. The fold's address-taken consumer reads
    // from `func.address_taken_locals` directly.
    func.address_taken_locals = effective_address_taken;
}

struct TypeBuilder {
    box_struct_types: IndexMap<TypeId, TypeId>,
    box_type_ids: IndexSet<TypeId>,
    generated_structs: Vec<TirStruct>,
    /// `#[compiler_item("box")]` on `struct Box<T>` in the prelude.
    box_module_source: ModuleSource,
    /// `GenericInstance` whose name is one of these is a variant
    /// and needs boxing.
    variant_names: IndexSet<String>,
}

impl TypeBuilder {
    fn new(box_module_source: ModuleSource) -> Self {
        Self {
            box_struct_types: IndexMap::default(),
            box_type_ids: IndexSet::default(),
            generated_structs: Vec::new(),
            box_module_source,
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

        // Register under the Box definition's module source (from #[compiler_item("box")]).
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

    /// Scan the type table to find which primitives need Box types.
    fn create_needed_box_types(&mut self, type_table: &mut TypeTable) {
        // Collect base TypeIds that need boxing, plus newtypes.
        // Boxing is required for:
        // - Primitives (except i128/u128 which are already GC types)
        // - Variant types (subtype hierarchy prevents field-by-field deref assignment)
        // - Function types (the local holds a `ref struct` value; `&mut fn`
        //   needs a stable heap slot for deref-assignment, and we box `&fn`
        //   for the same shape so reference semantics stay uniform across
        //   all `&T` / `&mut T` types — the optimizer can elide read-only
        //   wrappers later).
        let mut needs_box_base: IndexSet<TypeId> = IndexSet::default();

        for type_id in type_table.iter_type_ids().collect::<Vec<_>>() {
            match type_table.get(type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    let is_prim = matches!(type_table.get(inner), ResolvedType::Primitive(p)
                        if !matches!(p, PrimitiveType::I128 | PrimitiveType::U128));
                    let is_variant = self.is_variant_type(inner, type_table);
                    let is_fn = matches!(type_table.get(inner), ResolvedType::Function { .. });
                    if is_prim || is_variant || is_fn {
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
    fn rewrite_types(&mut self, type_table: &mut TypeTable) {
        // Collect entries to rewrite (can't mutate while iterating).
        // The tuple holds (rewritten_type_id, new_resolved_type, payload_inner_type_id)
        // so that we can register every rewritten id as a Box wrapper of
        // its original `inner` payload — many `Ref(T)` TypeIds may
        // collapse onto the same Box content, and downstream peeling
        // needs the mapping for each of them, not just the canonical
        // wrapper id stored in `box_struct_types`.
        let mut replacements: Vec<(TypeId, ResolvedType, TypeId)> = Vec::new();

        for type_id in type_table.iter_type_ids().collect::<Vec<_>>() {
            match type_table.get(type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    if let Some(&box_type_id) = self.box_struct_types.get(&inner) {
                        // Replace Ref(primitive) with the Box struct type
                        replacements.push((type_id, type_table.get(box_type_id).clone(), inner));
                    }
                }
                _ => {}
            }
        }

        for (type_id, new_type, _) in &replacements {
            type_table.replace_type(*type_id, new_type.clone());
        }

        // Add all rewritten TypeIds to box_type_ids so that Deref/Assign
        // handlers can recognize them as Box types. Mirror the rewrite as
        // a `wrapper -> payload` entry on the type table so downstream
        // passes can call `TypeTable::peel_refs_and_box` to look through
        // the wrapper in one step (used by DCE inspect scanning and the
        // canonical dispatch WIR builder).
        for (type_id, _, inner) in &replacements {
            self.box_type_ids.insert(*type_id);
            type_table.register_box_payload(*type_id, *inner);
        }
        // Also register the canonical `Box<T>` wrapper ids that
        // `create_needed_box_types` minted, so callers can ask for the
        // payload of *any* TypeId that ended up looking like a Box.
        for (&inner, &wrapper) in &self.box_struct_types {
            type_table.register_box_payload(wrapper, inner);
        }
    }
}

/// Rewrite every `Local { index }` reference in `block` according to
/// `remap`. Used by [`shadow_one_function`] to redirect address-taken
/// parameter reads to the param's `Box`-typed shadow local.
///
/// Closure bodies are skipped: closure locals live in their own scope
/// and reuse the same indices, so descending would cause unrelated
/// remappings.
fn remap_locals_in_block(block: &mut TirBlock, remap: &IndexMap<u32, u32>) {
    for stmt in &mut block.stmts {
        remap_locals_in_stmt(stmt, remap);
    }
}

fn remap_locals_in_stmt(stmt: &mut TirStmt, remap: &IndexMap<u32, u32>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => remap_locals_in_expr(value, remap),
        TirStmtKind::Expr(expr) => remap_locals_in_expr(expr, remap),
        TirStmtKind::Return { value: Some(v) } => remap_locals_in_expr(v, remap),
        TirStmtKind::Return { value: None } => {}
        TirStmtKind::TaskReturn { value } => remap_locals_in_expr(value, remap),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            remap_locals_in_expr(condition, remap);
            remap_locals_in_block(then_block, remap);
            if let Some(eb) = else_block {
                remap_locals_in_block(eb, remap);
            }
        }
        TirStmtKind::Loop { body } => remap_locals_in_block(body, remap),
        TirStmtKind::Break { value: Some(v), .. } => remap_locals_in_expr(v, remap),
        TirStmtKind::Break { value: None, .. } | TirStmtKind::Continue => {}
        TirStmtKind::LabeledBlock { block, .. } => remap_locals_in_block(block, remap),
        TirStmtKind::LetDestructure { value, pattern, .. } => {
            remap_locals_in_expr(value, remap);
            remap_locals_in_pattern(pattern, remap);
        }
        TirStmtKind::VariadicForOf { iterable, body, .. } => {
            remap_locals_in_expr(iterable, remap);
            remap_locals_in_block(body, remap);
        }
    }
}

fn remap_locals_in_expr(expr: &mut TirExpr, remap: &IndexMap<u32, u32>) {
    match &mut expr.kind {
        TirExprKind::Local { index, .. } => {
            if let Some(&new_idx) = remap.get(index) {
                *index = new_idx;
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            remap_locals_in_expr(left, remap);
            remap_locals_in_expr(right, remap);
        }
        TirExprKind::Unary { expr: inner, .. } => remap_locals_in_expr(inner, remap),
        TirExprKind::Assign { target, value } => {
            remap_locals_in_expr(target, remap);
            remap_locals_in_expr(value, remap);
        }
        TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::VariantTest { expr: inner, .. } => {
            remap_locals_in_expr(inner, remap);
        }
        TirExprKind::Index { expr: e, index, .. } => {
            remap_locals_in_expr(e, remap);
            remap_locals_in_expr(index, remap);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                remap_locals_in_expr(&mut arg.expr, remap);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                remap_locals_in_expr(arg, remap);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            remap_locals_in_expr(receiver, remap);
            for arg in args {
                remap_locals_in_expr(&mut arg.expr, remap);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            remap_locals_in_expr(callee, remap);
            for arg in args {
                remap_locals_in_expr(arg, remap);
            }
        }
        TirExprKind::Block(block) => remap_locals_in_block(block, remap),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remap_locals_in_expr(condition, remap);
            remap_locals_in_block(then_branch, remap);
            if let Some(eb) = else_branch {
                remap_locals_in_block(eb, remap);
            }
        }
        TirExprKind::Match { expr: e, arms } => {
            remap_locals_in_expr(e, remap);
            for arm in arms {
                remap_locals_in_pattern(&mut arm.pattern, remap);
                if let Some(g) = &mut arm.guard {
                    remap_locals_in_expr(g, remap);
                }
                remap_locals_in_expr(&mut arm.body, remap);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                remap_locals_in_expr(&mut field.value, remap);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                remap_locals_in_expr(elem, remap);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                remap_locals_in_expr(p, remap);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => remap_locals_in_expr(value, remap),
        TirExprKind::LabeledBlock { block, .. } => remap_locals_in_block(block, remap),
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let crate::tir::TirTemplatePart::Interpolation { expr, .. } = part {
                    remap_locals_in_expr(expr, remap);
                }
            }
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            for b in bindings {
                remap_locals_in_expr(&mut b.handler, remap);
            }
            remap_locals_in_block(body, remap);
        }
        TirExprKind::Resume { value } => remap_locals_in_expr(value, remap),
        // Closure bodies and Capture references live in the closure's own
        // local-index scope; do not descend.
        TirExprKind::Closure { .. } | TirExprKind::Capture { .. } => {}
        // Leaf nodes with no sub-expressions or no Local references.
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
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

fn remap_locals_in_pattern(pattern: &mut TirPattern, remap: &IndexMap<u32, u32>) {
    match pattern {
        TirPattern::Binding { local_index, .. } => {
            if let Some(&new_idx) = remap.get(local_index) {
                *local_index = new_idx;
            }
        }
        TirPattern::Tuple(sub, _) => {
            for p in sub {
                remap_locals_in_pattern(p, remap);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                remap_locals_in_pattern(&mut f.pattern, remap);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                remap_locals_in_pattern(p, remap);
            }
        }
        TirPattern::Or(alts) => {
            for p in alts {
                remap_locals_in_pattern(p, remap);
            }
        }
        TirPattern::ConstantValue { expr } => remap_locals_in_expr(expr, remap),
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::Range { .. } => {}
    }
}
