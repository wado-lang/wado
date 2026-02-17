//! WIR → pseudo-Wado unparser for `wado dump --wir --unparse`.
//!
//! Renders a `WirModule` as readable pseudo-Wado source code for debugging.
//! The output uses Wado syntax for type definitions (struct, variant, enum)
//! and WAT-style mnemonics for arithmetic instructions (i32.add, f64.mul, etc.).

use crate::wir::{
    WirAbstractHeapType, WirArrayType, WirEnumType, WirExport, WirExportDesc, WirField,
    WirFlagsType, WirFuncType, WirFunction, WirGlobal, WirImport, WirImportDesc, WirInstr,
    WirModule, WirStructType, WirType, WirTypeDef, WirVariantType,
};

/// Unparse a `WirModule` into pseudo-Wado source code.
pub fn unparse_wir(module: &WirModule) -> String {
    let mut unparser = WirUnparser::new();
    unparser.unparse(module);
    unparser.output
}

struct WirUnparser {
    output: String,
    indent: usize,
}

impl WirUnparser {
    fn new() -> Self {
        Self {
            output: String::new(),
            indent: 0,
        }
    }

    fn unparse(&mut self, module: &WirModule) {
        // Type definitions
        for type_def in &module.types {
            self.unparse_type_def(type_def);
            self.newline();
        }

        // Imports
        if !module.imports.is_empty() {
            for import in &module.imports {
                self.unparse_import(import);
            }
            self.newline();
        }

        // Globals
        for global in &module.globals {
            self.unparse_global(global);
        }
        if !module.globals.is_empty() {
            self.newline();
        }

        // Functions
        for func in &module.functions {
            self.unparse_function(func);
            self.newline();
        }

        // Exports
        if !module.exports.is_empty() {
            for export in &module.exports {
                self.unparse_export(export);
            }
        }
    }

    // === Type Definitions ===

    fn unparse_type_def(&mut self, type_def: &WirTypeDef) {
        match type_def {
            WirTypeDef::Struct(s) => self.unparse_struct_type(s),
            WirTypeDef::Variant(v) => self.unparse_variant_type(v),
            WirTypeDef::Enum(e) => self.unparse_enum_type(e),
            WirTypeDef::Flags(f) => self.unparse_flags_type(f),
            WirTypeDef::Array(a) => self.unparse_array_type(a),
            WirTypeDef::Func(f) => self.unparse_func_type(f),
        }
    }

    fn unparse_struct_type(&mut self, s: &WirStructType) {
        self.write_indent();
        self.write("struct ");
        self.write(&s.name.display);

        if let Some(ref origin) = s.generic_origin {
            self.write("<");
            self.write(&origin.type_args.join(", "));
            self.write(">");
        }

        self.write(" {");

        if s.fields.is_empty() {
            self.write(" }");
        } else {
            self.write(" ");
            for (i, field) in s.fields.iter().enumerate() {
                if i > 0 {
                    self.write(", ");
                }
                self.unparse_field(field);
            }
            self.write(" }");
        }

        self.unparse_source_comment(&s.meta);

        if let Some(ref origin) = s.generic_origin {
            self.write(&format!(
                "  // {}<T> with T={}",
                origin.base_name,
                origin.type_args.join(", ")
            ));
        }

        if let Some(ref newtype) = s.newtype_origin {
            self.write(&format!(
                "  // newtype {} from {}",
                newtype.name, newtype.module_source
            ));
        }

        self.newline();
    }

    fn unparse_field(&mut self, field: &WirField) {
        if field.mutable {
            self.write("mut ");
        }
        self.write(&field.name);
        self.write(": ");
        self.write(&format_type(&field.ty));
    }

    fn unparse_variant_type(&mut self, v: &WirVariantType) {
        self.write_indent();
        self.write("variant ");
        self.write(&v.name.display);
        self.write(" {");
        self.unparse_source_comment(&v.meta);
        self.newline();

        self.indent += 1;
        for case in &v.cases {
            self.write_indent();
            self.write(&case.name);
            if !case.payload.is_empty() {
                self.write("(");
                for (i, ty) in case.payload.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&format_type(ty));
                }
                self.write(")");
            }
            self.write(",");
            self.newline();
        }
        self.indent -= 1;

        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn unparse_enum_type(&mut self, e: &WirEnumType) {
        self.write_indent();
        self.write("enum ");
        self.write(&e.name.display);
        self.write(" { ");

        for (i, case) in e.cases.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&case.name);
            self.write(" = ");
            self.write(&case.discriminant.to_string());
        }

        self.write(" }");
        self.unparse_source_comment(&e.meta);
        self.newline();
    }

    fn unparse_flags_type(&mut self, f: &WirFlagsType) {
        self.write_indent();
        self.write("flags ");
        self.write(&f.name.display);
        self.write(" { ");

        for (i, bit) in f.bits.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&bit.name);
            self.write(" = ");
            self.write(&format!("bit{}", bit.position));
        }

        self.write(" }");
        self.unparse_source_comment(&f.meta);
        self.newline();
    }

    fn unparse_array_type(&mut self, a: &WirArrayType) {
        self.write_indent();
        self.write("array ");
        self.write(&a.name.display);
        if a.mutable {
            self.write(" (mut ");
        } else {
            self.write(" (");
        }
        self.write(&format_type(&a.element_type));
        self.write(")");
        self.unparse_source_comment(&a.meta);
        self.newline();
    }

    fn unparse_func_type(&mut self, f: &WirFuncType) {
        self.write_indent();
        self.write("type ");
        self.write(&f.name.display);
        self.write(" = fn(");
        for (i, param) in f.params.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&format_type(param));
        }
        self.write(")");
        if !f.results.is_empty() {
            self.write(" -> ");
            if f.results.len() == 1 {
                self.write(&format_type(&f.results[0]));
            } else {
                self.write("[");
                for (i, r) in f.results.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&format_type(r));
                }
                self.write("]");
            }
        }
        self.newline();
    }

    // === Imports ===

    fn unparse_import(&mut self, import: &WirImport) {
        self.write_indent();
        self.write("import ");
        match &import.desc {
            WirImportDesc::Func { name, .. } => {
                self.write("fn ");
                self.write(&name.display);
            }
            WirImportDesc::Global { ty, mutable } => {
                self.write("global ");
                if *mutable {
                    self.write("mut ");
                }
                self.write(&format_type(ty));
            }
            WirImportDesc::Memory { min, max } => {
                self.write(&format!("memory ({min}"));
                if let Some(max) = max {
                    self.write(&format!(", {max}"));
                }
                self.write(")");
            }
            WirImportDesc::Table { ty, min, max } => {
                self.write(&format!("table ({min}"));
                if let Some(max) = max {
                    self.write(&format!(", {max}"));
                }
                self.write(&format!(") {}", format_type(ty)));
            }
        }
        self.write(&format!(" from \"{}/{}\"", import.module, import.field));
        self.newline();
    }

    // === Globals ===

    fn unparse_global(&mut self, global: &WirGlobal) {
        self.write_indent();
        self.write("global ");
        if global.mutable {
            self.write("mut ");
        }
        self.write(&global.name.display);
        self.write(": ");
        self.write(&format_type(&global.ty));
        self.write(" = ");
        self.unparse_instr_inline(&global.init);
        self.write(";");
        self.unparse_source_comment(&global.meta);
        self.newline();
    }

    // === Functions ===

    fn unparse_function(&mut self, func: &WirFunction) {
        self.write_indent();
        self.write("fn \"");
        self.write(&func.name.display);
        self.write("\"(");

        // We need the function type to get parameter types
        // For now, just output parameter names
        for (i, name) in func.param_names.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(name);
        }
        self.write(")");

        if !func.effects.is_empty() {
            self.write(" with ");
            self.write(&func.effects.join(", "));
        }

        self.write(" {");
        self.unparse_source_comment(&func.meta);
        self.newline();

        if let Some(ref body) = func.body {
            self.indent += 1;
            for instr in body {
                self.unparse_instr(instr);
            }
            self.indent -= 1;
        }

        self.write_indent();
        self.write("}");
        self.newline();
    }

    // === Instructions ===

    fn unparse_instr(&mut self, instr: &WirInstr) {
        self.write_indent();
        self.unparse_instr_inline(instr);
        self.write(";");
        self.newline();
    }

    fn unparse_instr_inline(&mut self, instr: &WirInstr) {
        match instr {
            // Locals
            WirInstr::DeclareLocal { name, ty } => {
                self.write(&format!("let {name}: {}", format_type(ty)));
            }
            WirInstr::LocalGet { name } => {
                self.write(name);
            }
            WirInstr::LocalSet { name, value } => {
                self.write(name);
                self.write(" = ");
                self.unparse_instr_inline(value);
            }
            WirInstr::LocalTee { name, value } => {
                self.write(&format!("local.tee {name}("));
                self.unparse_instr_inline(value);
                self.write(")");
            }

            // Globals
            WirInstr::GlobalGet { name } => {
                self.write(&name.display);
            }
            WirInstr::GlobalSet { name, value } => {
                self.write(&name.display);
                self.write(" = ");
                self.unparse_instr_inline(value);
            }

            // Constants
            WirInstr::I32Const(v) => self.write(&v.to_string()),
            WirInstr::I64Const(v) => self.write(&format!("{v}_i64")),
            WirInstr::F32Const(v) => self.write(&format!("{v}_f32")),
            WirInstr::F64Const(v) => self.write(&v.to_string()),

            // i32 binary ops
            WirInstr::I32Add(a, b) => self.write_binop("i32.add", a, b),
            WirInstr::I32Sub(a, b) => self.write_binop("i32.sub", a, b),
            WirInstr::I32Mul(a, b) => self.write_binop("i32.mul", a, b),
            WirInstr::I32DivS(a, b) => self.write_binop("i32.div_s", a, b),
            WirInstr::I32DivU(a, b) => self.write_binop("i32.div_u", a, b),
            WirInstr::I32RemS(a, b) => self.write_binop("i32.rem_s", a, b),
            WirInstr::I32RemU(a, b) => self.write_binop("i32.rem_u", a, b),
            WirInstr::I32And(a, b) => self.write_binop("i32.and", a, b),
            WirInstr::I32Or(a, b) => self.write_binop("i32.or", a, b),
            WirInstr::I32Xor(a, b) => self.write_binop("i32.xor", a, b),
            WirInstr::I32Shl(a, b) => self.write_binop("i32.shl", a, b),
            WirInstr::I32ShrS(a, b) => self.write_binop("i32.shr_s", a, b),
            WirInstr::I32ShrU(a, b) => self.write_binop("i32.shr_u", a, b),
            WirInstr::I32Eq(a, b) => self.write_binop("i32.eq", a, b),
            WirInstr::I32Ne(a, b) => self.write_binop("i32.ne", a, b),
            WirInstr::I32LtS(a, b) => self.write_binop("i32.lt_s", a, b),
            WirInstr::I32LtU(a, b) => self.write_binop("i32.lt_u", a, b),
            WirInstr::I32GtS(a, b) => self.write_binop("i32.gt_s", a, b),
            WirInstr::I32GtU(a, b) => self.write_binop("i32.gt_u", a, b),
            WirInstr::I32LeS(a, b) => self.write_binop("i32.le_s", a, b),
            WirInstr::I32LeU(a, b) => self.write_binop("i32.le_u", a, b),
            WirInstr::I32GeS(a, b) => self.write_binop("i32.ge_s", a, b),
            WirInstr::I32GeU(a, b) => self.write_binop("i32.ge_u", a, b),

            // i32 unary ops
            WirInstr::I32Eqz(a) => self.write_unop("i32.eqz", a),
            WirInstr::I32WrapI64(a) => self.write_unop("i32.wrap_i64", a),
            WirInstr::I32Clz(a) => self.write_unop("i32.clz", a),
            WirInstr::I32Ctz(a) => self.write_unop("i32.ctz", a),
            WirInstr::I32Popcnt(a) => self.write_unop("i32.popcnt", a),
            WirInstr::I32TruncF64S(a) => self.write_unop("i32.trunc_f64_s", a),
            WirInstr::I32TruncF64U(a) => self.write_unop("i32.trunc_f64_u", a),
            WirInstr::I32TruncF32S(a) => self.write_unop("i32.trunc_f32_s", a),
            WirInstr::I32TruncF32U(a) => self.write_unop("i32.trunc_f32_u", a),
            WirInstr::I32ReinterpretF32(a) => self.write_unop("i32.reinterpret_f32", a),
            WirInstr::I32Extend8S(a) => self.write_unop("i32.extend8_s", a),
            WirInstr::I32Extend16S(a) => self.write_unop("i32.extend16_s", a),

            // i64 binary ops
            WirInstr::I64Add(a, b) => self.write_binop("i64.add", a, b),
            WirInstr::I64Sub(a, b) => self.write_binop("i64.sub", a, b),
            WirInstr::I64Mul(a, b) => self.write_binop("i64.mul", a, b),
            WirInstr::I64DivS(a, b) => self.write_binop("i64.div_s", a, b),
            WirInstr::I64DivU(a, b) => self.write_binop("i64.div_u", a, b),
            WirInstr::I64RemS(a, b) => self.write_binop("i64.rem_s", a, b),
            WirInstr::I64RemU(a, b) => self.write_binop("i64.rem_u", a, b),
            WirInstr::I64And(a, b) => self.write_binop("i64.and", a, b),
            WirInstr::I64Or(a, b) => self.write_binop("i64.or", a, b),
            WirInstr::I64Xor(a, b) => self.write_binop("i64.xor", a, b),
            WirInstr::I64Shl(a, b) => self.write_binop("i64.shl", a, b),
            WirInstr::I64ShrS(a, b) => self.write_binop("i64.shr_s", a, b),
            WirInstr::I64ShrU(a, b) => self.write_binop("i64.shr_u", a, b),
            WirInstr::I64Eq(a, b) => self.write_binop("i64.eq", a, b),
            WirInstr::I64Ne(a, b) => self.write_binop("i64.ne", a, b),
            WirInstr::I64LtS(a, b) => self.write_binop("i64.lt_s", a, b),
            WirInstr::I64LtU(a, b) => self.write_binop("i64.lt_u", a, b),
            WirInstr::I64GtS(a, b) => self.write_binop("i64.gt_s", a, b),
            WirInstr::I64GtU(a, b) => self.write_binop("i64.gt_u", a, b),
            WirInstr::I64LeS(a, b) => self.write_binop("i64.le_s", a, b),
            WirInstr::I64LeU(a, b) => self.write_binop("i64.le_u", a, b),
            WirInstr::I64GeS(a, b) => self.write_binop("i64.ge_s", a, b),
            WirInstr::I64GeU(a, b) => self.write_binop("i64.ge_u", a, b),

            // i64 unary ops
            WirInstr::I64Eqz(a) => self.write_unop("i64.eqz", a),
            WirInstr::I64ExtendI32S(a) => self.write_unop("i64.extend_i32_s", a),
            WirInstr::I64ExtendI32U(a) => self.write_unop("i64.extend_i32_u", a),
            WirInstr::I64Clz(a) => self.write_unop("i64.clz", a),
            WirInstr::I64Ctz(a) => self.write_unop("i64.ctz", a),
            WirInstr::I64Popcnt(a) => self.write_unop("i64.popcnt", a),
            WirInstr::I64TruncF64S(a) => self.write_unop("i64.trunc_f64_s", a),
            WirInstr::I64TruncF64U(a) => self.write_unop("i64.trunc_f64_u", a),
            WirInstr::I64TruncF32S(a) => self.write_unop("i64.trunc_f32_s", a),
            WirInstr::I64TruncF32U(a) => self.write_unop("i64.trunc_f32_u", a),
            WirInstr::I64ReinterpretF64(a) => self.write_unop("i64.reinterpret_f64", a),

            // i128 ops
            WirInstr::I64Add128(a, b, c, d) => {
                self.write("i64.add128(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(", ");
                self.unparse_instr_inline(d);
                self.write(")");
            }
            WirInstr::I64Sub128(a, b, c, d) => {
                self.write("i64.sub128(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(", ");
                self.unparse_instr_inline(d);
                self.write(")");
            }
            WirInstr::I64MulWideU(a, b) => self.write_binop("i64.mul_wide_u", a, b),
            WirInstr::I64MulWideS(a, b) => self.write_binop("i64.mul_wide_s", a, b),

            // f32 binary ops
            WirInstr::F32Add(a, b) => self.write_binop("f32.add", a, b),
            WirInstr::F32Sub(a, b) => self.write_binop("f32.sub", a, b),
            WirInstr::F32Mul(a, b) => self.write_binop("f32.mul", a, b),
            WirInstr::F32Div(a, b) => self.write_binop("f32.div", a, b),
            WirInstr::F32Min(a, b) => self.write_binop("f32.min", a, b),
            WirInstr::F32Max(a, b) => self.write_binop("f32.max", a, b),
            WirInstr::F32Copysign(a, b) => self.write_binop("f32.copysign", a, b),
            WirInstr::F32Eq(a, b) => self.write_binop("f32.eq", a, b),
            WirInstr::F32Ne(a, b) => self.write_binop("f32.ne", a, b),
            WirInstr::F32Lt(a, b) => self.write_binop("f32.lt", a, b),
            WirInstr::F32Gt(a, b) => self.write_binop("f32.gt", a, b),
            WirInstr::F32Le(a, b) => self.write_binop("f32.le", a, b),
            WirInstr::F32Ge(a, b) => self.write_binop("f32.ge", a, b),

            // f32 unary ops
            WirInstr::F32Neg(a) => self.write_unop("f32.neg", a),
            WirInstr::F32Abs(a) => self.write_unop("f32.abs", a),
            WirInstr::F32Ceil(a) => self.write_unop("f32.ceil", a),
            WirInstr::F32Floor(a) => self.write_unop("f32.floor", a),
            WirInstr::F32Trunc(a) => self.write_unop("f32.trunc", a),
            WirInstr::F32Nearest(a) => self.write_unop("f32.nearest", a),
            WirInstr::F32Sqrt(a) => self.write_unop("f32.sqrt", a),
            WirInstr::F32ConvertI32S(a) => self.write_unop("f32.convert_i32_s", a),
            WirInstr::F32ConvertI32U(a) => self.write_unop("f32.convert_i32_u", a),
            WirInstr::F32ConvertI64S(a) => self.write_unop("f32.convert_i64_s", a),
            WirInstr::F32ConvertI64U(a) => self.write_unop("f32.convert_i64_u", a),
            WirInstr::F32DemoteF64(a) => self.write_unop("f32.demote_f64", a),
            WirInstr::F32ReinterpretI32(a) => self.write_unop("f32.reinterpret_i32", a),

            // f64 binary ops
            WirInstr::F64Add(a, b) => self.write_binop("f64.add", a, b),
            WirInstr::F64Sub(a, b) => self.write_binop("f64.sub", a, b),
            WirInstr::F64Mul(a, b) => self.write_binop("f64.mul", a, b),
            WirInstr::F64Div(a, b) => self.write_binop("f64.div", a, b),
            WirInstr::F64Min(a, b) => self.write_binop("f64.min", a, b),
            WirInstr::F64Max(a, b) => self.write_binop("f64.max", a, b),
            WirInstr::F64Copysign(a, b) => self.write_binop("f64.copysign", a, b),
            WirInstr::F64Eq(a, b) => self.write_binop("f64.eq", a, b),
            WirInstr::F64Ne(a, b) => self.write_binop("f64.ne", a, b),
            WirInstr::F64Lt(a, b) => self.write_binop("f64.lt", a, b),
            WirInstr::F64Gt(a, b) => self.write_binop("f64.gt", a, b),
            WirInstr::F64Le(a, b) => self.write_binop("f64.le", a, b),
            WirInstr::F64Ge(a, b) => self.write_binop("f64.ge", a, b),

            // f64 unary ops
            WirInstr::F64Neg(a) => self.write_unop("f64.neg", a),
            WirInstr::F64Abs(a) => self.write_unop("f64.abs", a),
            WirInstr::F64Ceil(a) => self.write_unop("f64.ceil", a),
            WirInstr::F64Floor(a) => self.write_unop("f64.floor", a),
            WirInstr::F64Trunc(a) => self.write_unop("f64.trunc", a),
            WirInstr::F64Nearest(a) => self.write_unop("f64.nearest", a),
            WirInstr::F64Sqrt(a) => self.write_unop("f64.sqrt", a),
            WirInstr::F64ConvertI32S(a) => self.write_unop("f64.convert_i32_s", a),
            WirInstr::F64ConvertI32U(a) => self.write_unop("f64.convert_i32_u", a),
            WirInstr::F64ConvertI64S(a) => self.write_unop("f64.convert_i64_s", a),
            WirInstr::F64ConvertI64U(a) => self.write_unop("f64.convert_i64_u", a),
            WirInstr::F64PromoteF32(a) => self.write_unop("f64.promote_f32", a),
            WirInstr::F64ReinterpretI64(a) => self.write_unop("f64.reinterpret_i64", a),

            // GC: Struct
            WirInstr::StructNew { type_id, fields } => {
                self.write(&format!("struct.new {type_id}("));
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.unparse_instr_inline(f);
                }
                self.write(")");
            }
            WirInstr::StructGet {
                field_name, expr, ..
            } => {
                self.unparse_instr_inline(expr);
                self.write(".");
                self.write(field_name);
            }
            WirInstr::StructSet {
                field_name,
                expr,
                value,
                ..
            } => {
                self.unparse_instr_inline(expr);
                self.write(".");
                self.write(field_name);
                self.write(" = ");
                self.unparse_instr_inline(value);
            }

            // GC: Array
            WirInstr::ArrayNew { type_id, init, len } => {
                self.write(&format!("array.new {type_id}("));
                self.unparse_instr_inline(init);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayNewDefault { type_id, len } => {
                self.write(&format!("array.new_default {type_id}("));
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayNewData {
                type_id,
                data_index,
                offset,
                len,
            } => {
                self.write(&format!("array.new_data {type_id} {data_index}("));
                self.unparse_instr_inline(offset);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayNewFixed { type_id, elements } => {
                self.write(&format!("array.new_fixed {type_id}("));
                for (i, e) in elements.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.unparse_instr_inline(e);
                }
                self.write(")");
            }
            WirInstr::ArrayGet {
                type_id,
                array,
                index,
            } => {
                self.write(&format!("array.get {type_id}("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::ArrayGetS {
                type_id,
                array,
                index,
            } => {
                self.write(&format!("array.get_s {type_id}("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::ArrayGetU {
                type_id,
                array,
                index,
            } => {
                self.write(&format!("array.get_u {type_id}("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::ArraySet {
                type_id,
                array,
                index,
                value,
            } => {
                self.write(&format!("array.set {type_id}("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }
            WirInstr::ArrayLen(a) => self.write_unop("array.len", a),
            WirInstr::ArrayCopy {
                dest_type_id,
                src_type_id,
                dest,
                dest_offset,
                src,
                src_offset,
                len,
            } => {
                self.write(&format!("array.copy {dest_type_id} {src_type_id}("));
                self.unparse_instr_inline(dest);
                self.write(", ");
                self.unparse_instr_inline(dest_offset);
                self.write(", ");
                self.unparse_instr_inline(src);
                self.write(", ");
                self.unparse_instr_inline(src_offset);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayFill {
                type_id,
                array,
                offset,
                value,
                len,
            } => {
                self.write(&format!("array.fill {type_id}("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(offset);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }

            // GC: Reference
            WirInstr::RefNull { heap_type } => {
                self.write(&format!(
                    "ref.null {}",
                    format_abstract_heap_type(heap_type)
                ));
            }
            WirInstr::RefIsNull(a) => self.write_unop("ref.is_null", a),
            WirInstr::RefAsNonNull(a) => self.write_unop("ref.as_non_null", a),
            WirInstr::RefCast {
                type_id,
                nullable,
                expr,
            } => {
                let null_str = if *nullable { " null" } else { "" };
                self.write(&format!("ref.cast{null_str} {type_id}("));
                self.unparse_instr_inline(expr);
                self.write(")");
            }
            WirInstr::RefTest {
                type_id,
                nullable,
                expr,
            } => {
                let null_str = if *nullable { " null" } else { "" };
                self.write(&format!("ref.test{null_str} {type_id}("));
                self.unparse_instr_inline(expr);
                self.write(")");
            }
            WirInstr::RefEq(a, b) => self.write_binop("ref.eq", a, b),
            WirInstr::RefI31(a) => self.write_unop("ref.i31", a),
            WirInstr::I31GetS(a) => self.write_unop("i31.get_s", a),
            WirInstr::I31GetU(a) => self.write_unop("i31.get_u", a),
            WirInstr::ExternInternalize(a) => self.write_unop("extern.internalize", a),
            WirInstr::ExternExternalize(a) => self.write_unop("extern.externalize", a),

            // Control Flow
            WirInstr::Block {
                label,
                result,
                body,
            } => {
                if let Some(label) = label {
                    self.write(&format!("{label}: "));
                }
                self.write("block");
                if let Some(ty) = result {
                    self.write(&format!(" -> {}", format_type(ty)));
                }
                self.write(" {");
                self.newline();
                self.indent += 1;
                for instr in body {
                    self.unparse_instr(instr);
                }
                self.indent -= 1;
                self.write_indent();
                self.write("}");
            }
            WirInstr::Loop { label, body } => {
                if let Some(label) = label {
                    self.write(&format!("{label}: "));
                }
                self.write("loop {");
                self.newline();
                self.indent += 1;
                for instr in body {
                    self.unparse_instr(instr);
                }
                self.indent -= 1;
                self.write_indent();
                self.write("}");
            }
            WirInstr::If {
                condition,
                result,
                then_body,
                else_body,
            } => {
                self.write("if ");
                self.unparse_instr_inline(condition);
                if let Some(ty) = result {
                    self.write(&format!(" -> {}", format_type(ty)));
                }
                self.write(" {");
                self.newline();
                self.indent += 1;
                for instr in then_body {
                    self.unparse_instr(instr);
                }
                self.indent -= 1;
                if let Some(else_body) = else_body {
                    self.write_indent();
                    self.write("} else {");
                    self.newline();
                    self.indent += 1;
                    for instr in else_body {
                        self.unparse_instr(instr);
                    }
                    self.indent -= 1;
                }
                self.write_indent();
                self.write("}");
            }
            WirInstr::Br { depth } => {
                self.write(&format!("br {depth}"));
            }
            WirInstr::BrIf { depth, condition } => {
                self.write(&format!("br_if {depth} "));
                self.unparse_instr_inline(condition);
            }
            WirInstr::BrTable {
                index,
                targets,
                default,
            } => {
                self.write("br_table ");
                self.unparse_instr_inline(index);
                self.write(" [");
                for (i, t) in targets.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&t.to_string());
                }
                self.write(&format!("] default={default}"));
            }
            WirInstr::Return { value } => {
                self.write("return");
                if let Some(v) = value {
                    self.write(" ");
                    self.unparse_instr_inline(v);
                }
            }
            WirInstr::Unreachable => self.write("unreachable"),
            WirInstr::Nop => self.write("nop"),
            WirInstr::Drop(a) => self.write_unop("drop", a),
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ty,
            } => {
                self.write("select");
                if let Some(ty) = ty {
                    self.write(&format!(" {}", format_type(ty)));
                }
                self.write("(");
                self.unparse_instr_inline(condition);
                self.write(", ");
                self.unparse_instr_inline(if_true);
                self.write(", ");
                self.unparse_instr_inline(if_false);
                self.write(")");
            }

            // Calls
            WirInstr::Call { func_id, args } => {
                self.write(&format!("call {func_id}("));
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.unparse_instr_inline(a);
                }
                self.write(")");
            }
            WirInstr::CallIndirect {
                type_id,
                table,
                index,
                args,
            } => {
                self.write(&format!("call_indirect {type_id} table={table}("));
                self.unparse_instr_inline(index);
                for a in args {
                    self.write(", ");
                    self.unparse_instr_inline(a);
                }
                self.write(")");
            }
            WirInstr::CallRef {
                type_id,
                func_ref,
                args,
            } => {
                self.write(&format!("call_ref {type_id}("));
                self.unparse_instr_inline(func_ref);
                for a in args {
                    self.write(", ");
                    self.unparse_instr_inline(a);
                }
                self.write(")");
            }
            WirInstr::RefFunc { func_id } => {
                self.write(&format!("ref.func {func_id}"));
            }

            // Memory
            WirInstr::MemorySize => self.write("memory.size"),
            WirInstr::MemoryGrow(a) => self.write_unop("memory.grow", a),
            WirInstr::I32Load { offset, addr, .. } => {
                self.write(&format!("i32.load offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(")");
            }
            WirInstr::I32Load8U { offset, addr, .. } => {
                self.write(&format!("i32.load8_u offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(")");
            }
            WirInstr::I32Load8S { offset, addr, .. } => {
                self.write(&format!("i32.load8_s offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(")");
            }
            WirInstr::I32Load16U { offset, addr, .. } => {
                self.write(&format!("i32.load16_u offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(")");
            }
            WirInstr::I32Load16S { offset, addr, .. } => {
                self.write(&format!("i32.load16_s offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(")");
            }
            WirInstr::I32Store {
                offset,
                addr,
                value,
                ..
            } => {
                self.write(&format!("i32.store offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }
            WirInstr::I32Store8 {
                offset,
                addr,
                value,
                ..
            } => {
                self.write(&format!("i32.store8 offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }
            WirInstr::I32Store16 {
                offset,
                addr,
                value,
                ..
            } => {
                self.write(&format!("i32.store16 offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }
            WirInstr::I64Load { offset, addr, .. } => {
                self.write(&format!("i64.load offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(")");
            }
            WirInstr::I64Store {
                offset,
                addr,
                value,
                ..
            } => {
                self.write(&format!("i64.store offset={offset}("));
                self.unparse_instr_inline(addr);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }

            // Table
            WirInstr::TableGet { table, index } => {
                self.write(&format!("table.get {table}("));
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::TableSet {
                table,
                index,
                value,
            } => {
                self.write(&format!("table.set {table}("));
                self.unparse_instr_inline(index);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }

            // Compound
            WirInstr::ValueCopy { type_id, expr, .. } => {
                self.write(&format!("value_copy {type_id}("));
                self.unparse_instr_inline(expr);
                self.write(")");
            }

            WirInstr::Seq(instrs) => {
                for (i, instr) in instrs.iter().enumerate() {
                    if i > 0 {
                        self.write("; ");
                    }
                    self.unparse_instr_inline(instr);
                }
            }
        }
    }

    // === Exports ===

    fn unparse_export(&mut self, export: &WirExport) {
        self.write_indent();
        self.write("export ");
        match &export.desc {
            WirExportDesc::Func { func_id } => {
                self.write(&format!("fn {func_id}"));
            }
            WirExportDesc::Global { name } => {
                self.write(&format!("global {}", name.display));
            }
            WirExportDesc::Memory => {
                self.write("memory");
            }
            WirExportDesc::Table { index } => {
                self.write(&format!("table {index}"));
            }
        }
        self.write(&format!(" as \"{}\"", export.name));
        self.newline();
    }

    // === Helpers ===

    fn write_binop(&mut self, name: &str, a: &WirInstr, b: &WirInstr) {
        self.write(name);
        self.write("(");
        self.unparse_instr_inline(a);
        self.write(", ");
        self.unparse_instr_inline(b);
        self.write(")");
    }

    fn write_unop(&mut self, name: &str, a: &WirInstr) {
        self.write(name);
        self.write("(");
        self.unparse_instr_inline(a);
        self.write(")");
    }

    fn unparse_source_comment(&mut self, meta: &crate::wir::WirMeta) {
        if let Some(ref source) = meta.module_source {
            self.write(&format!("  // from {source}"));
        }
    }

    fn write(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.push_str("    ");
        }
    }

    fn newline(&mut self) {
        self.output.push('\n');
    }
}

/// Format a `WirType` as a string for display.
pub fn format_type(ty: &WirType) -> String {
    match ty {
        WirType::I8 => "i8".to_string(),
        WirType::I16 => "i16".to_string(),
        WirType::I32 => "i32".to_string(),
        WirType::I64 => "i64".to_string(),
        WirType::U8 => "u8".to_string(),
        WirType::U16 => "u16".to_string(),
        WirType::U32 => "u32".to_string(),
        WirType::U64 => "u64".to_string(),
        WirType::F32 => "f32".to_string(),
        WirType::F64 => "f64".to_string(),
        WirType::Bool => "bool".to_string(),
        WirType::Char => "char".to_string(),
        WirType::Unit => "()".to_string(),
        WirType::Enum { type_id } => type_id.to_string(),
        WirType::Flags { type_id } => type_id.to_string(),
        WirType::Ref { type_id, nullable } => {
            if *nullable {
                format!("ref null {type_id}")
            } else {
                format!("ref {type_id}")
            }
        }
        WirType::AbstractRef {
            heap_type,
            nullable,
        } => {
            let ht = format_abstract_heap_type(heap_type);
            if *nullable {
                format!("ref null {ht}")
            } else {
                format!("ref {ht}")
            }
        }
    }
}

fn format_abstract_heap_type(ht: &WirAbstractHeapType) -> &'static str {
    match ht {
        WirAbstractHeapType::Any => "any",
        WirAbstractHeapType::Eq => "eq",
        WirAbstractHeapType::Struct => "struct",
        WirAbstractHeapType::Array => "array",
        WirAbstractHeapType::Func => "func",
        WirAbstractHeapType::None => "none",
        WirAbstractHeapType::NoFunc => "nofunc",
        WirAbstractHeapType::Extern => "extern",
    }
}
