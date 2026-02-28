//! Expression resolution (literals, identifiers, field access, index,
//! if-expressions, match, cast, struct/tuple literals, etc.).

use indexmap::{IndexMap, IndexSet};

use crate::ast::{self, Expr, IfExpr, Item, Literal, MatchArm};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource, mangle_generic_name};
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirMatchArm, TirStmt, TirStmtKind,
    TirStructField, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::types::{FunctionContext, LabeledBlockTarget, TypeError, VarRef};
use super::util;

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_expr(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Try literal coercion when expected type is known
        if let Some(target_type) = expected_type
            && let Some(coerced) = self.try_coerce(expr, ctx, target_type)
        {
            return coerced;
        }

        // Main expression dispatch
        match expr {
            Expr::Literal(lit) => self.resolve_literal(lit, ctx),
            Expr::Ident(ident) => self.resolve_ident(ident, ctx),
            Expr::Binary(binary) => self.resolve_binary(binary, ctx, expected_type),
            Expr::Unary(unary) => self.resolve_unary(unary, ctx),
            Expr::Assign(assign) => self.resolve_assign(assign, ctx),
            Expr::Call(call) => self.resolve_call(call, ctx),
            Expr::MethodCall(method_call) => self.resolve_method_call(method_call, ctx),
            Expr::StaticMethodCall(static_call) => {
                self.resolve_static_method_call(static_call, ctx)
            }
            Expr::FieldAccess(field_access) => self.resolve_field_access(field_access, ctx),
            Expr::Index(index) => self.resolve_index(index, ctx),
            Expr::Block(block) => {
                let tir_block = self.resolve_block(block, ctx, None);
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
            Expr::If(if_expr) => self.resolve_if_expr(if_expr, ctx, expected_type),
            Expr::Match(match_expr) => self.resolve_match_expr(match_expr, ctx, expected_type),
            Expr::Closure(closure) => self.resolve_closure(closure, ctx),
            Expr::TemplateString(template) => self.resolve_template_string(template, ctx),
            Expr::Cast(cast) => self.resolve_cast(cast, ctx),
            Expr::StructLiteral(struct_lit) => self.resolve_struct_literal(struct_lit, ctx),
            Expr::CompoundAssign(compound) => self.resolve_compound_assign(compound, ctx),
            Expr::ComparisonChain(chain) => self.resolve_comparison_chain(chain, ctx),
            Expr::TupleLiteral(tuple_lit) => self.resolve_tuple_literal(tuple_lit, ctx),
            Expr::LabeledBlock(lb) => {
                ctx.labeled_block_targets.push(LabeledBlockTarget {
                    label: lb.label.clone(),
                    break_types: Vec::new(),
                });
                ctx.active_labels.push(lb.label.clone());

                ctx.enter_scope();
                let tir_block = self.resolve_block(&lb.block, ctx, None);
                ctx.exit_scope();

                ctx.active_labels.pop();
                let target = ctx.labeled_block_targets.pop().unwrap();

                let result_type = if target.break_types.is_empty() {
                    TypeTable::UNIT
                } else {
                    // TODO: type unification for multiple breaks
                    target.break_types[0]
                };

                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: lb.label.clone(),
                        block: tir_block,
                        result_type,
                    },
                    result_type,
                    lb.span,
                )
            }
            Expr::Matches(_) => {
                panic!("Matches expression should have been desugared to if-let before resolver")
            }
        }
    }

    /// Resolve a type without registering new types
    /// This is used for lookups where we need immutable access. It only handles
    /// primitive types and newtypes. For generic types, use `resolve_type` instead.
    /// Resolve a method call

    pub(super) fn resolve_literal(
        &mut self,
        lit: &ast::LiteralExpr,
        ctx: &FunctionContext,
    ) -> TirExpr {
        let (kind, type_id) = match &lit.value {
            Literal::Number(num_lit) => {
                // Default type: i32 if integer-compatible, f64 if float-only
                if util::is_float_only_literal(&num_lit.repr) {
                    // Must be float (has decimal point or negative exponent)
                    match util::parse_float_literal(&num_lit.repr) {
                        Ok(value) => (
                            TirExprKind::FloatLiteral {
                                value,
                                repr: num_lit.repr.clone(),
                            },
                            TypeTable::F64,
                        ),
                        Err(message) => {
                            let _ = self.logger.error(TypeError::InvalidLiteral {
                                message,
                                span: lit.span,
                            });
                            (
                                TirExprKind::FloatLiteral {
                                    value: 0.0,
                                    repr: num_lit.repr.clone(),
                                },
                                TypeTable::F64,
                            )
                        }
                    }
                } else {
                    // Can be integer (default to i32)
                    match util::parse_u128_literal(&num_lit.repr) {
                        Ok(value) => (
                            TirExprKind::IntLiteral {
                                value: value as u64,
                                repr: num_lit.repr.clone(),
                            },
                            TypeTable::I32,
                        ),
                        Err(message) => {
                            let _ = self.logger.error(TypeError::InvalidLiteral {
                                message,
                                span: lit.span,
                            });
                            (
                                TirExprKind::IntLiteral {
                                    value: 0,
                                    repr: num_lit.repr.clone(),
                                },
                                TypeTable::I32,
                            )
                        }
                    }
                }
            }
            Literal::Bool(b) => (TirExprKind::BoolLiteral(*b), TypeTable::BOOL),
            Literal::Char(c) => (TirExprKind::CharLiteral(*c), TypeTable::CHAR),
            Literal::String(s) => {
                let string_type = self.get_string_struct_type();
                (TirExprKind::StringLiteral(s.value.clone()), string_type)
            }
            Literal::Null => {
                let option_unknown = self.type_table.borrow_mut().make_option(TypeTable::UNKNOWN);
                (TirExprKind::Null, option_unknown)
            }
            Literal::Unit => (TirExprKind::Unit, TypeTable::UNIT),
            Literal::LocationFile => {
                // #file - returns the current module source as a string
                let file_path = self.current_module_source.to_string();
                let string_type = self.get_string_struct_type();
                (TirExprKind::StringLiteral(file_path), string_type)
            }
            Literal::LocationLine => {
                // #line - returns the line number (1-indexed)
                let line = lit.span.line as u64;
                (
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    TypeTable::I32,
                )
            }
            Literal::LocationFunction => {
                // #function - returns the current function name
                let string_type = self.get_string_struct_type();
                (
                    TirExprKind::StringLiteral(ctx.function_name.clone()),
                    string_type,
                )
            }
            Literal::DataSection => {
                // #data - returns the __DATA__ section content as a String
                let data = self
                    .loaded_modules
                    .get(&self.current_module_source)
                    .and_then(|m| m.data_section())
                    .map(str::to_owned);
                let string_type = self.get_string_struct_type();
                if let Some(content) = data {
                    (TirExprKind::StringLiteral(content), string_type)
                } else {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: "`#data` requires a `__DATA__` section in the source file"
                            .to_owned(),
                        span: lit.span,
                    });
                    (TirExprKind::StringLiteral(String::new()), string_type)
                }
            }
        };
        TirExpr::new(kind, type_id, lit.span)
    }

    /// Resolve an identifier expression
    pub(super) fn resolve_ident(
        &mut self,
        ident: &ast::IdentExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Check local variables, including captures from outer scope
        if let Some(var_ref) = ctx.lookup_or_capture(&ident.name) {
            match var_ref {
                VarRef::Local { index, type_id } => {
                    return TirExpr::new(
                        TirExprKind::Local {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                VarRef::Capture { index, type_id } => {
                    return TirExpr::new(
                        TirExprKind::Capture {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                VarRef::DerefCapture {
                    index,
                    ref_type_id,
                    inner_type_id,
                } => {
                    // Deref capture: `*self.__capture_N` where the field holds `&mut T`
                    let capture_expr = TirExpr::new(
                        TirExprKind::Capture {
                            index,
                            name: format!("__deref_cap_{index}"),
                        },
                        ref_type_id,
                        ident.span,
                    );
                    return TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(capture_expr),
                        },
                        inner_type_id,
                        ident.span,
                    );
                }
            }
        }

        // Check for associated constants (e.g., f64::PI, i32::MAX)
        if let Some((const_ty, const_expr)) = self.associated_constants.get(&ident.name).cloned() {
            let type_id = self.resolve_type(&const_ty);
            let resolved = self.resolve_expr(&const_expr, ctx, Some(type_id));
            return TirExpr::new(resolved.kind, type_id, ident.span);
        }

        // Check for qualified variant case names like Color::Red (without parentheses)
        if let Some(pos) = ident.name.find("::") {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];

            if let Some(variant_info) = self.variant_cases.get(prefix) {
                // Find the case by name
                if let Some((case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == suffix)
                {
                    // Unit variant - payload must be unit type
                    let payload_is_unit = matches!(
                        self.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    if !payload_is_unit {
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
                            expected: 1,
                            found: 0,
                            span: ident.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span);
                    }

                    // Create variant type
                    let variant_type = self
                        .type_table
                        .borrow_mut()
                        .make_variant(prefix.to_string(), variant_info.module_source.clone());

                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload: None, // Unit variant has no explicit payload
                        },
                        variant_type,
                        ident.span,
                    );
                }
            }

            // Check for enum case: Color::Red (enums have no payload)
            if let Some(enum_info) = self.enum_cases.get(prefix)
                && let Some(case_data) = enum_info.cases.iter().find(|c| c.name == suffix)
            {
                // Create enum type
                let enum_type = self
                    .type_table
                    .borrow_mut()
                    .make_enum(prefix.to_string(), enum_info.module_source.clone());

                return TirExpr::new(
                    TirExprKind::EnumConstruct {
                        enum_type,
                        case_index: case_data.index,
                        case_name: case_data.name.clone(),
                    },
                    enum_type,
                    ident.span,
                );
            }

            // Check for flags member: PathFlags::SymlinkFollow
            // Flags members are bitmask integers (1 << index) represented as IntLiteral
            if let Some(flags_info) = self.flags_cases.get(prefix)
                && let Some(member) = flags_info.members.iter().find(|m| m.name == suffix)
            {
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: u64::from(member.bitmask),
                        repr: member.bitmask.to_string(),
                    },
                    flags_info.type_id,
                    ident.span,
                );
            }
        }

        // Check for global variables in current module
        if let Some(&(ty, _mutable)) = self.current_module_globals.get(&ident.name) {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                ty,
                ident.span,
            );
        }

        // Check for imported global variables
        if let Some((source_module, original_name, ty, _mutable)) =
            self.imported_globals.get(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: source_module.clone(),
                    name: original_name.clone(),
                },
                *ty,
                ident.span,
            );
        }

        // Check if it's a known function (function reference)
        if self.function_return_types.contains_key(&ident.name)
            || self.imported_functions.contains(&ident.name)
        {
            // It's a function reference - return Global
            // The function type and proper handling will be done later
            let module_source = if self.function_return_types.contains_key(&ident.name) {
                self.current_module_source.clone()
            } else {
                // For imported functions, look up the module source from symbols
                self.symbols
                    .lookup(&ident.name)
                    .map(|s| s.module_source.clone())
                    .unwrap_or_else(|| self.current_module_source.clone())
            };
            return TirExpr::new(
                TirExprKind::Global {
                    module_source,
                    name: ident.name.clone(),
                },
                TypeTable::UNKNOWN,
                ident.span,
            );
        }

        // Check if it's a prelude function (panic, unreachable)
        // These are defined in core:internal and re-exported by core:prelude
        if matches!(ident.name.as_str(), "panic" | "unreachable") {
            return TirExpr::new(
                TirExprKind::Global {
                    module_source: ModuleSource::internal(),
                    name: ident.name.clone(),
                },
                TypeTable::UNKNOWN,
                ident.span,
            );
        }

        // Unknown variable - report error
        let _ = self.logger.error(TypeError::UnknownIdentifier {
            name: ident.name.clone(),
            span: ident.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span)
    }

    /// Resolve a binary expression

    pub(super) fn resolve_field_access(
        &mut self,
        field_access: &ast::FieldAccessExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let expr = self.resolve_expr(&field_access.expr, ctx, None);

        // Look up field type from struct type
        let (field_index, field_type) =
            self.lookup_field_type(expr.type_id, &field_access.field, field_access.span);

        // Check field visibility: non-pub fields cannot be accessed from other modules
        self.check_field_visibility(
            expr.type_id,
            &field_access.field,
            field_access.span,
        );

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

    /// Look up field type from a struct or tuple type
    pub(super) fn lookup_field_type(
        &mut self,
        struct_type: TypeId,
        field_name: &str,
        span: Span,
    ) -> (u32, TypeId) {
        // Clone the type to avoid borrow issues
        let resolved = self.type_table.borrow().get(struct_type).clone();
        match resolved {
            // Struct field access
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                // Use the flat struct_fields map first. When there's a name collision
                // (the entry is from a different module), re-resolve fields from the
                // source module to get the correct struct definition.
                if let Some(struct_info) = self.struct_fields.get(&name) {
                    if struct_info.module_source == module_source {
                        for (index, (fname, ftype, _)) in struct_info.fields.iter().enumerate() {
                            if fname == field_name {
                                return (index as u32, *ftype);
                            }
                        }
                    } else {
                        // Name collision: re-resolve field type from the loaded module
                        if let Some(ftype) =
                            self.resolve_field_in_source_module(&name, field_name, &module_source)
                        {
                            return ftype;
                        }
                    }
                }
            }
            // Tuple field access (numeric field names: 0, 1, 2, ...)
            ResolvedType::Tuple(elements) => {
                if let Ok(index) = field_name.parse::<usize>() {
                    if index < elements.len() {
                        return (index as u32, elements[index]);
                    }
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!(
                            "tuple index {} out of bounds, tuple has {} elements",
                            index,
                            elements.len()
                        ),
                        span,
                    });
                    return (0, TypeTable::UNKNOWN);
                }
            }
            // Reference types - look through to inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                return self.lookup_field_type(inner, field_name, span);
            }
            // Newtype - look through to base type for field access
            ResolvedType::Newtype { base_type, .. } => {
                return self.lookup_field_type(base_type, field_name, span);
            }
            // Generic instance - look up field from generic struct definition
            // and substitute type parameters with concrete type args
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Clone fields to avoid borrow issues
                let fields_clone = self.struct_fields.get(&name).cloned();
                if let Some(struct_info) = fields_clone {
                    for (index, (fname, ftype, _)) in struct_info.fields.iter().enumerate() {
                        if fname == field_name {
                            // Substitute type parameters with concrete types
                            let concrete_type = self.substitute_type_params(*ftype, &type_args);
                            return (index as u32, concrete_type);
                        }
                    }
                }
            }
            _ => {}
        }
        (0, TypeTable::UNKNOWN)
    }

    /// Resolve a struct field type from the source module where the struct is defined.
    /// Used when the flat `struct_fields` map has a name collision (two modules define
    /// a struct with the same name). This re-resolves the field type in the source
    /// module's type context to get the correct result.
    pub(super) fn resolve_field_in_source_module(
        &mut self,
        struct_name: &str,
        field_name: &str,
        module_source: &ModuleSource,
    ) -> Option<(u32, TypeId)> {
        // Pre-populate module type maps cache before borrowing loaded_modules
        self.ensure_module_maps_cached(module_source);

        let loaded_module = self.loaded_modules.get(module_source)?;
        // Find the struct declaration in the loaded module
        let struct_decl = loaded_module.items.iter().find_map(|item| {
            if let Item::Struct(s) = item
                && s.name == struct_name
            {
                Some(s)
            } else {
                None
            }
        })?;

        // Swap to source module's type context using cached maps (O(1))
        let mut cached = self
            .module_type_maps_cache
            .shift_remove(module_source)
            .expect("cache populated by ensure_module_maps_cached");
        std::mem::swap(&mut self.struct_fields, &mut cached.struct_fields);
        std::mem::swap(&mut self.variant_cases, &mut cached.variant_cases);
        std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
        std::mem::swap(&mut self.flags_cases, &mut cached.flags_cases);
        std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
        std::mem::swap(&mut self.resource_types, &mut cached.resource_types);

        // Find and resolve the field type
        let result = struct_decl
            .fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == field_name)
            .map(|(index, f)| (index as u32, self.resolve_type(&f.ty)));

        // Restore type maps and re-cache
        std::mem::swap(&mut self.struct_fields, &mut cached.struct_fields);
        std::mem::swap(&mut self.variant_cases, &mut cached.variant_cases);
        std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
        std::mem::swap(&mut self.flags_cases, &mut cached.flags_cases);
        std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
        std::mem::swap(&mut self.resource_types, &mut cached.resource_types);
        self.module_type_maps_cache
            .insert(module_source.clone(), cached);

        result
    }

    /// Check if a struct field is accessible from the current module.
    /// Non-pub fields are private to the module that defines them.
    fn check_field_visibility(
        &mut self,
        struct_type: TypeId,
        field_name: &str,
        span: Span,
    ) {
        let resolved = self.type_table.borrow().get(struct_type).clone();
        let (struct_name, module_source) = match resolved {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name, module_source),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.check_field_visibility(inner, field_name, span);
                return;
            }
            ResolvedType::Newtype { base_type, .. } => {
                self.check_field_visibility(base_type, field_name, span);
                return;
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name, module_source),
            _ => return,
        };

        // Same module — always allowed
        if module_source == self.current_module_source {
            return;
        }

        // Look up field visibility
        if let Some(struct_info) = self.struct_fields.get(&struct_name) {
            for (fname, _, is_pub) in &struct_info.fields {
                if fname == field_name && !is_pub {
                    let _ = self.logger.error(TypeError::TypeMismatch {
                        expected: format!(
                            "accessible field (field `{field_name}` of struct `{struct_name}` is private)"
                        ),
                        found: format!(
                            "private field access from module `{}`",
                            self.current_module_source
                        ),
                        span,
                    });
                    return;
                }
            }
        }
    }

    /// Substitute type parameters in a type with concrete type arguments
    pub(super) fn substitute_type_params(
        &mut self,
        type_id: TypeId,
        type_args: &[TypeId],
    ) -> TypeId {
        let resolved_type = self.type_table.borrow().get(type_id).clone();
        match resolved_type {
            ResolvedType::TypeParam { index, .. } => {
                // Direct substitution: T -> type_args[index]
                type_args.get(index as usize).copied().unwrap_or(type_id)
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type_params(elem, type_args);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::BuiltinArray(new_elem))
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type_params(inner, type_args);
                self.type_table.borrow_mut().make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type_params(inner, type_args);
                self.type_table.borrow_mut().make_mut_ref(new_inner)
            }
            ResolvedType::Tuple(elems) => {
                let new_elems: Vec<TypeId> = elems
                    .iter()
                    .map(|&e| self.substitute_type_params(e, type_args))
                    .collect();
                self.type_table.borrow_mut().make_tuple(new_elems)
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args: inner_args,
            } => {
                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = inner_args
                    .iter()
                    .map(|&arg| self.substitute_type_params(arg, type_args))
                    .collect();
                self.type_table
                    .borrow_mut()
                    .make_generic_instance(name, module_source, new_args)
            }
            // Other types don't contain type parameters
            _ => type_id,
        }
    }

    /// Resolve an index expression
    pub(super) fn resolve_index(
        &mut self,
        index: &ast::IndexExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let expr = self.resolve_expr(&index.expr, ctx, None);

        // Get base type (unwrap reference if needed)
        let base_type_id = match self.type_table.borrow().get(expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => expr.type_id,
        };
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        // Handle tuple indexing: t[0] is equivalent to t.0
        if let ResolvedType::Tuple(elements) = base_type {
            let elements = elements.clone();
            // Tuple indexing requires a constant integer index
            if let ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Number(num_lit),
                ..
            }) = &index.index
                && !util::is_float_only_literal(&num_lit.repr)
                && let Ok(idx) = num_lit.repr.parse::<usize>()
            {
                if idx < elements.len() {
                    let field_type = elements[idx];
                    return TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(expr),
                            field_index: idx as u32,
                            field_name: idx.to_string(),
                        },
                        field_type,
                        index.span,
                    );
                } else {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!(
                            "tuple index {} out of bounds, tuple has {} elements",
                            idx,
                            elements.len()
                        ),
                        span: index.span,
                    });
                    // Return a placeholder expression with unknown type
                    return TirExpr::new(TirExprKind::Unit, TypeTable::UNKNOWN, index.span);
                }
            }
            // Non-constant index on tuple
            let _ = self.logger.error(TypeError::InvalidLiteral {
                message: "tuple index must be a constant integer".to_string(),
                span: index.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::UNKNOWN, index.span);
        }

        // For Array and custom types, look for Index or IndexValue trait implementation
        // (Array implements IndexValue<i32> with type Output = T)
        let struct_name = match &base_type {
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::GenericInstance { name, .. } => name.clone(),
            _ => String::new(),
        };

        if !struct_name.is_empty() {
            let index_expr = self.resolve_expr(&index.index, ctx, None);
            let index_type = index_expr.type_id;

            // Reject &T/&mut T used as index expression (would ICE in codegen)
            let derefed_index_type = match self.type_table.borrow().get(index_type) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Some(*inner),
                _ => None,
            };
            if let Some(expected) = derefed_index_type {
                self.check_ref_type_mismatch(index_type, expected, index.index.span());
            }

            // First, try Index trait (returns reference)
            if let Some(trait_info) =
                self.find_index_trait_impl(&struct_name, base_type_id, index_type)
            {
                // Generate: *expr.index(index_expr)
                // First, create the method call to .index(index_expr)
                let receiver = self.adjust_receiver_for_self_kind(
                    expr.clone(),
                    trait_info.self_kind,
                    index.span,
                );

                let mangled_method_name =
                    MethodName::format_local(&struct_name, Some(&trait_info.trait_name), "index");

                // The method returns &Output, so the type is Ref(output_type)
                let ref_output_type = self
                    .type_table
                    .borrow_mut()
                    .make_ref(trait_info.output_type);

                let method_call = TirExpr::new(
                    TirExprKind::MethodCall {
                        receiver: Box::new(receiver),
                        func: FunctionRef::External {
                            module_source: self.current_module_source.clone(),
                            name: mangled_method_name,
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                struct_name.clone(),
                                Some(trait_info.trait_name.clone()),
                                "index".to_string(),
                            )),
                        },
                        type_args: vec![],
                        args: vec![index_expr],
                    },
                    ref_output_type,
                    index.span,
                );

                // Dereference the result: *expr.index(...)
                return TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: Box::new(method_call),
                    },
                    trait_info.output_type,
                    index.span,
                );
            }

            // Fallback: try IndexValue trait (returns value by copy)
            if let Some(trait_info) =
                self.find_index_value_trait_impl(&struct_name, base_type_id, index_type)
            {
                // Generate: expr.index_value(index_expr)
                let receiver = self.adjust_receiver_for_self_kind(
                    expr.clone(),
                    trait_info.self_kind,
                    index.span,
                );

                let mangled_method_name = MethodName::format_local(
                    &struct_name,
                    Some(&trait_info.trait_name),
                    "index_value",
                );

                // IndexValue returns Output directly (not a reference)
                return TirExpr::new(
                    TirExprKind::MethodCall {
                        receiver: Box::new(receiver),
                        func: FunctionRef::External {
                            module_source: self.current_module_source.clone(),
                            name: mangled_method_name,
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                struct_name.clone(),
                                Some(trait_info.trait_name.clone()),
                                "index_value".to_string(),
                            )),
                        },
                        type_args: vec![],
                        args: vec![index_expr],
                    },
                    trait_info.output_type,
                    index.span,
                );
            }
        }

        // Fallback: report error for unsupported indexing
        let _ = self.logger.error(TypeError::TypeMismatch {
            expected: "array or type implementing Index or IndexValue trait".to_string(),
            found: self.type_table.borrow().type_name(expr.type_id),
            span: index.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::UNKNOWN, index.span)
    }

    /// Extract the result type from a block (the type of its last expression, or Unit)
    pub(super) fn block_result_type(block: &TirBlock) -> TypeId {
        block
            .stmts
            .last()
            .and_then(|s| match &s.kind {
                TirStmtKind::Expr(e) => Some(e.type_id),
                // If statement can produce a value if both branches exist and have the same type
                TirStmtKind::If {
                    then_block,
                    else_block: Some(else_block),
                    ..
                } => {
                    let then_type = Self::block_result_type(then_block);
                    let else_type = Self::block_result_type(else_block);
                    // `never` is the bottom type: compatible with any other type.
                    if then_type == else_type {
                        Some(then_type)
                    } else if then_type == TypeTable::NEVER {
                        Some(else_type)
                    } else if else_type == TypeTable::NEVER {
                        Some(then_type)
                    } else {
                        None
                    }
                }
                // IfPattern can also produce a value if both branches exist and have the same type
                TirStmtKind::IfPattern {
                    then_block,
                    else_block: Some(else_block),
                    ..
                } => {
                    let then_type = Self::block_result_type(then_block);
                    let else_type = Self::block_result_type(else_block);
                    // `never` is the bottom type: compatible with any other type.
                    if then_type == else_type {
                        Some(then_type)
                    } else if then_type == TypeTable::NEVER {
                        Some(else_type)
                    } else if else_type == TypeTable::NEVER {
                        Some(then_type)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or(TypeTable::UNIT)
    }

    /// Resolve an if expression
    pub(super) fn resolve_if_expr(
        &mut self,
        if_expr: &IfExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        if if_expr.init.is_some() {
            ctx.enter_scope();
        }

        if let Some(init) = &if_expr.init {
            let _init_stmt = self.resolve_let(init, ctx);
        }

        let condition = match &if_expr.condition {
            ast::Condition::Expr(expr) => self.resolve_expr(expr, ctx, Some(TypeTable::BOOL)),
            ast::Condition::Pattern { span, .. } => {
                let _ = self.logger.error(TypeError::NotYetImplemented {
                    feature: "pattern matching in if expressions (use if statement instead)"
                        .to_string(),
                    span: *span,
                });
                TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, *span)
            }
        };

        let then_block = self.resolve_block(&if_expr.then_block, ctx, expected_type);
        let else_block = if_expr
            .else_block
            .as_ref()
            .map(|b| self.resolve_block(b, ctx, expected_type));

        let type_id = if let Some(ty) = expected_type {
            ty
        } else {
            let then_type = Self::block_result_type(&then_block);
            let else_type = else_block
                .as_ref()
                .map_or(TypeTable::UNIT, Self::block_result_type);

            // `never` is the bottom type: a branch returning `never` is compatible
            // with any type, so the result type comes from the non-never branch.
            if then_type == else_type {
                then_type
            } else if then_type == TypeTable::NEVER {
                else_type
            } else if else_type == TypeTable::NEVER {
                then_type
            } else if else_block.is_none() {
                if then_type != TypeTable::UNIT {
                    let type_name = self.type_table.borrow().type_name(then_type);
                    let _ = self.logger.error(TypeError::TypeMismatch {
                        expected: "()".to_string(),
                        found: type_name,
                        span: if_expr.then_block.span,
                    });
                }
                TypeTable::UNIT
            } else {
                let then_name = self.type_table.borrow().type_name(then_type);
                let else_name = self.type_table.borrow().type_name(else_type);
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: then_name,
                    found: else_name,
                    span: if_expr.else_block.as_ref().unwrap().span,
                });
                then_type
            }
        };

        let result = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(condition),
                then_branch: then_block,
                else_branch: else_block,
            },
            type_id,
            if_expr.span,
        );

        if if_expr.init.is_some() {
            ctx.exit_scope();
        }

        result
    }

    /// Resolve a match expression
    pub(super) fn resolve_match_expr(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        let scrutinee = self.resolve_expr(&match_expr.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| self.resolve_match_arm(arm, scrutinee_type, ctx, expected_type))
            .collect();

        let type_id = expected_type.unwrap_or_else(|| {
            // Skip `never`-typed arms: `never` is the bottom type and is compatible
            // with any type, so the match result type is determined by the non-never arms.
            arms.iter()
                .map(|a| a.body.type_id)
                .find(|&t| t != TypeTable::NEVER)
                .unwrap_or_else(|| {
                    // All arms return `never` — the match itself is `never`.
                    arms.first()
                        .map(|a| a.body.type_id)
                        .unwrap_or(TypeTable::UNIT)
                })
        });

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            type_id,
            match_expr.span,
        )
    }

    pub(super) fn resolve_match_arm(
        &mut self,
        arm: &MatchArm,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirMatchArm {
        ctx.enter_scope();

        let pattern = self.resolve_if_pattern(&arm.pattern, scrutinee_type, ctx, arm.span);
        let guard = arm
            .guard
            .as_ref()
            .map(|g| self.resolve_expr(g, ctx, Some(TypeTable::BOOL)));
        let body = self.resolve_expr(&arm.body, ctx, expected_type);

        ctx.exit_scope();

        TirMatchArm {
            pattern,
            guard,
            body,
            span: arm.span,
        }
    }

    /// Collect variable names that are directly assigned inside an expression,
    /// but NOT inside nested closures.
    pub(super) fn collect_mutated_vars(expr: &ast::Expr, result: &mut IndexSet<String>) {
        match expr {
            ast::Expr::Assign(a) => {
                if let ast::Expr::Ident(id) = &a.target {
                    result.insert(id.name.clone());
                }
                Self::collect_mutated_vars(&a.value, result);
            }
            ast::Expr::CompoundAssign(ca) => {
                if let ast::Expr::Ident(id) = &ca.target {
                    result.insert(id.name.clone());
                }
                Self::collect_mutated_vars(&ca.value, result);
            }
            ast::Expr::Closure(_) => {
                // Do NOT recurse into nested closures
            }
            ast::Expr::Block(block) => {
                for stmt in &block.stmts {
                    Self::collect_mutated_vars_stmt(stmt, result);
                }
            }
            ast::Expr::If(if_expr) => {
                if let Some(init) = &if_expr.init {
                    Self::collect_mutated_vars(&init.value, result);
                }
                if let ast::Condition::Expr(cond) = &if_expr.condition {
                    Self::collect_mutated_vars(cond, result);
                }
                for stmt in &if_expr.then_block.stmts {
                    Self::collect_mutated_vars_stmt(stmt, result);
                }
                if let Some(else_block) = &if_expr.else_block {
                    for stmt in &else_block.stmts {
                        Self::collect_mutated_vars_stmt(stmt, result);
                    }
                }
            }
            ast::Expr::Binary(b) => {
                Self::collect_mutated_vars(&b.left, result);
                Self::collect_mutated_vars(&b.right, result);
            }
            ast::Expr::Unary(u) => {
                Self::collect_mutated_vars(&u.expr, result);
            }
            ast::Expr::Call(c) => {
                for arg in &c.args {
                    Self::collect_mutated_vars(arg, result);
                }
            }
            ast::Expr::MethodCall(mc) => {
                Self::collect_mutated_vars(&mc.receiver, result);
                for arg in &mc.args {
                    Self::collect_mutated_vars(arg, result);
                }
            }
            _ => {}
        }
    }

    pub(super) fn collect_mutated_vars_stmt(stmt: &ast::Stmt, result: &mut IndexSet<String>) {
        match stmt {
            ast::Stmt::Expr(es) => Self::collect_mutated_vars(&es.expr, result),
            ast::Stmt::Let(ls) => {
                Self::collect_mutated_vars(&ls.value, result);
            }
            ast::Stmt::If(is) => {
                if let Some(init) = &is.init {
                    Self::collect_mutated_vars(&init.value, result);
                }
                if let ast::Condition::Expr(cond) = &is.condition {
                    Self::collect_mutated_vars(cond, result);
                }
                for stmt in &is.then_block.stmts {
                    Self::collect_mutated_vars_stmt(stmt, result);
                }
                if let Some(else_block) = &is.else_block {
                    for stmt in &else_block.stmts {
                        Self::collect_mutated_vars_stmt(stmt, result);
                    }
                }
            }
            ast::Stmt::Loop(ls) => {
                for stmt in &ls.body.stmts {
                    Self::collect_mutated_vars_stmt(stmt, result);
                }
            }
            _ => {}
        }
    }

    /// Resolve `&mut || { body }` - desugars mutable captures.
    ///
    /// For each outer mutable variable `v` assigned inside the body:
    /// - Creates `let __ref_v = &mut v;` in the outer scope
    /// - Inside the closure, `v` is accessed as `*__ref_v` (deref of the captured reference)
    ///
    /// This allows the closure to mutate the outer variable via the shared reference.

    pub(super) fn resolve_cast(
        &mut self,
        cast: &ast::CastExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let target_type = self.resolve_type(&cast.target_type);

        // Special case: tuple literal cast to a type implementing SequenceLiteralBuilder
        // [1, 2, 3] as Array<i32>, [1, 2, 3] as SeqVec<i32>
        if let Some(coerced) = self.try_coerce_tuple_to_sequence(&cast.expr, ctx, target_type) {
            return coerced;
        }

        // Special case: struct literal cast to a type implementing KeyValueLiteral
        // { a: 1, b: 2 } as TreeMap<String, i32>
        if let Some(coerced) = self.try_coerce_struct_to_map(&cast.expr, ctx, target_type) {
            return coerced;
        }

        // Cast to i128/u128: expr as u128 → u128::from_u64(expr as u64)
        // For large literals: 170... as i128 → i128::from_string("170...")
        let struct_name = match self.type_table.borrow().get(target_type).clone() {
            ResolvedType::Struct { name, .. } => Some(name),
            _ => None,
        };

        if let Some(ref name) = struct_name
            && (name == "u128" || name == "i128")
        {
            // Handle number literal cast specially to support values > u64
            if let ast::Expr::Literal(lit) = &cast.expr
                && let Literal::Number(num_lit) = &lit.value
                && !util::is_float_only_literal(&num_lit.repr)
            {
                let parse_result = if name == "u128" {
                    util::parse_u128_literal(&num_lit.repr).map(|v| v as i128)
                } else {
                    util::parse_i128_literal(&num_lit.repr)
                };

                match parse_result {
                    Ok(value) => {
                        // If value fits in u64/i64, use the cheaper from_u64/from_i64
                        let use_small = if name == "u128" {
                            u64::try_from(value).is_ok()
                        } else {
                            i64::try_from(value).is_ok()
                        };

                        if use_small {
                            let (inner_type, method_name, store_value) = if name == "u128" {
                                (TypeTable::U64, "from_u64", value as u64)
                            } else {
                                (TypeTable::I64, "from_i64", value as u64)
                            };

                            let inner_literal = TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: store_value,
                                    repr: num_lit.repr.clone(),
                                },
                                inner_type,
                                lit.span,
                            );

                            let method_info =
                                LocalMethodName::new(name.clone(), None, method_name.to_string());
                            let mangled_func_name = method_info.to_mangled_name();

                            return TirExpr::new(
                                TirExprKind::StaticCall {
                                    func: FunctionRef::External {
                                        module_source: ModuleSource::int128(),
                                        name: mangled_func_name,
                                        monomorph_info: None,
                                        method_info: Some(method_info),
                                    },
                                    args: vec![inner_literal],
                                },
                                target_type,
                                cast.span,
                            );
                        }

                        // Value doesn't fit in u64/i64, use from_pair
                        let (low, high) = util::unpack_i128(value);
                        return self.build_from_pair_call(name, low, high, target_type, cast.span);
                    }
                    Err(_) => {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: format!("invalid {} literal: {}", name, num_lit.repr),
                            span: lit.span,
                        });
                    }
                }
            }

            // Handle negated number literal cast: -170... as i128
            if let ast::Expr::Unary(unary) = &cast.expr
                && unary.op == ast::UnaryOp::Neg
                && let ast::Expr::Literal(lit) = &unary.expr
                && let Literal::Number(num_lit) = &lit.value
                && !util::is_float_only_literal(&num_lit.repr)
                && name == "i128"
            {
                // Parse the negated value directly using Rust's i128
                let negated_repr = format!("-{}", num_lit.repr);
                if let Ok(value) = util::parse_i128_literal(&negated_repr) {
                    let (low, high) = util::unpack_i128(value);
                    return self.build_from_pair_call(name, low, high, target_type, unary.span);
                }
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!("invalid i128 literal: -{}", num_lit.repr),
                    span: unary.span,
                });
            }

            // General expression cast (not a literal)
            let expr_resolved = self.resolve_expr(&cast.expr, ctx, None);
            let source_type = expr_resolved.type_id;

            // Check if source type is a numeric type we can convert from
            if self.type_table.borrow().is_integer(source_type)
                || self.type_table.borrow().is_float(source_type)
            {
                // Determine intermediate type and method based on target
                let (intermediate_type, method_name) = if name == "u128" {
                    (TypeTable::U64, "from_u64")
                } else {
                    (TypeTable::I64, "from_i64")
                };

                // Cast the expression to intermediate type first
                let casted_expr = TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(expr_resolved),
                        target_type: intermediate_type,
                    },
                    intermediate_type,
                    cast.span,
                );

                // Build static call: u128::from_u64(expr as u64)
                // Note: i128/u128 are defined in core:prelude/int128
                let method_info = LocalMethodName::new(name.clone(), None, method_name.to_string());
                let mangled_func_name = method_info.to_mangled_name();

                return TirExpr::new(
                    TirExprKind::StaticCall {
                        func: FunctionRef::External {
                            module_source: ModuleSource::int128(),
                            name: mangled_func_name,
                            monomorph_info: None,
                            method_info: Some(method_info),
                        },
                        args: vec![casted_expr],
                    },
                    target_type,
                    cast.span,
                );
            }
        }

        // Normal cast
        let expr = self.resolve_expr(&cast.expr, ctx, None);
        let source_type = expr.type_id;

        // Validate char casts: prohibit integer/float -> char (use char::from_u32 instead)
        // Exception: u8 -> char is always valid (0..255 are valid Unicode scalar values)
        let source_base = self.type_table.borrow().get_ultimate_base_type(source_type);
        let target_base = self.type_table.borrow().get_ultimate_base_type(target_type);
        if target_base == TypeTable::CHAR
            && source_base != TypeTable::CHAR
            && source_base != TypeTable::U8
        {
            let from_name = self.type_table.borrow().type_name(source_type);
            let _ = self.logger.error(TypeError::InvalidCast {
                from: from_name,
                to: "char".to_string(),
                hint: "use char::from_u32() or char::from_i32() for checked conversion".to_string(),
                span: cast.span,
            });
        }
        // char -> non-integer is invalid (char -> integer extracts code point)
        if source_base == TypeTable::CHAR
            && target_base != TypeTable::CHAR
            && !self.type_table.borrow().is_integer(target_base)
        {
            let to_name = self.type_table.borrow().type_name(target_type);
            let _ = self.logger.error(TypeError::InvalidCast {
                from: "char".to_string(),
                to: to_name,
                hint: "char can only be cast to integer types".to_string(),
                span: cast.span,
            });
        }

        TirExpr::new(
            TirExprKind::Cast {
                expr: Box::new(expr),
                target_type,
            },
            target_type,
            cast.span,
        )
    }

    /// Find the return type from return statements in a block.
    /// Returns the type of the first return statement found, or None if no returns.
    pub(super) fn find_return_type_in_block(block: &TirBlock) -> Option<TypeId> {
        for stmt in &block.stmts {
            if let Some(type_id) = Self::find_return_type_in_stmt(stmt) {
                return Some(type_id);
            }
        }
        None
    }

    pub(super) fn find_return_type_in_stmt(stmt: &TirStmt) -> Option<TypeId> {
        match &stmt.kind {
            TirStmtKind::Return { value: Some(expr) } => Some(expr.type_id),
            TirStmtKind::Return { value: None } => Some(TypeTable::UNIT),
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                if let Some(t) = Self::find_return_type_in_block(then_block) {
                    return Some(t);
                }
                if let Some(else_blk) = else_block
                    && let Some(t) = Self::find_return_type_in_block(else_blk)
                {
                    return Some(t);
                }
                None
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                Self::find_return_type_in_block(body)
            }
            _ => None,
        }
    }

    /// Resolve a struct literal
    pub(super) fn resolve_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Handle implicit struct literals (name is None)
        let Some(name) = &struct_lit.name else {
            // Implicit struct literal without type context - error
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: "named struct literal (e.g., Point { x: 1, y: 2 })".into(),
                found: "implicit struct literal without type context".into(),
                span: struct_lit.span,
            });
            // Return a dummy expression with unknown type
            return TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".into(),
                },
                TypeTable::UNKNOWN,
                struct_lit.span,
            );
        };

        // Look up the struct in the symbol table to resolve imports/aliases
        // We need both the struct name (for struct_fields lookup) and module_source (for disambiguation)
        // Local struct definitions (current module) shadow imported/prelude structs.
        let (struct_name, symbol_module_source) = if self
            .struct_fields
            .get(name)
            .is_some_and(|info| info.module_source == self.current_module_source)
        {
            // Current module defines this struct locally - skip symbol table
            (name.clone(), None)
        } else if let Some(symbol) = self.symbols.lookup(name) {
            match &symbol.kind {
                crate::symbol::SymbolKind::Struct(_) => {
                    (symbol.name.clone(), Some(symbol.module_source.clone()))
                }
                _ => (name.clone(), None),
            }
        } else {
            // Fall back to local struct name
            (name.clone(), None)
        };

        // Get expected field types for coercion (for generic structs)
        let struct_field_types: Vec<(String, TypeId)> = self
            .struct_fields
            .get(&struct_name)
            .map(|info| {
                info.fields
                    .iter()
                    .map(|(name, type_id, _)| (name.clone(), *type_id))
                    .collect()
            })
            .unwrap_or_default();

        // Resolve field expressions, converting tuple literals to arrays when needed.
        // For generic structs, tuple-to-sequence coercion may be deferred to a second
        // pass after type arguments are inferred from field values.
        let mut deferred_coercions: Vec<(usize, usize)> = Vec::new(); // (field_index, ast_field_index)
        let fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                // Find expected field type for literal coercion
                // We use expected type for numeric literals (including negated ones)
                // and null literals to avoid interfering with tuple-to-array coercion
                // for generic struct fields
                let is_numeric_literal = matches!(
                    &field.value,
                    ast::Expr::Literal(lit) if matches!(
                        &lit.value,
                        ast::Literal::Number(_)
                    )
                ) || matches!(
                    &field.value,
                    ast::Expr::Unary(unary) if unary.op == ast::UnaryOp::Neg && matches!(
                        &unary.expr,
                        ast::Expr::Literal(lit) if matches!(
                            &lit.value,
                            ast::Literal::Number(_)
                        )
                    )
                );

                let is_null_literal = matches!(
                    &field.value,
                    ast::Expr::Literal(lit) if matches!(&lit.value, ast::Literal::Null)
                );

                let is_anonymous_struct_literal = matches!(
                    &field.value,
                    ast::Expr::StructLiteral(s) if s.name.is_none()
                );

                let is_tuple_literal = matches!(&field.value, ast::Expr::TupleLiteral(_));

                let expected_field_type = if is_numeric_literal
                    || is_null_literal
                    || is_anonymous_struct_literal
                    || is_tuple_literal
                {
                    struct_field_types
                        .iter()
                        .find(|(name, _)| name == &field.name)
                        .map(|(_, type_id)| *type_id)
                } else {
                    None
                };

                // For tuple literals in generic struct fields where the field type
                // contains type params (e.g., Array<T>), skip providing the expected
                // type so the tuple isn't coerced yet. Instead, resolve as a plain
                // tuple and defer coercion to after type inference.
                let needs_deferred_coercion = is_tuple_literal
                    && expected_field_type
                        .is_some_and(|t| self.type_table.borrow().contains_type_param(t));
                let effective_expected = if needs_deferred_coercion {
                    None
                } else {
                    expected_field_type
                };

                // Use expected type for literal coercion (e.g., 0 -> u64 when field is u64)
                let value = self.resolve_expr(&field.value, ctx, effective_expected);

                // Track tuple literals whose coercion was deferred because the field
                // type had unresolved type parameters. After type inference, we'll
                // re-coerce with the concrete type.
                if needs_deferred_coercion && matches!(value.kind, TirExprKind::TupleLiteral { .. })
                {
                    deferred_coercions.push((index, index));
                }

                // Check field value type against declared struct field type
                if let Some((_, expected_type_id)) =
                    struct_field_types.iter().find(|(n, _)| n == &field.name)
                {
                    self.check_ref_type_mismatch(
                        value.type_id,
                        *expected_type_id,
                        field.value.span(),
                    );
                }

                TirStructField {
                    name: field.name.clone(),
                    value,
                    field_index: index as u32,
                }
            })
            .collect();

        // Get module_source for this struct
        // Priority: symbol table module_source > struct_fields > current_module_source
        // The symbol table module_source is needed for imported structs (especially with aliases)
        // to handle name collisions between local and imported structs
        let struct_module_source = if let Some(ms) = symbol_module_source {
            // Imported struct - use module_source from symbol table
            ms
        } else if let Some(info) = self.struct_fields.get(&struct_name) {
            // Local struct found in struct_fields
            info.module_source.clone()
        } else {
            // Fall back to current module
            self.current_module_source.clone()
        };

        // Check field visibility: non-pub fields cannot be set from other modules
        if struct_module_source != self.current_module_source {
            if let Some(struct_info) = self.struct_fields.get(&struct_name) {
                for (fname, _, is_pub) in &struct_info.fields {
                    if !is_pub && fields.iter().any(|f| f.name == *fname) {
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: format!(
                                "accessible field (field `{fname}` of struct `{struct_name}` is private)"
                            ),
                            found: format!(
                                "private field in struct literal from module `{}`",
                                self.current_module_source
                            ),
                            span: struct_lit.span,
                        });
                    }
                }
            }
        }

        // Check if this is a generic struct and infer type arguments
        let (struct_type, mangled_struct_name, fields) = if self
            .generic_struct_names
            .contains(&struct_name)
        {
            // This is a generic struct - infer type arguments from field values
            let type_args = self.infer_type_args_from_fields(&struct_name, &fields);

            // Substitute type parameters in field value types.
            // This is necessary for empty array literals in self-referential fields
            // (e.g., `children: []` in `Node<K> { children: Array<&Node<K>> }`)
            // which get typed with TypeParams before inference.
            let mut fields: Vec<TirStructField> = if type_args.is_empty() {
                fields
            } else {
                fields
                    .into_iter()
                    .map(|mut field| {
                        field.value.type_id =
                            self.substitute_type_params(field.value.type_id, &type_args);
                        field
                    })
                    .collect()
            };

            // Second pass: apply deferred tuple-to-sequence coercion now that
            // concrete type arguments are known. For example, [10, 20, 30] in
            // `Container<i32> { items: [10, 20, 30] }` needs Array<i32> coercion,
            // but at first pass the field type was Array<T> (type param).
            if !deferred_coercions.is_empty() && !type_args.is_empty() {
                for &(field_idx, ast_idx) in &deferred_coercions {
                    let field_name = &fields[field_idx].name;
                    let concrete_field_type = struct_field_types
                        .iter()
                        .find(|(name, _)| name == field_name)
                        .map(|(_, type_id)| self.substitute_type_params(*type_id, &type_args));

                    if let Some(concrete_type) = concrete_field_type {
                        let ast_field = &struct_lit.fields[ast_idx];
                        if let Some(coerced) =
                            self.try_coerce_tuple_to_sequence(&ast_field.value, ctx, concrete_type)
                        {
                            fields[field_idx].value = coerced;
                        }
                    }
                }
            }

            let struct_type = self.type_table.borrow_mut().make_generic_instance(
                struct_name.clone(),
                struct_module_source.clone(),
                type_args.clone(),
            );
            // Build mangled name with type arguments
            let arg_names: Vec<String> = type_args
                .iter()
                .map(|&t| self.type_table.borrow().type_name(t))
                .collect();
            let mangled_name = mangle_generic_name(&struct_name, &arg_names);
            (struct_type, mangled_name, fields)
        } else {
            let struct_type = self
                .type_table
                .borrow_mut()
                .make_struct(struct_name.clone(), struct_module_source.clone());
            (struct_type, struct_name, fields)
        };

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_struct_name,
                fields,
            },
            struct_type,
            struct_lit.span,
        )
    }

    /// Infer type arguments for a generic struct from field values
    pub(super) fn infer_type_args_from_fields(
        &self,
        struct_name: &str,
        fields: &[TirStructField],
    ) -> Vec<TypeId> {
        // Get the generic struct's field type information
        let Some(struct_info) = self.struct_fields.get(struct_name) else {
            return vec![];
        };

        // Build a map from type param TypeId to concrete TypeId
        let mut type_param_map: IndexMap<TypeId, TypeId> = IndexMap::new();

        for (struct_field, (_, expected_type_id, _)) in fields.iter().zip(struct_info.fields.iter()) {
            let actual_type_id = struct_field.value.type_id;

            // Try to unify expected_type with actual_type to extract type params
            self.unify_types_for_inference(*expected_type_id, actual_type_id, &mut type_param_map);
        }

        // Collect type args in order (by TypeParam index)
        let mut type_args: Vec<(u32, TypeId)> = type_param_map
            .iter()
            .filter_map(|(&param_id, &concrete_id)| {
                if let ResolvedType::TypeParam { index, .. } =
                    self.type_table.borrow().get(param_id)
                {
                    Some((*index, concrete_id))
                } else {
                    None
                }
            })
            .collect();

        type_args.sort_by_key(|(index, _)| *index);

        // If the struct has known type_param_type_ids, produce a full-length vector.
        // This handles phantom type parameters (e.g., D in `struct DirMap<D, V>` where D
        // is not used in any field). Without this, the inferred vector would be sparse
        // (e.g., [V] instead of [D, V]), causing the monomorphizer to create `DirMap<i32>`
        // instead of `DirMap<Direction,i32>`.
        let n = struct_info.type_param_type_ids.len();
        if n > 0 {
            let inferred_map: IndexMap<u32, TypeId> = type_args.into_iter().collect();
            return (0..n as u32)
                .map(|i| {
                    inferred_map
                        .get(&i)
                        .copied()
                        .unwrap_or(struct_info.type_param_type_ids[i as usize])
                })
                .collect();
        }

        type_args.into_iter().map(|(_, type_id)| type_id).collect()
    }

    /// Unify expected type with actual type to extract type parameter mappings.
    /// This handles nested generic types like Array<T> where T is a type param.
    pub(super) fn unify_types_for_inference(
        &self,
        expected: TypeId,
        actual: TypeId,
        type_param_map: &mut IndexMap<TypeId, TypeId>,
    ) {
        let expected_type = self.type_table.borrow().get(expected).clone();
        let actual_type = self.type_table.borrow().get(actual).clone();

        match (&expected_type, &actual_type) {
            // Direct type parameter mapping
            (ResolvedType::TypeParam { .. }, _) => {
                // Only insert if we don't already have a concrete mapping for this type param.
                // This prevents later fields with self-referential types (like Array<&Node<K>>)
                // from overwriting earlier correct mappings (like K -> String) with
                // incorrect mappings (like K -> K).
                type_param_map.entry(expected).or_insert(actual);
            }
            // Generic instance: unify type arguments recursively
            (
                ResolvedType::GenericInstance {
                    name: expected_name,
                    type_args: expected_args,
                    ..
                },
                ResolvedType::GenericInstance {
                    name: actual_name,
                    type_args: actual_args,
                    ..
                },
            ) if expected_name == actual_name && expected_args.len() == actual_args.len() => {
                for (&exp_arg, &act_arg) in expected_args.iter().zip(actual_args.iter()) {
                    self.unify_types_for_inference(exp_arg, act_arg, type_param_map);
                }
            }
            // Array<K> (GenericInstance) with Tuple (homogeneous) - infer K from tuple element type
            (
                ResolvedType::GenericInstance {
                    name,
                    type_args: expected_args,
                    ..
                },
                ResolvedType::Tuple(actual_elems),
            ) if name == "Array" && expected_args.len() == 1 && !actual_elems.is_empty() => {
                // Check if all tuple elements have the same type
                let first_elem_type = actual_elems[0];
                let all_same = actual_elems.iter().all(|&e| e == first_elem_type);
                if all_same {
                    // Unify Array's element type param with the tuple's element type
                    self.unify_types_for_inference(
                        expected_args[0],
                        first_elem_type,
                        type_param_map,
                    );
                }
            }
            // Array types (Wado's Array<T>)
            (
                ResolvedType::BuiltinArray(expected_elem),
                ResolvedType::BuiltinArray(actual_elem),
            ) => {
                self.unify_types_for_inference(*expected_elem, *actual_elem, type_param_map);
            }
            // Ref types
            (ResolvedType::Ref(expected_inner), ResolvedType::Ref(actual_inner))
            | (ResolvedType::MutRef(expected_inner), ResolvedType::MutRef(actual_inner)) => {
                self.unify_types_for_inference(*expected_inner, *actual_inner, type_param_map);
            }
            // Tuple types
            (ResolvedType::Tuple(expected_elems), ResolvedType::Tuple(actual_elems))
                if expected_elems.len() == actual_elems.len() =>
            {
                for (&exp, &act) in expected_elems.iter().zip(actual_elems.iter()) {
                    self.unify_types_for_inference(exp, act, type_param_map);
                }
            }
            // Function types: unify param types and return type
            (
                ResolvedType::Function {
                    params: expected_params,
                    return_type: expected_ret,
                    ..
                },
                ResolvedType::Function {
                    params: actual_params,
                    return_type: actual_ret,
                    ..
                },
            ) if expected_params.len() == actual_params.len() => {
                for (&exp, &act) in expected_params.iter().zip(actual_params.iter()) {
                    self.unify_types_for_inference(exp, act, type_param_map);
                }
                self.unify_types_for_inference(*expected_ret, *actual_ret, type_param_map);
            }
            // Other cases: no type params to extract
            _ => {}
        }
    }

    /// Resolve a tuple literal expression: `[1, 2, 3]` or `[1, "hello", true]`
    pub(super) fn resolve_tuple_literal(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Resolve each element expression
        let elements: Vec<TirExpr> = tuple_lit
            .elements
            .iter()
            .map(|elem| self.resolve_expr(elem, ctx, None))
            .collect();

        // Collect element types for the tuple type
        let elem_types: Vec<TypeId> = elements.iter().map(|e| e.type_id).collect();
        let tuple_type = self.type_table.borrow_mut().make_tuple(elem_types);

        TirExpr::new(
            TirExprKind::TupleLiteral { elements },
            tuple_type,
            tuple_lit.span,
        )
    }
}
