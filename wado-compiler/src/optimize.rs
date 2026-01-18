//! Optimization pass for Wado TIR
//!
//! This module provides:
//! - Dead Code Elimination (DCE) at function level
//! - Usage analysis for conditional feature inclusion

use crate::name::{FreeFunctionName, FunctionId, MethodName};
use crate::project::Project;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule, TirStmt,
    TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

/// WASI effects that can be used in Wado programs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WasiEffect {
    Stdout,
    Stderr,
    Environment,
    MonotonicClock,
    Exit,
}

impl WasiEffect {
    /// Parse effect name from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Stdout" => Some(Self::Stdout),
            "Stderr" => Some(Self::Stderr),
            "Environment" => Some(Self::Environment),
            "MonotonicClock" => Some(Self::MonotonicClock),
            "Exit" => Some(Self::Exit),
            _ => None,
        }
    }

    /// Get the effect name as a string (for WASI interface names)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdout => "Stdout",
            Self::Stderr => "Stderr",
            Self::Environment => "Environment",
            Self::MonotonicClock => "MonotonicClock",
            Self::Exit => "Exit",
        }
    }

    /// Standard effects (all except Exit which requires explicit usage)
    pub const STANDARD: &'static [WasiEffect] = &[
        WasiEffect::Stdout,
        WasiEffect::Stderr,
        WasiEffect::Environment,
        WasiEffect::MonotonicClock,
    ];
}

/// Canonical builtin functions imported from wasi or env namespace
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanonBuiltin {
    // Stream intrinsics (wasi namespace)
    StreamNew,
    StreamWrite,
    StreamDropWritable,
    StreamDropReadable,
    // Async/task intrinsics (wasi namespace)
    TaskReturn,
    WaitableSetNew,
    WaitableJoin,
    WaitableSetWait,
    SubtaskDrop,
    // Env intrinsics (env namespace)
    Realloc,
    F64ToBuffer,
    F32ToBuffer,
}

impl CanonBuiltin {
    /// Parse canonical name from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "stream-new" => Some(Self::StreamNew),
            "stream-write" => Some(Self::StreamWrite),
            "stream-drop-writable" => Some(Self::StreamDropWritable),
            "stream-drop-readable" => Some(Self::StreamDropReadable),
            "task-return" => Some(Self::TaskReturn),
            "waitable-set-new" => Some(Self::WaitableSetNew),
            "waitable-join" => Some(Self::WaitableJoin),
            "waitable-set-wait" => Some(Self::WaitableSetWait),
            "subtask-drop" => Some(Self::SubtaskDrop),
            "realloc" => Some(Self::Realloc),
            "f64_to_buffer" => Some(Self::F64ToBuffer),
            "f32_to_buffer" => Some(Self::F32ToBuffer),
            _ => None,
        }
    }

    /// Get the canonical name (for wasm imports)
    pub fn canonical_name(&self) -> &'static str {
        match self {
            Self::StreamNew => "stream-new",
            Self::StreamWrite => "stream-write",
            Self::StreamDropWritable => "stream-drop-writable",
            Self::StreamDropReadable => "stream-drop-readable",
            Self::TaskReturn => "task-return",
            Self::WaitableSetNew => "waitable-set-new",
            Self::WaitableJoin => "waitable-join",
            Self::WaitableSetWait => "waitable-set-wait",
            Self::SubtaskDrop => "subtask-drop",
            Self::Realloc => "realloc",
            Self::F64ToBuffer => "f64_to_buffer",
            Self::F32ToBuffer => "f32_to_buffer",
        }
    }

    /// Check if this is a float-to-string conversion builtin
    pub fn is_float_to_string(&self) -> bool {
        matches!(self, Self::F64ToBuffer | Self::F32ToBuffer)
    }

    /// All importable builtins
    pub const ALL: &'static [CanonBuiltin] = &[
        CanonBuiltin::StreamNew,
        CanonBuiltin::StreamWrite,
        CanonBuiltin::StreamDropWritable,
        CanonBuiltin::StreamDropReadable,
        CanonBuiltin::TaskReturn,
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
        CanonBuiltin::Realloc,
        CanonBuiltin::F64ToBuffer,
        CanonBuiltin::F32ToBuffer,
    ];

    /// Async/task-related builtins
    pub const ASYNC: &'static [CanonBuiltin] = &[
        CanonBuiltin::TaskReturn,
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
    ];

    /// Waitable-set builtins (only needed when effect_wait is called)
    pub const WAITABLE_SET: &'static [CanonBuiltin] = &[
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
    ];
}

/// Call graph: function ID -> set of called function IDs
type CallGraph = HashMap<FunctionId, HashSet<FunctionId>>;

/// Effect usage: function ID -> set of (effect_name, operation_name) pairs
type EffectUsageMap = HashMap<FunctionId, HashSet<(String, String)>>;

/// Optimization level for the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimizations. Used for debugging.
    #[default]
    None,
    /// Baseline optimizations including DCE. Intended for development.
    Basic,
    /// All optimizations including inlining, decomposition, etc. (TBD).
    /// Intended for production (server-side).
    Full,
    /// Full optimizations plus name section stripping. Intended for frontend.
    Size,
}

// =============================================================================
// Function Inlining
// =============================================================================

/// Maximum statement count for inline-eligible functions
const INLINE_THRESHOLD: usize = 20;

/// Count statements in a TIR block (recursive)
fn count_stmts(block: &TirBlock) -> usize {
    block
        .stmts
        .iter()
        .map(|s| match &s.kind {
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => 1 + count_stmts(then_block) + else_block.as_ref().map_or(0, count_stmts),
            TirStmtKind::While { body, .. }
            | TirStmtKind::Loop { body }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. } => 1 + count_stmts(body),
            _ => 1,
        })
        .sum()
}

/// Check if a function is eligible for inlining
fn is_inline_eligible(
    func: &TirFunction,
    recursive_functions: &HashSet<String>,
    module_path: &[String],
    type_table: &TypeTable,
) -> bool {
    // Must have a body
    let Some(body) = &func.body else {
        return false;
    };

    // Don't inline core library functions (they may be called by codegen or have
    // complex type dependencies across modules)
    if !module_path.is_empty() && module_path[0] == "core" {
        return false;
    }

    // No effects (pure functions only)
    if !func.effects.is_empty() {
        return false;
    }

    // Not generic (for now - generic inlining is complex)
    if !func.type_params.is_empty() || !func.impl_type_params.is_empty() {
        return false;
    }

    // Not a monomorphized generic function
    // These have complex type relationships that are difficult to inline correctly
    if func.monomorph_info.is_some() {
        return false;
    }

    // Not recursive
    if recursive_functions.contains(&func.name) {
        return false;
    }

    // Only inline functions with a single return at the end
    // Functions with early returns (inside if/while) are too complex to inline
    if has_early_return(body) {
        return false;
    }

    // Don't inline functions with reference parameters
    // Reference handling during inlining is complex (address-taken locals, box structs, etc.)
    for param in &func.params {
        match type_table.get(param.type_id) {
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) => return false,
            _ => {}
        }
    }

    // Don't inline functions that return references
    match type_table.get(func.return_type) {
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => return false,
        _ => {}
    }

    // Small enough
    count_stmts(body) < INLINE_THRESHOLD
}

/// Check if a block has early returns (returns inside if/while blocks)
fn has_early_return(block: &TirBlock) -> bool {
    for (i, stmt) in block.stmts.iter().enumerate() {
        let is_last = i == block.stmts.len() - 1;
        match &stmt.kind {
            TirStmtKind::Return { .. } => {
                // Return is only OK if it's the last statement
                if !is_last {
                    return true;
                }
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                // Check if there are returns inside if blocks
                if block_has_return(then_block) {
                    return true;
                }
                if let Some(else_blk) = else_block
                    && block_has_return(else_blk)
                {
                    return true;
                }
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::Loop { body }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. } => {
                if block_has_return(body) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Check if a block contains any return statement
fn block_has_return(block: &TirBlock) -> bool {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Return { .. } => return true,
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                if block_has_return(then_block) {
                    return true;
                }
                if let Some(else_blk) = else_block
                    && block_has_return(else_blk)
                {
                    return true;
                }
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::Loop { body }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. } => {
                if block_has_return(body) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Detect recursive functions using call graph analysis
fn find_recursive_functions(modules: &IndexMap<Vec<String>, TirModule>) -> HashSet<String> {
    let mut recursive = HashSet::new();

    // Build a simple call graph: function name -> called function names
    let mut call_graph: HashMap<String, HashSet<String>> = HashMap::new();

    for module in modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            let callees = collect_callees_from_function(&func);
            call_graph.insert(func.name.clone(), callees);
        }
    }

    // Find functions that can reach themselves
    for func_name in call_graph.keys() {
        if can_reach(&call_graph, func_name, func_name, &mut HashSet::new()) {
            recursive.insert(func_name.clone());
        }
    }

    recursive
}

/// Collect all function names called from a function
fn collect_callees_from_function(func: &TirFunction) -> HashSet<String> {
    let mut callees = HashSet::new();
    if let Some(body) = &func.body {
        collect_callees_from_block(body, &mut callees);
    }
    callees
}

fn collect_callees_from_block(block: &TirBlock, callees: &mut HashSet<String>) {
    for stmt in &block.stmts {
        collect_callees_from_stmt(stmt, callees);
    }
}

fn collect_callees_from_stmt(stmt: &TirStmt, callees: &mut HashSet<String>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            collect_callees_from_expr(value, callees);
        }
        TirStmtKind::Return { value } => {
            if let Some(expr) = value {
                collect_callees_from_expr(expr, callees);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_callees_from_expr(condition, callees);
            collect_callees_from_block(then_block, callees);
            if let Some(else_blk) = else_block {
                collect_callees_from_block(else_blk, callees);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_callees_from_expr(condition, callees);
            collect_callees_from_block(body, callees);
        }
        TirStmtKind::For {
            condition,
            body,
            update,
        } => {
            if let Some(cond) = condition {
                collect_callees_from_expr(cond, callees);
            }
            collect_callees_from_block(body, callees);
            if let Some(upd) = update {
                collect_callees_from_expr(upd, callees);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::ForOf { body, .. } => {
            collect_callees_from_block(body, callees);
        }
        TirStmtKind::Assert {
            condition, message, ..
        } => {
            collect_callees_from_expr(condition, callees);
            if let Some(msg) = message {
                collect_callees_from_expr(msg, callees);
            }
        }
        TirStmtKind::Break | TirStmtKind::Continue => {}
    }
}

fn collect_callees_from_expr(expr: &TirExpr, callees: &mut HashSet<String>) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            callees.insert(func.name());
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_callees_from_expr(receiver, callees);
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::StaticCall { func, args } => {
            callees.insert(func.name());
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_callees_from_expr(left, callees);
            collect_callees_from_expr(right, callees);
        }
        TirExprKind::Unary { expr, .. } => {
            collect_callees_from_expr(expr, callees);
        }
        TirExprKind::Assign { target, value } => {
            collect_callees_from_expr(target, callees);
            collect_callees_from_expr(value, callees);
        }
        TirExprKind::Cast { expr, .. } => {
            collect_callees_from_expr(expr, callees);
        }
        TirExprKind::FieldAccess { expr, .. } => {
            collect_callees_from_expr(expr, callees);
        }
        TirExprKind::Index { expr, index } => {
            collect_callees_from_expr(expr, callees);
            collect_callees_from_expr(index, callees);
        }
        TirExprKind::Block(block) => {
            collect_callees_from_block(block, callees);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_callees_from_expr(condition, callees);
            collect_callees_from_block(then_branch, callees);
            if let Some(else_blk) = else_branch {
                collect_callees_from_block(else_blk, callees);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_callees_from_expr(&field.value, callees);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_callees_from_expr(elem, callees);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_callees_from_expr(body, callees);
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_callees_from_expr(callee, callees);
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_callees_from_expr(expr, callees);
            for arm in arms {
                collect_callees_from_expr(&arm.body, callees);
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Check if `start` can reach `target` in the call graph
fn can_reach(
    call_graph: &HashMap<String, HashSet<String>>,
    start: &str,
    target: &str,
    visited: &mut HashSet<String>,
) -> bool {
    if !visited.insert(start.to_string()) {
        return false; // Already visited
    }

    if let Some(callees) = call_graph.get(start) {
        for callee in callees {
            if callee == target {
                return true;
            }
            if can_reach(call_graph, callee, target, visited) {
                return true;
            }
        }
    }

    false
}

/// Inline eligible functions in the project (TIR pass)
fn inline_functions(project: &mut Project) {
    let recursive_functions = find_recursive_functions(&project.tir_modules);

    // Collect inline candidates from all modules
    // Key: (module_path, func_name), Value: cloned function
    let mut inline_candidates: HashMap<(Vec<String>, String), TirFunction> = HashMap::new();

    // Also collect function_strings for each candidate (to update caller's strings after inlining)
    let mut candidate_strings: HashMap<(Vec<String>, String), Vec<String>> = HashMap::new();

    for (module_path, module) in &project.tir_modules {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if is_inline_eligible(
                &func,
                &recursive_functions,
                module_path,
                &module.type_table.borrow(),
            ) {
                inline_candidates.insert((module_path.clone(), func.name.clone()), func.clone());
                // Get the strings used by this function
                if let Some(strings) = module.function_strings.get(&func.name) {
                    candidate_strings
                        .insert((module_path.clone(), func.name.clone()), strings.clone());
                }
            }
        }
    }

    if inline_candidates.is_empty() {
        return;
    }

    // Inline at call sites in each module
    for module in project.tir_modules.values_mut() {
        let module_path = module.path.clone();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            let func_name = func.name.clone();
            if let Some(mut body) = func.body.take() {
                // Track which functions were inlined into this function
                let mut inlined_funcs: Vec<(Vec<String>, String)> = Vec::new();
                // Take ownership of local_count and local_types to avoid borrow conflicts
                let mut local_count = func.local_count;
                let mut local_types = std::mem::take(&mut func.local_types);
                inline_calls_in_block(
                    &mut body,
                    &inline_candidates,
                    &module_path,
                    &mut local_count,
                    &mut local_types,
                    &module.type_table.borrow(),
                    &mut inlined_funcs,
                );
                func.local_count = local_count;
                func.local_types = local_types;
                func.body = Some(body);

                // Update function_strings: add strings from inlined functions to the caller
                for inlined_key in inlined_funcs {
                    if let Some(inlined_strings) = candidate_strings.get(&inlined_key) {
                        let caller_strings = module
                            .function_strings
                            .entry(func_name.clone())
                            .or_default();
                        for s in inlined_strings {
                            if !caller_strings.contains(s) {
                                caller_strings.push(s.clone());
                            }
                            // Also ensure the string is in the module's string_literals
                            if !module.string_literals.contains(s) {
                                module.string_literals.push(s.clone());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Inline function calls in a block
fn inline_calls_in_block(
    block: &mut TirBlock,
    candidates: &HashMap<(Vec<String>, String), TirFunction>,
    current_module: &[String],
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(Vec<String>, String)>,
) {
    let mut new_stmts = Vec::new();

    for stmt in std::mem::take(&mut block.stmts) {
        match stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
            } => {
                // Try to inline the value expression if it's a call
                if let Some((inlined_stmts, final_expr, inlined_key)) = try_inline_call_expr(
                    &value,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                ) {
                    // Track the inlined function
                    if !inlined_funcs.contains(&inlined_key) {
                        inlined_funcs.push(inlined_key);
                    }
                    // Add the inlined statements
                    new_stmts.extend(inlined_stmts);
                    // Create the let with the final expression
                    new_stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index,
                            is_mut,
                            is_reactive,
                            type_id,
                            value: final_expr,
                        },
                        stmt.span,
                    ));
                } else {
                    // Recursively process nested calls in value
                    let mut new_value = value;
                    inline_calls_in_expr(
                        &mut new_value,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                    );
                    new_stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index,
                            is_mut,
                            is_reactive,
                            type_id,
                            value: new_value,
                        },
                        stmt.span,
                    ));
                }
            }
            TirStmtKind::Expr(expr) => {
                // Try to inline the expression if it's a call
                if let Some((inlined_stmts, final_expr, inlined_key)) = try_inline_call_expr(
                    &expr,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                ) {
                    if !inlined_funcs.contains(&inlined_key) {
                        inlined_funcs.push(inlined_key);
                    }
                    new_stmts.extend(inlined_stmts);
                    new_stmts.push(TirStmt::new(TirStmtKind::Expr(final_expr), stmt.span));
                } else {
                    let mut new_expr = expr;
                    inline_calls_in_expr(
                        &mut new_expr,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                    );
                    new_stmts.push(TirStmt::new(TirStmtKind::Expr(new_expr), stmt.span));
                }
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    if let Some((inlined_stmts, final_expr, inlined_key)) = try_inline_call_expr(
                        &expr,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                    ) {
                        if !inlined_funcs.contains(&inlined_key) {
                            inlined_funcs.push(inlined_key);
                        }
                        new_stmts.extend(inlined_stmts);
                        new_stmts.push(TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(final_expr),
                            },
                            stmt.span,
                        ));
                    } else {
                        let mut new_expr = expr;
                        inline_calls_in_expr(
                            &mut new_expr,
                            candidates,
                            current_module,
                            local_count,
                            local_types,
                            type_table,
                            &mut new_stmts,
                            inlined_funcs,
                        );
                        new_stmts.push(TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(new_expr),
                            },
                            stmt.span,
                        ));
                    }
                } else {
                    new_stmts.push(TirStmt::new(TirStmtKind::Return { value: None }, stmt.span));
                }
            }
            TirStmtKind::If {
                mut condition,
                mut then_block,
                else_block,
            } => {
                inline_calls_in_expr(
                    &mut condition,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    &mut new_stmts,
                    inlined_funcs,
                );
                inline_calls_in_block(
                    &mut then_block,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                );
                let new_else = else_block.map(|mut eb| {
                    inline_calls_in_block(
                        &mut eb,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        inlined_funcs,
                    );
                    eb
                });
                new_stmts.push(TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block: new_else,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::While {
                mut condition,
                mut body,
            } => {
                inline_calls_in_expr(
                    &mut condition,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    &mut new_stmts,
                    inlined_funcs,
                );
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                );
                new_stmts.push(TirStmt::new(
                    TirStmtKind::While { condition, body },
                    stmt.span,
                ));
            }
            TirStmtKind::For {
                condition,
                mut body,
                update,
            } => {
                let new_condition = condition.map(|mut c| {
                    inline_calls_in_expr(
                        &mut c,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                    );
                    c
                });
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                );
                let new_update = update.map(|mut u| {
                    inline_calls_in_expr(
                        &mut u,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                    );
                    u
                });
                new_stmts.push(TirStmt::new(
                    TirStmtKind::For {
                        condition: new_condition,
                        body,
                        update: new_update,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Loop { mut body } => {
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                );
                new_stmts.push(TirStmt::new(TirStmtKind::Loop { body }, stmt.span));
            }
            TirStmtKind::ForOf {
                binding_local,
                binding_type,
                is_mut,
                mut iterable,
                iterable_type,
                mut body,
            } => {
                inline_calls_in_expr(
                    &mut iterable,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    &mut new_stmts,
                    inlined_funcs,
                );
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                );
                new_stmts.push(TirStmt::new(
                    TirStmtKind::ForOf {
                        binding_local,
                        binding_type,
                        is_mut,
                        iterable,
                        iterable_type,
                        body,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Assert {
                mut condition,
                condition_source,
                message,
                intermediates,
            } => {
                inline_calls_in_expr(
                    &mut condition,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    &mut new_stmts,
                    inlined_funcs,
                );
                new_stmts.push(TirStmt::new(
                    TirStmtKind::Assert {
                        condition,
                        condition_source,
                        message,
                        intermediates,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Break | TirStmtKind::Continue => {
                new_stmts.push(stmt);
            }
        }
    }

    block.stmts = new_stmts;
}

/// Try to inline a call expression, returning the inlined statements, final expression,
/// and the key of the inlined function (for tracking string literals)
fn try_inline_call_expr(
    expr: &TirExpr,
    candidates: &HashMap<(Vec<String>, String), TirFunction>,
    current_module: &[String],
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    _type_table: &TypeTable,
) -> Option<(Vec<TirStmt>, TirExpr, (Vec<String>, String))> {
    let TirExprKind::Call {
        func,
        args,
        type_args,
    } = &expr.kind
    else {
        return None;
    };

    let module_path = func.module_path();
    let func_name = func.name();

    // Skip generic calls
    if !type_args.is_empty() {
        return None;
    }

    // Resolve the target module
    let target_module = if module_path.is_empty() {
        current_module.to_vec()
    } else {
        module_path.clone()
    };

    // Only inline functions from the same module
    // Cross-module inlining requires TypeId translation which is complex
    if target_module != current_module {
        return None;
    }

    // Look up the candidate
    let candidate = candidates.get(&(target_module.clone(), func_name.clone()))?;

    // Get the function body
    let body = candidate.body.as_ref()?;

    // Calculate local index offset for remapping
    let local_offset = *local_count;

    // Add space for the callee's locals (excluding parameters)
    let callee_param_count = candidate.params.len() as u32;
    let callee_local_count = candidate.local_count;
    let new_locals_needed = callee_local_count.saturating_sub(callee_param_count);

    // Extend local_types for the new locals
    for i in callee_param_count..callee_local_count {
        if let Some(&type_id) = candidate.local_types.get(i as usize) {
            local_types.push(type_id);
        }
    }
    *local_count += new_locals_needed;

    // Create argument bindings as let statements
    let mut inlined_stmts = Vec::new();
    let mut param_to_local: HashMap<u32, u32> = HashMap::new();

    for (i, (param, arg)) in candidate.params.iter().zip(args.iter()).enumerate() {
        let new_local_index = local_offset + i as u32;
        param_to_local.insert(param.local_index, new_local_index);

        // Extend local_types for parameter
        local_types.push(param.type_id);
        *local_count += 1;

        inlined_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: format!("_inline_{}", param.name),
                local_index: new_local_index,
                is_mut: false, // Parameters are immutable
                is_reactive: false,
                type_id: param.type_id,
                value: arg.clone(),
            },
            expr.span,
        ));
    }

    // Remap and inline the body statements
    let param_offset = local_offset + candidate.params.len() as u32;
    let (remapped_stmts, final_value) =
        remap_and_extract_return(body, &param_to_local, param_offset, callee_param_count);

    inlined_stmts.extend(remapped_stmts);

    // The final expression is either the return value or unit
    let final_expr =
        final_value.unwrap_or_else(|| TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span));

    // Return the inlined function key for string literal tracking
    let inlined_key = (target_module, func_name.clone());
    Some((inlined_stmts, final_expr, inlined_key))
}

/// Remap local indices and extract the return value from a block
fn remap_and_extract_return(
    block: &TirBlock,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> (Vec<TirStmt>, Option<TirExpr>) {
    let mut stmts = Vec::new();
    let mut return_value = None;

    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Return { value } => {
                // The return value becomes the final expression
                return_value = value
                    .as_ref()
                    .map(|v| remap_expr(v, param_to_local, local_offset, param_count));
                // Don't add the return statement to the inlined code
            }
            _ => {
                stmts.push(remap_stmt(stmt, param_to_local, local_offset, param_count));
            }
        }
    }

    (stmts, return_value)
}

/// Remap local indices in a statement
fn remap_stmt(
    stmt: &TirStmt,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> TirStmt {
    let kind = match &stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
        } => {
            let new_index =
                remap_local_index(*local_index, param_to_local, local_offset, param_count);
            TirStmtKind::Let {
                name: name.clone(),
                local_index: new_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: remap_expr(value, param_to_local, local_offset, param_count),
            }
        }
        TirStmtKind::Expr(expr) => {
            TirStmtKind::Expr(remap_expr(expr, param_to_local, local_offset, param_count))
        }
        TirStmtKind::Return { value } => TirStmtKind::Return {
            value: value
                .as_ref()
                .map(|v| remap_expr(v, param_to_local, local_offset, param_count)),
        },
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => TirStmtKind::If {
            condition: remap_expr(condition, param_to_local, local_offset, param_count),
            then_block: remap_block(then_block, param_to_local, local_offset, param_count),
            else_block: else_block
                .as_ref()
                .map(|b| remap_block(b, param_to_local, local_offset, param_count)),
        },
        TirStmtKind::While { condition, body } => TirStmtKind::While {
            condition: remap_expr(condition, param_to_local, local_offset, param_count),
            body: remap_block(body, param_to_local, local_offset, param_count),
        },
        TirStmtKind::For {
            condition,
            body,
            update,
        } => TirStmtKind::For {
            condition: condition
                .as_ref()
                .map(|c| remap_expr(c, param_to_local, local_offset, param_count)),
            body: remap_block(body, param_to_local, local_offset, param_count),
            update: update
                .as_ref()
                .map(|u| remap_expr(u, param_to_local, local_offset, param_count)),
        },
        TirStmtKind::Loop { body } => TirStmtKind::Loop {
            body: remap_block(body, param_to_local, local_offset, param_count),
        },
        TirStmtKind::ForOf {
            binding_local,
            binding_type,
            is_mut,
            iterable,
            iterable_type,
            body,
        } => TirStmtKind::ForOf {
            binding_local: remap_local_index(
                *binding_local,
                param_to_local,
                local_offset,
                param_count,
            ),
            binding_type: *binding_type,
            is_mut: *is_mut,
            iterable: remap_expr(iterable, param_to_local, local_offset, param_count),
            iterable_type: *iterable_type,
            body: remap_block(body, param_to_local, local_offset, param_count),
        },
        TirStmtKind::Assert {
            condition,
            condition_source,
            message,
            intermediates,
        } => TirStmtKind::Assert {
            condition: remap_expr(condition, param_to_local, local_offset, param_count),
            condition_source: condition_source.clone(),
            message: message
                .as_ref()
                .map(|m| remap_expr(m, param_to_local, local_offset, param_count)),
            intermediates: intermediates
                .iter()
                .map(|(name, expr, type_id)| {
                    (
                        name.clone(),
                        remap_expr(expr, param_to_local, local_offset, param_count),
                        *type_id,
                    )
                })
                .collect(),
        },
        TirStmtKind::Break => TirStmtKind::Break,
        TirStmtKind::Continue => TirStmtKind::Continue,
    };

    TirStmt::new(kind, stmt.span)
}

/// Remap local indices in a block
fn remap_block(
    block: &TirBlock,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> TirBlock {
    TirBlock::new(
        block
            .stmts
            .iter()
            .map(|s| remap_stmt(s, param_to_local, local_offset, param_count))
            .collect(),
        block.span,
    )
}

/// Remap a local index
fn remap_local_index(
    index: u32,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> u32 {
    // If it's a parameter, use the param_to_local mapping
    if let Some(&new_index) = param_to_local.get(&index) {
        return new_index;
    }
    // Otherwise, offset the non-parameter locals
    if index >= param_count {
        local_offset + (index - param_count)
    } else {
        // This shouldn't happen if param_to_local is complete
        index
    }
}

/// Remap local indices in an expression
fn remap_expr(
    expr: &TirExpr,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> TirExpr {
    let kind = match &expr.kind {
        TirExprKind::Local { index, name } => {
            let new_index = remap_local_index(*index, param_to_local, local_offset, param_count);
            TirExprKind::Local {
                index: new_index,
                name: name.clone(),
            }
        }
        TirExprKind::Binary { left, op, right } => TirExprKind::Binary {
            left: Box::new(remap_expr(left, param_to_local, local_offset, param_count)),
            op: *op,
            right: Box::new(remap_expr(right, param_to_local, local_offset, param_count)),
        },
        TirExprKind::Unary { op, expr: inner } => TirExprKind::Unary {
            op: *op,
            expr: Box::new(remap_expr(inner, param_to_local, local_offset, param_count)),
        },
        TirExprKind::Assign { target, value } => TirExprKind::Assign {
            target: Box::new(remap_expr(
                target,
                param_to_local,
                local_offset,
                param_count,
            )),
            value: Box::new(remap_expr(value, param_to_local, local_offset, param_count)),
        },
        TirExprKind::Cast {
            expr: inner,
            target_type,
        } => TirExprKind::Cast {
            expr: Box::new(remap_expr(inner, param_to_local, local_offset, param_count)),
            target_type: *target_type,
        },
        TirExprKind::Call {
            func,
            type_args,
            args,
        } => TirExprKind::Call {
            func: func.clone(),
            type_args: type_args.clone(),
            args: args
                .iter()
                .map(|a| remap_expr(a, param_to_local, local_offset, param_count))
                .collect(),
        },
        TirExprKind::MethodCall {
            receiver,
            func,
            type_args,
            args,
        } => TirExprKind::MethodCall {
            receiver: Box::new(remap_expr(
                receiver,
                param_to_local,
                local_offset,
                param_count,
            )),
            func: func.clone(),
            type_args: type_args.clone(),
            args: args
                .iter()
                .map(|a| remap_expr(a, param_to_local, local_offset, param_count))
                .collect(),
        },
        TirExprKind::StaticCall { func, args } => TirExprKind::StaticCall {
            func: func.clone(),
            args: args
                .iter()
                .map(|a| remap_expr(a, param_to_local, local_offset, param_count))
                .collect(),
        },
        TirExprKind::EffectCall {
            effect_name,
            op_name,
            args,
        } => TirExprKind::EffectCall {
            effect_name: effect_name.clone(),
            op_name: op_name.clone(),
            args: args
                .iter()
                .map(|a| remap_expr(a, param_to_local, local_offset, param_count))
                .collect(),
        },
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => TirExprKind::FieldAccess {
            expr: Box::new(remap_expr(inner, param_to_local, local_offset, param_count)),
            field_index: *field_index,
            field_name: field_name.clone(),
        },
        TirExprKind::Index { expr: inner, index } => TirExprKind::Index {
            expr: Box::new(remap_expr(inner, param_to_local, local_offset, param_count)),
            index: Box::new(remap_expr(index, param_to_local, local_offset, param_count)),
        },
        TirExprKind::Block(block) => TirExprKind::Block(remap_block(
            block,
            param_to_local,
            local_offset,
            param_count,
        )),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => TirExprKind::If {
            condition: Box::new(remap_expr(
                condition,
                param_to_local,
                local_offset,
                param_count,
            )),
            then_branch: remap_block(then_branch, param_to_local, local_offset, param_count),
            else_branch: else_branch
                .as_ref()
                .map(|b| remap_block(b, param_to_local, local_offset, param_count)),
        },
        TirExprKind::Match { expr: inner, arms } => TirExprKind::Match {
            expr: Box::new(remap_expr(inner, param_to_local, local_offset, param_count)),
            arms: arms
                .iter()
                .map(|arm| crate::tir::TirMatchArm {
                    pattern: arm.pattern.clone(), // Patterns don't contain locals in the same sense
                    body: remap_expr(&arm.body, param_to_local, local_offset, param_count),
                    span: arm.span,
                })
                .collect(),
        },
        TirExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } => TirExprKind::StructLiteral {
            struct_type: *struct_type,
            struct_name: struct_name.clone(),
            fields: fields
                .iter()
                .map(|f| crate::tir::TirStructField {
                    name: f.name.clone(),
                    value: remap_expr(&f.value, param_to_local, local_offset, param_count),
                    field_index: f.field_index,
                })
                .collect(),
        },
        TirExprKind::ArrayLiteral { elements } => TirExprKind::ArrayLiteral {
            elements: elements
                .iter()
                .map(|e| remap_expr(e, param_to_local, local_offset, param_count))
                .collect(),
        },
        TirExprKind::TupleLiteral { elements } => TirExprKind::TupleLiteral {
            elements: elements
                .iter()
                .map(|e| remap_expr(e, param_to_local, local_offset, param_count))
                .collect(),
        },
        TirExprKind::Closure {
            params,
            body,
            captures,
        } => TirExprKind::Closure {
            params: params.clone(),
            body: Box::new(remap_expr(body, param_to_local, local_offset, param_count)),
            captures: captures.clone(), // Captures reference outer scope, not remapped
        },
        TirExprKind::IndirectCall { callee, args } => TirExprKind::IndirectCall {
            callee: Box::new(remap_expr(
                callee,
                param_to_local,
                local_offset,
                param_count,
            )),
            args: args
                .iter()
                .map(|a| remap_expr(a, param_to_local, local_offset, param_count))
                .collect(),
        },
        // Leaf nodes - no remapping needed
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => expr.kind.clone(),
    };

    TirExpr::new(kind, expr.type_id, expr.span)
}

/// Recursively inline calls within an expression
fn inline_calls_in_expr(
    expr: &mut TirExpr,
    candidates: &HashMap<(Vec<String>, String), TirFunction>,
    current_module: &[String],
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    pre_stmts: &mut Vec<TirStmt>,
    inlined_funcs: &mut Vec<(Vec<String>, String)>,
) {
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            inline_calls_in_expr(
                left,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
            inline_calls_in_expr(
                right,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
        }
        TirExprKind::Unary { expr: inner, .. } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
        }
        TirExprKind::Assign { target, value } => {
            inline_calls_in_expr(
                target,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
            inline_calls_in_expr(
                value,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
        }
        TirExprKind::Cast { expr: inner, .. } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                );
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            inline_calls_in_expr(
                receiver,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                );
            }
        }
        TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                );
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                );
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
        }
        TirExprKind::Index { expr: inner, index } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
            inline_calls_in_expr(
                index,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                inline_calls_in_expr(
                    &mut field.value,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                );
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                inline_calls_in_expr(
                    elem,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                );
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            inline_calls_in_expr(
                callee,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
            );
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                );
            }
        }
        // For block/if/match expressions, we don't inline recursively here
        // as they would need proper block handling
        _ => {}
    }
}

// =============================================================================
// Strength Reduction (placeholder for future enhancement)
// =============================================================================

// Strength reduction optimization transforms patterns like:
//   for i in 0..n { let val = base + i * step; }
// to:
//   let mut acc = base; for i in 0..n { let val = acc; acc += step; }
//
// This eliminates the multiplication inside the loop.
// Currently not implemented - requires complex loop analysis.
// Potential patterns to target:
// - `base + counter * step` where counter increments by 1
// - Nested loops with induction variables

/// Standard WASI functions for each effect (for O0 mode)
pub const STANDARD_WASI_FUNCTIONS: &[&str] = &[
    "Stdout::write_via_stream",
    "Stderr::write_via_stream",
    "Environment::get_arguments",
    "Environment::get_environment",
    "Environment::get_initial_cwd",
    "MonotonicClock::now",
    "MonotonicClock::get_resolution",
    "MonotonicClock::wait_until",
    "MonotonicClock::wait_for",
];

// =============================================================================
// Dead Code Elimination (DCE)
// =============================================================================

/// Analysis results for a single function
#[derive(Debug, Clone, Default)]
struct FunctionAnalysis {
    /// Functions called by this function
    callees: HashSet<FunctionId>,
    /// Effect calls: (effect_name, op_name)
    effect_calls: HashSet<(String, String)>,
    /// Primitive types that need box types (for references like &i32, &mut f64)
    used_box_primitives: HashSet<PrimitiveType>,
}

/// Analyze the project and populate its usage fields with DCE analysis results.
///
/// This performs dead code elimination analysis starting from the entry point
/// and populates the project's `reachable_functions`, `used_effects`,
/// `used_builtins`, etc. fields.
fn analyze_project(project: &mut Project) {
    // Build call graph, effect usage, and box primitives from all modules
    let (call_graph, effect_usage, box_primitives_map) = build_analysis_graph(&project.tir_modules);

    // Find entry function (run in entry module)
    let entry_func = FunctionId::Free(FreeFunctionName::from_path_and_name(
        &project.entry_path,
        "run",
    ));

    // Compute reachable functions from entry point
    let mut reachable = compute_reachable(&call_graph, &entry_func);

    // Collect used effects and box primitives from reachable functions
    let mut used_effects: HashSet<WasiEffect> = HashSet::new();
    let mut used_wasi_functions: HashSet<String> = HashSet::new();
    let mut used_box_primitives: HashSet<PrimitiveType> = HashSet::new();
    for func_id in &reachable {
        if let Some(effects) = effect_usage.get(func_id) {
            for (effect_name, op_name) in effects {
                if let Some(effect) = WasiEffect::from_str(effect_name) {
                    used_effects.insert(effect);
                }
                used_wasi_functions.insert(format!("{}::{}", effect_name, op_name));
            }
        }
        if let Some(prims) = box_primitives_map.get(func_id) {
            used_box_primitives.extend(prims.iter().copied());
        }
    }

    // Helper to check if a core/internal function is reachable
    let core_internal = |name: &str| -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["core", "internal"], name))
    };

    // Helper to check if a core/builtin function is reachable
    let core_builtin = |name: &str| -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["core", "builtin"], name))
    };

    // Derive builtin usage from reachable internal functions
    // f64_to_string/f32_to_string call the bundled f64_to_buffer/f32_to_buffer
    let needs_f64_to_buffer = reachable.contains(&core_internal("f64_to_string"));
    let needs_f32_to_buffer = reachable.contains(&core_internal("f32_to_string"));

    // Add cm_list_string_to_array and helper if Environment effect functions are used
    // This conversion function is called from codegen, not Wado code
    if used_wasi_functions.contains("Environment::get_arguments")
        || used_wasi_functions.contains("Environment::get_environment")
    {
        reachable.insert(core_internal("cm_list_string_to_array"));
        reachable.insert(core_internal("copy_string_from_linear"));
    }

    // Note: array_copy_string is tracked via call graph analysis
    // It will be included if called from reachable user code

    // Check if stream intrinsics are needed by looking for:
    // 1. Stdout/Stderr effects being used
    // 2. Any builtin stream_* functions being called (for ambient logging)
    // 3. Any builtin call_indirect_* functions (ambient effect calls)
    let is_builtin_func = |f: &FreeFunctionName| {
        // New format: module_path == ["core", "builtin"]
        (f.module_path.len() == 2 && f.module_path[0] == "core" && f.module_path[1] == "builtin")
            // Legacy format: name starts with "builtin::"
            || f.name.starts_with("builtin::")
    };
    let is_builtin_stream = |f: &FreeFunctionName| {
        if is_builtin_func(f) {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            name.starts_with("stream_")
        } else {
            false
        }
    };
    let is_builtin_call_indirect_stdout = |f: &FreeFunctionName| {
        if is_builtin_func(f) {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            name.starts_with("call_indirect_stdout")
        } else {
            false
        }
    };
    let is_builtin_call_indirect_stderr = |f: &FreeFunctionName| {
        if is_builtin_func(f) {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            name.starts_with("call_indirect_stderr")
        } else {
            false
        }
    };

    let uses_stream_builtins = reachable.iter().any(|func_id| {
        if let FunctionId::Free(f) = func_id {
            is_builtin_stream(f)
                || is_builtin_call_indirect_stdout(f)
                || is_builtin_call_indirect_stderr(f)
        } else {
            false
        }
    });

    // Also mark effects as used if indirect calls are present (for ambient logging)
    if reachable
        .iter()
        .any(|func_id| matches!(func_id, FunctionId::Free(f) if is_builtin_call_indirect_stdout(f)))
    {
        used_effects.insert(WasiEffect::Stdout);
        used_wasi_functions.insert("Stdout::write_via_stream".to_string());
    }
    if reachable
        .iter()
        .any(|func_id| matches!(func_id, FunctionId::Free(f) if is_builtin_call_indirect_stderr(f)))
    {
        used_effects.insert(WasiEffect::Stderr);
        used_wasi_functions.insert("Stderr::write_via_stream".to_string());
    }

    // Compute precise builtin usage based on reachable builtin function calls
    let mut used_builtins: HashSet<CanonBuiltin> = HashSet::new();

    // Map builtin function names to canonical CanonBuiltin variants
    for func_id in &reachable {
        if let FunctionId::Free(f) = func_id
            && is_builtin_func(f)
        {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            match name {
                "stream_new" => {
                    used_builtins.insert(CanonBuiltin::StreamNew);
                }
                "stream_write" => {
                    used_builtins.insert(CanonBuiltin::StreamWrite);
                }
                "stream_drop_writable" => {
                    used_builtins.insert(CanonBuiltin::StreamDropWritable);
                }
                "stream_drop_readable" => {
                    used_builtins.insert(CanonBuiltin::StreamDropReadable);
                }
                // Ambient logging builtins also need stream intrinsics
                n if n.starts_with("call_indirect_stdout")
                    || n.starts_with("call_indirect_stderr") =>
                {
                    used_builtins.insert(CanonBuiltin::StreamNew);
                    used_builtins.insert(CanonBuiltin::StreamWrite);
                    used_builtins.insert(CanonBuiltin::StreamDropWritable);
                }
                _ => {}
            }
        }
    }

    // realloc is always needed for memory management
    used_builtins.insert(CanonBuiltin::Realloc);

    // Float-to-string builtins if their internal wrappers are used
    if needs_f64_to_buffer {
        used_builtins.insert(CanonBuiltin::F64ToBuffer);
    }
    if needs_f32_to_buffer {
        used_builtins.insert(CanonBuiltin::F32ToBuffer);
    }

    // Effect usage requires TaskReturn for async entry point
    // But waitable-set builtins are only needed when effect_wait is actually called
    if !used_effects.is_empty() || uses_stream_builtins {
        // TaskReturn is always needed for async exports
        used_builtins.insert(CanonBuiltin::TaskReturn);

        // Waitable-set builtins only needed when effect_wait is called
        // effect_wait is used by ambient logging functions (log_stdout, log_stderr)
        // but NOT by regular println/eprintln which don't wait for completion
        if reachable.contains(&core_builtin("effect_wait")) {
            for builtin in CanonBuiltin::WAITABLE_SET {
                used_builtins.insert(*builtin);
            }
        }
    }

    // Apply results to project
    project.reachable_functions = reachable.clone();
    project.all_reachable = false;
    project.used_effects = used_effects;
    project.used_wasi_functions = used_wasi_functions;
    project.used_builtins = used_builtins;
    project.used_box_primitives = used_box_primitives;

    // Filter string literals in each module to only include strings from reachable functions
    for module in project.tir_modules.values_mut() {
        let module_path = &module.path.clone();
        let mut reachable_strings: Vec<String> = Vec::new();

        for (func_name, strings) in &module.function_strings {
            // Build function ID to check if it's reachable
            let func_id = if func_name.contains("::") {
                // Method name like "Point::sum" - use MethodName
                let parts: Vec<&str> = func_name.splitn(2, "::").collect();
                if parts.len() == 2 {
                    FunctionId::Method(MethodName::new(
                        module_path.join("/"),
                        parts[0].to_string(),
                        None,
                        parts[1].to_string(),
                    ))
                } else {
                    FunctionId::Free(FreeFunctionName::from_path_and_name(module_path, func_name))
                }
            } else {
                // Regular function
                FunctionId::Free(FreeFunctionName::from_path_and_name(module_path, func_name))
            };

            if reachable.contains(&func_id) {
                for s in strings {
                    if !reachable_strings.contains(s) {
                        reachable_strings.push(s.clone());
                    }
                }
            }
        }

        module.string_literals = reachable_strings;
    }
}

/// Populate project with all features enabled (no DCE, for O0 mode).
fn populate_all_features(project: &mut Project) {
    use PrimitiveType::*;

    project.reachable_functions = HashSet::new();
    project.all_reachable = true;
    // Standard effects (all except Exit which requires explicit usage)
    project.used_effects = WasiEffect::STANDARD.iter().copied().collect();
    // Standard WASI functions for the above effects
    project.used_wasi_functions = STANDARD_WASI_FUNCTIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    // All importable builtins when DCE is disabled
    project.used_builtins = CanonBuiltin::ALL.iter().copied().collect();
    // All primitives that map to box types when DCE is disabled
    project.used_box_primitives = HashSet::from([I32, I64, F32, F64]);
}

/// Per-function box primitives usage
type BoxPrimitivesMap = HashMap<FunctionId, HashSet<PrimitiveType>>;

/// Build call graph and effect usage from all TIR modules
/// Returns (call_graph, effect_usage, box_primitives_map)
fn build_analysis_graph(
    modules: &IndexMap<Vec<String>, TirModule>,
) -> (CallGraph, EffectUsageMap, BoxPrimitivesMap) {
    let mut call_graph: CallGraph = HashMap::new();
    let mut effect_usage: EffectUsageMap = HashMap::new();
    let mut box_primitives_map: BoxPrimitivesMap = HashMap::new();

    for (path, module) in modules {
        let type_table = &*module.type_table.borrow();

        // Analyze functions (including methods stored as functions)
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Methods have names like "Point::sum", regular functions don't contain "::"
            let func_id = if let Some(sep_pos) = func.name.find("::") {
                // This is a method - use MethodName
                let struct_name = &func.name[..sep_pos];
                let method_name = &func.name[sep_pos + 2..];
                FunctionId::Method(MethodName::new(
                    path.join("/"),
                    struct_name.to_string(),
                    None,
                    method_name.to_string(),
                ))
            } else {
                // Regular function - use FreeFunctionName
                FunctionId::Free(FreeFunctionName::from_path_and_name(path, &func.name))
            };
            let analysis = analyze_function(&func, path, type_table);
            call_graph.insert(func_id.clone(), analysis.callees);
            if !analysis.effect_calls.is_empty() {
                effect_usage.insert(func_id.clone(), analysis.effect_calls);
            }
            if !analysis.used_box_primitives.is_empty() {
                box_primitives_map.insert(func_id, analysis.used_box_primitives);
            }
        }

        // Note: impl_block.methods is empty because resolver adds methods to functions
        // with mangled names like "Point::sum". This loop is kept for future compatibility.
        for impl_block in &module.impls {
            let struct_name = match type_table.get(impl_block.target_type) {
                ResolvedType::Struct { name, .. } => name.clone(),
                _ => continue,
            };

            for method in &impl_block.methods {
                let method_id = FunctionId::Method(MethodName::new(
                    path.join("/"),
                    struct_name.clone(),
                    None,
                    method.name.clone(),
                ));
                let analysis = analyze_function(method, path, type_table);
                call_graph.insert(method_id.clone(), analysis.callees);
                if !analysis.effect_calls.is_empty() {
                    effect_usage.insert(method_id.clone(), analysis.effect_calls);
                }
                if !analysis.used_box_primitives.is_empty() {
                    box_primitives_map.insert(method_id, analysis.used_box_primitives);
                }
            }
        }
    }

    (call_graph, effect_usage, box_primitives_map)
}

/// Analyze a TIR function for callees and effect usage
fn analyze_function(
    func: &TirFunction,
    current_module: &[String],
    type_table: &TypeTable,
) -> FunctionAnalysis {
    let mut analysis = FunctionAnalysis::default();

    // Check parameters for references to primitives (e.g., &i32, &mut f64)
    for param in &func.params {
        if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
            type_table.get(param.type_id)
            && let ResolvedType::Primitive(prim) = type_table.get(*inner)
        {
            analysis.used_box_primitives.insert(*prim);
        }
    }

    if let Some(body) = &func.body {
        analyze_block(body, current_module, type_table, &mut analysis);
    }
    analysis
}

fn analyze_block(
    block: &TirBlock,
    current_module: &[String],
    type_table: &TypeTable,
    analysis: &mut FunctionAnalysis,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                analyze_expr(value, current_module, type_table, analysis);
                // Check locals for references to primitives (e.g., let r: &i32 = ...)
                if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(*type_id)
                    && let ResolvedType::Primitive(prim) = type_table.get(*inner)
                {
                    analysis.used_box_primitives.insert(*prim);
                }
            }
            TirStmtKind::Expr(expr) => {
                analyze_expr(expr, current_module, type_table, analysis);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    analyze_expr(expr, current_module, type_table, analysis);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                analyze_expr(condition, current_module, type_table, analysis);
                analyze_block(then_block, current_module, type_table, analysis);
                if let Some(else_blk) = else_block {
                    analyze_block(else_blk, current_module, type_table, analysis);
                }
            }
            TirStmtKind::While { condition, body } => {
                analyze_expr(condition, current_module, type_table, analysis);
                analyze_block(body, current_module, type_table, analysis);
            }
            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                if let Some(cond) = condition {
                    analyze_expr(cond, current_module, type_table, analysis);
                }
                analyze_block(body, current_module, type_table, analysis);
                if let Some(upd) = update {
                    analyze_expr(upd, current_module, type_table, analysis);
                }
            }
            TirStmtKind::Loop { body } => {
                analyze_block(body, current_module, type_table, analysis);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                analyze_expr(iterable, current_module, type_table, analysis);
                analyze_block(body, current_module, type_table, analysis);
            }
            TirStmtKind::Assert {
                condition,
                message,
                intermediates,
                ..
            } => {
                analyze_expr(condition, current_module, type_table, analysis);
                if let Some(msg) = message {
                    analyze_expr(msg, current_module, type_table, analysis);
                }
                for (_, expr, type_id) in intermediates {
                    analyze_expr(expr, current_module, type_table, analysis);
                    // Assert formatting calls to_string on intermediate values
                    add_to_string_callee(*type_id, type_table, analysis);
                }
                // Assert codegen uses string_concat and panic directly
                analysis
                    .callees
                    .insert(FunctionId::Free(FreeFunctionName::from_strs(
                        &["core", "internal"],
                        "string_concat",
                    )));
                analysis
                    .callees
                    .insert(FunctionId::Free(FreeFunctionName::from_strs(
                        &["core", "prelude"],
                        "panic",
                    )));
                // Assert failure prints to stderr, so we need Stderr effect
                analysis
                    .effect_calls
                    .insert(("Stderr".to_string(), "write_via_stream".to_string()));
            }
            TirStmtKind::Break | TirStmtKind::Continue => {}
        }
    }
}

fn analyze_expr(
    expr: &TirExpr,
    current_module: &[String],
    type_table: &TypeTable,
    analysis: &mut FunctionAnalysis,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let module_path = func.module_path();
            let func_name = func.name();

            // Invariant: TirExprKind::Call should never have method names (containing "::")
            // Methods use TirExprKind::MethodCall instead. The only exception is "builtin::*".
            debug_assert!(
                !func_name.contains("::") || func_name.starts_with("builtin::"),
                "TirExprKind::Call should not have method-style names: {}",
                func_name
            );

            // Build function ID for the called function
            let callee_path = if module_path.is_empty() {
                current_module
            } else {
                module_path.as_slice()
            };
            let callee_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                callee_path,
                &func_name,
            ));
            analysis.callees.insert(callee_id);

            // Detect effect calls: single-element module_path with PascalCase name
            // (e.g., ["Stdout"], ["Stderr"], ["MonotonicClock"])
            // Effect calls are represented as Call in TIR, not as EffectCall
            if module_path.len() == 1 {
                let potential_effect = &module_path[0];
                // Check if it looks like an effect name (starts with uppercase, no file path chars)
                if potential_effect
                    .chars()
                    .next()
                    .is_some_and(|c: char| c.is_ascii_uppercase())
                    && !potential_effect.contains('/')
                    && !potential_effect.contains('.')
                {
                    analysis
                        .effect_calls
                        .insert((potential_effect.clone(), func_name.clone()));
                }
            }

            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            // Extract method name from func (format is "StructName::method_name")
            let method_name = {
                let full_name = func.name();
                if let Some(pos) = full_name.rfind("::") {
                    &full_name[pos + 2..]
                } else {
                    &full_name
                }
                .to_string()
            };
            // Get receiver type to determine method target
            let receiver_type = type_table.get(receiver.type_id);
            match receiver_type {
                ResolvedType::Struct {
                    name, module_path, ..
                } => {
                    // Struct method call - use FunctionId::Method
                    let method_id = FunctionId::Method(MethodName::new(
                        module_path.join("/"),
                        name.clone(),
                        None,
                        method_name.to_string(),
                    ));
                    analysis.callees.insert(method_id);
                }
                ResolvedType::Primitive(_) => {
                    // Primitive method call (e.g., i32.to_string())
                    if method_name == "to_string" {
                        add_to_string_callee(receiver.type_id, type_table, analysis);
                    }
                    // Other primitive methods are inline (no function call)
                }
                ResolvedType::GenericInstance {
                    name,
                    type_args,
                    module_path,
                } => {
                    // Generic instance method call (e.g., Box<i32>.get())
                    // Track as a free function with monomorphized name: Box<i32>::get
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| mangle_type_for_name(*t, type_table))
                        .collect();
                    let mangled_func_name =
                        format!("{}<{}>::{}", name, type_arg_names.join(","), method_name);
                    let callee_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                        module_path,
                        &mangled_func_name,
                    ));
                    analysis.callees.insert(callee_id);
                }
                ResolvedType::BuiltinArray(elem_type) => {
                    // Array<T> method call (e.g., arr.len(), arr.append())
                    // Track as a free function with monomorphized name: Array<i32>::len
                    let elem_name = mangle_type_for_name(*elem_type, type_table);
                    let mangled_func_name = format!("Array<{}>::{}", elem_name, method_name);
                    // Array methods are in core/prelude
                    let callee_id = FunctionId::Free(FreeFunctionName::from_strs(
                        &["core", "prelude"],
                        &mangled_func_name,
                    ));
                    analysis.callees.insert(callee_id);
                }
                _ => {}
            }

            analyze_expr(receiver, current_module, type_table, analysis);
            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            analyze_expr(left, current_module, type_table, analysis);
            analyze_expr(right, current_module, type_table, analysis);
        }
        TirExprKind::Unary { op, expr } => {
            analyze_expr(expr, current_module, type_table, analysis);
            // Track box types needed for references to primitives
            if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                && let ResolvedType::Primitive(prim) = type_table.get(expr.type_id)
            {
                analysis.used_box_primitives.insert(*prim);
            }
        }
        TirExprKind::Assign { target, value } => {
            analyze_expr(target, current_module, type_table, analysis);
            analyze_expr(value, current_module, type_table, analysis);
        }
        TirExprKind::Cast { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::EffectCall {
            effect_name,
            op_name,
            args,
        } => {
            // Track effect usage for WASI import DCE
            analysis
                .effect_calls
                .insert((effect_name.clone(), op_name.clone()));

            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::StaticCall { func, args } => {
            let module_path = func.module_path();
            let func_name = func.name();
            // Static method call - func_name already contains "StructName::method_name"
            // The function is registered as a free function with mangled name
            let callee_path = if module_path.is_empty() {
                current_module
            } else {
                module_path.as_slice()
            };
            let callee_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                callee_path,
                &func_name,
            ));
            analysis.callees.insert(callee_id);

            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::FieldAccess { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::Index { expr, index } => {
            analyze_expr(expr, current_module, type_table, analysis);
            analyze_expr(index, current_module, type_table, analysis);
        }
        TirExprKind::Block(block) => {
            analyze_block(block, current_module, type_table, analysis);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_expr(condition, current_module, type_table, analysis);
            analyze_block(then_branch, current_module, type_table, analysis);
            if let Some(else_blk) = else_branch {
                analyze_block(else_blk, current_module, type_table, analysis);
            }
        }
        TirExprKind::Match { expr, arms } => {
            analyze_expr(expr, current_module, type_table, analysis);
            for arm in arms {
                analyze_expr(&arm.body, current_module, type_table, analysis);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_expr(&field.value, current_module, type_table, analysis);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                analyze_expr(elem, current_module, type_table, analysis);
            }
        }
        TirExprKind::Closure { body, .. } => {
            analyze_expr(body, current_module, type_table, analysis);
        }
        TirExprKind::IndirectCall { callee, args } => {
            analyze_expr(callee, current_module, type_table, analysis);
            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        // Leaf nodes - no calls
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Add the appropriate to_string function call for a type
fn add_to_string_callee(type_id: TypeId, type_table: &TypeTable, analysis: &mut FunctionAnalysis) {
    let core_internal: &[&str] = &["core", "internal"];
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => {
            let func_name = match prim {
                PrimitiveType::I32 | PrimitiveType::I8 | PrimitiveType::I16 => "i32_to_string",
                PrimitiveType::U32 | PrimitiveType::U8 | PrimitiveType::U16 => "u32_to_string",
                PrimitiveType::I64 => "i64_to_string",
                PrimitiveType::U64 => "u64_to_string",
                PrimitiveType::F32 => "f32_to_string",
                PrimitiveType::F64 => "f64_to_string",
                PrimitiveType::Bool => "bool_to_string",
                PrimitiveType::Char => "char_to_string",
                _ => return,
            };
            analysis
                .callees
                .insert(FunctionId::Free(FreeFunctionName::from_strs(
                    core_internal,
                    func_name,
                )));
        }
        ResolvedType::String => {
            // String.to_string() is a no-op, no function call needed
        }
        _ => {}
    }
}

/// Mangle a type ID into a string suitable for struct/function names.
/// Used for creating monomorphized function names like Array<i32>::len.
fn mangle_type_for_name(type_id: TypeId, type_table: &TypeTable) -> String {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8 => "i8".to_string(),
            PrimitiveType::I16 => "i16".to_string(),
            PrimitiveType::I32 => "i32".to_string(),
            PrimitiveType::I64 => "i64".to_string(),
            PrimitiveType::I128 => "i128".to_string(),
            PrimitiveType::U8 => "u8".to_string(),
            PrimitiveType::U16 => "u16".to_string(),
            PrimitiveType::U32 => "u32".to_string(),
            PrimitiveType::U64 => "u64".to_string(),
            PrimitiveType::U128 => "u128".to_string(),
            PrimitiveType::F32 => "f32".to_string(),
            PrimitiveType::F64 => "f64".to_string(),
            PrimitiveType::Bool => "bool".to_string(),
            PrimitiveType::Char => "char".to_string(),
        },
        ResolvedType::Unit => "unit".to_string(),
        ResolvedType::String => "String".to_string(),
        ResolvedType::Struct { name, .. } => name.clone(),
        ResolvedType::GenericInstance {
            name, type_args, ..
        } => {
            let args: Vec<String> = type_args
                .iter()
                .map(|t| mangle_type_for_name(*t, type_table))
                .collect();
            format!("{}<{}>", name, args.join(","))
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            let ret_name = mangle_type_for_name(*return_type, type_table);
            format!("Fn<{},{}>", params.len(), ret_name)
        }
        ResolvedType::Tuple(elems) => {
            let elem_names: Vec<String> = elems
                .iter()
                .map(|t| mangle_type_for_name(*t, type_table))
                .collect();
            format!("Tuple<{}>", elem_names.join(","))
        }
        ResolvedType::Option(inner) => {
            let inner_name = mangle_type_for_name(*inner, type_table);
            format!("Option<{}>", inner_name)
        }
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            mangle_type_for_name(*inner, type_table)
        }
        _ => "unknown".to_string(),
    }
}

/// Compute the set of reachable functions from an entry point
fn compute_reachable(
    call_graph: &HashMap<FunctionId, HashSet<FunctionId>>,
    entry: &FunctionId,
) -> HashSet<FunctionId> {
    let mut reachable = HashSet::new();
    let mut worklist = vec![entry.clone()];

    while let Some(func) = worklist.pop() {
        if reachable.contains(&func) {
            continue;
        }
        reachable.insert(func.clone());

        // Add all callees to worklist
        if let Some(callees) = call_graph.get(&func) {
            for callee in callees {
                if !reachable.contains(callee) {
                    worklist.push(callee.clone());
                }
            }
        }
    }

    reachable
}

// =============================================================================
// Project-level optimization
// =============================================================================

/// Remove unreachable functions from the project's TIR modules.
///
/// This physically removes functions that are not in `reachable_functions`
/// from the TIR, so codegen doesn't need to filter them.
fn remove_unreachable_functions(project: &mut Project) {
    // Skip if all functions are reachable (no DCE)
    if project.all_reachable {
        return;
    }

    for (module_path, module) in &mut project.tir_modules {
        // Retain only reachable functions
        module.functions.retain(|func_rc| {
            let func = func_rc.borrow();
            // Check if this is a method (name contains "::")
            if let Some(sep_pos) = func.name.find("::") {
                // Could be either:
                // - Instance method tracked as FunctionId::Method
                // - Static method tracked as FunctionId::Free with mangled name
                let struct_name = &func.name[..sep_pos];
                let method_name = &func.name[sep_pos + 2..];

                // Try as instance method (FunctionId::Method)
                let method_id = FunctionId::Method(MethodName::new(
                    module_path.join("/"),
                    struct_name.to_string(),
                    None,
                    method_name.to_string(),
                ));
                if project.reachable_functions.contains(&method_id) {
                    return true;
                }

                // Try as static method (FunctionId::Free with mangled name)
                let free_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                    module_path,
                    &func.name,
                ));
                if project.reachable_functions.contains(&free_id) {
                    return true;
                }

                // For generic methods/static methods, check if any monomorphized version is reachable
                // Generic functions are named "Array::with_capacity" but calls use "Array<i32>::with_capacity"
                // Check if any function ID in reachable_functions matches this base name
                is_generic_func_reachable(&project.reachable_functions, module_path, &func.name)
            } else {
                // Regular function
                let func_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                    module_path,
                    &func.name,
                ));
                project.reachable_functions.contains(&func_id)
            }
        });
    }
}

/// Check if a generic function has any monomorphized version that is reachable.
/// For example, "Array::with_capacity" should be kept if "Array<i32>::with_capacity" is reachable.
fn is_generic_func_reachable(
    reachable: &HashSet<FunctionId>,
    module_path: &[String],
    func_name: &str,
) -> bool {
    // func_name is like "Array::with_capacity"
    // We need to find any "Array<..>::with_capacity" in reachable set
    let Some(sep_pos) = func_name.find("::") else {
        return false;
    };
    let base_struct = &func_name[..sep_pos];
    let method_name = &func_name[sep_pos + 2..];

    for id in reachable {
        if let FunctionId::Free(free_name) = id {
            // For monomorphized functions, the actual function may be in a different module
            // (e.g., entry module []) than the original definition (e.g., ["core", "prelude"]).
            // So we relax the module path check for monomorphized names (containing <).
            let module_matches = free_name.module_path.as_slice() == module_path
                || (free_name.name.contains('<') && module_path.is_empty());

            if !module_matches {
                continue;
            }
            // Check if name matches pattern "BaseStruct<..>::method_name"
            if let Some(call_sep_pos) = free_name.name.find("::") {
                let call_struct = &free_name.name[..call_sep_pos];
                let call_method = &free_name.name[call_sep_pos + 2..];

                // Check if method name matches
                if call_method != method_name {
                    continue;
                }

                // Check if struct name matches (with or without generic params)
                // "Array<i32>" should match "Array"
                if call_struct == base_struct {
                    return true;
                }
                if let Some(bracket_pos) = call_struct.find('<')
                    && &call_struct[..bracket_pos] == base_struct
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Optimize a Project by analyzing and populating its usage fields.
///
/// This is the main entry point for the optimizer. Based on the optimization
/// level, it either performs DCE analysis or enables all features.
pub fn optimize(mut project: Project, opt_level: OptLevel) -> Project {
    match opt_level {
        OptLevel::None => {
            populate_all_features(&mut project);
        }
        OptLevel::Basic => {
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
        }
        OptLevel::Full => {
            inline_functions(&mut project);
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
        }
        OptLevel::Size => {
            inline_functions(&mut project);
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            project.strip_names = true;
        }
    }

    project
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_fn(name: &str) -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["test"], name))
    }

    #[test]
    fn test_empty_reachable_set() {
        let call_graph = HashMap::new();
        let entry = free_fn("run");
        let reachable = compute_reachable(&call_graph, &entry);
        assert!(reachable.contains(&free_fn("run")));
        assert_eq!(reachable.len(), 1);
    }

    #[test]
    fn test_transitive_reachability() {
        let mut call_graph = HashMap::new();
        call_graph.insert(free_fn("run"), HashSet::from([free_fn("foo")]));
        call_graph.insert(free_fn("foo"), HashSet::from([free_fn("bar")]));
        call_graph.insert(free_fn("bar"), HashSet::new());
        call_graph.insert(free_fn("unused"), HashSet::from([free_fn("bar")]));

        let reachable = compute_reachable(&call_graph, &free_fn("run"));
        assert!(reachable.contains(&free_fn("run")));
        assert!(reachable.contains(&free_fn("foo")));
        assert!(reachable.contains(&free_fn("bar")));
        assert!(!reachable.contains(&free_fn("unused")));
    }
}
