//! Effect checking phase for Wado
//!
//! This phase validates that all function calls have the required effects.
//! A function can only call another function if it has all the effects
//! that the callee requires.
//!
//! Effect checking runs after type resolution (TIR construction) and before
//! lowering. It operates on the TIR and produces errors for any effect violations.

use crate::hashmap::{IndexMap, IndexSet};

use crate::compiler_host::CompilerHost;
use crate::logger::{Bail, Logger};
use crate::name::ModuleSource;
use crate::tir::{
    CallArg, EffectRef, FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction,
    TirModule, TirStmt, TirStmtKind, TirTemplatePart, TypeId, TypeTable,
};
use crate::token::Span;
use std::cell::RefCell;
use std::rc::Rc;

/// Lightweight signature extracted from `TirFunction` for effect checking.
/// Avoids cloning the entire function body.
struct EffectSignature {
    effects: Vec<EffectRef>,
    params: Vec<EffectParam>,
    is_method: bool,
    /// `#[ambient]` — the function bypasses caller effect requirements.
    /// Its own effects are ignored when checking callers.
    is_ambient: bool,
}

/// Minimal parameter info needed for effect resolution.
struct EffectParam {
    name: String,
    type_id: TypeId,
}

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

/// Check effects for all modules (runs before synthesis).
///
/// Errors are emitted to the logger. Returns `Err(Bail)` if any errors found.
pub fn check_effects<H: CompilerHost>(
    modules: &IndexMap<ModuleSource, TirModule>,
    logger: &Logger<H>,
) -> Result<(), Bail> {
    let mut checker = EffectChecker::new(modules, logger);
    checker.mode = CheckMode::EffectsOnly;
    let _ = checker.check_all();
    logger.ok_or_bail(())
}

/// Check stores for all modules (runs after synthesis, before optimization).
///
/// Validates that functions storing reference parameters declare `stores[...]`.
/// Runs after synthesis so synthesized functions are also checked.
pub fn check_stores<H: CompilerHost>(
    modules: &IndexMap<ModuleSource, TirModule>,
    logger: &Logger<H>,
) -> Result<(), Bail> {
    let mut checker = EffectChecker::new(modules, logger);
    checker.mode = CheckMode::StoresOnly;
    let _ = checker.check_all();
    logger.ok_or_bail(())
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
                        },
                    );
                }
            }
        }
        let closure = build_propagation_closure(modules, type_table.as_ref());
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

    /// Check all modules
    fn check_all(&mut self) -> Result<(), Bail> {
        for module in self.modules.values() {
            self.check_module(module)?;
        }
        Ok(())
    }

    /// Check a single module
    fn check_module(&mut self, module: &TirModule) -> Result<(), Bail> {
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

        // `#[ambient]` functions intentionally bypass the effect system so
        // logging / panic / unreachable work anywhere. Their bodies perform
        // resource operations (Stream::new, Future::drop) that would otherwise
        // require `with Stream`, `with Future`, etc.
        if func.is_ambient {
            return Ok(());
        }

        // Set current context. Expand concrete effects through the propagation
        // closure so resources referenced by an effect's operations (e.g.
        // `Stdout::write_via_stream` references `Stream`, `Future`) are
        // implicitly admitted.
        let declared: IndexSet<EffectRef> = func.effects.iter().cloned().collect();
        self.current_effects = self.expand_effects(&declared);
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
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.check_expr(scrutinee)?;
                self.check_block(then_block)?;
                if let Some(else_blk) = else_block {
                    self.check_block(else_blk)?;
                }
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
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.check_expr(functor)?;
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
            TirExprKind::Closure { body, .. } => {
                // Closures inherit effects from enclosing function, so we continue checking
                self.check_expr(body)?;
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
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.check_expr(scrutinee)?;
                for arm in arms {
                    self.check_block(arm)?;
                }
                self.check_block(default)?;
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
        effects
    }

    /// Find the effect signature for a function by reference (O(1) lookup)
    fn find_effect_signature(&self, func_ref: &FunctionRef) -> Option<&EffectSignature> {
        let key = (func_ref.module_source.clone(), func_ref.name.clone());
        self.func_index.get(&key)
    }
}

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
        TirExprKind::ClosureToCanonical { functor, .. } => {
            check_pure_expr(checker, functor, logger);
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
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            check_pure_expr(checker, scrutinee, logger);
            for arm in arms {
                check_pure_block(checker, arm, logger);
            }
            check_pure_block(checker, default, logger);
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
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            check_pure_expr(checker, scrutinee, logger);
            check_pure_block(checker, then_block, logger);
            if let Some(else_blk) = else_block {
                check_pure_block(checker, else_blk, logger);
            }
        }
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
                    collect_resource_refs(param.type_id, &tt, &mut refs, &mut IndexSet::default());
                }
                collect_resource_refs(op.return_type, &tt, &mut refs, &mut IndexSet::default());
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
                    collect_resource_refs(param.type_id, &tt, &mut refs, &mut IndexSet::default());
                }
                collect_resource_refs(op.return_type, &tt, &mut refs, &mut IndexSet::default());
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

/// Walk a type recursively, collecting every resource (`Resource` or
/// `GenericResource`) reference as an `EffectRef::Concrete`.
///
/// Handles nested containers (`Option<T>`, `Result<T,E>`, tuples, `Array<T>`,
/// function types, refs, newtypes). Uses `visited` to stop at cycles (e.g.
/// recursive struct types).
fn collect_resource_refs(
    type_id: TypeId,
    tt: &TypeTable,
    out: &mut IndexSet<EffectRef>,
    visited: &mut IndexSet<TypeId>,
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
                    collect_resource_refs(*ta, tt, out, visited);
                }
            }
        }
        ResolvedType::GenericInstance { type_args, .. } => {
            for ta in type_args {
                collect_resource_refs(*ta, tt, out, visited);
            }
        }
        ResolvedType::Ref(t)
        | ResolvedType::MutRef(t)
        | ResolvedType::Reactive(t)
        | ResolvedType::BuiltinArray(t) => {
            collect_resource_refs(*t, tt, out, visited);
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            for p in params {
                collect_resource_refs(*p, tt, out, visited);
            }
            collect_resource_refs(*return_type, tt, out, visited);
        }
        ResolvedType::Newtype { base_type, .. } => {
            collect_resource_refs(*base_type, tt, out, visited);
        }
        // Primitives, Unit, Never, Struct, Enum, Variant, TypeParam, TypePack,
        // AssocTypeProjection, Flags, Unknown, Error — no resource refs.
        _ => {}
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
