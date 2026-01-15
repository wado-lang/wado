//! Type resolution phase for Wado
//!
//! The type resolver:
//! 1. Takes the desugared AST and symbol table from the analyzer
//! 2. Performs type inference and type checking
//! 3. Produces the Typed Intermediate Representation (TIR)
//!
//! All type resolution happens in this phase. The output TIR has fully
//! resolved types on every expression, making code generation mechanical.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    self, AssertStmt, BinaryOp, Block, BreakStmt, ContinueStmt, Expr, ExprStmt, ForStmt, Function,
    IfExpr, IfStmt, Item, LetStmt, Literal, LoopStmt, MatchArm, Module, Pattern, ReturnStmt, Stmt,
    Type, UnaryOp, WhileStmt,
};
use crate::symbol::SymbolTable;
use crate::tir::{
    ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirLiteralPattern,
    TirMatchArm, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Errors from the type resolution phase
#[derive(Debug, Clone)]
pub enum TypeError {
    /// Type mismatch
    TypeMismatch {
        expected: String,
        found: String,
        span: Span,
    },

    /// Unknown type name
    UnknownType { name: String, span: Span },

    /// Unknown function
    UnknownFunction { name: String, span: Span },

    /// Unknown variable
    UnknownVariable { name: String, span: Span },

    /// Field not found on struct
    FieldNotFound {
        struct_name: String,
        field_name: String,
        span: Span,
    },

    /// Wrong number of arguments
    ArgumentCountMismatch {
        expected: usize,
        found: usize,
        span: Span,
    },

    /// Invalid numeric literal
    InvalidLiteral { message: String, span: Span },
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeError::TypeMismatch {
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: type mismatch: expected '{}', found '{}'",
                    span.line, span.column, expected, found
                )
            }
            TypeError::UnknownType { name, span } => {
                write!(f, "{}:{}: unknown type '{}'", span.line, span.column, name)
            }
            TypeError::UnknownFunction { name, span } => {
                write!(
                    f,
                    "{}:{}: unknown function '{}'",
                    span.line, span.column, name
                )
            }
            TypeError::UnknownVariable { name, span } => {
                write!(
                    f,
                    "{}:{}: unknown variable '{}'",
                    span.line, span.column, name
                )
            }
            TypeError::FieldNotFound {
                struct_name,
                field_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: field '{}' not found on struct '{}'",
                    span.line, span.column, field_name, struct_name
                )
            }
            TypeError::ArgumentCountMismatch {
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: expected {} arguments, found {}",
                    span.line, span.column, expected, found
                )
            }
            TypeError::InvalidLiteral { message, span } => {
                write!(f, "{}:{}: {}", span.line, span.column, message)
            }
        }
    }
}

impl std::error::Error for TypeError {}

/// Local variable information during resolution
#[derive(Debug, Clone)]
struct LocalVar {
    name: String,
    type_id: TypeId,
    index: u32,
    is_mut: bool,
}

/// Function context during resolution
struct FunctionContext {
    /// Local variables (name -> info)
    locals: HashMap<String, LocalVar>,
    /// Next local index
    next_local: u32,
    /// Return type of the function
    return_type: TypeId,
    /// Local variable types in order
    local_types: Vec<TypeId>,
}

impl FunctionContext {
    fn new(return_type: TypeId) -> Self {
        Self {
            locals: HashMap::new(),
            next_local: 0,
            return_type,
            local_types: Vec::new(),
        }
    }

    fn add_local(&mut self, name: String, type_id: TypeId, is_mut: bool) -> u32 {
        let index = self.next_local;
        self.next_local += 1;
        self.local_types.push(type_id);
        self.locals.insert(
            name.clone(),
            LocalVar {
                name,
                type_id,
                index,
                is_mut,
            },
        );
        index
    }

    fn lookup(&self, name: &str) -> Option<&LocalVar> {
        self.locals.get(name)
    }
}

/// The resolver converts AST to TIR with resolved types
pub struct Resolver<'a> {
    /// Type table (shared across all modules)
    type_table: TypeTable,
    /// Symbol table from analyzer
    #[allow(dead_code)]
    symbols: &'a SymbolTable,
    /// Loaded modules from analyzer
    #[allow(dead_code)]
    loaded_modules: &'a HashMap<Vec<String>, Module>,
    /// Type aliases (name -> resolved type)
    type_aliases: HashMap<String, TypeId>,
    /// Struct field info (struct name -> fields)
    struct_fields: HashMap<String, Vec<(String, TypeId)>>,
    /// Function return types (name -> return type)
    function_return_types: HashMap<String, TypeId>,
    /// Imported function names for the current module
    imported_functions: HashSet<String>,
    /// Errors collected during resolution
    errors: Vec<TypeError>,
    /// Source code for extracting source text (for power-assert)
    source_code: &'a str,
    /// Current module path being resolved (for struct type module_path)
    current_module_path: Vec<String>,
}

impl<'a> Resolver<'a> {
    /// Create a new resolver
    pub fn new(
        symbols: &'a SymbolTable,
        loaded_modules: &'a HashMap<Vec<String>, Module>,
        source_code: &'a str,
    ) -> Self {
        Self {
            type_table: TypeTable::new(),
            symbols,
            loaded_modules,
            type_aliases: HashMap::new(),
            struct_fields: HashMap::new(),
            function_return_types: HashMap::new(),
            imported_functions: HashSet::new(),
            errors: Vec::new(),
            source_code,
            current_module_path: Vec::new(),
        }
    }

    /// Get source text for a span (for power-assert)
    fn get_source_text(&self, span: &crate::token::Span) -> String {
        if span.start < self.source_code.len() && span.end <= self.source_code.len() {
            self.source_code[span.start..span.end].to_string()
        } else {
            String::from("<unknown>")
        }
    }

    /// Resolve a module, converting AST to TIR
    pub fn resolve_module(
        &mut self,
        module: &Module,
        module_path: Vec<String>,
    ) -> Result<TirModule, Vec<TypeError>> {
        // Set current module path for struct type creation
        self.current_module_path = module_path.clone();

        // First pass: collect type definitions
        self.collect_types(module);

        // Second pass: collect function signatures (for call resolution)
        self.collect_function_signatures(module);

        // Third pass: resolve functions
        let mut tir_module = TirModule::new(module_path);

        for item in &module.items {
            match item {
                Item::Function(func) => {
                    if let Some(tir_func) = self.resolve_function(func) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Struct(struct_decl) => {
                    let tir_struct = self.resolve_struct(struct_decl);
                    tir_module.add_struct(tir_struct);
                }
                Item::Impl(impl_block) => {
                    // Resolve impl block methods with mangled names
                    let struct_name = self.get_type_name(&impl_block.ty);
                    for method in &impl_block.methods {
                        if let Some(mut tir_func) = self.resolve_method(method, &struct_name) {
                            // Mangle the method name: StructName::method_name
                            tir_func.name = format!("{}::{}", struct_name, method.name);
                            tir_module.add_function(tir_func);
                        }
                    }
                }
                // Other items will be added as needed
                _ => {}
            }
        }

        // Transfer type table
        tir_module.type_table = std::mem::take(&mut self.type_table);

        // Preserve data section
        if let Some(data) = module.data_section() {
            tir_module = tir_module.with_data_section(Some(data.to_string()));
        }

        if self.errors.is_empty() {
            Ok(tir_module)
        } else {
            Err(std::mem::take(&mut self.errors))
        }
    }

    /// Resolve all modules to TIR
    ///
    /// This resolves every module (entry and dependencies) to TIR,
    /// enabling TIR-only code generation.
    pub fn resolve_all_modules(
        symbols: &'a SymbolTable,
        modules: &'a HashMap<Vec<String>, Module>,
        entry_path: &[String],
        entry_source: &'a str,
    ) -> Result<HashMap<Vec<String>, TirModule>, Vec<TypeError>> {
        let mut result = HashMap::new();
        let mut all_errors = Vec::new();

        // Create a shared type table
        let mut type_table = TypeTable::new();
        let mut type_aliases = HashMap::new();
        let mut struct_fields = HashMap::new();

        // First pass: collect types from all modules
        for module in modules.values() {
            for item in &module.items {
                match item {
                    Item::Struct(struct_decl) => {
                        let mut fields = Vec::new();
                        for field in &struct_decl.fields {
                            let type_id = Self::resolve_type_static(
                                &field.ty,
                                &mut type_table,
                                &type_aliases,
                            );
                            fields.push((field.name.clone(), type_id));
                        }
                        struct_fields.insert(struct_decl.name.clone(), fields);
                    }
                    Item::Type(type_alias) => {
                        let type_id = Self::resolve_type_static(
                            &type_alias.ty,
                            &mut type_table,
                            &type_aliases,
                        );
                        type_aliases.insert(type_alias.name.clone(), type_id);
                    }
                    _ => {}
                }
            }
        }

        // Second pass: resolve each module with per-module function_return_types and imports
        for (path, module) in modules {
            // Build function_return_types for this module only
            // (functions defined in this module)
            let mut function_return_types = HashMap::new();
            for item in &module.items {
                if let Item::Function(func) = item {
                    let return_type = if let Some(ret_ty) = &func.return_type {
                        Self::resolve_type_static(ret_ty, &mut type_table, &type_aliases)
                    } else {
                        TypeTable::UNIT
                    };
                    function_return_types.insert(func.name.clone(), return_type);
                }
            }

            // Collect imported function names from this module's use declarations
            let mut imported_functions = HashSet::new();
            for item in &module.items {
                if let Item::Use(use_decl) = item {
                    for use_item in &use_decl.items {
                        match use_item {
                            crate::ast::UseItem::Simple { name, alias } => {
                                // Add both original name and alias (if any)
                                imported_functions.insert(name.clone());
                                if let Some(a) = alias {
                                    imported_functions.insert(a.clone());
                                }
                            }
                            crate::ast::UseItem::EffectFunctions { functions, .. } => {
                                // Effect functions are imported by their function name
                                for func_item in functions {
                                    imported_functions.insert(func_item.name.clone());
                                    if let Some(a) = &func_item.alias {
                                        imported_functions.insert(a.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Use entry source only for entry module, empty for others
            let source = if path == entry_path { entry_source } else { "" };

            let mut resolver = Resolver {
                type_table: type_table.clone(),
                symbols,
                loaded_modules: modules,
                type_aliases: type_aliases.clone(),
                struct_fields: struct_fields.clone(),
                function_return_types,
                imported_functions,
                errors: Vec::new(),
                source_code: source,
                current_module_path: Vec::new(), // Set in resolve_module
            };

            match resolver.resolve_module(module, path.clone()) {
                Ok(tir_module) => {
                    // Merge type table updates
                    type_table = tir_module.type_table.clone();
                    result.insert(path.clone(), tir_module);
                }
                Err(errors) => {
                    all_errors.extend(errors);
                }
            }
        }

        if all_errors.is_empty() {
            Ok(result)
        } else {
            Err(all_errors)
        }
    }

    /// Static version of resolve_type for use before the resolver is fully constructed
    fn resolve_type_static(
        ty: &Type,
        type_table: &mut TypeTable,
        type_aliases: &HashMap<String, TypeId>,
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check type aliases first
                if let Some(&alias_type_id) = type_aliases.get(&named.name) {
                    return alias_type_id;
                }

                // Built-in primitives
                match named.name.as_str() {
                    "bool" => TypeTable::BOOL,
                    "char" => TypeTable::CHAR,
                    "i8" => TypeTable::I8,
                    "i16" => TypeTable::I16,
                    "i32" => TypeTable::I32,
                    "i64" => TypeTable::I64,
                    "u8" => TypeTable::U8,
                    "u16" => TypeTable::U16,
                    "u32" => TypeTable::U32,
                    "u64" => TypeTable::U64,
                    "f32" => TypeTable::F32,
                    "f64" => TypeTable::F64,
                    "String" => TypeTable::STRING,
                    "()" => TypeTable::UNIT,
                    "!" => TypeTable::NEVER,
                    _ => TypeTable::UNKNOWN,
                }
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "Option" if !generic.args.is_empty() => {
                    let inner =
                        Self::resolve_type_static(&generic.args[0], type_table, type_aliases);
                    type_table.intern(ResolvedType::Option(inner))
                }
                "Result" if generic.args.len() >= 2 => {
                    let ok = Self::resolve_type_static(&generic.args[0], type_table, type_aliases);
                    let err = Self::resolve_type_static(&generic.args[1], type_table, type_aliases);
                    type_table.intern(ResolvedType::Result { ok, err })
                }
                "Array" if !generic.args.is_empty() => {
                    let elem =
                        Self::resolve_type_static(&generic.args[0], type_table, type_aliases);
                    type_table.intern(ResolvedType::Array(elem))
                }
                _ => TypeTable::UNKNOWN,
            },
            Type::Reference(inner) => {
                let inner_type = Self::resolve_type_static(inner, type_table, type_aliases);
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = Self::resolve_type_static(inner, type_table, type_aliases);
                type_table.make_mut_ref(inner_type)
            }
            _ => TypeTable::UNKNOWN,
        }
    }

    /// Collect type definitions from the module
    fn collect_types(&mut self, module: &Module) {
        // First, collect types from loaded modules (so aliases like Instant = u64 are available)
        for loaded_module in self.loaded_modules.values() {
            for item in &loaded_module.items {
                if let Item::Type(type_alias) = item {
                    // Only add if not already present (main module takes priority)
                    if !self.type_aliases.contains_key(&type_alias.name) {
                        let type_id = self.resolve_type(&type_alias.ty);
                        self.type_aliases.insert(type_alias.name.clone(), type_id);
                    }
                }
            }
        }

        // Then collect from the main module
        for item in &module.items {
            match item {
                Item::Struct(struct_decl) => {
                    let mut fields = Vec::new();
                    for field in &struct_decl.fields {
                        let type_id = self.resolve_type(&field.ty);
                        fields.push((field.name.clone(), type_id));
                    }
                    self.struct_fields.insert(struct_decl.name.clone(), fields);
                }
                Item::Type(type_alias) => {
                    let type_id = self.resolve_type(&type_alias.ty);
                    self.type_aliases.insert(type_alias.name.clone(), type_id);
                }
                _ => {}
            }
        }
    }

    /// Collect function signatures for call resolution
    fn collect_function_signatures(&mut self, module: &Module) {
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let return_type = func
                        .return_type
                        .as_ref()
                        .map(|t| self.resolve_type(t))
                        .unwrap_or(TypeTable::UNIT);
                    self.function_return_types
                        .insert(func.name.clone(), return_type);
                }
                Item::Impl(impl_block) => {
                    // Collect method signatures with mangled names
                    let struct_name = self.get_type_name(&impl_block.ty);
                    for method in &impl_block.methods {
                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|t| self.resolve_type(t))
                            .unwrap_or(TypeTable::UNIT);
                        let mangled_name = format!("{}::{}", struct_name, method.name);
                        self.function_return_types.insert(mangled_name, return_type);
                    }
                }
                _ => {}
            }
        }
    }

    /// Resolve a struct declaration
    fn resolve_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        let mut fields = Vec::new();
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = self.resolve_type(&field.ty);
            fields.push(crate::tir::TirField {
                name: field.name.clone(),
                type_id,
                index: index as u32,
                span: field.span,
            });
        }

        TirStruct {
            name: struct_decl.name.clone(),
            is_pub: struct_decl.is_pub,
            fields,
            span: struct_decl.span,
        }
    }

    /// Resolve a function
    fn resolve_function(&mut self, func: &Function) -> Option<TirFunction> {
        // Resolve return type
        let return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        let mut ctx = FunctionContext::new(return_type);

        // Resolve parameters
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = self.resolve_type(&param.ty);
            let index = ctx.add_local(param.name.clone(), type_id, false);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                span: param.span,
            });
        }

        // Resolve body
        let body = func.body.as_ref().map(|b| self.resolve_block(b, &mut ctx));

        Some(TirFunction {
            name: func.name.clone(),
            is_pub: func.is_pub,
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
        })
    }

    /// Resolve a method (function with &self parameter)
    fn resolve_method(&mut self, func: &Function, struct_name: &str) -> Option<TirFunction> {
        // Resolve return type
        let return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        let mut ctx = FunctionContext::new(return_type);

        // Resolve parameters (including &self)
        let mut params = Vec::new();
        for param in &func.params {
            let is_self = !matches!(param.self_kind, ast::SelfKind::None);
            let type_id = if is_self {
                // &self parameter is a reference to the struct type
                // Use current_module_path so the type is correctly qualified
                self.type_table
                    .make_struct(struct_name.to_string(), self.current_module_path.clone())
            } else {
                self.resolve_type(&param.ty)
            };
            let index = ctx.add_local(param.name.clone(), type_id, false);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                span: param.span,
            });
        }

        // Resolve body
        let body = func.body.as_ref().map(|b| self.resolve_block(b, &mut ctx));

        Some(TirFunction {
            name: func.name.clone(), // Will be mangled by caller
            is_pub: func.is_pub,
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
        })
    }

    /// Get the type name from a Type node
    fn get_type_name(&self, ty: &Type) -> String {
        match ty {
            Type::Named(named) => named.name.clone(),
            Type::Reference(inner) => self.get_type_name(inner),
            Type::MutReference(inner) => self.get_type_name(inner),
            _ => "Unknown".to_string(),
        }
    }

    /// Resolve a block
    fn resolve_block(&mut self, block: &Block, ctx: &mut FunctionContext) -> TirBlock {
        let stmts: Vec<TirStmt> = block
            .stmts
            .iter()
            .flat_map(|s| self.resolve_stmt(s, ctx))
            .collect();
        TirBlock::new(stmts, block.span)
    }

    /// Resolve a statement (may return multiple statements for desugared constructs)
    fn resolve_stmt(&mut self, stmt: &Stmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        match stmt {
            Stmt::Let(let_stmt) => vec![self.resolve_let(let_stmt, ctx)],
            Stmt::Expr(expr_stmt) => vec![self.resolve_expr_stmt(expr_stmt, ctx)],
            Stmt::Return(ret_stmt) => vec![self.resolve_return(ret_stmt, ctx)],
            Stmt::If(if_stmt) => vec![self.resolve_if_stmt(if_stmt, ctx)],
            Stmt::While(while_stmt) => vec![self.resolve_while(while_stmt, ctx)],
            Stmt::For(for_stmt) => self.resolve_for(for_stmt, ctx),
            Stmt::Loop(loop_stmt) => vec![self.resolve_loop(loop_stmt, ctx)],
            Stmt::Break(break_stmt) => vec![self.resolve_break(break_stmt)],
            Stmt::Continue(continue_stmt) => vec![self.resolve_continue(continue_stmt)],
            Stmt::Assert(assert_stmt) => self.resolve_assert(assert_stmt, ctx),
        }
    }

    /// Resolve a let statement
    fn resolve_let(&mut self, let_stmt: &LetStmt, ctx: &mut FunctionContext) -> TirStmt {
        let value = self.resolve_expr(&let_stmt.value, ctx);
        let type_id = let_stmt
            .ty
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(value.type_id);

        let local_index = ctx.add_local(let_stmt.name.clone(), type_id, let_stmt.is_mut);

        TirStmt::new(
            TirStmtKind::Let {
                name: let_stmt.name.clone(),
                local_index,
                is_mut: let_stmt.is_mut,
                is_reactive: let_stmt.is_reactive,
                type_id,
                value,
            },
            let_stmt.span,
        )
    }

    /// Resolve an expression statement
    fn resolve_expr_stmt(&mut self, expr_stmt: &ExprStmt, ctx: &mut FunctionContext) -> TirStmt {
        let expr = self.resolve_expr(&expr_stmt.expr, ctx);
        TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span)
    }

    /// Resolve a return statement
    fn resolve_return(&mut self, ret_stmt: &ReturnStmt, ctx: &mut FunctionContext) -> TirStmt {
        let value = ret_stmt.value.as_ref().map(|e| self.resolve_expr(e, ctx));
        TirStmt::new(TirStmtKind::Return { value }, ret_stmt.span)
    }

    /// Resolve an if statement
    fn resolve_if_stmt(&mut self, if_stmt: &IfStmt, ctx: &mut FunctionContext) -> TirStmt {
        let condition = self.resolve_expr(&if_stmt.condition, ctx);
        let then_block = self.resolve_block(&if_stmt.then_block, ctx);
        let else_block = if_stmt
            .else_block
            .as_ref()
            .map(|b| self.resolve_block(b, ctx));

        TirStmt::new(
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            },
            if_stmt.span,
        )
    }

    /// Resolve a while statement
    fn resolve_while(&mut self, while_stmt: &WhileStmt, ctx: &mut FunctionContext) -> TirStmt {
        let condition = self.resolve_expr(&while_stmt.condition, ctx);
        let body = self.resolve_block(&while_stmt.body, ctx);

        TirStmt::new(TirStmtKind::While { condition, body }, while_stmt.span)
    }

    /// Resolve a for statement - generates init + For node
    /// The For node handles continue correctly (executes update before next iteration)
    fn resolve_for(&mut self, for_stmt: &ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        let mut result = Vec::new();

        // Add init statement if present (e.g., let i = 0)
        if let Some(init_stmt) = &for_stmt.init {
            result.extend(self.resolve_stmt(init_stmt, ctx));
        }

        // Resolve the body
        let body = self.resolve_block(&for_stmt.body, ctx);

        // Resolve condition (None means infinite loop)
        let condition = for_stmt
            .condition
            .as_ref()
            .map(|c| self.resolve_expr(c, ctx));

        // Resolve update expression
        let update = for_stmt.update.as_ref().map(|u| self.resolve_expr(u, ctx));

        // Create For statement
        let for_tir = TirStmt::new(
            TirStmtKind::For {
                condition,
                body,
                update,
            },
            for_stmt.span,
        );
        result.push(for_tir);

        result
    }

    /// Resolve a loop statement (infinite loop)
    fn resolve_loop(&mut self, loop_stmt: &LoopStmt, ctx: &mut FunctionContext) -> TirStmt {
        let body = self.resolve_block(&loop_stmt.body, ctx);
        TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)
    }

    /// Resolve a break statement
    fn resolve_break(&mut self, break_stmt: &BreakStmt) -> TirStmt {
        TirStmt::new(TirStmtKind::Break, break_stmt.span)
    }

    /// Resolve a continue statement
    fn resolve_continue(&mut self, continue_stmt: &ContinueStmt) -> TirStmt {
        TirStmt::new(TirStmtKind::Continue, continue_stmt.span)
    }

    /// Resolve an assert statement - creates TirStmtKind::Assert for power-assert
    fn resolve_assert(
        &mut self,
        assert_stmt: &AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        // Resolve the condition expression
        let condition = self.resolve_expr(&assert_stmt.condition, ctx);

        // Get condition source text
        let condition_source = self.get_source_text(&assert_stmt.condition.span());

        // Resolve optional message
        let message = assert_stmt
            .message
            .as_ref()
            .map(|m| self.resolve_expr(m, ctx));

        // Extract intermediate values for power-assert display
        let mut intermediates = Vec::new();
        self.collect_intermediate_values(&assert_stmt.condition, &mut intermediates, ctx, true);

        // Create Assert statement
        let assert_tir = TirStmt::new(
            TirStmtKind::Assert {
                condition,
                condition_source,
                message,
                intermediates,
            },
            assert_stmt.span,
        );

        vec![assert_tir]
    }

    /// Collect intermediate values from an expression for power-assert display
    fn collect_intermediate_values(
        &mut self,
        expr: &Expr,
        values: &mut Vec<(String, TirExpr, TypeId)>,
        ctx: &mut FunctionContext,
        is_root: bool,
    ) {
        match expr {
            Expr::Binary(bin) => {
                // Recursively collect from operands
                self.collect_intermediate_values(&bin.left, values, ctx, false);
                self.collect_intermediate_values(&bin.right, values, ctx, false);

                // Add the binary expression itself if it's NOT the root comparison
                // (the root is shown as "condition: ..." so we don't need to show it again)
                if !is_root {
                    let source = self.get_source_text(&bin.span);
                    let tir = self.resolve_expr(expr, ctx);
                    let type_id = tir.type_id;
                    values.push((source, tir, type_id));
                }
            }
            Expr::Ident(ident) => {
                // Always show identifiers - they're the most useful values
                let tir = self.resolve_expr(expr, ctx);
                let type_id = tir.type_id;
                values.push((ident.name.clone(), tir, type_id));
            }
            Expr::Call(call) => {
                // Show function call results
                let source = self.get_source_text(&call.span);
                let tir = self.resolve_expr(expr, ctx);
                let type_id = tir.type_id;
                values.push((source, tir, type_id));
            }
            Expr::MethodCall(call) => {
                // Show method call results
                let source = self.get_source_text(&call.span);
                let tir = self.resolve_expr(expr, ctx);
                let type_id = tir.type_id;
                values.push((source, tir, type_id));
            }
            Expr::FieldAccess(access) => {
                // Show field access results
                let source = self.get_source_text(&access.span);
                let tir = self.resolve_expr(expr, ctx);
                let type_id = tir.type_id;
                values.push((source, tir, type_id));
            }
            Expr::Index(idx) => {
                // Show index access results
                let source = self.get_source_text(&idx.span);
                let tir = self.resolve_expr(expr, ctx);
                let type_id = tir.type_id;
                values.push((source, tir, type_id));
            }
            Expr::Unary(unary) => {
                // Recurse into the operand
                self.collect_intermediate_values(&unary.expr, values, ctx, false);
                // Also show the unary expression itself
                let source = self.get_source_text(&unary.span);
                let tir = self.resolve_expr(expr, ctx);
                let type_id = tir.type_id;
                values.push((source, tir, type_id));
            }
            Expr::Cast(cast) => {
                // Recurse into the expression being cast
                self.collect_intermediate_values(&cast.expr, values, ctx, false);
            }
            _ => {
                // Literals and other expressions - don't collect
            }
        }
    }

    /// [Deprecated] Old resolve_assert desugaring - kept for reference
    #[allow(dead_code)]
    fn resolve_assert_old(
        &mut self,
        assert_stmt: &AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let condition = self.resolve_expr(&assert_stmt.condition, ctx);

        // Build the negated condition: !condition
        let negated_condition = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(condition),
            },
            TypeTable::BOOL,
            assert_stmt.span,
        );

        // Build panic call with message
        let message_expr = if let Some(msg) = &assert_stmt.message {
            // User provided message: "Assertion failed: {message}"
            let user_msg = self.resolve_expr(msg, ctx);
            let prefix = TirExpr::new(
                TirExprKind::StringLiteral("Assertion failed: ".to_string()),
                TypeTable::STRING,
                assert_stmt.span,
            );
            TirExpr::new(
                TirExprKind::Call {
                    module_path: vec!["core".to_string(), "internal".to_string()],
                    func_name: "string_concat".to_string(),
                    args: vec![prefix, user_msg],
                },
                TypeTable::STRING,
                assert_stmt.span,
            )
        } else {
            // No message: "Assertion failed:"
            TirExpr::new(
                TirExprKind::StringLiteral("Assertion failed:".to_string()),
                TypeTable::STRING,
                assert_stmt.span,
            )
        };

        // Create panic call: panic(message)
        let panic_call = TirExpr::new(
            TirExprKind::Call {
                module_path: vec!["core".to_string(), "prelude".to_string()],
                func_name: "panic".to_string(),
                args: vec![message_expr],
            },
            TypeTable::NEVER,
            assert_stmt.span,
        );

        // Create if statement: if !cond { panic(msg) }
        let if_stmt = TirStmt::new(
            TirStmtKind::If {
                condition: negated_condition,
                then_block: TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Expr(panic_call),
                        assert_stmt.span,
                    )],
                    assert_stmt.span,
                ),
                else_block: None,
            },
            assert_stmt.span,
        );

        vec![if_stmt]
    }

    /// Resolve an expression
    fn resolve_expr(&mut self, expr: &Expr, ctx: &mut FunctionContext) -> TirExpr {
        match expr {
            Expr::Literal(lit) => self.resolve_literal(lit),
            Expr::Ident(ident) => self.resolve_ident(ident, ctx),
            Expr::Binary(binary) => self.resolve_binary(binary, ctx),
            Expr::Unary(unary) => self.resolve_unary(unary, ctx),
            Expr::Assign(assign) => self.resolve_assign(assign, ctx),
            Expr::Call(call) => self.resolve_call(call, ctx),
            Expr::MethodCall(method_call) => self.resolve_method_call(method_call, ctx),
            Expr::FieldAccess(field_access) => self.resolve_field_access(field_access, ctx),
            Expr::Index(index) => self.resolve_index(index, ctx),
            Expr::Block(block) => {
                let tir_block = self.resolve_block(block, ctx);
                // Block expression type is the last expression's type or Unit
                let type_id = tir_block
                    .stmts
                    .last()
                    .and_then(|s| match &s.kind {
                        TirStmtKind::Expr(e) => Some(e.type_id),
                        _ => None,
                    })
                    .unwrap_or(TypeTable::UNIT);
                TirExpr::new(TirExprKind::Block(tir_block), type_id, block.span)
            }
            Expr::If(if_expr) => self.resolve_if_expr(if_expr, ctx),
            Expr::Match(match_expr) => self.resolve_match_expr(match_expr, ctx),
            Expr::Closure(closure) => self.resolve_closure(closure, ctx),
            Expr::TemplateString(template) => self.resolve_template_string(template, ctx),
            Expr::Cast(cast) => self.resolve_cast(cast, ctx),
            Expr::StructLiteral(struct_lit) => self.resolve_struct_literal(struct_lit, ctx),
            Expr::CompoundAssign(compound) => self.resolve_compound_assign(compound, ctx),
            Expr::ComparisonChain(chain) => self.resolve_comparison_chain(chain, ctx),
        }
    }

    /// Parse an integer literal string into a u64 value
    fn parse_int_literal(repr: &str) -> Result<u64, String> {
        // Remove underscores for parsing
        let clean: String = repr.chars().filter(|&c| c != '_').collect();

        if clean.starts_with("0x") || clean.starts_with("0X") {
            u64::from_str_radix(&clean[2..], 16)
                .map_err(|_| format!("invalid hex literal: {repr}"))
        } else if clean.starts_with("0b") || clean.starts_with("0B") {
            u64::from_str_radix(&clean[2..], 2)
                .map_err(|_| format!("invalid binary literal: {repr}"))
        } else if clean.starts_with("0o") || clean.starts_with("0O") {
            u64::from_str_radix(&clean[2..], 8)
                .map_err(|_| format!("invalid octal literal: {repr}"))
        } else {
            clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))
        }
    }

    /// Parse a float literal string into an f64 value
    fn parse_float_literal(repr: &str) -> Result<f64, String> {
        // Remove underscores for parsing
        let clean: String = repr.chars().filter(|&c| c != '_').collect();
        clean
            .parse()
            .map_err(|_| format!("invalid float literal: {repr}"))
    }

    /// Resolve a literal expression
    fn resolve_literal(&mut self, lit: &ast::LiteralExpr) -> TirExpr {
        let (kind, type_id) = match &lit.value {
            Literal::Int(int_lit) => {
                match Self::parse_int_literal(&int_lit.repr) {
                    Ok(value) => (
                        TirExprKind::IntLiteral {
                            value,
                            repr: int_lit.repr.clone(),
                        },
                        TypeTable::I32,
                    ),
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        // Return 0 as fallback to continue resolution
                        (
                            TirExprKind::IntLiteral {
                                value: 0,
                                repr: int_lit.repr.clone(),
                            },
                            TypeTable::I32,
                        )
                    }
                }
            }
            Literal::Float(float_lit) => {
                match Self::parse_float_literal(&float_lit.repr) {
                    Ok(value) => (
                        TirExprKind::FloatLiteral {
                            value,
                            repr: float_lit.repr.clone(),
                        },
                        TypeTable::F64,
                    ),
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        // Return 0.0 as fallback to continue resolution
                        (
                            TirExprKind::FloatLiteral {
                                value: 0.0,
                                repr: float_lit.repr.clone(),
                            },
                            TypeTable::F64,
                        )
                    }
                }
            }
            Literal::Bool(b) => (TirExprKind::BoolLiteral(*b), TypeTable::BOOL),
            Literal::Char(c) => (TirExprKind::CharLiteral(*c), TypeTable::CHAR),
            Literal::String(s) => (TirExprKind::StringLiteral(s.clone()), TypeTable::STRING),
            Literal::Null => {
                // Null is Option<T> where T is unknown
                let option_unknown = self.type_table.make_option(TypeTable::UNKNOWN);
                (TirExprKind::Null, option_unknown)
            }
            Literal::Unit => (TirExprKind::Unit, TypeTable::UNIT),
        };
        TirExpr::new(kind, type_id, lit.span)
    }

    /// Resolve an identifier expression
    fn resolve_ident(&mut self, ident: &ast::IdentExpr, ctx: &FunctionContext) -> TirExpr {
        // First check local variables
        if let Some(local) = ctx.lookup(&ident.name) {
            return TirExpr::new(
                TirExprKind::Local {
                    index: local.index,
                    name: local.name.clone(),
                },
                local.type_id,
                ident.span,
            );
        }

        // Otherwise it's a global reference (function, constant, etc.)
        // For now, return Unknown type - will be resolved by looking up in symbol table
        TirExpr::new(
            TirExprKind::Global {
                module_path: Vec::new(),
                name: ident.name.clone(),
            },
            TypeTable::UNKNOWN,
            ident.span,
        )
    }

    /// Resolve a binary expression
    fn resolve_binary(&mut self, binary: &ast::BinaryExpr, ctx: &mut FunctionContext) -> TirExpr {
        let left = self.resolve_expr(&binary.left, ctx);
        let right = self.resolve_expr(&binary.right, ctx);

        let op = convert_binary_op(binary.op);

        // Determine result type based on operator
        let type_id = match binary.op {
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or => TypeTable::BOOL,
            _ => left.type_id, // Arithmetic ops preserve the type
        };

        TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            type_id,
            binary.span,
        )
    }

    /// Resolve a unary expression
    fn resolve_unary(&mut self, unary: &ast::UnaryExpr, ctx: &mut FunctionContext) -> TirExpr {
        let expr = self.resolve_expr(&unary.expr, ctx);
        let op = convert_unary_op(unary.op);

        let type_id = match unary.op {
            UnaryOp::Not => TypeTable::BOOL,
            UnaryOp::Ref => self.type_table.make_ref(expr.type_id),
            UnaryOp::Deref => {
                // Dereference returns the inner type
                if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    self.type_table.get(expr.type_id)
                {
                    *inner
                } else {
                    expr.type_id
                }
            }
            _ => expr.type_id,
        };

        TirExpr::new(
            TirExprKind::Unary {
                op,
                expr: Box::new(expr),
            },
            type_id,
            unary.span,
        )
    }

    /// Resolve an assignment expression
    fn resolve_assign(&mut self, assign: &ast::AssignExpr, ctx: &mut FunctionContext) -> TirExpr {
        let target = self.resolve_expr(&assign.target, ctx);
        let value = self.resolve_expr(&assign.value, ctx);

        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value.clone()),
            },
            value.type_id,
            assign.span,
        )
    }

    /// Resolve a compound assignment (already desugared, but handle anyway)
    fn resolve_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // This should have been desugared, but handle it anyway
        let target = self.resolve_expr(&compound.target, ctx);
        let value = self.resolve_expr(&compound.value, ctx);

        let op = match compound.op {
            ast::CompoundAssignOp::Add => TirBinaryOp::Add,
            ast::CompoundAssignOp::Sub => TirBinaryOp::Sub,
            ast::CompoundAssignOp::Mul => TirBinaryOp::Mul,
            ast::CompoundAssignOp::Div => TirBinaryOp::Div,
            ast::CompoundAssignOp::Mod => TirBinaryOp::Mod,
        };

        // target = target op value
        let binary = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(target.clone()),
                op,
                right: Box::new(value),
            },
            target.type_id,
            compound.span,
        );

        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(binary),
            },
            TypeTable::UNIT,
            compound.span,
        )
    }

    /// Resolve a comparison chain (already desugared, but handle anyway)
    fn resolve_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // This should have been desugared to binary && chain
        // Just resolve the first expression for now
        self.resolve_expr(&chain.first, ctx)
    }

    /// Resolve a call expression
    fn resolve_call(&mut self, call: &ast::CallExpr, ctx: &mut FunctionContext) -> TirExpr {
        let args: Vec<TirExpr> = call
            .args
            .iter()
            .map(|a| self.resolve_expr(a, ctx))
            .collect();

        // Get function name from callee
        let (module_path, func_name, is_known) = match &call.callee {
            Expr::Ident(ident) => {
                // Check for qualified name with :: (e.g., "Stdout::write_via_stream")
                // Parser creates a single ident for Effect::operation syntax
                if let Some(pos) = ident.name.find("::") {
                    let prefix = &ident.name[..pos];
                    let suffix = &ident.name[pos + 2..];

                    // Builtin functions are always allowed
                    if prefix == "builtin" {
                        (Vec::new(), ident.name.clone(), true)
                    }
                    // Effect operations and other qualified calls - always allowed
                    // (validated by effect system/codegen)
                    else {
                        (vec![prefix.to_string()], suffix.to_string(), true)
                    }
                }
                // First, check if it's a local function (defined in this module)
                else if self.function_return_types.contains_key(&ident.name) {
                    (Vec::new(), ident.name.clone(), true)
                }
                // Check for built-in type constructors (Ok, Err, Some, None)
                // These are variant constructors, generated inline
                else if matches!(ident.name.as_str(), "Ok" | "Err" | "Some" | "None") {
                    (Vec::new(), ident.name.clone(), true)
                }
                // Check for prelude functions (panic, unreachable)
                // These are actual functions in core::prelude
                else if matches!(ident.name.as_str(), "panic" | "unreachable") {
                    (
                        vec!["core".to_string(), "prelude".to_string()],
                        ident.name.clone(),
                        true,
                    )
                }
                // Check if this is an imported function (per-module imports)
                else if self.imported_functions.contains(&ident.name) {
                    // Get module path from symbol table for codegen
                    if let Some(symbol) = self.symbols.lookup(&ident.name) {
                        (symbol.module_path.clone(), symbol.name.clone(), true)
                    } else {
                        // Imported but not in symbols - shouldn't happen but allow
                        (Vec::new(), ident.name.clone(), true)
                    }
                } else {
                    // Unknown function - will report error
                    (Vec::new(), ident.name.clone(), false)
                }
            }
            Expr::FieldAccess(field_access) => {
                // e.g., Stdout.write (unlikely but possible)
                // These are always considered known - validated elsewhere
                if let Expr::Ident(ident) = &field_access.expr {
                    (vec![ident.name.clone()], field_access.field.clone(), true)
                } else {
                    (Vec::new(), String::from("unknown"), false)
                }
            }
            _ => (Vec::new(), String::from("unknown"), false),
        };

        // Report error for unknown functions
        if !is_known {
            self.errors.push(TypeError::UnknownFunction {
                name: func_name.clone(),
                span: call.span,
            });
        }

        // Look up function return type
        let return_type = self.lookup_function_return_type(&module_path, &func_name);

        TirExpr::new(
            TirExprKind::Call {
                module_path,
                func_name,
                args,
            },
            return_type,
            call.span,
        )
    }

    /// Look up the return type of a function
    fn lookup_function_return_type(&self, module_path: &[String], func_name: &str) -> TypeId {
        // Handle builtin functions (builtin::name pattern)
        if let Some(builtin_name) = func_name.strip_prefix("builtin::") {
            // Skip "builtin::"
            return self.get_builtin_return_type(builtin_name);
        }

        // First, try local functions (no module path)
        if module_path.is_empty()
            && let Some(&return_type) = self.function_return_types.get(func_name)
        {
            return return_type;
        }

        // Try looking up in loaded modules
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(module_path)
        {
            for item in &module.items {
                if let Item::Function(func) = item
                    && func.name == func_name
                {
                    return func
                        .return_type
                        .as_ref()
                        .map(|t| self.resolve_type_no_register(t))
                        .unwrap_or(TypeTable::UNIT);
                }
            }
        }

        // Default to UNIT for unknown functions (they might be external/builtin)
        TypeTable::UNIT
    }

    /// Get the return type of a builtin function
    fn get_builtin_return_type(&self, name: &str) -> TypeId {
        match name {
            // Array operations
            "array_len" => TypeTable::I32,
            "array_get_u8" => TypeTable::I32, // Returns u8 as i32
            "array_set_u8" => TypeTable::UNIT,
            "string_new" => TypeTable::STRING,

            // Memory operations
            "realloc" => TypeTable::I32, // Returns pointer (i32)
            "memory_load8_u" => TypeTable::I32,
            "memory_store8" => TypeTable::UNIT,

            // Float-to-string (returns length)
            "f64_to_buffer" | "f32_to_buffer" => TypeTable::I32,

            // Bitwise operations
            "i32_and" | "i32_or" | "i32_xor" | "i32_shl" | "i32_shr_u" | "i32_shr_s" => {
                TypeTable::I32
            }
            "i32_eqz" => TypeTable::BOOL,

            // Control flow / hints
            "likely" | "unlikely" => TypeTable::BOOL,
            "unreachable" | "effect_wait" => TypeTable::NEVER,

            // Stream builtins for IO
            "call_indirect_stdout_write_via_stream" | "call_indirect_stderr_write_via_stream" => {
                TypeTable::UNIT
            }

            // WASI stream operations
            "stream_new" => TypeTable::I64, // Returns i64 (two i32s packed)
            "stream_write" => TypeTable::I32, // Returns result code
            "stream_drop_writable" | "stream_drop_readable" => TypeTable::UNIT,

            // Unknown builtin - default to UNIT
            _ => TypeTable::UNIT,
        }
    }

    /// Resolve a type without registering new types
    fn resolve_type_no_register(&self, ty: &Type) -> TypeId {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i8" => TypeTable::I8,
                "i16" => TypeTable::I16,
                "i32" => TypeTable::I32,
                "i64" => TypeTable::I64,
                "i128" => TypeTable::I128,
                "u8" => TypeTable::U8,
                "u16" => TypeTable::U16,
                "u32" => TypeTable::U32,
                "u64" => TypeTable::U64,
                "u128" => TypeTable::U128,
                "f32" => TypeTable::F32,
                "f64" => TypeTable::F64,
                "bool" => TypeTable::BOOL,
                "char" => TypeTable::CHAR,
                "String" => TypeTable::STRING,
                "!" => TypeTable::NEVER,
                "()" => TypeTable::UNIT,
                _ => {
                    // Check type aliases (e.g., Instant = u64, Duration = u64)
                    if let Some(&type_id) = self.type_aliases.get(&named.name) {
                        type_id
                    } else {
                        TypeTable::UNKNOWN
                    }
                }
            },
            _ => TypeTable::UNKNOWN,
        }
    }

    /// Resolve a method call
    fn resolve_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let receiver = self.resolve_expr(&method_call.receiver, ctx);
        let args: Vec<TirExpr> = method_call
            .args
            .iter()
            .map(|a| self.resolve_expr(a, ctx))
            .collect();

        // Look up method return type based on receiver type
        let return_type = self.lookup_method_return_type(receiver.type_id, &method_call.method);

        TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                method_name: method_call.method.clone(),
                args,
            },
            return_type,
            method_call.span,
        )
    }

    /// Look up method return type based on receiver type and method name
    fn lookup_method_return_type(&self, receiver_type: TypeId, method_name: &str) -> TypeId {
        // Get the struct name and module path from the receiver type
        let (struct_name, module_path) = match self.type_table.get(receiver_type) {
            ResolvedType::Struct { name, module_path } => (name.clone(), module_path.clone()),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                if let ResolvedType::Struct { name, module_path } = self.type_table.get(*inner) {
                    (name.clone(), module_path.clone())
                } else {
                    return TypeTable::UNKNOWN;
                }
            }
            // Primitive types have built-in methods like to_string()
            ResolvedType::Primitive(_) => {
                if method_name == "to_string" {
                    return TypeTable::STRING;
                }
                return TypeTable::UNKNOWN;
            }
            _ => return TypeTable::UNKNOWN,
        };

        // Build the mangled method name and look it up locally first
        let mangled_name = format!("{}::{}", struct_name, method_name);
        if let Some(&return_type) = self.function_return_types.get(&mangled_name) {
            return return_type;
        }

        // Try looking up in loaded modules (for imported structs)
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(&module_path)
        {
            for item in &module.items {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            if method.name == method_name {
                                return method
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_type_no_register(t))
                                    .unwrap_or(TypeTable::UNIT);
                            }
                        }
                    }
                }
            }
        }

        TypeTable::UNKNOWN
    }

    /// Resolve a field access
    fn resolve_field_access(
        &mut self,
        field_access: &ast::FieldAccessExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let expr = self.resolve_expr(&field_access.expr, ctx);

        // Look up field type from struct type
        let (field_index, field_type) =
            self.lookup_field_type(expr.type_id, &field_access.field, field_access.span);

        TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(expr),
                field_index,
                field_name: field_access.field.clone(),
            },
            field_type,
            field_access.span,
        )
    }

    /// Look up field type from a struct type
    fn lookup_field_type(
        &self,
        struct_type: TypeId,
        field_name: &str,
        _span: Span,
    ) -> (u32, TypeId) {
        if let ResolvedType::Struct { name, .. } = self.type_table.get(struct_type)
            && let Some(fields) = self.struct_fields.get(name)
        {
            for (index, (fname, ftype)) in fields.iter().enumerate() {
                if fname == field_name {
                    return (index as u32, *ftype);
                }
            }
        }
        (0, TypeTable::UNKNOWN)
    }

    /// Resolve an index expression
    fn resolve_index(&mut self, index: &ast::IndexExpr, ctx: &mut FunctionContext) -> TirExpr {
        let expr = self.resolve_expr(&index.expr, ctx);
        let index_expr = self.resolve_expr(&index.index, ctx);

        // Get element type from array type
        let element_type = if let ResolvedType::Array(elem) = self.type_table.get(expr.type_id) {
            *elem
        } else {
            TypeTable::UNKNOWN
        };

        TirExpr::new(
            TirExprKind::Index {
                expr: Box::new(expr),
                index: Box::new(index_expr),
            },
            element_type,
            index.span,
        )
    }

    /// Resolve an if expression
    fn resolve_if_expr(&mut self, if_expr: &IfExpr, ctx: &mut FunctionContext) -> TirExpr {
        let condition = self.resolve_expr(&if_expr.condition, ctx);
        let then_block = self.resolve_block(&if_expr.then_block, ctx);
        let else_block = if_expr
            .else_block
            .as_ref()
            .map(|b| self.resolve_block(b, ctx));

        // If expression type is the type of the branches
        let type_id = then_block
            .stmts
            .last()
            .and_then(|s| match &s.kind {
                TirStmtKind::Expr(e) => Some(e.type_id),
                _ => None,
            })
            .unwrap_or(TypeTable::UNIT);

        TirExpr::new(
            TirExprKind::If {
                condition: Box::new(condition),
                then_branch: then_block,
                else_branch: else_block,
            },
            type_id,
            if_expr.span,
        )
    }

    /// Resolve a match expression
    fn resolve_match_expr(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let expr = self.resolve_expr(&match_expr.expr, ctx);
        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| self.resolve_match_arm(arm, ctx))
            .collect();

        // Match expression type is the type of the first arm body
        let type_id = arms
            .first()
            .map(|a| a.body.type_id)
            .unwrap_or(TypeTable::UNIT);

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(expr),
                arms,
            },
            type_id,
            match_expr.span,
        )
    }

    /// Resolve a match arm
    fn resolve_match_arm(&mut self, arm: &MatchArm, ctx: &mut FunctionContext) -> TirMatchArm {
        let pattern = self.resolve_pattern(&arm.pattern, ctx);
        let body = self.resolve_expr(&arm.body, ctx);

        TirMatchArm {
            pattern,
            body,
            span: arm.span,
        }
    }

    /// Resolve a pattern
    fn resolve_pattern(&mut self, pattern: &Pattern, ctx: &mut FunctionContext) -> TirPattern {
        match pattern {
            Pattern::Wildcard => TirPattern::Wildcard,
            Pattern::Ident(name) => {
                // Create a local for the binding
                let index = ctx.add_local(name.clone(), TypeTable::UNKNOWN, false);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index: index,
                }
            }
            Pattern::Literal(lit) => {
                let tir_lit = match lit {
                    Literal::Int(i) => {
                        // Parse the integer literal
                        match Self::parse_int_literal(&i.repr) {
                            Ok(value) => TirLiteralPattern::Int(value),
                            Err(_) => {
                                // Error was already reported during literal resolution
                                TirLiteralPattern::Int(0)
                            }
                        }
                    }
                    Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    Literal::Char(c) => TirLiteralPattern::Char(*c),
                    Literal::String(s) => TirLiteralPattern::String(s.clone()),
                    Literal::Null => TirLiteralPattern::Null,
                    _ => TirLiteralPattern::Null,
                };
                TirPattern::Literal(tir_lit)
            }
            Pattern::Tuple(patterns) => {
                let resolved: Vec<TirPattern> = patterns
                    .iter()
                    .map(|p| self.resolve_pattern(p, ctx))
                    .collect();
                TirPattern::Tuple(resolved)
            }
        }
    }

    /// Resolve a closure
    fn resolve_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        _ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Create a new context for the closure
        let mut closure_ctx = FunctionContext::new(TypeTable::UNKNOWN);

        // Add closure parameters
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .map(|p| {
                let type_id =
                    p.ty.as_ref()
                        .map(|t| self.resolve_type(t))
                        .unwrap_or(TypeTable::UNKNOWN);
                closure_ctx.add_local(p.name.clone(), type_id, false);
                (p.name.clone(), type_id)
            })
            .collect();

        // Resolve body
        let body = self.resolve_expr(&closure.body, &mut closure_ctx);

        // TODO: Capture analysis
        let captures = Vec::new();

        // Create function type
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self
            .type_table
            .make_function(param_types, body.type_id, Vec::new());

        TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
            },
            func_type,
            closure.span,
        )
    }

    /// Resolve a template string
    /// Resolve a template string - desugars to string concatenation
    /// `Hello, {name}!` → string_concat("Hello, ", to_string(name), "!")
    fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Collect all parts as expressions
        let mut parts: Vec<TirExpr> = Vec::new();

        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    if !s.is_empty() {
                        parts.push(TirExpr::new(
                            TirExprKind::StringLiteral(s.clone()),
                            TypeTable::STRING,
                            template.span,
                        ));
                    }
                }
                ast::TemplatePart::Interpolation { expr, format: _ } => {
                    // Resolve the expression
                    let resolved = self.resolve_expr(expr, ctx);
                    // TODO: handle format specifiers
                    // For now, wrap in to_string if not already a string
                    let string_expr = if resolved.type_id == TypeTable::STRING {
                        resolved
                    } else {
                        // Call to_string method
                        TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(resolved.clone()),
                                method_name: "to_string".to_string(),
                                args: vec![],
                            },
                            TypeTable::STRING,
                            expr.span(),
                        )
                    };
                    parts.push(string_expr);
                }
            }
        }

        // If only one part, return it directly
        if parts.len() == 1 {
            return parts.remove(0);
        }

        // If empty, return empty string
        if parts.is_empty() {
            return TirExpr::new(
                TirExprKind::StringLiteral(String::new()),
                TypeTable::STRING,
                template.span,
            );
        }

        // Build a chain of pairwise string concatenations: concat(concat(a, b), c)
        // string_concat only takes 2 arguments, so we chain them
        let mut result = parts.remove(0);
        for part in parts {
            result = TirExpr::new(
                TirExprKind::Call {
                    module_path: vec!["core".to_string(), "internal".to_string()],
                    func_name: "string_concat".to_string(),
                    args: vec![result, part],
                },
                TypeTable::STRING,
                template.span,
            );
        }
        result
    }

    /// Resolve a cast expression
    fn resolve_cast(&mut self, cast: &ast::CastExpr, ctx: &mut FunctionContext) -> TirExpr {
        let expr = self.resolve_expr(&cast.expr, ctx);
        let target_type = self.resolve_type(&cast.target_type);

        TirExpr::new(
            TirExprKind::Cast {
                expr: Box::new(expr),
                target_type,
            },
            target_type,
            cast.span,
        )
    }

    /// Resolve a struct literal
    fn resolve_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Look up the struct in the symbol table to resolve imports/aliases
        let (struct_name, module_path) = if let Some(symbol) = self.symbols.lookup(&struct_lit.name)
        {
            match &symbol.kind {
                crate::symbol::SymbolKind::Struct(_) => {
                    (symbol.name.clone(), symbol.module_path.clone())
                }
                _ => (struct_lit.name.clone(), Vec::new()),
            }
        } else {
            // Fall back to local struct (no module path)
            (struct_lit.name.clone(), Vec::new())
        };

        let struct_type = self
            .type_table
            .make_struct(struct_name.clone(), module_path);

        let fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let value = self.resolve_expr(&field.value, ctx);
                TirStructField {
                    name: field.name.clone(),
                    value,
                    field_index: index as u32,
                }
            })
            .collect();

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            },
            struct_type,
            struct_lit.span,
        )
    }

    /// Resolve a type from AST Type to TypeId
    fn resolve_type(&mut self, ty: &Type) -> TypeId {
        match ty {
            Type::Named(named) => self.resolve_named_type(&named.name, named.span),
            Type::Generic(generic) => self.resolve_generic_type(&generic.name, &generic.args),
            Type::Function(func_ty) => {
                let params: Vec<TypeId> = func_ty
                    .params
                    .iter()
                    .map(|p| self.resolve_type(p))
                    .collect();
                let return_type = self.resolve_type(&func_ty.return_type);
                self.type_table
                    .make_function(params, return_type, func_ty.effects.clone())
            }
            Type::Tuple(elements) => {
                let elem_types: Vec<TypeId> =
                    elements.iter().map(|e| self.resolve_type(e)).collect();
                self.type_table.make_tuple(elem_types)
            }
            Type::Reference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.type_table.make_mut_ref(inner_type)
            }
        }
    }

    /// Resolve a named type
    fn resolve_named_type(&mut self, name: &str, _span: Span) -> TypeId {
        match name {
            // Primitives
            "i8" => TypeTable::I8,
            "i16" => TypeTable::I16,
            "i32" => TypeTable::I32,
            "i64" => TypeTable::I64,
            "i128" => TypeTable::I128,
            "u8" => TypeTable::U8,
            "u16" => TypeTable::U16,
            "u32" => TypeTable::U32,
            "u64" => TypeTable::U64,
            "u128" => TypeTable::U128,
            "f32" => TypeTable::F32,
            "f64" => TypeTable::F64,
            "bool" => TypeTable::BOOL,
            "char" => TypeTable::CHAR,
            "()" => TypeTable::UNIT,
            "!" => TypeTable::NEVER,
            "String" => TypeTable::STRING,

            // Check type aliases
            _ => {
                if let Some(&type_id) = self.type_aliases.get(name) {
                    type_id
                } else if self.struct_fields.contains_key(name) {
                    // It's a struct defined in the current module (or a previously collected module)
                    // Use current_module_path to properly qualify the type
                    self.type_table
                        .make_struct(name.to_string(), self.current_module_path.clone())
                } else {
                    // Unknown type
                    TypeTable::UNKNOWN
                }
            }
        }
    }

    /// Resolve a generic type
    fn resolve_generic_type(&mut self, name: &str, args: &[Type]) -> TypeId {
        match name {
            "Array" | "Vec" => {
                let elem_type = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.make_array(elem_type)
            }
            "Option" => {
                let inner = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.make_option(inner)
            }
            "Result" => {
                let ok = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                let err = args
                    .get(1)
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.make_result(ok, err)
            }
            "Stream" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.intern(ResolvedType::Stream(elem))
            }
            "Future" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.intern(ResolvedType::Future(elem))
            }
            "Dict" => {
                let key = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                let value = args
                    .get(1)
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.intern(ResolvedType::Dict { key, value })
            }
            _ => TypeTable::UNKNOWN,
        }
    }

    /// Get the type table (after resolution)
    pub fn into_type_table(self) -> TypeTable {
        self.type_table
    }
}

/// Convert AST BinaryOp to TIR BinaryOp
fn convert_binary_op(op: BinaryOp) -> TirBinaryOp {
    match op {
        BinaryOp::Add => TirBinaryOp::Add,
        BinaryOp::Sub => TirBinaryOp::Sub,
        BinaryOp::Mul => TirBinaryOp::Mul,
        BinaryOp::Div => TirBinaryOp::Div,
        BinaryOp::Mod => TirBinaryOp::Mod,
        BinaryOp::Eq => TirBinaryOp::Eq,
        BinaryOp::NotEq => TirBinaryOp::NotEq,
        BinaryOp::Lt => TirBinaryOp::Lt,
        BinaryOp::LtEq => TirBinaryOp::LtEq,
        BinaryOp::Gt => TirBinaryOp::Gt,
        BinaryOp::GtEq => TirBinaryOp::GtEq,
        BinaryOp::And => TirBinaryOp::And,
        BinaryOp::Or => TirBinaryOp::Or,
        BinaryOp::BitAnd => TirBinaryOp::BitAnd,
        BinaryOp::BitOr => TirBinaryOp::BitOr,
        BinaryOp::BitXor => TirBinaryOp::BitXor,
        BinaryOp::Shl => TirBinaryOp::Shl,
        BinaryOp::Shr => TirBinaryOp::Shr,
    }
}

/// Convert AST UnaryOp to TIR UnaryOp
fn convert_unary_op(op: UnaryOp) -> TirUnaryOp {
    match op {
        UnaryOp::Neg => TirUnaryOp::Neg,
        UnaryOp::Not => TirUnaryOp::Not,
        UnaryOp::BitNot => TirUnaryOp::BitNot,
        UnaryOp::Ref => TirUnaryOp::Ref,
        UnaryOp::Deref => TirUnaryOp::Deref,
    }
}

/// Convenience function to resolve a module
pub fn resolve_module(
    module: &Module,
    module_path: Vec<String>,
    symbols: &SymbolTable,
    loaded_modules: &HashMap<Vec<String>, Module>,
    source_code: &str,
) -> Result<TirModule, Vec<TypeError>> {
    let mut resolver = Resolver::new(symbols, loaded_modules, source_code);
    resolver.resolve_module(module, module_path)
}
