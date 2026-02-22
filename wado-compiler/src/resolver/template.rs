//! Template string interpolation and format specifier resolution.

use crate::ast::{self, Item};
use crate::compiler_host::CompilerHost;
use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind,
    TirStructField, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::types::{FunctionContext, ParsedFormatSpec};

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let string_type = self.get_string_struct_type();
        let span = template.span;

        // Fast paths: empty template or single literal
        let has_interpolation = template
            .parts
            .iter()
            .any(|p| matches!(p, ast::TemplatePart::Interpolation { .. }));

        if !has_interpolation {
            // All-literal template: concatenate at compile time
            let mut combined = String::new();
            for part in &template.parts {
                if let ast::TemplatePart::String(s) = part {
                    combined.push_str(s);
                }
            }
            return TirExpr::new(TirExprKind::StringLiteral(combined), string_type, span);
        }

        // Single interpolation, no literals around it, no format spec, and already String?
        if template.parts.len() == 1
            && let ast::TemplatePart::Interpolation { expr, format: None } = &template.parts[0]
        {
            let resolved = self.resolve_expr(expr, ctx, None);
            if resolved.type_id == string_type {
                return resolved;
            }
        }

        // --- Build a labeled block: __tmpl: { let mut __r = ...; ...; break __tmpl: __r; } ---
        let label = "__tmpl".to_string();

        // Estimate capacity: sum of literal lengths + 16 per interpolation
        let capacity_estimate: i64 = template
            .parts
            .iter()
            .map(|p| match p {
                ast::TemplatePart::String(s) => s.len() as i64,
                ast::TemplatePart::Interpolation { .. } => 16,
            })
            .sum();

        // Enter a new scope for the block
        ctx.enter_scope();

        // let mut __r = String::with_capacity(N);
        let buf_index = ctx.add_local("__r".to_string(), string_type, true);
        let with_capacity_call = TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: ModuleSource::string(),
                    name: "String::with_capacity".to_string(),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        "String".to_string(),
                        None,
                        "with_capacity".to_string(),
                    )),
                },
                args: vec![TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: capacity_estimate as u64,
                        repr: capacity_estimate.to_string(),
                    },
                    TypeTable::I32,
                    span,
                )],
            },
            string_type,
            span,
        );
        let mut stmts = vec![TirStmt::new(
            TirStmtKind::Let {
                name: "__r".to_string(),
                local_index: buf_index,
                is_mut: true,
                is_reactive: false,
                type_id: string_type,
                value: with_capacity_call,
            },
            span,
        )];

        // Prepare Formatter type and its &mut type
        let formatter_type = self
            .type_table
            .borrow_mut()
            .make_struct("Formatter".to_string(), ModuleSource::format());
        let mut_ref_formatter = self.type_table.borrow_mut().make_mut_ref(formatter_type);

        // Track whether we've created the __f local yet
        let mut fmt_local_index: Option<u32> = None;

        // Helper closures can't capture &mut self, so we build parts inline
        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    if s.is_empty() {
                        continue;
                    }
                    // __r.append("literal")
                    let buf_ref = TirExpr::new(
                        TirExprKind::Local {
                            index: buf_index,
                            name: "__r".to_string(),
                        },
                        string_type,
                        span,
                    );
                    let append_call = TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(buf_ref),
                            func: FunctionRef::External {
                                module_source: ModuleSource::string(),
                                name: "String::append".to_string(),
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    "String".to_string(),
                                    None,
                                    "append".to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![TirExpr::new(
                                TirExprKind::StringLiteral(s.clone()),
                                string_type,
                                span,
                            )],
                        },
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(append_call), span));
                }
                ast::TemplatePart::Interpolation { expr, format } => {
                    let resolved = self.resolve_expr(expr, ctx, None);

                    // If String type with no format spec, just append directly
                    if resolved.type_id == string_type && format.is_none() {
                        let buf_ref = TirExpr::new(
                            TirExprKind::Local {
                                index: buf_index,
                                name: "__r".to_string(),
                            },
                            string_type,
                            span,
                        );
                        let append_call = TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(buf_ref),
                                func: FunctionRef::External {
                                    module_source: ModuleSource::string(),
                                    name: "String::append".to_string(),
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        "String".to_string(),
                                        None,
                                        "append".to_string(),
                                    )),
                                },
                                type_args: vec![],
                                args: vec![resolved],
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(append_call), span));
                        continue;
                    }

                    // Parse format spec (if any) to determine trait + Formatter fields
                    let parsed = format.as_ref().map(|f| self.parse_format_spec(&f.spec));

                    // Check if this is an inspect format specifier (:?)
                    let is_inspect = parsed.as_ref().is_some_and(|pf| pf.type_char == Some('?'));

                    // Determine which trait's fmt to call
                    let (trait_name, _trait_type_char) = match &parsed {
                        Some(pf) => match pf.type_char {
                            Some('b') => ("Binary", Some('b')),
                            Some('o') => ("Octal", Some('o')),
                            Some('x') => ("LowerHex", Some('x')),
                            Some('X') => ("UpperHex", Some('X')),
                            Some('e') => ("LowerExp", Some('e')),
                            Some('E') => ("UpperExp", Some('E')),
                            Some('?') => ("Display", None), // placeholder, not used
                            _ => ("Display", None),
                        },
                        None => ("Display", None),
                    };

                    // Create or reassign Formatter local
                    let fmt_index = if let Some(idx) = fmt_local_index {
                        // Reassign __f = Formatter::new(&mut __r) or Formatter { ... }
                        let formatter_expr = self.build_formatter_expr(
                            buf_index,
                            string_type,
                            formatter_type,
                            &parsed,
                            span,
                        );
                        let assign = TirExpr::new(
                            TirExprKind::Assign {
                                target: Box::new(TirExpr::new(
                                    TirExprKind::Local {
                                        index: idx,
                                        name: "__f".to_string(),
                                    },
                                    formatter_type,
                                    span,
                                )),
                                value: Box::new(formatter_expr),
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(assign), span));
                        idx
                    } else {
                        // First interpolation: let mut __f = ...
                        let idx = ctx.add_local("__f".to_string(), formatter_type, true);
                        fmt_local_index = Some(idx);
                        let formatter_expr = self.build_formatter_expr(
                            buf_index,
                            string_type,
                            formatter_type,
                            &parsed,
                            span,
                        );
                        stmts.push(TirStmt::new(
                            TirStmtKind::Let {
                                name: "__f".to_string(),
                                local_index: idx,
                                is_mut: true,
                                is_reactive: false,
                                type_id: formatter_type,
                                value: formatter_expr,
                            },
                            span,
                        ));
                        idx
                    };

                    // Check if this is a float type with precision → call
                    // fmt_f64_fixed/fmt_f32_fixed directly to avoid pulling in
                    // the fixed-point bundled code when only shortest is needed.
                    let float_fixed_func = if trait_name == "Display"
                        && parsed.as_ref().is_some_and(|pf| pf.precision.is_some())
                    {
                        let resolved_type = self.type_table.borrow().get(resolved.type_id).clone();
                        match resolved_type {
                            ResolvedType::Primitive(PrimitiveType::F64) => Some("fmt_f64_fixed"),
                            ResolvedType::Primitive(PrimitiveType::F32) => Some("fmt_f32_fixed"),
                            _ => None,
                        }
                    } else {
                        None
                    };

                    let fmt_mut_ref = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: fmt_index,
                                    name: "__f".to_string(),
                                },
                                formatter_type,
                                span,
                            )),
                        },
                        mut_ref_formatter,
                        span,
                    );

                    if is_inspect {
                        // Emit builtin::inspect marker — replaced by synthesis::inspect phase
                        // Pass alternate flag (#) as 3rd arg for closure pretty-print
                        let alternate = parsed.as_ref().is_some_and(|pf| pf.alternate);
                        let alternate_expr = TirExpr::new(
                            TirExprKind::BoolLiteral(alternate),
                            TypeTable::BOOL,
                            span,
                        );
                        let inspect_call = TirExpr::new(
                            TirExprKind::StaticCall {
                                func: FunctionRef::External {
                                    module_source: ModuleSource::builtin(),
                                    name: "builtin::inspect".to_string(),
                                    monomorph_info: None,
                                    method_info: None,
                                },
                                args: vec![resolved, fmt_mut_ref, alternate_expr],
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(inspect_call), span));
                    } else if let Some(func_name) = float_fixed_func {
                        // Direct call: fmt_f64_fixed(value, precision, &mut __f)
                        let precision_value = parsed.as_ref().unwrap().precision.unwrap();
                        let precision_expr = TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: precision_value as u64,
                                repr: precision_value.to_string(),
                            },
                            TypeTable::I32,
                            span,
                        );
                        let fmt_call = TirExpr::new(
                            TirExprKind::StaticCall {
                                func: FunctionRef::External {
                                    module_source: ModuleSource::primitives(),
                                    name: func_name.to_string(),
                                    monomorph_info: None,
                                    method_info: None,
                                },
                                args: vec![resolved, precision_expr, fmt_mut_ref],
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(fmt_call), span));
                    } else {
                        // Standard path: receiver.fmt(&mut __f)
                        // Fall back to inspect if no Display impl is found
                        let base_type_id = self.get_ultimate_base_type(resolved.type_id);
                        let display_impl = self.resolve_display_impl_source(
                            base_type_id,
                            resolved.type_id,
                            trait_name,
                        );

                        if let Some((receiver_type_name, impl_module_source)) = display_impl {
                            let receiver_expr = {
                                let resolved_type =
                                    self.type_table.borrow().get(resolved.type_id).clone();
                                match resolved_type {
                                    ResolvedType::Ref(_) | ResolvedType::MutRef(_) => resolved,
                                    _ => {
                                        let ref_type =
                                            self.type_table.borrow_mut().make_ref(resolved.type_id);
                                        TirExpr::new(
                                            TirExprKind::Unary {
                                                op: TirUnaryOp::Ref,
                                                expr: Box::new(resolved),
                                            },
                                            ref_type,
                                            span,
                                        )
                                    }
                                }
                            };
                            let mangled_name = MethodName::format_local(
                                &receiver_type_name,
                                Some(trait_name),
                                "fmt",
                            );
                            let fmt_call = TirExpr::new(
                                TirExprKind::MethodCall {
                                    receiver: Box::new(receiver_expr),
                                    func: FunctionRef::External {
                                        module_source: impl_module_source,
                                        name: mangled_name,
                                        monomorph_info: None,
                                        method_info: Some(LocalMethodName::new(
                                            receiver_type_name,
                                            Some(trait_name.to_string()),
                                            "fmt".to_string(),
                                        )),
                                    },
                                    type_args: vec![],
                                    args: vec![fmt_mut_ref],
                                },
                                TypeTable::UNIT,
                                span,
                            );
                            stmts.push(TirStmt::new(TirStmtKind::Expr(fmt_call), span));
                        } else {
                            // No Display impl found — fall back to inspect
                            let alternate_expr = TirExpr::new(
                                TirExprKind::BoolLiteral(false),
                                TypeTable::BOOL,
                                span,
                            );
                            let inspect_call = TirExpr::new(
                                TirExprKind::StaticCall {
                                    func: FunctionRef::External {
                                        module_source: ModuleSource::builtin(),
                                        name: "builtin::inspect".to_string(),
                                        monomorph_info: None,
                                        method_info: None,
                                    },
                                    args: vec![resolved, fmt_mut_ref, alternate_expr],
                                },
                                TypeTable::UNIT,
                                span,
                            );
                            stmts.push(TirStmt::new(TirStmtKind::Expr(inspect_call), span));
                        }
                    }
                }
            }
        }

        // break __tmpl: __r;
        let buf_final = TirExpr::new(
            TirExprKind::Local {
                index: buf_index,
                name: "__r".to_string(),
            },
            string_type,
            span,
        );
        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(buf_final),
            },
            span,
        ));

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: string_type,
            },
            string_type,
            span,
        )
    }

    /// Parse a format specifier string like "05", "<10", "#x", ".2" etc.
    /// Syntax: `[[fill]align][sign][#][0][width][.precision]type`
    pub(super) fn parse_format_spec(&self, spec: &str) -> ParsedFormatSpec {
        let chars: Vec<char> = spec.chars().collect();
        let len = chars.len();
        let mut i = 0;

        let mut fill = None;
        let mut align = None;
        let mut sign_plus = false;
        let mut alternate = false;
        let mut zero_pad = false;
        let mut width = None;
        let mut precision = None;
        let mut type_char = None;

        // Parse [fill][align]: fill is any char, align is '<', '^', '>'
        if i + 1 < len && matches!(chars[i + 1], '<' | '^' | '>') {
            fill = Some(chars[i]);
            align = Some(chars[i + 1]);
            i += 2;
        } else if i < len && matches!(chars[i], '<' | '^' | '>') {
            align = Some(chars[i]);
            i += 1;
        }

        // Parse [sign]: '+'
        if i < len && chars[i] == '+' {
            sign_plus = true;
            i += 1;
        }

        // Parse [#]: alternate form
        if i < len && chars[i] == '#' {
            alternate = true;
            i += 1;
        }

        // Parse [0]: zero-pad
        if i < len && chars[i] == '0' && (i + 1 >= len || chars[i + 1].is_ascii_digit()) {
            zero_pad = true;
            i += 1;
        }

        // Parse [width]: digits
        let width_start = i;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i > width_start {
            let w: String = chars[width_start..i].iter().collect();
            width = w.parse().ok();
        }

        // Parse [.precision]: '.' followed by digits
        if i < len && chars[i] == '.' {
            i += 1;
            let prec_start = i;
            while i < len && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i > prec_start {
                let p: String = chars[prec_start..i].iter().collect();
                precision = p.parse().ok();
            } else {
                precision = Some(0);
            }
        }

        // Parse type: b, o, x, X, e, E, ?
        if i < len && matches!(chars[i], 'b' | 'o' | 'x' | 'X' | 'e' | 'E' | '?') {
            type_char = Some(chars[i]);
        }

        ParsedFormatSpec {
            fill,
            align,
            sign_plus,
            alternate,
            zero_pad,
            width,
            precision,
            type_char,
        }
    }

    /// Build a `Formatter::new(&mut __r)` or `Formatter { fill: ..., buf: &mut __r }` expression.
    pub(super) fn build_formatter_expr(
        &mut self,
        buf_index: u32,
        string_type: TypeId,
        formatter_type: TypeId,
        parsed: &Option<ParsedFormatSpec>,
        span: Span,
    ) -> TirExpr {
        let mut_ref_string = self.type_table.borrow_mut().make_mut_ref(string_type);
        let buf_mut_ref = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: buf_index,
                        name: "__r".to_string(),
                    },
                    string_type,
                    span,
                )),
            },
            mut_ref_string,
            span,
        );

        let has_custom_spec = parsed.as_ref().is_some_and(|p| {
            p.fill.is_some()
                || p.align.is_some()
                || p.sign_plus
                || p.alternate
                || p.zero_pad
                || p.width.is_some()
                || p.precision.is_some()
        });

        if !has_custom_spec {
            // Formatter::new(&mut __r)
            return TirExpr::new(
                TirExprKind::StaticCall {
                    func: FunctionRef::External {
                        module_source: ModuleSource::format(),
                        name: "Formatter::new".to_string(),
                        monomorph_info: None,
                        method_info: Some(LocalMethodName::new(
                            "Formatter".to_string(),
                            None,
                            "new".to_string(),
                        )),
                    },
                    args: vec![buf_mut_ref],
                },
                formatter_type,
                span,
            );
        }

        // Construct Formatter struct literal with custom fields
        let pf = parsed.as_ref().unwrap();
        let alignment_type = self
            .type_table
            .borrow_mut()
            .make_enum("Alignment".to_string(), ModuleSource::format());
        let fill_char = pf.fill.unwrap_or(if pf.zero_pad { '0' } else { ' ' });
        let align_index: u32 = match pf.align {
            Some('<') => 0, // Left
            Some('^') => 1, // Center
            _ => 2,         // Right (default)
        };
        let align_name = match align_index {
            0 => "Left",
            1 => "Center",
            _ => "Right",
        };

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: formatter_type,
                struct_name: "Formatter".to_string(),
                fields: vec![
                    TirStructField {
                        name: "fill".to_string(),
                        value: TirExpr::new(
                            TirExprKind::CharLiteral(fill_char),
                            TypeTable::CHAR,
                            span,
                        ),
                        field_index: 0,
                    },
                    TirStructField {
                        name: "align".to_string(),
                        value: TirExpr::new(
                            TirExprKind::EnumConstruct {
                                enum_type: alignment_type,
                                case_index: align_index,
                                case_name: align_name.to_string(),
                            },
                            alignment_type,
                            span,
                        ),
                        field_index: 1,
                    },
                    TirStructField {
                        name: "sign_plus".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(pf.sign_plus),
                            TypeTable::BOOL,
                            span,
                        ),
                        field_index: 2,
                    },
                    TirStructField {
                        name: "alternate".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(pf.alternate),
                            TypeTable::BOOL,
                            span,
                        ),
                        field_index: 3,
                    },
                    TirStructField {
                        name: "zero_pad".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(pf.zero_pad),
                            TypeTable::BOOL,
                            span,
                        ),
                        field_index: 4,
                    },
                    TirStructField {
                        name: "width".to_string(),
                        value: TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: pf.width.unwrap_or(-1) as u64,
                                repr: pf.width.unwrap_or(-1).to_string(),
                            },
                            TypeTable::I32,
                            span,
                        ),
                        field_index: 5,
                    },
                    TirStructField {
                        name: "precision".to_string(),
                        value: TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: pf.precision.unwrap_or(-1) as u64,
                                repr: pf.precision.unwrap_or(-1).to_string(),
                            },
                            TypeTable::I32,
                            span,
                        ),
                        field_index: 6,
                    },
                    TirStructField {
                        name: "buf".to_string(),
                        value: buf_mut_ref,
                        field_index: 7,
                    },
                ],
            },
            formatter_type,
            span,
        )
    }

    /// Determine the module source for a format trait impl (Display, Binary, etc.)
    pub(super) fn resolve_display_impl_source(
        &self,
        base_type_id: TypeId,
        original_type_id: TypeId,
        trait_name: &str,
    ) -> Option<(String, ModuleSource)> {
        let type_name = match self.type_table.borrow().get(base_type_id).clone() {
            ResolvedType::Struct { name, .. } | ResolvedType::GenericInstance { name, .. } => {
                name.clone()
            }
            ResolvedType::Primitive(_) => self.type_table.borrow().mangle_type_name(base_type_id),
            _ => self.type_table.borrow().mangle_type_name(original_type_id),
        };

        // Search for the actual module where `impl TraitName for TypeName` is defined,
        // since the trait impl may live in a different module than the type itself
        // (e.g., `impl Display for String` is in format.wado, not string.wado).
        for (module_src, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    let impl_type_name = self.get_type_name(&impl_block.ty);
                    let impl_trait_name = self.get_type_name(trait_type);
                    if impl_type_name == type_name && impl_trait_name == trait_name {
                        return Some((type_name, module_src.clone()));
                    }
                }
            }
        }

        // Also check current module
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
            {
                let impl_type_name = self.get_type_name(&impl_block.ty);
                let impl_trait_name = self.get_type_name(trait_type);
                if impl_type_name == type_name && impl_trait_name == trait_name {
                    return Some((type_name, self.current_module_source.clone()));
                }
            }
        }

        // Primitives always have Display in prelude
        if matches!(
            self.type_table.borrow().get(base_type_id),
            ResolvedType::Primitive(_)
        ) {
            return Some((type_name, ModuleSource::primitives()));
        }

        None
    }
}
