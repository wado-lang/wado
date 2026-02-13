// WIR emitter — converts a WirInstr stream into wasm_encoder::Function instructions.

use wasm_encoder::{Function, Instruction};

use crate::wir::{WirInstr, WirMemArg};

/// Emit a slice of `WirInstr` into a `wasm_encoder::Function`.
pub fn emit(instrs: &[WirInstr], func: &mut Function) {
    for instr in instrs {
        emit_instr(instr, func);
    }
}

fn mem(m: &WirMemArg) -> wasm_encoder::MemArg {
    (*m).into()
}

fn emit_instr(instr: &WirInstr, func: &mut Function) {
    match *instr {
        // Constants
        WirInstr::I32Const(v) => {
            func.instruction(&Instruction::I32Const(v));
        }
        WirInstr::I64Const(v) => {
            func.instruction(&Instruction::I64Const(v));
        }
        WirInstr::F32Const(bits) => {
            func.instruction(&Instruction::F32Const(wasm_encoder::Ieee32::new(bits)));
        }
        WirInstr::F64Const(bits) => {
            func.instruction(&Instruction::F64Const(wasm_encoder::Ieee64::new(bits)));
        }

        // Locals / Globals
        WirInstr::LocalGet(i) => {
            func.instruction(&Instruction::LocalGet(i));
        }
        WirInstr::LocalSet(i) => {
            func.instruction(&Instruction::LocalSet(i));
        }
        WirInstr::LocalTee(i) => {
            func.instruction(&Instruction::LocalTee(i));
        }
        WirInstr::GlobalGet(i) => {
            func.instruction(&Instruction::GlobalGet(i));
        }
        WirInstr::GlobalSet(i) => {
            func.instruction(&Instruction::GlobalSet(i));
        }

        // i32 arithmetic
        WirInstr::I32Add => {
            func.instruction(&Instruction::I32Add);
        }
        WirInstr::I32Sub => {
            func.instruction(&Instruction::I32Sub);
        }
        WirInstr::I32Mul => {
            func.instruction(&Instruction::I32Mul);
        }
        WirInstr::I32DivS => {
            func.instruction(&Instruction::I32DivS);
        }
        WirInstr::I32DivU => {
            func.instruction(&Instruction::I32DivU);
        }
        WirInstr::I32RemS => {
            func.instruction(&Instruction::I32RemS);
        }
        WirInstr::I32RemU => {
            func.instruction(&Instruction::I32RemU);
        }
        WirInstr::I32And => {
            func.instruction(&Instruction::I32And);
        }
        WirInstr::I32Or => {
            func.instruction(&Instruction::I32Or);
        }
        WirInstr::I32Xor => {
            func.instruction(&Instruction::I32Xor);
        }
        WirInstr::I32Shl => {
            func.instruction(&Instruction::I32Shl);
        }
        WirInstr::I32ShrS => {
            func.instruction(&Instruction::I32ShrS);
        }
        WirInstr::I32ShrU => {
            func.instruction(&Instruction::I32ShrU);
        }
        WirInstr::I32Clz => {
            func.instruction(&Instruction::I32Clz);
        }
        WirInstr::I32Eq => {
            func.instruction(&Instruction::I32Eq);
        }
        WirInstr::I32Ne => {
            func.instruction(&Instruction::I32Ne);
        }
        WirInstr::I32Eqz => {
            func.instruction(&Instruction::I32Eqz);
        }
        WirInstr::I32LtS => {
            func.instruction(&Instruction::I32LtS);
        }
        WirInstr::I32LtU => {
            func.instruction(&Instruction::I32LtU);
        }
        WirInstr::I32GtS => {
            func.instruction(&Instruction::I32GtS);
        }
        WirInstr::I32GtU => {
            func.instruction(&Instruction::I32GtU);
        }
        WirInstr::I32LeS => {
            func.instruction(&Instruction::I32LeS);
        }
        WirInstr::I32LeU => {
            func.instruction(&Instruction::I32LeU);
        }
        WirInstr::I32GeS => {
            func.instruction(&Instruction::I32GeS);
        }
        WirInstr::I32GeU => {
            func.instruction(&Instruction::I32GeU);
        }

        // i64 arithmetic
        WirInstr::I64Add => {
            func.instruction(&Instruction::I64Add);
        }
        WirInstr::I64Sub => {
            func.instruction(&Instruction::I64Sub);
        }
        WirInstr::I64Mul => {
            func.instruction(&Instruction::I64Mul);
        }
        WirInstr::I64DivS => {
            func.instruction(&Instruction::I64DivS);
        }
        WirInstr::I64DivU => {
            func.instruction(&Instruction::I64DivU);
        }
        WirInstr::I64RemS => {
            func.instruction(&Instruction::I64RemS);
        }
        WirInstr::I64RemU => {
            func.instruction(&Instruction::I64RemU);
        }
        WirInstr::I64And => {
            func.instruction(&Instruction::I64And);
        }
        WirInstr::I64Or => {
            func.instruction(&Instruction::I64Or);
        }
        WirInstr::I64Xor => {
            func.instruction(&Instruction::I64Xor);
        }
        WirInstr::I64Shl => {
            func.instruction(&Instruction::I64Shl);
        }
        WirInstr::I64ShrS => {
            func.instruction(&Instruction::I64ShrS);
        }
        WirInstr::I64ShrU => {
            func.instruction(&Instruction::I64ShrU);
        }
        WirInstr::I64Clz => {
            func.instruction(&Instruction::I64Clz);
        }
        WirInstr::I64Eq => {
            func.instruction(&Instruction::I64Eq);
        }
        WirInstr::I64Ne => {
            func.instruction(&Instruction::I64Ne);
        }
        WirInstr::I64LtS => {
            func.instruction(&Instruction::I64LtS);
        }
        WirInstr::I64LtU => {
            func.instruction(&Instruction::I64LtU);
        }
        WirInstr::I64GtS => {
            func.instruction(&Instruction::I64GtS);
        }
        WirInstr::I64GtU => {
            func.instruction(&Instruction::I64GtU);
        }
        WirInstr::I64LeS => {
            func.instruction(&Instruction::I64LeS);
        }
        WirInstr::I64LeU => {
            func.instruction(&Instruction::I64LeU);
        }
        WirInstr::I64GeS => {
            func.instruction(&Instruction::I64GeS);
        }
        WirInstr::I64GeU => {
            func.instruction(&Instruction::I64GeU);
        }

        // Wide integer
        WirInstr::I64Add128 => {
            func.instruction(&Instruction::I64Add128);
        }
        WirInstr::I64Sub128 => {
            func.instruction(&Instruction::I64Sub128);
        }
        WirInstr::I64MulWideS => {
            func.instruction(&Instruction::I64MulWideS);
        }
        WirInstr::I64MulWideU => {
            func.instruction(&Instruction::I64MulWideU);
        }

        // f32 arithmetic
        WirInstr::F32Add => {
            func.instruction(&Instruction::F32Add);
        }
        WirInstr::F32Sub => {
            func.instruction(&Instruction::F32Sub);
        }
        WirInstr::F32Mul => {
            func.instruction(&Instruction::F32Mul);
        }
        WirInstr::F32Div => {
            func.instruction(&Instruction::F32Div);
        }
        WirInstr::F32Eq => {
            func.instruction(&Instruction::F32Eq);
        }
        WirInstr::F32Ne => {
            func.instruction(&Instruction::F32Ne);
        }
        WirInstr::F32Lt => {
            func.instruction(&Instruction::F32Lt);
        }
        WirInstr::F32Gt => {
            func.instruction(&Instruction::F32Gt);
        }
        WirInstr::F32Le => {
            func.instruction(&Instruction::F32Le);
        }
        WirInstr::F32Ge => {
            func.instruction(&Instruction::F32Ge);
        }
        WirInstr::F32Abs => {
            func.instruction(&Instruction::F32Abs);
        }
        WirInstr::F32Neg => {
            func.instruction(&Instruction::F32Neg);
        }
        WirInstr::F32Ceil => {
            func.instruction(&Instruction::F32Ceil);
        }
        WirInstr::F32Floor => {
            func.instruction(&Instruction::F32Floor);
        }
        WirInstr::F32Trunc => {
            func.instruction(&Instruction::F32Trunc);
        }
        WirInstr::F32Nearest => {
            func.instruction(&Instruction::F32Nearest);
        }
        WirInstr::F32Sqrt => {
            func.instruction(&Instruction::F32Sqrt);
        }
        WirInstr::F32Min => {
            func.instruction(&Instruction::F32Min);
        }
        WirInstr::F32Max => {
            func.instruction(&Instruction::F32Max);
        }
        WirInstr::F32Copysign => {
            func.instruction(&Instruction::F32Copysign);
        }

        // f64 arithmetic
        WirInstr::F64Add => {
            func.instruction(&Instruction::F64Add);
        }
        WirInstr::F64Sub => {
            func.instruction(&Instruction::F64Sub);
        }
        WirInstr::F64Mul => {
            func.instruction(&Instruction::F64Mul);
        }
        WirInstr::F64Div => {
            func.instruction(&Instruction::F64Div);
        }
        WirInstr::F64Eq => {
            func.instruction(&Instruction::F64Eq);
        }
        WirInstr::F64Ne => {
            func.instruction(&Instruction::F64Ne);
        }
        WirInstr::F64Lt => {
            func.instruction(&Instruction::F64Lt);
        }
        WirInstr::F64Gt => {
            func.instruction(&Instruction::F64Gt);
        }
        WirInstr::F64Le => {
            func.instruction(&Instruction::F64Le);
        }
        WirInstr::F64Ge => {
            func.instruction(&Instruction::F64Ge);
        }
        WirInstr::F64Abs => {
            func.instruction(&Instruction::F64Abs);
        }
        WirInstr::F64Neg => {
            func.instruction(&Instruction::F64Neg);
        }
        WirInstr::F64Ceil => {
            func.instruction(&Instruction::F64Ceil);
        }
        WirInstr::F64Floor => {
            func.instruction(&Instruction::F64Floor);
        }
        WirInstr::F64Trunc => {
            func.instruction(&Instruction::F64Trunc);
        }
        WirInstr::F64Nearest => {
            func.instruction(&Instruction::F64Nearest);
        }
        WirInstr::F64Sqrt => {
            func.instruction(&Instruction::F64Sqrt);
        }
        WirInstr::F64Min => {
            func.instruction(&Instruction::F64Min);
        }
        WirInstr::F64Max => {
            func.instruction(&Instruction::F64Max);
        }
        WirInstr::F64Copysign => {
            func.instruction(&Instruction::F64Copysign);
        }

        // Conversions
        WirInstr::I32WrapI64 => {
            func.instruction(&Instruction::I32WrapI64);
        }
        WirInstr::I64ExtendI32S => {
            func.instruction(&Instruction::I64ExtendI32S);
        }
        WirInstr::I64ExtendI32U => {
            func.instruction(&Instruction::I64ExtendI32U);
        }
        WirInstr::I32TruncF32S => {
            func.instruction(&Instruction::I32TruncF32S);
        }
        WirInstr::I32TruncF64S => {
            func.instruction(&Instruction::I32TruncF64S);
        }
        WirInstr::I64TruncF32S => {
            func.instruction(&Instruction::I64TruncF32S);
        }
        WirInstr::I64TruncF32U => {
            func.instruction(&Instruction::I64TruncF32U);
        }
        WirInstr::I64TruncF64S => {
            func.instruction(&Instruction::I64TruncF64S);
        }
        WirInstr::I64TruncF64U => {
            func.instruction(&Instruction::I64TruncF64U);
        }
        WirInstr::F32ConvertI32S => {
            func.instruction(&Instruction::F32ConvertI32S);
        }
        WirInstr::F32ConvertI64S => {
            func.instruction(&Instruction::F32ConvertI64S);
        }
        WirInstr::F32ConvertI64U => {
            func.instruction(&Instruction::F32ConvertI64U);
        }
        WirInstr::F64ConvertI32S => {
            func.instruction(&Instruction::F64ConvertI32S);
        }
        WirInstr::F64ConvertI64S => {
            func.instruction(&Instruction::F64ConvertI64S);
        }
        WirInstr::F64ConvertI64U => {
            func.instruction(&Instruction::F64ConvertI64U);
        }
        WirInstr::F32DemoteF64 => {
            func.instruction(&Instruction::F32DemoteF64);
        }
        WirInstr::F64PromoteF32 => {
            func.instruction(&Instruction::F64PromoteF32);
        }
        WirInstr::I32ReinterpretF32 => {
            func.instruction(&Instruction::I32ReinterpretF32);
        }
        WirInstr::I64ReinterpretF64 => {
            func.instruction(&Instruction::I64ReinterpretF64);
        }
        WirInstr::F32ReinterpretI32 => {
            func.instruction(&Instruction::F32ReinterpretI32);
        }
        WirInstr::F64ReinterpretI64 => {
            func.instruction(&Instruction::F64ReinterpretI64);
        }

        // Memory
        WirInstr::I32Load(ref m) => {
            func.instruction(&Instruction::I32Load(mem(m)));
        }
        WirInstr::I64Load(ref m) => {
            func.instruction(&Instruction::I64Load(mem(m)));
        }
        WirInstr::F32Load(ref m) => {
            func.instruction(&Instruction::F32Load(mem(m)));
        }
        WirInstr::F64Load(ref m) => {
            func.instruction(&Instruction::F64Load(mem(m)));
        }
        WirInstr::I32Load8U(ref m) => {
            func.instruction(&Instruction::I32Load8U(mem(m)));
        }
        WirInstr::I32Store(ref m) => {
            func.instruction(&Instruction::I32Store(mem(m)));
        }
        WirInstr::I32Store8(ref m) => {
            func.instruction(&Instruction::I32Store8(mem(m)));
        }

        // Control
        WirInstr::Block(bt) => {
            func.instruction(&Instruction::Block(bt));
        }
        WirInstr::Loop(bt) => {
            func.instruction(&Instruction::Loop(bt));
        }
        WirInstr::If(bt) => {
            func.instruction(&Instruction::If(bt));
        }
        WirInstr::Else => {
            func.instruction(&Instruction::Else);
        }
        WirInstr::End => {
            func.instruction(&Instruction::End);
        }
        WirInstr::Br(d) => {
            func.instruction(&Instruction::Br(d));
        }
        WirInstr::BrIf(d) => {
            func.instruction(&Instruction::BrIf(d));
        }
        WirInstr::BrTable {
            ref targets,
            default,
        } => {
            func.instruction(&Instruction::BrTable(targets.as_slice().into(), default));
        }
        WirInstr::Return => {
            func.instruction(&Instruction::Return);
        }
        WirInstr::Unreachable => {
            func.instruction(&Instruction::Unreachable);
        }

        // Call
        WirInstr::Call(i) => {
            func.instruction(&Instruction::Call(i));
        }
        WirInstr::CallRef(i) => {
            func.instruction(&Instruction::CallRef(i));
        }

        // GC: struct
        WirInstr::StructNew(i) => {
            func.instruction(&Instruction::StructNew(i));
        }
        WirInstr::StructGet {
            struct_type_index,
            field_index,
        } => {
            func.instruction(&Instruction::StructGet {
                struct_type_index,
                field_index,
            });
        }
        WirInstr::StructSet {
            struct_type_index,
            field_index,
        } => {
            func.instruction(&Instruction::StructSet {
                struct_type_index,
                field_index,
            });
        }

        // GC: array
        WirInstr::ArrayNew(i) => {
            func.instruction(&Instruction::ArrayNew(i));
        }
        WirInstr::ArrayNewDefault(i) => {
            func.instruction(&Instruction::ArrayNewDefault(i));
        }
        WirInstr::ArrayNewData {
            array_type_index,
            array_data_index,
        } => {
            func.instruction(&Instruction::ArrayNewData {
                array_type_index,
                array_data_index,
            });
        }
        WirInstr::ArrayNewFixed {
            array_type_index,
            array_size,
        } => {
            func.instruction(&Instruction::ArrayNewFixed {
                array_type_index,
                array_size,
            });
        }
        WirInstr::ArrayGet(i) => {
            func.instruction(&Instruction::ArrayGet(i));
        }
        WirInstr::ArrayGetS(i) => {
            func.instruction(&Instruction::ArrayGetS(i));
        }
        WirInstr::ArrayGetU(i) => {
            func.instruction(&Instruction::ArrayGetU(i));
        }
        WirInstr::ArraySet(i) => {
            func.instruction(&Instruction::ArraySet(i));
        }
        WirInstr::ArrayLen => {
            func.instruction(&Instruction::ArrayLen);
        }
        WirInstr::ArrayCopy {
            array_type_index_dst,
            array_type_index_src,
        } => {
            func.instruction(&Instruction::ArrayCopy {
                array_type_index_dst,
                array_type_index_src,
            });
        }
        WirInstr::ArrayFill(i) => {
            func.instruction(&Instruction::ArrayFill(i));
        }

        // GC: ref
        WirInstr::RefNull(ht) => {
            func.instruction(&Instruction::RefNull(ht));
        }
        WirInstr::RefIsNull => {
            func.instruction(&Instruction::RefIsNull);
        }
        WirInstr::RefAsNonNull => {
            func.instruction(&Instruction::RefAsNonNull);
        }
        WirInstr::RefEq => {
            func.instruction(&Instruction::RefEq);
        }
        WirInstr::RefFunc(i) => {
            func.instruction(&Instruction::RefFunc(i));
        }
        WirInstr::RefCastNonNull(ht) => {
            func.instruction(&Instruction::RefCastNonNull(ht));
        }
        WirInstr::RefTestNonNull(ht) => {
            func.instruction(&Instruction::RefTestNonNull(ht));
        }

        // Misc
        WirInstr::Drop => {
            func.instruction(&Instruction::Drop);
        }
        WirInstr::TypedSelect(vt) => {
            func.instruction(&Instruction::TypedSelect(vt));
        }
    }
}
