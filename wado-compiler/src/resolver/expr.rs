//! Expression resolution (literals, identifiers, field access, index,
//! if-expressions, match, cast, struct/tuple literals, etc.).

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, AstId, AstVisitor, Condition, Expr, IfExpr, Item, Literal, MatchArm};
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName, mangle_generic_name};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirField, TirMatchArm,
    TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::infer::InferCtx;
use super::typecheck::{TypeCheckResult, check_assignable};
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
            Expr::Ident(ident) => self.resolve_ident(ident, ctx, expected_type),
            Expr::Binary(binary) => self.resolve_binary(binary, ctx, expected_type),
            Expr::Unary(unary) => self.resolve_unary(unary, ctx, expected_type),
            Expr::Assign(assign) => self.resolve_assign(assign, ctx),
            Expr::Call(call) => self.resolve_call(call, ctx, expected_type),
            Expr::MethodCall(method_call) => {
                self.resolve_method_call(method_call, ctx, expected_type)
            }
            Expr::StaticMethodCall(static_call) => {
                self.resolve_static_method_call(static_call, ctx)
            }
            Expr::FieldAccess(field_access) => self.resolve_field_access(field_access, ctx),
            Expr::Index(index) => self.resolve_index(index, ctx),
            Expr::Block(block) => {
                let tir_block = self.resolve_block(block, ctx, expected_type);
                // Reuse `block_result_type` so the block expression's
                // overall type matches the inference rule applied
                // everywhere else (function body trailing expressions,
                // `if` / `match` arm bodies, etc.). The previous local
                // copy of the rule only handled `TirStmtKind::Expr` /
                // `Return` / `Break` / `Continue` as trailing forms
                // and fell back to `Unit` for trailing `if` / `if let`
                // — which mis-typed `{ if cond { a } else { b } }` as
                // `Unit` and silently miscompiled when such a block
                // was used as a match-arm body. Routing through the
                // shared `block_result_type` makes a trailing
                // `TirStmtKind::If` / `IfLet` with both branches
                // present propagate its branch-agreed type through.
                let type_id = Self::block_result_type(&tir_block);
                TirExpr::new(TirExprKind::Block(tir_block), type_id, block.span)
            }
            Expr::If(if_expr) => self.resolve_if_expr(if_expr, ctx, expected_type),
            Expr::Match(match_expr) => self.resolve_match_expr(match_expr, ctx, expected_type),
            Expr::Closure(closure) => self.resolve_closure(closure, ctx, expected_type),
            Expr::TemplateString(template) => self.resolve_template_string(template, ctx),
            Expr::Cast(cast) => self.resolve_cast(cast, ctx),
            Expr::StructLiteral(struct_lit) => {
                self.resolve_struct_literal(struct_lit, ctx, expected_type)
            }
            Expr::CompoundAssign(compound) => self.resolve_compound_assign(compound, ctx),
            Expr::ComparisonChain(chain) => self.resolve_comparison_chain(chain, ctx),
            Expr::TupleLiteral(tuple_lit) => {
                self.resolve_tuple_literal(tuple_lit, ctx, expected_type)
            }
            Expr::LabeledBlock(lb) => {
                ctx.labeled_block_targets.push(LabeledBlockTarget {
                    label: lb.label.clone(),
                    break_types: Vec::new(),
                });
                ctx.active_labels.push(lb.label.clone());

                ctx.enter_scope();
                let tir_block = self.resolve_block(&lb.block, ctx, expected_type);
                ctx.exit_scope();

                ctx.active_labels.pop();
                let target = ctx.labeled_block_targets.pop().unwrap();

                let result_type = if !target.break_types.is_empty() {
                    // TODO: type unification for multiple breaks
                    target.break_types[0]
                } else if let Some(ty) = expected_type {
                    ty
                } else {
                    TypeTable::UNIT
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
            Expr::Spread(..) => {
                panic!("Spread expression should only appear inside TupleLiteral handling")
            }
            Expr::TryOp(qm) => self.resolve_question_mark(qm, ctx),
            Expr::Range(range) => self.resolve_range(range, ctx),
            Expr::WithHandler(w) => self.resolve_with_handler(w, ctx),
            Expr::Resume(r) => self.resolve_resume(r, ctx),
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
            Literal::Number(repr) => {
                // Default type: i32 if integer-compatible, f64 if float-only
                if util::is_float_only_literal(repr) {
                    // Must be float (has decimal point or negative exponent)
                    match util::parse_float_literal(repr) {
                        Ok(value) => (
                            TirExprKind::FloatLiteral {
                                value,
                                repr: repr.clone(),
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
                                    repr: repr.clone(),
                                },
                                TypeTable::F64,
                            )
                        }
                    }
                } else {
                    // Can be integer (default to i32)
                    match util::parse_u128_literal(repr) {
                        Ok(value) => (
                            TirExprKind::IntLiteral {
                                value: value as u64,
                                repr: repr.clone(),
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
                                    repr: repr.clone(),
                                },
                                TypeTable::I32,
                            )
                        }
                    }
                }
            }
            Literal::Bool(b) => (TirExprKind::BoolLiteral(*b), TypeTable::BOOL),
            Literal::Char(raw) => {
                let c = match util::unescape_char(raw) {
                    Ok(c) => c,
                    Err(message) => {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        '\0'
                    }
                };
                (TirExprKind::CharLiteral(c), TypeTable::CHAR)
            }
            Literal::String(raw) => {
                let string_type = self.get_string_struct_type();
                let value = match util::unescape_string(raw) {
                    Ok(s) => s,
                    Err(message) => {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        String::new()
                    }
                };
                (TirExprKind::StringLiteral(value), string_type)
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
            Literal::IncludeStr(raw_path) => {
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let string_type = self.get_string_struct_type();
                if let Some(bytes) = self.included_files.get(&key) {
                    if let Ok(s) = std::str::from_utf8(bytes) {
                        (TirExprKind::StringLiteral(s.to_owned()), string_type)
                    } else {
                        let _ = self.logger.error(TypeError::InvalidLiteral {
                            message: format!("file is not valid UTF-8: \"{raw_path}\""),
                            span: lit.span,
                        });
                        (TirExprKind::StringLiteral(String::new()), string_type)
                    }
                } else {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!("file not found: \"{raw_path}\""),
                        span: lit.span,
                    });
                    (TirExprKind::StringLiteral(String::new()), string_type)
                }
            }
            Literal::IncludeBytes(raw_path) => {
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let array_u8_type = self.type_table.borrow_mut().make_array(TypeTable::U8);
                if let Some(bytes) = self.included_files.get(&key) {
                    (TirExprKind::BytesLiteral(bytes.clone()), array_u8_type)
                } else {
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!("file not found: \"{raw_path}\""),
                        span: lit.span,
                    });
                    (TirExprKind::BytesLiteral(Vec::new()), array_u8_type)
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
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Check local variables, including captures from outer scope
        if let Some(var_ref) = ctx.lookup_or_capture(&ident.name) {
            match var_ref {
                VarRef::Local {
                    index,
                    type_id,
                    defining_ast_id,
                } => {
                    self.record_reference_opt(ident.id, defining_ast_id);
                    return TirExpr::new(
                        TirExprKind::Local {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                VarRef::Capture {
                    index,
                    type_id,
                    defining_ast_id,
                } => {
                    self.record_reference_opt(ident.id, defining_ast_id);
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
                    defining_ast_id,
                } => {
                    self.record_reference_opt(ident.id, defining_ast_id);
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

            if let Some(variant_info) = self.lookup_variant_case(prefix).cloned() {
                // Find the case by name
                if let Some((case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == suffix)
                    .map(|(i, c)| (i, c.clone()))
                {
                    self.record_qualified_case(
                        ident,
                        prefix,
                        &variant_info.module_source,
                        case_data.ast_id,
                    );
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

                    // Infer variant type for generic variants
                    let variant_type = if variant_info.type_params.is_empty() {
                        self.type_table.borrow_mut().make_variant(
                            variant_info.name.clone(),
                            variant_info.module_source.clone(),
                        )
                    } else {
                        self.infer_variant_type_args(
                            prefix,
                            &variant_info,
                            &case_data,
                            None,
                            expected_type,
                        )
                    };

                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload: None,
                        },
                        variant_type,
                        ident.span,
                    );
                }
            }

            // Check for enum case: Color::Red (enums have no payload)
            if let Some(enum_info) = self.lookup_enum_case(prefix).cloned()
                && let Some(case_data) = enum_info.find_case(suffix).cloned()
            {
                self.record_qualified_case(
                    ident,
                    prefix,
                    &enum_info.module_source,
                    case_data.ast_id,
                );
                // Use canonical name (not import alias) for consistent TypeId interning
                let enum_type = self
                    .type_table
                    .borrow_mut()
                    .make_enum(enum_info.name.clone(), enum_info.module_source);

                return TirExpr::new(
                    TirExprKind::EnumConstruct {
                        enum_type,
                        case_index: case_data.index,
                        case_name: case_data.name,
                    },
                    enum_type,
                    ident.span,
                );
            }

            // Check for flags member: PathFlags::SymlinkFollow
            // Flags members are bitmask integers (1 << index) represented as IntLiteral
            if let Some(flags_info) = self.lookup_flags_case(prefix).cloned()
                && let Some(member) = flags_info
                    .members
                    .iter()
                    .find(|m| m.name == suffix)
                    .cloned()
            {
                self.record_qualified_case(ident, prefix, &flags_info.module_source, member.ast_id);
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: u64::from(member.bitmask),
                        repr: member.bitmask.to_string(),
                    },
                    flags_info.type_id,
                    ident.span,
                );
            }

            // Check for namespace import: ns::Type::Case or ns::Enum::Case
            if let Some(ns_source) = self.namespace_imports.get(prefix).cloned()
                && let Some(inner_pos) = suffix.find("::")
            {
                let type_name = &suffix[..inner_pos];
                let case_name = &suffix[inner_pos + 2..];

                // Check variant cases — namespace imports always look up by
                // canonical name in the source module, so we can read directly
                // from the shared `all_*` table without any per-module cache.
                let ns_variant = self
                    .all_variant_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned();
                if let Some(variant_info) = ns_variant
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == case_name)
                        .map(|(i, c)| (i, c.clone()))
                {
                    self.record_namespaced_case(
                        ident,
                        &variant_info.module_source,
                        case_data.ast_id,
                    );
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
                    let variant_type = if variant_info.type_params.is_empty() {
                        self.type_table.borrow_mut().make_variant(
                            variant_info.name.clone(),
                            variant_info.module_source.clone(),
                        )
                    } else {
                        self.infer_variant_type_args(
                            type_name,
                            &variant_info,
                            &case_data,
                            None,
                            expected_type,
                        )
                    };
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload: None,
                        },
                        variant_type,
                        ident.span,
                    );
                }

                // Check enum cases
                let ns_enum = self
                    .all_enum_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned();
                if let Some(enum_info) = ns_enum
                    && let Some(case_data) = enum_info.find_case(case_name).cloned()
                {
                    self.record_namespaced_case(ident, &enum_info.module_source, case_data.ast_id);
                    let enum_type = self
                        .type_table
                        .borrow_mut()
                        .make_enum(enum_info.name.clone(), enum_info.module_source);
                    return TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type,
                            case_index: case_data.index,
                            case_name: case_data.name,
                        },
                        enum_type,
                        ident.span,
                    );
                }

                // Check flags members
                let ns_flags = self
                    .all_flags_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned();
                if let Some(flags_info) = ns_flags
                    && let Some(member) = flags_info
                        .members
                        .iter()
                        .find(|m| m.name == case_name)
                        .cloned()
                {
                    self.record_namespaced_case(ident, &flags_info.module_source, member.ast_id);
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
        }

        // Check for global variables in current module
        if let Some(&(ty, _mutable)) = self.current_module_globals.get(&ident.name) {
            self.record_item_reference_by_name(ident.id, &ident.name);
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
            self.record_item_reference_by_name(ident.id, &ident.name);
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
                    .map(|s| s.module_source().clone())
                    .unwrap_or_else(|| self.current_module_source.clone())
            };
            self.record_item_reference_by_name(ident.id, &ident.name);
            return TirExpr::new(
                TirExprKind::FuncRef {
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
                TirExprKind::FuncRef {
                    module_source: ModuleSource::internal(),
                    name: ident.name.clone(),
                },
                TypeTable::UNKNOWN,
                ident.span,
            );
        }

        // Fallback: when resolving a default expression, look up the
        // identifier in the callee's lexical scope (see
        // `default_scope_module`). This gives defaults access to the
        // definition module's private globals and functions.
        if let Some(fallback) = self.default_scope_module.clone()
            && fallback != self.current_module_source
            && let Some(result) =
                self.resolve_ident_in_fallback_module(&ident.name, ident.span, &fallback)
        {
            return result;
        }

        // Unknown variable - report error
        let _ = self.logger.error(TypeError::UnknownIdentifier {
            name: ident.name.clone(),
            span: ident.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span)
    }

    /// Look up an identifier in the callee module's global scope during
    /// default-expression resolution. Supports globals and function refs.
    fn resolve_ident_in_fallback_module(
        &mut self,
        name: &str,
        span: Span,
        fallback: &ModuleSource,
    ) -> Option<TirExpr> {
        let module = self.loaded_modules.get(fallback)?;
        for item in &module.items {
            match item {
                crate::ast::Item::Global(global_decl) if global_decl.name == name => {
                    let ty = self.resolve_type(&global_decl.ty);
                    return Some(TirExpr::new(
                        TirExprKind::GlobalVarGet {
                            module_source: fallback.clone(),
                            name: name.to_string(),
                        },
                        ty,
                        span,
                    ));
                }
                crate::ast::Item::Function(func) if func.name == name => {
                    return Some(TirExpr::new(
                        TirExprKind::FuncRef {
                            module_source: fallback.clone(),
                            name: name.to_string(),
                        },
                        TypeTable::UNKNOWN,
                        span,
                    ));
                }
                _ => {}
            }
        }
        None
    }

    /// Resolve a binary expression
    pub(super) fn resolve_field_access(
        &mut self,
        field_access: &ast::FieldAccessExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let expr = self.resolve_expr(&field_access.expr, ctx, None);

        // Record use→def reference for the field name, pointing at the field
        // definition's AstId in the struct declaration.
        self.record_field_reference(expr.type_id, &field_access.field, field_access.field_id);

        // Look up field type from struct type
        let (field_index, field_type) =
            self.lookup_field_type(expr.type_id, &field_access.field, field_access.span);

        // Check field visibility: non-pub fields cannot be accessed from other modules
        self.check_field_visibility(expr.type_id, &field_access.field, field_access.span);

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

    /// Record a use→def reference for a struct field access.
    /// `receiver_type` is the type of the struct being accessed;
    /// `field_name` is the accessed field; `use_id` is the `AstId` of the
    /// field-name token at the use site.
    pub(super) fn record_field_reference(
        &mut self,
        receiver_type: TypeId,
        field_name: &str,
        use_id: AstId,
    ) {
        let resolved = self.type_table.borrow().get(receiver_type).clone();
        let (struct_name, module_source) = match resolved {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            }
            | ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name, module_source),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                return self.record_field_reference(inner, field_name, use_id);
            }
            ResolvedType::Newtype { base_type, .. } => {
                return self.record_field_reference(base_type, field_name, use_id);
            }
            _ => return,
        };
        if let Some(info) = self.lookup_struct_fields_in(&struct_name, &module_source) {
            for ((fname, _, _), fid) in info.fields.iter().zip(info.field_ast_ids.iter()) {
                if fname == field_name {
                    self.record_reference_to_decl(use_id, &module_source, *fid);
                    return;
                }
            }
        }
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
                if let Some(struct_info) = self.lookup_struct_fields_in(&name, &module_source) {
                    for (index, (fname, ftype, _)) in struct_info.fields.iter().enumerate() {
                        if fname == field_name {
                            return (index as u32, *ftype);
                        }
                    }
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
            // and substitute type parameters with concrete type args.
            // Tuples use numeric field access (0, 1, 2, ...).
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // Tuple field access (numeric field names: 0, 1, 2, ...)
                if TypeTable::is_tuple_type(&name, &module_source)
                    && let Ok(index) = field_name.parse::<usize>()
                {
                    if index < type_args.len() {
                        return (index as u32, type_args[index]);
                    }
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!(
                            "tuple index {} out of bounds, tuple has {} elements",
                            index,
                            type_args.len()
                        ),
                        span,
                    });
                    return (0, TypeTable::UNKNOWN);
                }
                // Clone fields to avoid borrow issues
                let fields_clone = self.lookup_struct_fields_in(&name, &module_source).cloned();
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

    /// Check if a struct field is accessible from the current module.
    /// Non-pub fields are private to the module that defines them.
    fn check_field_visibility(&mut self, struct_type: TypeId, field_name: &str, span: Span) {
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
        if let Some(struct_info) = self.lookup_struct_fields_in(&struct_name, &module_source) {
            for (fname, _, is_pub) in &struct_info.fields {
                if fname == field_name && !is_pub {
                    let _ = self.logger.error(TypeError::PrivateFieldAccess {
                        struct_name: struct_name.clone(),
                        field_name: field_name.to_string(),
                        span,
                    });
                    return;
                }
            }
        }
    }

    /// Substitute type parameters in a type with concrete type arguments.
    ///
    /// Treats `type_args` as a dense substitution map keyed by `TypeParam`
    /// index (i.e. `TypeParam { index: i }` is replaced by `type_args[i]`),
    /// delegating the heavy lifting to
    /// [`TypeTable::substitute_type_params`].
    pub(super) fn substitute_type_params(
        &mut self,
        type_id: TypeId,
        type_args: &[TypeId],
    ) -> TypeId {
        if type_args.is_empty() {
            return type_id;
        }
        let substitution: IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &t)| (i as u32, t))
            .collect();
        self.type_table
            .borrow_mut()
            .substitute_type_params(type_id, &substitution)
    }

    /// Substitute type parameters using a TypeId-to-TypeId map.
    /// Unlike `substitute_type_params` (which substitutes by index), this only
    /// replaces `TypeIds` that are explicitly in the map, leaving all others unchanged.
    /// This is used in struct literal field type fixup to avoid incorrectly replacing
    /// impl-scope `TypeParams` that share the same index as the struct's own `TypeParams`.
    pub(super) fn substitute_type_params_by_map(
        &mut self,
        type_id: TypeId,
        map: &IndexMap<TypeId, TypeId>,
    ) -> TypeId {
        if map.is_empty() {
            return type_id;
        }
        if let Some(&concrete) = map.get(&type_id) {
            return concrete;
        }
        let resolved_type = self.type_table.borrow().get(type_id).clone();
        match resolved_type {
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type_params_by_map(elem, map);
                if new_elem == elem {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::BuiltinArray(new_elem))
                }
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type_params_by_map(inner, map);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table.borrow_mut().make_ref(new_inner)
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type_params_by_map(inner, map);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table.borrow_mut().make_mut_ref(new_inner)
                }
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args: inner_args,
            } => {
                let new_args: Vec<TypeId> = inner_args
                    .iter()
                    .map(|&a| self.substitute_type_params_by_map(a, map))
                    .collect();
                if new_args == inner_args {
                    type_id
                } else {
                    self.type_table.borrow_mut().make_generic_instance(
                        name,
                        module_source,
                        new_args,
                    )
                }
            }
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
        if let ResolvedType::GenericInstance {
            ref name,
            ref module_source,
            type_args: ref elements,
        } = base_type
            && TypeTable::is_tuple_type(name, module_source)
        {
            // Tuple indexing requires a constant integer index
            if let ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Number(repr),
                ..
            }) = &index.index
                && !util::is_float_only_literal(repr)
                && let Ok(idx) = repr.parse::<usize>()
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
            ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => name.clone(),
            _ => String::new(),
        };

        // For newtypes, also resolve the base type name for trait impl lookup
        let (lookup_name, lookup_type_id) = self.newtype_base_lookup(&struct_name, base_type_id);

        if !struct_name.is_empty() {
            let index_expr = self.resolve_expr(&index.index, ctx, None);
            let index_type = index_expr.type_id;

            // Reject &T/&mut T used as index expression (would ICE in codegen)
            let derefed_index_type = match self.type_table.borrow().get(index_type) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Some(*inner),
                _ => None,
            };
            if let Some(expected) = derefed_index_type {
                self.typecheck(index_type, expected, index.index.span());
            }

            // First, try Index trait (returns reference)
            // Try the direct name first, then fall back to base type name for newtypes
            let index_trait_info = self
                .find_index_trait_impl(&struct_name, base_type_id, index_type)
                .or_else(|| self.find_index_trait_impl(&lookup_name, lookup_type_id, index_type));
            if let Some(trait_info) = index_trait_info {
                // Generate: *expr.index(index_expr)
                // First, create the method call to .index(index_expr)
                let receiver = self.adjust_receiver_for_self_kind(
                    expr,
                    trait_info.self_kind,
                    false,
                    index.span,
                );

                let mangled_method_name =
                    MethodName::format_local(&lookup_name, Some(&trait_info.trait_name), "index");

                // The method returns &Output, so the type is Ref(output_type)
                let ref_output_type = self
                    .type_table
                    .borrow_mut()
                    .make_ref(trait_info.output_type);

                let method_call = Self::build_tir_method_call(
                    receiver,
                    FunctionRef {
                        module_source: trait_info.impl_module_source.clone(),
                        name: mangled_method_name,
                        monomorph_info: None,
                        method_info: Some(LocalMethodName::new(
                            lookup_name,
                            Some(trait_info.trait_name.clone()),
                            "index".to_string(),
                        )),
                    },
                    vec![],
                    vec![CallArg::new(index_expr, false)],
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
            let index_value_info = self
                .find_index_value_trait_impl(&struct_name, base_type_id, index_type)
                .or_else(|| {
                    self.find_index_value_trait_impl(&lookup_name, lookup_type_id, index_type)
                });
            if let Some(trait_info) = index_value_info {
                // Generate: expr.index_value(index_expr)
                let receiver = self.adjust_receiver_for_self_kind(
                    expr,
                    trait_info.self_kind,
                    false,
                    index.span,
                );

                let mangled_method_name = MethodName::format_local(
                    &lookup_name,
                    Some(&trait_info.trait_name),
                    "index_value",
                );

                // IndexValue returns Output directly (not a reference)
                return Self::build_tir_method_call(
                    receiver,
                    FunctionRef {
                        module_source: trait_info.impl_module_source.clone(),
                        name: mangled_method_name,
                        monomorph_info: None,
                        method_info: Some(LocalMethodName::new(
                            lookup_name,
                            Some(trait_info.trait_name.clone()),
                            "index_value".to_string(),
                        )),
                    },
                    vec![],
                    vec![CallArg::new(index_expr, false)],
                    trait_info.output_type,
                    index.span,
                );
            }
        }

        // Fallback: report error for unsupported indexing
        let type_name = self.type_table.borrow().type_name(expr.type_id);
        let _ = self.logger.error(TypeError::MissingTraitImpl {
            type_name,
            trait_name: "Index or IndexValue".to_string(),
            span: index.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::UNKNOWN, index.span)
    }

    /// Extract the result type from a block (the type of its last
    /// expression, or Unit). Thin wrapper around the shared
    /// [`crate::tir::block_result_type`] free function.
    pub(super) fn block_result_type(block: &TirBlock) -> TypeId {
        crate::tir::block_result_type(block)
    }

    /// Resolve an if expression
    pub(super) fn resolve_if_expr(
        &mut self,
        if_expr: &IfExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        match &if_expr.condition {
            Condition::LetChain { elements, .. } => {
                // Resolve else_block in outer scope (chain bindings not visible there)
                let else_block = if_expr
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx, expected_type));

                // Enter scope for chain elements and then_block
                ctx.enter_scope();
                let stmts = self.resolve_let_chain_stmts(
                    elements,
                    &if_expr.then_block,
                    else_block.as_ref(),
                    ctx,
                    expected_type,
                    if_expr.span,
                );
                ctx.exit_scope();

                let chain_block = TirBlock::new(stmts, if_expr.span);
                let type_id = if let Some(ty) = expected_type {
                    ty
                } else {
                    // Infer result type from the nested block structure
                    let chain_type = Self::block_result_type(&chain_block);
                    let else_type = else_block
                        .as_ref()
                        .map_or(TypeTable::UNIT, Self::block_result_type);
                    if chain_type == else_type
                        || chain_type == TypeTable::NEVER
                        || else_type == TypeTable::NEVER
                    {
                        if chain_type == TypeTable::NEVER {
                            else_type
                        } else {
                            chain_type
                        }
                    } else if if_expr.else_block.is_none() {
                        TypeTable::UNIT
                    } else {
                        let chain_name = self.type_table.borrow().type_name(chain_type);
                        let else_name = self.type_table.borrow().type_name(else_type);
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: chain_name,
                            found: else_name,
                            span: if_expr.else_block.as_ref().unwrap().span,
                        });
                        chain_type
                    }
                };

                // Same arm-agreement rule as the `Condition::Expr` arm
                // below: when `expected_type=Some(X)` pinned `type_id`
                // to `X` unconditionally, the chain and else blocks
                // could still produce different types — a divergent
                // `if let` branch in expression position would
                // silently miscompile the same way the
                // `Condition::Expr` arm did before this PR. Mirror
                // the check here so both `if` shapes share the
                // soundness guarantee. Skipped for `type_id == Unit`
                // (statement-position use, branches drop their
                // values per `translate_stmts`).
                if expected_type.is_some() && type_id != TypeTable::UNIT {
                    // The chain's then-branch is resolved inside
                    // `resolve_let_chain_stmts` with the same
                    // `expected_type`, so a then-block that can't
                    // satisfy `type_id` already surfaces a
                    // diagnostic from `resolve_expr` /
                    // `try_coerce` during that walk — no separate
                    // chain-side check is needed (and using
                    // `block_result_type(&chain_block)` to check
                    // here would emit a spurious "expected X,
                    // found ()" because `block_result_type`
                    // recurses through the chain's nested
                    // `TirStmtKind::IfLet` and `agree_branch_types`
                    // collapses divergent then/else to `Unit`).
                    //
                    // The else-block sits outside the chain and is
                    // resolved independently, so check it
                    // directly. Missing-else falls back to the
                    // implicit `else { () }` rule below.
                    if let Some(eb) = &else_block {
                        let else_type = Self::block_result_type(eb);
                        self.check_branch_type(
                            else_type,
                            type_id,
                            if_expr.else_block.as_ref().unwrap().span,
                        );
                    } else {
                        // Missing `else` with a non-Unit expected
                        // type: the implicit `else { () }` cannot
                        // produce the expected type. See the
                        // `Condition::Expr` arm for the rationale
                        // (without this guard the WIR builder
                        // would produce `(if (result T) ...)`
                        // without an else and `wasmparser` would
                        // reject the module at `-O0`).
                        self.check_branch_type(TypeTable::UNIT, type_id, if_expr.span);
                    }
                }

                TirExpr::new(TirExprKind::Block(chain_block), type_id, if_expr.span)
            }
            Condition::Expr(expr) => {
                let condition = self.resolve_expr(expr, ctx, Some(TypeTable::BOOL));
                let mut then_block = self.resolve_block(&if_expr.then_block, ctx, expected_type);
                let mut else_block = if_expr
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
                    //
                    // We also let `Option<UNKNOWN>` (typically a bare `null` literal whose
                    // inner type could not be inferred) defer to the sibling branch's
                    // resolved type. The unresolved branch's tail is patched below.
                    let tt = self.type_table.borrow();
                    let then_unknown = tt.contains_unknown(then_type);
                    let else_unknown = tt.contains_unknown(else_type);
                    drop(tt);
                    if then_type == else_type {
                        then_type
                    } else if then_type == TypeTable::NEVER {
                        else_type
                    } else if else_type == TypeTable::NEVER {
                        then_type
                    } else if then_unknown && !else_unknown {
                        else_type
                    } else if else_unknown && !then_unknown {
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

                // Patch unresolved `null` tails in either branch using the determined
                // result type — see `patch_unresolved_null` for the rationale.
                {
                    let tt = self.type_table.borrow();
                    if !tt.contains_unknown(type_id) {
                        patch_unresolved_null_in_block(&mut then_block, type_id, &tt);
                        if let Some(eb) = else_block.as_mut() {
                            patch_unresolved_null_in_block(eb, type_id, &tt);
                        }
                    }
                }

                // Same rule as `resolve_match_expr`: an if-expression
                // whose result is consumed must have branches that
                // agree on a common type. When `expected_type=None`
                // the existing inference logic above already
                // diagnosed the mismatch (line ~1318), but when
                // `expected_type=Some(X)` the inference was bypassed
                // (`type_id = X` unconditionally) and a divergent
                // branch would silently produce a wasm `(if (result
                // X) ...)` whose other branch pushes the wrong
                // type. Skip when `type_id == Unit`: that's
                // statement-position use, where each branch's value
                // gets dropped at the WIR stmt level.
                if expected_type.is_some() && type_id != TypeTable::UNIT {
                    let then_type = Self::block_result_type(&then_block);
                    self.check_branch_type(then_type, type_id, if_expr.then_block.span);
                    if let Some(eb) = &else_block {
                        let else_type = Self::block_result_type(eb);
                        self.check_branch_type(
                            else_type,
                            type_id,
                            if_expr.else_block.as_ref().unwrap().span,
                        );
                    } else {
                        // See the `Condition::LetChain` arm for the
                        // rationale. Without an explicit `else`, the
                        // implicit branch is `()`, which cannot
                        // satisfy a non-Unit expected type. Emit the
                        // diagnostic; the surrounding context (e.g.
                        // `let x: T = ...`) typically emits a
                        // redundant secondary mismatch on the same
                        // span, which the user will see as a single
                        // grouped error in editor diagnostics. We
                        // don't downgrade `type_id` here — the
                        // resolver-recorded diagnostic will abort
                        // compilation before WIR build runs, so the
                        // result-typed `if` with no else never
                        // reaches `wasmparser`.
                        self.check_branch_type(TypeTable::UNIT, type_id, if_expr.span);
                    }
                }

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
        }
    }

    /// Emit a `TypeMismatch` error when a branch's block-result type
    /// is incompatible with the surrounding context's expected type.
    /// `never` and `unknown` (and unresolved type-params) defer via
    /// `check_assignable`'s `Deferred` / `Compatible` rules. Used by
    /// `resolve_if_expr` (`Condition::Expr` arm) to validate then /
    /// else branches when an outer type annotation pinned the
    /// expected result type; the same rule lives inline in
    /// `resolve_match_expr` for arm bodies.
    fn check_branch_type(&mut self, actual: TypeId, expected: TypeId, span: Span) {
        let result = {
            let tt = self.type_table.borrow();
            check_assignable(actual, expected, &tt)
        };
        if matches!(result, TypeCheckResult::Incompatible) {
            let expected_name = self.type_table.borrow().type_name(expected);
            let found_name = self.type_table.borrow().type_name(actual);
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: expected_name,
                found: found_name,
                span,
            });
        }
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

        let mut arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| self.resolve_match_arm(arm, scrutinee_type, ctx, expected_type))
            .collect();

        self.check_match_exhaustiveness(&arms, scrutinee_type, match_expr.span);

        let type_id = expected_type.unwrap_or_else(|| {
            // Skip `never`-typed arms: `never` is the bottom type and is compatible
            // with any type, so the match result type is determined by the non-never arms.
            //
            // Also skip arms whose type still contains UNKNOWN — typically a bare
            // `null` literal whose `Option<...>` inner could not be inferred from
            // the arm body alone. A sibling arm with a fully-resolved type
            // (e.g. `Option::Some(s)` where `s: String`) wins; we then patch the
            // unresolved `null` arm bodies below.
            let tt = self.type_table.borrow();
            arms.iter()
                .map(|a| a.body.type_id)
                .find(|&t| t != TypeTable::NEVER && !tt.contains_unknown(t))
                .or_else(|| {
                    arms.iter()
                        .map(|a| a.body.type_id)
                        .find(|&t| t != TypeTable::NEVER)
                })
                .unwrap_or_else(|| {
                    // All arms return `never` — the match itself is `never`.
                    arms.first()
                        .map(|a| a.body.type_id)
                        .unwrap_or(TypeTable::UNIT)
                })
        });

        // Patch `null`-bodied arms whose `Option<???>` inner could not be inferred.
        // If the match's overall type is fully resolved we know the inner type;
        // rewrite the `Null`'s `type_id` so WIR translation can see it.
        {
            let tt = self.type_table.borrow();
            if !tt.contains_unknown(type_id) {
                for arm in &mut arms {
                    patch_unresolved_null(&mut arm.body, type_id, &tt);
                }
            }
        }

        // Reject arms whose body type disagrees with the match's overall
        // result type. Skipped when `type_id == Unit`: that means the
        // match sits in statement position (see `resolver::resolve_stmt`
        // for `Stmt::Match`, which pins `expected_type = Some(Unit)`),
        // and the WIR builder's `translate_match` already drops each
        // arm body's value via `WirInstr::Drop`. In every other context
        // (`let x = match {...}`, `f(match {...})`, a match as the
        // trailing expression of a block whose result is consumed)
        // divergent arms would silently miscompile — the match's wasm
        // result type would be picked from one arm, but the other
        // arm's branch pushes a different type onto the stack, which
        // either trips wasmparser or produces type-confused output.
        //
        // `Unit` here is the match-level type, not the arm-level type:
        // a unit-typed match can still have `never`-typed arms (e.g.
        // a `panic`), and those remain compatible. `check_assignable`
        // already encodes `NEVER` / `UNKNOWN` / type-param deferrals,
        // so we route through it instead of repeating the rules here.
        if type_id != TypeTable::UNIT {
            for arm in &arms {
                let arm_type = arm.body.type_id;
                let result = {
                    let tt = self.type_table.borrow();
                    check_assignable(arm_type, type_id, &tt)
                };
                if matches!(result, TypeCheckResult::Incompatible) {
                    let expected_name = self.type_table.borrow().type_name(type_id);
                    let found_name = self.type_table.borrow().type_name(arm_type);
                    let _ = self.logger.error(TypeError::TypeMismatch {
                        expected: expected_name,
                        found: found_name,
                        span: arm.body.span,
                    });
                }
            }
        }

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

    fn check_match_exhaustiveness(&self, arms: &[TirMatchArm], scrutinee_type: TypeId, span: Span) {
        // Always check for overlapping range patterns first
        self.check_range_overlaps(arms, span);

        // If any arm has a wildcard or binding pattern (without a guard), the match is exhaustive
        if arms
            .iter()
            .any(|arm| arm.guard.is_none() && Self::is_catch_all_pattern(&arm.pattern))
        {
            return;
        }

        let tt = self.type_table.borrow();
        let resolved = tt.get(scrutinee_type).clone();
        drop(tt);

        match &resolved {
            ResolvedType::Enum { name, .. } => {
                if let Some(enum_info) = self.lookup_enum_case(name) {
                    let all_cases: IndexSet<&str> =
                        enum_info.cases.iter().map(|c| c.name.as_str()).collect();
                    let covered: IndexSet<&str> = {
                        let mut names = Vec::new();
                        for arm in arms {
                            Self::collect_enum_case_names(&arm.pattern, &mut names);
                        }
                        names.into_iter().collect()
                    };
                    let missing: Vec<&&str> = all_cases.difference(&covered).collect();
                    if !missing.is_empty() {
                        let missing_names: Vec<String> =
                            missing.iter().map(|s| (*s).to_string()).collect();
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!(
                                "non-exhaustive match: missing {}",
                                Self::format_missing_cases(&missing_names),
                            ),
                            span,
                        });
                    }
                }
            }
            ResolvedType::Variant { name, .. } => {
                self.check_variant_exhaustiveness(arms, name, span);
            }
            ResolvedType::GenericInstance { name, .. } => {
                if self.contains_variant(name) {
                    self.check_variant_exhaustiveness(arms, name, span);
                }
            }
            ResolvedType::Primitive(crate::tir::PrimitiveType::Bool) => {
                let has_true = arms
                    .iter()
                    .any(|arm| Self::pattern_contains_bool(&arm.pattern, true));
                let has_false = arms
                    .iter()
                    .any(|arm| Self::pattern_contains_bool(&arm.pattern, false));
                if !has_true || !has_false {
                    let mut missing = Vec::new();
                    if !has_true {
                        missing.push("true".to_string());
                    }
                    if !has_false {
                        missing.push("false".to_string());
                    }
                    let _ = self.logger.error(TypeError::InvalidPattern {
                        message: format!(
                            "non-exhaustive match: missing {}",
                            Self::format_missing_cases(&missing),
                        ),
                        span,
                    });
                }
            }
            ResolvedType::Primitive(prim) => {
                if let Some((type_min, type_max)) = Self::primitive_range(*prim) {
                    self.check_integer_range_exhaustiveness(arms, type_min, type_max, span);
                }
            }
            _ => {
                // For other types (strings, structs, etc.) we don't check exhaustiveness.
            }
        }
    }

    fn check_variant_exhaustiveness(&self, arms: &[TirMatchArm], variant_name: &str, span: Span) {
        if let Some(variant_info) = self.lookup_variant_case(variant_name) {
            let all_cases: IndexSet<&str> =
                variant_info.cases.iter().map(|c| c.name.as_str()).collect();
            let covered: IndexSet<&str> = {
                let mut names = Vec::new();
                for arm in arms {
                    Self::collect_variant_case_names(&arm.pattern, &mut names);
                }
                names.into_iter().collect()
            };
            let missing: Vec<&&str> = all_cases.difference(&covered).collect();
            if !missing.is_empty() {
                let missing_names: Vec<String> = missing.iter().map(|s| (*s).to_string()).collect();
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!(
                        "non-exhaustive match: missing {}",
                        Self::format_missing_cases(&missing_names),
                    ),
                    span,
                });
            }
        }
    }

    fn is_catch_all_pattern(pattern: &TirPattern) -> bool {
        match pattern {
            TirPattern::Wildcard | TirPattern::Binding { .. } => true,
            TirPattern::Or(alternatives) => alternatives.iter().any(Self::is_catch_all_pattern),
            _ => false,
        }
    }

    fn collect_enum_case_names<'a>(pattern: &'a TirPattern, out: &mut Vec<&'a str>) {
        match pattern {
            TirPattern::Enum { case_name, .. } => out.push(case_name),
            TirPattern::Or(alternatives) => {
                for alt in alternatives {
                    Self::collect_enum_case_names(alt, out);
                }
            }
            _ => {}
        }
    }

    fn collect_variant_case_names<'a>(pattern: &'a TirPattern, out: &mut Vec<&'a str>) {
        match pattern {
            TirPattern::Variant { variant_name, .. } => out.push(variant_name),
            TirPattern::Or(alternatives) => {
                for alt in alternatives {
                    Self::collect_variant_case_names(alt, out);
                }
            }
            _ => {}
        }
    }

    fn pattern_contains_bool(pattern: &TirPattern, value: bool) -> bool {
        match pattern {
            TirPattern::Literal(crate::tir::TirLiteralPattern::Bool(b)) => *b == value,
            TirPattern::Or(alternatives) => alternatives
                .iter()
                .any(|p| Self::pattern_contains_bool(p, value)),
            _ => false,
        }
    }

    fn format_missing_cases(cases: &[String]) -> String {
        match cases.len() {
            1 => format!("case `{}`", cases[0]),
            2 => format!("cases `{}` and `{}`", cases[0], cases[1]),
            _ => {
                let last = &cases[cases.len() - 1];
                let rest: Vec<String> = cases[..cases.len() - 1]
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect();
                format!("cases {}, and `{last}`", rest.join(", "))
            }
        }
    }

    fn primitive_range(prim: crate::tir::PrimitiveType) -> Option<(i128, i128)> {
        use crate::tir::PrimitiveType;
        match prim {
            PrimitiveType::I8 => Some((i128::from(i8::MIN), i128::from(i8::MAX))),
            PrimitiveType::I16 => Some((i128::from(i16::MIN), i128::from(i16::MAX))),
            PrimitiveType::I32 => Some((i128::from(i32::MIN), i128::from(i32::MAX))),
            PrimitiveType::I64 => Some((i128::from(i64::MIN), i128::from(i64::MAX))),
            PrimitiveType::U8 => Some((0, i128::from(u8::MAX))),
            PrimitiveType::U16 => Some((0, i128::from(u16::MAX))),
            PrimitiveType::U32 => Some((0, i128::from(u32::MAX))),
            PrimitiveType::U64 => Some((0, i128::from(u64::MAX))),
            PrimitiveType::Char => Some((0, 0x0010_FFFF)),
            _ => None,
        }
    }

    fn collect_ranges_from_pattern(pattern: &TirPattern) -> Vec<(i128, i128)> {
        match pattern {
            TirPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                let hi = if *inclusive { *end } else { *end - 1 };
                vec![(*start, hi)]
            }
            TirPattern::Literal(lit) => {
                let val = match lit {
                    crate::tir::TirLiteralPattern::I128(v) => *v,
                    crate::tir::TirLiteralPattern::U128(v) => *v as i128,
                    crate::tir::TirLiteralPattern::Char(c) => *c as i128,
                    crate::tir::TirLiteralPattern::Bool(b) => i128::from(*b),
                    _ => return vec![],
                };
                vec![(val, val)]
            }
            TirPattern::Or(alts) => {
                let mut result = Vec::new();
                for alt in alts {
                    result.extend(Self::collect_ranges_from_pattern(alt));
                }
                result
            }
            _ => vec![],
        }
    }

    fn check_integer_range_exhaustiveness(
        &self,
        arms: &[TirMatchArm],
        type_min: i128,
        type_max: i128,
        span: Span,
    ) {
        // Collect all ranges from all arms (only arms without guards count)
        let mut all_ranges: Vec<(i128, i128)> = Vec::new();
        let mut has_catch_all = false;

        for arm in arms {
            if arm.guard.is_none() && Self::is_catch_all_pattern(&arm.pattern) {
                has_catch_all = true;
            }
            if arm.guard.is_some() {
                continue;
            }
            all_ranges.extend(Self::collect_ranges_from_pattern(&arm.pattern));
        }

        if has_catch_all {
            return;
        }

        // Check exhaustiveness: sort ranges and verify they cover [type_min, type_max]
        if all_ranges.is_empty() {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "non-exhaustive match: integer type requires a wildcard `_` or full range coverage".to_string(),
                span,
            });
            return;
        }

        all_ranges.sort_unstable();
        // Merge overlapping/adjacent ranges
        let mut merged: Vec<(i128, i128)> = Vec::new();
        for (lo, hi) in all_ranges {
            if let Some(last) = merged.last_mut() {
                if lo <= last.1 + 1 {
                    last.1 = last.1.max(hi);
                } else {
                    merged.push((lo, hi));
                }
            } else {
                merged.push((lo, hi));
            }
        }

        // Check if merged ranges cover [type_min, type_max]
        let covers = merged.len() == 1 && merged[0].0 <= type_min && merged[0].1 >= type_max;
        if !covers {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "non-exhaustive match: not all values in the integer range are covered"
                    .to_string(),
                span,
            });
        }
    }

    fn check_range_overlaps(&self, arms: &[TirMatchArm], span: Span) {
        // Collect ranges per arm (only guardless arms)
        let mut arm_ranges: Vec<Vec<(i128, i128)>> = Vec::new();
        for arm in arms {
            if arm.guard.is_some() {
                continue;
            }
            let ranges = Self::collect_ranges_from_pattern(&arm.pattern);
            if !ranges.is_empty() {
                arm_ranges.push(ranges);
            }
        }

        // Check for overlaps between different arms
        for i in 0..arm_ranges.len() {
            for j in (i + 1)..arm_ranges.len() {
                for &(a_lo, a_hi) in &arm_ranges[i] {
                    for &(b_lo, b_hi) in &arm_ranges[j] {
                        if a_lo <= b_hi && b_lo <= a_hi {
                            let _ = self.logger.error(TypeError::InvalidPattern {
                                message: "overlapping range patterns in match arms".to_string(),
                                span,
                            });
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Collect outer-binding names that are mutated inside an expression.
    /// Mutation = direct or compound assignment whose target's root
    /// identifier is the binding (e.g. `count`, `point.x`, `arr[i]`,
    /// `pair.p.children[i].name` all resolve to their root ident).
    ///
    /// Nested closures are skipped: they have their own capture context
    /// and run their own collector.
    pub(super) fn collect_mutated_vars(expr: &ast::Expr, result: &mut IndexSet<String>) {
        let mut collector = MutatedVarsCollector { result };
        collector.visit_expr(expr);
    }

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
                && let Literal::Number(repr) = &lit.value
                && !util::is_float_only_literal(repr)
            {
                let parse_result = if name == "u128" {
                    util::parse_u128_literal(repr).map(|v| v as i128)
                } else {
                    util::parse_i128_literal(repr)
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
                                (
                                    TypeTable::U64,
                                    "from_u64",
                                    u64::try_from(value).expect("value fits in u64"),
                                )
                            } else {
                                (
                                    TypeTable::I64,
                                    "from_i64",
                                    i64::try_from(value)
                                        .expect("value fits in i64")
                                        .cast_unsigned(),
                                )
                            };

                            let inner_literal = TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: store_value,
                                    repr: repr.clone(),
                                },
                                inner_type,
                                lit.span,
                            );

                            let method_info =
                                LocalMethodName::new(name.clone(), None, method_name.to_string());
                            let mangled_func_name = method_info.to_mangled_name();

                            return TirExpr::new(
                                TirExprKind::Call {
                                    func: FunctionRef {
                                        module_source: ModuleSource::int128(),
                                        name: mangled_func_name,
                                        monomorph_info: None,
                                        method_info: Some(method_info),
                                    },
                                    type_args: vec![],
                                    args: vec![CallArg::new(inner_literal, false)],
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
                            message: format!("invalid {name} literal: {repr}"),
                            span: lit.span,
                        });
                    }
                }
            }

            // Handle negated number literal cast: -170... as i128
            if let ast::Expr::Unary(unary) = &cast.expr
                && unary.op == ast::UnaryOp::Neg
                && let ast::Expr::Literal(lit) = &unary.expr
                && let Literal::Number(repr) = &lit.value
                && !util::is_float_only_literal(repr)
                && name == "i128"
            {
                // Parse the negated value directly using Rust's i128
                let negated_repr = format!("-{repr}");
                if let Ok(value) = util::parse_i128_literal(&negated_repr) {
                    let (low, high) = util::unpack_i128(value);
                    return self.build_from_pair_call(name, low, high, target_type, unary.span);
                }
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!("invalid i128 literal: -{repr}"),
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
                    TirExprKind::Call {
                        func: FunctionRef {
                            module_source: ModuleSource::int128(),
                            name: mangled_func_name,
                            monomorph_info: None,
                            method_info: Some(method_info),
                        },
                        type_args: vec![],
                        args: vec![CallArg::new(casted_expr, false)],
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
            TirStmtKind::IfLet {
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
            TirStmtKind::Expr(expr) => {
                if expr.type_id == TypeTable::NEVER {
                    return Some(TypeTable::NEVER);
                }
                Self::find_return_type_in_expr(expr)
            }
            _ => None,
        }
    }

    fn find_return_type_in_expr(expr: &TirExpr) -> Option<TypeId> {
        match &expr.kind {
            TirExprKind::Match { arms, .. } => {
                for arm in arms {
                    if let Some(t) = Self::find_return_type_in_expr(&arm.body) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::Block(block) => Self::find_return_type_in_block(block),
            // `resume value` is the handler-method analogue of `return value`:
            // it transfers control out of the current method and yields
            // `value` to the enclosing computation. The MVP lowers it to
            // `Return { value }` (no post-resume continuation), so a method
            // whose body terminates with `resume` satisfies the
            // missing-return check just like a method that ends with
            // `return`.
            TirExprKind::Resume { value } => Some(value.type_id),
            _ => None,
        }
    }

    /// Resolve a struct literal
    pub(super) fn resolve_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Handle implicit struct literals (name is None) — anonymous struct inference
        let Some(name) = &struct_lit.name else {
            return self.resolve_anonymous_struct_literal(struct_lit, ctx);
        };

        // Record use→def reference for the struct type name.
        if let Some(name_id) = struct_lit.name_id {
            self.record_item_reference_by_name(name_id, name);
        }

        // Look up the struct in the symbol table to resolve imports/aliases
        // We need both the struct name (for struct_fields lookup) and module_source (for disambiguation)
        // Local struct definitions (current module) shadow imported/prelude structs.
        // Resolve struct name and module_source.
        // Local definitions shadow imported/prelude structs.
        let (struct_name, struct_module_source) = if self
            .lookup_struct_fields_in(name, &self.current_module_source)
            .is_some()
        {
            (name.clone(), self.current_module_source.clone())
        } else if let Some(symbol) = self.symbols.lookup(name) {
            if let crate::symbol::SymbolKind::Struct(_) = &symbol.kind {
                (symbol.name.clone(), symbol.module_source().clone())
            } else {
                let _ = self.logger.error(TypeError::UnknownType {
                    name: name.clone(),
                    span: struct_lit.span,
                });
                (name.clone(), self.current_module_source.clone())
            }
        } else {
            // The struct name is neither locally defined nor imported.
            // Emit a clear diagnostic instead of silently falling back to
            // `current_module_source` — that fallback creates a TypeId
            // whose key does not match the registered struct in WIR build,
            // which used to surface as a downstream
            // `StructLiteral expected Ref WirType` panic. The fallback
            // module_source is still returned so subsequent passes have a
            // best-effort type; the error has already been logged.
            let _ = self.logger.error(TypeError::UnknownType {
                name: name.clone(),
                span: struct_lit.span,
            });
            (name.clone(), self.current_module_source.clone())
        };

        // Use canonical name from struct_fields info (not import alias) for consistent TypeId
        let struct_name = self
            .lookup_struct_fields_in(&struct_name, &struct_module_source)
            .map(|info| info.name.clone())
            .unwrap_or(struct_name);

        // Get expected field types using (name, module_source) lookup.
        let struct_field_types: Vec<(String, TypeId)> = self
            .lookup_struct_fields_in(&struct_name, &struct_module_source)
            .map(|info| {
                info.fields
                    .iter()
                    .map(|(name, type_id, _)| (name.clone(), *type_id))
                    .collect()
            })
            .unwrap_or_default();

        // Record use→def references for each field name, pointing at the
        // field definition's AstId in the struct declaration.
        let field_refs: Vec<(AstId, AstId)> = self
            .lookup_struct_fields_in(&struct_name, &struct_module_source)
            .map(|info| {
                struct_lit
                    .fields
                    .iter()
                    .filter_map(|f| {
                        info.fields
                            .iter()
                            .zip(info.field_ast_ids.iter())
                            .find(|((fname, _, _), _)| fname == &f.name)
                            .map(|(_, def_id)| (f.name_id, *def_id))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (use_id, def_id) in field_refs {
            self.record_reference_to_decl(use_id, &struct_module_source, def_id);
        }

        // Resolve field expressions, converting tuple literals to arrays when needed.
        // For generic structs, tuple-to-sequence coercion may be deferred to a second
        // pass after type arguments are inferred from field values.
        let mut deferred_coercions: Vec<(usize, usize)> = Vec::new(); // (field_index, ast_field_index)
        let fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .enumerate()
            .map(|(provided_idx, field)| {
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

                // Generic call expressions (e.g. `Array::filled(n, 0)` inside
                // a struct literal field) need the expected field type so the
                // call's type-parameter inference can back-infer from it. Without
                // this, the call falls back to literal defaults.
                let is_call = matches!(&field.value, ast::Expr::Call(_));

                let expected_field_type = if is_numeric_literal
                    || is_null_literal
                    || is_anonymous_struct_literal
                    || is_tuple_literal
                    || is_call
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
                    deferred_coercions.push((provided_idx, provided_idx));
                }

                // Check field name exists in struct definition
                if !struct_field_types.iter().any(|(n, _)| n == &field.name)
                    && !struct_field_types.is_empty()
                {
                    let _ = self.logger.error(TypeError::ExtraField {
                        struct_name: struct_name.clone(),
                        field_name: field.name.clone(),
                        span: field.span,
                    });
                }

                // Check field value type against declared struct field type
                if let Some((_, expected_type_id)) =
                    struct_field_types.iter().find(|(n, _)| n == &field.name)
                {
                    self.typecheck(value.type_id, *expected_type_id, field.value.span());
                }

                let decl_idx = struct_field_types
                    .iter()
                    .position(|(n, _)| n == &field.name)
                    .unwrap_or(provided_idx);

                TirStructField {
                    name: field.name.clone(),
                    value,
                    field_index: decl_idx as u32,
                }
            })
            .collect();

        // struct_module_source was already determined above (before field resolution).

        // Check for missing fields: fields without a declared default must be
        // provided; fields with `= expr` are synthesized from the default
        // expression (pure, resolved in the struct's module scope).
        let struct_field_defaults: Vec<Option<ast::Expr>> = self
            .lookup_struct_fields_in(&struct_name, &struct_module_source)
            .map(|info| info.field_defaults.clone())
            .unwrap_or_default();
        let mut fields = fields;
        if !struct_field_types.is_empty() {
            let provided_names: IndexSet<String> = fields.iter().map(|f| f.name.clone()).collect();
            for (idx, (expected_name, expected_type_id)) in struct_field_types.iter().enumerate() {
                if provided_names.contains(expected_name) {
                    continue;
                }
                let default_ast = struct_field_defaults.get(idx).and_then(Option::clone);
                if let Some(default_expr) = default_ast {
                    let resolved = self.resolve_expr(&default_expr, ctx, Some(*expected_type_id));
                    self.typecheck(resolved.type_id, *expected_type_id, struct_lit.span);
                    fields.push(TirStructField {
                        name: expected_name.clone(),
                        value: resolved,
                        field_index: idx as u32,
                    });
                } else {
                    let _ = self.logger.error(TypeError::MissingField {
                        struct_name: struct_name.clone(),
                        field_name: expected_name.clone(),
                        span: struct_lit.span,
                    });
                }
            }
            fields.sort_by_key(|f| f.field_index);
        }

        // Check field visibility: non-pub fields cannot be set from other modules
        if struct_module_source != self.current_module_source
            && let Some(struct_info) =
                self.lookup_struct_fields_in(&struct_name, &struct_module_source)
        {
            for (fname, _, is_pub) in &struct_info.fields {
                if !is_pub && fields.iter().any(|f| f.name == *fname) {
                    let _ = self.logger.error(TypeError::PrivateFieldAccess {
                        struct_name: struct_name.clone(),
                        field_name: fname.clone(),
                        span: struct_lit.span,
                    });
                }
            }
        }

        // Check if this is a generic struct and infer type arguments
        let (struct_type, mangled_struct_name, fields) = if self
            .generic_struct_names
            .contains(&struct_name)
        {
            // This is a generic struct - infer type arguments from field values.
            // `expected_type` lets the caller's annotation (e.g.
            // `let x: Container<i32> = Container { value: 0 }`) fill phantom
            // parameters that never appear in a field, matching the
            // behaviour of plain function calls.
            let type_args = self.infer_struct_type_args(&struct_name, &fields, expected_type);

            // Substitute type parameters in field value types.
            // This is necessary for empty array literals in self-referential fields
            // (e.g., `children: []` in `Node<K> { children: Array<&Node<K>> }`)
            // which get typed with TypeParams before inference.
            //
            // Use map-based substitution (TypeId → TypeId) instead of index-based, so
            // that only the struct's own TypeParam TypeIds are replaced. Index-based
            // substitution incorrectly replaces TypeParams from outer scopes (e.g., impl
            // type params) that happen to share the same index as the struct's TypeParams.
            let mut fields: Vec<TirStructField> = if type_args.is_empty() {
                fields
            } else {
                let struct_param_map: IndexMap<TypeId, TypeId> = self
                    .lookup_struct_fields(&struct_name)
                    .map(|info| {
                        info.type_param_type_ids
                            .iter()
                            .zip(type_args.iter())
                            .map(|(&param_id, &concrete_id)| (param_id, concrete_id))
                            .collect()
                    })
                    .unwrap_or_default();
                fields
                    .into_iter()
                    .map(|mut field| {
                        field.value.type_id = self
                            .substitute_type_params_by_map(field.value.type_id, &struct_param_map);
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

            // Check trait bounds on inferred type arguments
            if let Some(struct_info) = self
                .lookup_struct_fields_in(&struct_name, &struct_module_source)
                .cloned()
            {
                for (i, (param_name, bounds)) in struct_info.type_param_bounds.iter().enumerate() {
                    if let Some(&type_arg) = type_args.get(i) {
                        for bound in bounds {
                            if !self.type_implements_trait(type_arg, bound) {
                                let type_name = self.type_id_to_string(type_arg);
                                let _ = self.logger.error(TypeError::TraitBoundNotSatisfied {
                                    type_name,
                                    trait_name: bound.clone(),
                                    param_name: param_name.clone(),
                                    span: struct_lit.span,
                                });
                            }
                        }
                    }
                }
            }

            let struct_type = self.type_table.borrow_mut().make_generic_instance(
                struct_name.clone(),
                struct_module_source,
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
                .make_struct(struct_name.clone(), struct_module_source);
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

    /// Resolve an anonymous struct literal `{ x: 1, y: 2 }` by inferring a struct type
    /// from the field names and types.
    fn resolve_anonymous_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Resolve all field expressions first
        let mut resolved_fields: Vec<TirStructField> = Vec::new();
        for (index, field) in struct_lit.fields.iter().enumerate() {
            let value = self.resolve_expr(&field.value, ctx, None);
            resolved_fields.push(TirStructField {
                name: field.name.clone(),
                value,
                field_index: index as u32,
            });
        }

        // Generate a deterministic name from field names and types
        let anon_name = {
            let mut parts = Vec::new();
            for f in &resolved_fields {
                let type_name = self.type_table.borrow().type_name(f.value.type_id);
                parts.push(format!("{}:{}", f.name, type_name));
            }
            format!("__anon_{{{}}}", parts.join(","))
        };

        let module_source = self.current_module_source.clone();

        // Check if this anonymous struct type already exists (structural equivalence)
        if let Some(existing_type) = self
            .type_table
            .borrow()
            .find_struct_type(&anon_name, &module_source)
        {
            return TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type: existing_type,
                    struct_name: anon_name,
                    fields: resolved_fields,
                },
                existing_type,
                struct_lit.span,
            );
        }

        // Register the new anonymous struct type
        let struct_type = self
            .type_table
            .borrow_mut()
            .make_struct(anon_name.clone(), module_source.clone());

        // Register field info so field access works
        let field_info = super::types::StructFieldInfo {
            name: anon_name.clone(),
            module_source,
            fields: resolved_fields
                .iter()
                .map(|f| (f.name.clone(), f.value.type_id, true))
                .collect(),
            field_ast_ids: Vec::new(),
            field_defaults: vec![None; resolved_fields.len()],
            type_param_bounds: Vec::new(),
            type_param_type_ids: Vec::new(),
        };
        self.local_struct_fields
            .insert(anon_name.clone(), field_info);

        // Create TirStruct definition for codegen
        let tir_fields: Vec<TirField> = resolved_fields
            .iter()
            .enumerate()
            .map(|(i, f)| TirField {
                name: f.name.clone(),
                is_pub: true,
                type_id: f.value.type_id,
                index: i as u32,
                span: struct_lit.span,
                is_hidden: false,
                serde_rename: None,
                serde_default: false,
                default_expr: None,
            })
            .collect();

        self.pending_anonymous_structs.push(TirStruct {
            name: anon_name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: false,
            type_params: Vec::new(),
            monomorph_info: None,
            fields: tir_fields,
            span: struct_lit.span,
            serde_rename_all: None,
        });

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: anon_name,
                fields: resolved_fields,
            },
            struct_type,
            struct_lit.span,
        )
    }

    /// Infer type arguments for a generic struct from its field values, with
    /// optional expected-type driven back-inference for phantom parameters.
    ///
    /// Runs [`InferCtx`] over the struct's declared field types
    /// against the literal's resolved field values. If `expected_type` is a
    /// `GenericInstance` of the same struct (e.g. the caller wrote
    /// `let m: DirMap<Direction, i32> = DirMap { values: [] }`), the expected
    /// type-arguments are unified against the struct's declaration-order
    /// type-parameter ids so that phantom parameters (those that appear in no
    /// field) still end up concrete.
    ///
    /// Unlike the function/method inference sites this returns the *partial*
    /// result — unbound parameters fall back to their original `TypeParam`
    /// ids, which the monomorphizer then substitutes from the surrounding
    /// context. This matches the historical "phantoms are OK" behaviour.
    pub(super) fn infer_struct_type_args(
        &self,
        struct_name: &str,
        fields: &[TirStructField],
        expected_type: Option<TypeId>,
    ) -> Vec<TypeId> {
        let Some(struct_info) = self.lookup_struct_fields(struct_name).cloned() else {
            return vec![];
        };
        if struct_info.type_param_type_ids.is_empty() {
            return vec![];
        }

        let mut infer = InferCtx::new(&self.type_table, struct_info.type_param_type_ids.clone());

        for (struct_field, (_, expected_field_type, _)) in
            fields.iter().zip(struct_info.fields.iter())
        {
            infer.add(*expected_field_type, struct_field.value.type_id);
        }

        // Back-infer from the caller's expected type: if it's a GenericInstance
        // of this same struct, unify its type-args against the declaration-order
        // type params so phantoms (fields-less params) get concrete bindings.
        if let Some(expected) = expected_type {
            let expected_resolved = self.type_table.borrow().get(expected).clone();
            if let ResolvedType::GenericInstance {
                name,
                type_args: expected_args,
                ..
            } = expected_resolved
                && name == struct_info.name
                && expected_args.len() == struct_info.type_param_type_ids.len()
            {
                for (&param_id, &expected_arg) in struct_info
                    .type_param_type_ids
                    .iter()
                    .zip(expected_args.iter())
                {
                    infer.add_expected_return(param_id, expected_arg);
                }
            }
        }

        let (inferred, _) = infer.solve_with_phantoms();
        inferred
    }

    /// Check if a type contains a `TypePack` (variadic pack parameter).
    pub(super) fn type_contains_pack(&self, type_id: TypeId) -> bool {
        let ty = self.type_table.borrow().get(type_id).clone();
        match ty {
            ResolvedType::TypePack { .. } => true,
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } if TypeTable::is_tuple_type(&name, &module_source) => {
                type_args.iter().any(|e| self.type_contains_pack(*e))
            }
            _ => false,
        }
    }

    /// Resolve a tuple literal expression: `[1, 2, 3]` or `[1, "hello", true]`
    pub(super) fn resolve_tuple_literal(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // When the expected type is a concrete tuple of matching arity and the
        // literal has no spread elements, propagate per-element expected types
        // so numeric literals and nested tuples are coerced to the target shape.
        let expected_elem_types: Option<Vec<TypeId>> = expected_type.and_then(|ty| {
            let has_spread = tuple_lit
                .elements
                .iter()
                .any(|e| matches!(e, Expr::Spread(..)));
            if has_spread {
                return None;
            }
            let elems = self.type_table.borrow().as_tuple(ty)?;
            if elems.len() != tuple_lit.elements.len() {
                return None;
            }
            Some(elems)
        });

        // Resolve each element expression, handling spread elements
        let mut elements: Vec<TirExpr> = Vec::new();
        let mut elem_types: Vec<TypeId> = Vec::new();
        // Bindings for non-trivial spread expressions: (local_idx, name, expr, span)
        let mut spread_bindings: Vec<(u32, String, TirExpr, crate::token::Span)> = Vec::new();
        for (elem_idx, elem) in tuple_lit.elements.iter().enumerate() {
            if let Expr::Spread(inner, _span) = elem {
                let spread_expr = self.resolve_expr(inner, ctx, None);
                let contains_pack = self.type_contains_pack(spread_expr.type_id);
                let spread_type = self.type_table.borrow().get(spread_expr.type_id).clone();
                if contains_pack {
                    // Check if the expression's type IS a TypePack directly.
                    // If so, this is a type pack expansion (e.g., [..T::method()])
                    // where the expression template is instantiated per element.
                    // If the type is a Tuple containing TypePack, this is a value
                    // spread (e.g., [..rest] where rest: [..T]).
                    let is_direct_pack = matches!(
                        self.type_table.borrow().get(spread_expr.type_id),
                        ResolvedType::TypePack { .. }
                    );
                    if is_direct_pack {
                        let pack_type_id = spread_expr.type_id;
                        elem_types.push(spread_expr.type_id);
                        elements.push(TirExpr::new(
                            TirExprKind::TypePackExpansion {
                                call_expr: Box::new(spread_expr),
                                pack_type_id,
                            },
                            *elem_types.last().unwrap(),
                            elem.span(),
                        ));
                    } else {
                        // Keep as TupleSpread for monomorphization to expand later
                        elem_types.push(spread_expr.type_id);
                        elements.push(TirExpr::new(
                            TirExprKind::TupleSpread {
                                expr: Box::new(spread_expr),
                            },
                            *elem_types.last().unwrap(),
                            elem.span(),
                        ));
                    }
                } else if let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args: inner_elems,
                } = spread_type
                    && TypeTable::is_tuple_type(&name, &module_source)
                {
                    // For concrete tuples, expand inline via FieldAccess.
                    // If the expression is non-trivial (not a local), bind it to a
                    // temporary to ensure single evaluation.
                    let spread_ref = if matches!(spread_expr.kind, TirExprKind::Local { .. }) {
                        spread_expr
                    } else {
                        let spread_type_id = spread_expr.type_id;
                        let tmp_name = format!("__spread_{}", ctx.next_local);
                        let tmp_idx = ctx.add_local(tmp_name.clone(), spread_type_id, false, None);
                        spread_bindings.push((tmp_idx, tmp_name.clone(), spread_expr, elem.span()));
                        TirExpr::new(
                            TirExprKind::Local {
                                index: tmp_idx,
                                name: tmp_name,
                            },
                            spread_type_id,
                            elem.span(),
                        )
                    };
                    for (i, &et) in inner_elems.iter().enumerate() {
                        elements.push(TirExpr::new(
                            TirExprKind::FieldAccess {
                                expr: Box::new(spread_ref.clone()),
                                field_index: i as u32,
                                field_name: i.to_string(),
                            },
                            et,
                            elem.span(),
                        ));
                        elem_types.push(et);
                    }
                } else {
                    let _ = self
                        .logger
                        .error(crate::resolver::types::TypeError::InvalidLiteral {
                            message: "spread operator `..` can only be used with tuple types"
                                .to_string(),
                            span: elem.span(),
                        });
                    elem_types.push(spread_expr.type_id);
                    elements.push(spread_expr);
                }
            } else {
                let elem_expected = expected_elem_types.as_ref().map(|v| v[elem_idx]);
                let resolved = self.resolve_expr(elem, ctx, elem_expected);
                elem_types.push(resolved.type_id);
                elements.push(resolved);
            }
        }

        let tuple_type = self.type_table.borrow_mut().make_tuple(elem_types);

        let tuple_expr = TirExpr::new(
            TirExprKind::TupleLiteral { elements },
            tuple_type,
            tuple_lit.span,
        );

        if spread_bindings.is_empty() {
            tuple_expr
        } else {
            // Wrap in a block: { let __spread_N = expr; ...; [fields...] }
            let mut stmts: Vec<TirStmt> = spread_bindings
                .into_iter()
                .map(|(idx, name, value, span)| {
                    let type_id = value.type_id;
                    TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index: idx,
                            value,
                            is_mut: false,
                            is_reactive: false,
                            type_id,
                            skip_value_copy: false,
                        },
                        span,
                    )
                })
                .collect();
            // The tuple literal is the block's return expression
            stmts.push(TirStmt::new(TirStmtKind::Expr(tuple_expr), tuple_lit.span));
            let block = TirBlock::new(stmts, tuple_lit.span);
            TirExpr::new(TirExprKind::Block(block), tuple_type, tuple_lit.span)
        }
    }

    /// Resolve the postfix `?` operator.
    ///
    /// Desugars `expr?` into a match that unwraps the success case and
    /// performs an early return for the failure case.
    ///
    /// For `Result<T, E>` in a function returning `Result<U, F>`:
    ///   match expr { Ok(v) => v, Err(e) => return `Result::Err(F::from(e))` }
    ///
    /// For `Option<T>` in a function returning `Option<U>`:
    ///   match expr { Some(v) => v, None => return null }
    pub(super) fn resolve_question_mark(
        &mut self,
        qm: &ast::TryOpExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let inner = self.resolve_expr(&qm.expr, ctx, None);
        let inner_type = inner.type_id;
        let tt = self.type_table.borrow();
        let type_name = tt.type_name(inner_type);

        // Determine whether the operand is Option<T> or Result<T, E>
        let is_option = tt.as_option(inner_type).is_some();
        let is_result = matches!(
            tt.get(inner_type),
            ResolvedType::GenericInstance { name, .. } if name == "Result"
        );
        drop(tt);

        if !is_option && !is_result {
            let _ = self.logger.error(TypeError::InvalidQuestionMark {
                message: format!("cannot use ? on type {type_name}"),
                span: qm.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span);
        }

        // Check that the enclosing function returns a compatible type
        let return_type = ctx.return_type;
        let tt = self.type_table.borrow();
        let ret_is_option = tt.as_option(return_type).is_some();
        let ret_is_result = matches!(
            tt.get(return_type),
            ResolvedType::GenericInstance { name, .. } if name == "Result"
        );
        drop(tt);

        if is_option && !ret_is_option {
            let _ = self.logger.error(TypeError::InvalidQuestionMark {
                message: "cannot use ? on Option in a function returning Result".to_string(),
                span: qm.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span);
        }
        if is_result && !ret_is_result {
            if ret_is_option {
                let _ = self.logger.error(TypeError::InvalidQuestionMark {
                    message: "cannot use ? on Result in a function returning Option".to_string(),
                    span: qm.span,
                });
            } else {
                let _ = self.logger.error(TypeError::InvalidQuestionMark {
                    message: "? requires function to return Result or Option".to_string(),
                    span: qm.span,
                });
            }
            return TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span);
        }

        if is_option {
            self.resolve_question_mark_option(inner, ctx, qm.span)
        } else {
            self.resolve_question_mark_result(inner, ctx, qm.span)
        }
    }

    fn resolve_question_mark_option(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirExpr {
        let inner_type = inner.type_id;
        let tt = self.type_table.borrow();
        let some_type = tt.as_option(inner_type).unwrap();
        // Look up the `Some` / `None` case names through the
        // `CompilerItem` registry so a stdlib rename of either case
        // flows through the `?` lowering for `Option` without touching
        // this site.
        let items = tt.compiler_items();
        let some_name = items
            .variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
            .to_string();
        let none_name = items
            .variant_case_name(crate::compiler_item::CompilerItem::OptionNone)
            .to_string();
        drop(tt);

        // Allocate a local for the Some payload binding
        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), some_type, false, None);

        // Arm 0: Some(v) => v
        let some_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: some_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: some_type,
                }],
                payload_type: some_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                some_type,
                span,
            ),
            span,
        };

        // Arm 1: None => return null
        let none_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: none_name,
                bindings: vec![],
                payload_type: TypeTable::UNIT,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(TirExpr::new(TirExprKind::Null, inner_type, span)),
                        },
                        span,
                    )],
                    span,
                )),
                TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![some_arm, none_arm],
            },
            some_type,
            span,
        )
    }

    fn resolve_question_mark_result(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirExpr {
        let inner_type = inner.type_id;
        let return_type = ctx.return_type;

        // Extract T, E from inner Result<T, E>
        let tt = self.type_table.borrow();
        let (ok_type, inner_err_type) = match tt.get(inner_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => panic!("? operand must be Result<T, E>"),
        };
        // Extract F from return Result<U, F>
        let outer_err_type = match tt.get(return_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => type_args[1],
            _ => panic!("? return type must be Result<U, F>"),
        };
        drop(tt);

        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), ok_type, false, None);
        let e_local = ctx.add_local("__qm_e".to_string(), inner_err_type, false, None);

        // Look up the `Ok` / `Err` case names + indexes through the
        // `CompilerItem` registry so a stdlib rename of either case
        // flows through the `?` lowering path without touching this site.
        let (ok_name, _ok_index, err_name, err_index) = {
            let tt = self.type_table.borrow();
            let items = tt.compiler_items();
            let (_, _, ok_n, ok_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
            let (_, _, err_n, err_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
            (ok_n.to_string(), ok_i, err_n.to_string(), err_i)
        };

        // Arm 0: Ok(v) => v
        let ok_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: ok_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: ok_type,
                }],
                payload_type: ok_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                ok_type,
                span,
            ),
            span,
        };

        // Build the error conversion expression.
        // If inner_err_type == outer_err_type, just use the error directly.
        // Otherwise, call From::from(e) to convert.
        let e_expr = TirExpr::new(
            TirExprKind::Local {
                index: e_local,
                name: "__qm_e".to_string(),
            },
            inner_err_type,
            span,
        );
        let converted_err = if inner_err_type == outer_err_type {
            e_expr
        } else {
            self.resolve_from_call(outer_err_type, inner_err_type, e_expr, span)
        };

        // Build Result::Err(converted_err) with the function's return Result type
        let err_variant = TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: return_type,
                case_index: err_index,
                case_name: err_name.clone(),
                payload: Some(Box::new(converted_err)),
            },
            return_type,
            span,
        );

        // Arm 1: Err(e) => return Result::Err(From::from(e))
        let err_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: err_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_e".to_string(),
                    local_index: e_local,
                    type_id: inner_err_type,
                }],
                payload_type: inner_err_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(err_variant),
                        },
                        span,
                    )],
                    span,
                )),
                TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![ok_arm, err_arm],
            },
            ok_type,
            span,
        )
    }

    /// Generate a call to `From::from(value)` that converts `value` of type
    /// `from_type` to `target_type`.
    ///
    /// Looks up `impl From<from_type> for target_type` and generates
    /// `target_type::from(value)` as a static method call.
    pub(super) fn resolve_from_call(
        &mut self,
        target_type: TypeId,
        from_type: TypeId,
        value: TirExpr,
        span: Span,
    ) -> TirExpr {
        let tt = self.type_table.borrow();
        let target_name = tt.type_name(target_type);
        let from_name = tt.type_name(from_type);
        let from_trait_name = tt
            .compiler_items()
            .trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        drop(tt);

        // Use "From<SourceType>" as the trait name in mangled names to disambiguate
        // multiple From impls on the same target type.
        let from_trait = format!("{from_trait_name}<{from_name}>");
        let method_name = MethodName::format_local(&target_name, Some(&from_trait), "from");

        // Find the module source that provides the From impl
        let module_source = self.find_from_impl_module(&target_name, &from_name);

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source,
                    name: method_name,
                    monomorph_info: None,
                    method_info: Some(crate::name::LocalMethodName {
                        struct_name: target_name.clone(),
                        base_struct_name: target_name,
                        trait_name: Some(from_trait),
                        base_trait_name: Some(from_trait_name),
                        // Auto-derived `From` impl (synthesis-side): the
                        // dispatch builder never needs `From`'s declaring
                        // module because it's not an effect / resource.
                        base_trait_module: None,
                        trait_type_args: vec![],
                        method_name: "from".to_string(),
                        method_type_args: vec![],
                        is_type_param_receiver: false,
                        is_ref_impl: false,
                        cm_name: None,
                    }),
                },
                type_args: vec![],
                args: vec![CallArg::new(value, false)],
            },
            target_type,
            span,
        )
    }

    /// Find the module that provides `impl From<FromType> for TargetType`.
    fn find_from_impl_module(&self, target_name: &str, from_name: &str) -> ModuleSource {
        let from_trait_name = self
            .type_table
            .borrow()
            .compiler_items()
            .trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let check_impl = |impl_block: &ast::ImplBlock| -> bool {
            let impl_target = Self::get_type_name_static(&impl_block.ty);
            if impl_target != target_name {
                return false;
            }
            if let Some(trait_type) = &impl_block.trait_type {
                let base = Self::get_type_name_static(trait_type);
                if base != from_trait_name {
                    return false;
                }
                // Check the type arg matches from_name
                if let ast::Type::Generic(g) = trait_type
                    && let Some(arg) = g.args.first()
                {
                    return Self::get_type_name_static(arg) == from_name;
                }
            }
            false
        };

        // Search current module items
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && check_impl(impl_block)
            {
                return self.current_module_source.clone();
            }
        }

        // Search loaded modules
        for (source, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && check_impl(impl_block)
                {
                    return source.clone();
                }
            }
        }

        // Fallback: use current module (the From impl may be synthesized later)
        self.current_module_source.clone()
    }
}

enum LiteralOrdValue {
    Int(i128),
    Float(f64),
    Char(u32),
}

impl LiteralOrdValue {
    fn is_greater_than(&self, other: &Self) -> bool {
        match (self, other) {
            (LiteralOrdValue::Int(a), LiteralOrdValue::Int(b)) => a > b,
            (LiteralOrdValue::Float(a), LiteralOrdValue::Float(b)) => a > b,
            (LiteralOrdValue::Char(a), LiteralOrdValue::Char(b)) => a > b,
            _ => false, // different kinds — type mismatch error handles this
        }
    }
}

impl<H: CompilerHost> Resolver<'_, H> {
    /// Extract a compile-time orderable value from a literal expression.
    /// Returns the value in its native representation to avoid precision loss.
    fn extract_literal_ord_value(expr: &Expr) -> Option<LiteralOrdValue> {
        match expr {
            Expr::Literal(lit) => match &lit.value {
                Literal::Number(s) => {
                    let s = s.replace('_', "");
                    if s.contains('.') {
                        s.parse::<f64>().ok().map(LiteralOrdValue::Float)
                    } else if s.starts_with("0x") || s.starts_with("0X") {
                        i128::from_str_radix(&s[2..], 16)
                            .ok()
                            .map(LiteralOrdValue::Int)
                    } else if s.starts_with("0b") || s.starts_with("0B") {
                        i128::from_str_radix(&s[2..], 2)
                            .ok()
                            .map(LiteralOrdValue::Int)
                    } else if s.starts_with("0o") || s.starts_with("0O") {
                        i128::from_str_radix(&s[2..], 8)
                            .ok()
                            .map(LiteralOrdValue::Int)
                    } else {
                        s.parse::<i128>().ok().map(LiteralOrdValue::Int)
                    }
                }
                Literal::Char(s) => super::util::unescape_char(s)
                    .ok()
                    .map(|c| LiteralOrdValue::Char(c as u32)),
                _ => None,
            },
            Expr::Unary(unary) if unary.op == ast::UnaryOp::Neg => {
                match Self::extract_literal_ord_value(&unary.expr)? {
                    LiteralOrdValue::Int(v) => Some(LiteralOrdValue::Int(-v)),
                    LiteralOrdValue::Float(v) => Some(LiteralOrdValue::Float(-v)),
                    LiteralOrdValue::Char(_) => None,
                }
            }
            Expr::Cast(cast) => Self::extract_literal_ord_value(&cast.expr),
            _ => None,
        }
    }

    /// Resolve a range expression: `a..<b` or `a..=b`
    pub(super) fn resolve_range(
        &mut self,
        range: &crate::ast::RangeExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::ast::RangeKind;
        use crate::module_source::ModuleSource;
        use crate::tir::{TirExprKind, TirStructField};

        // Bidirectional coercion: resolve non-literal first to infer the element type
        let start_is_literal = self.is_numeric_literal(&range.start);
        let end_is_literal = self.is_numeric_literal(&range.end);

        let (start, end) = if start_is_literal && !end_is_literal {
            let end = self.resolve_expr(&range.end, ctx, None);
            let start = self.resolve_expr(&range.start, ctx, Some(end.type_id));
            (start, end)
        } else {
            let start = self.resolve_expr(&range.start, ctx, None);
            let end = self.resolve_expr(&range.end, ctx, Some(start.type_id));
            (start, end)
        };

        // Check type mismatch between start and end
        if start.type_id != end.type_id
            && start.type_id != TypeTable::ERROR
            && end.type_id != TypeTable::ERROR
        {
            let type_table = self.type_table.borrow();
            let start_name = type_table.type_name(start.type_id);
            let end_name = type_table.type_name(end.type_id);
            if start_name != end_name {
                let op_str = match range.kind {
                    RangeKind::Exclusive => "..<",
                    RangeKind::Inclusive => "..=",
                };
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: start_name,
                    found: format!(
                        "{end_name} (range `{op_str}` requires both operands to have the same type)"
                    ),
                    span: range.span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, range.span);
            }
        }

        let element_type = start.type_id;

        // Check that the element type implements Ord
        let ord_trait_name = self
            .type_table
            .borrow()
            .compiler_items()
            .trait_name(crate::compiler_item::CompilerItem::Ord)
            .to_string();
        if element_type != TypeTable::ERROR
            && !self.type_implements_trait(element_type, &ord_trait_name)
        {
            let type_name = self.type_id_to_string(element_type);
            let _ = self.logger.error(TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name: ord_trait_name,
                param_name: "T".to_string(),
                span: range.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, range.span);
        }

        // Check for reversed range literals (start > end)
        if let Some(start_val) = Self::extract_literal_ord_value(&range.start)
            && let Some(end_val) = Self::extract_literal_ord_value(&range.end)
        {
            let is_reversed = start_val.is_greater_than(&end_val);
            if is_reversed {
                let op_str = match range.kind {
                    RangeKind::Exclusive => "..<",
                    RangeKind::Inclusive => "..=",
                };
                let _ = self.logger.error(TypeError::InvalidLiteral {
                    message: format!(
                        "reversed range `{op_str}` is not supported (start must be less than end)"
                    ),
                    span: range.span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, range.span);
            }
        }

        let (struct_name, module_source, fields) = match range.kind {
            RangeKind::Exclusive => {
                let ms = self
                    .type_table
                    .borrow()
                    .compiler_items()
                    .struct_module(crate::compiler_item::CompilerItem::RangeExclusive)
                    .cloned()
                    .unwrap_or_else(ModuleSource::range);
                let fields = vec![
                    TirStructField {
                        name: "start".to_string(),
                        value: start,
                        field_index: 0,
                    },
                    TirStructField {
                        name: "end".to_string(),
                        value: end,
                        field_index: 1,
                    },
                ];
                ("RangeExclusive".to_string(), ms, fields)
            }
            RangeKind::Inclusive => {
                let ms = self
                    .type_table
                    .borrow()
                    .compiler_items()
                    .struct_module(crate::compiler_item::CompilerItem::RangeInclusive)
                    .cloned()
                    .unwrap_or_else(ModuleSource::range);
                let fields = vec![
                    TirStructField {
                        name: "start".to_string(),
                        value: start,
                        field_index: 0,
                    },
                    TirStructField {
                        name: "end".to_string(),
                        value: end,
                        field_index: 1,
                    },
                    TirStructField {
                        name: "exhausted".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(false),
                            TypeTable::BOOL,
                            range.span,
                        ),
                        field_index: 2,
                    },
                ];
                ("RangeInclusive".to_string(), ms, fields)
            }
        };

        let struct_type = self.type_table.borrow_mut().make_generic_instance(
            struct_name.clone(),
            module_source,
            vec![element_type],
        );

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            },
            struct_type,
            range.span,
        )
    }
}

/// Recursively patch any `Null` literals inside `expr` whose `Option<???>`
/// inner is still UNKNOWN, rewriting them to use `target_type`.
///
/// `Literal::Null` initially resolves to `Option<UNKNOWN>` (see
/// `resolve_literal`). When a `null` literal sits in a match arm or `if`
/// branch, its concrete inner type is determined by a sibling arm — but
/// that information isn't propagated back during arm resolution. Once the
/// match/if's overall result type is known we walk the arm bodies and
/// rewrite the unresolved `Null`'s `type_id` so WIR translation sees a
/// fully-resolved `Option<T>`.
///
/// Only the tail/result positions are walked; nested expressions whose
/// value is discarded (e.g. inside an arbitrary call argument) are not
/// affected — they would have been forced to a known type by their own
/// surrounding context anyway.
fn patch_unresolved_null(expr: &mut TirExpr, target_type: TypeId, type_table: &TypeTable) {
    if matches!(expr.kind, TirExprKind::Null) && type_table.contains_unknown(expr.type_id) {
        expr.type_id = target_type;
        return;
    }
    match &mut expr.kind {
        TirExprKind::Block(block) => {
            patch_unresolved_null_in_block(block, target_type, type_table);
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            patch_unresolved_null_in_block(then_branch, target_type, type_table);
            if let Some(eb) = else_branch {
                patch_unresolved_null_in_block(eb, target_type, type_table);
            }
        }
        TirExprKind::Match { arms, .. } => {
            for arm in arms {
                patch_unresolved_null(&mut arm.body, target_type, type_table);
            }
        }
        _ => {}
    }
}

fn patch_unresolved_null_in_block(
    block: &mut TirBlock,
    target_type: TypeId,
    type_table: &TypeTable,
) {
    let Some(last) = block.stmts.last_mut() else {
        return;
    };
    match &mut last.kind {
        TirStmtKind::Expr(e) => {
            patch_unresolved_null(e, target_type, type_table);
        }
        TirStmtKind::If {
            then_block,
            else_block: Some(eb),
            ..
        } => {
            patch_unresolved_null_in_block(then_block, target_type, type_table);
            patch_unresolved_null_in_block(eb, target_type, type_table);
        }
        TirStmtKind::IfLet {
            then_block,
            else_block: Some(eb),
            ..
        } => {
            patch_unresolved_null_in_block(then_block, target_type, type_table);
            patch_unresolved_null_in_block(eb, target_type, type_table);
        }
        _ => {}
    }
}

/// Walks a closure body and records outer-binding names that the body
/// mutates. Built on `AstVisitor`'s `walk_*` defaults, so every AST
/// node is descended into automatically — including future syntax that
/// adds new `Expr` / `Stmt` variants. Only three observations are
/// recorded:
///
/// * `Assign { target, .. }` / `CompoundAssign { target, .. }` —
///   extract the target's *root identifier* (a name that survives
///   `.field` and `[index]` accessors) and record it.
/// * Nested closures (`Expr::Closure(_)`) are NOT descended; they have
///   their own capture context and run their own collector.
///
/// Everything else falls through to `walk_*`, which recurses without
/// any per-variant code on this side. That's the property we want: no
/// `_ => {}` catch-all to silently miss new syntax.
struct MutatedVarsCollector<'a> {
    result: &'a mut IndexSet<String>,
}

impl MutatedVarsCollector<'_> {
    /// Walk an l-value down to its root identifier so `point.x = ...`
    /// and `arr[i] = ...` count as mutations of `point` / `arr`.
    fn root_ident_of_lvalue(expr: &ast::Expr) -> Option<&str> {
        match expr {
            ast::Expr::Ident(id) => Some(&id.name),
            ast::Expr::FieldAccess(fa) => Self::root_ident_of_lvalue(&fa.expr),
            ast::Expr::Index(idx) => Self::root_ident_of_lvalue(&idx.expr),
            _ => None,
        }
    }
}

impl AstVisitor for MutatedVarsCollector<'_> {
    fn visit_expr(&mut self, expr: &ast::Expr) {
        match expr {
            ast::Expr::Assign(a) => {
                if let Some(name) = Self::root_ident_of_lvalue(&a.target) {
                    self.result.insert(name.to_string());
                }
                // Still descend into the target (it may contain
                // sub-expressions like `arr[bump()] = ...`) and the value.
                ast::walk_expr(self, expr);
            }
            ast::Expr::CompoundAssign(ca) => {
                if let Some(name) = Self::root_ident_of_lvalue(&ca.target) {
                    self.result.insert(name.to_string());
                }
                ast::walk_expr(self, expr);
            }
            // Nested closures get their own capture context — skip.
            ast::Expr::Closure(_) => {}
            // Everything else: let the generic walker recurse into every
            // sub-expression. Adding new `Expr` variants therefore does
            // not require touching this collector.
            _ => ast::walk_expr(self, expr),
        }
    }
}
