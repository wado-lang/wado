//! CM Adapter Synthesis phase.
//!
//! Generates TIR adapter functions for Component Model boundary crossing.
//! Each adapter handles lifting Wado values to CM flat ABI (lowering params)
//! and lifting CM flat ABI values back to Wado types (lifting results).
//!
//! Pipeline position: after `effect_check`, before monomorphize.
//! This ensures adapter functions go through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-15-cm-adapter-synthesis.md` for design details.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};

use crate::ast::Type;
use crate::cm_abi;
use crate::component_model::WasiFunctionInfo;
use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{
    FunctionRef, TirBlock, TirExpr, TirExprKind, TirFunction, TirParam, TirStmt, TirStmtKind,
    TypeId, TypeTable,
};
use crate::token::Span;

/// Synthetic span used for all generated adapter code.
fn synth_span() -> Span {
    Span::new(0, 0, 1, 1)
}

// ============================================================================
// TIR construction helpers
// ============================================================================

/// Create a call to a builtin function (e.g., `builtin::i32_load`).
pub fn builtin_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: crate::tir::FunctionRef::External {
                module_source: ModuleSource::core("builtin"),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create a call to an internal function (e.g., `internal::cm_lower_string`).
pub fn internal_call(name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: crate::tir::FunctionRef::External {
                module_source: ModuleSource::core("internal"),
                name: name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create an i32 literal expression.
#[allow(clippy::cast_sign_loss)]
pub fn i32_const(value: i32) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: value as u64,
            repr: value.to_string(),
        },
        TypeTable::I32,
        synth_span(),
    )
}

/// Create an i64 literal expression.
#[allow(clippy::cast_sign_loss)]
pub fn i64_const(value: i64) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: value as u64,
            repr: value.to_string(),
        },
        TypeTable::I64,
        synth_span(),
    )
}

/// Create a local variable reference.
pub fn local_ref(index: u32, name: &str, type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Local {
            index,
            name: name.to_string(),
        },
        type_id,
        synth_span(),
    )
}

/// Create a binary expression.
pub fn binary(op: crate::tir::TirBinaryOp, left: TirExpr, right: TirExpr, ty: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty,
        synth_span(),
    )
}

/// Create an i32 addition expression.
fn binary_add(left: TirExpr, right: TirExpr) -> TirExpr {
    binary(crate::tir::TirBinaryOp::Add, left, right, TypeTable::I32)
}

/// Generate a memory load for a CM primitive type at the given address.
fn load_cm_primitive(prim: &crate::component_model::CmPrimitiveType, addr: TirExpr) -> TirExpr {
    use crate::component_model::CmPrimitiveType;
    let (load_name, type_id) = match prim {
        CmPrimitiveType::I32 | CmPrimitiveType::U32 => ("i32_load", TypeTable::I32),
        CmPrimitiveType::I64 | CmPrimitiveType::U64 => ("i64_load", TypeTable::I64),
        CmPrimitiveType::F32 => ("f32_load", TypeTable::F32),
        CmPrimitiveType::F64 => ("f64_load", TypeTable::F64),
    };
    builtin_call(load_name, vec![addr], type_id)
}

/// Create a cast expression.
pub fn cast(expr: TirExpr, target_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(expr),
            target_type,
        },
        target_type,
        synth_span(),
    )
}

/// Create a let statement.
pub fn let_stmt(name: &str, local_index: u32, type_id: TypeId, value: TirExpr) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Let {
            name: name.to_string(),
            local_index,
            is_mut: false,
            is_reactive: false,
            type_id,
            value,
        },
        synth_span(),
    )
}

/// Create an expression statement.
pub fn expr_stmt(expr: TirExpr) -> TirStmt {
    TirStmt::new(TirStmtKind::Expr(expr), synth_span())
}

/// Create a return statement.
pub fn return_stmt(value: Option<TirExpr>) -> TirStmt {
    TirStmt::new(TirStmtKind::Return { value }, synth_span())
}

/// Create a `CmRawCall` expression targeting a lowered WASI import.
pub fn cm_raw_call(local_name: &str, args: Vec<TirExpr>, return_type: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::CmRawCall {
            local_name: local_name.to_string(),
            args,
        },
        return_type,
        synth_span(),
    )
}

// ============================================================================
// Lift / Lower synthesis
// ============================================================================

/// Create a mutable let statement.
pub fn let_mut_stmt(name: &str, local_index: u32, type_id: TypeId, value: TirExpr) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Let {
            name: name.to_string(),
            local_index,
            is_mut: true,
            is_reactive: false,
            type_id,
            value,
        },
        synth_span(),
    )
}

/// Create an if statement.
pub fn if_stmt(condition: TirExpr, then_block: TirBlock, else_block: Option<TirBlock>) -> TirStmt {
    TirStmt::new(
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        },
        synth_span(),
    )
}

/// Create a loop statement.
pub fn loop_stmt(body: TirBlock) -> TirStmt {
    TirStmt::new(TirStmtKind::Loop { body }, synth_span())
}

/// Create a break statement.
pub fn break_stmt() -> TirStmt {
    TirStmt::new(
        TirStmtKind::Break {
            label: None,
            value: None,
        },
        synth_span(),
    )
}

/// Create a local assignment expression.
pub fn assign(target: TirExpr, value: TirExpr) -> TirExpr {
    TirExpr::new(
        TirExprKind::Assign {
            target: Box::new(target),
            value: Box::new(value),
        },
        TypeTable::UNIT,
        synth_span(),
    )
}

/// Create a method call expression.
pub fn method_call(
    receiver: TirExpr,
    method_name: &str,
    method_module_source: ModuleSource,
    type_args: Vec<TypeId>,
    args: Vec<TirExpr>,
    return_type: TypeId,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: crate::tir::FunctionRef::External {
                module_source: method_module_source,
                name: method_name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args,
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create a static call expression (e.g., `Array::<T>::with_capacity(n)`).
pub fn static_call(
    method_name: &str,
    module_source: ModuleSource,
    args: Vec<TirExpr>,
    return_type: TypeId,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::StaticCall {
            func: crate::tir::FunctionRef::External {
                module_source,
                name: method_name.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            args,
        },
        return_type,
        synth_span(),
    )
}

/// Create an `Option::Some(value)` expression.
pub fn option_some(value: TirExpr, option_type_id: TypeId) -> TirExpr {
    TirExpr::new(
        TirExprKind::OptionSome {
            value: Box::new(value),
        },
        option_type_id,
        synth_span(),
    )
}

/// Create a `null` (`Option::None`) expression.
pub fn null_expr(type_id: TypeId) -> TirExpr {
    TirExpr::new(TirExprKind::Null, type_id, synth_span())
}

/// Create a TIR block from statements.
pub fn block(stmts: Vec<TirStmt>) -> TirBlock {
    TirBlock {
        stmts,
        span: synth_span(),
    }
}

/// Synthesize a TIR expression that loads a CM value from linear memory.
///
/// For primitives, this is a single `builtin::i32_load` (or similar).
/// For String, this reads (ptr, len) and calls `memory_to_gc_string`.
///
/// `next_local` is a counter for allocating intermediate local variables.
/// Primitives don't use it; composite types may allocate locals.
pub fn synthesize_lift(ty: &Type, addr: TirExpr, _next_local: &mut u32) -> TirExpr {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => builtin_call("i32_load", vec![addr], TypeTable::I32),
            "i64" | "u64" => builtin_call("i64_load", vec![addr], TypeTable::I64),
            "f32" => builtin_call("f32_load", vec![addr], TypeTable::F32),
            "f64" => builtin_call("f64_load", vec![addr], TypeTable::F64),
            "i8" | "u8" => builtin_call("i32_load8_u", vec![addr], TypeTable::I32),
            "i16" | "u16" => builtin_call("i32_load16_u", vec![addr], TypeTable::I32),
            "bool" => {
                let raw = builtin_call("i32_load8_u", vec![addr], TypeTable::I32);
                binary(
                    crate::tir::TirBinaryOp::NotEq,
                    raw,
                    i32_const(0),
                    TypeTable::BOOL,
                )
            }
            "char" => builtin_call("i32_load", vec![addr], TypeTable::CHAR),
            "String" => {
                let ptr = builtin_call("i32_load", vec![addr.clone()], TypeTable::I32);
                let len = builtin_call(
                    "i32_load",
                    vec![binary(
                        crate::tir::TirBinaryOp::Add,
                        addr,
                        i32_const(4),
                        TypeTable::I32,
                    )],
                    TypeTable::I32,
                );
                // Return type uses I32 as placeholder; caller must fix up
                // to the actual String TypeId from the type table.
                internal_call("memory_to_gc_string", vec![ptr, len], TypeTable::I32)
            }
            _ => panic!("synthesize_lift: unsupported type `{}`", named.name),
        },
        Type::Tuple(elems) if elems.is_empty() => {
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span())
        }
        _ => panic!("synthesize_lift: unsupported type {ty:?}"),
    }
}

/// Synthesize TIR statements that store a Wado value into linear memory.
///
/// For primitives, this is a single `builtin::i32_store` (or similar).
/// For String, calls `cm_lower_string` and stores ptr/len.
///
/// `next_local` is a counter for allocating intermediate local variables.
pub fn synthesize_lower(
    ty: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
) -> Vec<TirStmt> {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i64" | "u64" => vec![expr_stmt(builtin_call(
                "i64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f32" => vec![expr_stmt(builtin_call(
                "f32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f64" => vec![expr_stmt(builtin_call(
                "f64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i8" | "u8" => vec![expr_stmt(builtin_call(
                "i32_store8",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i16" | "u16" => vec![expr_stmt(builtin_call(
                "i32_store16",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "bool" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store8",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            "char" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            "String" => {
                // cm_lower_string(value) returns packed i64: (ptr | (len << 32))
                let packed_local = *next_local;
                *next_local += 1;
                let packed = internal_call("cm_lower_string", vec![value], TypeTable::I64);
                let mut stmts = vec![let_stmt("__packed", packed_local, TypeTable::I64, packed)];

                // Store ptr (low 32 bits) at addr
                let ptr = cast(
                    local_ref(packed_local, "__packed", TypeTable::I64),
                    TypeTable::I32,
                );
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr.clone(), ptr],
                    TypeTable::UNIT,
                )));

                // Store len (high 32 bits) at addr + 4
                let shifted = binary(
                    crate::tir::TirBinaryOp::Shr,
                    local_ref(packed_local, "__packed", TypeTable::I64),
                    i64_const(32),
                    TypeTable::I64,
                );
                let len = cast(shifted, TypeTable::I32);
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![
                        binary(
                            crate::tir::TirBinaryOp::Add,
                            addr,
                            i32_const(4),
                            TypeTable::I32,
                        ),
                        len,
                    ],
                    TypeTable::UNIT,
                )));
                stmts
            }
            _ => panic!("synthesize_lower: unsupported type `{}`", named.name),
        },
        Type::Tuple(elems) if elems.is_empty() => vec![],
        _ => panic!("synthesize_lower: unsupported type {ty:?}"),
    }
}

// ============================================================================
// Flat ABI parameter/result computation
// ============================================================================

/// Compute the flat ABI parameter types for a WASI function parameter.
pub fn flatten_param_type(ty: &Type) -> Vec<TypeId> {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" | "bool" | "char" | "i8" | "u8" | "i16" | "u16" => {
                vec![TypeTable::I32]
            }
            "i64" | "u64" => vec![TypeTable::I64],
            "f32" => vec![TypeTable::F32],
            "f64" => vec![TypeTable::F64],
            "String" => vec![TypeTable::I32, TypeTable::I32],
            _ => vec![TypeTable::I32],
        },
        Type::Generic(g) if g.name == "Stream" => vec![TypeTable::I32],
        Type::Reference(_) | Type::MutReference(_) => vec![TypeTable::I32],
        Type::Tuple(elems) if elems.is_empty() => vec![],
        _ => {
            let flat = cm_abi::cm_flat_types(ty);
            flat.iter()
                .map(|t| match t {
                    cm_abi::CmValType::I32 => TypeTable::I32,
                    cm_abi::CmValType::I64 => TypeTable::I64,
                    cm_abi::CmValType::F32 => TypeTable::F32,
                    cm_abi::CmValType::F64 => TypeTable::F64,
                })
                .collect()
        }
    }
}

// ============================================================================
// Adapter function generation
// ============================================================================

/// Adapter function name prefix.
pub const ADAPTER_PREFIX: &str = "__cm_adapter__";

/// Build the adapter function name for a WASI import.
pub fn adapter_func_name(effect_name: &str, method_name: &str) -> String {
    format!("{ADAPTER_PREFIX}{effect_name}_{method_name}")
}

// ============================================================================
// Adapter TirFunction synthesis
// ============================================================================

/// Fixed async outptr address (matches codegen convention).
const ASYNC_OUTPTR: i32 = 2048;

/// Parse a converter function path like `"core/internal/cm_list_string_to_array"`
/// into `(ModuleSource, function_name)`.
fn parse_converter_path(path: &str) -> (ModuleSource, String) {
    // Format: "core/{module}/{function}" → ModuleSource::core(module), function
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() == 3 && parts[0] == "core" {
        (ModuleSource::core(parts[1]), parts[2].to_string())
    } else {
        // Fallback: use the full path as module and last segment as name
        let name = parts.last().unwrap_or(&path).to_string();
        let module_parts: Vec<String> = parts[..parts.len().saturating_sub(1)]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        (ModuleSource::from_path(&module_parts), name)
    }
}

/// Create a `TirFunction` with default metadata fields.
fn make_adapter_function(
    name: String,
    params: Vec<TirParam>,
    return_type: TypeId,
    body: TirBlock,
    local_count: u32,
    local_types: Vec<TypeId>,
) -> Rc<RefCell<TirFunction>> {
    Rc::new(RefCell::new(TirFunction {
        name,
        is_pub: false,
        is_export: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params,
        return_type,
        effects: vec![],
        body: Some(body),
        span: synth_span(),
        local_count,
        local_types,
        address_taken_locals: IndexSet::new(),
        needed_copy_types: IndexSet::new(),
        scratch_locals: vec![],
        copy_source_types: IndexSet::new(),
        indirect_call_counts: IndexMap::new(),
        match_scrutinee_types: vec![],
        let_pattern_types: vec![],
        cm_export_info: None,
    }))
}

/// Map a WASI return type to the flat return `TypeId` for the adapter.
/// Sync functions with outptr return void from the raw call itself.
fn wasi_return_type_id(func_info: &WasiFunctionInfo) -> TypeId {
    let conv = &func_info.call_convention;
    if conv.is_async || conv.outptr_alloc.is_some() {
        // Async: raw call returns subtask handle (i32)
        // Outptr: raw call returns void; result is read from outptr
        if conv.is_async {
            TypeTable::I32
        } else {
            TypeTable::UNIT
        }
    } else if let Some(ty) = &func_info.return_type {
        // Flat return: use the core type
        match ty {
            Type::Named(n) => match n.name.as_str() {
                "i32" | "u32" => TypeTable::I32,
                "i64" | "u64" => TypeTable::I64,
                "f32" => TypeTable::F32,
                "f64" => TypeTable::F64,
                "bool" => TypeTable::I32, // CM returns bool as i32
                _ => TypeTable::I32,
            },
            _ => TypeTable::I32,
        }
    } else {
        TypeTable::UNIT
    }
}

/// Synthesize a CM adapter function for a WASI import.
///
/// The adapter function:
/// 1. Accepts the same parameter types as the WASI function
/// 2. Lowers parameters to flat CM ABI (String → ptr/len, etc.)
/// 3. Calls the lowered WASI function via `CmRawCall`
/// 4. Lifts the result from flat CM ABI back to Wado types
/// 5. Returns the Wado-typed result
///
/// The adapter's Wado-level return type matches the WASI function declaration.
/// For functions with `result_converter` (complex return types like list<string>),
/// the adapter delegates to the existing converter function.
fn synthesize_adapter(func_info: &WasiFunctionInfo) -> Rc<RefCell<TirFunction>> {
    let name = adapter_func_name(&func_info.effect_name, &func_info.method_name);
    let conv = &func_info.call_convention;
    let local_name = func_info.local_alias_name();

    let mut next_local: u32 = 0;
    let mut params = Vec::new();
    let mut local_types: Vec<TypeId> = Vec::new();
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut flat_args: Vec<TirExpr> = Vec::new();

    // ---- Build parameters and parameter lowering ----
    for (param_name, param_type) in &func_info.params {
        let flat_tys = flatten_param_type(param_type);

        match param_type {
            // String param: accept Wado String, lower to (ptr, len) pair
            Type::Named(n) if n.name == "String" => {
                let param_local = next_local;
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32, // placeholder for String
                    local_index: param_local,
                    span: synth_span(),
                });
                local_types.push(TypeTable::I32);
                next_local += 1;

                // Call cm_lower_string → packed i64
                let packed_local = next_local;
                let packed = internal_call(
                    "cm_lower_string",
                    vec![local_ref(param_local, param_name, TypeTable::I32)],
                    TypeTable::I64,
                );
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_packed"),
                    packed_local,
                    TypeTable::I64,
                    packed,
                ));
                local_types.push(TypeTable::I64);
                next_local += 1;

                // ptr = packed as i32 (low 32 bits)
                flat_args.push(cast(
                    local_ref(
                        packed_local,
                        &format!("__{param_name}_packed"),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
                // len = (packed >> 32) as i32 (high 32 bits)
                flat_args.push(cast(
                    binary(
                        crate::tir::TirBinaryOp::Shr,
                        local_ref(
                            packed_local,
                            &format!("__{param_name}_packed"),
                            TypeTable::I64,
                        ),
                        i64_const(32),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
            }

            // Array<u8> param: accept Wado Array<u8>, lower to (ptr, len) pair
            Type::Generic(g)
                if g.name == "Array"
                    && g.args.len() == 1
                    && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                let param_local = next_local;
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32, // placeholder for Array<u8>
                    local_index: param_local,
                    span: synth_span(),
                });
                local_types.push(TypeTable::I32);
                next_local += 1;

                // Call cm_lower_array_u8 → packed i64
                let packed_local = next_local;
                let packed = internal_call(
                    "cm_lower_array_u8",
                    vec![local_ref(param_local, param_name, TypeTable::I32)],
                    TypeTable::I64,
                );
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_packed"),
                    packed_local,
                    TypeTable::I64,
                    packed,
                ));
                local_types.push(TypeTable::I64);
                next_local += 1;

                // Split packed i64 → (ptr, len)
                flat_args.push(cast(
                    local_ref(
                        packed_local,
                        &format!("__{param_name}_packed"),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
                flat_args.push(cast(
                    binary(
                        crate::tir::TirBinaryOp::Shr,
                        local_ref(
                            packed_local,
                            &format!("__{param_name}_packed"),
                            TypeTable::I64,
                        ),
                        i64_const(32),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
            }

            // Simple types: pass directly as flat args
            _ => {
                if flat_tys.is_empty() {
                    continue; // unit param, no flat args
                }
                // For simple types with 1 flat representation, pass directly
                let param_type_id = flat_tys[0];
                let param_local = next_local;
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: param_type_id,
                    local_index: param_local,
                    span: synth_span(),
                });
                local_types.push(param_type_id);
                next_local += 1;

                flat_args.push(local_ref(param_local, param_name, param_type_id));
            }
        }
    }

    // ---- Handle outptr for async or complex returns ----
    if conv.is_async {
        flat_args.push(i32_const(ASYNC_OUTPTR));
    } else if let Some((size, align)) = conv.outptr_alloc {
        // Allocate outptr via realloc
        let outptr_local = next_local;
        let outptr_alloc = builtin_call(
            "realloc",
            vec![
                i32_const(0),            // old_ptr
                i32_const(0),            // old_size
                i32_const(align as i32), // align
                i32_const(size as i32),  // new_size
            ],
            TypeTable::I32,
        );
        body_stmts.push(let_stmt(
            "__outptr",
            outptr_local,
            TypeTable::I32,
            outptr_alloc,
        ));
        local_types.push(TypeTable::I32);
        next_local += 1;

        flat_args.push(local_ref(outptr_local, "__outptr", TypeTable::I32));
    }

    // ---- Build CmRawCall ----
    let raw_call_return_type = wasi_return_type_id(func_info);
    let raw_call_expr = cm_raw_call(&local_name, flat_args, raw_call_return_type);

    // ---- Handle result ----
    // The adapter's return type to the Wado caller:
    let adapter_return_type;

    if conv.is_async {
        // Async: discard subtask handle, return void
        body_stmts.push(expr_stmt(raw_call_expr));
        adapter_return_type = TypeTable::UNIT;
    } else if let Some(ref converter) = conv.result_converter {
        // Complex return with converter: call cm_raw_call, then call converter with outptr
        body_stmts.push(expr_stmt(raw_call_expr));

        // Read outptr local index (it was the last allocated before outptr)
        let outptr_local = next_local - 1; // __outptr was the last local before raw call

        // Call the converter function with outptr
        // Converter paths are like "core/internal/cm_list_string_to_array"
        let (conv_module, conv_name) = parse_converter_path(converter);
        let converter_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef::External {
                    module_source: conv_module,
                    name: conv_name,
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: vec![local_ref(outptr_local, "__outptr", TypeTable::I32)],
            },
            TypeTable::I32, // placeholder for converter return type
            synth_span(),
        );
        body_stmts.push(return_stmt(Some(converter_call)));
        adapter_return_type = TypeTable::I32; // placeholder
    } else if let Some(ref elements) = conv.tuple_return {
        // Tuple return: call cm_raw_call, then load each element from outptr
        body_stmts.push(expr_stmt(raw_call_expr));
        let outptr_local = next_local - 1;

        // Load each tuple element from outptr and construct a TupleLiteral
        let mut tuple_elements = Vec::new();
        let mut field_offset: u32 = 0;
        for prim in elements {
            // Align offset
            let align = prim.align();
            if !field_offset.is_multiple_of(align) {
                field_offset += align - (field_offset % align);
            }

            // Generate: builtin::i32_load(outptr + offset) or builtin::i64_load(outptr + offset)
            let outptr_ref = local_ref(outptr_local, "__outptr", TypeTable::I32);
            let offset_expr = i32_const(field_offset as i32);
            let addr = binary_add(outptr_ref, offset_expr);
            let load = load_cm_primitive(prim, addr);

            tuple_elements.push(load);
            field_offset += prim.size();
        }

        let tuple_expr = TirExpr::new(
            TirExprKind::TupleLiteral {
                elements: tuple_elements,
            },
            TypeTable::I32, // placeholder, fixed up by call-site rewriting
            synth_span(),
        );
        body_stmts.push(return_stmt(Some(tuple_expr)));
        adapter_return_type = TypeTable::I32; // placeholder, fixed up by call-site rewriting
    } else if conv.result_return.is_some() {
        // Result return: call cm_raw_call, then read discriminant and payload from outptr
        body_stmts.push(expr_stmt(raw_call_expr));
        let outptr_local = next_local - 1;

        // Delegate to converter or read directly
        // For now, return the outptr (codegen handles result construction)
        body_stmts.push(return_stmt(Some(local_ref(
            outptr_local,
            "__outptr",
            TypeTable::I32,
        ))));
        adapter_return_type = TypeTable::I32; // placeholder
    } else if conv.outptr_alloc.is_some() {
        // Outptr return without converter: unexpected, but handle gracefully
        body_stmts.push(expr_stmt(raw_call_expr));
        let outptr_local = next_local - 1;
        body_stmts.push(return_stmt(Some(local_ref(
            outptr_local,
            "__outptr",
            TypeTable::I32,
        ))));
        adapter_return_type = TypeTable::I32;
    } else if func_info.return_type.is_some() {
        // Flat return: cm_raw_call directly returns the value
        body_stmts.push(return_stmt(Some(raw_call_expr)));
        adapter_return_type = raw_call_return_type;
    } else {
        // No return: just call
        body_stmts.push(expr_stmt(raw_call_expr));
        adapter_return_type = TypeTable::UNIT;
    }

    let body = block(body_stmts);

    make_adapter_function(
        name,
        params,
        adapter_return_type,
        body,
        next_local,
        local_types,
    )
}

// ============================================================================
// Phase entry point
// ============================================================================

/// Phase entry point: generate CM adapter functions and rewrite call sites.
///
/// For each WASI import function used in the program:
/// 1. Synthesizes an adapter TIR function that handles CM boundary crossing
/// 2. Rewrites effect-like `Call` nodes to target the adapter function
///
/// Adapter functions flow through monomorphize → lower → optimize → codegen
/// like any other function.
pub fn generate_adapters(mut project: Project) -> Project {
    // Step 1: Collect all used WASI effect calls
    let mut seen_effects: IndexSet<String> = IndexSet::new();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(body, &mut seen_effects);
            }
        }
    }

    if seen_effects.is_empty() {
        return project;
    }

    // Step 2: Synthesize adapter functions for each used WASI function
    // Skip functions with tuple_return or result_return that require complex
    // lifting not yet implemented (those stay on the old codegen path).
    let mut adapters: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::new();
    for qualified_name in &seen_effects {
        if let Some(func_info) = project.wasi_registry.get_function(qualified_name) {
            let conv = &func_info.call_convention;
            // Skip functions that require complex result construction
            // from outptr that we haven't implemented yet.
            // Async functions are exempt because their immediate return is just
            // an i32 subtask handle (result fields are irrelevant).
            // tuple_return is handled by load_cm_primitive in synthesize_adapter.
            if !conv.is_async && conv.result_return.is_some() && conv.result_converter.is_none() {
                continue;
            }
            let func_info = func_info.clone();
            let adapter_name = adapter_func_name(&func_info.effect_name, &func_info.method_name);
            let adapter = synthesize_adapter(&func_info);
            adapters.insert(qualified_name.clone(), adapter.clone());
            // Also index by adapter function name for lookup
            adapters.insert(adapter_name, adapter);
        }
    }

    // Step 3: Add adapter functions to the entry module
    let entry_source = project.entry_module_source.clone();
    if let Some(entry_module) = project.tir_modules.get_mut(&entry_source) {
        for (key, adapter_rc) in &adapters {
            // Only add each adapter once (skip the duplicate keyed by adapter_name)
            if key.contains("::") {
                entry_module.functions.push(adapter_rc.clone());
            }
        }
    }

    // Step 4: Rewrite effect-like call nodes to target adapters
    let adapter_map: IndexMap<String, Rc<RefCell<TirFunction>>> = adapters
        .iter()
        .filter(|(k, _)| k.contains("::"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                rewrite_calls_in_block(body, &adapter_map, &entry_source);
            }
        }
    }

    project
}

// ============================================================================
// Adapter type fixup
// ============================================================================

/// Fix up the return expression's type in the adapter body to match the caller's
/// expected return type. The adapter was created with placeholder `TypeId`s
/// (e.g., `TypeTable::I32`) that need to be corrected to actual Wado types.
fn fixup_return_type_in_body(adapter: &mut TirFunction, return_type: TypeId) {
    if let Some(body) = &mut adapter.body {
        for stmt in &mut body.stmts {
            if let TirStmtKind::Return {
                value: Some(ret_expr),
            } = &mut stmt.kind
            {
                fixup_expr_type(ret_expr, return_type);
            }
        }
    }
}

/// Recursively fix the `type_id` of an expression and its leaf nodes.
/// This is used to replace placeholder `TypeId`s in adapter return expressions.
fn fixup_expr_type(expr: &mut TirExpr, type_id: TypeId) {
    expr.type_id = type_id;
    // For Call/Local expressions, also fix inner type
    match &mut expr.kind {
        TirExprKind::Call { .. } | TirExprKind::Local { .. } => {
            // Already fixed the outer type_id
        }
        _ => {}
    }
}

// ============================================================================
// Call site rewriting
// ============================================================================

fn rewrite_calls_in_block(
    block: &mut TirBlock,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
) {
    for stmt in &mut block.stmts {
        rewrite_calls_in_stmt(stmt, adapters, entry_source);
    }
}

fn rewrite_calls_in_stmt(
    stmt: &mut TirStmt,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            rewrite_calls_in_expr(value, adapters, entry_source);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source);
            rewrite_calls_in_block(then_block, adapters, entry_source);
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_calls_in_block(body, adapters, entry_source);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source);
            rewrite_calls_in_block(then_block, adapters, entry_source);
            if let Some(blk) = else_block {
                rewrite_calls_in_block(blk, adapters, entry_source);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_calls_in_expr(v, adapters, entry_source);
            }
        }
        TirStmtKind::Continue | TirStmtKind::LetPattern { .. } => {}
    }
}

fn rewrite_calls_in_expr(
    expr: &mut TirExpr,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
) {
    // Check if this is an EffectCall that should be rewritten to target an adapter
    if let TirExprKind::EffectCall {
        effect_name,
        op_name,
        ..
    } = &expr.kind
    {
        let qualified = format!("{effect_name}::{op_name}");
        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Collect args before replacing (need to move them out)
            let mut taken_args: Vec<TirExpr> = Vec::new();
            if let TirExprKind::EffectCall { args, .. } = &mut expr.kind {
                taken_args = std::mem::take(args);
            }

            // Fix up adapter function types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if adapter.return_type != expr.type_id {
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, expr.type_id);
                }
                for (i, arg) in taken_args.iter().enumerate() {
                    if i < adapter.params.len() && adapter.params[i].type_id != arg.type_id {
                        let local_idx = adapter.params[i].local_index as usize;
                        adapter.params[i].type_id = arg.type_id;
                        if local_idx < adapter.local_types.len() {
                            adapter.local_types[local_idx] = arg.type_id;
                        }
                    }
                }
            }

            // Replace EffectCall with Call targeting the adapter
            expr.kind = TirExprKind::Call {
                func: FunctionRef::Resolved {
                    func: adapter_rc.clone(),
                    module_source: entry_source.clone(),
                },
                args: taken_args,
                type_args: vec![],
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(arg, adapters, entry_source);
                }
            }
            return;
        }
    }

    // Check if this is an effect-like Call that should be rewritten
    let is_effect_call = matches!(&expr.kind, TirExprKind::Call { func, .. }
        if func.module_source().is_effect_like() && func.module_source().effect_name().is_some());
    if is_effect_call
        && let TirExprKind::Call {
            func,
            args,
            type_args,
        } = &mut expr.kind
    {
        let effect_name = func.module_source().effect_name().unwrap_or_default();
        let method_name = func.name();
        let qualified = format!("{effect_name}::{method_name}");

        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Fix up adapter function types from the call site
            {
                let mut adapter = adapter_rc.borrow_mut();
                if adapter.return_type != expr.type_id {
                    adapter.return_type = expr.type_id;
                    fixup_return_type_in_body(&mut adapter, expr.type_id);
                }
                for (i, arg) in args.iter().enumerate() {
                    if i < adapter.params.len() && adapter.params[i].type_id != arg.type_id {
                        let local_idx = adapter.params[i].local_index as usize;
                        adapter.params[i].type_id = arg.type_id;
                        if local_idx < adapter.local_types.len() {
                            adapter.local_types[local_idx] = arg.type_id;
                        }
                    }
                }
            }

            // Rewrite to call the adapter function
            *func = FunctionRef::Resolved {
                func: adapter_rc.clone(),
                module_source: entry_source.clone(),
            };
            *type_args = vec![];

            // Recurse into args
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
            return;
        }
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Call { args, .. }
        | TirExprKind::EffectCall { args, .. }
        | TirExprKind::CmRawCall { args, .. }
        | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_calls_in_expr(receiver, adapters, entry_source);
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_calls_in_expr(callee, adapters, entry_source);
            for arg in args {
                rewrite_calls_in_expr(arg, adapters, entry_source);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_calls_in_block(block, adapters, entry_source);
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_calls_in_expr(left, adapters, entry_source);
            rewrite_calls_in_expr(right, adapters, entry_source);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { expr: inner } => {
            rewrite_calls_in_expr(inner, adapters, entry_source);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_calls_in_expr(condition, adapters, entry_source);
            rewrite_calls_in_block(then_branch, adapters, entry_source);
            if let Some(blk) = else_branch {
                rewrite_calls_in_block(blk, adapters, entry_source);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            rewrite_calls_in_expr(e, adapters, entry_source);
            rewrite_calls_in_expr(index, adapters, entry_source);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source);
            for arm in arms {
                rewrite_calls_in_block(arm, adapters, entry_source);
            }
            rewrite_calls_in_block(default, adapters, entry_source);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            rewrite_calls_in_expr(scrutinee, adapters, entry_source);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rewrite_calls_in_expr(guard, adapters, entry_source);
                }
                rewrite_calls_in_expr(&mut arm.body, adapters, entry_source);
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_calls_in_expr(body, adapters, entry_source);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_calls_in_expr(functor, adapters, entry_source);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in &mut fields.iter_mut() {
                rewrite_calls_in_expr(&mut field.value, adapters, entry_source);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_calls_in_expr(value, adapters, entry_source);
        }
        _ => {} // Leaf nodes: no sub-expressions
    }
}

// ============================================================================
// Effect call collection
// ============================================================================

fn collect_effect_calls_in_block(block: &TirBlock, effects: &mut IndexSet<String>) {
    for stmt in &block.stmts {
        collect_effect_calls_in_stmt(stmt, effects);
    }
}

fn collect_effect_calls_in_stmt(stmt: &TirStmt, effects: &mut IndexSet<String>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            collect_effect_calls_in_expr(value, effects);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_effect_calls_in_expr(condition, effects);
            collect_effect_calls_in_block(then_block, effects);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_effect_calls_in_block(body, effects);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects);
            collect_effect_calls_in_block(then_block, effects);
            if let Some(blk) = else_block {
                collect_effect_calls_in_block(blk, effects);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_effect_calls_in_expr(v, effects);
            }
        }
        TirStmtKind::Continue | TirStmtKind::LetPattern { .. } => {}
    }
}

fn collect_effect_calls_in_expr(expr: &TirExpr, effects: &mut IndexSet<String>) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let module_source = func.module_source();
            if module_source.is_effect_like()
                && let Some(effect_name) = module_source.effect_name()
            {
                let method_name = func.name();
                effects.insert(format!("{effect_name}::{method_name}"));
            }
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::EffectCall {
            effect_name,
            op_name,
            args,
            ..
        } => {
            effects.insert(format!("{effect_name}::{op_name}"));
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::CmRawCall { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_effect_calls_in_expr(receiver, effects);
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_effect_calls_in_expr(callee, effects);
            for arg in args {
                collect_effect_calls_in_expr(arg, effects);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_effect_calls_in_block(block, effects);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_effect_calls_in_expr(left, effects);
            collect_effect_calls_in_expr(right, effects);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { expr: inner } => {
            collect_effect_calls_in_expr(inner, effects);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_effect_calls_in_expr(condition, effects);
            collect_effect_calls_in_block(then_branch, effects);
            if let Some(blk) = else_branch {
                collect_effect_calls_in_block(blk, effects);
            }
        }
        TirExprKind::Index { expr: e, index }
        | TirExprKind::Assign {
            target: e,
            value: index,
        } => {
            collect_effect_calls_in_expr(e, effects);
            collect_effect_calls_in_expr(index, effects);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_effect_calls_in_expr(scrutinee, effects);
            for arm in arms {
                collect_effect_calls_in_block(arm, effects);
            }
            collect_effect_calls_in_block(default, effects);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            collect_effect_calls_in_expr(scrutinee, effects);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_effect_calls_in_expr(guard, effects);
                }
                collect_effect_calls_in_expr(&arm.body, effects);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_effect_calls_in_expr(body, effects);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_effect_calls_in_expr(functor, effects);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_effect_calls_in_expr(&field.value, effects);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_effect_calls_in_expr(value, effects);
        }
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Local { .. }
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Capture { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        // Catch-all for any remaining leaf or rare variants
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NamedType;

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType {
            name: name.to_string(),
            span: Span::new(0, 0, 1, 1),
        })
    }

    #[test]
    fn flatten_param_i32() {
        assert_eq!(flatten_param_type(&named_type("i32")), vec![TypeTable::I32]);
    }

    #[test]
    fn flatten_param_i64() {
        assert_eq!(flatten_param_type(&named_type("i64")), vec![TypeTable::I64]);
    }

    #[test]
    fn flatten_param_f64() {
        assert_eq!(flatten_param_type(&named_type("f64")), vec![TypeTable::F64]);
    }

    #[test]
    fn flatten_param_string() {
        assert_eq!(
            flatten_param_type(&named_type("String")),
            vec![TypeTable::I32, TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_bool() {
        assert_eq!(
            flatten_param_type(&named_type("bool")),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_unit() {
        assert!(flatten_param_type(&Type::Tuple(vec![])).is_empty());
    }

    #[test]
    fn adapter_name() {
        assert_eq!(
            adapter_func_name("Stdout", "write_via_stream"),
            "__cm_adapter__Stdout_write_via_stream"
        );
    }

    #[test]
    fn lift_i32() {
        let expr = synthesize_lift(&named_type("i32"), i32_const(100), &mut 0);
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lift_bool() {
        let expr = synthesize_lift(&named_type("bool"), i32_const(100), &mut 0);
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_string() {
        let expr = synthesize_lift(&named_type("String"), i32_const(100), &mut 0);
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lower_i32() {
        let stmts = synthesize_lower(&named_type("i32"), i32_const(42), i32_const(100), &mut 0);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_bool() {
        let value = TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            synth_span(),
        );
        let stmts = synthesize_lower(&named_type("bool"), value, i32_const(100), &mut 0);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_unit() {
        let value = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
        let stmts = synthesize_lower(&Type::Tuple(vec![]), value, i32_const(100), &mut 0);
        assert!(stmts.is_empty());
    }

    #[test]
    fn lower_string() {
        let value = TirExpr::new(
            TirExprKind::StringLiteral("hello".to_string()),
            TypeTable::I32, // placeholder
            synth_span(),
        );
        let mut next_local = 10_u32;
        let stmts = synthesize_lower(
            &named_type("String"),
            value,
            i32_const(100),
            &mut next_local,
        );
        // Should produce: let __packed = cm_lower_string(value); store ptr; store len
        assert_eq!(stmts.len(), 3);
        assert_eq!(next_local, 11); // one local allocated for __packed
    }

    #[test]
    fn helpers_i32_const() {
        let expr = i32_const(42);
        assert_eq!(expr.type_id, TypeTable::I32);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 42),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_i64_const() {
        let expr = i64_const(123);
        assert_eq!(expr.type_id, TypeTable::I64);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 123),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_builtin_call() {
        let call = builtin_call("i32_load", vec![i32_const(0)], TypeTable::I32);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name(), "i32_load");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_internal_call() {
        let call = internal_call("cm_lower_string", vec![i32_const(0)], TypeTable::I64);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name(), "cm_lower_string");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_cm_raw_call() {
        let call = cm_raw_call(
            "wasi:cli/Stdout::write_via_stream",
            vec![i32_const(0), i32_const(1), i32_const(2)],
            TypeTable::I32,
        );
        match &call.kind {
            TirExprKind::CmRawCall { local_name, args } => {
                assert_eq!(local_name, "wasi:cli/Stdout::write_via_stream");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected CmRawCall, got {other:?}"),
        }
    }

    #[test]
    fn helpers_let_stmt() {
        let stmt = let_stmt("x", 0, TypeTable::I32, i32_const(42));
        match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                type_id,
                ..
            } => {
                assert_eq!(name, "x");
                assert_eq!(*local_index, 0);
                assert_eq!(*type_id, TypeTable::I32);
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }
}
