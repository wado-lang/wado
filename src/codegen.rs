// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder

use crate::ast::{Block, CallExpr, Expr, Item, Literal, Stmt};
use wasm_encoder::{
    CanonicalOption, CodeSection, ComponentBuilder, ComponentExportKind, ComponentTypeRef,
    ConstExpr, DataSection, DataSegment, DataSegmentMode, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, ImportSection, Instruction, MemorySection, MemoryType, Module,
    ModuleArg, PrimitiveValType, TypeBounds, TypeSection, ValType,
};

/// Code generator that produces Component Model components
pub struct Codegen {
    string_literals: Vec<String>,
}

impl Default for Codegen {
    fn default() -> Self {
        Self::new()
    }
}

impl Codegen {
    pub fn new() -> Self {
        Self {
            string_literals: Vec::new(),
        }
    }

    /// Generate Component Model binary Wasm
    pub fn generate(&mut self, module: &crate::ast::Module) -> Vec<u8> {
        // First pass: collect string literals
        self.collect_strings(module);

        // Generate binary Wasm
        self.generate_component(module)
    }

    /// Generate WAT text format (for debugging)
    pub fn generate_wat(&mut self, module: &crate::ast::Module) -> String {
        let wasm = self.generate(module);
        wasmprinter::print_bytes(&wasm).unwrap_or_else(|e| format!("Error: {}", e))
    }

    fn collect_strings(&mut self, module: &crate::ast::Module) {
        for item in &module.items {
            if let Item::Function(func) = item {
                self.collect_strings_from_block(&func.body);
            }
        }
    }

    fn collect_strings_from_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.collect_strings_from_stmt(stmt);
        }
    }

    fn collect_strings_from_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Expr(expr_stmt) => self.collect_strings_from_expr(&expr_stmt.expr),
            Stmt::Let(let_stmt) => self.collect_strings_from_expr(&let_stmt.value),
            Stmt::Return(ret_stmt) => {
                if let Some(expr) = &ret_stmt.value {
                    self.collect_strings_from_expr(expr);
                }
            }
            Stmt::If(if_stmt) => {
                self.collect_strings_from_expr(&if_stmt.condition);
                self.collect_strings_from_block(&if_stmt.then_block);
                if let Some(else_block) = &if_stmt.else_block {
                    self.collect_strings_from_block(else_block);
                }
            }
            Stmt::While(while_stmt) => {
                self.collect_strings_from_expr(&while_stmt.condition);
                self.collect_strings_from_block(&while_stmt.body);
            }
            Stmt::For(for_stmt) => {
                self.collect_strings_from_expr(&for_stmt.iter);
                self.collect_strings_from_block(&for_stmt.body);
            }
        }
    }

    fn collect_strings_from_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Literal(lit) => {
                if let Literal::String(s) = &lit.value {
                    if !self.string_literals.contains(s) {
                        self.string_literals.push(s.clone());
                    }
                }
            }
            Expr::Call(call) => {
                self.collect_strings_from_expr(&call.callee);
                for arg in &call.args {
                    self.collect_strings_from_expr(arg);
                }
            }
            Expr::Binary(bin) => {
                self.collect_strings_from_expr(&bin.left);
                self.collect_strings_from_expr(&bin.right);
            }
            Expr::Unary(un) => {
                self.collect_strings_from_expr(&un.expr);
            }
            Expr::MethodCall(mc) => {
                self.collect_strings_from_expr(&mc.receiver);
                for arg in &mc.args {
                    self.collect_strings_from_expr(arg);
                }
            }
            Expr::FieldAccess(fa) => {
                self.collect_strings_from_expr(&fa.expr);
            }
            Expr::Index(idx) => {
                self.collect_strings_from_expr(&idx.expr);
                self.collect_strings_from_expr(&idx.index);
            }
            Expr::Closure(closure) => {
                self.collect_strings_from_expr(&closure.body);
            }
            _ => {}
        }
    }

    fn get_string_offset(&self, s: &str) -> u32 {
        let mut offset = 0u32;
        for lit in &self.string_literals {
            if lit == s {
                return offset;
            }
            offset += lit.len() as u32;
        }
        panic!("String not found: {}", s);
    }

    fn generate_component(&self, ast_module: &crate::ast::Module) -> Vec<u8> {
        let mut builder = ComponentBuilder::default();

        // Build string data for memory
        let string_data: Vec<u8> = self.string_literals.iter().flat_map(|s| s.bytes()).collect();

        // ========================================
        // Type 0: wasi:io/error instance type
        // ========================================
        {
            let mut io_error_type = wasm_encoder::InstanceType::new();
            io_error_type.export("error", ComponentTypeRef::Type(TypeBounds::SubResource));
            let (_, enc) = builder.ty(Some("io-error-type"));
            enc.instance(&io_error_type);
        }

        // Import wasi:io/error@0.2.3 as instance 0
        builder.import("wasi:io/error@0.2.3", ComponentTypeRef::Instance(0));

        // Alias export "error" from instance 0 as type $error (type 1)
        builder.alias_export(0, "error", ComponentExportKind::Type);

        // ========================================
        // Type 2: wasi:io/streams instance type
        // ========================================
        {
            let mut streams_type = wasm_encoder::InstanceType::new();
            // alias outer error type
            streams_type.alias(wasm_encoder::Alias::Outer {
                count: 1,
                kind: wasm_encoder::ComponentOuterAliasKind::Type,
                index: 1, // $error
            });
            streams_type.export("error", ComponentTypeRef::Type(TypeBounds::Eq(0)));
            streams_type.export("output-stream", ComponentTypeRef::Type(TypeBounds::SubResource));

            // Type indices after exports:
            // - alias error: type 0
            // - export error (eq 0): type 1
            // - export output-stream (sub resource): type 2
            // type 3: (own 0) - own error (for stream-error variant)
            streams_type.ty().defined_type().own(0);
            // type 4: stream-error variant
            streams_type.ty().defined_type().variant([
                ("last-operation-failed", Some(wasm_encoder::ComponentValType::Type(3)), None),
                ("closed", None, None),
            ]);
            streams_type.export("stream-error", ComponentTypeRef::Type(TypeBounds::Eq(4)));
            // export creates type 5
            // type 6: (borrow 2) - borrow output-stream
            streams_type.ty().defined_type().borrow(2);
            // type 7: (list u8)
            streams_type.ty().defined_type().list(wasm_encoder::ComponentValType::Primitive(PrimitiveValType::U8));
            // type 8: (result (error 5)) - result<_, stream-error>
            streams_type.ty().defined_type().result(None, Some(wasm_encoder::ComponentValType::Type(5)));
            // type 9: func type for blocking-write-and-flush
            // func(self: borrow<output-stream>, contents: list<u8>) -> result<_, stream-error>
            streams_type.ty().function().params([
                ("self", wasm_encoder::ComponentValType::Type(6)),
                ("contents", wasm_encoder::ComponentValType::Type(7)),
            ]).result(Some(wasm_encoder::ComponentValType::Type(8)));
            streams_type.export("[method]output-stream.blocking-write-and-flush", ComponentTypeRef::Func(9));

            let (_, enc) = builder.ty(Some("io-streams-type"));
            enc.instance(&streams_type);
        }

        // Import wasi:io/streams@0.2.3 as instance 1
        builder.import("wasi:io/streams@0.2.3", ComponentTypeRef::Instance(2));

        // Alias export "output-stream" from instance 1 as type $output-stream (type 3)
        builder.alias_export(1, "output-stream", ComponentExportKind::Type);

        // ========================================
        // Type 4: wasi:cli/stdout instance type
        // ========================================
        {
            let mut stdout_type = wasm_encoder::InstanceType::new();
            // alias outer output-stream type
            stdout_type.alias(wasm_encoder::Alias::Outer {
                count: 1,
                kind: wasm_encoder::ComponentOuterAliasKind::Type,
                index: 3, // $output-stream
            });
            stdout_type.export("output-stream", ComponentTypeRef::Type(TypeBounds::Eq(0)));
            // Type indices after exports:
            // - alias: type 0
            // - export output-stream (eq 0): type 1
            // type 2: (own 1) - own output-stream
            stdout_type.ty().defined_type().own(1);
            // type 3: func () -> own output-stream
            stdout_type.ty().function().params::<[(&str, wasm_encoder::ComponentValType); 0], wasm_encoder::ComponentValType>([]).result(Some(wasm_encoder::ComponentValType::Type(2)));
            stdout_type.export("get-stdout", ComponentTypeRef::Func(3));

            let (_, enc) = builder.ty(Some("cli-stdout-type"));
            enc.instance(&stdout_type);
        }

        // Import wasi:cli/stdout@0.2.3 as instance 2
        builder.import("wasi:cli/stdout@0.2.3", ComponentTypeRef::Instance(4));

        // Alias functions from instances
        builder.alias_export(2, "get-stdout", ComponentExportKind::Func); // func 0: $get-stdout
        builder.alias_export(1, "[method]output-stream.blocking-write-and-flush", ComponentExportKind::Func); // func 1: $write-flush

        // ========================================
        // Core memory module
        // ========================================
        let mem_module = self.build_memory_module(&string_data);
        builder.core_module_raw(Some("mem-mod"), &mem_module);

        // Instantiate memory module
        builder.core_instantiate(Some("mem"), 0, Vec::<(&str, ModuleArg)>::new());

        // Alias memory and realloc from mem instance
        builder.core_alias_export(Some("memory"), 0, "memory", ExportKind::Memory);
        builder.core_alias_export(Some("realloc"), 0, "realloc", ExportKind::Func);

        // ========================================
        // Canon lower WASI functions
        // ========================================
        // Lower get-stdout (no memory needed)
        builder.lower_func(Some("get-stdout-core"), 0, Vec::<CanonicalOption>::new());

        // Lower write-flush (needs memory and realloc)
        builder.lower_func(Some("write-flush-core"), 1, [
            CanonicalOption::Memory(0),
            CanonicalOption::Realloc(0),
        ]);

        // Resource drop for output-stream
        builder.resource_drop(3); // type index for $output-stream

        // ========================================
        // Main core module
        // ========================================
        let main_module = self.build_main_module(ast_module);
        builder.core_module_raw(Some("main-mod"), &main_module);

        // Create instance with WASI imports
        // Core func indices: 0=realloc, 1=get-stdout, 2=write-flush, 3=drop
        let wasi_exports = [
            ("get-stdout", ExportKind::Func, 1),
            ("write-flush", ExportKind::Func, 2),
            ("drop-ostream", ExportKind::Func, 3),
        ];
        let wasi_instance = builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports);

        let env_exports = [
            ("memory", ExportKind::Memory, 0),
        ];
        let env_instance = builder.core_instantiate_exports(Some("env-instance"), env_exports);

        // Instantiate main module
        builder.core_instantiate(Some("main"), 1, [
            ("wasi", ModuleArg::Instance(wasi_instance)),
            ("env", ModuleArg::Instance(env_instance)),
        ]);

        // Alias run function from main instance (instance 3)
        // Instances: 0=mem, 1=wasi, 2=env, 3=main
        builder.core_alias_export(Some("run-core"), 3, "run", ExportKind::Func);

        // ========================================
        // Result type and canon lift
        // ========================================
        // Type for result unit
        {
            let (_, enc) = builder.ty(Some("result-unit"));
            enc.defined_type().result(None, None);
        }

        // Type for run function: () -> result
        {
            let (_, enc) = builder.ty(Some("run-func-type"));
            enc.function().params::<[(&str, wasm_encoder::ComponentValType); 0], wasm_encoder::ComponentValType>([]).result(Some(wasm_encoder::ComponentValType::Type(5)));
        }

        // Lift run function
        // Core funcs: 0=realloc, 1=get-stdout, 2=write-flush, 3=drop, 4=run
        // This creates component func 2
        builder.lift_func(Some("run"), 4, 6, Vec::<CanonicalOption>::new());

        // For WASI CLI, we need to export the run function.
        // wasmtime with --invoke run should work for direct function exports.
        builder.export("run", ComponentExportKind::Func, 2, None);

        builder.finish()
    }

    fn build_memory_module(&self, string_data: &[u8]) -> Vec<u8> {
        let mut module = Module::new();

        // Type section: realloc type
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32, ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
        module.section(&types);

        // Function section
        let mut functions = FunctionSection::new();
        functions.function(0); // realloc uses type 0
        module.section(&functions);

        // Memory section
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memories);

        // Export section
        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("realloc", ExportKind::Func, 0);
        module.section(&exports);

        // Code section: realloc function
        let mut code = CodeSection::new();
        let mut realloc_func = Function::new([]);
        realloc_func.instruction(&Instruction::I32Const(1024));
        realloc_func.instruction(&Instruction::End);
        code.function(&realloc_func);
        module.section(&code);

        // Data section: string literals
        if !string_data.is_empty() {
            let mut data = DataSection::new();
            data.segment(DataSegment {
                mode: DataSegmentMode::Active {
                    memory_index: 0,
                    offset: &ConstExpr::i32_const(0),
                },
                data: string_data.iter().copied(),
            });
            module.section(&data);
        }

        module.finish()
    }

    fn build_main_module(&self, ast_module: &crate::ast::Module) -> Vec<u8> {
        let mut module = Module::new();

        // Type section
        let mut types = TypeSection::new();
        // Type 0: get_stdout () -> i32
        types.ty().function([], [ValType::I32]);
        // Type 1: write_flush (i32, i32, i32, i32) -> ()
        types.ty().function([ValType::I32, ValType::I32, ValType::I32, ValType::I32], []);
        // Type 2: drop_ostream (i32) -> ()
        types.ty().function([ValType::I32], []);
        // Type 3: run () -> i32
        types.ty().function([], [ValType::I32]);
        module.section(&types);

        // Import section
        let mut imports = ImportSection::new();
        imports.import("wasi", "get-stdout", EntityType::Function(0));
        imports.import("wasi", "write-flush", EntityType::Function(1));
        imports.import("wasi", "drop-ostream", EntityType::Function(2));
        imports.import("env", "memory", EntityType::Memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        }));
        module.section(&imports);

        // Function section
        let mut functions = FunctionSection::new();
        functions.function(3); // run function uses type 3
        module.section(&functions);

        // Export section
        let mut exports = ExportSection::new();
        exports.export("run", ExportKind::Func, 3); // function index 3 (after 3 imports)
        module.section(&exports);

        // Code section
        let mut code = CodeSection::new();
        let mut run_func = Function::new([(1, ValType::I32)]); // local $handle

        // Generate body from AST
        self.generate_run_body_instructions(&mut run_func, ast_module);

        // Return 0
        run_func.instruction(&Instruction::I32Const(0));
        run_func.instruction(&Instruction::End);
        code.function(&run_func);
        module.section(&code);

        module.finish()
    }

    fn generate_run_body_instructions(&self, func: &mut Function, ast_module: &crate::ast::Module) {
        // Find main function
        let main_func = ast_module.items.iter().find_map(|item| {
            if let Item::Function(f) = item {
                if f.name == "main" {
                    return Some(f);
                }
            }
            None
        });

        if let Some(main) = main_func {
            for stmt in &main.body.stmts {
                self.generate_stmt_instructions(func, stmt);
            }
        }
    }

    fn generate_stmt_instructions(&self, func: &mut Function, stmt: &Stmt) {
        if let Stmt::Expr(expr_stmt) = stmt {
            self.generate_expr_instructions(func, &expr_stmt.expr);
        }
    }

    fn generate_expr_instructions(&self, func: &mut Function, expr: &Expr) {
        if let Expr::Call(call) = expr {
            self.generate_call_instructions(func, call);
        }
    }

    fn generate_call_instructions(&self, func: &mut Function, call: &CallExpr) {
        if let Expr::Ident(ident) = &call.callee {
            if ident.name == "println" {
                self.generate_println_instructions(func, call);
            }
        }
    }

    fn generate_println_instructions(&self, func: &mut Function, call: &CallExpr) {
        if call.args.len() != 1 {
            return;
        }

        if let Expr::Literal(lit) = &call.args[0] {
            if let Literal::String(s) = &lit.value {
                let str_offset = self.get_string_offset(s);
                let str_len = s.len() as i32;

                // Call get_stdout and store handle
                func.instruction(&Instruction::Call(0)); // get_stdout
                func.instruction(&Instruction::LocalSet(0)); // store in $handle

                // Call write_flush(handle, offset, len, result_ptr)
                func.instruction(&Instruction::LocalGet(0)); // handle
                func.instruction(&Instruction::I32Const(str_offset as i32)); // offset
                func.instruction(&Instruction::I32Const(str_len)); // len
                func.instruction(&Instruction::I32Const(100)); // result ptr
                func.instruction(&Instruction::Call(1)); // write_flush

                // Drop the handle
                func.instruction(&Instruction::LocalGet(0)); // handle
                func.instruction(&Instruction::Call(2)); // drop_ostream
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> crate::ast::Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        parser.parse().expect("parser error")
    }

    #[test]
    fn test_collect_strings() {
        let ast = parse(
            r#"
            use core::cli::{println, Stdout};

            fn main() with Stdout {
                println("Hello, world!\n");
            }
        "#,
        );

        let mut codegen = Codegen::new();
        codegen.collect_strings(&ast);

        assert_eq!(codegen.string_literals.len(), 1);
        assert_eq!(codegen.string_literals[0], "Hello, world!\n");
    }

    #[test]
    fn test_generate_binary() {
        let ast = parse(
            r#"
            use core::cli::{println, Stdout};

            fn main() with Stdout {
                println("Hello!\n");
            }
        "#,
        );

        let mut codegen = Codegen::new();
        let wasm = codegen.generate(&ast);

        // Verify it starts with Wasm magic number
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[0..4], b"\0asm");

        // Validate the generated Wasm using wasmparser
        let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        validator.validate_all(&wasm).expect("Wasm validation failed");
    }

    #[test]
    fn test_generate_wat() {
        let ast = parse(
            r#"
            use core::cli::{println, Stdout};

            fn main() with Stdout {
                println("Hello!\n");
            }
        "#,
        );

        let mut codegen = Codegen::new();
        let wat = codegen.generate_wat(&ast);

        // Verify it produces valid WAT
        assert!(wat.contains("(component"));
    }
}
