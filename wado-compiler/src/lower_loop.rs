//! Loop lowering pass for Wado TIR
//!
//! This phase transforms all loop constructs to the canonical Loop form:
//! - While -> Loop + If + Break
//! - For -> Loop + If + Break (with labeled body for continue handling)
//! - `ForOf` -> Loop + `IfPattern` + Break (using iterator pattern)
//! - `WhilePattern` -> Loop + `IfPattern` + Break
//! - `ForPattern` -> Loop + `IfPattern` + Break (with labeled body for continue handling)
//!
//! This simplifies codegen and optimizer by having only one loop construct to handle.

use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirModule, TirStmt, TirStmtKind, TirUnaryOp,
};

/// Lower all loops in a module to canonical Loop form
pub fn lower_loops(module: &mut TirModule) {
    let mut lowerer = LoopLowerer::new();
    lowerer.lower_module(module);
}

/// Counter for generating unique labels for for-loop bodies
struct LoopLowerer {
    label_counter: u32,
}

impl LoopLowerer {
    fn new() -> Self {
        Self { label_counter: 0 }
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let label = format!("__{prefix}_{}", self.label_counter);
        self.label_counter += 1;
        label
    }

    fn lower_module(&mut self, module: &mut TirModule) {
        for func in &mut module.functions {
            self.lower_function(&mut func.borrow_mut());
        }
    }

    fn lower_function(&mut self, func: &mut TirFunction) {
        if let Some(body) = &mut func.body {
            self.lower_block(body);
        }
    }

    fn lower_block(&mut self, block: &mut TirBlock) {
        let mut new_stmts = Vec::with_capacity(block.stmts.len());

        for stmt in std::mem::take(&mut block.stmts) {
            let lowered = self.lower_stmt(stmt);
            new_stmts.extend(lowered);
        }

        block.stmts = new_stmts;
    }

    fn lower_stmt(&mut self, mut stmt: TirStmt) -> Vec<TirStmt> {
        match &mut stmt.kind {
            // While -> Loop + If + Break
            TirStmtKind::While { condition, body } => {
                let span = stmt.span;

                // First lower the body recursively
                self.lower_block(body);
                self.lower_expr(condition);

                // Clone condition before moving body
                let condition_clone = condition.clone();
                let condition_type_id = condition.type_id;
                let body_span = body.span;
                let body_stmts = std::mem::take(&mut body.stmts);

                // Create: if !condition { break; }
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let break_block = TirBlock {
                    stmts: vec![break_stmt],
                    span,
                };

                // Negate condition: !condition
                let negated_condition = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(condition_clone),
                    },
                    condition_type_id,
                    span,
                );

                let if_break = TirStmt::new(
                    TirStmtKind::If {
                        condition: negated_condition,
                        then_block: break_block,
                        else_block: None,
                    },
                    span,
                );

                // Prepend if_break to body
                let mut loop_body_stmts = vec![if_break];
                loop_body_stmts.extend(body_stmts);
                let loop_body = TirBlock {
                    stmts: loop_body_stmts,
                    span: body_span,
                };

                // Create: loop { if !condition { break; } body }
                let loop_stmt = TirStmt::new(TirStmtKind::Loop { body: loop_body }, span);

                vec![loop_stmt]
            }

            // WhilePattern -> Loop + IfPattern + Break
            TirStmtKind::WhilePattern {
                scrutinee,
                pattern,
                body,
            } => {
                let span = stmt.span;

                // First lower the body recursively
                self.lower_block(body);
                self.lower_expr(scrutinee);

                // Clone values before moving
                let scrutinee_clone = scrutinee.clone();
                let pattern_clone = pattern.clone();
                let body_clone = body.clone();

                // Create: if let pattern = scrutinee { body } else { break; }
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let break_block = TirBlock {
                    stmts: vec![break_stmt],
                    span,
                };

                let if_pattern = TirStmt::new(
                    TirStmtKind::IfPattern {
                        scrutinee: scrutinee_clone,
                        pattern: pattern_clone,
                        then_block: body_clone,
                        else_block: Some(break_block),
                    },
                    span,
                );

                let loop_body = TirBlock {
                    stmts: vec![if_pattern],
                    span,
                };

                let loop_stmt = TirStmt::new(TirStmtKind::Loop { body: loop_body }, span);

                vec![loop_stmt]
            }

            // For -> Loop + If + Break (with labeled body for continue)
            // Structure:
            //   __for_loop: {
            //       init
            //       loop {
            //           if !condition { break __for_loop; }
            //           __for_body: {
            //               body  // break → break __for_loop, continue → break __for_body
            //           }
            //           update
            //       }
            //   }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                let span = stmt.span;

                // Generate label for the entire for loop (for break statements)
                let for_loop_label = self.fresh_label("for_loop");

                // First lower init statements, body, condition, and update recursively
                let mut lowered_init = Vec::new();
                for init_stmt in std::mem::take(init) {
                    let lowered = self.lower_stmt(init_stmt);
                    lowered_init.extend(lowered);
                }
                self.lower_block(body);
                if let Some(cond) = condition {
                    self.lower_expr(cond);
                }
                if let Some(upd) = update {
                    self.lower_expr(upd);
                }

                // Transform break in body to break __for_loop
                self.transform_break_to_labeled(&mut body.stmts, &for_loop_label);

                // Transform continue in body to break __for_body (if there's an update)
                let for_body_label = if update.is_some() {
                    let label = self.fresh_label("for_body");
                    self.transform_continue_to_break(&mut body.stmts, &label);
                    Some(label)
                } else {
                    None
                };

                let body_span = body.span;
                let body_stmts = std::mem::take(&mut body.stmts);
                let mut loop_body_stmts = Vec::new();

                // Add condition check if present: if !condition { break __for_loop; }
                if let Some(cond) = condition.take() {
                    let cond_type_id = cond.type_id;
                    let break_stmt = TirStmt::new(
                        TirStmtKind::Break {
                            label: Some(for_loop_label.clone()),
                            value: None,
                        },
                        span,
                    );
                    let break_block = TirBlock {
                        stmts: vec![break_stmt],
                        span,
                    };
                    let negated_condition = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Not,
                            expr: Box::new(cond),
                        },
                        cond_type_id,
                        span,
                    );
                    let if_break = TirStmt::new(
                        TirStmtKind::If {
                            condition: negated_condition,
                            then_block: break_block,
                            else_block: None,
                        },
                        span,
                    );
                    loop_body_stmts.push(if_break);
                }

                // Add body (wrapped in labeled block if there's an update)
                if let Some(label) = for_body_label {
                    let labeled_body = TirStmt::new(
                        TirStmtKind::LabeledBlock {
                            label,
                            block: TirBlock {
                                stmts: body_stmts,
                                span: body_span,
                            },
                        },
                        body_span,
                    );
                    loop_body_stmts.push(labeled_body);
                } else {
                    loop_body_stmts.extend(body_stmts);
                }

                // Add update expression if present
                if let Some(upd) = update.take() {
                    let update_stmt = TirStmt::new(TirStmtKind::Expr(upd), span);
                    loop_body_stmts.push(update_stmt);
                }

                let loop_body = TirBlock {
                    stmts: loop_body_stmts,
                    span: body_span,
                };

                let loop_stmt = TirStmt::new(TirStmtKind::Loop { body: loop_body }, span);

                // Wrap in block with init statements and for_loop label
                lowered_init.push(loop_stmt);

                // Always wrap in a labeled block for break to work
                vec![TirStmt::new(
                    TirStmtKind::LabeledBlock {
                        label: for_loop_label,
                        block: TirBlock {
                            stmts: lowered_init,
                            span,
                        },
                    },
                    span,
                )]
            }

            // ForPattern -> Loop + IfPattern + Break (with labeled body for continue)
            // Structure:
            //   __for_loop: {
            //       init
            //       loop {
            //           if let pattern = scrutinee {
            //               __for_body: {
            //                   body  // break → break __for_loop, continue → break __for_body
            //               }
            //               update
            //           } else {
            //               break __for_loop;
            //           }
            //       }
            //   }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                pattern,
                body,
                update,
            } => {
                let span = stmt.span;

                // Generate label for the entire for loop (for break statements)
                let for_loop_label = self.fresh_label("for_loop");

                // First lower init statements, body, scrutinee, and update recursively
                let mut lowered_init = Vec::new();
                for init_stmt in std::mem::take(init) {
                    let lowered = self.lower_stmt(init_stmt);
                    lowered_init.extend(lowered);
                }
                self.lower_block(body);
                self.lower_expr(scrutinee);
                if let Some(upd) = update {
                    self.lower_expr(upd);
                }

                // Transform break in body to break __for_loop
                self.transform_break_to_labeled(&mut body.stmts, &for_loop_label);

                // Transform continue in body to break __for_body (if there's an update)
                let for_body_label = if update.is_some() {
                    let label = self.fresh_label("for_body");
                    self.transform_continue_to_break(&mut body.stmts, &label);
                    Some(label)
                } else {
                    None
                };

                let body_span = body.span;
                let body_stmts = std::mem::take(&mut body.stmts);

                // Build the then block (body + update, wrapped in labeled block if needed)
                let mut then_stmts = if let Some(label) = for_body_label {
                    vec![TirStmt::new(
                        TirStmtKind::LabeledBlock {
                            label,
                            block: TirBlock {
                                stmts: body_stmts,
                                span: body_span,
                            },
                        },
                        body_span,
                    )]
                } else {
                    body_stmts
                };

                // Add update expression after the body (inside then block)
                if let Some(upd) = update.take() {
                    let update_stmt = TirStmt::new(TirStmtKind::Expr(upd), span);
                    then_stmts.push(update_stmt);
                }

                let then_block = TirBlock {
                    stmts: then_stmts,
                    span: body_span,
                };

                // Create else block with break __for_loop
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: Some(for_loop_label.clone()),
                        value: None,
                    },
                    span,
                );
                let else_block = TirBlock {
                    stmts: vec![break_stmt],
                    span,
                };

                // Clone values before moving
                let scrutinee_clone = scrutinee.clone();
                let pattern_clone = pattern.clone();

                // Create: if let pattern = scrutinee { body; update } else { break __for_loop; }
                let if_pattern = TirStmt::new(
                    TirStmtKind::IfPattern {
                        scrutinee: scrutinee_clone,
                        pattern: pattern_clone,
                        then_block,
                        else_block: Some(else_block),
                    },
                    span,
                );

                let loop_body = TirBlock {
                    stmts: vec![if_pattern],
                    span,
                };

                let loop_stmt = TirStmt::new(TirStmtKind::Loop { body: loop_body }, span);

                // Wrap in block with init statements and for_loop label
                lowered_init.push(loop_stmt);

                // Always wrap in a labeled block for break to work
                vec![TirStmt::new(
                    TirStmtKind::LabeledBlock {
                        label: for_loop_label,
                        block: TirBlock {
                            stmts: lowered_init,
                            span,
                        },
                    },
                    span,
                )]
            }

            // ForOf -> Loop + IfPattern + Break (using iterator pattern)
            // Note: ForOf is currently kept as-is for now because it requires
            // generating method calls (into_iter, next) which need type resolution.
            // This will be implemented when the infrastructure is ready.
            TirStmtKind::ForOf { body, iterable, .. } => {
                // For now, just lower recursively but keep ForOf
                self.lower_block(body);
                self.lower_expr(iterable);
                vec![stmt]
            }

            // Recursively process other statement kinds
            TirStmtKind::Loop { body } => {
                self.lower_block(body);
                vec![stmt]
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.lower_expr(condition);
                self.lower_block(then_block);
                if let Some(else_b) = else_block {
                    self.lower_block(else_b);
                }
                vec![stmt]
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.lower_expr(scrutinee);
                self.lower_block(then_block);
                if let Some(else_b) = else_block {
                    self.lower_block(else_b);
                }
                vec![stmt]
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                self.lower_block(block);
                vec![stmt]
            }
            TirStmtKind::Let { value, .. } => {
                self.lower_expr(value);
                vec![stmt]
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.lower_expr(value);
                vec![stmt]
            }
            TirStmtKind::Expr(expr) => {
                self.lower_expr(expr);
                vec![stmt]
            }
            TirStmtKind::Return { value } => {
                if let Some(v) = value {
                    self.lower_expr(v);
                }
                vec![stmt]
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.lower_expr(v);
                }
                vec![stmt]
            }
            TirStmtKind::Continue => vec![stmt],
        }
    }

    fn lower_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.lower_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.lower_expr(condition);
                self.lower_block(then_branch);
                if let Some(else_b) = else_branch {
                    self.lower_block(else_b);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.lower_expr(scrutinee);
                for arm in arms {
                    self.lower_expr(&mut arm.body);
                }
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.lower_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.lower_expr(receiver);
                for arg in args {
                    self.lower_expr(arg);
                }
            }
            TirExprKind::IndirectCall { callee, args, .. } => {
                self.lower_expr(callee);
                for arg in args {
                    self.lower_expr(arg);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.lower_expr(left);
                self.lower_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.lower_expr(inner);
            }
            TirExprKind::Assign { target, value } => {
                self.lower_expr(target);
                self.lower_expr(value);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.lower_expr(inner);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.lower_expr(inner);
            }
            TirExprKind::Index { expr: inner, index } => {
                self.lower_expr(inner);
                self.lower_expr(index);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.lower_expr(&mut field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                for elem in elements {
                    self.lower_expr(elem);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.lower_closure_body(body);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.lower_expr(p);
                }
            }
            TirExprKind::OptionSome { value } => {
                self.lower_expr(value);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.lower_expr(value);
            }
            TirExprKind::Move { value } => {
                self.lower_expr(value);
            }
            // Leaf expressions - no recursion needed
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::ClosureToCanonical { .. } => {}
        }
    }

    fn lower_closure_body(&mut self, body: &mut TirExpr) {
        // Closure body is a TirExpr, not a TirBlock
        self.lower_expr(body);
    }

    /// Transform unlabeled Break statements to labeled Break (for For loop bodies)
    /// This only transforms Break at the current loop level, not nested loops
    fn transform_break_to_labeled(&self, stmts: &mut [TirStmt], label: &str) {
        for stmt in stmts {
            self.transform_break_in_stmt(stmt, label);
        }
    }

    fn transform_break_in_stmt(&self, stmt: &mut TirStmt, label: &str) {
        match &mut stmt.kind {
            TirStmtKind::Break {
                label: break_label,
                value: _,
            } => {
                // Only transform unlabeled breaks
                if break_label.is_none() {
                    *break_label = Some(label.to_string());
                }
            }
            // Stop at nested loops - don't transform their breaks
            TirStmtKind::Loop { .. }
            | TirStmtKind::While { .. }
            | TirStmtKind::For { .. }
            | TirStmtKind::ForOf { .. }
            | TirStmtKind::WhilePattern { .. }
            | TirStmtKind::ForPattern { .. } => {}
            // Recurse into blocks
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.transform_break_to_labeled(&mut then_block.stmts, label);
                if let Some(else_b) = else_block {
                    self.transform_break_to_labeled(&mut else_b.stmts, label);
                }
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                self.transform_break_to_labeled(&mut then_block.stmts, label);
                if let Some(else_b) = else_block {
                    self.transform_break_to_labeled(&mut else_b.stmts, label);
                }
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                self.transform_break_to_labeled(&mut block.stmts, label);
            }
            TirStmtKind::Expr(expr) => {
                self.transform_break_in_expr(expr, label);
            }
            // Other statements don't contain breaks
            TirStmtKind::Let { .. }
            | TirStmtKind::LetPattern { .. }
            | TirStmtKind::Return { .. }
            | TirStmtKind::Continue => {}
        }
    }

    fn transform_break_in_expr(&self, expr: &mut TirExpr, label: &str) {
        match &mut expr.kind {
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.transform_break_to_labeled(&mut block.stmts, label);
            }
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.transform_break_to_labeled(&mut then_branch.stmts, label);
                if let Some(else_b) = else_branch {
                    self.transform_break_to_labeled(&mut else_b.stmts, label);
                }
            }
            // Don't recurse into closures - they have their own scope
            TirExprKind::Closure { .. } => {}
            // Other expressions don't contain statements
            _ => {}
        }
    }

    /// Transform Continue statements to Break with label (for For loop bodies)
    /// This only transforms Continue at the current loop level, not nested loops
    fn transform_continue_to_break(&self, stmts: &mut [TirStmt], label: &str) {
        for stmt in stmts {
            self.transform_continue_in_stmt(stmt, label);
        }
    }

    fn transform_continue_in_stmt(&self, stmt: &mut TirStmt, label: &str) {
        match &mut stmt.kind {
            TirStmtKind::Continue => {
                // Transform to: break label;
                stmt.kind = TirStmtKind::Break {
                    label: Some(label.to_string()),
                    value: None,
                };
            }
            // Stop at nested loops - don't transform their continues
            TirStmtKind::Loop { .. }
            | TirStmtKind::While { .. }
            | TirStmtKind::For { .. }
            | TirStmtKind::ForOf { .. }
            | TirStmtKind::WhilePattern { .. }
            | TirStmtKind::ForPattern { .. } => {}
            // Recurse into blocks
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.transform_continue_to_break(&mut then_block.stmts, label);
                if let Some(else_b) = else_block {
                    self.transform_continue_to_break(&mut else_b.stmts, label);
                }
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                self.transform_continue_to_break(&mut then_block.stmts, label);
                if let Some(else_b) = else_block {
                    self.transform_continue_to_break(&mut else_b.stmts, label);
                }
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                self.transform_continue_to_break(&mut block.stmts, label);
            }
            TirStmtKind::Expr(expr) => {
                self.transform_continue_in_expr(expr, label);
            }
            // Other statements don't contain continues
            TirStmtKind::Let { .. }
            | TirStmtKind::LetPattern { .. }
            | TirStmtKind::Return { .. }
            | TirStmtKind::Break { .. } => {}
        }
    }

    fn transform_continue_in_expr(&self, expr: &mut TirExpr, label: &str) {
        match &mut expr.kind {
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.transform_continue_to_break(&mut block.stmts, label);
            }
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.transform_continue_to_break(&mut then_branch.stmts, label);
                if let Some(else_b) = else_branch {
                    self.transform_continue_to_break(&mut else_b.stmts, label);
                }
            }
            // Don't recurse into closures - they have their own scope
            TirExprKind::Closure { .. } => {}
            // Other expressions don't contain statements
            _ => {}
        }
    }
}
