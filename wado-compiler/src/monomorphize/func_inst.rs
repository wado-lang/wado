//! Function instantiation: creating concrete functions from generic definitions,
//! collecting instantiation sites, variadic expansion, and method call resolution.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::compiler_item::CompilerItem;
use crate::elaborator::trait_env::{BlanketImpl, BlanketReceiver, TraitEnv};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, RefKind, mangle_generic_name};
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBinaryOp,
    TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirModule, TirParam, TirPattern,
    TirStmt, TirStmtKind, TirTemplatePart, TirUnaryOp, TypeId, TypeTable, method_param_offset,
    transpose_tuple_expr,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use super::state::Monomorphizer;
use super::{Templates, module_source_for_trait_impl};
use crate::defs::DefId;
use crate::name::FqTraitName;
use crate::synthesis::template::{
    blanket_impl_args, blanket_is_reflect_keyed, has_reflect_kind, method_template_at,
    ranked_value_blanket, trait_call_template,
};
use crate::tir;
use crate::tir::TemplateId;
use crate::token::Span;

/// Lower remaining comparison operators on non-primitive types in all module functions.
pub fn lower_comparisons_in_module(module: &mut TirModule, trait_env: &Arc<TraitEnv>) {
    let type_table_rc = module.type_table.clone();

    struct ComparisonLowerer<'a> {
        trait_env: &'a Arc<TraitEnv>,
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
                    self.trait_env,
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
        trait_env,
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
        lowerer.visit_expr(global.init.slot_expr_mut());
    }
}

/// Expand every `TypePackExpansion` whose own site settled the pack, in every
/// body of the module: a function's and a global initializer's alike.
///
/// Such a node sits in a caller that may declare no type parameter, which
/// `instantiate_function` never visits. Runs before instantiation sites are
/// collected, so the calls it produces are monomorphized like any other.
pub fn expand_settled_packs_in_module(mono: &mut Monomorphizer, module: &mut TirModule) {
    mono.current_param_substitution_key = IndexMap::default();
    mono.current_impl_type_param_count = 0;
    mono.current_impl_receiver = None;

    let type_table_rc = module.type_table.clone();

    struct SettledPackExpander<'a> {
        mono: &'a Monomorphizer,
        type_table: &'a Rc<RefCell<TypeTable>>,
        local_count: &'a mut u32,
        locals: &'a mut Vec<TirLocal>,
    }

    impl TirMutVisitor for SettledPackExpander<'_> {
        fn visit_expr(&mut self, expr: &mut TirExpr) {
            if let TirExprKind::TupleLiteral { elements } = &expr.kind
                && let Some(substitution) = elements.iter().find_map(|e| match e.kind {
                    TirExprKind::TypePackExpansion {
                        pack_type_id,
                        settled_pack: Some(settled),
                        ..
                    } => {
                        let index = self
                            .type_table
                            .borrow()
                            .param_slot(pack_type_id)
                            .expect("an expansion names its pack before substitution");
                        Some(IndexMap::from_iter([(index, settled)]))
                    }
                    _ => None,
                })
            {
                // Everything the subtree names belongs to the callee whose
                // default this is, and the call settled all of it — so the one
                // pack entry is the whole substitution, and the ordinary
                // expansion below it needs nothing else. Recurses itself.
                self.mono.substitute_types_in_expr(
                    expr,
                    &substitution,
                    &mut self.type_table.borrow_mut(),
                    self.local_count,
                    self.locals,
                );
                return;
            }
            self.walk_expr(expr);
        }
    }

    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        let Some(mut body) = func.body.take() else {
            continue;
        };
        let mut local_count = func.local_count;
        let mut locals = std::mem::take(&mut func.locals);
        SettledPackExpander {
            mono,
            type_table: &type_table_rc,
            local_count: &mut local_count,
            locals: &mut locals,
        }
        .visit_block(&mut body);
        // Each expanded element needs its own slot for whatever the template
        // body declared, exactly as an instantiated generic does.
        PackExpansionLocalSplitter {
            local_count: &mut local_count,
            locals: &mut locals,
        }
        .visit_block(&mut body);
        func.local_count = local_count;
        func.locals = locals;
        func.body = Some(body);
    }
}

/// The post-substitution values `resolve_method_call_substitution` computes
/// before it branches on the receiver kind, handed to the per-kind resolver.
struct SubstitutedCall {
    /// The post-substitution method info (concrete receiver name).
    info: LocalMethodName,
    /// Its mangled name.
    mangled: String,
    /// The pre-substitution mangled name, still the blanket-template key for a
    /// bare-`T` blanket dispatch.
    original_name: String,
    /// Substituted impl type args, in param-index order.
    type_args: Vec<TypeId>,
    /// Substituted method-level type args, in declaration order. Non-empty for
    /// a method carrying type params of its own (`serialize<S: Serializer>`).
    method_type_args: Vec<TypeId>,
    /// The call's current module source, used as the fallback home.
    module_source: ModuleSource,
}

/// The mangled name of the template `blanket` provides for `method`: a
/// blanket's body is keyed by its receiver *parameter*, never by the type that
/// dispatches to it.
fn blanket_template_name(
    blanket: &BlanketImpl,
    method: &LocalMethodName,
    type_table: &TypeTable,
) -> String {
    LocalMethodName::new(
        blanket.receiver_binder(type_table.defs()),
        method.trait_name.clone(),
        method.method_name.clone(),
    )
    .to_mangled_name()
}

/// Collects function instantiation sites by traversing TIR with `TirRefVisitor`.
///
/// Replaces the manual recursive traversal in `collect_func_instantiation_sites_in_*`
/// with visitor-based traversal. The visitor's `walk_expr`/`walk_stmt` handles
/// recursion into all TIR node kinds automatically, so only call
/// need custom handling.
pub(super) struct InstantiationCollector<'a> {
    pub mono: &'a mut Monomorphizer,
    pub generic_functions: &'a Templates,
    pub type_table: &'a mut TypeTable,
}

impl TirRefVisitor for InstantiationCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let Some(key) = self
            .mono
            .call_instance(expr, self.generic_functions, self.type_table)
        {
            let mangled = self
                .mono
                .instance_name(&key, self.generic_functions, self.type_table);
            self.mono.try_queue_function(key, mangled, self.type_table);
        }
        self.walk_expr(expr);
    }
}

struct LocalIndexRewriter {
    old_idx: u32,
    new_idx: u32,
}

/// Retypes every use of one unrolled binding to its concrete element type,
/// including the `&`/`&mut` wrapper a method receiver takes — dispatch resolves
/// off the receiver's type, so pinning it is what keeps a nested unroll from
/// retargeting the call. Closures keep their own local namespace, so their
/// bodies are left alone.
struct BindingTypePinner<'a> {
    binding_local: u32,
    elem_type: TypeId,
    type_table: &'a mut TypeTable,
}

impl TirMutVisitor for BindingTypePinner<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &expr.kind {
            TirExprKind::Local { index, .. } if *index == self.binding_local => {
                expr.type_id = self.elem_type;
            }
            TirExprKind::Closure { .. } => return,
            _ => {}
        }
        self.walk_expr(expr);

        if let TirExprKind::Unary { op, expr: inner } = &expr.kind
            && matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
            && expr_projects_local(inner, self.binding_local)
        {
            expr.type_id = match op {
                TirUnaryOp::MutRef => self.type_table.make_mut_ref(self.elem_type),
                _ => self.type_table.make_ref(self.elem_type),
            };
        }
    }
}

/// Folds `t[i]` — a pack-typed tuple subscripted by a variadic
/// `.enumerate()` index — into the unrolled element's field access.
/// The index is a compile-time constant by construction, so the read and the
/// write (`slots[i] = v`, an `Assign` over the same node) both land on a
/// concrete tuple field (WEP 2026-03-14).
struct EnumerateSubscriptRewriter<'a> {
    index_local: u32,
    element: u32,
    type_table: &'a TypeTable,
}

impl TirMutVisitor for EnumerateSubscriptRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.walk_expr(expr);
        let TirExprKind::Index { expr: base, index } = &mut expr.kind else {
            return;
        };
        if !matches!(&index.kind, TirExprKind::Local { index, .. } if *index == self.index_local) {
            return;
        }
        // Only a tuple has a compile-time field at this index. A `List` read
        // keyed by the same local is an ordinary runtime index and must lower
        // as one.
        if self.type_table.as_tuple_through_ref(base.type_id).is_none() {
            return;
        }
        expr.kind = TirExprKind::FieldAccess {
            expr: Box::new((**base).clone()),
            field_index: self.element,
            field_name: self.element.to_string(),
        };
    }
}

impl TirMutVisitor for LocalIndexRewriter {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } if *index == self.old_idx => {
                *index = self.new_idx;
            }
            TirExprKind::Closure { captures, .. } => {
                // A capture reading a parent local names the *parent* frame, so
                // it is rewritten like any other such reference (this is
                // what makes a for-of binding captured by a closure follow the
                // per-iteration rename). The closure *body*, by contrast, uses
                // its own local-index namespace and reaches the parent only
                // through these captures, so it must NOT be descended into —
                // doing so would rewrite a closure-scoped local that happens to
                // share `old_idx`.
                for cap in captures {
                    cap.source = cap.source.map_local(|index| {
                        if index == self.old_idx {
                            self.new_idx
                        } else {
                            index
                        }
                    });
                }
                return;
            }
            _ => {}
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

/// Collects every local index a `let` or pattern binding introduces in the
/// *current function's* frame, so variadic for-of expansion knows which body
/// locals to retype or reallocate per cloned iteration. Stops at `Closure`
/// boundaries: those allocate in a separate index namespace, and collecting them
/// would let the reallocation loop corrupt closure-scoped slots.
struct LocalCollector {
    locals: Vec<u32>,
}

impl TirRefVisitor for LocalCollector {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if matches!(&expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
    }

    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            self.locals.push(*local_index);
        }
        self.walk_stmt(stmt);
    }

    fn visit_pattern(&mut self, pattern: &TirPattern) {
        if let TirPattern::Binding { local_index, .. } = pattern {
            self.locals.push(*local_index);
        }
        self.walk_pattern(pattern);
    }
}

fn locals_defined_in_expr(expr: &TirExpr) -> Vec<u32> {
    let mut collector = LocalCollector { locals: Vec::new() };
    collector.visit_expr(expr);
    collector.locals
}

fn locals_defined_in_block(block: &TirBlock) -> Vec<u32> {
    let mut collector = LocalCollector { locals: Vec::new() };
    collector.visit_block(block);
    collector.locals
}

/// Finds the declared type of a specific local index by scanning the `let`
/// statement or pattern binding that introduces it. A local is defined exactly
/// once, so the first match is the answer. Stops at `Closure` boundaries for
/// the same reason as [`LocalCollector`] — a closure-scoped local could share a
/// numeric index with the current frame's local and return the wrong type.
struct LocalTypeFinder {
    target: u32,
    found: Option<TypeId>,
}

impl TirRefVisitor for LocalTypeFinder {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.found.is_some() || matches!(&expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
    }

    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index,
            type_id,
            ..
        } = &stmt.kind
            && *local_index == self.target
            && self.found.is_none()
        {
            self.found = Some(*type_id);
        }
        self.walk_stmt(stmt);
    }

    fn visit_pattern(&mut self, pattern: &TirPattern) {
        if let TirPattern::Binding {
            local_index,
            type_id,
            ..
        } = pattern
            && *local_index == self.target
            && self.found.is_none()
        {
            self.found = Some(*type_id);
        }
        self.walk_pattern(pattern);
    }
}

fn local_type_in_expr(expr: &TirExpr, local_idx: u32) -> Option<TypeId> {
    let mut finder = LocalTypeFinder {
        target: local_idx,
        found: None,
    };
    finder.visit_expr(expr);
    finder.found
}

fn local_type_in_block(block: &TirBlock, local_idx: u32) -> Option<TypeId> {
    let mut finder = LocalTypeFinder {
        target: local_idx,
        found: None,
    };
    finder.visit_block(block);
    finder.found
}

/// Whether `expr` denotes local `local_index`, or a projection out of it.
///
/// Narrower than [`expr_reads_local`] on purpose: only these shapes make
/// `&expr` a reference *to the binding*, which is what pins a nested unroll's
/// receiver type. Merely reading the binding yields something else.
fn expr_projects_local(expr: &TirExpr, local_index: u32) -> bool {
    match &expr.kind {
        TirExprKind::Local { index, .. } => *index == local_index,
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. } => expr_projects_local(inner, local_index),
        TirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| expr_projects_local(&f.value, local_index)),
        _ => false,
    }
}

/// Whether `expr` reads local `index` anywhere inside it.
struct LocalReadFinder {
    index: u32,
    found: bool,
}

impl TirRefVisitor for LocalReadFinder {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if matches!(expr.kind, TirExprKind::Local { index, .. } if index == self.index) {
            self.found = true;
        }
        self.walk_expr(expr);
    }
}

fn expr_reads_local(expr: &TirExpr, index: u32) -> bool {
    let mut finder = LocalReadFinder {
        index,
        found: false,
    };
    finder.visit_expr(expr);
    finder.found
}

/// Give each element of an expanded `TypePackExpansion` its own locals.
///
/// Expanding `[..T::method()?]` clones the call per pack member, so every clone
/// carries the same local indices under a different element type. The first
/// element keeps the template's slots, retyped from its own bindings; later
/// elements move to fresh ones. Closures allocate in their own index namespace,
/// so their bodies are left alone.
struct PackExpansionLocalSplitter<'a> {
    local_count: &'a mut u32,
    locals: &'a mut Vec<TirLocal>,
}

impl PackExpansionLocalSplitter<'_> {
    fn split(&mut self, elements: &mut [TirExpr]) {
        let mut first_seen: IndexSet<u32> = IndexSet::default();
        for element in elements.iter_mut() {
            // An or-pattern binds the same slot in each alternative; that repeat
            // is not a collision between elements, so collapse it first.
            let defined: IndexSet<u32> = locals_defined_in_expr(element).into_iter().collect();
            for old_idx in defined {
                if first_seen.insert(old_idx) {
                    // Keeps the slot, but takes the binding's own type: pack
                    // substitution left the frame entry generic.
                    if let Some(concrete) = local_type_in_expr(element, old_idx)
                        && let Some(entry) = self.locals.get_mut(old_idx as usize)
                    {
                        entry.type_id = concrete;
                    }
                    continue;
                }
                let new_idx = *self.local_count;
                *self.local_count += 1;
                let local_type = local_type_in_expr(element, old_idx).unwrap_or_else(|| {
                    self.locals
                        .get(old_idx as usize)
                        .map_or(TypeTable::UNIT, |l| l.type_id)
                });
                self.locals
                    .push(TirLocal::synth(new_idx, local_type, false));
                LocalIndexRewriter { old_idx, new_idx }.visit_expr(element);
            }
        }
    }
}

impl TirMutVisitor for PackExpansionLocalSplitter<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if matches!(expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        if let TirExprKind::TupleLiteral { elements } = &mut expr.kind
            && elements.len() > 1
        {
            self.split(elements);
            return;
        }
        self.walk_expr(expr);
    }
}

/// The type slots a `return` value carries in the current frame: the value's
/// own `type_id`, and a `VariantConstruct`'s `variant_type` plus its payload
/// chain. Closure bodies are their own frame.
///
/// Exactly the slots that must name the *enclosing* function's return type.
/// Everything else in an expanded `[..T::method()?]` element is per-element, a
/// call argument included: `return Result::Err(From::from(e))` returns the full
/// pack, but `e` is this element's error.
struct ReturnTypeSlots<F> {
    on_slot: F,
}

impl<F: FnMut(&mut TypeId)> ReturnTypeSlots<F> {
    fn new(on_slot: F) -> Self {
        Self { on_slot }
    }

    fn slots_of(&mut self, value: &mut TirExpr) {
        (self.on_slot)(&mut value.type_id);
        if let TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } = &mut value.kind
        {
            (self.on_slot)(variant_type);
            if let Some(payload) = payload {
                self.slots_of(payload);
            }
        }
    }
}

impl<F: FnMut(&mut TypeId)> TirMutVisitor for ReturnTypeSlots<F> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if matches!(expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
    }

    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        if let TirStmtKind::Return { value: Some(value) } = &mut stmt.kind {
            self.slots_of(value);
        }
        self.walk_stmt(stmt);
    }
}

/// Fill in method-level `type_args` left empty because `T` came from a
/// `TypePack` element and only turns concrete after variadic expansion. Both
/// gates matter: the argument must read the loop binding — the one shape the
/// elaborator could not pin — and the callee must declare a method type param,
/// or the instance is named after an ordinary parameter's type instead.
struct MethodTypeArgInferer<'a> {
    type_table: &'a TypeTable,
    binding_local: u32,
    templates: &'a Templates,
}

impl MethodTypeArgInferer<'_> {
    /// Whether the callee declares a method-level type param. Only generic
    /// functions are registered as templates, so a miss is a definite "no".
    fn callee_is_method_generic(&self, func: &FunctionRef) -> bool {
        func.template
            .as_ref()
            .and_then(|id| self.templates.get(id))
            .is_some_and(|t| t.borrow().has_real_type_params())
    }
}

impl TirMutVisitor for MethodTypeArgInferer<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if matches!(&expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        // Recurse first so nested calls are handled bottom-up, matching the
        // original walk order.
        self.walk_expr(expr);
        let TirExprKind::Call {
            func,
            type_args,
            args,
            has_receiver: true,
        } = &mut expr.kind
        else {
            return;
        };
        let Some((receiver, args)) = args.split_first() else {
            return;
        };
        let receiver = &receiver.expr;
        // Only methods with empty `type_args` and an empty `method_type_args`
        // need inference (the latter signals method-level type params inferred
        // from arguments rather than pinned by turbofish).
        if !type_args.is_empty() {
            return;
        }
        let Some(info) = &func.method_info else {
            return;
        };
        if !info.method_type_args.is_empty() {
            return;
        }
        let Some(first_arg) = args.first() else {
            return;
        };
        if !expr_reads_local(&first_arg.expr, self.binding_local) {
            return;
        }
        if !self.callee_is_method_generic(func) {
            return;
        }
        // Infer T from the first non-self argument's inner type. For
        // `element<T: Serialize>(&mut self, value: &T)` the first arg is `&T`,
        // so unwrap references (and an auto-boxed `Box<T>`) to reach T.
        let peeled = self.type_table.peel_refs(first_arg.expr.type_id);
        let arg_type = self.type_table.as_box(peeled).unwrap_or(peeled);
        // Only set if `arg_type` is concrete (not a type param) and is not
        // already an impl-level type arg of the receiver (e.g. `List<String>`'s
        // `String`), which would double-count during instantiation.
        let is_concrete = !matches!(
            self.type_table.get(arg_type),
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } | ResolvedType::Unknown
        );
        let receiver_impl_type_args = self
            .type_table
            .nominal_type_args(self.type_table.peel_refs(receiver.type_id))
            .unwrap_or_default();
        let is_impl_type_arg = receiver_impl_type_args.contains(&arg_type);
        if is_concrete && !is_impl_type_arg {
            type_args.push(arg_type);
        }
    }
}

impl Monomorphizer {
    /// Collect function instantiation sites from call expressions, skipping the
    /// `scanned` leading functions a previous run left rewritten.
    pub fn collect_function_instantiation_sites(
        &mut self,
        module: &TirModule,
        generic_functions: &Templates,
        scanned: usize,
    ) {
        let mut type_table = module.type_table.borrow_mut();
        let mut collector = InstantiationCollector {
            mono: self,
            generic_functions,
            type_table: &mut type_table,
        };
        for func_rc in &module.functions[scanned..] {
            let func = func_rc.borrow();
            // A template's body is scanned once instantiated, in Phase 9.
            if func.is_template() {
                continue;
            }
            if let Some(body) = &func.body {
                collector.visit_block(body);
            }
        }

        // Global initializers, on the first run only: a resume adds no globals,
        // and the first run left these rewritten too.
        if scanned == 0 {
            for global in &module.globals {
                collector.visit_expr(global.init.slot_expr());
            }
        }
    }

    /// The template a call of `info` on the concrete `receiver` instantiates: a
    /// written block or blanket, else the derived body beside the receiver's head.
    pub(super) fn dispatch_template(
        &self,
        info: &LocalMethodName,
        receiver: TypeId,
        home: &ModuleSource,
        type_table: &TypeTable,
    ) -> Option<TemplateId> {
        method_template_at(&self.functions.trait_env, info, receiver, type_table).or_else(|| {
            info.trait_decl()?;
            let module = info.fq_base_struct_name().module().unwrap_or(home).clone();
            Some(TemplateId::derived(module, info))
        })
    }

    /// [`Self::dispatch_template`], or `blanket`'s method where one serves the call.
    fn served_template(
        &self,
        blanket: Option<&BlanketImpl>,
        info: &LocalMethodName,
        receiver: TypeId,
        home: &ModuleSource,
        type_table: &TypeTable,
    ) -> Option<TemplateId> {
        match blanket {
            Some(b) => self
                .functions
                .trait_env
                .method_template(b.def, &info.method_name),
            None => self.dispatch_template(info, receiver, home, type_table),
        }
    }

    /// The template a static call on a generic block's head reaches at
    /// `head_args`: a block written for that instantiation wins over `written`.
    fn static_template_at(
        &self,
        written: Option<TemplateId>,
        info: &LocalMethodName,
        head_args: &[TypeId],
        type_table: &mut TypeTable,
    ) -> Option<TemplateId> {
        let Some(TemplateId::Declared {
            block: Some(block), ..
        }) = written
        else {
            return written;
        };
        type_table
            .impl_target_at(block, head_args)
            .and_then(|receiver| {
                method_template_at(&self.functions.trait_env, info, receiver, type_table)
            })
            .or(written)
    }

    /// The concrete type a `T^Trait::method` receiver dispatches on: the
    /// parameter named `T`, else (a synthesised receiver) the lowest slot.
    fn receiver_substitution_tid(
        &self,
        info: &LocalMethodName,
        substitution: &IndexMap<u32, TypeId>,
    ) -> Option<TypeId> {
        if let Some(key) = self
            .current_param_substitution_key
            .get(&info.base_struct_name())
            && let Some(&tid) = substitution.get(key)
        {
            return Some(tid);
        }
        substitution
            .iter()
            .min_by_key(|(idx, _)| **idx)
            .map(|(_, &tid)| tid)
    }

    /// Resolve a `T^Trait::method` static call's dispatch receiver, in order:
    /// the receiver's own impl, the newtype or reference link it inherits one
    /// from, a value blanket serving it — never inherited — then the base.
    fn type_param_dispatch_tid(
        &self,
        tid: TypeId,
        info: &LocalMethodName,
        type_table: &TypeTable,
    ) -> TypeId {
        let base = match type_table.get(tid) {
            // What the newtype inherits impls from, which for a newtype over a
            // `flags` type is that declaration — `representation_head` would
            // carry on to `u32`, the representation, whose impls are a
            // different set and which carries no `ReflectFlags` at all.
            ResolvedType::Newtype { .. } => type_table.reflect_structure_head(tid),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => return tid,
        };
        if base == tid {
            return tid;
        }
        let Some(trait_name) = &info.trait_name else {
            return tid;
        };
        // Identity is the one fact a newtype does not inherit (WEP 2026-06-13):
        // the root and the newtype kind are synthesized per newtype and carry
        // no AST header, so the checks below would peel past them and a
        // derivation would answer with the base's name. Every other trait does
        // inherit, and peeling is how it is reached.
        if type_table.reflect_kind(tid) == Some(CompilerItem::ReflectNewtype)
            && trait_name.canonical().is_some_and(|declared| {
                type_table
                    .compiler_items()
                    .trait_among(
                        declared,
                        &[CompilerItem::Reflect, CompilerItem::ReflectNewtype],
                    )
                    .is_some()
            })
        {
            return tid;
        }
        if let Some(decl) = self.functions.trait_env.trait_def_of_fq(trait_name) {
            if self.has_own_trait_impl(type_table, tid, decl) {
                return tid;
            }
            if let Some(link) = self.newtype_link_with_trait_impl(tid, type_table, decl) {
                return link;
            }
        }
        if trait_name
            .canonical()
            .is_some_and(|trait_| self.value_blanket_serves(tid, trait_, type_table))
        {
            return tid;
        }
        base
    }

    /// Where the impl serving `tid` lives. A newtype that wrote its own homes
    /// it in its own module; only an inherited impl lives with the base, which
    /// is the convention [`module_source_for_trait_impl`] peels to.
    fn impl_module_of_receiver(
        &self,
        type_table: &TypeTable,
        tid: TypeId,
        info: &LocalMethodName,
    ) -> Option<ModuleSource> {
        let link = info
            .trait_name
            .as_ref()
            .and_then(|trait_name| self.functions.trait_env.trait_def_of_fq(trait_name))
            .and_then(|trait_| self.newtype_link_with_trait_impl(tid, type_table, trait_));
        match link {
            Some(link) => type_table.nominal_head(link).map(|(_, m)| m),
            None => module_source_for_trait_impl(type_table, tid),
        }
    }

    /// Whether an operator on `id` lowers to a scalar instruction rather than
    /// the trait call monomorphization produced. A primitive *is* the
    /// instruction; a type that merely erases to one keeps its own impl.
    fn operator_lowers_to_scalar(
        &self,
        type_table: &TypeTable,
        id: TypeId,
        trait_decl: Option<DefId>,
    ) -> bool {
        if !type_table.is_scalar_primitive_like(id) {
            return false;
        }
        if matches!(type_table.get(id), ResolvedType::Primitive(_)) {
            return true;
        }
        let Some(trait_) = trait_decl else {
            return true;
        };
        // `enum` and `flags` erase to a scalar without being a newtype link,
        // and an impl a link below the receiver wrote is still inherited.
        // Relowering over either takes the primitive's instruction instead.
        !self.has_own_trait_impl(type_table, id, trait_)
            && self
                .newtype_link_with_trait_impl(id, type_table, trait_)
                .is_none()
    }

    fn value_blanket_serves(&self, tid: TypeId, trait_: DefId, type_table: &TypeTable) -> bool {
        ranked_value_blanket(
            &self.functions.trait_env,
            trait_,
            module_source_for_trait_impl(type_table, tid).as_ref(),
            tid,
            type_table,
        )
        .is_some()
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
        let mangled_name = self.lookup_function_instantiation(key)?.clone();
        // Claims are per instantiation: this function gets its own `locals`
        // table, so no slot it unrolls into is shared with another's.
        self.unrolled_local_claims.borrow_mut().clear();

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
            if let Some((src_idx, assoc_name)) = &param.projected_from {
                // A projected pack `..F` (`impl<T: ReflectStruct<FieldTypes = [..F]>>`) is
                // not caller-supplied: resolve `T::Fields` for the concrete `T`,
                // which precedes the pack and is already bound.
                let projected = substitution
                    .get(src_idx)
                    .copied()
                    .and_then(|src| type_table.resolve_assoc_type_of_instance(src, assoc_name))
                    .unwrap_or_else(|| type_table.make_tuple(vec![]));
                substitution.insert(param.index, projected);
            } else if param.is_pack {
                // Read the shape off the declaration, never off `key`:
                // `InstantiationKey` leaves `method_info` out of its equality
                // and hash, so two keys differing only there share one entry.
                let one_arg_per_param = generic
                    .method_info
                    .as_ref()
                    .is_some_and(|info| info.receiver.is_declared_type());
                let before = param.index as usize;
                let pack_type = if one_arg_per_param {
                    key.impl_type_args
                        .get(before)
                        .copied()
                        .unwrap_or_else(|| type_table.make_tuple(vec![]))
                } else {
                    let pack_args_count = key
                        .impl_type_args
                        .len()
                        .saturating_sub(non_pack_impl_params_count);
                    let pack_args: Vec<TypeId> = key
                        .impl_type_args
                        .iter()
                        .skip(before)
                        .take(pack_args_count)
                        .copied()
                        .collect();
                    type_table.make_tuple(pack_args)
                };
                substitution.insert(param.index, pack_type);
            } else if let Some(&arg) = key.impl_type_args.get(param.index as usize) {
                substitution.insert(param.index, arg);
            }
        }
        // A parameter nested in the target (`T` in `Pair<List<T>, i32>`) sits
        // past the receiver's positions, and is read out of the argument there.
        if let Some(block) = generic.impl_origin
            && let written = type_table.impl_target_args(block)
            && let Some(positions) = key.impl_type_args.get(..written.len())
            && let Some(bound) = type_table.bind_type_params(written, positions)
        {
            for (slot, ty) in bound {
                substitution.entry(slot).or_insert(ty);
            }
        }

        let offset = method_param_offset(&generic.impl_type_params);
        for (param, &arg) in generic.type_params.iter().zip(key.method_type_args.iter()) {
            substitution.insert(offset + param.index, arg);
        }

        // Map each type-param name to its substitution key, so a
        // `T^Trait::method` receiver resolves by name (the param named
        // `base_struct_name`) rather than positionally. Built with the same
        // key rule as `substitution`: impl params by their index, method
        // params offset past them. Method params are inserted last so an
        // inner-scope name shadows an impl-level one.
        let mut param_key_by_name: IndexMap<String, u32> = IndexMap::default();
        for param in &generic.impl_type_params {
            param_key_by_name.insert(param.name.clone(), param.index);
        }
        for param in &generic.type_params {
            param_key_by_name.insert(param.name.clone(), offset + param.index);
        }
        self.current_param_substitution_key = param_key_by_name;

        // Substitute types in parameters
        let params: Vec<TirParam> = generic
            .params
            .iter()
            .map(|param| TirParam {
                name: param.name.clone(),
                type_id: self.substitute_type(param.type_id, &substitution, type_table),
                local_index: param.local_index,
                is_mut: param.is_mut,
                is_mut_ref: false,
                span: param.span,
            })
            .collect();

        // Substitute return type
        let return_type = self.substitute_type(generic.return_type, &substitution, type_table);

        // Substitute types in `locals`
        let mut locals: Vec<TirLocal> = generic
            .locals
            .iter()
            .map(|local| TirLocal {
                name: local.name.clone(),
                type_id: self.substitute_type(local.type_id, &substitution, type_table),
                is_mut: local.is_mut,
                span: local.span,
            })
            .collect();

        // Clone and substitute types in body
        let mut local_count = generic.local_count;
        self.current_impl_type_param_count = generic.impl_type_params.len();
        self.current_impl_receiver = generic
            .method_info
            .as_ref()
            .map(LocalMethodName::fq_base_struct_name);
        let body = generic.body.as_ref().map(|b| {
            let mut new_body = b.clone();
            self.substitute_types_in_block(
                &mut new_body,
                &substitution,
                type_table,
                &mut local_count,
                &mut locals,
            );
            // Fixup TypePackExpansion: allocate separate locals for each expanded element
            PackExpansionLocalSplitter {
                local_count: &mut local_count,
                locals: &mut locals,
            }
            .visit_block(&mut new_body);
            new_body
        });

        Some(TirFunction {
            module_source: key.module_source.clone(),
            def_id: generic.def_id,
            is_async: generic.is_async,
            name: mangled_name,
            visibility: generic.visibility,
            is_export: generic.is_export, // Inherit from generic
            type_params: vec![],          // Concrete function has no type params
            impl_type_params: vec![],     // Already monomorphized, no impl type params
            impl_origin: None,
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                impl_type_args: key.impl_type_args.clone(),
                method_type_args: key.method_type_args.clone(),
                is_blanket: false,
            }),
            // Mangle through `mangle_type_arg_for_generic` so the definition-side
            // struct name matches what call-site rewrites produce. With
            // `mangle_type_name` the definition said `Node<String>` and the call
            // site `Node<core:prelude/string.wado/String>`; the inliner then lost
            // the self-call link and `-O3` recursed into a stack overflow.
            method_info: generic.method_info.as_ref().map(|info| {
                let impl_type_arg_names: Vec<FqTypeName> = key
                    .impl_type_args
                    .iter()
                    .map(|&t| type_table.fq_type_name(t))
                    .collect();
                let method_type_arg_names: Vec<FqTypeName> = key
                    .method_type_args
                    .iter()
                    .map(|&t| type_table.fq_type_name(t))
                    .collect();
                // A blanket impl's receiver IS one of its type params (e.g.
                // "I"): substitute the concrete name instead of appending args.
                let is_blanket = info.receiver().is_declared_binder_of(
                    generic.impl_type_params.iter().map(|p| p.name.as_str()),
                );
                if is_blanket && !impl_type_arg_names.is_empty() {
                    info.with_substituted_struct_name(&impl_type_arg_names[0])
                } else {
                    info.with_type_args(&impl_type_arg_names, &method_type_arg_names)
                }
            }),
            params,
            return_type,
            task_return_type: None,
            effects: generic.effects.clone(),
            retains: generic.retains.clone(),
            immediates: generic.immediates.clone(),
            trap: generic.trap.clone(),
            linear_memory: generic.linear_memory,
            body,
            span: generic.span,
            local_count,
            locals,
            address_taken_locals: generic.address_taken_locals.clone(),
            stores_aliased_locals: generic.stores_aliased_locals.clone(),
            // Scratch local fields - computed by lower phase (after monomorphization)
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: generic.inline_hint,
            compiler_item: generic.compiler_item,
            export_name: generic.export_name.clone(),
            allocator_tag: generic.allocator_tag.clone(),
            declared_return_convention: generic.declared_return_convention,
            kind: FunctionKind::Regular,

            return_abi: tir::ReturnAbi::default(),
        })
    }

    /// Substitute type parameters in a block
    pub fn substitute_types_in_block(
        &self,
        block: &mut TirBlock,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        locals: &mut Vec<TirLocal>,
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
                        locals,
                    ));
                } else {
                    self.substitute_types_in_stmt(
                        &mut stmt,
                        substitution,
                        type_table,
                        local_count,
                        locals,
                    );
                    block.stmts.push(stmt);
                }
            }
        } else {
            for stmt in &mut block.stmts {
                self.substitute_types_in_stmt(stmt, substitution, type_table, local_count, locals);
            }
        }
    }

    fn substitute_types_in_stmt(
        &self,
        stmt: &mut TirStmt,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        locals: &mut Vec<TirLocal>,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                *type_id = self.substitute_type(*type_id, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table, local_count, locals);
            }
            TirStmtKind::Expr(expr) => {
                self.substitute_types_in_expr(expr, substitution, type_table, local_count, locals);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.substitute_types_in_expr(
                        expr,
                        substitution,
                        type_table,
                        local_count,
                        locals,
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
                    locals,
                );
                self.substitute_types_in_block(
                    then_block,
                    substitution,
                    type_table,
                    local_count,
                    locals,
                );
                if let Some(else_blk) = else_block {
                    self.substitute_types_in_block(
                        else_blk,
                        substitution,
                        type_table,
                        local_count,
                        locals,
                    );
                }
            }
            TirStmtKind::Loop { body } => {
                self.substitute_types_in_block(body, substitution, type_table, local_count, locals);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.substitute_types_in_expr(v, substitution, type_table, local_count, locals);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.substitute_types_in_block(
                    block,
                    substitution,
                    type_table,
                    local_count,
                    locals,
                );
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.substitute_types_in_pattern(pattern, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table, local_count, locals);
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
        locals: &mut Vec<TirLocal>,
    ) {
        // Substitute the expression's own type
        expr.type_id = self.substitute_type(expr.type_id, substitution, type_table);

        if matches!(expr.kind, TirExprKind::VariadicTupleComprehension { .. }) {
            self.expand_tuple_comprehension(expr, substitution, type_table, local_count, locals);
            return;
        }

        match &mut expr.kind {
            TirExprKind::VariadicTupleComprehension { .. } => unreachable!("expanded above"),
            TirExprKind::Call {
                func: call_func,
                type_args,
                args,
                has_receiver: false,
            } => {
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                // A static method call carries method_info; substitute the type
                // args embedded in its mangled name too.
                let is_static_method = call_func.method_info.is_some();
                if is_static_method
                    && !substitution.is_empty()
                    && let Some(info) = call_func.method_info.clone()
                {
                    // Only a receiver the substitution answers carries its
                    // trait's arguments with it; every other instance keeps the
                    // template's spelling, which is what defines it.
                    let info = if info.is_type_param_receiver {
                        self.trait_named_at_instance(info, substitution, type_table)
                    } else {
                        info
                    };
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
                    } = &**call_func
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
                        && info.struct_type_args.is_empty()
                        && self.current_impl_receiver.as_ref() == Some(&info.fq_base_struct_name())
                    {
                        // The callee has no monomorph_info, but the outer function
                        // has impl-level type params AND the callee's struct matches
                        // the outer impl's struct (e.g., we're inside
                        // `impl<K,V> TreeMap<K,V>` and calling `TreeMap::new()`).
                        // The elaborator didn't annotate the bare struct reference with
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
                        // The struct name is a type parameter (e.g., T^Ord::cmp);
                        // resolve the concrete receiver by the param's name.
                        match self.receiver_substitution_tid(&info, substitution) {
                            Some(concrete_tid) => {
                                let dispatch_tid =
                                    self.type_param_dispatch_tid(concrete_tid, &info, type_table);
                                info.with_substituted_struct_name(
                                    &type_table.fq_type_name(dispatch_tid),
                                )
                            }
                            None => info.clone(),
                        }
                    } else {
                        // Apply the callee's own substituted type args.
                        // Type args feeding into MethodInfo names use the
                        // qualified mangle so call sites match the
                        // function-definition naming produced by the
                        // `method_instantiation_name*` helpers.
                        let impl_names: Vec<FqTypeName> = sub_impl_type_args
                            .iter()
                            .map(|&tid| type_table.fq_type_name(tid))
                            .collect();
                        let method_names: Vec<FqTypeName> = sub_method_type_args
                            .iter()
                            .map(|&tid| type_table.fq_type_name(tid))
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
                        } = &**call_func
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
                                .map(|&tid| type_table.fq_type_name(tid))
                                .collect();
                        }
                    }
                    let new_func_name = new_info.to_mangled_name();

                    // Each branch is gated on its own precondition, not on
                    // whether the mangled name changed. A type-param receiver
                    // needs queueing exactly when it resolved concretely — the
                    // name stays `S^…` for a user struct named `S`, and skipping
                    // it there leaves the instance unresolved at WIR build.
                    let receiver_tid = self
                        .receiver_substitution_tid(&info, substitution)
                        .map(|tid| self.type_param_dispatch_tid(tid, &info, type_table));
                    let receiver_is_concrete = receiver_tid.is_some_and(|tid| {
                        !matches!(
                            type_table.get(tid),
                            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
                        )
                    });

                    if info.is_type_param_receiver {
                        if receiver_is_concrete {
                            let concrete_type_id = receiver_tid
                                .expect("receiver_is_concrete implies a resolved receiver");
                            let receiver_module = self.impl_module_of_receiver(
                                type_table,
                                concrete_type_id,
                                &new_info,
                            );
                            // Consult `concrete_impl_module_for` only: letting a
                            // generic `impl<T> Trait for Foo<T>` in would route
                            // `&List<i32>^Inspect` to List's impl instead of the
                            // ref blanket's, dropping the leading `&` at codegen.
                            // A generic impl lives in the receiver's own module.
                            let trait_name_for_blanket = new_info.trait_decl();
                            let generic_or_concrete =
                                self.functions.generic_or_concrete_impl_module(
                                    &new_info,
                                    receiver_module.as_ref(),
                                    concrete_type_id,
                                    type_table,
                                );
                            let blanket = if generic_or_concrete.is_none() {
                                trait_name_for_blanket.and_then(|tn| {
                                    ranked_value_blanket(
                                        &self.functions.trait_env,
                                        tn,
                                        receiver_module.as_ref(),
                                        concrete_type_id,
                                        type_table,
                                    )
                                    .cloned()
                                })
                            } else {
                                None
                            };
                            let blanket_module = blanket.as_ref().map(|b| b.module.clone());
                            let concrete_impl_module = self
                                .functions
                                .impl_module(&new_info, receiver_module.as_ref());
                            let concrete_module =
                                concrete_impl_module.or(blanket_module).or(receiver_module);
                            let method_type_arg_tids: Vec<TypeId> = if let FunctionRef {
                                monomorph_info: Some(mi),
                                ..
                            } = &**call_func
                            {
                                mi.method_type_args
                                    .iter()
                                    .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            let impl_type_arg_tids: Vec<TypeId> = type_table
                                .generic_type_args(concrete_type_id)
                                .unwrap_or_default();
                            // A generic-instance receiver
                            // (`List<i32>^Default::default`) must queue its impl
                            // instantiation even with no method type args of its
                            // own — they are impl-level. A non-generic receiver
                            // (`i32^Default::default`) is a direct function,
                            // unless it dispatches through a blanket impl (see
                            // below).
                            let blanket_generic_name = blanket
                                .as_ref()
                                .map(|b| blanket_template_name(b, &new_info, type_table));
                            // The receiver, then whatever the blanket's bounds
                            // project off it. Keying by the receiver alone left
                            // a blanket reached from inside another instance's
                            // body — every link past the first of a newtype
                            // chain — with its other parameters unbound.
                            let blanket_impl_args = blanket.as_ref().and_then(|b| {
                                blanket_impl_args(
                                    &self.functions.trait_env,
                                    b,
                                    concrete_type_id,
                                    type_table,
                                )
                            });
                            let new_monomorph = if let Some(generic_name) = blanket_generic_name {
                                Some(MonomorphInfo {
                                    generic_name,
                                    impl_type_args: blanket_impl_args
                                        .unwrap_or_else(|| vec![concrete_type_id]),
                                    method_type_args: method_type_arg_tids,
                                    is_blanket: true,
                                })
                            } else if method_type_arg_tids.is_empty()
                                && impl_type_arg_tids.is_empty()
                            {
                                None
                            } else {
                                let base_info = LocalMethodName::new(
                                    new_info.fq_base_struct_name(),
                                    new_info.trait_name.clone(),
                                    new_info.method_name.clone(),
                                );
                                let generic_name = base_info.to_mangled_name();
                                Some(MonomorphInfo {
                                    generic_name,
                                    impl_type_args: impl_type_arg_tids,
                                    method_type_args: method_type_arg_tids,
                                    is_blanket: false,
                                })
                            };
                            // Generics have a defined home module by construction:
                            // either the trait-impl block's module, or — for
                            // auto-derived impls — the receiver type's module
                            // (`module_source_for_trait_impl`). Crash here rather
                            // than fall back to the call-site module; a failure
                            // means a missing prelude definition or an unhandled
                            // `ResolvedType` arm in `module_source_for_trait_impl`.
                            let resolved_module = concrete_module.unwrap_or_else(|| {
                                panic!(
                                    "no home module for generic dispatch `{}` \
                                         (concrete type id {:?}); add the missing impl \
                                         to the prelude, or extend \
                                         `module_source_for_trait_impl` for the receiver's \
                                         `ResolvedType` arm",
                                    new_info.to_mangled_name(),
                                    concrete_type_id,
                                )
                            });
                            let template = self.served_template(
                                blanket.as_ref(),
                                &new_info,
                                concrete_type_id,
                                &resolved_module,
                                type_table,
                            );
                            let resolved_module = template
                                .as_ref()
                                .map_or(resolved_module, |t| t.home(type_table.defs()));
                            **call_func = FunctionRef {
                                module_source: resolved_module,
                                name: new_func_name,
                                template,
                                monomorph_info: new_monomorph,
                                method_info: Some(new_info),
                            };
                        }
                    } else if new_func_name != old_func_name {
                        let template = self.static_template_at(
                            call_func.template.take(),
                            &new_info,
                            &sub_impl_type_args,
                            type_table,
                        );
                        let module_source = template
                            .as_ref()
                            .map_or(module_source, |t| t.home(type_table.defs()));
                        let monomorph_info = Some(MonomorphInfo {
                            generic_name: old_func_name,
                            impl_type_args: sub_impl_type_args,
                            method_type_args: sub_method_type_args,
                            is_blanket: false,
                        });
                        **call_func = FunctionRef {
                            module_source,
                            name: new_func_name,
                            template,
                            monomorph_info,
                            method_info: Some(new_info),
                        };
                    }
                }
                for arg in args {
                    self.substitute_types_in_expr(
                        &mut arg.expr,
                        substitution,
                        type_table,
                        local_count,
                        locals,
                    );
                }
            }
            TirExprKind::Call {
                func: method_func,
                type_args,
                args,
                has_receiver: true,
            } => {
                let Some((receiver, args)) = args.split_first_mut() else {
                    return;
                };
                let receiver = &mut receiver.expr;
                self.substitute_types_in_expr(
                    receiver,
                    substitution,
                    type_table,
                    local_count,
                    locals,
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
                        locals,
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
                    let recv_inner = type_table.peel_refs(receiver.type_id);
                    let trait_decl = trait_name_before.as_ref().and_then(FqTraitName::canonical);
                    // Which operator this is, by declaration: a user trait
                    // spelled `Neg` supplies no instruction of its own.
                    let item = trait_decl.and_then(|decl| {
                        type_table
                            .compiler_items()
                            .trait_item_of_decl(type_table.defs().ast_id(decl))
                    });
                    // The op decides first: a trait method that is no operator
                    // — `Display::to_string`, `Iterator::next` — must not pay
                    // for the receiver's impl-index query.
                    let unary = trait_method_to_unary_op(item, &method_name_before);
                    let binary = trait_method_to_binary_op(item, &method_name_before);
                    let lowers_to_scalar = (unary.is_some() || binary.is_some())
                        && self.operator_lowers_to_scalar(type_table, recv_inner, trait_decl);
                    if let Some(unary_op) = unary
                        && lowers_to_scalar
                    {
                        let operand = unref_operand(receiver, type_table);
                        expr.type_id = operand.type_id;
                        expr.kind = TirExprKind::Unary {
                            op: unary_op,
                            expr: Box::new(operand),
                        };
                    } else if let Some(binary_op) = binary
                        && lowers_to_scalar
                    {
                        let left = unref_operand(receiver, type_table);
                        let Some(arg) = args.first() else {
                            unreachable!("a binary-op trait method always takes one argument")
                        };
                        let mut right = unref_operand(&arg.expr, type_table);
                        // `Shl` / `Shr` declare `rhs: u32` whatever the
                        // receiver is; the native instruction takes both
                        // operands at one width.
                        if matches!(binary_op, TirBinaryOp::Shl | TirBinaryOp::Shr)
                            && right.type_id != left.type_id
                        {
                            let span = right.span;
                            right = TirExpr {
                                kind: TirExprKind::Cast {
                                    expr: Box::new(right),
                                    target_type: left.type_id,
                                },
                                type_id: left.type_id,
                                span,
                            };
                        }
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
                        locals,
                    );
                }
            }
            TirExprKind::Binary { op, left, right } => {
                self.substitute_types_in_expr(left, substitution, type_table, local_count, locals);
                self.substitute_types_in_expr(right, substitution, type_table, local_count, locals);
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
                    &self.functions.trait_env,
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
                self.substitute_types_in_expr(inner, substitution, type_table, local_count, locals);
            }
            TirExprKind::Block(block) => {
                self.substitute_types_in_block(
                    block,
                    substitution,
                    type_table,
                    local_count,
                    locals,
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
                    locals,
                );
                self.substitute_types_in_block(
                    then_branch,
                    substitution,
                    type_table,
                    local_count,
                    locals,
                );
                if let Some(else_blk) = else_branch {
                    self.substitute_types_in_block(
                        else_blk,
                        substitution,
                        type_table,
                        local_count,
                        locals,
                    );
                }
            }
            TirExprKind::ArrayLiteral { elements } => {
                // Homogeneous and spread-free: an array literal's elements are
                // plain expressions, so there is no second expansion pass.
                for elem in elements.iter_mut() {
                    self.substitute_types_in_expr(
                        elem,
                        substitution,
                        type_table,
                        local_count,
                        locals,
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
                            locals,
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
                            ..
                        } = elem.kind
                        {
                            // Expand type pack: for each concrete type in the pack,
                            // clone the expression and substitute with per-element types.
                            let pack_index = type_table
                                .param_slot(pack_type_id)
                                .expect("an expansion names its pack before substitution");
                            let concrete_pack =
                                self.substitute_type(pack_type_id, substitution, type_table);
                            let pack_elems = type_table.elem_types_or_self(concrete_pack);
                            for &elem_type in &pack_elems {
                                let mut elem_call = call_expr.as_ref().clone();
                                // A `return` here exits the *enclosing* function,
                                // whose return type names the whole pack, so
                                // resolve those slots under the outer
                                // substitution — the per-element pass below then
                                // has nothing left to rewrite on them.
                                ReturnTypeSlots::new(|slot: &mut TypeId| {
                                    *slot = self.substitute_type(*slot, substitution, type_table);
                                    assert!(
                                        !type_table.contains_type_param_index(*slot, pack_index),
                                        "return slot still names pack {pack_index}"
                                    );
                                })
                                .visit_expr(&mut elem_call);
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
                                    locals,
                                );
                                elements.push(elem_call);
                            }
                        } else {
                            elements.push(elem);
                        }
                    }
                    // Rebuild the tuple type from expanded element types.
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
                    locals,
                );
                self.substitute_types_in_expr(value, substitution, type_table, local_count, locals);
            }
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                *target_type = self.substitute_type(*target_type, substitution, type_table);
                self.substitute_types_in_expr(inner, substitution, type_table, local_count, locals);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.substitute_types_in_expr(inner, substitution, type_table, local_count, locals);
            }
            TirExprKind::TupleSpread { expr: inner } => {
                self.substitute_types_in_expr(inner, substitution, type_table, local_count, locals);
            }
            TirExprKind::TupleLen { expr: len_inner } => {
                self.substitute_types_in_expr(
                    len_inner,
                    substitution,
                    type_table,
                    local_count,
                    locals,
                );
                // After substitution the tuple arity is concrete; fold to a
                // literal. The receiver may be behind a `&`/`&mut` (e.g. a
                // `self: &[..T]` impl), so peel refs before reading the arity.
                let peeled = type_table.peel_refs(len_inner.type_id);
                if let Some(elems) = type_table.as_tuple(peeled) {
                    let len = elems.len() as u64;
                    expr.kind = TirExprKind::IntLiteral {
                        value: len,
                        repr: len.to_string(),
                    };
                    expr.type_id = TypeTable::I32;
                }
            }
            TirExprKind::TupleZip { expr: zip_inner } => {
                self.substitute_types_in_expr(
                    zip_inner,
                    substitution,
                    type_table,
                    local_count,
                    locals,
                );
                let inner_expr = zip_inner.as_ref().clone();
                let transposed = transpose_tuple_expr(&inner_expr, expr.span, type_table);
                expr.kind = transposed.kind;
                expr.type_id = transposed.type_id;
            }
            TirExprKind::TypePackExpansion {
                call_expr,
                pack_type_id,
                ..
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
                    locals,
                );
            }
            TirExprKind::Index { expr: array, index } => {
                self.substitute_types_in_expr(array, substitution, type_table, local_count, locals);
                self.substitute_types_in_expr(index, substitution, type_table, local_count, locals);
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
                    locals,
                );
                for arm in arms {
                    self.substitute_types_in_pattern(&mut arm.pattern, substitution, type_table);
                    if let Some(guard) = &mut arm.guard {
                        self.substitute_types_in_expr(
                            guard,
                            substitution,
                            type_table,
                            local_count,
                            locals,
                        );
                    }
                    self.substitute_types_in_expr(
                        &mut arm.body,
                        substitution,
                        type_table,
                        local_count,
                        locals,
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
                self.substitute_types_in_expr(body, substitution, type_table, local_count, locals);
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
                        locals,
                    );
                }

                // Then substitute struct_type
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);

                // Important: expr.type_id has already been substituted at the top
                // of this function. Use it to get the correct struct type and name
                // This handles the case where struct_type is a plain Struct but expr.type_id
                // has been properly substituted to the monomorphized version
                if expr.type_id != *struct_type {
                    *struct_type = expr.type_id;
                }

                // Update struct_name to match the (possibly monomorphized) struct_type
                match type_table.get(*struct_type) {
                    ResolvedType::Struct { def, type_args } => {
                        *struct_name = type_table.struct_rendered_name(*def, type_args);
                    }
                    ResolvedType::GenericInstance { def, type_args } => {
                        let name = &type_table.def_name(*def).to_string();
                        if type_args.is_empty() && !substitution.is_empty() {
                            // GenericInstance with empty type_args in a substitution context
                            // Build the name using the substitution map. Type
                            // args go through `mangle_type_arg_for_generic` so
                            // the resulting struct name matches what the
                            // function-definition side produces.
                            let mut sorted_entries: Vec<_> = substitution.iter().collect();
                            sorted_entries.sort_by_key(|(idx, _)| **idx);
                            let args: Vec<String> = sorted_entries
                                .iter()
                                .map(|(_, tid)| type_table.mangle_type_arg_for_generic(**tid))
                                .collect();
                            *struct_name = mangle_generic_name(name, &args);
                        } else {
                            // For generic instances like Container<i32>, compute the mangled name.
                            let args: Vec<String> = type_args
                                .iter()
                                .map(|arg| type_table.mangle_type_arg_for_generic(*arg))
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
                    locals,
                );
                for arg in args {
                    self.substitute_types_in_expr(
                        arg,
                        substitution,
                        type_table,
                        local_count,
                        locals,
                    );
                }
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
                        locals,
                    );
                }
                // After substitution, if variant_type is still a bare Variant (from
                // generic library code), convert it to a GenericInstance using the
                // payload type as type arg (e.g., Option + &mut Node<String> → Option<&mut Node<String>>).
                // Only promote if the payload type was actually changed by substitution,
                // indicating the variant is generic. Non-generic variants like
                // `Shape { Circle(f64), Point }` have concrete payload types that aren't
                // affected by substitution and should NOT be promoted to GenericInstance.
                if let ResolvedType::Variant { def } = type_table.get(*variant_type).clone()
                    && let Some(payload_expr) = payload
                    && original_payload_type.is_some_and(|orig| orig != payload_expr.type_id)
                {
                    // The bare variant already names its declaration, so the
                    // instance is interned against that one — `Option` included.
                    let new_id = type_table.make_generic_instance(def, vec![payload_expr.type_id]);
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
                    locals,
                );
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.substitute_types_in_expr(value, substitution, type_table, local_count, locals);
            }
            TirExprKind::VariantTag { expr } => {
                self.substitute_types_in_expr(expr, substitution, type_table, local_count, locals);
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.substitute_types_in_expr(expr, substitution, type_table, local_count, locals);
            }
            TirExprKind::VariantPayload {
                expr, payload_type, ..
            } => {
                self.substitute_types_in_expr(expr, substitution, type_table, local_count, locals);
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
            }
            // A `FuncRef` carries `type_args` when the reference was pinned
            // by turbofish or expected-type inference. When the enclosing
            // function is itself generic, those args may contain `TypeParam`
            // ids that still need substitution before the monomorph collector
            // can queue the right instance.
            TirExprKind::FuncRef { type_args, .. } => {
                for arg in type_args.iter_mut() {
                    *arg = self.substitute_type(*arg, substitution, type_table);
                }
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
                            locals,
                        );
                    }
                }
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }

    /// A type-param receiver whose param resolves to a *reference*
    /// (`..F::inspect()` on a `&List<i32>` struct field) dispatches through the
    /// ref/mutref blanket (`impl<T: Inspect> Inspect for &T`), keyed by shape.
    /// The general path peels every ref off the auto-`&self`'d receiver, which
    /// would collapse `&List<i32>^Inspect` to `List<i32>::inspect` and drop the
    /// leading `&`. Resolve the param's own value (ref intact) and, if it is a
    /// reference to a universal-ref-blanket trait, route to that blanket in its
    /// own module and report `true`. Otherwise leave `method_func` untouched.
    fn try_ref_blanket_shortcut(
        &self,
        method_func: &mut FunctionRef,
        info: &LocalMethodName,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> bool {
        if !info.is_type_param_receiver {
            return false;
        }
        let Some(self_tid) = self.receiver_substitution_tid(info, substitution) else {
            return false;
        };
        let (ref_kind, inner) = match type_table.get(self_tid) {
            ResolvedType::Ref(inner) => (RefKind::Shared, *inner),
            ResolvedType::MutRef(inner) => (RefKind::Mut, *inner),
            _ => return false,
        };
        let Some(trait_fq) = info.trait_name.as_ref() else {
            return false;
        };
        let Some(trait_) = trait_fq.canonical() else {
            return false;
        };
        let Some(blanket) = self.functions.trait_env.universal_ref_blanket(
            trait_,
            &type_table.fq_type_name(self_tid),
            trait_fq.args(),
        ) else {
            return false;
        };
        // Mirror the template ref arm (`method_call_info_for_type`): the call
        // name carries the shape + inner type; `call_rewrite` resolves it to the
        // queued `&<inner>^Trait::method` instance via the blanket `monomorph_info`.
        let inner_name = type_table.fq_type_name(inner);
        let ref_info =
            LocalMethodName::new_ref(ref_kind, Some(trait_fq.clone()), info.method_name.clone())
                .with_struct_type_args(&[inner_name]);
        let generic_name =
            LocalMethodName::new_ref(ref_kind, Some(trait_fq.clone()), info.method_name.clone())
                .to_mangled_name();
        // The method's own type arguments belong to the call, not to the
        // blanket, and are written in the enclosing body's type parameters, so
        // they take the same substitution the receiver did.
        let method_type_args: Vec<TypeId> = method_func
            .monomorph_info
            .iter()
            .flat_map(|m| &m.method_type_args)
            .map(|arg| self.substitute_type(*arg, substitution, type_table))
            .collect();
        let template = self
            .functions
            .trait_env
            .method_template(blanket.def, &info.method_name);
        *method_func = FunctionRef {
            module_source: blanket.module.clone(),
            name: ref_info.to_mangled_name(),
            template,
            monomorph_info: Some(MonomorphInfo {
                generic_name,
                impl_type_args: vec![inner],
                method_type_args,
                is_blanket: true,
            }),
            method_info: Some(ref_info),
        };
        true
    }

    /// The name with the template's type parameters replaced in the trait's
    /// arguments: `T^Add<T>::add` under `T = Meters` names `Add<Meters>`, and
    /// `T^Make<T::Base>::make` under `T = UserName` names `Make<String>`.
    fn trait_named_at_instance(
        &self,
        info: LocalMethodName,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &TypeTable,
    ) -> LocalMethodName {
        let Some(trait_name) = info.trait_name.as_ref() else {
            return info;
        };
        if !trait_name.args_mention_binder() {
            return info;
        }
        let bound = |name: &str| -> Option<TypeId> {
            let key = self.current_param_substitution_key.get(name)?;
            substitution.get(key).copied()
        };
        let args: Vec<FqTypeName> = trait_name
            .args()
            .iter()
            .map(|arg| Self::trait_arg_at_instance(arg, &bound, type_table))
            .collect();
        info.with_trait_type_args(&args)
    }

    /// One trait argument re-spelled at the instance, at every position a type
    /// stands in: `Make<List<T::Base>>` under `T = Bag` names `Make<List<String>>`.
    ///
    /// A position the frame does not bind stays as written: the instance is
    /// still inside a template, and the substitution that does bind it settles
    /// its name.
    fn trait_arg_at_instance(
        arg: &FqTypeName,
        bound: &impl Fn(&str) -> Option<TypeId>,
        type_table: &TypeTable,
    ) -> FqTypeName {
        arg.rewrite(&|node| {
            let answer = Self::type_at_instance(node, bound, type_table)?;
            Some(type_table.fq_type_name(answer))
        })
    }

    /// The type a name stands for at the instance: a binder the frame binds, or
    /// a projection off one — through any depth, since `C::Iter::Item` answers
    /// only once its own base does.
    fn type_at_instance(
        node: &FqTypeName,
        bound: &impl Fn(&str) -> Option<TypeId>,
        type_table: &TypeTable,
    ) -> Option<TypeId> {
        if let Some((base, assoc, owning_trait)) = node.projected() {
            let base_id = Self::type_at_instance(base, bound, type_table)?;
            return type_table.resolve_assoc_type_qualified(base_id, &owning_trait, assoc);
        }
        if !node.args().is_empty() {
            return None;
        }
        bound(node.binder_name()?)
    }

    /// The name with the trait's arguments cut back to what the answering impl
    /// writes. An instance minted under a longer name defines nothing.
    fn named_by_impl(&self, info: LocalMethodName) -> LocalMethodName {
        let shorter = || {
            let trait_fq = info.trait_name.as_ref()?;
            if trait_fq.args().is_empty() {
                return None;
            }
            let trait_ = self.functions.trait_env.trait_def_of_fq(trait_fq)?;
            let kept = self.functions.trait_env.impl_written_arg_count(
                info.receiver(),
                trait_,
                trait_fq.args(),
            )?;
            (kept < trait_fq.args().len())
                .then(|| info.with_trait_type_args(&trait_fq.args()[..kept]))
        };
        shorter().unwrap_or(info)
    }

    /// Whether `func` calls a universal `&T` blanket at a pointee the
    /// substitution cannot reach, which leaves it already concrete.
    fn is_concrete_ref_blanket_call(&self, func: &FunctionRef, type_table: &TypeTable) -> bool {
        let (
            Some(TemplateId::Declared {
                block: Some(block), ..
            }),
            Some(monomorph),
        ) = (&func.template, &func.monomorph_info)
        else {
            return false;
        };
        self.functions
            .trait_env
            .blanket_of_block(*block)
            .is_some_and(|blanket| matches!(blanket.receiver, BlanketReceiver::Ref { .. }))
            && !monomorph
                .impl_type_args
                .iter()
                .chain(&monomorph.method_type_args)
                .any(|&arg| type_table.contains_type_param(arg))
    }

    /// Resolve a method call in a generic body to its concrete target after
    /// substitution, delegating by receiver kind: a reference type-param to
    /// [`Self::try_ref_blanket_shortcut`], a type-param (`T^Ord::cmp` →
    /// `i32^Ord::cmp`) to [`Self::resolve_type_param_dispatch`], anything else
    /// (`List<T>::len` → `List<i32>::len`) to [`Self::resolve_generic_dispatch`].
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
        let info = self.trait_named_at_instance(info, substitution, type_table);

        if self.try_ref_blanket_shortcut(method_func, &info, substitution, type_table)
            || self.is_concrete_ref_blanket_call(method_func, type_table)
        {
            return;
        }

        // Check if the struct actually needs type arg substitution.
        // Skip for non-generic structs (e.g., String::push_str from template strings)
        // that happen to appear inside a generic impl block.
        let has_explicit_type_params = info.struct_name() != info.base_struct_name();
        let receiver_is_generic = {
            let base = type_table.peel_refs(receiver_type_id);
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
                    | ResolvedType::Struct { .. }
            )
        };
        // Newtype-override guard: when the method already names a newtype with
        // its OWN impl of this trait (e.g. `ByteList^Serialize::serialize`) and
        // the receiver IS that newtype, keep the name as-is. The
        // `receiver_is_generic` path below would otherwise peel the newtype to
        // its erased base (`List<u8>`) and retarget at the inherited base impl.
        if self.receiver_keeps_newtype_own_impl(receiver_type_id, type_table, &info) {
            return;
        }

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

        // Left unsubstituted, the instance keys on the template's abstract `S`.
        let sub_method_type_args: Vec<TypeId> = match &*method_func {
            FunctionRef {
                monomorph_info: Some(mi),
                ..
            } => mi
                .method_type_args
                .iter()
                .map(|&tid| self.substitute_type(tid, substitution, type_table))
                .collect(),
            _ => Vec::new(),
        };

        // Compute the new method info with concrete type names.
        // If the struct is a type param (e.g., T^Ord::cmp), substitute the struct
        // name directly instead of adding type args.
        let mut new_info = if info.is_type_param_receiver && !type_names.is_empty() {
            // Use the (already-substituted) receiver type to find the concrete name.
            let inner = type_table.peel_refs(receiver_type_id);
            // For newtypes/flags: first try the newtype's own name (e.g., "Meters"),
            // then fall back to the base type name (e.g., "f64") if no direct impl exists.
            let candidate = self
                .named_by_impl(info.with_substituted_struct_name(&type_table.fq_type_name(inner)));
            if self.functions.has_impl(&candidate)
                || self.reflect_blanket_claims(&info, inner, type_table)
            {
                candidate
            } else if let Some(link) = info
                .trait_name
                .as_ref()
                .and_then(|trait_name| self.functions.trait_env.trait_def_of_fq(trait_name))
                .and_then(|trait_| self.newtype_link_with_trait_impl(inner, type_table, trait_))
            {
                self.named_by_impl(
                    info.with_substituted_struct_name(&type_table.fq_type_name(link)),
                )
            } else {
                // Newtypes must inherit the underlying head, else the trait_env
                // candidate lookup misses the per-type impl.
                let resolved_inner = type_table.representation_head(inner);
                self.named_by_impl(
                    info.with_substituted_struct_name(&type_table.fq_type_name(resolved_inner)),
                )
            }
        } else if needs_struct_type_args {
            // Resolve through newtypes so the receiver matches the TraitEnv key
            // for the template's home module (issue #1110).
            let recv_inner = type_table.peel_refs(receiver_type_id);
            let resolved_recv = type_table.representation_head(recv_inner);
            let mut new_info =
                info.with_substituted_struct_name(&type_table.fq_type_name(resolved_recv));
            // For ref-type impls (e.g., impl IntoIterator for &List<T>), preserve
            // the ref receiver (`&` / `&mut`) so that the monomorphizer selects the
            // correct generic function template ("&^IntoIterator::into_iter" instead
            // of "List^IntoIterator::into_iter").
            if info.is_ref_impl {
                new_info.receiver = info.receiver.clone();
            }
            new_info
        } else {
            info.clone()
        };
        if !sub_method_type_args.is_empty() {
            new_info.method_type_args = sub_method_type_args
                .iter()
                .map(|&tid| type_table.fq_type_name(tid))
                .collect();
        }
        let new_func_name = new_info.to_mangled_name();

        if new_func_name == old_func_name {
            return;
        }

        let call = SubstitutedCall {
            info: new_info,
            mangled: new_func_name,
            original_name: old_func_name,
            type_args,
            method_type_args: sub_method_type_args,
            module_source,
        };
        if info.is_type_param_receiver {
            self.resolve_type_param_dispatch(method_func, receiver_type_id, type_table, call);
        } else {
            self.resolve_generic_dispatch(
                method_func,
                receiver_type_id,
                substitution,
                type_table,
                call,
            );
        }
    }

    /// Whether `receiver` derives `info`'s trait through a `Reflect*` blanket.
    ///
    /// A `flags` type erases to `u32` and an erased base carries no reflect
    /// kind, so peeling the receiver before the blanket lookup drops the
    /// derivation its declaration owns. A newtype over a non-reflect base keeps
    /// inheriting the base's impl.
    fn reflect_blanket_claims(
        &self,
        info: &LocalMethodName,
        receiver: TypeId,
        type_table: &TypeTable,
    ) -> bool {
        let Some(trait_name) = info.trait_decl() else {
            return false;
        };
        if !has_reflect_kind(receiver, type_table) {
            return false;
        }
        let receiver_module = module_source_for_trait_impl(type_table, receiver);
        ranked_value_blanket(
            &self.functions.trait_env,
            trait_name,
            receiver_module.as_ref(),
            receiver,
            type_table,
        )
        .is_some_and(|blanket| blanket_is_reflect_keyed(&blanket.bounds, type_table))
    }

    /// Route a type-param receiver (`T^Trait::method`, resolved to a concrete
    /// type) to its impl: a concrete/generic per-type impl in the receiver's
    /// module, or — when the type has none — a blanket impl in the blanket's
    /// module. Only claims the blanket if the receiver satisfies its param
    /// bound, and keys the `ReflectStruct` struct blanket by `[T, Fields]`.
    fn resolve_type_param_dispatch(
        &self,
        method_func: &mut FunctionRef,
        receiver_type_id: TypeId,
        type_table: &mut TypeTable,
        call: SubstitutedCall,
    ) {
        let SubstitutedCall {
            info: new_info,
            mangled: new_func_name,
            original_name: old_func_name,
            type_args,
            method_type_args,
            module_source,
        } = call;
        let receiver_module = {
            let inner = type_table.peel_refs(receiver_type_id);
            self.impl_module_of_receiver(type_table, inner, &new_info)
        };
        // Consult `concrete_impl_module_for` only: a broader `impl_module_for`
        // would route `&List<i32>^Inspect` to List's generic impl instead of the
        // ref blanket's, dropping the leading `&` at codegen. With no concrete
        // impl, a generic one lives in the receiver type's own module — how
        // newtype inheritance reuses it — and only a blanket in `blanket_impls`.
        let trait_name_for_blanket = new_info.trait_decl();
        let generic_or_concrete = self.functions.generic_or_concrete_impl_module(
            &new_info,
            receiver_module.as_ref(),
            receiver_type_id,
            type_table,
        );
        // Module and receiver param must be read off this same blanket: the
        // call-site type-param head matches only a direct `T::method` call.
        let blanket = if generic_or_concrete.is_none() {
            let recv_inner = type_table.peel_refs(receiver_type_id);
            trait_name_for_blanket.and_then(|tn| {
                ranked_value_blanket(
                    &self.functions.trait_env,
                    tn,
                    receiver_module.as_ref(),
                    recv_inner,
                    type_table,
                )
                .cloned()
            })
        } else {
            None
        };
        let blanket_module = blanket.as_ref().map(|b| b.module.clone());
        let template = self.served_template(
            blanket.as_ref(),
            &new_info,
            type_table.peel_refs(receiver_type_id),
            receiver_module.as_ref().unwrap_or(&module_source),
            type_table,
        );
        let concrete_module = template
            .as_ref()
            .map(|template| template.home(type_table.defs()))
            .or(receiver_module);

        let receiver_has_type_args = {
            let inner = type_table.peel_refs(receiver_type_id);
            // Peel newtypes: `type FieldValue = List<u8>` inherits
            // List's generic-impl dispatch, so the call must not be
            // marked blanket even though FieldValue itself has no impl.
            let resolved = type_table.representation_head(inner);
            matches!(
                type_table.get(resolved),
                ResolvedType::GenericInstance {
                    type_args: args, ..
                } if !args.is_empty()
            ) || matches!(type_table.get(resolved), ResolvedType::BuiltinArray(_))
        };
        // A generic-instance receiver normally reaches a generic impl on its
        // own type, which the receiver scan instantiates. Reaching a blanket
        // instead means the type has no impl of its own, so nothing else queues
        // that instance.
        let served_by_receiver_scan = receiver_has_type_args && blanket_module.is_none();
        let monomorph_info = if self.functions.has_impl(&new_info) || served_by_receiver_scan {
            None
        } else {
            // Blanket impl: an associated-type projection (`S::SeqSerializer^…`)
            // keeps `new_func_name`; every other blanket is keyed by its own
            // receiver param. The impl args below stay `ReflectStruct`-only.
            let recv_inner = type_table.peel_refs(receiver_type_id);
            // A blanket `impl<T: Bound<Assoc = P>, P> Trait for T` is keyed by
            // `[T, T::Assoc, …]`, so its instance name matches the template's
            // arity. A plain one-arg blanket (`impl<I: Iterator> IntoIterator
            // for I`) projects nothing and stays keyed by the call-site args.
            let projected = blanket.as_ref().and_then(|b| {
                blanket_impl_args(&self.functions.trait_env, b, recv_inner, type_table)
            });
            let has_projected = projected.as_ref().is_some_and(|args| args.len() > 1);
            let blanket_name = if let Some(b) = blanket.as_ref() {
                blanket_template_name(b, &new_info, type_table)
            } else {
                old_func_name
            };
            let blanket_impl_args = match projected {
                Some(args) if has_projected => args,
                // A value blanket is keyed by the receiver it serves. The
                // enclosing function's arguments name that only where the
                // receiver is its first parameter, never for `X::Item`.
                _ if blanket.is_some() => vec![recv_inner],
                _ => type_args,
            };
            Some(MonomorphInfo {
                generic_name: blanket_name,
                impl_type_args: blanket_impl_args,
                // A pack-bound blanket's template is shared across every
                // (subject, method arg) pair, so its instance needs both. A
                // per-type impl already carries its method args in the mangled
                // name; keying on them too mints a second instance under it.
                method_type_args: if has_projected {
                    method_type_args
                } else {
                    Vec::new()
                },
                is_blanket: true,
            })
        };
        let home = concrete_module.unwrap_or_else(|| module_source.clone());
        *method_func = FunctionRef {
            module_source: home,
            name: new_func_name,
            template,
            monomorph_info,
            method_info: Some(new_info),
        };
    }

    /// Normal monomorphization for a concrete/generic receiver (`List<T>::len` →
    /// `List<i32>::len`): keep any existing blanket `monomorph_info` (substituting
    /// its impl args), and re-resolve the body's home module through `TraitEnv`
    /// for the post-substitution name.
    fn resolve_generic_dispatch(
        &self,
        method_func: &mut FunctionRef,
        receiver_type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        call: SubstitutedCall,
    ) {
        let SubstitutedCall {
            info: new_info,
            mangled: new_func_name,
            original_name: old_func_name,
            type_args,
            method_type_args: _,
            module_source,
        } = call;
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
        // The body was checked against a block reaching every instance its
        // parameters stood for; at this one a block written for it alone wins.
        let written = method_func.template.take();
        let template = self
            .dispatch_template(&new_info, receiver_type_id, &module_source, type_table)
            .or_else(|| written.clone());
        let same_block = template == written;
        let existing_is_blanket = existing_is_blanket && same_block;
        // The call's own impl args name its block's parameters; the enclosing
        // substitution names them only where the call has none of its own.
        let final_impl_ta = match existing_impl_ta {
            Some(args) if same_block => args
                .iter()
                .map(|&tid| self.substitute_type(tid, substitution, type_table))
                .collect(),
            _ => type_args,
        };
        let final_method_ta = existing_method_ta.unwrap_or_default();
        let monomorph_info = Some(MonomorphInfo {
            generic_name: existing_generic_name.unwrap_or(old_func_name),
            impl_type_args: final_impl_ta,
            method_type_args: final_method_ta,
            is_blanket: existing_is_blanket,
        });
        let resolved_module = template
            .as_ref()
            .map_or(module_source, |template| template.home(type_table.defs()));
        *method_func = FunctionRef {
            module_source: resolved_module,
            name: new_func_name,
            template,
            monomorph_info,
            method_info: Some(new_info),
        };
    }

    /// Expand a `VariadicForOf` TIR node into concrete unrolled blocks.
    ///
    /// After type substitution resolves `TypePack` to a concrete tuple, this generates
    /// the same structure as the elaborator's `resolve_tuple_for_of`.
    fn expand_variadic_for_of(
        &self,
        stmt: &mut TirStmt,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        locals: &mut Vec<TirLocal>,
    ) -> Vec<TirStmt> {
        let span = stmt.span;
        let TirStmtKind::VariadicForOf {
            iterable,
            binding_name,
            binding_local,
            is_mut,
            body,
            unique_id,
            by_ref,
            is_enumerate,
        } = &mut stmt.kind
        else {
            unreachable!()
        };
        let by_ref = *by_ref;
        let is_enumerate = *is_enumerate;

        // A mapped pack (`[..Case<T, P>]`) substitutes per element from the
        // source pack element `P_k`, not the mapped tuple element, so read its
        // index and mapped-ness off the pre-substitution type.
        let (iterable_pack_index, iterable_pack_mapped) = type_table
            .as_tuple_through_ref(iterable.type_id)
            .and_then(|(elems, _)| {
                elems.iter().find_map(|&e| match type_table.get(e) {
                    ResolvedType::TypePack {
                        index, mapped_elem, ..
                    } => Some((Some(*index), mapped_elem.is_some())),
                    _ => None,
                })
            })
            .unwrap_or((None, false));

        // Substitute types in the iterable to get the concrete tuple type
        self.substitute_types_in_expr(iterable, substitution, type_table, local_count, locals);

        // Get the concrete tuple elements. When iterating by reference the
        // iterable type is `&[concrete...]`; look through the wrapper.
        let iterable_type = iterable.type_id;
        let (elements, _) = type_table
            .as_tuple_through_ref(iterable_type)
            .unwrap_or_else(|| {
                panic!(
                    "VariadicForOf: expected concrete Tuple after substitution, got {:?}",
                    type_table.get(iterable_type)
                );
            });

        // The pack index to override per element; fall back to a tuple-valued
        // substitution entry when the iterable carries none.
        let pack_index = iterable_pack_index.or_else(|| {
            let mut found = None;
            for (&idx, &tid) in substitution {
                if tid == iterable_type || type_table.is_tuple(tid) {
                    found = Some(idx);
                    break;
                }
            }
            found
        });

        // For a mapped pack the override values are the source elements `P_k`;
        // the mapped element `Case<V, P_k>` stays the binding type only.
        let mapped_source_elems: Option<Vec<TypeId>> = if iterable_pack_mapped {
            pack_index
                .and_then(|idx| self.pack_source_tuple(idx, substitution))
                .and_then(|t| type_table.as_tuple(t))
        } else {
            None
        };

        let uid = *unique_id;
        let temp_name = format!("$tuple_{uid}");
        let binding_local_idx = *binding_local;
        let b_name = binding_name.clone();
        let b_mut = *is_mut;

        // `.enumerate()` binds `[i, v]`, so the index is the sub-binding
        // reading field 0 of the pair. A `t[i]` in the body folds against it
        // once the element position is known.
        let enumerate_index_local: Option<u32> = is_enumerate
            .then(|| {
                body.stmts.iter().find_map(|s| {
                    let TirStmtKind::Let {
                        local_index, value, ..
                    } = &s.kind
                    else {
                        return None;
                    };
                    let TirExprKind::FieldAccess {
                        expr: inner,
                        field_index: 0,
                        ..
                    } = &value.kind
                    else {
                        return None;
                    };
                    matches!(&inner.kind, TirExprKind::Local { index, .. } if *index == binding_local_idx)
                        .then_some(*local_index)
                })
            })
            .flatten();

        // Allocate a dedicated temp local for the tuple (the original binding_local
        // will be reused per-iteration below with distinct types per element).
        //
        // The temp is private to this unroll and each of its fields is read by
        // exactly one iteration, so every element binding below moves its field
        // out (`skip_value_copy`) instead of deep-copying it.
        let temp_local_idx = *local_count;
        *local_count += 1;
        locals.push(TirLocal {
            name: temp_name.clone(),
            type_id: iterable_type,
            is_mut: false,
            span: Span::default(),
        });

        let destruct_count = Self::destructure_prefix_len(body, binding_local_idx);

        // A destructured `.enumerate()` binds the pair's fields, never the pair,
        // so they come from the index literal and the element directly — no pair,
        // and no value copy taken into it. `[i, _]` then reads nothing off the
        // temp, whose binding would deep-copy the whole tuple.
        let inline_enumerate_pair = is_enumerate && destruct_count > 0;
        let temp_read =
            !inline_enumerate_pair || Self::binds_element_field(&body.stmts[..destruct_count]);

        let mut outer_stmts = Vec::new();

        // let $tuple_N = iterable;
        if temp_read || !Self::is_pure_place(iterable) {
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
        }

        // For each element, create: { let v = $tuple_N.i; body }
        for (i, &elem_type) in elements.iter().enumerate() {
            let mut iter_stmts = Vec::new();

            // Allocate a unique binding local per iteration (each element has a different type)
            let iter_binding = *local_count;
            *local_count += 1;

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
            // By reference (`for v of &[..T]`), the binding is `&T_k`: a
            // reference to a fresh copy of the field. Otherwise it is `T_k`.
            let (elem_bind_type, elem_bind_value) =
                type_table.tuple_element_binding(field, elem_type, by_ref, span);
            let index_literal = TirExpr::new(
                TirExprKind::IntLiteral {
                    value: i as u64,
                    repr: i.to_string(),
                },
                TypeTable::I32,
                span,
            );
            let bind_type = if is_enumerate {
                type_table.make_tuple(vec![TypeTable::I32, elem_bind_type])
            } else {
                elem_bind_type
            };
            locals.push(TirLocal {
                name: b_name.clone(),
                type_id: bind_type,
                is_mut: b_mut,
                span: Span::default(),
            });

            if !inline_enumerate_pair {
                let bind_value = if is_enumerate {
                    TirExpr::new(
                        TirExprKind::TupleLiteral {
                            elements: vec![index_literal.clone(), elem_bind_value.clone()],
                        },
                        bind_type,
                        span,
                    )
                } else {
                    elem_bind_value.clone()
                };
                iter_stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: b_name.clone(),
                        local_index: iter_binding,
                        is_mut: b_mut,
                        is_reactive: false,
                        type_id: bind_type,
                        value: bind_value,
                        skip_value_copy: true,
                    },
                    span,
                ));
            }

            // Clone the body and substitute types with per-element substitution.
            // Override the TypePack → elem_type (instead of TypePack → tuple type)
            let mut elem_body = body.clone();
            if let Some(pack_idx) = pack_index {
                // The pack's own tuple, for types in the body that spell the
                // whole pack (`[..T]`) rather than the element the loop binds.
                let pack_tuple = self.pack_source_tuple(pack_idx, substitution);

                if destruct_count > 0 {
                    // Destructured zip patterns only arise from `for [a, b] of
                    // x.zip(y)`, whose iterable is a tuple *value*, never a
                    // reference — so `by_ref` and this branch are mutually
                    // exclusive. The branch rebuilds the destructure with
                    // non-reference field types, which would be wrong for a
                    // `&T_k` binding; assert the combination never reaches here.
                    // `.enumerate()` is exempt: its pair fields are read off
                    // `bind_type`, which already carries the `&`.
                    assert!(
                        !by_ref || is_enumerate,
                        "by-reference variadic for-of with a destructured zip binding is unsupported"
                    );
                    // For destructured zip patterns, substitute_type's Tuple splicing
                    // corrupts the pair type Tuple([TypePack, TypePack]) → Tuple([T0,T1,...,T0,T1,...]).
                    // Instead, generate fresh destructured Let stmts with correct field types
                    // from elem_type, and only substitute the remaining user body stmts.
                    //
                    // `.enumerate()`'s fields come from the synthesized pair.
                    let pair_fields = if is_enumerate {
                        type_table
                            .as_tuple(bind_type)
                            .unwrap_or_else(|| vec![TypeTable::I32, elem_type])
                    } else {
                        type_table.elem_types_or_self(elem_type)
                    };

                    // The body_pack_type is the individual field type (e.g., [i32, i32] → i32).
                    // For non-tuple field types this is just the field type itself.
                    //
                    // Under `.enumerate()` the binding is the mapped element,
                    // but the body's own pack positions still mean the source
                    // element — `[..Option<F>]` binds `Option<F_k>` and spells
                    // `F` as `F_k`.
                    let body_pack_type = if is_enumerate {
                        mapped_source_elems
                            .as_ref()
                            .and_then(|es| es.get(i).copied())
                            .unwrap_or(elem_type)
                    } else {
                        pair_fields[0]
                    };

                    // Replace the destructured stmts with fresh ones that have correct types.
                    let mut fresh_destruct_stmts = Vec::with_capacity(destruct_count);
                    let mut pinned: Vec<(u32, TypeId)> = Vec::new();
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
                            // The field a sub-binding reads is its pattern
                            // position, which the template recorded; a wildcard
                            // makes it differ from its position in the list.
                            let field_index = match &orig_value.kind {
                                TirExprKind::FieldAccess { field_index, .. } => *field_index,
                                _ => j as u32,
                            };
                            let field_type = pair_fields
                                .get(field_index as usize)
                                .copied()
                                .unwrap_or(body_pack_type);
                            // The `locals` entry is left to
                            // `reconcile_unrolled_body_locals`: writing it here
                            // would let the last iteration's type clobber the
                            // slot the first one still holds.
                            let field_access = if inline_enumerate_pair {
                                if field_index == 0 {
                                    index_literal.clone()
                                } else {
                                    elem_bind_value.clone()
                                }
                            } else {
                                let binding_ref = TirExpr::new(
                                    TirExprKind::Local {
                                        index: binding_local_idx,
                                        name: b_name.clone(),
                                    },
                                    bind_type,
                                    span,
                                );
                                TirExpr::new(
                                    TirExprKind::FieldAccess {
                                        expr: Box::new(binding_ref),
                                        field_index,
                                        field_name: field_index.to_string(),
                                    },
                                    field_type,
                                    span,
                                )
                            };
                            pinned.push((*local_index, field_type));
                            fresh_destruct_stmts.push(TirStmt::new(
                                TirStmtKind::Let {
                                    name: name.clone(),
                                    local_index: *local_index,
                                    is_mut: *is_mut,
                                    is_reactive: *is_reactive,
                                    type_id: field_type,
                                    value: field_access,
                                    skip_value_copy: true,
                                },
                                span,
                            ));
                        }
                    }

                    // Extract only the user body stmts (after destructuring)
                    let user_stmts: Vec<TirStmt> =
                        elem_body.stmts.drain(destruct_count..).collect();
                    let mut user_body = TirBlock::new(user_stmts, span);

                    // Decide this element's bindings before the body is
                    // substituted: a nested unroll over the same pack
                    // substitutes it again with *its* element, and a use of
                    // this binding would follow it.
                    for (local, ty) in &pinned {
                        self.pin_binding_types(&mut user_body, *local, *ty, type_table);
                    }

                    // Substitute user body with field type (not pair type)
                    let mut elem_substitution = substitution.clone();
                    elem_substitution.insert(pack_idx, body_pack_type);
                    let displaced = pack_tuple.map(|t| self.bind_pack_splice(pack_idx, t));
                    self.substitute_types_in_block(
                        &mut user_body,
                        &elem_substitution,
                        type_table,
                        local_count,
                        locals,
                    );
                    if let Some(previous) = displaced {
                        self.restore_pack_splice(pack_idx, previous);
                    }

                    // Reassemble: fresh destructured stmts + substituted user body
                    elem_body.stmts = fresh_destruct_stmts;
                    elem_body.stmts.extend(user_body.stmts);
                } else {
                    let mut elem_substitution = substitution.clone();
                    let override_ty = mapped_source_elems
                        .as_ref()
                        .and_then(|es| es.get(i).copied())
                        .unwrap_or(elem_type);
                    elem_substitution.insert(pack_idx, override_ty);
                    self.pin_binding_types(
                        &mut elem_body,
                        binding_local_idx,
                        bind_type,
                        type_table,
                    );
                    let displaced = pack_tuple.map(|t| self.bind_pack_splice(pack_idx, t));
                    self.substitute_types_in_block(
                        &mut elem_body,
                        &elem_substitution,
                        type_table,
                        local_count,
                        locals,
                    );
                    if let Some(previous) = displaced {
                        self.restore_pack_splice(pack_idx, previous);
                    }
                }
            } else {
                // Fallback: no TypePack index in the substitution. A
                // `VariadicForOf` is only built for a tuple that contains a
                // TypePack, so `pack_index` is `Some` whenever this node
                // exists; this arm is defensive. It rewrites the binding to
                // the bare `elem_type`, so a `&T_k` (by_ref) binding could not
                // be reconstructed here — assert the combination is unreachable.
                assert!(
                    !by_ref,
                    "by-reference variadic for-of reached the no-TypePack fallback"
                );
                // Substitute with original and rewrite manually
                self.substitute_types_in_block(
                    &mut elem_body,
                    substitution,
                    type_table,
                    local_count,
                    locals,
                );
                self.pin_binding_types(&mut elem_body, binding_local_idx, elem_type, type_table);
            }
            // Fold `t[i]` against this element's position before the body's
            // method calls are typed: the fold turns a tuple `Index` — which
            // has no lowering — into the element's field access.
            if let Some(index_local) = enumerate_index_local {
                EnumerateSubscriptRewriter {
                    index_local,
                    element: i as u32,
                    type_table,
                }
                .visit_block(&mut elem_body);
            }

            // Populate type_args on method-call nodes that have inferred generic params.
            // Inside variadic for-of, method calls like `seq.element(&v)` have empty type_args
            // because T was inferred from a TypePack at resolution time. Now that types are
            // concrete, fill in type_args so the monomorphizer can instantiate the generic method.
            self.infer_method_call_type_args(&mut elem_body, type_table, binding_local_idx);

            // Rewrite binding_local_idx → iter_binding in the body
            for s in &mut elem_body.stmts {
                Self::rewrite_local_index_in_stmt(s, binding_local_idx, iter_binding);
            }

            // Per iteration, after the index rewrite: every cloned iteration
            // substitutes against the same shared `locals` table.
            self.reconcile_unrolled_body_locals(&mut elem_body, local_count, locals);

            iter_stmts.extend(elem_body.stmts);

            let label = format!("$tuple_iter_{uid}_{i}");
            outer_stmts.push(TirStmt::new(
                TirStmtKind::LabeledBlock {
                    label,
                    block: TirBlock::new(iter_stmts, span),
                },
                span,
            ));
        }

        let outer_label = format!("$tuple_for_of_{uid}");
        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: outer_label,
                block: TirBlock::new(outer_stmts, span),
            },
            span,
        )]
    }

    /// Reconcile one unrolled iteration's locals against those already emitted,
    /// so heterogeneous elements never share a slot: the first iteration retypes
    /// the template's generic `let` to the concrete element type, later ones
    /// move to fresh locals. Retyping comes after reallocation, or a later
    /// element clobbers the type an earlier iteration still depends on.
    fn reconcile_unrolled_body_locals(
        &self,
        body: &mut TirBlock,
        local_count: &mut u32,
        locals: &mut Vec<TirLocal>,
    ) {
        // A single local can be collected more than once (an or-pattern binds
        // the same slot in each alternative); dedup so the first-seen /
        // collision bookkeeping below treats it once.
        let mut body_locals = locals_defined_in_block(body);
        body_locals.sort_unstable();
        body_locals.dedup();
        for body_local in body_locals {
            let concrete_type = local_type_in_block(body, body_local);
            if !self.claim_unrolled_local(body_local) {
                let new_idx = *local_count;
                *local_count += 1;
                let body_local_type = concrete_type.unwrap_or_else(|| {
                    locals
                        .get(body_local as usize)
                        .map(|l| l.type_id)
                        .unwrap_or(TypeTable::UNIT)
                });
                locals.push(TirLocal::synth(new_idx, body_local_type, false));
                for s in &mut body.stmts {
                    Self::rewrite_local_index_in_stmt(s, body_local, new_idx);
                }
            } else if let Some(ty) = concrete_type
                && let Some(slot) = locals.get_mut(body_local as usize)
            {
                slot.type_id = ty;
            }
        }
    }

    /// Whether a destructure template binds a field other than the
    /// `.enumerate()` index — i.e. whether the unroll reads the element at all.
    fn binds_element_field(destructure: &[TirStmt]) -> bool {
        destructure.iter().any(|s| {
            matches!(&s.kind, TirStmtKind::Let { value, .. }
                if matches!(&value.kind, TirExprKind::FieldAccess { field_index, .. } if *field_index != 0))
        })
    }

    /// Whether evaluating `expr` is observable. Only a place rooted at a local
    /// qualifies, so dropping an unread iterable changes nothing.
    fn is_pure_place(expr: &TirExpr) -> bool {
        match &expr.kind {
            TirExprKind::Local { .. } => true,
            TirExprKind::FieldAccess { expr: inner, .. } => Self::is_pure_place(inner),
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref,
                expr: inner,
            } => Self::is_pure_place(inner),
            _ => false,
        }
    }

    /// How many leading `let`s of an unroll body read a field of `binding` —
    /// the destructured sub-bindings a `for let [a, b] of …` pattern lowered to.
    fn destructure_prefix_len(body: &TirBlock, binding: u32) -> usize {
        body.stmts
            .iter()
            .take_while(|s| {
                if let TirStmtKind::Let { value, .. } = &s.kind
                    && let TirExprKind::FieldAccess { expr: inner, .. } = &value.kind
                    && let TirExprKind::Local { index, .. } = &inner.kind
                {
                    return *index == binding;
                }
                false
            })
            .count()
    }

    /// Unroll `[for let v of tuple { expr }]` into the tuple literal it denotes,
    /// once the pack is concrete (WEP 2026-03-14).
    ///
    /// The result is a labelled block holding the source tuple in a temp, so the
    /// iterable is evaluated once, and breaking with one element per source
    /// element — each itself a labelled block that binds the element and breaks
    /// with the body's value.
    fn expand_tuple_comprehension(
        &self,
        expr: &mut TirExpr,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
        local_count: &mut u32,
        locals: &mut Vec<TirLocal>,
    ) {
        let span = expr.span;
        let result_type = expr.type_id;
        let TirExprKind::VariadicTupleComprehension {
            iterable,
            binding_name,
            binding_local,
            destructure,
            body,
            unique_id,
            is_enumerate,
        } = &mut expr.kind
        else {
            unreachable!()
        };
        let is_enumerate = *is_enumerate;
        let uid = *unique_id;
        let binding_local_idx = *binding_local;
        let b_name = binding_name.clone();
        let template_destructure = destructure.clone();
        let template_body = body.clone();
        let inline_enumerate_pair = is_enumerate && !template_destructure.is_empty();
        let temp_read = !inline_enumerate_pair || Self::binds_element_field(&template_destructure);

        let (pack_index, pack_mapped) = type_table
            .as_tuple_through_ref(iterable.type_id)
            .and_then(|(elems, _)| {
                elems.iter().find_map(|&e| match type_table.get(e) {
                    ResolvedType::TypePack {
                        index, mapped_elem, ..
                    } => Some((Some(*index), mapped_elem.is_some())),
                    _ => None,
                })
            })
            .unwrap_or((None, false));

        let mut source = iterable.as_ref().clone();
        self.substitute_types_in_expr(&mut source, substitution, type_table, local_count, locals);
        let source_type = source.type_id;
        let (elements, _) = type_table
            .as_tuple_through_ref(source_type)
            .unwrap_or_else(|| {
                panic!(
                    "tuple comprehension: expected a concrete tuple after substitution, got {:?}",
                    type_table.get(source_type)
                )
            });

        // A mapped pack binds the mapped element, but the body's own pack
        // positions substitute from the source element `P_k`.
        let mapped_source_elems: Option<Vec<TypeId>> = if pack_mapped {
            pack_index
                .and_then(|idx| self.pack_source_tuple(idx, substitution))
                .and_then(|t| type_table.as_tuple(t))
        } else {
            None
        };
        let pack_tuple = pack_index.and_then(|idx| self.pack_source_tuple(idx, substitution));

        // Private to this unroll, one reader per field — so each element binding
        // moves its field out (`skip_value_copy`) rather than deep-copying it.
        let temp_name = format!("$comp_{uid}");
        let temp_local = *local_count;
        *local_count += 1;
        locals.push(TirLocal {
            name: temp_name.clone(),
            type_id: source_type,
            is_mut: false,
            span: Span::default(),
        });

        let mut result_elements = Vec::with_capacity(elements.len());
        for (i, &elem_type) in elements.iter().enumerate() {
            let element_label = format!("$comp_{uid}_{i}");
            let mut stmts = Vec::new();

            let field = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(TirExpr::new(
                        TirExprKind::Local {
                            index: temp_local,
                            name: temp_name.clone(),
                        },
                        source_type,
                        span,
                    )),
                    field_index: i as u32,
                    field_name: i.to_string(),
                },
                elem_type,
                span,
            );
            let index_literal = TirExpr::new(
                TirExprKind::IntLiteral {
                    value: i as u64,
                    repr: i.to_string(),
                },
                TypeTable::I32,
                span,
            );
            let bind_type = if is_enumerate {
                type_table.make_tuple(vec![TypeTable::I32, elem_type])
            } else {
                elem_type
            };

            let iter_binding = *local_count;
            *local_count += 1;
            locals.push(TirLocal {
                name: b_name.clone(),
                type_id: bind_type,
                is_mut: false,
                span: Span::default(),
            });
            if !inline_enumerate_pair {
                let bind_value = if is_enumerate {
                    TirExpr::new(
                        TirExprKind::TupleLiteral {
                            elements: vec![index_literal.clone(), field.clone()],
                        },
                        bind_type,
                        span,
                    )
                } else {
                    field.clone()
                };
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: b_name.clone(),
                        local_index: iter_binding,
                        is_mut: false,
                        is_reactive: false,
                        type_id: bind_type,
                        value: bind_value,
                        skip_value_copy: true,
                    },
                    span,
                ));
            }

            // The destructure reads off the concrete binding, so it is rebuilt
            // rather than substituted: splicing the pack through the pair type
            // would widen `[i32, ..T]` into the whole tuple.
            let pair_fields = type_table.elem_types_or_self(bind_type);
            // `index_local` is the sub-binding that reads field 0 — the
            // `.enumerate()` index — which a wildcard (`[_, v]`) leaves absent.
            let mut index_local: Option<u32> = None;
            let mut sub_locals: Vec<(u32, u32)> = Vec::new();
            let mut pinned: Vec<(u32, TypeId)> = Vec::new();
            for (j, template) in template_destructure.iter().enumerate() {
                let TirStmtKind::Let {
                    name,
                    local_index,
                    value,
                    ..
                } = &template.kind
                else {
                    continue;
                };
                // The field a sub-binding reads is its pattern position, which
                // the template recorded; a wildcard makes it differ from this
                // statement's position in the destructure list.
                let field_index = match &value.kind {
                    TirExprKind::FieldAccess { field_index, .. } => *field_index,
                    _ => j as u32,
                };
                let field_type = pair_fields
                    .get(field_index as usize)
                    .copied()
                    .unwrap_or(elem_type);
                let sub_local = *local_count;
                *local_count += 1;
                locals.push(TirLocal {
                    name: name.clone(),
                    type_id: field_type,
                    is_mut: false,
                    span: Span::default(),
                });
                sub_locals.push((*local_index, sub_local));
                pinned.push((*local_index, field_type));
                if field_index == 0 {
                    index_local = Some(sub_local);
                }
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: sub_local,
                        is_mut: false,
                        is_reactive: false,
                        type_id: field_type,
                        value: if inline_enumerate_pair {
                            if field_index == 0 {
                                index_literal.clone()
                            } else {
                                field.clone()
                            }
                        } else {
                            TirExpr::new(
                                TirExprKind::FieldAccess {
                                    expr: Box::new(TirExpr::new(
                                        TirExprKind::Local {
                                            index: iter_binding,
                                            name: b_name.clone(),
                                        },
                                        bind_type,
                                        span,
                                    )),
                                    field_index,
                                    field_name: field_index.to_string(),
                                },
                                field_type,
                                span,
                            )
                        },
                        skip_value_copy: true,
                    },
                    span,
                ));
            }

            let mut elem_body = TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Break {
                        label: Some(element_label.clone()),
                        value: Some(template_body.as_ref().clone()),
                    },
                    span,
                )],
                span,
            );
            if let Some(pack_idx) = pack_index {
                let mut elem_substitution = substitution.clone();
                elem_substitution.insert(
                    pack_idx,
                    mapped_source_elems
                        .as_ref()
                        .and_then(|es| es.get(i).copied())
                        .unwrap_or(elem_type),
                );
                // Decide this element's bindings before substituting, so a
                // nested unroll over the same pack cannot re-map a use of them.
                self.pin_binding_types(&mut elem_body, binding_local_idx, bind_type, type_table);
                for (local, ty) in &pinned {
                    self.pin_binding_types(&mut elem_body, *local, *ty, type_table);
                }
                let displaced = pack_tuple.map(|t| self.bind_pack_splice(pack_idx, t));
                self.substitute_types_in_block(
                    &mut elem_body,
                    &elem_substitution,
                    type_table,
                    local_count,
                    locals,
                );
                if let Some(previous) = displaced {
                    self.restore_pack_splice(pack_idx, previous);
                }
            }

            for s in &mut elem_body.stmts {
                Self::rewrite_local_index_in_stmt(s, binding_local_idx, iter_binding);
                for &(template_local, sub_local) in &sub_locals {
                    Self::rewrite_local_index_in_stmt(s, template_local, sub_local);
                }
            }
            if is_enumerate && let Some(index_local) = index_local {
                EnumerateSubscriptRewriter {
                    index_local,
                    element: i as u32,
                    type_table,
                }
                .visit_block(&mut elem_body);
            }
            self.infer_method_call_type_args(&mut elem_body, type_table, iter_binding);
            self.reconcile_unrolled_body_locals(&mut elem_body, local_count, locals);

            let element_result_type = Self::break_value_type(&elem_body).unwrap_or(elem_type);
            stmts.extend(elem_body.stmts);
            result_elements.push(TirExpr::new(
                TirExprKind::LabeledBlock {
                    label: element_label,
                    block: TirBlock::new(stmts, span),
                    result_type: element_result_type,
                },
                element_result_type,
                span,
            ));
        }

        let outer_label = format!("$comp_{uid}_result");
        let tuple = TirExpr::new(
            TirExprKind::TupleLiteral {
                elements: result_elements,
            },
            result_type,
            span,
        );
        let mut block_stmts = Vec::with_capacity(2);
        if temp_read || !Self::is_pure_place(&source) {
            block_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: temp_name,
                    local_index: temp_local,
                    is_mut: false,
                    is_reactive: false,
                    type_id: source_type,
                    value: source,
                    skip_value_copy: false,
                },
                span,
            ));
        }
        block_stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(outer_label.clone()),
                value: Some(tuple),
            },
            span,
        ));
        *expr = TirExpr::new(
            TirExprKind::LabeledBlock {
                label: outer_label,
                block: TirBlock::new(block_stmts, span),
                result_type,
            },
            result_type,
            span,
        );
    }

    /// The whole tuple a pack stands for. An enclosing unroll pins
    /// `substitution[index]` to the single element it walks, so a nested unroll
    /// over the same pack reads the splice binding — which holds the tuple for
    /// exactly that span — the same way a `[..T]` splice position does.
    fn pack_source_tuple(
        &self,
        index: u32,
        substitution: &IndexMap<u32, TypeId>,
    ) -> Option<TypeId> {
        self.pack_splice_bindings
            .borrow()
            .get(&index)
            .copied()
            .or_else(|| substitution.get(&index).copied())
    }

    /// The type of the value a single-`break` block yields.
    fn break_value_type(block: &TirBlock) -> Option<TypeId> {
        block.stmts.iter().find_map(|s| match &s.kind {
            TirStmtKind::Break {
                value: Some(value), ..
            } => Some(value.type_id),
            _ => None,
        })
    }

    /// After variadic for-of expansion, method calls that had inferred type params
    /// (empty `type_args`) need their `type_args` populated from the concrete argument types.
    /// e.g., `seq.element(&v)` where element<T: Serialize> — infer T from the arg type.
    /// Populate inferred method-level `type_args` on every method call in a
    /// (post variadic-for-of-expansion) block. Delegates to
    /// [`MethodTypeArgInferer`] so the traversal is exhaustive.
    fn infer_method_call_type_args(
        &self,
        block: &mut TirBlock,
        type_table: &TypeTable,
        binding_local: u32,
    ) {
        MethodTypeArgInferer {
            type_table,
            binding_local,
            templates: &self.functions.templates,
        }
        .visit_block(block);
    }

    /// Pin the uses of one unrolled binding to its concrete element type.
    ///
    /// A nested unroll over the same pack substitutes this body again with
    /// *its* element, and the substitution is type-directed — it cannot tell an
    /// enclosing binding's use from its own. Deciding the enclosing one first
    /// takes it out of reach.
    fn pin_binding_types(
        &self,
        block: &mut TirBlock,
        binding_local: u32,
        elem_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        BindingTypePinner {
            binding_local,
            elem_type,
            type_table,
        }
        .visit_block(block);
    }

    fn rewrite_local_index_in_stmt(stmt: &mut TirStmt, old_idx: u32, new_idx: u32) {
        LocalIndexRewriter { old_idx, new_idx }.visit_stmt(stmt);
    }
}

/// Convert `==`/`!=`/`<`/`>`/`<=`/`>=` on Struct/Variant/GenericInstance types to
/// `Eq::eq` / `Ord::cmp` method calls. Returns `None` for primitives.
fn try_lower_comparison(
    trait_env: &Arc<TraitEnv>,
    span: Span,
    op: TirBinaryOp,
    left: &TirExpr,
    right: &TirExpr,
    type_table: &mut TypeTable,
) -> Option<TirExprKind> {
    let operand_type = type_table.get(left.type_id);
    let (impl_type_args, type_module_source): (Vec<FqTypeName>, Option<ModuleSource>) =
        match operand_type {
            ResolvedType::Struct { .. } | ResolvedType::Variant { .. } => (
                vec![],
                type_table.nominal_head(left.type_id).map(|(_, m)| m),
            ),
            ResolvedType::GenericInstance { def, type_args } => {
                if type_table.is_tuple_def(*def) {
                    // Tuple Eq/Ord are provided by variadic impls in core:prelude/tuple.wado
                    // and already lowered to method calls by the elaborator.
                    return None;
                }
                // Must match the struct-name form
                // `method_instantiation_name_inner` writes into `MethodInfo`.
                let args: Vec<FqTypeName> = type_args
                    .iter()
                    .map(|&t| type_table.fq_type_name(t))
                    .collect();
                (args, Some(type_table.def_module(*def).clone()))
            }
            _ => return None,
        };
    let base_struct_name = type_table.fq_base_type_name(left.type_id);
    let eq_trait = type_table.compiler_trait_fq(CompilerItem::Eq);
    let ord_trait = type_table.compiler_trait_fq(CompilerItem::Ord);

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

    let resolve_module = |info: &LocalMethodName, type_mod: Option<ModuleSource>| -> ModuleSource {
        // A generic impl's instance lives in the receiver's module, which
        // `try_lower_comparison` has already required the operand to have.
        trait_env
            .concrete_impl_module_of(info, type_mod.as_ref())
            .cloned()
            .or(type_mod)
            .unwrap_or_else(|| {
                panic!(
                    "comparison-lowering `resolve_module`: operand type \
                     for `{}` has no defining module — `try_lower_comparison` \
                     should have returned None before reaching this point",
                    info.to_mangled_name()
                )
            })
    };

    if matches!(op, TirBinaryOp::Eq | TirBinaryOp::NotEq) {
        let receiver = make_ref(left, type_table);
        let arg_ref = make_ref(right, type_table);
        let method_info = LocalMethodName::new(base_struct_name, Some(eq_trait), "eq".to_string())
            .with_struct_type_args(&impl_type_args);
        let mangled_name = method_info.to_mangled_name();
        let method_module = resolve_module(&method_info, type_module_source);
        let template = trait_call_template(
            trait_env,
            &method_info,
            left.type_id,
            &method_module,
            type_table,
        );

        let method_call = TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: method_module,
                name: mangled_name,
                template,
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
        let ordering_def = type_table
            .compiler_item_def(CompilerItem::Ordering)
            .expect("`Ordering` is a registered compiler item");
        let ordering_type_id = type_table.intern(ResolvedType::Enum { def: ordering_def });
        let method_info =
            LocalMethodName::new(base_struct_name, Some(ord_trait), "cmp".to_string())
                .with_struct_type_args(&impl_type_args);
        let mangled_name = method_info.to_mangled_name();
        let method_module = resolve_module(&method_info, type_module_source);
        let template = trait_call_template(
            trait_env,
            &method_info,
            left.type_id,
            &method_module,
            type_table,
        );

        let cmp_call = TirExpr::new(
            TirExprKind::method_call(
                Box::new(receiver),
                FunctionRef {
                    module_source: method_module,
                    name: mangled_name,
                    template,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                vec![],
                vec![CallArg::new(arg_ref, false)],
            ),
            ordering_type_id,
            span,
        );

        // Look up Ordering's `Less` / `Greater` cases through the
        // `CompilerItem` registry so a stdlib rename of either case
        // flows through this primitive-ord-dispatch path without touching
        // the literal-case mapping table.
        let items = type_table.compiler_items();
        let (_, _, less_n, less_i) = items.require_enum_case(CompilerItem::OrderingLess);
        let (_, _, greater_n, greater_i) = items.require_enum_case(CompilerItem::OrderingGreater);
        let less_n = less_n.to_string();
        let greater_n = greater_n.to_string();
        let (compare_op, case_name, case_index): (TirBinaryOp, String, u32) = match op {
            TirBinaryOp::Lt => (TirBinaryOp::Eq, less_n, less_i),
            TirBinaryOp::Gt => (TirBinaryOp::Eq, greater_n, greater_i),
            TirBinaryOp::LtEq => (TirBinaryOp::NotEq, greater_n, greater_i),
            TirBinaryOp::GtEq => (TirBinaryOp::NotEq, less_n, less_i),
            _ => unreachable!(),
        };

        let ordering_variant = TirExpr::new(
            TirExprKind::EnumConstruct {
                enum_type: ordering_type_id,
                case_name,
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

/// Strip one `&` from an operand of a trait method being rewritten to a
/// primitive binary op, which takes values rather than references.
///
/// `&expr` collapses to `expr`. A reference *value* — a `&T` parameter, a field
/// holding one — has no `&` node to drop, so it gets an explicit deref; leaving
/// it alone handed codegen a `(ref $type)` where the instruction wanted an i32.
fn unref_operand(operand: &TirExpr, type_table: &TypeTable) -> TirExpr {
    let mut expr = if let TirExprKind::Unary {
        op: TirUnaryOp::Ref,
        expr: inner,
    } = &operand.kind
    {
        (**inner).clone()
    } else {
        operand.clone()
    };
    // `T = &i32` leaves the inner value a reference of its own, so peel a
    // layer at a time and type each `Deref` by what it yields.
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = type_table.get(expr.type_id)
    {
        let inner = *inner;
        let span = expr.span;
        expr = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: Box::new(expr),
            },
            inner,
            span,
        );
    }
    expr
}

/// The instruction a monomorphized operator call lowers to, named by the
/// compiler item its trait is.
fn trait_method_to_unary_op(item: Option<CompilerItem>, method_name: &str) -> Option<TirUnaryOp> {
    match (item?, method_name) {
        (CompilerItem::Neg, "neg") => Some(TirUnaryOp::Neg),
        (CompilerItem::BitNot, "bitnot") => Some(TirUnaryOp::BitNot),
        _ => None,
    }
}

/// [`trait_method_to_unary_op`] for the binary operators.
fn trait_method_to_binary_op(item: Option<CompilerItem>, method_name: &str) -> Option<TirBinaryOp> {
    match (item?, method_name) {
        (CompilerItem::Add, "add") => Some(TirBinaryOp::Add),
        (CompilerItem::Sub, "sub") => Some(TirBinaryOp::Sub),
        (CompilerItem::Mul, "mul") => Some(TirBinaryOp::Mul),
        (CompilerItem::Div, "div") => Some(TirBinaryOp::Div),
        (CompilerItem::Rem, "rem") => Some(TirBinaryOp::Mod),
        (CompilerItem::BitAnd, "bitand") => Some(TirBinaryOp::BitAnd),
        (CompilerItem::BitOr, "bitor") => Some(TirBinaryOp::BitOr),
        (CompilerItem::BitXor, "bitxor") => Some(TirBinaryOp::BitXor),
        (CompilerItem::Shl, "shl") => Some(TirBinaryOp::Shl),
        (CompilerItem::Shr, "shr") => Some(TirBinaryOp::Shr),
        (CompilerItem::Eq, "eq") => Some(TirBinaryOp::Eq),
        _ => None,
    }
}
