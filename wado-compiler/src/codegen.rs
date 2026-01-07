// WAT code generator for Wado

use crate::ast::*;

pub struct Codegen {
    output: String,
    indent: usize,
    string_literals: Vec<String>,
    // Memory layout:
    // 0-99: reserved for iovec and nwritten
    // 100+: string data
    next_string_offset: usize,
}

impl Codegen {
    pub fn new() -> Self {
        Self {
            output: String::new(),
            indent: 0,
            string_literals: Vec::new(),
            next_string_offset: 100, // Start string data at offset 100
        }
    }

    pub fn generate(&mut self, module: &Module) -> String {
        // First pass: collect string literals
        self.collect_strings(module);

        // Generate WAT
        self.emit_module(module);

        self.output.clone()
    }

    fn collect_strings(&mut self, module: &Module) {
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

    fn emit(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn emit_line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.output.push_str("  ");
        }
        self.output.push_str(s);
        self.output.push('\n');
    }

    fn emit_module(&mut self, module: &Module) {
        self.emit_line("(module");
        self.indent += 1;

        // Emit WASI imports
        self.emit_wasi_imports();

        // Emit memory
        self.emit_line("");
        self.emit_line(";; Memory (1 page = 64KB)");
        self.emit_line("(memory (export \"memory\") 1)");

        // Emit string data
        self.emit_string_data();

        // Emit functions
        for item in &module.items {
            if let Item::Function(func) = item {
                self.emit_line("");
                self.emit_function(func);
            }
        }

        self.indent -= 1;
        self.emit_line(")");
    }

    fn emit_wasi_imports(&mut self) {
        self.emit_line(";; WASI imports");
        self.emit_line("(import \"wasi_snapshot_preview1\" \"fd_write\"");
        self.indent += 1;
        self.emit_line("(func $fd_write (param i32 i32 i32 i32) (result i32)))");
        self.indent -= 1;
    }

    fn emit_string_data(&mut self) {
        if self.string_literals.is_empty() {
            return;
        }

        self.emit_line("");
        self.emit_line(";; String data");

        // Collect data lines first to avoid borrow issues
        let mut lines = Vec::new();
        let mut offset = self.next_string_offset;
        for (i, s) in self.string_literals.iter().enumerate() {
            let escaped = escape_wat_string(s);
            lines.push(format!(
                "(data (i32.const {}) \"{}\") ;; string_{}, len={}",
                offset, escaped, i, s.len()
            ));
            offset += s.len();
        }

        for line in lines {
            self.emit_line(&line);
        }
    }

    fn get_string_offset(&self, s: &str) -> usize {
        let mut offset = self.next_string_offset;
        for lit in &self.string_literals {
            if lit == s {
                return offset;
            }
            offset += lit.len();
        }
        panic!("String not found: {}", s);
    }

    fn emit_function(&mut self, func: &Function) {
        // For now, only handle main -> _start
        let export_name = if func.name == "main" {
            "_start"
        } else {
            &func.name
        };

        self.emit_line(&format!(";; function: {}", func.name));
        self.emit_line(&format!("(func (export \"{}\")", export_name));
        self.indent += 1;

        // Emit function body
        self.emit_block(&func.body);

        self.indent -= 1;
        self.emit_line(")");
    }

    fn emit_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.emit_stmt(stmt);
        }
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Expr(expr_stmt) => {
                self.emit_expr(&expr_stmt.expr);
            }
            _ => {
                self.emit_line(";; TODO: unimplemented statement");
            }
        }
    }

    fn emit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => {
                self.emit_call(call);
            }
            _ => {
                self.emit_line(";; TODO: unimplemented expression");
            }
        }
    }

    fn emit_call(&mut self, call: &CallExpr) {
        // Check if it's a println call
        if let Expr::Ident(ident) = &call.callee {
            if ident.name == "println" {
                self.emit_println(call);
                return;
            }
        }

        self.emit_line(";; TODO: general function call");
    }

    fn emit_println(&mut self, call: &CallExpr) {
        // Get the string argument
        if call.args.len() != 1 {
            self.emit_line(";; ERROR: println expects 1 argument");
            return;
        }

        if let Expr::Literal(lit) = &call.args[0] {
            if let Literal::String(s) = &lit.value {
                let str_offset = self.get_string_offset(s);
                let str_len = s.len();

                self.emit_line(";; println call");

                // Set up iovec at address 0
                // iovec.buf = str_offset (at address 0)
                self.emit_line(&format!(
                    "(i32.store (i32.const 0) (i32.const {})) ;; iovec.buf",
                    str_offset
                ));
                // iovec.len = str_len (at address 4)
                self.emit_line(&format!(
                    "(i32.store (i32.const 4) (i32.const {})) ;; iovec.len",
                    str_len
                ));

                // Call fd_write(stdout=1, iovs=0, iovs_len=1, nwritten=8)
                self.emit_line("(call $fd_write");
                self.indent += 1;
                self.emit_line("(i32.const 1)  ;; fd: stdout");
                self.emit_line("(i32.const 0)  ;; iovs: pointer to iovec array");
                self.emit_line("(i32.const 1)  ;; iovs_len: 1 iovec");
                self.emit_line("(i32.const 8)) ;; nwritten: where to store bytes written");
                self.indent -= 1;
                self.emit_line("drop ;; ignore return value");

                return;
            }
        }

        self.emit_line(";; ERROR: println expects a string literal");
    }
}

fn escape_wat_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            c if c.is_ascii_graphic() || c == ' ' => result.push(c),
            c => {
                // Escape as hex
                for byte in c.to_string().as_bytes() {
                    result.push_str(&format!("\\{:02x}", byte));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn generate(source: &str) -> String {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        let module = parser.parse().expect("parser error");
        let mut codegen = Codegen::new();
        codegen.generate(&module)
    }

    #[test]
    fn test_hello_world() {
        let wat = generate(r#"
            use core::cli::{println, Stdout};

            fn main() with Stdout {
                println("Hello, world!\n");
            }
        "#);

        assert!(wat.contains("fd_write"));
        assert!(wat.contains("Hello, world!"));
        assert!(wat.contains("_start"));
    }
}
