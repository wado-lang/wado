// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder
// Targets WASI P3 (0.3.0-rc-2025-09-16) with native stream<T> types

use crate::ast::{Block, CallExpr, Expr, Item, Literal, Stmt};
use crate::symbol::{SymbolKind, SymbolTable};
use wasm_encoder::{
    Alias, CanonicalOption, CodeSection, ComponentBuilder, ComponentExportKind,
    ComponentOuterAliasKind, ComponentValType, ConstExpr, DataSection, DataSegment,
    DataSegmentMode, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    ImportSection, InstanceType, Instruction, MemorySection, MemoryType, Module, ModuleArg,
    PrimitiveValType, TypeBounds, TypeSection, ValType,
};

/// Code generator that produces Component Model components
/// Targets WASI P3 (0.3.0-rc-2025-09-16)
pub struct Codegen {
    string_literals: Vec<String>,
    symbols: SymbolTable,
}

impl Codegen {
    /// Create a new code generator with a symbol table
    pub fn new(symbols: SymbolTable) -> Self {
        Self {
            string_literals: Vec::new(),
            symbols,
        }
    }

    /// Generate Component Model binary Wasm
    pub fn generate_wasm(&mut self, module: &crate::ast::Module) -> Vec<u8> {
        // First pass: collect string literals
        self.collect_strings(module);

        // Generate binary Wasm
        self.generate_component(module)
    }

    /// Generate WAT text format (for debugging)
    pub fn generate_wat(&mut self, module: &crate::ast::Module) -> String {
        let wasm = self.generate_wasm(module);
        wasmprinter::print_bytes(&wasm).unwrap_or_else(|e| format!("Error: {e}"))
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
        panic!("String not found: {s}");
    }

    /// Generate component for WASI P3
    /// Uses native stream<T> types and imports wasi:cli/stdout
    fn generate_component(&self, ast_module: &crate::ast::Module) -> Vec<u8> {
        let mut builder = ComponentBuilder::default();

        // Build string data for memory
        let string_data: Vec<u8> = self
            .string_literals
            .iter()
            .flat_map(|s| s.bytes())
            .collect();

        // ========================================
        // Type 0: types instance type (for wasi:cli/types)
        // Contains error-code enum definition
        // ========================================
        {
            let (_, enc) = builder.ty(Some("types-instance-type"));
            let mut instance_type = InstanceType::new();
            // Type 0 within instance: error-code enum
            instance_type
                .ty()
                .defined_type()
                .enum_type(["io", "illegal-byte-sequence", "pipe"]);
            // Export error-code type
            instance_type.export(
                "error-code",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(0)),
            );
            enc.instance(&instance_type);
        }

        // Import types instance from WASI P3 (instance 0)
        builder.import(
            "wasi:cli/types@0.3.0-rc-2025-09-16",
            wasm_encoder::ComponentTypeRef::Instance(0),
        );

        // Alias error-code from types instance (type 1)
        builder.alias_export(0, "error-code", ComponentExportKind::Type);

        // ========================================
        // Type 2: stdout instance type
        // Uses the aliased error-code type for result via outer alias
        // ========================================
        {
            let (_, enc) = builder.ty(Some("stdout-instance-type"));
            let mut instance_type = InstanceType::new();
            // Type 0 within instance: stream<u8>
            instance_type
                .ty()
                .defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
            // Type 1 within instance: outer alias to type 1 (error-code)
            // count=1 means go up 1 level (to parent component), index=1 is the error-code type
            instance_type.alias(Alias::Outer {
                kind: ComponentOuterAliasKind::Type,
                count: 1,
                index: 1,
            });
            // Type 2 within instance: result<_, error-code>
            instance_type
                .ty()
                .defined_type()
                .result(None, Some(ComponentValType::Type(1)));
            // Type 3 within instance: async func(stream<u8>) -> result<_, error-code>
            instance_type
                .ty()
                .function()
                .async_(true)
                .params([("data", ComponentValType::Type(0))])
                .result(Some(ComponentValType::Type(2)));
            // Export write-via-stream referencing type 3 within this instance type
            instance_type.export("write-via-stream", wasm_encoder::ComponentTypeRef::Func(3));
            enc.instance(&instance_type);
        }

        // Import stdout instance from WASI P3 (instance 1)
        builder.import(
            "wasi:cli/stdout@0.3.0-rc-2025-09-16",
            wasm_encoder::ComponentTypeRef::Instance(2),
        );

        // Alias write-via-stream from stdout instance (instance 1)
        builder.alias_export(1, "write-via-stream", ComponentExportKind::Func);

        // ========================================
        // Type 3: stream<u8> for stream intrinsics
        // ========================================
        {
            let (_, enc) = builder.ty(Some("stream-u8"));
            enc.defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
        }

        // ========================================
        // Type 4: result unit for run function (needed for task.return)
        // ========================================
        {
            let (_, enc) = builder.ty(Some("result-unit"));
            enc.defined_type().result(None, None);
        }

        // ========================================
        // Core memory module
        // ========================================
        let mem_module = self.build_memory_module(&string_data);
        builder.core_module_raw(Some("mem-mod"), &mem_module);

        // Instantiate memory module (core instance 0)
        builder.core_instantiate(Some("mem"), 0, Vec::<(&str, ModuleArg)>::new());

        // Alias memory and realloc from mem instance
        builder.core_alias_export(Some("memory"), 0, "memory", ExportKind::Memory);
        builder.core_alias_export(Some("realloc"), 0, "realloc", ExportKind::Func);

        // ========================================
        // Stream canonical intrinsics for stream<u8> (type index 3)
        // ========================================
        // Core func 1: stream.new -> returns i64 (rx in low 32, tx in high 32)
        builder.stream_new(3);
        // Core func 2: stream.write (tx, ptr, len) -> i32 status
        builder.stream_write(3, [CanonicalOption::Memory(0), CanonicalOption::Realloc(0)]);
        // Core func 3: stream.drop-writable (tx)
        builder.stream_drop_writable(3);

        // Core func 4: stream.drop-readable (rx)
        builder.stream_drop_readable(3);

        // Lower write-via-stream (func 0) to core func 5
        // Use Async lowering - returns i32 (subtask handle or completion status)
        builder.lower_func(
            Some("write-via-stream-core"),
            0,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(0),
                CanonicalOption::Realloc(0),
            ],
        );

        // Core func 6: task.return for completing async tasks
        // Result type is type 4 (result unit)
        // For simple result types without payloads, no memory option needed
        builder.task_return(Some(ComponentValType::Type(4)), []);

        // Core func 7: waitable-set.new
        builder.waitable_set_new();

        // Core func 8: waitable.join
        builder.waitable_join();

        // Core func 9: waitable-set.wait
        builder.waitable_set_wait(false, 0);

        // Core func 10: subtask.drop
        builder.subtask_drop();

        // ========================================
        // Main core module for P3
        // ========================================
        let main_module = self.build_main_module_p3(ast_module);
        builder.core_module_raw(Some("main-mod"), &main_module);

        // Create instance with stream intrinsics + lowered WASI function + async intrinsics
        // Core func indices:
        // 0=realloc, 1=stream.new, 2=stream.write, 3=stream.drop-writable,
        // 4=stream.drop-readable, 5=write-via-stream, 6=task.return, 7=waitable-set.new,
        // 8=waitable.join, 9=waitable-set.wait, 10=subtask.drop
        let wasi_exports = [
            ("stream-new", ExportKind::Func, 1),
            ("stream-write", ExportKind::Func, 2),
            ("stream-drop-writable", ExportKind::Func, 3),
            ("stream-drop-readable", ExportKind::Func, 4),
            ("write-via-stream", ExportKind::Func, 5),
            ("task-return", ExportKind::Func, 6),
            ("waitable-set-new", ExportKind::Func, 7),
            ("waitable-join", ExportKind::Func, 8),
            ("waitable-set-wait", ExportKind::Func, 9),
            ("subtask-drop", ExportKind::Func, 10),
        ];
        let wasi_instance = builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports);

        let env_exports = [("memory", ExportKind::Memory, 0)];
        let env_instance = builder.core_instantiate_exports(Some("env-instance"), env_exports);

        // Instantiate main module (core instance 3)
        builder.core_instantiate(
            Some("main"),
            1,
            [
                ("wasi", ModuleArg::Instance(wasi_instance)),
                ("env", ModuleArg::Instance(env_instance)),
            ],
        );

        // Alias run function from main instance
        // Core instances: 0=mem, 1=wasi, 2=env, 3=main
        builder.core_alias_export(Some("run-core"), 3, "run", ExportKind::Func);

        // Type 5: async run function type () -> result
        {
            let (_, enc) = builder.ty(Some("run-func-type"));
            enc.function()
                .async_(true)
                .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                .result(Some(ComponentValType::Type(4)));
        }

        // Lift run function using type 5 with Async option
        // Core func 11 = the run function (after 10 imports + 1 defined)
        builder.lift_func(
            Some("run"),
            11,
            5,
            [CanonicalOption::Async, CanonicalOption::Memory(0)],
        );

        // Export run function (func 1)
        builder.export("run", ComponentExportKind::Func, 1, None);

        builder.finish()
    }

    /// Build main module for WASI P3 with write-via-stream
    fn build_main_module_p3(&self, ast_module: &crate::ast::Module) -> Vec<u8> {
        let mut module = Module::new();

        // Type section
        let mut types = TypeSection::new();
        // Type 0: stream-new () -> i64 (rx in low 32, tx in high 32)
        types.ty().function([], [ValType::I64]);
        // Type 1: stream-write (tx, ptr, len) -> i32 status
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
        // Type 2: stream-drop-writable (tx) -> ()
        types.ty().function([ValType::I32], []);
        // Type 3: stream-drop-readable (rx) -> ()
        types.ty().function([ValType::I32], []);
        // Type 4: write-via-stream async lowered (rx, outptr) -> i32
        // Async lowering: takes stream handle + outptr for result, returns status
        // Return value: if bit 0 set (odd) = completed, if bit 0 clear (even) = subtask handle
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        // Type 5: task-return (discriminant) -> ()
        types.ty().function([ValType::I32], []);
        // Type 6: waitable-set-new () -> i32
        types.ty().function([], [ValType::I32]);
        // Type 7: waitable-join (set, waitable) -> ()
        types.ty().function([ValType::I32, ValType::I32], []);
        // Type 8: waitable-set-wait (set, outptr) -> i32
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        // Type 9: subtask-drop (subtask) -> ()
        types.ty().function([ValType::I32], []);
        // Type 10: run () -> ()
        // Async entry point - uses task.return to provide result
        types.ty().function([], []);
        module.section(&types);

        // Import section
        let mut imports = ImportSection::new();
        imports.import("wasi", "stream-new", EntityType::Function(0));
        imports.import("wasi", "stream-write", EntityType::Function(1));
        imports.import("wasi", "stream-drop-writable", EntityType::Function(2));
        imports.import("wasi", "stream-drop-readable", EntityType::Function(3));
        imports.import("wasi", "write-via-stream", EntityType::Function(4));
        imports.import("wasi", "task-return", EntityType::Function(5));
        imports.import("wasi", "waitable-set-new", EntityType::Function(6));
        imports.import("wasi", "waitable-join", EntityType::Function(7));
        imports.import("wasi", "waitable-set-wait", EntityType::Function(8));
        imports.import("wasi", "subtask-drop", EntityType::Function(9));
        imports.import(
            "env",
            "memory",
            EntityType::Memory(MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            }),
        );
        module.section(&imports);

        // Function section
        let mut functions = FunctionSection::new();
        functions.function(10); // run function uses type 10 (no return value, uses task.return)
        module.section(&functions);

        // Export section
        let mut exports = ExportSection::new();
        exports.export("run", ExportKind::Func, 10); // function index 10 (after 10 imports)
        module.section(&exports);

        // Code section
        let mut code = CodeSection::new();
        // Locals:
        // 0: $ret64 (i64) - for stream.new result / waitable-set handle (i64 but used as i32)
        // 1: $rx (i32) - readable stream handle
        // 2: $tx (i32) - writable stream handle
        // 3: $status (i32) - write status / waitable handle
        let mut run_func = Function::new([(1, ValType::I64), (3, ValType::I32)]);

        // Generate body from AST
        self.generate_run_body_instructions_p3(&mut run_func, ast_module);

        // Call task.return to complete the async task
        // For result unit with no payload, pass discriminant directly (0 = ok)
        run_func.instruction(&Instruction::I32Const(0)); // 0 = ok discriminant
        run_func.instruction(&Instruction::Call(5)); // task-return (func 5)

        // No return value - task.return already provided the result
        run_func.instruction(&Instruction::End);
        code.function(&run_func);
        module.section(&code);

        module.finish()
    }

    fn generate_run_body_instructions_p3(
        &self,
        func: &mut Function,
        ast_module: &crate::ast::Module,
    ) {
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
                self.generate_stmt_instructions_p3(func, stmt);
            }
        }
    }

    fn generate_stmt_instructions_p3(&self, func: &mut Function, stmt: &Stmt) {
        if let Stmt::Expr(expr_stmt) = stmt {
            self.generate_expr_instructions_p3(func, &expr_stmt.expr);
        }
    }

    fn generate_expr_instructions_p3(&self, func: &mut Function, expr: &Expr) {
        if let Expr::Call(call) = expr {
            self.generate_call_instructions_p3(func, call);
        }
    }

    fn generate_call_instructions_p3(&self, func: &mut Function, call: &CallExpr) {
        if let Expr::Ident(ident) = &call.callee {
            // Look up the function in the symbol table
            if let Some(symbol) = self.symbols.lookup(&ident.name) {
                if let SymbolKind::Function(func_sym) = &symbol.kind {
                    // Check if it's a builtin function
                    if func_sym.is_builtin {
                        self.generate_builtin_call(func, &ident.name, call);
                    }
                    // TODO: handle user-defined function calls
                }
            } else {
                // Symbol not found - for now, try as builtin (backwards compatibility)
                self.generate_builtin_call(func, &ident.name, call);
            }
        }
    }

    /// Generate code for a builtin function call
    fn generate_builtin_call(&self, func: &mut Function, name: &str, call: &CallExpr) {
        match name {
            "println" => self.generate_println_instructions_p3(func, call),
            // Add more builtins here as needed
            _ => {
                // Unknown builtin - ignore for now
            }
        }
    }

    /// Generate println using WASI P3 write-via-stream
    /// Pattern:
    /// 1. Create stream with stream.new (returns rx and tx)
    /// 2. Call write-via-stream(rx) to connect readable end to stdout (starts consumer)
    /// 3. Write data to tx with stream.write
    /// 4. Drop tx to signal EOF (stream.drop-writable)
    /// 5. Wait for write-via-stream subtask to complete
    /// 6. Drop subtask
    fn generate_println_instructions_p3(&self, func: &mut Function, call: &CallExpr) {
        if call.args.len() != 1 {
            return;
        }

        if let Expr::Literal(lit) = &call.args[0] {
            if let Literal::String(s) = &lit.value {
                let str_offset = self.get_string_offset(s);
                let str_len = s.len() as i32;

                // Local indices (defined in build_main_module_p3):
                // 0: $ret64 (i64) - stream.new result / waitable-set handle
                // 1: $rx (i32) - readable stream handle
                // 2: $tx (i32) - writable stream handle
                // 3: $status (i32) - subtask handle / status

                // 1. Create stream: stream-new() -> i64 (rx in low 32, tx in high 32)
                func.instruction(&Instruction::Call(0)); // stream-new
                func.instruction(&Instruction::LocalSet(0)); // store i64 result

                // Extract rx (low 32 bits)
                func.instruction(&Instruction::LocalGet(0));
                func.instruction(&Instruction::I32WrapI64);
                func.instruction(&Instruction::LocalSet(1)); // $rx

                // Extract tx (high 32 bits)
                func.instruction(&Instruction::LocalGet(0));
                func.instruction(&Instruction::I64Const(32));
                func.instruction(&Instruction::I64ShrU);
                func.instruction(&Instruction::I32WrapI64);
                func.instruction(&Instruction::LocalSet(2)); // $tx

                // 2. Call write-via-stream(rx, outptr) to start consuming from the stream
                // Async lowered: takes stream handle + outptr, returns status
                // Status: bit 0 set (odd) = completed, bit 0 clear (even) = subtask handle
                func.instruction(&Instruction::LocalGet(1)); // rx (readable end goes to stdout)
                func.instruction(&Instruction::I32Const(2048)); // outptr for result
                func.instruction(&Instruction::Call(4)); // write-via-stream (func 4)
                func.instruction(&Instruction::LocalSet(3)); // save subtask/status

                // 3. Write string to stream: stream-write(tx, ptr, len) -> status
                func.instruction(&Instruction::LocalGet(2)); // tx
                func.instruction(&Instruction::I32Const(str_offset as i32)); // ptr
                func.instruction(&Instruction::I32Const(str_len)); // len
                func.instruction(&Instruction::Call(1)); // stream-write
                func.instruction(&Instruction::Drop); // ignore write status for now

                // 4. Close writable end to signal EOF: stream-drop-writable(tx)
                func.instruction(&Instruction::LocalGet(2)); // tx
                func.instruction(&Instruction::Call(2)); // stream-drop-writable

                // 5. Wait for write-via-stream subtask to complete if still pending
                // Check if pending (bit 0 clear = even means subtask handle)
                func.instruction(&Instruction::LocalGet(3));
                func.instruction(&Instruction::I32Const(1));
                func.instruction(&Instruction::I32And);
                func.instruction(&Instruction::I32Eqz); // true if pending (subtask handle)
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

                // Pending - wait for write-via-stream to complete
                // local 3 contains the subtask handle

                // Create waitable-set: waitable-set-new() -> i32
                func.instruction(&Instruction::Call(6)); // waitable-set-new (func 6)
                // Store set handle in local 0 (as i64)
                func.instruction(&Instruction::I64ExtendI32U);
                func.instruction(&Instruction::LocalSet(0));

                // Join subtask to waitable-set: waitable-join(set, waitable)
                func.instruction(&Instruction::LocalGet(0));
                func.instruction(&Instruction::I32WrapI64); // set handle
                func.instruction(&Instruction::LocalGet(3)); // subtask handle
                func.instruction(&Instruction::Call(7)); // waitable-join (func 7)

                // Wait for completion: waitable-set-wait(set, outptr) -> i32
                func.instruction(&Instruction::LocalGet(0));
                func.instruction(&Instruction::I32WrapI64); // set handle
                func.instruction(&Instruction::I32Const(2048)); // outptr
                func.instruction(&Instruction::Call(8)); // waitable-set-wait (func 8)
                func.instruction(&Instruction::Drop); // ignore result

                // 6. Drop subtask: subtask-drop(subtask)
                func.instruction(&Instruction::LocalGet(3)); // subtask handle
                func.instruction(&Instruction::Call(9)); // subtask-drop (func 9)

                func.instruction(&Instruction::End);
            }
        }
    }

    fn build_memory_module(&self, string_data: &[u8]) -> Vec<u8> {
        let mut module = Module::new();

        // Type section: realloc type
        let mut types = TypeSection::new();
        types.ty().function(
            [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            [ValType::I32],
        );
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::Analyzer;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> crate::ast::Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        parser.parse().expect("parser error")
    }

    fn analyze(module: &crate::ast::Module) -> SymbolTable {
        let mut analyzer = Analyzer::new();
        analyzer.analyze(module, &[]).expect("analysis error");
        analyzer.into_symbols()
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

        let symbols = analyze(&ast);
        let mut codegen = Codegen::new(symbols);
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

        let symbols = analyze(&ast);
        let mut codegen = Codegen::new(symbols);
        let wasm = codegen.generate_wasm(&ast);

        // Verify it starts with Wasm magic number
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[0..4], b"\0asm");

        // Validate the generated Wasm using wasmparser
        let mut validator =
            wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        validator
            .validate_all(&wasm)
            .expect("Wasm validation failed");
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

        let symbols = analyze(&ast);
        let mut codegen = Codegen::new(symbols);
        let wat = codegen.generate_wat(&ast);

        // Verify it produces valid WAT
        assert!(wat.contains("(component"));
    }
}
