//! Effect, stores, and default-purity checking for Wado (Design B).
//!
//! These checks validate, respectively, that every call holds the effects its
//! callee requires, that a reference parameter that escapes declares
//! `stores[param]`, and that parameter / field defaults are pure.
//!
//! All three operate on [`Semantics`] (the AST plus the facts recorded during
//! `annotate`), not on the emitted TIR. They therefore see every source
//! function regardless of what reify emits — immune to dead-code gating — and
//! run on the LSP path, which builds no TIR. Each returns its violations so the
//! caller can route them (LSP diagnostics or the batch logger).

use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::tir::{EffectRef, FunctionRef, ResolvedType, TypeId, TypeSet, TypeTable};
use crate::token::Span;

use crate::ast::{self, AstVisitor, Expr, Function, Item, Stmt};
use crate::semantics::Semantics;
use crate::symbol::SymbolKey;

/// Whether a missing `with` entry refers to a resource or a regular effect.
/// Used to select the diagnostic wording (`missing resource` vs `missing effect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    /// A `resource` declaration — the caller needs the resource capability.
    Resource,
    /// An `effect` declaration (or unknown — default wording).
    Effect,
}

impl EffectKind {
    fn noun(self) -> &'static str {
        match self {
            EffectKind::Resource => "resource",
            EffectKind::Effect => "effect",
        }
    }
}

/// Error from effect checking
#[derive(Debug, Clone)]
pub struct EffectError {
    /// The function being called
    pub callee: String,
    /// The missing effect
    pub missing_effect: String,
    /// Whether the missing item is a resource or a regular effect
    pub kind: EffectKind,
    /// Source location of the call
    pub span: Span,
}

impl std::fmt::Display for EffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: missing {} '{}' required by '{}'",
            self.span.line,
            self.span.column,
            self.kind.noun(),
            self.missing_effect,
            self.callee
        )
    }
}

impl std::error::Error for EffectError {}

impl From<EffectError> for crate::compiler_host::Diagnostic {
    fn from(e: EffectError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: format!(
                "missing {} '{}' required by '{}'",
                e.kind.noun(),
                e.missing_effect,
                e.callee
            ),
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
        }
    }
}

/// Error from stores checking
#[derive(Debug, Clone)]
pub struct StoresError {
    /// Description of the violation
    pub message: String,
    /// Source location
    pub span: Span,
}

impl std::fmt::Display for StoresError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

impl std::error::Error for StoresError {}

impl From<StoresError> for crate::compiler_host::Diagnostic {
    fn from(e: StoresError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: e.message.clone(),
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
        }
    }
}

<<<<<<< HEAD
#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckMode {
    EffectsOnly,
    StoresOnly,
}

/// Effect checker that walks TIR and validates effect requirements
struct EffectChecker<'a, H: CompilerHost> {
    modules: &'a IndexMap<ModuleSource, TirModule>,
    logger: &'a Logger<'a, H>,
    /// Current function's effects (set when entering a function)
    current_effects: IndexSet<EffectRef>,
    /// Current function's stores-declared parameter names
    current_stores: IndexSet<String>,
    /// Current function's reference parameter names (for detecting violations).
    /// Only contains parameters whose type is `&T` or `&mut T`.
    current_ref_params: IndexSet<String>,
    /// Type table (shared across modules)
    type_table: Option<Rc<RefCell<TypeTable>>>,
    /// What this checker is checking
    mode: CheckMode,
    /// Pre-built index: (`module_source`, `func_name`) -> `EffectSignature`
    func_index: IndexMap<(ModuleSource, String), EffectSignature>,
    /// Set of `(module_source, resource_name)` pairs for every declared resource.
    /// Used to inject the resource as an effect for any call to one of its methods.
    resource_names: IndexSet<(ModuleSource, String)>,
    /// Effect / resource propagation closure.
    ///
    /// For each effect or resource `E`, `closure[E]` is the transitive set
    /// of resource effects implied by holding `E`. When a function enters a
    /// `with` scope, every concrete effect in its signature is expanded through
    /// this map before checking callees. Keyed by the full `EffectRef::Concrete`
    /// (i.e. `(module_source, name)`), matching how every other resolved
    /// symbol identifies itself.
    closure: IndexMap<EffectRef, IndexSet<EffectRef>>,
    /// Pre-indexed struct field types, keyed by `(module_source, struct_name)`.
    /// Shared by `build_propagation_closure` and signature-resource inference so
    /// both paths can walk nested resource references inside struct values.
    struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Pre-indexed variant case payload types, keyed by `(module_source,
    /// variant_name)`. Same role as `struct_fields` for variant payloads.
    variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>>,
}

impl<'a, H: CompilerHost> EffectChecker<'a, H> {
    fn new(modules: &'a IndexMap<ModuleSource, TirModule>, logger: &'a Logger<'a, H>) -> Self {
        let type_table = modules.values().next().map(|m| Rc::clone(&m.type_table));
        let mut func_index = IndexMap::default();
        let mut resource_names: IndexSet<(ModuleSource, String)> = IndexSet::default();
        for (module_source, module) in modules {
            for resource in &module.resources {
                resource_names.insert((module_source.clone(), resource.name.clone()));
            }
        }
        for (module_source, module) in modules {
            for func_rc in &module.functions {
                let func = func_rc.borrow();
                func_index.insert(
                    (module_source.clone(), func.name.clone()),
                    EffectSignature {
                        effects: func.effects.clone(),
                        params: func
                            .params
                            .iter()
                            .map(|p| EffectParam {
                                name: p.name.clone(),
                                type_id: p.type_id,
                            })
                            .collect(),
                        is_method: func.is_method(),
                        is_ambient: func.is_ambient,
                        benign: func.benign_effects.clone(),
                    },
                );
            }
            for impl_block in &module.impls {
                for method in &impl_block.methods {
                    func_index.insert(
                        (module_source.clone(), method.name.clone()),
                        EffectSignature {
                            effects: method.effects.clone(),
                            params: method
                                .params
                                .iter()
                                .map(|p| EffectParam {
                                    name: p.name.clone(),
                                    type_id: p.type_id,
                                })
                                .collect(),
                            is_method: method.is_method(),
                            is_ambient: method.is_ambient,
                            benign: method.benign_effects.clone(),
                        },
                    );
                }
            }
        }
        let mut struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        let mut variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>> =
            IndexMap::default();
        for (module_source, module) in modules {
            for s in &module.structs {
                let tids: Vec<TypeId> = s.fields.iter().map(|f| f.type_id).collect();
                struct_fields.insert((module_source.clone(), s.name.clone()), tids);
            }
            for v in &module.variants {
                let tids: Vec<TypeId> = v.cases.iter().map(|c| c.payload).collect();
                variant_payloads.insert((module_source.clone(), v.name.clone()), tids);
            }
        }
        let closure = build_propagation_closure(
            modules,
            type_table.as_ref(),
            &struct_fields,
            &variant_payloads,
        );
        Self {
            modules,
            logger,
            current_effects: IndexSet::default(),
            current_stores: IndexSet::default(),
            current_ref_params: IndexSet::default(),
            type_table,
            mode: CheckMode::EffectsOnly,
            func_index,
            resource_names,
            closure,
            struct_fields,
            variant_payloads,
        }
    }

    /// Classify an effect reference as a `resource` or `effect` for diagnostic
    /// wording. Looks up `(module_source, name)` in the resource index; falls
    /// back to `Effect` for anything not registered as a resource.
    fn classify(&self, effect: &EffectRef) -> EffectKind {
        match effect {
            EffectRef::Concrete {
                name,
                module_source,
            } => {
                if self
                    .resource_names
                    .contains(&(module_source.clone(), name.clone()))
                {
                    EffectKind::Resource
                } else {
                    EffectKind::Effect
                }
            }
            EffectRef::Param { .. } => EffectKind::Effect,
        }
    }

    /// Expand a set of effects through the propagation closure.
    ///
    /// For each concrete effect in the input, unions in the closure entries.
    /// `Param` effects are passed through unchanged — they are resolved at
    /// call sites via function-typed arguments.
    fn expand_effects(&self, effects: &IndexSet<EffectRef>) -> IndexSet<EffectRef> {
        let mut out: IndexSet<EffectRef> = IndexSet::default();
        for eff in effects {
            out.insert(eff.clone());
            if matches!(eff, EffectRef::Concrete { .. })
                && let Some(extra) = self.closure.get(eff)
            {
                for e in extra {
                    out.insert(e.clone());
                }
            }
        }
        out
    }

    /// Resources that appear anywhere in the function's signature — parameter
    /// types or the return type, recursively through containers, struct fields
    /// and variant payloads. These are unioned with the declared `with` set so
    /// `fn f(s: Stream<u8>)` does not need to repeat `with Stream`.
    fn signature_resources(&self, func: &TirFunction) -> IndexSet<EffectRef> {
        let mut out: IndexSet<EffectRef> = IndexSet::default();
        let Some(tt_rc) = &self.type_table else {
            return out;
        };
        let tt = tt_rc.borrow();
        let mut visited = TypeSet::default();
        for p in &func.params {
            collect_resource_refs(
                p.type_id,
                &tt,
                &self.struct_fields,
                &self.variant_payloads,
                &mut out,
                &mut visited,
            );
        }
        collect_resource_refs(
            func.return_type,
            &tt,
            &self.struct_fields,
            &self.variant_payloads,
            &mut out,
            &mut visited,
        );
        // Async functions erase `return_type` to unit (the result travels via
        // `task return`), so walk the declared task return type too.
        if let Some(task_ret) = func.task_return_type {
            collect_resource_refs(
                task_ret,
                &tt,
                &self.struct_fields,
                &self.variant_payloads,
                &mut out,
                &mut visited,
            );
        }
        out
    }

    /// Check all modules
    fn check_all(&mut self) -> Result<(), Bail> {
        for module in self.modules.values() {
            self.check_module(module)?;
        }
        Ok(())
    }

    /// Check a single module
    fn check_module(&mut self, module: &TirModule) -> Result<(), Bail> {
        // Scope diagnostics to this module so EffectError spans carry the
        // correct filename instead of whatever an earlier phase last set.
        self.logger
            .set_file(module.module_source.diagnostic_filename());

        // Check all functions
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            self.check_function(&func)?;
        }

        // Check impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                self.check_function(method)?;
            }
        }
        Ok(())
    }

    /// Whether stores checking applies to this function.
    fn should_check_stores(&self, func: &TirFunction) -> bool {
        if self.mode != CheckMode::StoresOnly {
            return false;
        }
        // Skip CM binding functions — they are wrappers generated for the component model boundary
        // and handle data transfer via CM semantics (copy/own/borrow), not Wado references.
        if func.is_cm_binding {
            return false;
        }
        // Skip functions with no parameters at all
        if func.params.is_empty() {
            return false;
        }
        true
    }

    /// Check a single function
    fn check_function(&mut self, func: &TirFunction) -> Result<(), Bail> {
        // Skip test functions - they implicitly have all effects
        if func.name.starts_with("__test_") {
            return Ok(());
        }

        // Skip CM binding functions - they are boundary code with special effect semantics
        if func.is_cm_binding {
            return Ok(());
        }

        // Skip dispatch wrapper functions - they are synthesised dispatch
        // infrastructure that internally calls cm_binding adapters /
        // canonical intrinsics, which carry their own effects. The
        // wrapper's caller doesn't need to declare those effects because
        // the wrapper itself satisfies them; mirroring the `is_cm_binding`
        // skip above keeps the call-site analysis honest.
        if func.is_dispatch_wrapper {
            return Ok(());
        }

        // `#[ambient]` functions intentionally bypass the effect system so
        // logging / panic / unreachable work anywhere. Their bodies perform
        // resource operations (Stream::new, Future::drop) that would otherwise
        // require `with Stream`, `with Future`, etc.
        if func.is_ambient {
            return Ok(());
        }

        // Set current context. Start with the user-declared `with` set, add
        // resources that appear anywhere in the signature (param / return types,
        // recursively through containers, struct fields, variant payloads) so
        // the user does not need to repeat `with R` when `R` is already visible
        // in the signature, then expand through the propagation closure so
        // resources referenced by an effect's operations (e.g.
        // `Stdout::write_via_stream` references `Stream`, `Future`) are
        // implicitly admitted.
        let mut effects: IndexSet<EffectRef> = func.effects.iter().cloned().collect();
        effects.extend(self.signature_resources(func));
        // `#[benign(E)]` admits `E` in the body without a `with E` clause.
        // `get_function_effects` strips it from the outgoing set, so callers
        // never see it.
        effects.extend(func.benign_effects.iter().cloned());
        self.current_effects = self.expand_effects(&effects);
        self.current_stores = func.stores.iter().cloned().collect();

        // Track which parameters are reference types (only for stores checking)
        self.current_ref_params = IndexSet::default();
        if self.should_check_stores(func)
            && let Some(tt_rc) = &self.type_table
        {
            let tt = tt_rc.borrow();
            for param in &func.params {
                let resolved = tt.get(param.type_id);
                if matches!(
                    resolved,
                    crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
                ) {
                    self.current_ref_params.insert(param.name.clone());
                }
            }
        }

        // Check the body if present
        if let Some(body) = &func.body {
            self.check_block(body)?;
        }
        Ok(())
    }

    /// Check a block
    fn check_block(&mut self, block: &TirBlock) -> Result<(), Bail> {
        for stmt in &block.stmts {
            self.check_stmt(stmt)?;
        }
        Ok(())
    }

    /// Check a statement
    fn check_stmt(&mut self, stmt: &TirStmt) -> Result<(), Bail> {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.check_expr(value)?;
            }
            TirStmtKind::Expr(expr) => self.check_expr(expr)?,
            TirStmtKind::Return { value } => {
                if let Some(e) = value {
                    self.check_stores_violation_return(e)?;
                    self.check_expr(e)?;
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.check_expr(condition)?;
                self.check_block(then_block)?;
                if let Some(else_blk) = else_block {
                    self.check_block(else_blk)?;
                }
            }
            TirStmtKind::Loop { body } => {
                self.check_block(body)?;
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(expr) = value {
                    self.check_expr(expr)?;
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.check_block(block)?;
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.check_expr(value)?;
            }
            TirStmtKind::TaskReturn { value } => {
                self.check_expr(value)?;
            }
            TirStmtKind::VariadicForOf { .. } => {}
        }
        Ok(())
    }

    /// Check an expression for effect violations
    fn check_expr(&mut self, expr: &TirExpr) -> Result<(), Bail> {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                self.check_call_with_args(func, args, expr.span)?;
                for arg in args {
                    self.check_expr(&arg.expr)?;
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                self.check_expr(receiver)?;
                self.check_call_with_args(func, args, expr.span)?;
                for arg in args {
                    self.check_expr(&arg.expr)?;
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                // CmRawCall is used inside synthesized adapter functions;
                // no effect checking needed (adapter functions are always effectful)
                for arg in args {
                    self.check_expr(arg)?;
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.check_expr(callee)?;
                for arg in args {
                    self.check_expr(arg)?;
                }
                // Check effects from the callee's function type.
                //
                // The function type's `effects` list is the exact set the type
                // checker stored at the closure expression — it is NOT
                // closure-expanded. `current_effects` *is* expanded at function
                // entry, so a caller that declares `with Stdout` can invoke an
                // indirect closure that internally needs `Stream` (via Stdout
                // propagation). Cases where the function-type effect list
                // itself should be augmented with propagation (e.g. handing
                // such a closure further down) are not yet handled; any such
                // call that actually leaks a resource through propagation
                // would need to re-resolve the callee's concrete effects
                // first.
                if self.mode == CheckMode::EffectsOnly
                    && let Some(tt) = &self.type_table
                {
                    let tt = tt.borrow();
                    if let ResolvedType::Function { effects, .. } = tt.get(callee.type_id) {
                        for effect in effects {
                            if !self.current_effects.contains(effect) {
                                let kind = self.classify(effect);
                                self.logger.error(EffectError {
                                    callee: "(indirect call)".to_string(),
                                    missing_effect: effect.name().to_string(),
                                    kind,
                                    span: expr.span,
                                })?;
                            }
                        }
                    }
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.check_expr(left)?;
                self.check_expr(right)?;
            }
            TirExprKind::Unary { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Assign { target, value } => {
                self.check_expr(target)?;
                self.check_expr(value)?;
            }
            TirExprKind::Cast { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::FieldAccess { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Index { expr, index } => {
                self.check_expr(expr)?;
                self.check_expr(index)?;
            }
            TirExprKind::Block(block) => {
                self.check_block(block)?;
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.check_expr(condition)?;
                self.check_block(then_branch)?;
                if let Some(else_blk) = else_branch {
                    self.check_block(else_blk)?;
                }
            }
            TirExprKind::Match { expr, arms } => {
                self.check_expr(expr)?;
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.check_expr(guard)?;
                    }
                    self.check_expr(&arm.body)?;
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.check_stores_violation_struct_field(&field.value, &field.name)?;
                    self.check_expr(&field.value)?;
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.check_expr(elem)?;
                }
            }
            TirExprKind::TupleSpread { expr }
            | TirExprKind::TupleZip { expr }
            | TirExprKind::TupleLen { expr }
            | TirExprKind::TypePackExpansion {
                call_expr: expr, ..
            } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Closure {
                body,
                declared_effects,
                ..
            } => {
                // When the closure has an explicit effect annotation (e.g. via
                // `let f: fn() = ...`), the body must satisfy only that effect
                // set — leaking outer effects through a more-pure-than-declared
                // closure type would break callers that trusted the annotation.
                //
                // Unannotated closures keep inheriting the enclosing
                // function's effects, matching the pre-WEP behaviour.
                if let Some(declared) = declared_effects {
                    let mut declared_set: IndexSet<EffectRef> = declared.iter().cloned().collect();
                    declared_set = self.expand_effects(&declared_set);
                    let saved = std::mem::replace(&mut self.current_effects, declared_set);
                    let result = self.check_expr(body);
                    self.current_effects = saved;
                    result?;
                } else {
                    self.check_expr(body)?;
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.check_expr(payload_expr)?;
                }
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.check_expr(inner)?;
                    }
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.check_block(block)?;
            }
            TirExprKind::GlobalVarSet { name, value, .. } => {
                self.check_stores_violation_global(value, name)?;
                self.check_expr(value)?;
            }
            TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::VariantPayload { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::WithHandler { bindings, body, .. } => {
                // Handler expressions are evaluated in the outer scope (no
                // effect substitution applies to them).
                for binding in bindings {
                    self.check_expr(&binding.handler)?;
                }
                // Inside the body, any effect handled by a binding is
                // satisfied locally — temporarily extend `current_effects`
                // with the handled effects so the body's calls to those
                // effects don't propagate to the caller.
                let added: Vec<EffectRef> = bindings
                    .iter()
                    .filter_map(|b| b.effect.clone())
                    .filter(|e| !self.current_effects.contains(e))
                    .collect();
                for eff in &added {
                    self.current_effects.insert(eff.clone());
                }
                let result = self.check_block(body);
                for eff in &added {
                    self.current_effects.shift_remove(eff);
                }
                result?;
            }
            TirExprKind::Resume { value } => {
                // `resume` is control-flow; the value flows into the
                // suspended computation. Check the value expression itself.
                self.check_expr(value)?;
            }
            // Leaf expressions - no sub-expressions to check
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
        }
        Ok(())
    }

    /// Check a function call for effect violations, resolving effect parameters
    /// from function-typed arguments when needed.
    fn check_call_with_args(
        &mut self,
        func_ref: &FunctionRef,
        args: &[CallArg],
        span: Span,
    ) -> Result<(), Bail> {
        if self.mode != CheckMode::EffectsOnly {
            return Ok(());
        }
        let callee_effects = self.get_function_effects(func_ref);

        // Separate effect params from concrete effects
        let has_params = callee_effects.iter().any(super::tir::EffectRef::is_param);

        // Resolve effect params to concrete effects from function-typed arguments
        let resolved_effects = if has_params {
            self.resolve_effect_params(&callee_effects, func_ref, args)
        } else {
            callee_effects
        };

        for effect in &resolved_effects {
            if !self.current_effects.contains(effect) {
                let kind = self.classify(effect);
                self.logger.error(EffectError {
                    callee: func_ref.name.clone(),
                    missing_effect: effect.name().to_string(),
                    kind,
                    span,
                })?;
            }
        }
        Ok(())
    }

    /// Check if an expression traces back to a reference parameter not declared in stores.
    /// Returns the parameter name if it's a stores violation, None otherwise.
    fn find_unstored_ref_param(&self, expr: &TirExpr) -> Option<String> {
        match &expr.kind {
            TirExprKind::Local { name, .. } => {
                // Check if this local is a reference parameter not in stores
                if self.current_ref_params.contains(name) && !self.current_stores.contains(name) {
                    Some(name.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Check stores violations: a function that stores a reference parameter must declare stores[param]
    fn check_stores_violation_return(&mut self, value: &TirExpr) -> Result<(), Bail> {
        if let Some(param_name) = self.find_unstored_ref_param(value) {
            self.logger.error(StoresError {
                message: format!(
                    "returning reference parameter '{param_name}' requires `stores[{param_name}]` declaration"
                ),
                span: value.span,
            })?;
        }
        Ok(())
    }

    /// Check stores violation: storing a reference parameter in a struct field
    fn check_stores_violation_struct_field(
        &mut self,
        value: &TirExpr,
        _field_name: &str,
    ) -> Result<(), Bail> {
        if let Some(param_name) = self.find_unstored_ref_param(value) {
            self.logger.error(StoresError {
                message: format!(
                    "storing reference parameter '{param_name}' in struct field requires `stores[{param_name}]` declaration"
                ),
                span: value.span,
            })?;
        }
        Ok(())
    }

    /// Check stores violation: assigning a reference parameter to a global
    fn check_stores_violation_global(
        &mut self,
        value: &TirExpr,
        global_name: &str,
    ) -> Result<(), Bail> {
        if let Some(param_name) = self.find_unstored_ref_param(value) {
            self.logger.error(StoresError {
                message: format!(
                    "storing reference parameter '{param_name}' in global '{global_name}' requires `stores[{param_name}]` declaration"
                ),
                span: value.span,
            })?;
        }
        Ok(())
    }

    /// Resolve effect parameters to concrete effects by examining function-typed arguments.
    ///
    /// For `fn wrapper<effect E>(f: fn() with E) with E`, when called with a closure
    /// that has effects `[Stdout]`, E resolves to `[Stdout]`.
    fn resolve_effect_params(
        &self,
        callee_effects: &[EffectRef],
        func_ref: &FunctionRef,
        args: &[CallArg],
    ) -> Vec<EffectRef> {
        // Collect the names of effect params
        let effect_param_names: IndexSet<String> = callee_effects
            .iter()
            .filter_map(|e| match e {
                EffectRef::Param { name } => Some(name.clone()),
                EffectRef::Concrete { .. } => None,
            })
            .collect();

        // Map each effect param name to its resolved concrete effects
        let mut effect_param_concrete: IndexMap<String, IndexSet<EffectRef>> = IndexMap::default();
        for name in &effect_param_names {
            effect_param_concrete.insert(name.clone(), IndexSet::default());
        }

        // Look at the callee's parameter types and actual argument types
        if let Some(sig) = self.find_effect_signature(func_ref)
            && let Some(tt) = &self.type_table
        {
            let tt = tt.borrow();
            // For method calls, args doesn't include the receiver (self), but params does.
            // Skip the self parameter when the function is a method.
            let params_iter: Box<dyn Iterator<Item = _>> =
                if sig.is_method && !sig.params.is_empty() && sig.params[0].name == "self" {
                    Box::new(sig.params.iter().skip(1))
                } else {
                    Box::new(sig.params.iter())
                };
            for (param, arg) in params_iter.zip(args.iter()) {
                if let ResolvedType::Function {
                    effects: formal_effects,
                    ..
                } = tt.get(param.type_id)
                {
                    let has_effect_params = formal_effects
                        .iter()
                        .any(|e| e.is_param() && effect_param_names.contains(e.name()));
                    if !has_effect_params {
                        continue;
                    }

                    if let ResolvedType::Function {
                        effects: actual_effects,
                        ..
                    } = tt.get(arg.expr.type_id)
                    {
                        for formal_effect in formal_effects {
                            if let EffectRef::Param { name } = formal_effect
                                && let Some(concrete_set) = effect_param_concrete.get_mut(name)
                            {
                                for actual in actual_effects {
                                    concrete_set.insert(actual.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        // Build resolved effect list: replace param names with concrete effects.
        // Expand concrete effects through the propagation closure so that a
        // resolved effect param pointing at e.g. `Stdout` also picks up
        // `Stream`, `Future`, etc. (propagation is applied symmetrically to
        // caller context; see `check_function`).
        let mut resolved: IndexSet<EffectRef> = IndexSet::default();
        for effect in callee_effects {
            match effect {
                EffectRef::Param { name } => {
                    if let Some(concrete_set) = effect_param_concrete.get(name) {
                        let expanded = self.expand_effects(concrete_set);
                        for concrete in expanded {
                            resolved.insert(concrete);
                        }
                    }
                }
                EffectRef::Concrete { .. } => {
                    resolved.insert(effect.clone());
                }
            }
        }
        resolved.into_iter().collect()
    }

    /// Get the effects for a function.
    ///
    /// If the call targets a method on a resource type, the resource itself is
    /// implicitly required as an effect (resources are effects). Resource
    /// operations are host calls so the caller must declare the resource in
    /// its `with` clause to use them.
    fn get_function_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        let key = (func_ref.module_source.clone(), func_ref.name.clone());
        // `#[ambient]` callees expose no effects to callers — they bypass the
        // effect system by design.
        if self.func_index.get(&key).is_some_and(|sig| sig.is_ambient) {
            return Vec::new();
        }
        let mut effects = self
            .func_index
            .get(&key)
            .map(|sig| sig.effects.clone())
            .unwrap_or_default();
        if let Some(method_info) = &func_ref.method_info {
            // Trait methods (`trait_name` is Some) are dispatch through user or
            // auto-derived trait impls — they are not host operations on the
            // resource itself, so they don't trigger the resource effect.
            // Only direct resource operations (constructors, instance, statics
            // declared in the `resource` block) require the effect.
            if method_info.trait_name.is_none() {
                let resource_key = (
                    func_ref.module_source.clone(),
                    method_info.base_struct_name.clone(),
                );
                if self.resource_names.contains(&resource_key) {
                    let resource_effect = EffectRef::Concrete {
                        name: method_info.base_struct_name.clone(),
                        module_source: func_ref.module_source.clone(),
                    };
                    if !effects.contains(&resource_effect) {
                        effects.push(resource_effect);
                    }
                }
            }
        }
        // `#[benign(E)]` effects never propagate to callers; strip them here.
        if let Some(benign) = self
            .func_index
            .get(&key)
            .map(|sig| &sig.benign)
            .filter(|benign| !benign.is_empty())
        {
            effects.retain(|e| !benign.contains(e));
        }
        effects
    }

    /// Find the effect signature for a function by reference (O(1) lookup)
    fn find_effect_signature(&self, func_ref: &FunctionRef) -> Option<&EffectSignature> {
        let key = (func_ref.module_source.clone(), func_ref.name.clone());
        self.func_index.get(&key)
    }
}

||||||| 87daa297
#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckMode {
    EffectsOnly,
    StoresOnly,
}

/// Effect checker that walks TIR and validates effect requirements
struct EffectChecker<'a, H: CompilerHost> {
    modules: &'a IndexMap<ModuleSource, TirModule>,
    logger: &'a Logger<'a, H>,
    /// Current function's effects (set when entering a function)
    current_effects: IndexSet<EffectRef>,
    /// Current function's stores-declared parameter names
    current_stores: IndexSet<String>,
    /// Current function's reference parameter names (for detecting violations).
    /// Only contains parameters whose type is `&T` or `&mut T`.
    current_ref_params: IndexSet<String>,
    /// Type table (shared across modules)
    type_table: Option<Rc<RefCell<TypeTable>>>,
    /// What this checker is checking
    mode: CheckMode,
    /// Pre-built index: (`module_source`, `func_name`) -> `EffectSignature`
    func_index: IndexMap<(ModuleSource, String), EffectSignature>,
    /// Set of `(module_source, resource_name)` pairs for every declared resource.
    /// Used to inject the resource as an effect for any call to one of its methods.
    resource_names: IndexSet<(ModuleSource, String)>,
    /// Effect / resource propagation closure.
    ///
    /// For each effect or resource `E`, `closure[E]` is the transitive set
    /// of resource effects implied by holding `E`. When a function enters a
    /// `with` scope, every concrete effect in its signature is expanded through
    /// this map before checking callees. Keyed by the full `EffectRef::Concrete`
    /// (i.e. `(module_source, name)`), matching how every other resolved
    /// symbol identifies itself.
    closure: IndexMap<EffectRef, IndexSet<EffectRef>>,
    /// Pre-indexed struct field types, keyed by `(module_source, struct_name)`.
    /// Shared by `build_propagation_closure` and signature-resource inference so
    /// both paths can walk nested resource references inside struct values.
    struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Pre-indexed variant case payload types, keyed by `(module_source,
    /// variant_name)`. Same role as `struct_fields` for variant payloads.
    variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>>,
}

impl<'a, H: CompilerHost> EffectChecker<'a, H> {
    fn new(modules: &'a IndexMap<ModuleSource, TirModule>, logger: &'a Logger<'a, H>) -> Self {
        let type_table = modules.values().next().map(|m| Rc::clone(&m.type_table));
        let mut func_index = IndexMap::default();
        let mut resource_names: IndexSet<(ModuleSource, String)> = IndexSet::default();
        for (module_source, module) in modules {
            for resource in &module.resources {
                resource_names.insert((module_source.clone(), resource.name.clone()));
            }
        }
        for (module_source, module) in modules {
            for func_rc in &module.functions {
                let func = func_rc.borrow();
                func_index.insert(
                    (module_source.clone(), func.name.clone()),
                    EffectSignature {
                        effects: func.effects.clone(),
                        params: func
                            .params
                            .iter()
                            .map(|p| EffectParam {
                                name: p.name.clone(),
                                type_id: p.type_id,
                            })
                            .collect(),
                        is_method: func.is_method(),
                        is_ambient: func.is_ambient,
                        benign: func.benign_effects.clone(),
                    },
                );
            }
            for impl_block in &module.impls {
                for method in &impl_block.methods {
                    func_index.insert(
                        (module_source.clone(), method.name.clone()),
                        EffectSignature {
                            effects: method.effects.clone(),
                            params: method
                                .params
                                .iter()
                                .map(|p| EffectParam {
                                    name: p.name.clone(),
                                    type_id: p.type_id,
                                })
                                .collect(),
                            is_method: method.is_method(),
                            is_ambient: method.is_ambient,
                            benign: method.benign_effects.clone(),
                        },
                    );
                }
            }
        }
        let mut struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        let mut variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>> =
            IndexMap::default();
        for (module_source, module) in modules {
            for s in &module.structs {
                let tids: Vec<TypeId> = s.fields.iter().map(|f| f.type_id).collect();
                struct_fields.insert((module_source.clone(), s.name.clone()), tids);
            }
            for v in &module.variants {
                let tids: Vec<TypeId> = v.cases.iter().map(|c| c.payload).collect();
                variant_payloads.insert((module_source.clone(), v.name.clone()), tids);
            }
        }
        let closure = build_propagation_closure(
            modules,
            type_table.as_ref(),
            &struct_fields,
            &variant_payloads,
        );
        Self {
            modules,
            logger,
            current_effects: IndexSet::default(),
            current_stores: IndexSet::default(),
            current_ref_params: IndexSet::default(),
            type_table,
            mode: CheckMode::EffectsOnly,
            func_index,
            resource_names,
            closure,
            struct_fields,
            variant_payloads,
        }
    }

    /// Classify an effect reference as a `resource` or `effect` for diagnostic
    /// wording. Looks up `(module_source, name)` in the resource index; falls
    /// back to `Effect` for anything not registered as a resource.
    fn classify(&self, effect: &EffectRef) -> EffectKind {
        match effect {
            EffectRef::Concrete {
                name,
                module_source,
            } => {
                if self
                    .resource_names
                    .contains(&(module_source.clone(), name.clone()))
                {
                    EffectKind::Resource
                } else {
                    EffectKind::Effect
                }
            }
            EffectRef::Param { .. } => EffectKind::Effect,
        }
    }

    /// Expand a set of effects through the propagation closure.
    ///
    /// For each concrete effect in the input, unions in the closure entries.
    /// `Param` effects are passed through unchanged — they are resolved at
    /// call sites via function-typed arguments.
    fn expand_effects(&self, effects: &IndexSet<EffectRef>) -> IndexSet<EffectRef> {
        let mut out: IndexSet<EffectRef> = IndexSet::default();
        for eff in effects {
            out.insert(eff.clone());
            if matches!(eff, EffectRef::Concrete { .. })
                && let Some(extra) = self.closure.get(eff)
            {
                for e in extra {
                    out.insert(e.clone());
                }
            }
        }
        out
    }

    /// Resources that appear anywhere in the function's signature — parameter
    /// types or the return type, recursively through containers, struct fields
    /// and variant payloads. These are unioned with the declared `with` set so
    /// `fn f(s: Stream<u8>)` does not need to repeat `with Stream`.
    fn signature_resources(&self, func: &TirFunction) -> IndexSet<EffectRef> {
        let mut out: IndexSet<EffectRef> = IndexSet::default();
        let Some(tt_rc) = &self.type_table else {
            return out;
        };
        let tt = tt_rc.borrow();
        let mut visited = TypeSet::default();
        for p in &func.params {
            collect_resource_refs(
                p.type_id,
                &tt,
                &self.struct_fields,
                &self.variant_payloads,
                &mut out,
                &mut visited,
            );
        }
        collect_resource_refs(
            func.return_type,
            &tt,
            &self.struct_fields,
            &self.variant_payloads,
            &mut out,
            &mut visited,
        );
        // Async functions erase `return_type` to unit (the result travels via
        // `task return`), so walk the declared task return type too.
        if let Some(task_ret) = func.task_return_type {
            collect_resource_refs(
                task_ret,
                &tt,
                &self.struct_fields,
                &self.variant_payloads,
                &mut out,
                &mut visited,
            );
        }
        out
    }

    /// Check all modules
    fn check_all(&mut self) -> Result<(), Bail> {
        for module in self.modules.values() {
            self.check_module(module)?;
        }
        Ok(())
    }

    /// Check a single module
    fn check_module(&mut self, module: &TirModule) -> Result<(), Bail> {
        // Scope diagnostics to this module so EffectError spans carry the
        // correct filename instead of whatever an earlier phase last set.
        self.logger
            .set_file(module.module_source.diagnostic_filename());

        // Check all functions
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            self.check_function(&func)?;
        }

        // Check impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                self.check_function(method)?;
            }
        }
        Ok(())
    }

    /// Whether stores checking applies to this function.
    fn should_check_stores(&self, func: &TirFunction) -> bool {
        if self.mode != CheckMode::StoresOnly {
            return false;
        }
        // Skip CM binding functions — they are wrappers generated for the component model boundary
        // and handle data transfer via CM semantics (copy/own/borrow), not Wado references.
        if func.is_cm_binding {
            return false;
        }
        // Skip functions with no parameters at all
        if func.params.is_empty() {
            return false;
        }
        true
    }

    /// Check a single function
    fn check_function(&mut self, func: &TirFunction) -> Result<(), Bail> {
        // Skip test functions - they implicitly have all effects
        if func.name.starts_with("__test_") {
            return Ok(());
        }

        // Skip CM binding functions - they are boundary code with special effect semantics
        if func.is_cm_binding {
            return Ok(());
        }

        // Skip dispatch wrapper functions - they are synthesised dispatch
        // infrastructure that internally calls cm_binding adapters /
        // canonical intrinsics, which carry their own effects. The
        // wrapper's caller doesn't need to declare those effects because
        // the wrapper itself satisfies them; mirroring the `is_cm_binding`
        // skip above keeps the call-site analysis honest.
        if func.is_dispatch_wrapper {
            return Ok(());
        }

        // `#[ambient]` functions intentionally bypass the effect system so
        // logging / panic / unreachable work anywhere. Their bodies perform
        // resource operations (Stream::new, Future::drop) that would otherwise
        // require `with Stream`, `with Future`, etc.
        if func.is_ambient {
            return Ok(());
        }

        // Set current context. Start with the user-declared `with` set, add
        // resources that appear anywhere in the signature (param / return types,
        // recursively through containers, struct fields, variant payloads) so
        // the user does not need to repeat `with R` when `R` is already visible
        // in the signature, then expand through the propagation closure so
        // resources referenced by an effect's operations (e.g.
        // `Stdout::write_via_stream` references `Stream`, `Future`) are
        // implicitly admitted.
        let mut effects: IndexSet<EffectRef> = func.effects.iter().cloned().collect();
        effects.extend(self.signature_resources(func));
        // `#[benign(E)]` admits `E` in the body without a `with E` clause.
        // `get_function_effects` strips it from the outgoing set, so callers
        // never see it.
        effects.extend(func.benign_effects.iter().cloned());
        self.current_effects = self.expand_effects(&effects);
        self.current_stores = func.stores.iter().cloned().collect();

        // Track which parameters are reference types (only for stores checking)
        self.current_ref_params = IndexSet::default();
        if self.should_check_stores(func)
            && let Some(tt_rc) = &self.type_table
        {
            let tt = tt_rc.borrow();
            for param in &func.params {
                let resolved = tt.get(param.type_id);
                if matches!(
                    resolved,
                    crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
                ) {
                    self.current_ref_params.insert(param.name.clone());
                }
            }
        }

        // Check the body if present
        if let Some(body) = &func.body {
            self.check_block(body)?;
        }
        Ok(())
    }

    /// Check a block
    fn check_block(&mut self, block: &TirBlock) -> Result<(), Bail> {
        for stmt in &block.stmts {
            self.check_stmt(stmt)?;
        }
        Ok(())
    }

    /// Check a statement
    fn check_stmt(&mut self, stmt: &TirStmt) -> Result<(), Bail> {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.check_expr(value)?;
            }
            TirStmtKind::Expr(expr) => self.check_expr(expr)?,
            TirStmtKind::Return { value } => {
                if let Some(e) = value {
                    self.check_stores_violation_return(e)?;
                    self.check_expr(e)?;
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.check_expr(condition)?;
                self.check_block(then_block)?;
                if let Some(else_blk) = else_block {
                    self.check_block(else_blk)?;
                }
            }
            TirStmtKind::Loop { body } => {
                self.check_block(body)?;
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(expr) = value {
                    self.check_expr(expr)?;
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.check_block(block)?;
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.check_expr(value)?;
            }
            TirStmtKind::TaskReturn { value } => {
                self.check_expr(value)?;
            }
            TirStmtKind::VariadicForOf { .. } => {}
        }
        Ok(())
    }

    /// Check an expression for effect violations
    fn check_expr(&mut self, expr: &TirExpr) -> Result<(), Bail> {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                self.check_call_with_args(func, args, expr.span)?;
                for arg in args {
                    self.check_expr(&arg.expr)?;
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                self.check_expr(receiver)?;
                self.check_call_with_args(func, args, expr.span)?;
                for arg in args {
                    self.check_expr(&arg.expr)?;
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                // CmRawCall is used inside synthesized adapter functions;
                // no effect checking needed (adapter functions are always effectful)
                for arg in args {
                    self.check_expr(arg)?;
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.check_expr(callee)?;
                for arg in args {
                    self.check_expr(arg)?;
                }
                // Check effects from the callee's function type.
                //
                // The function type's `effects` list is the exact set the type
                // checker stored at the closure expression — it is NOT
                // closure-expanded. `current_effects` *is* expanded at function
                // entry, so a caller that declares `with Stdout` can invoke an
                // indirect closure that internally needs `Stream` (via Stdout
                // propagation). Cases where the function-type effect list
                // itself should be augmented with propagation (e.g. handing
                // such a closure further down) are not yet handled; any such
                // call that actually leaks a resource through propagation
                // would need to re-resolve the callee's concrete effects
                // first.
                if self.mode == CheckMode::EffectsOnly
                    && let Some(tt) = &self.type_table
                {
                    let tt = tt.borrow();
                    if let ResolvedType::Function { effects, .. } = tt.get(callee.type_id) {
                        for effect in effects {
                            if !self.current_effects.contains(effect) {
                                let kind = self.classify(effect);
                                self.logger.error(EffectError {
                                    callee: "(indirect call)".to_string(),
                                    missing_effect: effect.name().to_string(),
                                    kind,
                                    span: expr.span,
                                })?;
                            }
                        }
                    }
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.check_expr(left)?;
                self.check_expr(right)?;
            }
            TirExprKind::Unary { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Assign { target, value } => {
                self.check_expr(target)?;
                self.check_expr(value)?;
            }
            TirExprKind::Cast { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::FieldAccess { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Index { expr, index } => {
                self.check_expr(expr)?;
                self.check_expr(index)?;
            }
            TirExprKind::Block(block) => {
                self.check_block(block)?;
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.check_expr(condition)?;
                self.check_block(then_branch)?;
                if let Some(else_blk) = else_branch {
                    self.check_block(else_blk)?;
                }
            }
            TirExprKind::Match { expr, arms } => {
                self.check_expr(expr)?;
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.check_expr(guard)?;
                    }
                    self.check_expr(&arm.body)?;
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.check_stores_violation_struct_field(&field.value, &field.name)?;
                    self.check_expr(&field.value)?;
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.check_expr(elem)?;
                }
            }
            TirExprKind::TupleSpread { expr }
            | TirExprKind::TupleZip { expr }
            | TirExprKind::TypePackExpansion {
                call_expr: expr, ..
            } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Closure {
                body,
                declared_effects,
                ..
            } => {
                // When the closure has an explicit effect annotation (e.g. via
                // `let f: fn() = ...`), the body must satisfy only that effect
                // set — leaking outer effects through a more-pure-than-declared
                // closure type would break callers that trusted the annotation.
                //
                // Unannotated closures keep inheriting the enclosing
                // function's effects, matching the pre-WEP behaviour.
                if let Some(declared) = declared_effects {
                    let mut declared_set: IndexSet<EffectRef> = declared.iter().cloned().collect();
                    declared_set = self.expand_effects(&declared_set);
                    let saved = std::mem::replace(&mut self.current_effects, declared_set);
                    let result = self.check_expr(body);
                    self.current_effects = saved;
                    result?;
                } else {
                    self.check_expr(body)?;
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.check_expr(payload_expr)?;
                }
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.check_expr(inner)?;
                    }
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.check_block(block)?;
            }
            TirExprKind::GlobalVarSet { name, value, .. } => {
                self.check_stores_violation_global(value, name)?;
                self.check_expr(value)?;
            }
            TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::VariantPayload { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::WithHandler { bindings, body, .. } => {
                // Handler expressions are evaluated in the outer scope (no
                // effect substitution applies to them).
                for binding in bindings {
                    self.check_expr(&binding.handler)?;
                }
                // Inside the body, any effect handled by a binding is
                // satisfied locally — temporarily extend `current_effects`
                // with the handled effects so the body's calls to those
                // effects don't propagate to the caller.
                let added: Vec<EffectRef> = bindings
                    .iter()
                    .filter_map(|b| b.effect.clone())
                    .filter(|e| !self.current_effects.contains(e))
                    .collect();
                for eff in &added {
                    self.current_effects.insert(eff.clone());
                }
                let result = self.check_block(body);
                for eff in &added {
                    self.current_effects.shift_remove(eff);
                }
                result?;
            }
            TirExprKind::Resume { value } => {
                // `resume` is control-flow; the value flows into the
                // suspended computation. Check the value expression itself.
                self.check_expr(value)?;
            }
            // Leaf expressions - no sub-expressions to check
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
        }
        Ok(())
    }

    /// Check a function call for effect violations, resolving effect parameters
    /// from function-typed arguments when needed.
    fn check_call_with_args(
        &mut self,
        func_ref: &FunctionRef,
        args: &[CallArg],
        span: Span,
    ) -> Result<(), Bail> {
        if self.mode != CheckMode::EffectsOnly {
            return Ok(());
        }
        let callee_effects = self.get_function_effects(func_ref);

        // Separate effect params from concrete effects
        let has_params = callee_effects.iter().any(super::tir::EffectRef::is_param);

        // Resolve effect params to concrete effects from function-typed arguments
        let resolved_effects = if has_params {
            self.resolve_effect_params(&callee_effects, func_ref, args)
        } else {
            callee_effects
        };

        for effect in &resolved_effects {
            if !self.current_effects.contains(effect) {
                let kind = self.classify(effect);
                self.logger.error(EffectError {
                    callee: func_ref.name.clone(),
                    missing_effect: effect.name().to_string(),
                    kind,
                    span,
                })?;
            }
        }
        Ok(())
    }

    /// Check if an expression traces back to a reference parameter not declared in stores.
    /// Returns the parameter name if it's a stores violation, None otherwise.
    fn find_unstored_ref_param(&self, expr: &TirExpr) -> Option<String> {
        match &expr.kind {
            TirExprKind::Local { name, .. } => {
                // Check if this local is a reference parameter not in stores
                if self.current_ref_params.contains(name) && !self.current_stores.contains(name) {
                    Some(name.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Check stores violations: a function that stores a reference parameter must declare stores[param]
    fn check_stores_violation_return(&mut self, value: &TirExpr) -> Result<(), Bail> {
        if let Some(param_name) = self.find_unstored_ref_param(value) {
            self.logger.error(StoresError {
                message: format!(
                    "returning reference parameter '{param_name}' requires `stores[{param_name}]` declaration"
                ),
                span: value.span,
            })?;
        }
        Ok(())
    }

    /// Check stores violation: storing a reference parameter in a struct field
    fn check_stores_violation_struct_field(
        &mut self,
        value: &TirExpr,
        _field_name: &str,
    ) -> Result<(), Bail> {
        if let Some(param_name) = self.find_unstored_ref_param(value) {
            self.logger.error(StoresError {
                message: format!(
                    "storing reference parameter '{param_name}' in struct field requires `stores[{param_name}]` declaration"
                ),
                span: value.span,
            })?;
        }
        Ok(())
    }

    /// Check stores violation: assigning a reference parameter to a global
    fn check_stores_violation_global(
        &mut self,
        value: &TirExpr,
        global_name: &str,
    ) -> Result<(), Bail> {
        if let Some(param_name) = self.find_unstored_ref_param(value) {
            self.logger.error(StoresError {
                message: format!(
                    "storing reference parameter '{param_name}' in global '{global_name}' requires `stores[{param_name}]` declaration"
                ),
                span: value.span,
            })?;
        }
        Ok(())
    }

    /// Resolve effect parameters to concrete effects by examining function-typed arguments.
    ///
    /// For `fn wrapper<effect E>(f: fn() with E) with E`, when called with a closure
    /// that has effects `[Stdout]`, E resolves to `[Stdout]`.
    fn resolve_effect_params(
        &self,
        callee_effects: &[EffectRef],
        func_ref: &FunctionRef,
        args: &[CallArg],
    ) -> Vec<EffectRef> {
        // Collect the names of effect params
        let effect_param_names: IndexSet<String> = callee_effects
            .iter()
            .filter_map(|e| match e {
                EffectRef::Param { name } => Some(name.clone()),
                EffectRef::Concrete { .. } => None,
            })
            .collect();

        // Map each effect param name to its resolved concrete effects
        let mut effect_param_concrete: IndexMap<String, IndexSet<EffectRef>> = IndexMap::default();
        for name in &effect_param_names {
            effect_param_concrete.insert(name.clone(), IndexSet::default());
        }

        // Look at the callee's parameter types and actual argument types
        if let Some(sig) = self.find_effect_signature(func_ref)
            && let Some(tt) = &self.type_table
        {
            let tt = tt.borrow();
            // For method calls, args doesn't include the receiver (self), but params does.
            // Skip the self parameter when the function is a method.
            let params_iter: Box<dyn Iterator<Item = _>> =
                if sig.is_method && !sig.params.is_empty() && sig.params[0].name == "self" {
                    Box::new(sig.params.iter().skip(1))
                } else {
                    Box::new(sig.params.iter())
                };
            for (param, arg) in params_iter.zip(args.iter()) {
                if let ResolvedType::Function {
                    effects: formal_effects,
                    ..
                } = tt.get(param.type_id)
                {
                    let has_effect_params = formal_effects
                        .iter()
                        .any(|e| e.is_param() && effect_param_names.contains(e.name()));
                    if !has_effect_params {
                        continue;
                    }

                    if let ResolvedType::Function {
                        effects: actual_effects,
                        ..
                    } = tt.get(arg.expr.type_id)
                    {
                        for formal_effect in formal_effects {
                            if let EffectRef::Param { name } = formal_effect
                                && let Some(concrete_set) = effect_param_concrete.get_mut(name)
                            {
                                for actual in actual_effects {
                                    concrete_set.insert(actual.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        // Build resolved effect list: replace param names with concrete effects.
        // Expand concrete effects through the propagation closure so that a
        // resolved effect param pointing at e.g. `Stdout` also picks up
        // `Stream`, `Future`, etc. (propagation is applied symmetrically to
        // caller context; see `check_function`).
        let mut resolved: IndexSet<EffectRef> = IndexSet::default();
        for effect in callee_effects {
            match effect {
                EffectRef::Param { name } => {
                    if let Some(concrete_set) = effect_param_concrete.get(name) {
                        let expanded = self.expand_effects(concrete_set);
                        for concrete in expanded {
                            resolved.insert(concrete);
                        }
                    }
                }
                EffectRef::Concrete { .. } => {
                    resolved.insert(effect.clone());
                }
            }
        }
        resolved.into_iter().collect()
    }

    /// Get the effects for a function.
    ///
    /// If the call targets a method on a resource type, the resource itself is
    /// implicitly required as an effect (resources are effects). Resource
    /// operations are host calls so the caller must declare the resource in
    /// its `with` clause to use them.
    fn get_function_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        let key = (func_ref.module_source.clone(), func_ref.name.clone());
        // `#[ambient]` callees expose no effects to callers — they bypass the
        // effect system by design.
        if self.func_index.get(&key).is_some_and(|sig| sig.is_ambient) {
            return Vec::new();
        }
        let mut effects = self
            .func_index
            .get(&key)
            .map(|sig| sig.effects.clone())
            .unwrap_or_default();
        if let Some(method_info) = &func_ref.method_info {
            // Trait methods (`trait_name` is Some) are dispatch through user or
            // auto-derived trait impls — they are not host operations on the
            // resource itself, so they don't trigger the resource effect.
            // Only direct resource operations (constructors, instance, statics
            // declared in the `resource` block) require the effect.
            if method_info.trait_name.is_none() {
                let resource_key = (
                    func_ref.module_source.clone(),
                    method_info.base_struct_name.clone(),
                );
                if self.resource_names.contains(&resource_key) {
                    let resource_effect = EffectRef::Concrete {
                        name: method_info.base_struct_name.clone(),
                        module_source: func_ref.module_source.clone(),
                    };
                    if !effects.contains(&resource_effect) {
                        effects.push(resource_effect);
                    }
                }
            }
        }
        // `#[benign(E)]` effects never propagate to callers; strip them here.
        if let Some(benign) = self
            .func_index
            .get(&key)
            .map(|sig| &sig.benign)
            .filter(|benign| !benign.is_empty())
        {
            effects.retain(|e| !benign.contains(e));
        }
        effects
    }

    /// Find the effect signature for a function by reference (O(1) lookup)
    fn find_effect_signature(&self, func_ref: &FunctionRef) -> Option<&EffectSignature> {
        let key = (func_ref.module_source.clone(), func_ref.name.clone());
        self.func_index.get(&key)
    }
}

=======
>>>>>>> origin/main
/// Error from default-value purity checking
#[derive(Debug, Clone)]
pub struct DefaultPurityError {
    pub callee: String,
    pub span: Span,
}

impl std::fmt::Display for DefaultPurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: default value expression must be pure (no effects), but calls effectful function '{}'",
            self.span.line, self.span.column, self.callee
        )
    }
}

impl std::error::Error for DefaultPurityError {}

impl From<DefaultPurityError> for crate::compiler_host::Diagnostic {
    fn from(e: DefaultPurityError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: format!(
                "default value expression must be pure (no effects), but calls effectful function '{}'",
                e.callee
            ),
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
        }
    }
}

<<<<<<< HEAD
/// Check that every parameter and struct-field default expression is pure.
///
/// Defaults must not transitively call any function that declares effects
/// (WEP 2026-04-11). Runs after `check_effects` so the effect map is built.
pub fn check_default_purity<H: CompilerHost>(
    modules: &IndexMap<ModuleSource, TirModule>,
    logger: &Logger<H>,
) -> Result<(), Bail> {
    let checker = EffectChecker::new(modules, logger);
    for module in modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            for param in &func.params {
                if let Some(default) = &param.default_expr {
                    check_pure_expr(&checker, default, logger);
                }
            }
        }
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                for param in &method.params {
                    if let Some(default) = &param.default_expr {
                        check_pure_expr(&checker, default, logger);
                    }
                }
            }
        }
        for struct_decl in &module.structs {
            for field in &struct_decl.fields {
                if let Some(default) = &field.default_expr {
                    check_pure_expr(&checker, default, logger);
                }
            }
        }
    }
    logger.ok_or_bail(())
}

fn check_pure_expr<H: CompilerHost>(
    checker: &EffectChecker<'_, H>,
    expr: &TirExpr,
    logger: &Logger<'_, H>,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let effects = checker.get_function_effects(func);
            if !effects.is_empty() {
                let _ = logger.error(DefaultPurityError {
                    callee: func.name.clone(),
                    span: expr.span,
                });
            }
            for arg in args {
                check_pure_expr(checker, &arg.expr, logger);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let effects = checker.get_function_effects(func);
            if !effects.is_empty() {
                let _ = logger.error(DefaultPurityError {
                    callee: func.name.clone(),
                    span: expr.span,
                });
            }
            check_pure_expr(checker, receiver, logger);
            for arg in args {
                check_pure_expr(checker, &arg.expr, logger);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            check_pure_expr(checker, callee, logger);
            for arg in args {
                check_pure_expr(checker, arg, logger);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            let _ = logger.error(DefaultPurityError {
                callee: "<cm-raw>".to_string(),
                span: expr.span,
            });
            for arg in args {
                check_pure_expr(checker, arg, logger);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            check_pure_expr(checker, left, logger);
            check_pure_expr(checker, right, logger);
        }
        TirExprKind::Unary { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::Assign { target, value } => {
            check_pure_expr(checker, target, logger);
            check_pure_expr(checker, value, logger);
        }
        TirExprKind::Cast { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::FieldAccess { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::Index { expr: e, index } => {
            check_pure_expr(checker, e, logger);
            check_pure_expr(checker, index, logger);
        }
        TirExprKind::Block(block) => check_pure_block(checker, block, logger),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_pure_expr(checker, condition, logger);
            check_pure_block(checker, then_branch, logger);
            if let Some(else_blk) = else_branch {
                check_pure_block(checker, else_blk, logger);
            }
        }
        TirExprKind::Match { expr: e, arms } => {
            check_pure_expr(checker, e, logger);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    check_pure_expr(checker, guard, logger);
                }
                check_pure_expr(checker, &arm.body, logger);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                check_pure_expr(checker, &field.value, logger);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                check_pure_expr(checker, elem, logger);
            }
        }
        TirExprKind::TupleSpread { expr: e }
        | TirExprKind::TupleZip { expr: e }
        | TirExprKind::TupleLen { expr: e }
        | TirExprKind::TypePackExpansion { call_expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::Closure { body, .. } => {
            check_pure_expr(checker, body, logger);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                check_pure_expr(checker, p, logger);
            }
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    check_pure_expr(checker, inner, logger);
                }
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            check_pure_block(checker, block, logger);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            check_pure_expr(checker, value, logger);
        }
        TirExprKind::VariantTag { expr: e } | TirExprKind::VariantTest { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::VariantPayload { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            // `with` blocks install handlers and run a body. They are not
            // pure (they touch the dispatch global), so emit an error.
            let _ = logger.error(DefaultPurityError {
                callee: "<with-handler>".to_string(),
                span: expr.span,
            });
            for binding in bindings {
                check_pure_expr(checker, &binding.handler, logger);
            }
            check_pure_block(checker, body, logger);
        }
        TirExprKind::Resume { value } => {
            // `resume` is control-flow inside a handler; cannot appear in
            // a pure default expression context.
            let _ = logger.error(DefaultPurityError {
                callee: "<resume>".to_string(),
                span: expr.span,
            });
            check_pure_expr(checker, value, logger);
        }
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
    }
}

fn check_pure_block<H: CompilerHost>(
    checker: &EffectChecker<'_, H>,
    block: &TirBlock,
    logger: &Logger<'_, H>,
) {
    for stmt in &block.stmts {
        check_pure_stmt(checker, stmt, logger);
    }
}

fn check_pure_stmt<H: CompilerHost>(
    checker: &EffectChecker<'_, H>,
    stmt: &TirStmt,
    logger: &Logger<'_, H>,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            check_pure_expr(checker, value, logger);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(e) = value {
                check_pure_expr(checker, e, logger);
            }
        }
        TirStmtKind::TaskReturn { value } => check_pure_expr(checker, value, logger),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            check_pure_expr(checker, condition, logger);
            check_pure_block(checker, then_block, logger);
            if let Some(else_blk) = else_block {
                check_pure_block(checker, else_blk, logger);
            }
        }
        TirStmtKind::Loop { body } => check_pure_block(checker, body, logger),
        TirStmtKind::Continue => {}
        TirStmtKind::LabeledBlock { block, .. } => check_pure_block(checker, block, logger),
        TirStmtKind::LetDestructure { value, .. } => check_pure_expr(checker, value, logger),
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

/// Build the transitive propagation closure for effects and resources.
///
/// An effect (or resource) `E` "propagates" any resource that appears in the
/// parameter or return types of its operations. For example,
/// `Stdout::write_via_stream(rx: Stream<u8>) -> Future<...>` makes `Stdout`
/// propagate `Stream` and `Future`. The closure is then taken over the
/// `propagates` relation so holding `with Stdout` transitively admits
/// `Stream`, `StreamWritable`, `Future`, `FutureWritable`.
///
/// Returns a map keyed by `EffectRef::Concrete` (i.e. `(module_source, name)`).
/// Each entry contains the transitively-reachable resources (also
/// `EffectRef::Concrete`), but does NOT include the effect itself.
fn build_propagation_closure(
    modules: &IndexMap<ModuleSource, TirModule>,
    type_table: Option<&Rc<RefCell<TypeTable>>>,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
) -> IndexMap<EffectRef, IndexSet<EffectRef>> {
    let mut direct: IndexMap<EffectRef, IndexSet<EffectRef>> = IndexMap::default();
    let Some(tt_rc) = type_table else {
        return direct;
    };
    let tt = tt_rc.borrow();

    for (module_source, module) in modules {
        for effect in &module.effects {
            let mut refs: IndexSet<EffectRef> = IndexSet::default();
            for op in &effect.operations {
                for param in &op.params {
                    collect_resource_refs(
                        param.type_id,
                        &tt,
                        struct_fields,
                        variant_payloads,
                        &mut refs,
                        &mut TypeSet::default(),
                    );
                }
                collect_resource_refs(
                    op.return_type,
                    &tt,
                    struct_fields,
                    variant_payloads,
                    &mut refs,
                    &mut TypeSet::default(),
                );
            }
            // Do not include the effect itself (effects are not resources).
            let key = EffectRef::Concrete {
                name: effect.name.clone(),
                module_source: module_source.clone(),
            };
            merge_sets(direct.entry(key).or_default(), &refs);
        }
        for resource in &module.resources {
            let mut refs: IndexSet<EffectRef> = IndexSet::default();
            for op in &resource.operations {
                for param in &op.params {
                    collect_resource_refs(
                        param.type_id,
                        &tt,
                        struct_fields,
                        variant_payloads,
                        &mut refs,
                        &mut TypeSet::default(),
                    );
                }
                collect_resource_refs(
                    op.return_type,
                    &tt,
                    struct_fields,
                    variant_payloads,
                    &mut refs,
                    &mut TypeSet::default(),
                );
            }
            // Drop the self-reference: holding `with R` already implies `R`.
            let self_ref = EffectRef::Concrete {
                name: resource.name.clone(),
                module_source: module_source.clone(),
            };
            refs.shift_remove(&self_ref);
            merge_sets(direct.entry(self_ref).or_default(), &refs);
        }
    }

    // Fixpoint: close over `propagates` until no set changes.
    loop {
        let mut changed = false;
        let keys: Vec<EffectRef> = direct.keys().cloned().collect();
        for key in &keys {
            let cur = direct.get(key).cloned().unwrap_or_default();
            let mut merged: IndexSet<EffectRef> = cur.clone();
            for eff in &cur {
                if matches!(eff, EffectRef::Concrete { .. })
                    && let Some(child) = direct.get(eff).cloned()
                {
                    for e in &child {
                        if merged.insert(e.clone()) {
                            changed = true;
                        }
                    }
                }
            }
            if merged.len() != cur.len() {
                direct.insert(key.clone(), merged);
            }
        }
        if !changed {
            break;
        }
    }

    direct
}

fn merge_sets(dst: &mut IndexSet<EffectRef>, src: &IndexSet<EffectRef>) {
    for e in src {
        dst.insert(e.clone());
    }
}

||||||| 87daa297
/// Check that every parameter and struct-field default expression is pure.
///
/// Defaults must not transitively call any function that declares effects
/// (WEP 2026-04-11). Runs after `check_effects` so the effect map is built.
pub fn check_default_purity<H: CompilerHost>(
    modules: &IndexMap<ModuleSource, TirModule>,
    logger: &Logger<H>,
) -> Result<(), Bail> {
    let checker = EffectChecker::new(modules, logger);
    for module in modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            for param in &func.params {
                if let Some(default) = &param.default_expr {
                    check_pure_expr(&checker, default, logger);
                }
            }
        }
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                for param in &method.params {
                    if let Some(default) = &param.default_expr {
                        check_pure_expr(&checker, default, logger);
                    }
                }
            }
        }
        for struct_decl in &module.structs {
            for field in &struct_decl.fields {
                if let Some(default) = &field.default_expr {
                    check_pure_expr(&checker, default, logger);
                }
            }
        }
    }
    logger.ok_or_bail(())
}

fn check_pure_expr<H: CompilerHost>(
    checker: &EffectChecker<'_, H>,
    expr: &TirExpr,
    logger: &Logger<'_, H>,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let effects = checker.get_function_effects(func);
            if !effects.is_empty() {
                let _ = logger.error(DefaultPurityError {
                    callee: func.name.clone(),
                    span: expr.span,
                });
            }
            for arg in args {
                check_pure_expr(checker, &arg.expr, logger);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let effects = checker.get_function_effects(func);
            if !effects.is_empty() {
                let _ = logger.error(DefaultPurityError {
                    callee: func.name.clone(),
                    span: expr.span,
                });
            }
            check_pure_expr(checker, receiver, logger);
            for arg in args {
                check_pure_expr(checker, &arg.expr, logger);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            check_pure_expr(checker, callee, logger);
            for arg in args {
                check_pure_expr(checker, arg, logger);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            let _ = logger.error(DefaultPurityError {
                callee: "<cm-raw>".to_string(),
                span: expr.span,
            });
            for arg in args {
                check_pure_expr(checker, arg, logger);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            check_pure_expr(checker, left, logger);
            check_pure_expr(checker, right, logger);
        }
        TirExprKind::Unary { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::Assign { target, value } => {
            check_pure_expr(checker, target, logger);
            check_pure_expr(checker, value, logger);
        }
        TirExprKind::Cast { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::FieldAccess { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::Index { expr: e, index } => {
            check_pure_expr(checker, e, logger);
            check_pure_expr(checker, index, logger);
        }
        TirExprKind::Block(block) => check_pure_block(checker, block, logger),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_pure_expr(checker, condition, logger);
            check_pure_block(checker, then_branch, logger);
            if let Some(else_blk) = else_branch {
                check_pure_block(checker, else_blk, logger);
            }
        }
        TirExprKind::Match { expr: e, arms } => {
            check_pure_expr(checker, e, logger);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    check_pure_expr(checker, guard, logger);
                }
                check_pure_expr(checker, &arm.body, logger);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                check_pure_expr(checker, &field.value, logger);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                check_pure_expr(checker, elem, logger);
            }
        }
        TirExprKind::TupleSpread { expr: e }
        | TirExprKind::TupleZip { expr: e }
        | TirExprKind::TypePackExpansion { call_expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::Closure { body, .. } => {
            check_pure_expr(checker, body, logger);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                check_pure_expr(checker, p, logger);
            }
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                    check_pure_expr(checker, inner, logger);
                }
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            check_pure_block(checker, block, logger);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            check_pure_expr(checker, value, logger);
        }
        TirExprKind::VariantTag { expr: e } | TirExprKind::VariantTest { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::VariantPayload { expr: e, .. } => {
            check_pure_expr(checker, e, logger);
        }
        TirExprKind::WithHandler { bindings, body, .. } => {
            // `with` blocks install handlers and run a body. They are not
            // pure (they touch the dispatch global), so emit an error.
            let _ = logger.error(DefaultPurityError {
                callee: "<with-handler>".to_string(),
                span: expr.span,
            });
            for binding in bindings {
                check_pure_expr(checker, &binding.handler, logger);
            }
            check_pure_block(checker, body, logger);
        }
        TirExprKind::Resume { value } => {
            // `resume` is control-flow inside a handler; cannot appear in
            // a pure default expression context.
            let _ = logger.error(DefaultPurityError {
                callee: "<resume>".to_string(),
                span: expr.span,
            });
            check_pure_expr(checker, value, logger);
        }
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
    }
}

fn check_pure_block<H: CompilerHost>(
    checker: &EffectChecker<'_, H>,
    block: &TirBlock,
    logger: &Logger<'_, H>,
) {
    for stmt in &block.stmts {
        check_pure_stmt(checker, stmt, logger);
    }
}

fn check_pure_stmt<H: CompilerHost>(
    checker: &EffectChecker<'_, H>,
    stmt: &TirStmt,
    logger: &Logger<'_, H>,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            check_pure_expr(checker, value, logger);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(e) = value {
                check_pure_expr(checker, e, logger);
            }
        }
        TirStmtKind::TaskReturn { value } => check_pure_expr(checker, value, logger),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            check_pure_expr(checker, condition, logger);
            check_pure_block(checker, then_block, logger);
            if let Some(else_blk) = else_block {
                check_pure_block(checker, else_blk, logger);
            }
        }
        TirStmtKind::Loop { body } => check_pure_block(checker, body, logger),
        TirStmtKind::Continue => {}
        TirStmtKind::LabeledBlock { block, .. } => check_pure_block(checker, block, logger),
        TirStmtKind::LetDestructure { value, .. } => check_pure_expr(checker, value, logger),
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

/// Build the transitive propagation closure for effects and resources.
///
/// An effect (or resource) `E` "propagates" any resource that appears in the
/// parameter or return types of its operations. For example,
/// `Stdout::write_via_stream(rx: Stream<u8>) -> Future<...>` makes `Stdout`
/// propagate `Stream` and `Future`. The closure is then taken over the
/// `propagates` relation so holding `with Stdout` transitively admits
/// `Stream`, `StreamWritable`, `Future`, `FutureWritable`.
///
/// Returns a map keyed by `EffectRef::Concrete` (i.e. `(module_source, name)`).
/// Each entry contains the transitively-reachable resources (also
/// `EffectRef::Concrete`), but does NOT include the effect itself.
fn build_propagation_closure(
    modules: &IndexMap<ModuleSource, TirModule>,
    type_table: Option<&Rc<RefCell<TypeTable>>>,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
) -> IndexMap<EffectRef, IndexSet<EffectRef>> {
    let mut direct: IndexMap<EffectRef, IndexSet<EffectRef>> = IndexMap::default();
    let Some(tt_rc) = type_table else {
        return direct;
    };
    let tt = tt_rc.borrow();

    for (module_source, module) in modules {
        for effect in &module.effects {
            let mut refs: IndexSet<EffectRef> = IndexSet::default();
            for op in &effect.operations {
                for param in &op.params {
                    collect_resource_refs(
                        param.type_id,
                        &tt,
                        struct_fields,
                        variant_payloads,
                        &mut refs,
                        &mut TypeSet::default(),
                    );
                }
                collect_resource_refs(
                    op.return_type,
                    &tt,
                    struct_fields,
                    variant_payloads,
                    &mut refs,
                    &mut TypeSet::default(),
                );
            }
            // Do not include the effect itself (effects are not resources).
            let key = EffectRef::Concrete {
                name: effect.name.clone(),
                module_source: module_source.clone(),
            };
            merge_sets(direct.entry(key).or_default(), &refs);
        }
        for resource in &module.resources {
            let mut refs: IndexSet<EffectRef> = IndexSet::default();
            for op in &resource.operations {
                for param in &op.params {
                    collect_resource_refs(
                        param.type_id,
                        &tt,
                        struct_fields,
                        variant_payloads,
                        &mut refs,
                        &mut TypeSet::default(),
                    );
                }
                collect_resource_refs(
                    op.return_type,
                    &tt,
                    struct_fields,
                    variant_payloads,
                    &mut refs,
                    &mut TypeSet::default(),
                );
            }
            // Drop the self-reference: holding `with R` already implies `R`.
            let self_ref = EffectRef::Concrete {
                name: resource.name.clone(),
                module_source: module_source.clone(),
            };
            refs.shift_remove(&self_ref);
            merge_sets(direct.entry(self_ref).or_default(), &refs);
        }
    }

    // Fixpoint: close over `propagates` until no set changes.
    loop {
        let mut changed = false;
        let keys: Vec<EffectRef> = direct.keys().cloned().collect();
        for key in &keys {
            let cur = direct.get(key).cloned().unwrap_or_default();
            let mut merged: IndexSet<EffectRef> = cur.clone();
            for eff in &cur {
                if matches!(eff, EffectRef::Concrete { .. })
                    && let Some(child) = direct.get(eff).cloned()
                {
                    for e in &child {
                        if merged.insert(e.clone()) {
                            changed = true;
                        }
                    }
                }
            }
            if merged.len() != cur.len() {
                direct.insert(key.clone(), merged);
            }
        }
        if !changed {
            break;
        }
    }

    direct
}

fn merge_sets(dst: &mut IndexSet<EffectRef>, src: &IndexSet<EffectRef>) {
    for e in src {
        dst.insert(e.clone());
    }
}

=======
>>>>>>> origin/main
/// Walk a type recursively, collecting every resource (`Resource` or
/// `GenericResource`) reference as an `EffectRef::Concrete`.
///
/// Handles nested containers (`Option<T>`, `Result<T,E>`, tuples, `List<T>`,
/// function types, refs, newtypes, struct fields, variant case payloads).
/// Uses `visited` to stop at cycles (e.g. recursive struct types).
fn collect_resource_refs(
    type_id: TypeId,
    tt: &TypeTable,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    out: &mut IndexSet<EffectRef>,
    visited: &mut TypeSet,
) {
    if !visited.insert(type_id) {
        return;
    }
    let ty = tt.get(type_id);
    match ty {
        ResolvedType::Resource {
            name,
            module_source,
        }
        | ResolvedType::GenericResource {
            name,
            module_source,
            ..
        } => {
            out.insert(EffectRef::Concrete {
                name: name.clone(),
                module_source: module_source.clone(),
            });
            if let ResolvedType::GenericResource { type_args, .. } = ty {
                for ta in type_args {
                    collect_resource_refs(*ta, tt, struct_fields, variant_payloads, out, visited);
                }
            }
        }
        ResolvedType::GenericInstance { type_args, .. } => {
            for ta in type_args {
                collect_resource_refs(*ta, tt, struct_fields, variant_payloads, out, visited);
            }
        }
        ResolvedType::Ref(t)
        | ResolvedType::MutRef(t)
        | ResolvedType::Reactive(t)
        | ResolvedType::BuiltinArray(t) => {
            collect_resource_refs(*t, tt, struct_fields, variant_payloads, out, visited);
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            for p in params {
                collect_resource_refs(*p, tt, struct_fields, variant_payloads, out, visited);
            }
            collect_resource_refs(
                *return_type,
                tt,
                struct_fields,
                variant_payloads,
                out,
                visited,
            );
        }
        ResolvedType::Newtype { base_type, .. } => {
            collect_resource_refs(
                *base_type,
                tt,
                struct_fields,
                variant_payloads,
                out,
                visited,
            );
        }
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => {
            if let Some(fields) = struct_fields.get(&(module_source.clone(), name.clone())) {
                for ft in fields {
                    collect_resource_refs(*ft, tt, struct_fields, variant_payloads, out, visited);
                }
            }
        }
        ResolvedType::Variant {
            name,
            module_source,
        } => {
            if let Some(payloads) = variant_payloads.get(&(module_source.clone(), name.clone())) {
                for pt in payloads {
                    collect_resource_refs(*pt, tt, struct_fields, variant_payloads, out, visited);
                }
            }
        }
        // Primitives, Unit, Never, Enum, Flags, TypeParam, TypePack,
        // AssocTypeProjection, Unknown, Error — no resource refs.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Semantics-based effect checking (Design B, Phase 1b)
// ---------------------------------------------------------------------------

/// Effect checking over [`Semantics`] (AST + recorded facts) — the Design B
/// effect checker. It runs after `annotate_bodies`, so it sees every function,
/// dead or live, and is independent of what reify emits. It also works on the
/// LSP path, which builds no TIR. Violations are returned rather than emitted
/// so the caller routes them (LSP diagnostics or the batch logger).
///
/// Covers free-function, method, and static dispatch with resource injection,
/// the effect / resource propagation closure, signature-resource inference,
/// effect-parameter resolution, `#[benign]`, handler-scope grants, and
/// indirect (closure) calls, over user-authored modules.
#[must_use]
pub fn check_effects_semantic(sem: &Semantics) -> Vec<EffectError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };
    let data = OwnedEffectData::build(sem, state);
    run_effect_checks(sem, &data.index(), &mut out);
    out
}

/// All three Design-B semantic diagnostics, computed in one pass that builds the
/// shared [`OwnedEffectData`] once. Used by the batch driver and the LSP so
/// effect / stores / purity stay in lockstep across both.
#[must_use]
pub fn check_semantics(sem: &Semantics) -> SemanticDiagnostics {
    let mut diags = SemanticDiagnostics::default();
    let Some(state) = sem.state.as_ref() else {
        return diags;
    };
    let data = OwnedEffectData::build(sem, state);
    let index = data.index();
    run_effect_checks(sem, &index, &mut diags.effects);
    run_purity_checks(sem, &index, &mut diags.purity);
    diags.stores = check_stores_semantic(sem);
    diags
}

/// Bundle of the Design-B semantic diagnostics returned by [`check_semantics`].
#[derive(Default)]
pub struct SemanticDiagnostics {
    pub effects: Vec<EffectError>,
    pub stores: Vec<StoresError>,
    pub purity: Vec<DefaultPurityError>,
}

impl SemanticDiagnostics {
    /// Whether any check produced a violation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.stores.is_empty() && self.purity.is_empty()
    }
}

/// Walk every user-authored function / method / trait method, appending effect
/// violations. Shared by [`check_effects_semantic`] and [`check_semantics`].
fn run_effect_checks(sem: &Semantics, index: &EffectIndex, out: &mut Vec<EffectError>) {
    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    check_function_effects_sem(sem, src, func, index, out);
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        check_function_effects_sem(sem, src, method, index, out);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        check_function_effects_sem(sem, src, method, index, out);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Owns the cross-module effect maps so multiple checks (effects, default
/// purity) can borrow a single [`EffectIndex`] view over them. Assembled once
/// from [`Semantics`] + [`AnnotateState`].
struct OwnedEffectData {
    fn_effects: IndexMap<SymbolKey, Vec<EffectRef>>,
    fn_params: IndexMap<SymbolKey, Vec<TypeId>>,
    mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    resource_names: IndexSet<(ModuleSource, String)>,
    struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>>,
    closure: IndexMap<EffectRef, IndexSet<EffectRef>>,
    effect_by_name: IndexMap<String, EffectRef>,
}

impl OwnedEffectData {
    fn build(sem: &Semantics, state: &crate::elaborator::orchestration::AnnotateState) -> Self {
        // Resolved effect lists, indexed two ways: by the function's
        // declaration key (free calls resolve through `references`) and by
        // `(module, mangled name)` (method dispatch carries a `FunctionRef`).
        let mut fn_effects: IndexMap<SymbolKey, Vec<EffectRef>> = IndexMap::default();
        let mut fn_params: IndexMap<SymbolKey, Vec<TypeId>> = IndexMap::default();
        let mut mangled_index: IndexMap<(ModuleSource, String), Vec<EffectRef>> =
            IndexMap::default();
        let mut mangled_params: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        for (src, module_sem) in &state.module_semantics {
            let types = &module_sem.types;
            for (key, effects) in &types.function_effects {
                fn_effects.insert(key.clone(), effects.clone());
            }
            for (key, params) in &types.fn_param_types {
                fn_params.insert(key.clone(), params.clone());
            }
            for (key, names) in &types.method_names {
                if let Some(effects) = types.function_effects.get(key) {
                    mangled_index.insert((src.clone(), names.mangled.clone()), effects.clone());
                }
                if let Some(params) = types.fn_param_types.get(key) {
                    mangled_params.insert((src.clone(), names.mangled.clone()), params.clone());
                }
            }
        }

        let mut resource_names: IndexSet<(ModuleSource, String)> = IndexSet::default();
        // `(module, struct name)` → field type ids, so resource detection
        // follows resources nested in struct fields of a signature / op type.
        let mut struct_fields: IndexMap<(ModuleSource, String), Vec<TypeId>> = IndexMap::default();
        for (src, module) in &sem.modules {
            let annotations = state.module_semantics.get(src).map(|m| &m.types);
            for item in &module.items {
                match item {
                    Item::Resource(resource) => {
                        resource_names.insert((src.clone(), resource.name.clone()));
                    }
                    Item::Struct(struct_decl) => {
                        if let Some(field_types) = annotations.and_then(|ann| {
                            ann.struct_field_types
                                .get(&SymbolKey::new(src.clone(), struct_decl.id))
                        }) {
                            struct_fields.insert(
                                (src.clone(), struct_decl.name.clone()),
                                field_types.clone(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        // `(module, variant name)` → case payload type ids, so resource
        // detection descends into variant case payloads.
        let mut variant_payloads: IndexMap<(ModuleSource, String), Vec<TypeId>> =
            IndexMap::default();
        for (module, variants) in state.tysys.all_variant_cases.iter() {
            for (variant_name, info) in variants {
                variant_payloads.insert(
                    (module.clone(), variant_name.clone()),
                    info.cases.iter().map(|case| case.payload).collect(),
                );
            }
        }

        // Effect / resource propagation closure: holding effect `E` admits the
        // resources `E`'s operations reference (e.g. `Stdout` → `Stream`).
        let closure = build_propagation_closure_sem(sem, state, &struct_fields, &variant_payloads);

        // Name → resolved `EffectRef` for every declared effect / resource,
        // used to resolve `#[benign(E)]` names and to canonicalise.
        let mut effect_by_name: IndexMap<String, EffectRef> = IndexMap::default();
        for key in closure.keys() {
            if let EffectRef::Concrete { name, .. } = key {
                effect_by_name
                    .entry(name.clone())
                    .or_insert_with(|| key.clone());
            }
        }

        Self {
            fn_effects,
            fn_params,
            mangled_index,
            mangled_params,
            resource_names,
            struct_fields,
            variant_payloads,
            closure,
            effect_by_name,
        }
    }

    fn index(&self) -> EffectIndex<'_> {
        EffectIndex {
            fn_effects: &self.fn_effects,
            fn_params: &self.fn_params,
            mangled_index: &self.mangled_index,
            mangled_params: &self.mangled_params,
            resource_names: &self.resource_names,
            struct_fields: &self.struct_fields,
            variant_payloads: &self.variant_payloads,
            closure: &self.closure,
            effect_by_name: &self.effect_by_name,
        }
    }
}

/// The cross-module effect data the body walk consults, assembled once.
struct EffectIndex<'a> {
    /// Declaration key → resolved effects (free calls resolve via `references`).
    fn_effects: &'a IndexMap<SymbolKey, Vec<EffectRef>>,
    /// Declaration key → parameter type ids (for effect-parameter resolution).
    fn_params: &'a IndexMap<SymbolKey, Vec<TypeId>>,
    /// `(module, mangled name)` → effects (method / static dispatch).
    mangled_index: &'a IndexMap<(ModuleSource, String), Vec<EffectRef>>,
    /// `(module, mangled name)` → parameter type ids.
    mangled_params: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Declared resources, for resource injection and effect classification.
    resource_names: &'a IndexSet<(ModuleSource, String)>,
    /// `(module, struct name)` → field type ids, for nested-resource detection.
    struct_fields: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// `(module, variant name)` → case payload type ids.
    variant_payloads: &'a IndexMap<(ModuleSource, String), Vec<TypeId>>,
    /// Effect → implied resources propagation closure.
    closure: &'a IndexMap<EffectRef, IndexSet<EffectRef>>,
    /// Declared effect / resource name → resolved `EffectRef` (`#[benign]`).
    effect_by_name: &'a IndexMap<String, EffectRef>,
}

fn check_function_effects_sem(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    index: &EffectIndex,
    out: &mut Vec<EffectError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    // `#[ambient]` bypasses the effect system; test helpers implicitly hold
    // every effect.
    if func.attrs.iter().any(|attr| attr.name == "ambient") || func.name.starts_with("__test_") {
        return;
    }
    let caller_key = SymbolKey::new(module.clone(), func.id);

    // Per-module annotations carry the dispatch facts and signature types that
    // have no flattened `Semantics` mirror (static-method dispatch,
    // param / return type ids).
    let annotations = sem
        .state
        .as_ref()
        .and_then(|state| state.module_semantics.get(module))
        .map(|module_sem| &module_sem.types);

    // Declared effects, plus resources that appear in the signature so a
    // `fn f(s: Stream<u8>)` need not repeat `with Stream`.
    let mut current: IndexSet<EffectRef> = index
        .fn_effects
        .get(&caller_key)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    if let Some(ann) = annotations {
        add_signature_resources(
            ann,
            &caller_key,
            &sem.types,
            index.struct_fields,
            index.variant_payloads,
            &mut current,
        );
    }
    // `#[benign(E)]` admits `E` in the body without a `with E` clause.
    for name in benign_effect_names(&func.attrs) {
        if let Some(effect) = index.effect_by_name.get(&name) {
            current.insert(effect.clone());
        }
    }
    // Canonicalise before expanding so the closure keys (built from the
    // declarations, i.e. canonical) match, then expand: a function holding
    // `Stdout` may call operations that internally need `Stream`, etc.
    let current: IndexSet<EffectRef> = current
        .iter()
        .map(|effect| canonicalize_effect(effect, index.effect_by_name))
        .collect();
    let current = expand_through_closure(&current, index.closure);

    // Parameter name → type id (aligned with the recorded signature types),
    // for resolving indirect calls through function-typed parameters.
    let mut param_types: IndexMap<String, TypeId> = IndexMap::default();
    if let Some(type_ids) = annotations.and_then(|ann| ann.fn_param_types.get(&caller_key)) {
        for (param, type_id) in func.params.iter().zip(type_ids.iter()) {
            param_types.insert(param.name.clone(), *type_id);
        }
    }

    let mut walker = SemEffectWalker {
        module,
        sem,
        annotations,
        index,
        current,
        param_types,
        out,
    };
    ast::walk_block(&mut walker, body);
}

/// Build the effect / resource propagation closure from `Semantics`: for each
/// effect or resource declaration, the resources its operations' parameter and
/// return types reference, transitively closed. Reads the resolved operation
/// signatures from the `effect_ops` facts; `struct_fields` / `variant_payloads`
/// let resource detection descend into struct fields and variant payloads of an
/// operation's types.
fn build_propagation_closure_sem(
    sem: &Semantics,
    state: &crate::elaborator::orchestration::AnnotateState,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
) -> IndexMap<EffectRef, IndexSet<EffectRef>> {
    let type_table = &sem.types;
    let mut direct: IndexMap<EffectRef, IndexSet<EffectRef>> = IndexMap::default();

    for (src, module) in &sem.modules {
        let Some(annotations) = state.module_semantics.get(src).map(|m| &m.types) else {
            continue;
        };
        for item in &module.items {
            let (decl_id, decl_name, is_resource) = match item {
                Item::Interface(decl) => (decl.id, &decl.name, false),
                Item::Resource(decl) => (decl.id, &decl.name, true),
                _ => continue,
            };
            let Some(ops) = annotations
                .effect_ops
                .get(&SymbolKey::new(src.clone(), decl_id))
            else {
                continue;
            };
            let mut refs: IndexSet<EffectRef> = IndexSet::default();
            for op in ops {
                for param in &op.params {
                    collect_resource_refs(
                        param.type_id,
                        type_table,
                        struct_fields,
                        variant_payloads,
                        &mut refs,
                        &mut TypeSet::default(),
                    );
                }
                collect_resource_refs(
                    op.return_type,
                    type_table,
                    struct_fields,
                    variant_payloads,
                    &mut refs,
                    &mut TypeSet::default(),
                );
            }
            let key = EffectRef::Concrete {
                name: decl_name.clone(),
                module_source: src.clone(),
            };
            if is_resource {
                // Holding `with R` already implies `R` — drop the self-reference.
                refs.shift_remove(&key);
            }
            let entry = direct.entry(key).or_default();
            for r in refs {
                entry.insert(r);
            }
        }
    }

    // Transitive closure to a fixpoint.
    loop {
        let mut changed = false;
        let keys: Vec<EffectRef> = direct.keys().cloned().collect();
        for key in &keys {
            let cur = direct.get(key).cloned().unwrap_or_default();
            let mut merged = cur.clone();
            for eff in &cur {
                if matches!(eff, EffectRef::Concrete { .. })
                    && let Some(child) = direct.get(eff).cloned()
                {
                    for e in &child {
                        if merged.insert(e.clone()) {
                            changed = true;
                        }
                    }
                }
            }
            if merged.len() != cur.len() {
                direct.insert(key.clone(), merged);
            }
        }
        if !changed {
            break;
        }
    }
    direct
}

/// Canonicalise an effect by name through the declaration index. The raw
/// `EffectRef::Concrete.module_source` recorded in `function_effects` reflects
/// the recording module's import perspective, so two references to the same
/// effect can carry different module sources (user entry vs `wasi:cli`). The
/// declaration index (built from the propagation-closure keys) holds one
/// canonical `EffectRef` per name; mapping through it makes cross-module
/// effect comparison and closure lookups consistent. Effect parameters and
/// names without a declaration are returned unchanged.
fn canonicalize_effect(
    effect: &EffectRef,
    effect_by_name: &IndexMap<String, EffectRef>,
) -> EffectRef {
    match effect {
        EffectRef::Concrete { name, .. } => effect_by_name
            .get(name)
            .cloned()
            .unwrap_or_else(|| effect.clone()),
        EffectRef::Param { .. } => effect.clone(),
    }
}

/// Expand an effect set through the propagation closure.
fn expand_through_closure(
    effects: &IndexSet<EffectRef>,
    closure: &IndexMap<EffectRef, IndexSet<EffectRef>>,
) -> IndexSet<EffectRef> {
    let mut out: IndexSet<EffectRef> = IndexSet::default();
    for effect in effects {
        out.insert(effect.clone());
        if matches!(effect, EffectRef::Concrete { .. })
            && let Some(extra) = closure.get(effect)
        {
            for e in extra {
                out.insert(e.clone());
            }
        }
    }
    out
}

/// Union into `out` the resources that appear in a function's signature —
/// parameter types, the return type, and the async task-return type — so a
/// signature that already exposes a resource does not also require an explicit
/// `with R`.
///
/// Resources nested inside struct fields and variant case payloads are
/// followed via `struct_fields` / `variant_payloads`; direct and
/// container-nested resources (`Option<R>`, `List<R>`, `&R`, `fn() -> R`) are
/// too.
fn add_signature_resources(
    annotations: &crate::elaborator::sem::types::TypeAnnotations,
    fn_key: &SymbolKey,
    type_table: &TypeTable,
    struct_fields: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    variant_payloads: &IndexMap<(ModuleSource, String), Vec<TypeId>>,
    out: &mut IndexSet<EffectRef>,
) {
    let mut visited = TypeSet::default();
    for &type_id in annotations.fn_param_types.get(fn_key).into_iter().flatten() {
        collect_resource_refs(
            type_id,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
    if let Some(&return_type) = annotations.fn_return_types.get(fn_key) {
        collect_resource_refs(
            return_type,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
    if let Some(&task_return) = annotations.function_task_returns.get(fn_key) {
        collect_resource_refs(
            task_return,
            type_table,
            struct_fields,
            variant_payloads,
            out,
            &mut visited,
        );
    }
}

/// `#[benign(E, F)]` effect names declared on a function.
fn benign_effect_names(attrs: &[crate::ast::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|attr| attr.name == "benign")
        .flat_map(|attr| attr.args.iter().map(crate::ast::AttrArg::as_str))
        .map(str::to_string)
        .collect()
}

/// Best-effort display name for a call's callee, for the diagnostic message.
fn callee_name(callee: &Expr) -> &str {
    match callee {
        Expr::Ident(ident) => &ident.name,
        _ => "(call)",
    }
}

/// Walks a function body, checking that each call's required effects are held.
struct SemEffectWalker<'a> {
    module: &'a ModuleSource,
    sem: &'a Semantics,
    annotations: Option<&'a crate::elaborator::sem::types::TypeAnnotations>,
    index: &'a EffectIndex<'a>,
    /// Effects available at the current point: the function's declared +
    /// signature + benign + propagated set, plus any effects granted by an
    /// enclosing `with H => … do { … }` handler scope (pushed / popped as the
    /// walk enters / leaves the do-block body).
    current: IndexSet<EffectRef>,
    /// This function's parameter name → type id, for resolving the callee of an
    /// indirect call through a function-typed parameter (which leaves no
    /// `references` edge or recorded expression type at the call site).
    param_types: IndexMap<String, TypeId>,
    out: &'a mut Vec<EffectError>,
}

impl EffectIndex<'_> {
    /// Effects a method dispatch requires: the callee's declared effects plus,
    /// for a direct (non-trait) method on a `resource`, the resource effect.
    fn method_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        let mut effects = self
            .mangled_index
            .get(&(func_ref.module_source.clone(), func_ref.name.clone()))
            .cloned()
            .unwrap_or_default();
        if let Some(method_info) = &func_ref.method_info
            && method_info.trait_name.is_none()
        {
            let resource_key = (
                func_ref.module_source.clone(),
                method_info.base_struct_name.clone(),
            );
            if self.resource_names.contains(&resource_key) {
                let resource_effect = EffectRef::Concrete {
                    name: method_info.base_struct_name.clone(),
                    module_source: func_ref.module_source.clone(),
                };
                if !effects.contains(&resource_effect) {
                    effects.push(resource_effect);
                }
            }
        }
        effects
    }

    /// Parameter type ids for a method / static dispatch target.
    fn method_param_types(&self, func_ref: &FunctionRef) -> Vec<TypeId> {
        self.mangled_params
            .get(&(func_ref.module_source.clone(), func_ref.name.clone()))
            .cloned()
            .unwrap_or_default()
    }
}

impl SemEffectWalker<'_> {
    fn method_effects(&self, func_ref: &FunctionRef) -> Vec<EffectRef> {
        self.index.method_effects(func_ref)
    }

    /// Resolve `EffectRef::Param` effects to concrete effects by matching the
    /// callee's function-typed parameters against the actual argument types.
    /// `is_method` drops the leading `self` parameter so params line up with
    /// `args`.
    fn resolve_effect_params(
        &self,
        callee_effects: &[EffectRef],
        param_types: &[TypeId],
        is_method: bool,
        args: &[Expr],
    ) -> Vec<EffectRef> {
        let param_names: IndexSet<String> = callee_effects
            .iter()
            .filter_map(|e| match e {
                EffectRef::Param { name } => Some(name.clone()),
                EffectRef::Concrete { .. } => None,
            })
            .collect();
        if param_names.is_empty() {
            return callee_effects.to_vec();
        }
        let mut concrete: IndexMap<String, IndexSet<EffectRef>> = param_names
            .iter()
            .map(|n| (n.clone(), IndexSet::default()))
            .collect();
        let type_table = &self.sem.types;
        let skip = usize::from(is_method && !param_types.is_empty());
        for (param_type, arg) in param_types.iter().skip(skip).zip(args.iter()) {
            let ResolvedType::Function {
                effects: formal, ..
            } = type_table.get(*param_type)
            else {
                continue;
            };
            if !formal
                .iter()
                .any(|e| e.is_param() && param_names.contains(e.name()))
            {
                continue;
            }
            let Some(arg_type) = self
                .sem
                .expression_types
                .get(&SymbolKey::new(self.module.clone(), arg.id()))
                .copied()
            else {
                continue;
            };
            let ResolvedType::Function {
                effects: actual, ..
            } = type_table.get(arg_type)
            else {
                continue;
            };
            for formal_effect in formal {
                if let EffectRef::Param { name } = formal_effect
                    && let Some(set) = concrete.get_mut(name)
                {
                    for a in actual {
                        set.insert(a.clone());
                    }
                }
            }
        }
        let mut resolved = Vec::new();
        for effect in callee_effects {
            match effect {
                EffectRef::Param { name } => {
                    if let Some(set) = concrete.get(name) {
                        for c in expand_through_closure(set, self.index.closure) {
                            resolved.push(c);
                        }
                    }
                }
                EffectRef::Concrete { .. } => resolved.push(effect.clone()),
            }
        }
        resolved
    }

    fn report_missing(&mut self, effects: &[EffectRef], callee: &str, span: Span) {
        for effect in effects {
            // Canonicalise: `EffectRef::Concrete.module_source` reflects the
            // recording module's import perspective (a user `with Stdout`
            // records `Stdout` against the entry module, while stdlib records
            // it against `wasi:cli`), so compare through the declaration's
            // canonical form rather than by raw `module_source`.
            let effect = canonicalize_effect(effect, self.index.effect_by_name);
            // Any `Param` left after resolution did not bind to a concrete
            // effect; skip it rather than report a spurious miss.
            if effect.is_param() || self.current.contains(&effect) {
                continue;
            }
            let effect = &effect;
            let kind = match effect {
                EffectRef::Concrete {
                    name,
                    module_source,
                } if self
                    .index
                    .resource_names
                    .contains(&(module_source.clone(), name.clone())) =>
                {
                    EffectKind::Resource
                }
                _ => EffectKind::Effect,
            };
            self.out.push(EffectError {
                callee: callee.to_string(),
                missing_effect: effect.name().to_string(),
                kind,
                span,
            });
        }
    }
}

impl AstVisitor for SemEffectWalker<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        // `for let v of iterable { … }` desugars to synthetic `.into_iter()` /
        // `.next()` calls that have no source call id, so they record no
        // `method_dispatch` fact for `visit_expr` to consult. Check their
        // declared effects here from the recorded `for_of_iterator` fact.
        if let Stmt::ForOf(for_of) = stmt
            && let Some(info) = self.annotations.and_then(|ann| {
                ann.for_of_iterator
                    .get(&SymbolKey::new(self.module.clone(), for_of.id))
            })
        {
            for func_ref in [&info.into_iter, &info.next] {
                let effects = self.index.method_effects(func_ref);
                let callee = func_ref
                    .method_info
                    .as_ref()
                    .map_or(func_ref.name.as_str(), |m| m.method_name.as_str());
                self.report_missing(&effects, callee, for_of.span);
            }
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                // Free calls resolve through `references` on the callee
                // identifier. `Type::method(...)` / `Self::method(...)` parse as
                // a `Call` with a path callee whose identifier has no free-
                // function reference; they resolve through
                // `static_method_dispatch` keyed by the call id. (Free
                // functions also appear in `static_method_dispatch`, so try
                // `references` first — it is the authoritative free-call edge.)
                let free = if let Expr::Ident(ident) = &call.callee {
                    self.sem
                        .references
                        .get(&SymbolKey::new(self.module.clone(), ident.id))
                        .and_then(|def| {
                            self.index
                                .fn_effects
                                .get(def)
                                .map(|effects| (def.clone(), effects.clone(), ident.name.clone()))
                        })
                } else {
                    None
                };
                if let Some((def, effects, name)) = free {
                    let params = self.index.fn_params.get(&def).cloned().unwrap_or_default();
                    let resolved = self.resolve_effect_params(&effects, &params, false, &call.args);
                    self.report_missing(&resolved, &name, call.span);
                } else if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| {
                        ann.static_method_dispatch
                            .get(&SymbolKey::new(self.module.clone(), call.id))
                    })
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.method_effects(&func_ref);
                    let params = self.method_param_types(&func_ref);
                    let is_method = func_ref.method_info.is_some();
                    let resolved =
                        self.resolve_effect_params(&effects, &params, is_method, &call.args);
                    self.report_missing(&resolved, callee_name(&call.callee), call.span);
                } else if let Some(callee_type) = self.indirect_callee_type(call) {
                    // Indirect call: the callee is a function-typed value (a
                    // closure or `fn(...)` parameter). Its type carries the
                    // effects it performs when invoked.
                    if let ResolvedType::Function { effects, .. } = self.sem.types.get(callee_type)
                    {
                        let effects = effects.clone();
                        self.report_missing(&effects, "(indirect call)", call.span);
                    }
                }
            }
            Expr::MethodCall(method_call) => {
                let call_key = SymbolKey::new(self.module.clone(), method_call.id);
                if let Some(dispatch) = self.sem.method_dispatch.get(&call_key) {
                    let func_ref = dispatch.function_ref.clone();
                    let effects = self.method_effects(&func_ref);
                    let params = self.method_param_types(&func_ref);
                    let resolved =
                        self.resolve_effect_params(&effects, &params, true, &method_call.args);
                    self.report_missing(&resolved, &method_call.method, method_call.span);
                }
            }
            Expr::StaticMethodCall(static_call) => {
                let call_key = SymbolKey::new(self.module.clone(), static_call.id);
                if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| ann.static_method_dispatch.get(&call_key))
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.method_effects(&func_ref);
                    let params = self.method_param_types(&func_ref);
                    let is_method = func_ref.method_info.is_some();
                    let resolved =
                        self.resolve_effect_params(&effects, &params, is_method, &static_call.args);
                    self.report_missing(&resolved, &static_call.method, static_call.span);
                }
            }
            Expr::WithHandler(with_handler) => {
                // `with H => h do { body }` installs handlers, granting each
                // handled effect to the body (calls inside it — directly or via
                // helpers — observe the installed handler). The handler
                // expressions themselves run outside the grant.
                for binding in &with_handler.handlers {
                    ast::walk_expr(self, &binding.handler);
                }
                let granted: Vec<EffectRef> = with_handler
                    .handlers
                    .iter()
                    .filter_map(|b| b.effect.as_ref())
                    .filter_map(|ty| match ty {
                        crate::ast::Type::Named(named) => {
                            self.index.effect_by_name.get(&named.name).cloned()
                        }
                        _ => None,
                    })
                    .collect();
                let added: Vec<EffectRef> = granted
                    .into_iter()
                    .filter(|effect| self.current.insert(effect.clone()))
                    .collect();
                ast::walk_block(self, &with_handler.body);
                for effect in added {
                    self.current.shift_remove(&effect);
                }
                return;
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

impl SemEffectWalker<'_> {
    fn method_param_types(&self, func_ref: &FunctionRef) -> Vec<TypeId> {
        self.index.method_param_types(func_ref)
    }

    /// Type of an indirect call's callee. When the callee is an identifier
    /// bound to a local or parameter, its type lives in `local_types` keyed by
    /// the binding's def (resolved through `references`); function-typed
    /// parameters are recorded there, not in `expression_types`. Other callee
    /// shapes fall back to the expression's recorded type.
    fn indirect_callee_type(&self, call: &crate::ast::CallExpr) -> Option<TypeId> {
        if let Expr::Ident(ident) = &call.callee {
            // A function-typed parameter callee leaves no `references` edge or
            // recorded expression type at the call, so resolve it against the
            // enclosing function's parameter types by name first.
            if let Some(type_id) = self.param_types.get(&ident.name) {
                return Some(*type_id);
            }
            if let Some(type_id) = self
                .sem
                .references
                .get(&SymbolKey::new(self.module.clone(), ident.id))
                .and_then(|def| self.sem.local_types.get(def))
            {
                return Some(*type_id);
            }
        }
        self.sem
            .expression_types
            .get(&SymbolKey::new(self.module.clone(), call.callee.id()))
            .copied()
    }
}

// ---------------------------------------------------------------------------
// Semantics-based stores checking (Design B)
// ---------------------------------------------------------------------------

/// Stores checking over [`Semantics`] — the Design B stores checker. A
/// function that lets a reference parameter
/// escape — by returning it, storing it in a struct field, or assigning it to
/// a global — must declare `stores[param]`. Walks the source AST, so it sees
/// every function regardless of what reify emits and is immune to dead-code
/// gating. Violations are returned for the caller to route.
#[must_use]
pub fn check_stores_semantic(sem: &Semantics) -> Vec<StoresError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };

    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        let annotations = state.module_semantics.get(src).map(|m| &m.types);
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    check_function_stores_sem(sem, src, func, annotations, &mut out);
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        check_function_stores_sem(sem, src, method, annotations, &mut out);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn check_function_stores_sem(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    annotations: Option<&crate::elaborator::sem::types::TypeAnnotations>,
    out: &mut Vec<StoresError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    // `#[ambient]` bypasses the reference discipline; test helpers are exempt.
    if func.attrs.iter().any(|attr| attr.name == "ambient") || func.name.starts_with("__test_") {
        return;
    }
    if func.params.is_empty() {
        return;
    }
    let Some(annotations) = annotations else {
        return;
    };
    let key = SymbolKey::new(module.clone(), func.id);
    let Some(param_types) = annotations.fn_param_types.get(&key) else {
        return;
    };

    // Reference parameters: only `&T` / `&mut T` parameters can be stored, so
    // only they can produce a stores violation.
    let type_table = &sem.types;
    let mut ref_params: IndexSet<String> = IndexSet::default();
    for (param, &type_id) in func.params.iter().zip(param_types.iter()) {
        if matches!(
            type_table.get(type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        ) {
            ref_params.insert(param.name.clone());
        }
    }
    if ref_params.is_empty() {
        return;
    }

    let stores: IndexSet<String> = func.stores.iter().cloned().collect();
    let mut walker = StoresWalker {
        module,
        annotations,
        ref_params,
        stores,
        out,
    };
    ast::walk_block(&mut walker, body);
}

/// Walks a function body flagging reference parameters that escape without a
/// matching `stores[param]` declaration.
struct StoresWalker<'a> {
    module: &'a ModuleSource,
    annotations: &'a crate::elaborator::sem::types::TypeAnnotations,
    /// Reference (`&T` / `&mut T`) parameter names of the enclosing function.
    ref_params: IndexSet<String>,
    /// `stores[...]`-declared parameter names — escapes of these are allowed.
    stores: IndexSet<String>,
    out: &'a mut Vec<StoresError>,
}

impl StoresWalker<'_> {
    /// If `expr` is a bare reference to a reference parameter that is *not*
    /// declared in `stores[...]`, return its name. Only a direct identifier
    /// counts — `&x.field` and the like do not store the parameter itself.
    fn unstored_ref_param<'e>(&self, expr: &'e Expr) -> Option<&'e str> {
        if let Expr::Ident(ident) = expr
            && self.ref_params.contains(&ident.name)
            && !self.stores.contains(&ident.name)
        {
            return Some(&ident.name);
        }
        None
    }
}

impl AstVisitor for StoresWalker<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let Stmt::Return(ret) = stmt
            && let Some(value) = &ret.value
            && let Some(param) = self.unstored_ref_param(value)
        {
            self.out.push(StoresError {
                message: format!(
                    "returning reference parameter '{param}' requires `stores[{param}]` declaration"
                ),
                span: value.span(),
            });
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::StructLiteral(lit) => {
                for field in &lit.fields {
                    if let Some(param) = self.unstored_ref_param(&field.value) {
                        self.out.push(StoresError {
                            message: format!(
                                "storing reference parameter '{param}' in struct field requires `stores[{param}]` declaration"
                            ),
                            span: field.value.span(),
                        });
                    }
                }
            }
            Expr::Assign(assign) => {
                // A reference parameter assigned to a module global escapes; the
                // assign place recorded by the elaborator (the same fact reify
                // reads to build `GlobalVarSet`) identifies the global by name.
                if let Some(param) = self.unstored_ref_param(&assign.value)
                    && let Some(place) = self
                        .annotations
                        .assign_places
                        .get(&SymbolKey::new(self.module.clone(), assign.target.id()))
                    && let crate::elaborator::sem::types::AssignPlace::Global { name, .. } = place
                {
                    self.out.push(StoresError {
                        message: format!(
                            "storing reference parameter '{param}' in global '{name}' requires `stores[{param}]` declaration"
                        ),
                        span: assign.value.span(),
                    });
                }
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

// ---------------------------------------------------------------------------
// Semantics-based default-value purity checking (Design B)
// ---------------------------------------------------------------------------

/// Default-value purity over [`Semantics`] — the Design B default-value purity
/// checker. Every `param: T = expr` and
/// `field: T = expr` default must be pure: it may not call any function that
/// declares effects, nor install an effect handler. Walks the source default
/// expressions directly. Violations are returned for the caller to route.
#[must_use]
pub fn check_default_purity_semantic(sem: &Semantics) -> Vec<DefaultPurityError> {
    let mut out = Vec::new();
    let Some(state) = sem.state.as_ref() else {
        return out;
    };
    let data = OwnedEffectData::build(sem, state);
    run_purity_checks(sem, &data.index(), &mut out);
    out
}

/// Walk every user-authored parameter / field default, appending impurity
/// violations. Shared by [`check_default_purity_semantic`] and
/// [`check_semantics`].
fn run_purity_checks(sem: &Semantics, index: &EffectIndex, out: &mut Vec<DefaultPurityError>) {
    let Some(state) = sem.state.as_ref() else {
        return;
    };
    let walk = |src: &ModuleSource,
                annotations: Option<&crate::elaborator::sem::types::TypeAnnotations>,
                params: &[crate::ast::Param],
                out: &mut Vec<DefaultPurityError>| {
        for param in params {
            if let Some(default) = &param.default {
                purity_walk_default(sem, src, annotations, index, default, out);
            }
        }
    };

    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        let annotations = state.module_semantics.get(src).map(|m| &m.types);
        for item in &module.items {
            match item {
                Item::Function(func) => walk(src, annotations, &func.params, out),
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        walk(src, annotations, &method.params, out);
                    }
                }
                Item::Trait(trait_decl) => {
                    // Parity with the effect checker's trait coverage. Note the
                    // trait/effect method signature path does not yet resolve
                    // param defaults (item.rs builds them with `default_expr:
                    // None` and no expression context), so a trait-method
                    // default's calls leave no `references` edge for the walker
                    // to flag until that annotation lands.
                    for method in &trait_decl.methods {
                        walk(src, annotations, &method.params, out);
                    }
                }
                Item::Struct(struct_decl) => {
                    for field in &struct_decl.fields {
                        if let Some(default) = &field.default {
                            purity_walk_default(sem, src, annotations, index, default, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn purity_walk_default(
    sem: &Semantics,
    module: &ModuleSource,
    annotations: Option<&crate::elaborator::sem::types::TypeAnnotations>,
    index: &EffectIndex,
    default: &Expr,
    out: &mut Vec<DefaultPurityError>,
) {
    let mut walker = PurityWalker {
        module,
        sem,
        annotations,
        index,
        out,
    };
    walker.visit_expr(default);
}

/// Walks a default expression flagging any call to an effectful function (or an
/// effect-handler install), which would make the default impure.
struct PurityWalker<'a> {
    module: &'a ModuleSource,
    sem: &'a Semantics,
    annotations: Option<&'a crate::elaborator::sem::types::TypeAnnotations>,
    index: &'a EffectIndex<'a>,
    out: &'a mut Vec<DefaultPurityError>,
}

impl PurityWalker<'_> {
    fn flag_if_effectful(&mut self, effects: &[EffectRef], callee: &str, span: Span) {
        if !effects.is_empty() {
            self.out.push(DefaultPurityError {
                callee: callee.to_string(),
                span,
            });
        }
    }
}

impl AstVisitor for PurityWalker<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                let free = if let Expr::Ident(ident) = &call.callee {
                    self.sem
                        .references
                        .get(&SymbolKey::new(self.module.clone(), ident.id))
                        .and_then(|def| self.index.fn_effects.get(def))
                        .map(|effects| (effects.clone(), ident.name.clone()))
                } else {
                    None
                };
                if let Some((effects, name)) = free {
                    self.flag_if_effectful(&effects, &name, call.span);
                } else if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| {
                        ann.static_method_dispatch
                            .get(&SymbolKey::new(self.module.clone(), call.id))
                    })
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.index.method_effects(&func_ref);
                    self.flag_if_effectful(&effects, callee_name(&call.callee), call.span);
                }
            }
            Expr::MethodCall(method_call) => {
                if let Some(dispatch) = self
                    .sem
                    .method_dispatch
                    .get(&SymbolKey::new(self.module.clone(), method_call.id))
                {
                    let effects = self.index.method_effects(&dispatch.function_ref);
                    self.flag_if_effectful(&effects, &method_call.method, method_call.span);
                }
            }
            Expr::StaticMethodCall(static_call) => {
                if let Some(func_ref) = self
                    .annotations
                    .and_then(|ann| {
                        ann.static_method_dispatch
                            .get(&SymbolKey::new(self.module.clone(), static_call.id))
                    })
                    .map(|dispatch| dispatch.function_ref.clone())
                {
                    let effects = self.index.method_effects(&func_ref);
                    self.flag_if_effectful(&effects, &static_call.method, static_call.span);
                }
            }
            Expr::WithHandler(with_handler) => {
                // Installing a handler touches the dispatch global — impure.
                self.out.push(DefaultPurityError {
                    callee: "<with-handler>".to_string(),
                    span: with_handler.span,
                });
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effect_error_display() {
        let error = EffectError {
            callee: "println".to_string(),
            missing_effect: "Stdout".to_string(),
            kind: EffectKind::Effect,
            span: Span {
                start: 100,
                end: 107,
                line: 10,
                column: 5,
                end_line: 10,
                end_column: 12,
            },
        };
        assert_eq!(
            error.to_string(),
            "10:5: missing effect 'Stdout' required by 'println'"
        );
    }
}
