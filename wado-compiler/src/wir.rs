// WIR (Wasm IR) — an interposition layer between codegen and wasm_encoder.
//
// WirFunction captures the instruction stream as WirInstr values, then
// emits them to a wasm_encoder::Function via wir_emit.  This proves the
// WIR representation can round-trip every instruction the codegen produces.

use wasm_encoder::{BlockType, HeapType, Instruction, MemArg, ValType};

use crate::wir_emit;

/// Owned memory-access argument (mirrors `wasm_encoder::MemArg`).
#[derive(Clone, Copy, Debug)]
pub struct WirMemArg {
    pub offset: u64,
    pub align: u32,
    pub memory_index: u32,
}

impl From<MemArg> for WirMemArg {
    fn from(m: MemArg) -> Self {
        Self {
            offset: m.offset,
            align: m.align,
            memory_index: m.memory_index,
        }
    }
}

impl From<WirMemArg> for MemArg {
    fn from(m: WirMemArg) -> Self {
        Self {
            offset: m.offset,
            align: m.align,
            memory_index: m.memory_index,
        }
    }
}

/// Owned Wasm instruction — lifetime-free mirror of `wasm_encoder::Instruction`.
///
/// Only the variants actually used by codegen are included.
#[derive(Clone, Debug)]
pub enum WirInstr {
    // ── Constants ───────────────────────────────────────────────────────
    I32Const(i32),
    I64Const(i64),
    /// Stored as raw IEEE-754 bits to avoid NaN canonicalization.
    F32Const(u32),
    /// Stored as raw IEEE-754 bits to avoid NaN canonicalization.
    F64Const(u64),

    // ── Locals / Globals ───────────────────────────────────────────────
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),

    // ── i32 arithmetic & comparison ────────────────────────────────────
    I32Add,
    I32Sub,
    I32Mul,
    I32DivS,
    I32DivU,
    I32RemS,
    I32RemU,
    I32And,
    I32Or,
    I32Xor,
    I32Shl,
    I32ShrS,
    I32ShrU,
    I32Clz,
    I32Eq,
    I32Ne,
    I32Eqz,
    I32LtS,
    I32LtU,
    I32GtS,
    I32GtU,
    I32LeS,
    I32LeU,
    I32GeS,
    I32GeU,

    // ── i64 arithmetic & comparison ────────────────────────────────────
    I64Add,
    I64Sub,
    I64Mul,
    I64DivS,
    I64DivU,
    I64RemS,
    I64RemU,
    I64And,
    I64Or,
    I64Xor,
    I64Shl,
    I64ShrS,
    I64ShrU,
    I64Clz,
    I64Eq,
    I64Ne,
    I64LtS,
    I64LtU,
    I64GtS,
    I64GtU,
    I64LeS,
    I64LeU,
    I64GeS,
    I64GeU,

    // ── Wide-integer instructions (Wasm 3.0) ───────────────────────────
    I64Add128,
    I64Sub128,
    I64MulWideS,
    I64MulWideU,

    // ── f32 arithmetic & comparison ────────────────────────────────────
    F32Add,
    F32Sub,
    F32Mul,
    F32Div,
    F32Eq,
    F32Ne,
    F32Lt,
    F32Gt,
    F32Le,
    F32Ge,
    F32Abs,
    F32Neg,
    F32Ceil,
    F32Floor,
    F32Trunc,
    F32Nearest,
    F32Sqrt,
    F32Min,
    F32Max,
    F32Copysign,

    // ── f64 arithmetic & comparison ────────────────────────────────────
    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    F64Eq,
    F64Ne,
    F64Lt,
    F64Gt,
    F64Le,
    F64Ge,
    F64Abs,
    F64Neg,
    F64Ceil,
    F64Floor,
    F64Trunc,
    F64Nearest,
    F64Sqrt,
    F64Min,
    F64Max,
    F64Copysign,

    // ── Conversions / reinterpret ──────────────────────────────────────
    I32WrapI64,
    I64ExtendI32S,
    I64ExtendI32U,
    I32TruncF32S,
    I32TruncF64S,
    I64TruncF32S,
    I64TruncF32U,
    I64TruncF64S,
    I64TruncF64U,
    F32ConvertI32S,
    F32ConvertI64S,
    F32ConvertI64U,
    F64ConvertI32S,
    F64ConvertI64S,
    F64ConvertI64U,
    F32DemoteF64,
    F64PromoteF32,
    I32ReinterpretF32,
    I64ReinterpretF64,
    F32ReinterpretI32,
    F64ReinterpretI64,

    // ── Memory ─────────────────────────────────────────────────────────
    I32Load(WirMemArg),
    I64Load(WirMemArg),
    F32Load(WirMemArg),
    F64Load(WirMemArg),
    I32Load8U(WirMemArg),
    I32Store(WirMemArg),
    I32Store8(WirMemArg),

    // ── Control flow ───────────────────────────────────────────────────
    Block(BlockType),
    Loop(BlockType),
    If(BlockType),
    Else,
    End,
    Br(u32),
    BrIf(u32),
    BrTable {
        targets: Vec<u32>,
        default: u32,
    },
    Return,
    Unreachable,

    // ── Call ────────────────────────────────────────────────────────────
    Call(u32),
    CallRef(u32),

    // ── GC: struct ─────────────────────────────────────────────────────
    StructNew(u32),
    StructGet {
        struct_type_index: u32,
        field_index: u32,
    },
    StructSet {
        struct_type_index: u32,
        field_index: u32,
    },

    // ── GC: array ──────────────────────────────────────────────────────
    ArrayNew(u32),
    ArrayNewDefault(u32),
    ArrayNewData {
        array_type_index: u32,
        array_data_index: u32,
    },
    ArrayNewFixed {
        array_type_index: u32,
        array_size: u32,
    },
    ArrayGet(u32),
    ArrayGetS(u32),
    ArrayGetU(u32),
    ArraySet(u32),
    ArrayLen,
    ArrayCopy {
        array_type_index_dst: u32,
        array_type_index_src: u32,
    },
    ArrayFill(u32),

    // ── GC: ref ────────────────────────────────────────────────────────
    RefNull(HeapType),
    RefIsNull,
    RefAsNonNull,
    RefEq,
    RefFunc(u32),
    RefCastNonNull(HeapType),
    RefTestNonNull(HeapType),

    // ── Misc ───────────────────────────────────────────────────────────
    Drop,
    TypedSelect(ValType),
}

impl WirInstr {
    /// Convert a borrowed `wasm_encoder::Instruction` to an owned `WirInstr`.
    ///
    /// Panics on instruction variants that codegen never emits.
    pub fn from_instruction(instr: &Instruction<'_>) -> Self {
        match *instr {
            // Constants
            Instruction::I32Const(v) => Self::I32Const(v),
            Instruction::I64Const(v) => Self::I64Const(v),
            Instruction::F32Const(ieee) => Self::F32Const(ieee.bits()),
            Instruction::F64Const(ieee) => Self::F64Const(ieee.bits()),

            // Locals / Globals
            Instruction::LocalGet(i) => Self::LocalGet(i),
            Instruction::LocalSet(i) => Self::LocalSet(i),
            Instruction::LocalTee(i) => Self::LocalTee(i),
            Instruction::GlobalGet(i) => Self::GlobalGet(i),
            Instruction::GlobalSet(i) => Self::GlobalSet(i),

            // i32 arithmetic
            Instruction::I32Add => Self::I32Add,
            Instruction::I32Sub => Self::I32Sub,
            Instruction::I32Mul => Self::I32Mul,
            Instruction::I32DivS => Self::I32DivS,
            Instruction::I32DivU => Self::I32DivU,
            Instruction::I32RemS => Self::I32RemS,
            Instruction::I32RemU => Self::I32RemU,
            Instruction::I32And => Self::I32And,
            Instruction::I32Or => Self::I32Or,
            Instruction::I32Xor => Self::I32Xor,
            Instruction::I32Shl => Self::I32Shl,
            Instruction::I32ShrS => Self::I32ShrS,
            Instruction::I32ShrU => Self::I32ShrU,
            Instruction::I32Clz => Self::I32Clz,
            Instruction::I32Eq => Self::I32Eq,
            Instruction::I32Ne => Self::I32Ne,
            Instruction::I32Eqz => Self::I32Eqz,
            Instruction::I32LtS => Self::I32LtS,
            Instruction::I32LtU => Self::I32LtU,
            Instruction::I32GtS => Self::I32GtS,
            Instruction::I32GtU => Self::I32GtU,
            Instruction::I32LeS => Self::I32LeS,
            Instruction::I32LeU => Self::I32LeU,
            Instruction::I32GeS => Self::I32GeS,
            Instruction::I32GeU => Self::I32GeU,

            // i64 arithmetic
            Instruction::I64Add => Self::I64Add,
            Instruction::I64Sub => Self::I64Sub,
            Instruction::I64Mul => Self::I64Mul,
            Instruction::I64DivS => Self::I64DivS,
            Instruction::I64DivU => Self::I64DivU,
            Instruction::I64RemS => Self::I64RemS,
            Instruction::I64RemU => Self::I64RemU,
            Instruction::I64And => Self::I64And,
            Instruction::I64Or => Self::I64Or,
            Instruction::I64Xor => Self::I64Xor,
            Instruction::I64Shl => Self::I64Shl,
            Instruction::I64ShrS => Self::I64ShrS,
            Instruction::I64ShrU => Self::I64ShrU,
            Instruction::I64Clz => Self::I64Clz,
            Instruction::I64Eq => Self::I64Eq,
            Instruction::I64Ne => Self::I64Ne,
            Instruction::I64LtS => Self::I64LtS,
            Instruction::I64LtU => Self::I64LtU,
            Instruction::I64GtS => Self::I64GtS,
            Instruction::I64GtU => Self::I64GtU,
            Instruction::I64LeS => Self::I64LeS,
            Instruction::I64LeU => Self::I64LeU,
            Instruction::I64GeS => Self::I64GeS,
            Instruction::I64GeU => Self::I64GeU,

            // Wide integer
            Instruction::I64Add128 => Self::I64Add128,
            Instruction::I64Sub128 => Self::I64Sub128,
            Instruction::I64MulWideS => Self::I64MulWideS,
            Instruction::I64MulWideU => Self::I64MulWideU,

            // f32 arithmetic
            Instruction::F32Add => Self::F32Add,
            Instruction::F32Sub => Self::F32Sub,
            Instruction::F32Mul => Self::F32Mul,
            Instruction::F32Div => Self::F32Div,
            Instruction::F32Eq => Self::F32Eq,
            Instruction::F32Ne => Self::F32Ne,
            Instruction::F32Lt => Self::F32Lt,
            Instruction::F32Gt => Self::F32Gt,
            Instruction::F32Le => Self::F32Le,
            Instruction::F32Ge => Self::F32Ge,
            Instruction::F32Abs => Self::F32Abs,
            Instruction::F32Neg => Self::F32Neg,
            Instruction::F32Ceil => Self::F32Ceil,
            Instruction::F32Floor => Self::F32Floor,
            Instruction::F32Trunc => Self::F32Trunc,
            Instruction::F32Nearest => Self::F32Nearest,
            Instruction::F32Sqrt => Self::F32Sqrt,
            Instruction::F32Min => Self::F32Min,
            Instruction::F32Max => Self::F32Max,
            Instruction::F32Copysign => Self::F32Copysign,

            // f64 arithmetic
            Instruction::F64Add => Self::F64Add,
            Instruction::F64Sub => Self::F64Sub,
            Instruction::F64Mul => Self::F64Mul,
            Instruction::F64Div => Self::F64Div,
            Instruction::F64Eq => Self::F64Eq,
            Instruction::F64Ne => Self::F64Ne,
            Instruction::F64Lt => Self::F64Lt,
            Instruction::F64Gt => Self::F64Gt,
            Instruction::F64Le => Self::F64Le,
            Instruction::F64Ge => Self::F64Ge,
            Instruction::F64Abs => Self::F64Abs,
            Instruction::F64Neg => Self::F64Neg,
            Instruction::F64Ceil => Self::F64Ceil,
            Instruction::F64Floor => Self::F64Floor,
            Instruction::F64Trunc => Self::F64Trunc,
            Instruction::F64Nearest => Self::F64Nearest,
            Instruction::F64Sqrt => Self::F64Sqrt,
            Instruction::F64Min => Self::F64Min,
            Instruction::F64Max => Self::F64Max,
            Instruction::F64Copysign => Self::F64Copysign,

            // Conversions
            Instruction::I32WrapI64 => Self::I32WrapI64,
            Instruction::I64ExtendI32S => Self::I64ExtendI32S,
            Instruction::I64ExtendI32U => Self::I64ExtendI32U,
            Instruction::I32TruncF32S => Self::I32TruncF32S,
            Instruction::I32TruncF64S => Self::I32TruncF64S,
            Instruction::I64TruncF32S => Self::I64TruncF32S,
            Instruction::I64TruncF32U => Self::I64TruncF32U,
            Instruction::I64TruncF64S => Self::I64TruncF64S,
            Instruction::I64TruncF64U => Self::I64TruncF64U,
            Instruction::F32ConvertI32S => Self::F32ConvertI32S,
            Instruction::F32ConvertI64S => Self::F32ConvertI64S,
            Instruction::F32ConvertI64U => Self::F32ConvertI64U,
            Instruction::F64ConvertI32S => Self::F64ConvertI32S,
            Instruction::F64ConvertI64S => Self::F64ConvertI64S,
            Instruction::F64ConvertI64U => Self::F64ConvertI64U,
            Instruction::F32DemoteF64 => Self::F32DemoteF64,
            Instruction::F64PromoteF32 => Self::F64PromoteF32,
            Instruction::I32ReinterpretF32 => Self::I32ReinterpretF32,
            Instruction::I64ReinterpretF64 => Self::I64ReinterpretF64,
            Instruction::F32ReinterpretI32 => Self::F32ReinterpretI32,
            Instruction::F64ReinterpretI64 => Self::F64ReinterpretI64,

            // Memory
            Instruction::I32Load(m) => Self::I32Load(m.into()),
            Instruction::I64Load(m) => Self::I64Load(m.into()),
            Instruction::F32Load(m) => Self::F32Load(m.into()),
            Instruction::F64Load(m) => Self::F64Load(m.into()),
            Instruction::I32Load8U(m) => Self::I32Load8U(m.into()),
            Instruction::I32Store(m) => Self::I32Store(m.into()),
            Instruction::I32Store8(m) => Self::I32Store8(m.into()),

            // Control
            Instruction::Block(bt) => Self::Block(bt),
            Instruction::Loop(bt) => Self::Loop(bt),
            Instruction::If(bt) => Self::If(bt),
            Instruction::Else => Self::Else,
            Instruction::End => Self::End,
            Instruction::Br(d) => Self::Br(d),
            Instruction::BrIf(d) => Self::BrIf(d),
            Instruction::BrTable(ref targets, default) => Self::BrTable {
                targets: targets.to_vec(),
                default,
            },
            Instruction::Return => Self::Return,
            Instruction::Unreachable => Self::Unreachable,

            // Call
            Instruction::Call(i) => Self::Call(i),
            Instruction::CallRef(i) => Self::CallRef(i),

            // GC: struct
            Instruction::StructNew(i) => Self::StructNew(i),
            Instruction::StructGet {
                struct_type_index,
                field_index,
            } => Self::StructGet {
                struct_type_index,
                field_index,
            },
            Instruction::StructSet {
                struct_type_index,
                field_index,
            } => Self::StructSet {
                struct_type_index,
                field_index,
            },

            // GC: array
            Instruction::ArrayNew(i) => Self::ArrayNew(i),
            Instruction::ArrayNewDefault(i) => Self::ArrayNewDefault(i),
            Instruction::ArrayNewData {
                array_type_index,
                array_data_index,
            } => Self::ArrayNewData {
                array_type_index,
                array_data_index,
            },
            Instruction::ArrayNewFixed {
                array_type_index,
                array_size,
            } => Self::ArrayNewFixed {
                array_type_index,
                array_size,
            },
            Instruction::ArrayGet(i) => Self::ArrayGet(i),
            Instruction::ArrayGetS(i) => Self::ArrayGetS(i),
            Instruction::ArrayGetU(i) => Self::ArrayGetU(i),
            Instruction::ArraySet(i) => Self::ArraySet(i),
            Instruction::ArrayLen => Self::ArrayLen,
            Instruction::ArrayCopy {
                array_type_index_dst,
                array_type_index_src,
            } => Self::ArrayCopy {
                array_type_index_dst,
                array_type_index_src,
            },
            Instruction::ArrayFill(i) => Self::ArrayFill(i),

            // GC: ref
            Instruction::RefNull(ht) => Self::RefNull(ht),
            Instruction::RefIsNull => Self::RefIsNull,
            Instruction::RefAsNonNull => Self::RefAsNonNull,
            Instruction::RefEq => Self::RefEq,
            Instruction::RefFunc(i) => Self::RefFunc(i),
            Instruction::RefCastNonNull(ht) => Self::RefCastNonNull(ht),
            Instruction::RefTestNonNull(ht) => Self::RefTestNonNull(ht),

            // Misc
            Instruction::Drop => Self::Drop,
            Instruction::TypedSelect(vt) => Self::TypedSelect(vt),

            _ => panic!("WirInstr: unsupported instruction"),
        }
    }
}

/// A function body represented as a `WirInstr` stream.
///
/// Provides the same `instruction(&Instruction)` API as `wasm_encoder::Function`
/// so that codegen can switch types with minimal changes.
pub struct WirFunction {
    local_decls: Vec<(u32, ValType)>,
    instrs: Vec<WirInstr>,
    /// Scratch function used only for `byte_len()` (branch-hint offsets).
    scratch: wasm_encoder::Function,
}

impl WirFunction {
    pub fn new(locals: Vec<(u32, ValType)>) -> Self {
        let scratch = wasm_encoder::Function::new(locals.clone());
        Self {
            local_decls: locals,
            instrs: Vec::new(),
            scratch,
        }
    }

    /// Record one instruction.  Same signature as `wasm_encoder::Function::instruction`.
    pub fn instruction(&mut self, instr: &Instruction<'_>) -> &mut Self {
        self.instrs.push(WirInstr::from_instruction(instr));
        self.scratch.instruction(instr);
        self
    }

    /// Current encoded byte length (used for branch-hint offset recording).
    pub fn byte_len(&self) -> usize {
        self.scratch.byte_len()
    }

    /// Consume self and produce a `wasm_encoder::Function` by replaying the
    /// `WirInstr` stream through `wir_emit`.
    pub fn emit(self) -> wasm_encoder::Function {
        let mut func = wasm_encoder::Function::new(self.local_decls);
        wir_emit::emit(&self.instrs, &mut func);
        func
    }
}
