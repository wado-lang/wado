//! Pattern matching translation — lowers `match`, `if let`, `while let`,
//! `matches`, and `switch` expressions to WIR control flow, including
//! variant construction, variant testing, and payload extraction.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::module_source::ModuleSource;
use crate::nir::{NirBlock, NirExpr, NirLiteralPattern, NirMatchArm, NirPattern};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::wir::{WirInstr, WirType};

use super::translate::{FunctionTranslator, LabelEntry};

/// Case enumeration for a variant or enum scrutinee, used to check whether a
/// set of match arms exhaustively covers every case.
struct CaseIndexer {
    /// Case names in declaration order — present for variants so `Variant`
    /// patterns can be mapped to indices by name. `None` for enums, which
    /// carry their case index directly on the pattern.
    names: Option<Vec<String>>,
    /// Total number of cases in the scrutinee type.
    total: usize,
}

impl CaseIndexer {
    fn by_name(&self, case_name: &str) -> Option<usize> {
        self.names.as_ref()?.iter().position(|n| n == case_name)
    }
}

impl FunctionTranslator<'_, '_> {
    /// Translate switch expression using `br_table`.
    pub(super) fn translate_switch(
        &mut self,
        scrutinee: &NirExpr,
        min_value: i64,
        arms: &[NirBlock],
        default: &NirBlock,
        result_type: TypeId,
    ) -> WirInstr {
        let has_result = result_type != TypeTable::UNIT && result_type != TypeTable::NEVER;
        let result_wir_type = if has_result {
            Some(self.ctx.type_id_to_wir_type(self.type_table, result_type))
        } else {
            None
        };

        // Translate scrutinee and adjust for min_value
        let scrut = self.translate_expr(scrutinee);
        let is_i64 = matches!(
            self.type_table.get(scrutinee.type_id),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );

        let adjusted = if min_value != 0 {
            if is_i64 {
                WirInstr::I32WrapI64(Box::new(WirInstr::I64Sub(
                    Box::new(scrut),
                    Box::new(WirInstr::I64Const(min_value)),
                )))
            } else {
                WirInstr::I32Sub(
                    Box::new(scrut),
                    Box::new(WirInstr::I32Const(min_value as i32)),
                )
            }
        } else if is_i64 {
            WirInstr::I32WrapI64(Box::new(scrut))
        } else {
            scrut
        };

        let num_arms = arms.len();

        // br_table targets: target[i] = i + 1 (depth to arm[i]'s wrapper block)
        // Block nesting (innermost to outermost): default, arm[0], arm[1], ..., arm[n-1], result
        // From br_table: depth 0 = default block, depth i+1 = arm[i]'s block
        let targets: Vec<u32> = (1..=num_arms as u32).collect();
        let default_target = 0u32; // Default block is innermost

        let br_table = WirInstr::BrTable {
            index: Box::new(adjusted),
            targets,
            default: default_target,
        };

        // The br_table switch generates wrapper blocks around each arm body.
        // arm[i]'s body ends up inside (num_arms - i) wrapper blocks, and the
        // default body inside (num_arms + 1) blocks. We must push dummy label
        // entries so that break/continue inside arm bodies compute correct br
        // depths.

        // Translate default body (wrapped in num_arms + 1 blocks)
        let default_block_count = num_arms + 1;
        for _ in 0..default_block_count {
            self.label_stack.push(LabelEntry {
                label: None,
                is_loop_break: false,
                is_loop_continue: false,
            });
        }
        let default_body = if has_result {
            self.translate_stmts_as_value(&default.stmts)
        } else {
            self.translate_stmts(&default.stmts)
        };
        for _ in 0..default_block_count {
            self.label_stack.pop();
        }

        // Translate arm bodies (arm[i] wrapped in num_arms - i blocks)
        let arm_bodies: Vec<Vec<WirInstr>> = arms
            .iter()
            .enumerate()
            .map(|(i, arm)| {
                let block_count = num_arms - i;
                for _ in 0..block_count {
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                }
                let body = if has_result {
                    self.translate_stmts_as_value(&arm.stmts)
                } else {
                    self.translate_stmts(&arm.stmts)
                };
                for _ in 0..block_count {
                    self.label_stack.pop();
                }
                body
            })
            .collect();

        // Build from innermost out:
        // block $default { br_table }; default_body; br N
        let mut current = vec![WirInstr::Block {
            label: None,
            result: None,
            body: vec![br_table],
        }];
        current.extend(default_body);
        if num_arms > 0 {
            current.push(WirInstr::Br {
                depth: num_arms as u32,
            });
        }

        // For each arm, wrap in a block
        for (i, arm_body) in arm_bodies.into_iter().enumerate() {
            let mut next = vec![WirInstr::Block {
                label: None,
                result: None,
                body: current,
            }];
            next.extend(arm_body);
            let remaining = num_arms - 1 - i;
            if remaining > 0 {
                next.push(WirInstr::Br {
                    depth: remaining as u32,
                });
            }
            current = next;
        }

        // Outer result block
        WirInstr::Block {
            label: None,
            result: result_wir_type,
            body: current,
        }
    }

    /// Translate a `LetDestructure` statement.
    ///
    /// By the time WIR build runs, pattern lowering
    /// ([`crate::lower::translate::pattern::lower`]) has rewritten every
    /// `LetDestructure` form *except* the multivalue-builtin tuple
    /// shape (a tuple whose RHS is a builtin call producing multiple
    /// scalar return values) into plain `Let` / `Expr` statements.
    /// So the only variant that reaches this translator is:
    ///
    /// * `Tuple` — multivalue-builtin call returning a tuple; destructure
    ///   each element into its `Binding` slot or skip `Wildcard` slots.
    ///
    /// Other variants are unreachable; we still match the obvious
    /// `Binding` case (single multivalue result) defensively and leave a
    /// catch-all `None` arm rather than `unreachable!` so a future
    /// lower-pass change cannot turn into a hard ICE.
    pub(super) fn translate_let_pattern(
        &mut self,
        pattern: &NirPattern,
        value: &NirExpr,
    ) -> Option<WirInstr> {
        let value_instr = self.translate_expr(value);

        match pattern {
            NirPattern::Tuple(patterns, _) => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, value.type_id);
                if let WirType::Ref { ref type_id, .. } = wir_type {
                    let mut instrs = Vec::new();

                    // Declare and assign a temp local for the tuple
                    let temp_name = format!("__let_pattern_{}", self.match_counter);
                    self.match_counter += 1;
                    instrs.push(WirInstr::DeclareLocal {
                        name: temp_name.clone(),
                        ty: wir_type.clone(),
                    });
                    instrs.push(WirInstr::LocalSet {
                        name: temp_name.clone(),
                        value: Box::new(value_instr),
                    });

                    // Bind each element
                    for (i, sub_pattern) in patterns.iter().enumerate() {
                        if let NirPattern::Binding { local_index, .. } = sub_pattern {
                            let local_name = self.local_name(*local_index);
                            let field_name_str = format!("{i}");
                            let field_result_ty =
                                self.struct_field_wir_type(type_id, &field_name_str);
                            instrs.push(WirInstr::LocalSet {
                                name: local_name,
                                value: Box::new(WirInstr::StructGet {
                                    type_id: type_id.clone(),
                                    field_name: field_name_str,
                                    expr: Box::new(WirInstr::LocalGet {
                                        name: temp_name.clone(),
                                        result_ty: wir_type.clone(),
                                    }),
                                    result_ty: field_result_ty,
                                }),
                            });
                        }
                        // Wildcard patterns: skip (no binding)
                    }

                    Some(WirInstr::Seq(instrs))
                } else {
                    // Non-ref tuple type — shouldn't happen for real tuples
                    None
                }
            }
            NirPattern::Binding { local_index, .. } => {
                // Simple binding (not a tuple destructure)
                let local_name = self.local_name(*local_index);
                Some(WirInstr::LocalSet {
                    name: local_name,
                    value: Box::new(value_instr),
                })
            }
            _ => None,
        }
    }

    /// Translate match expression as nested if-else chain.
    pub(super) fn translate_match(
        &mut self,
        scrutinee: &NirExpr,
        arms: &[NirMatchArm],
        result_type: TypeId,
    ) -> WirInstr {
        let has_result = result_type != TypeTable::UNIT && result_type != TypeTable::NEVER;
        let result_wir_type = if has_result {
            Some(self.ctx.type_id_to_wir_type(self.type_table, result_type))
        } else {
            None
        };

        // Store scrutinee in a local to avoid re-evaluation
        let scrut = self.translate_expr(scrutinee);
        let match_id = self.match_counter;
        self.match_counter += 1;
        let scrut_local_name = format!("__match_scrut_{match_id}");
        let scrut_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, scrutinee.type_id);

        // Precompute, per source-order arm, whether it will be lowered as
        // irrefutable (body only, no surrounding `If`). Both the `if_depths`
        // depth counter below and the emission loop that follows consume this
        // slice, so the two views cannot disagree.
        let emitted_as_irrefutable = self.compute_emitted_as_irrefutable(scrutinee.type_id, arms);

        // Build the if-else chain from inside out (last arm first)
        let mut result = WirInstr::Unreachable; // fallback: non-exhaustive

        // Pre-compute the wasm If nesting depth for each arm body.
        // Each non-irrefutable arm generates a WirInstr::If (guarded arms generate 2).
        // The arm at source index s will be nested inside all Ifs from arms 0..s,
        // so we need to push dummy label entries to make break/continue compute
        // correct br depths.
        let mut if_depths = Vec::with_capacity(arms.len());
        {
            let mut depth = 0u32;
            for (idx, arm) in arms.iter().enumerate() {
                // When the pattern is trivially true (Binding/Wildcard) and a guard
                // is present, we fold into a single If instead of nested 2-level If,
                // so only count 1 depth instead of 2.
                let pattern_trivially_true = matches!(
                    arm.pattern,
                    NirPattern::Wildcard | NirPattern::Binding { .. }
                );
                if !emitted_as_irrefutable[idx] {
                    depth += 1;
                    if arm.guard.is_some() && !pattern_trivially_true {
                        depth += 1; // guarded arms with non-trivial pattern create an extra inner If
                    }
                }
                if_depths.push(depth);
            }
        }

        for (reverse_idx, arm) in arms.iter().rev().enumerate() {
            let source_idx = arms.len() - 1 - reverse_idx;
            let if_nesting = if_depths[source_idx];

            // Translate the body in two parts: the binding-emission instrs
            // (which set up local slots referenced by the body and/or the
            // guard) and the body-proper instr. Keeping them separate lets
            // the guarded branches place each binding write at exactly one
            // point — either in the condition `Seq` (so the guard can read
            // it) or in the inner-if's `then_body`, never both. Emitting the
            // bindings unconditionally inside both sites would leave a
            // visibly redundant `_n = i; if guard { _n = i; … }` shape in
            // the lowered output that no later pass cleans up: write-only
            // local elimination only removes locals that are *never* read,
            // not locals that get overwritten by a duplicate store.
            let mut bindings = Vec::new();
            let body = {
                for _ in 0..if_nesting {
                    self.label_stack.push(LabelEntry {
                        label: None,
                        is_loop_break: false,
                        is_loop_continue: false,
                    });
                }
                self.emit_pattern_bindings(
                    &arm.pattern,
                    &scrut_local_name,
                    scrutinee.type_id,
                    &mut bindings,
                );
                let body = if has_result {
                    self.translate_expr_as_value(&arm.body)
                } else {
                    let instr = self.translate_expr(&arm.body);
                    // If the arm body produces a non-unit value (e.g. after inlining
                    // transforms a Block into a bare call), drop it to avoid leaving
                    // values on the Wasm stack. Guard with `produces_stack_value()` to
                    // avoid emitting `drop` after instructions that produce no value
                    // (e.g. `Block{result: None}` from LabeledBlock fusion).
                    if arm.body.type_id != TypeTable::UNIT
                        && arm.body.type_id != TypeTable::NEVER
                        && instr.produces_stack_value()
                    {
                        WirInstr::Drop(Box::new(instr))
                    } else {
                        instr
                    }
                };
                // Note: `translate_expr` already appends `unreachable` for
                // `never`-typed arm bodies, so no extra push is needed here.
                for _ in 0..if_nesting {
                    self.label_stack.pop();
                }
                body
            };
            let body_instrs: Vec<WirInstr> = bindings
                .iter()
                .cloned()
                .chain(std::iter::once(body.clone()))
                .collect();

            let condition = self.translate_pattern_condition(
                &arm.pattern,
                &scrut_local_name,
                scrutinee.type_id,
            );

            // `emitted_as_irrefutable[source_idx]` already folds in both the
            // base irrefutable patterns (wildcard/binding/struct/tuple) and the
            // last-arm-of-exhaustive-match case, so the depth counter above
            // and this check stay in lockstep.
            let is_irrefutable = emitted_as_irrefutable[source_idx];

            if is_irrefutable && arm.guard.is_none() {
                // This arm always matches — it becomes the fallback
                if body_instrs.len() == 1 {
                    result = body_instrs.into_iter().next().unwrap();
                } else {
                    result = WirInstr::Seq(body_instrs);
                }
            } else if let Some(guard) = &arm.guard {
                // Guard present: use nested if to avoid eager evaluation.
                // Outer if checks the pattern condition; inner if checks the guard.
                // This prevents pattern bindings (ref.cast etc.) from executing
                // when the pattern doesn't match.
                //
                // Optimization: when the pattern condition is trivially true (i.e., the
                // pattern is irrefutable like Binding/Wildcard), fold the guard into a
                // single If to avoid cloning `result` (which causes 2^N tree explosion
                // for many guarded arms, e.g., string match with N branches).
                let pattern_is_trivially_true = matches!(&condition, WirInstr::I32Const(1));
                if pattern_is_trivially_true {
                    // Pattern always matches — just use the guard as the sole condition.
                    // Emit pattern bindings before the guard expression so that bound
                    // variables (e.g., `__lit_N`) are available when evaluating the guard
                    // (e.g., `__lit_N.eq("str")`). Since the pattern is irrefutable
                    // (Binding/Wildcard), these bindings are safe to emit unconditionally.
                    // We embed bindings into the condition via Seq so that the If is the
                    // top-level instruction (required for value-producing match expressions).
                    let guard_expr = self.translate_expr(guard);
                    let condition_with_bindings = if bindings.is_empty() {
                        guard_expr
                    } else {
                        let mut seq = bindings.clone();
                        seq.push(guard_expr);
                        WirInstr::Seq(seq)
                    };
                    // Bindings already live in the condition `Seq`, so the
                    // arm body should not re-emit them. Use `body` alone.
                    result = WirInstr::If {
                        condition: Box::new(condition_with_bindings),
                        result: result_wir_type.clone(),
                        then_body: vec![body.clone()],
                        else_body: Some(vec![result]),
                    };
                } else {
                    let mut inner_then = bindings.clone();
                    let guard_expr = self.translate_expr(guard);
                    // Bindings run once, before the guard, in the
                    // `inner_then` prefix below — the guard may reference
                    // them. The inner-if's `then_body` is the arm body
                    // alone; re-emitting bindings inside it would produce
                    // duplicate `_n = i; if guard { _n = i; … }` writes
                    // that no later pass cleans up.
                    let inner_if = WirInstr::If {
                        condition: Box::new(guard_expr),
                        result: result_wir_type.clone(),
                        then_body: vec![body.clone()],
                        else_body: Some(vec![result.clone()]),
                    };
                    inner_then.push(inner_if);
                    // Outer if: check pattern condition
                    result = WirInstr::If {
                        condition: Box::new(condition),
                        result: result_wir_type.clone(),
                        then_body: inner_then,
                        else_body: Some(vec![result]),
                    };
                }
            } else {
                let then_body = body_instrs;
                let else_body = Some(vec![result]);
                result = WirInstr::If {
                    condition: Box::new(condition),
                    result: result_wir_type.clone(),
                    then_body,
                    else_body,
                };
            }
        }

        // Wrap everything: declare local, set, then the if-else chain
        WirInstr::Seq(vec![
            WirInstr::DeclareLocal {
                name: scrut_local_name.clone(),
                ty: scrut_wir_type,
            },
            WirInstr::LocalSet {
                name: scrut_local_name,
                value: Box::new(scrut),
            },
            result,
        ])
    }

    /// Returns, per source-order arm, whether that arm will be lowered as
    /// irrefutable — i.e. emitted as just its body (with pattern bindings)
    /// and no surrounding `If` test.
    ///
    /// Two sources:
    /// * The pattern itself is always true (wildcard / binding / struct /
    ///   tuple) and the arm has no guard.
    /// * The arm is the LAST arm of an exhaustive variant-or-enum match,
    ///   every earlier arm has failed by the time control reaches it, and
    ///   the arm has no guard. The pattern test and the trailing
    ///   `unreachable` fallback are both dead in that case.
    ///
    /// The caller uses the resulting `Vec<bool>` both for depth accounting
    /// and for emission, so the two stages cannot drift.
    fn compute_emitted_as_irrefutable(
        &self,
        scrut_type: TypeId,
        arms: &[NirMatchArm],
    ) -> Vec<bool> {
        let mut out: Vec<bool> = arms
            .iter()
            .map(|arm| {
                matches!(
                    arm.pattern,
                    NirPattern::Wildcard
                        | NirPattern::Binding { .. }
                        | NirPattern::Struct { .. }
                        | NirPattern::Tuple(_, _)
                ) && arm.guard.is_none()
            })
            .collect();
        if let Some(last_idx) = arms.len().checked_sub(1)
            && !out[last_idx]
            && arms[last_idx].guard.is_none()
            && self.match_is_exhaustive(scrut_type, arms)
        {
            out[last_idx] = true;
        }
        out
    }

    /// Returns true when `arms` exhaustively cover every case of the
    /// scrutinee's variant or enum type using distinct, unguarded patterns.
    ///
    /// Accepts `NirPattern::Variant`, `NirPattern::Enum`, and one level of
    /// `NirPattern::Or` whose alternatives are themselves `Variant`/`Enum`
    /// patterns. Anything else (wildcards, literals, ranges, guards, nested
    /// Ors with non-Variant/Enum alternatives) bails out.
    fn match_is_exhaustive(&self, scrut_type: TypeId, arms: &[NirMatchArm]) -> bool {
        if arms.is_empty() {
            return false;
        }
        if arms.iter().any(|a| a.guard.is_some()) {
            return false;
        }

        let Some(index_of) = self.case_indexer(scrut_type) else {
            return false;
        };
        let total_cases = index_of.total;

        let mut seen = vec![false; total_cases];
        let mut covered = 0usize;
        for arm in arms {
            if !self.arm_pattern_covers_cases(&arm.pattern, &index_of, &mut seen, &mut covered) {
                return false;
            }
        }
        covered == total_cases
    }

    /// Walk a single arm's pattern (possibly an `Or`) and mark every case it
    /// covers in `seen`. Returns `false` if the arm touches any case already
    /// covered by a previous arm/alternative, or if it contains a pattern
    /// shape outside the supported set.
    fn arm_pattern_covers_cases(
        &self,
        pattern: &NirPattern,
        index_of: &CaseIndexer,
        seen: &mut [bool],
        covered: &mut usize,
    ) -> bool {
        match pattern {
            NirPattern::Variant { variant_name, .. } => {
                let Some(i) = index_of.by_name(variant_name) else {
                    return false;
                };
                self.record_case(i, seen, covered)
            }
            NirPattern::Enum { case_index, .. } => {
                self.record_case(*case_index as usize, seen, covered)
            }
            NirPattern::Or(alts) => alts
                .iter()
                .all(|alt| self.arm_pattern_covers_cases(alt, index_of, seen, covered)),
            _ => false,
        }
    }

    fn record_case(&self, i: usize, seen: &mut [bool], covered: &mut usize) -> bool {
        if i >= seen.len() || seen[i] {
            return false;
        }
        seen[i] = true;
        *covered += 1;
        true
    }

    /// Resolve the scrutinee type to a list of case names (variant) or a bare
    /// count (enum). Returns `None` for any type that isn't a concrete variant
    /// or enum the compiler can enumerate here.
    fn case_indexer(&self, scrut_type: TypeId) -> Option<CaseIndexer> {
        match self.type_table.get(scrut_type) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => self.variant_case_indexer(name, module_source),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_arg_for_generic(*t))
                    .collect();
                let mangled = crate::name::mangle_generic_name(name, &type_arg_names);
                self.variant_case_indexer(&mangled, module_source)
            }
            ResolvedType::Enum {
                name,
                module_source,
            } => self
                .ctx
                .package
                .enums
                .iter()
                .find(|e| e.name == *name && e.module_source == *module_source)
                .map(|e| CaseIndexer {
                    names: None,
                    total: e.cases.len(),
                }),
            _ => None,
        }
    }

    fn variant_case_indexer(
        &self,
        variant_name: &str,
        module_source: &ModuleSource,
    ) -> Option<CaseIndexer> {
        let fq = format!("{module_source}//{variant_name}");
        let variant_type_id = self.ctx.type_map.get(&fq)?;
        let crate::wir::WirTypeDef::Variant(vt) = &self.ctx.types[variant_type_id.index() as usize]
        else {
            return None;
        };
        let names: Vec<String> = vt.cases.iter().map(|c| c.name.clone()).collect();
        let total = names.len();
        Some(CaseIndexer {
            names: Some(names),
            total,
        })
    }

    /// Generate a condition expression for a pattern.
    /// Returns an i32 (0 or 1) indicating whether the pattern matches.
    fn translate_pattern_condition(
        &self,
        pattern: &NirPattern,
        scrut_local: &str,
        scrut_type: TypeId,
    ) -> WirInstr {
        match pattern {
            NirPattern::Wildcard | NirPattern::Binding { .. } => {
                WirInstr::I32Const(1) // always matches
            }
            NirPattern::Literal(lit) => {
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
                };
                self.translate_literal_pattern_condition(lit, scrut_get, scrut_type)
            }
            NirPattern::Enum { case_index, .. } => {
                // Enum: compare i32 discriminant
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
                };
                WirInstr::I32Eq(
                    Box::new(scrut_get),
                    Box::new(WirInstr::I32Const(*case_index as i32)),
                )
            }
            NirPattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                // For variant patterns, use ref.test on the case type
                // or discriminant comparison for unit cases
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
                };

                // Look up variant type info to find the case WirTypeId
                let (var_name, var_module) = match self.type_table.get(scrut_type) {
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    } => (name.clone(), module_source.clone()),
                    ResolvedType::GenericInstance {
                        name,
                        module_source,
                        type_args,
                        ..
                    } => {
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| self.type_table.mangle_type_arg_for_generic(*t))
                            .collect();
                        (
                            crate::name::mangle_generic_name(name, &type_arg_names),
                            module_source.clone(),
                        )
                    }
                    _ => return WirInstr::I32Const(0),
                };
                let fq = format!("{var_module}//{var_name}");

                // Look up variant case info
                if let Some(variant_type_id) = self.ctx.type_map.get(&fq) {
                    // Find the case by name
                    if let crate::wir::WirTypeDef::Variant(vt) =
                        &self.ctx.types[variant_type_id.index() as usize]
                    {
                        if let Some(case) = vt.cases.iter().find(|c| c.name == *variant_name) {
                            if case.payload.is_empty() && bindings.is_empty() {
                                // Unit variant: check discriminant
                                let wir_type =
                                    self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                                if let WirType::Ref { type_id, .. } = wir_type {
                                    WirInstr::I32Eq(
                                        Box::new(WirInstr::StructGet {
                                            type_id,
                                            field_name: "discriminant".to_string(),
                                            expr: Box::new(scrut_get),
                                            result_ty: WirType::I32,
                                        }),
                                        Box::new(WirInstr::I32Const(case.index as i32)),
                                    )
                                } else {
                                    WirInstr::I32Const(0)
                                }
                            } else {
                                // Payload variant: use ref.test on case subtype
                                let case_fq = format!("{fq}::{variant_name}");
                                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq) {
                                    WirInstr::RefTest {
                                        type_id: case_type_id.clone(),
                                        nullable: false,
                                        expr: Box::new(scrut_get),
                                    }
                                } else {
                                    // Case type not found (unit payload): fall back to discriminant check
                                    let wir_type =
                                        self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                                    if let WirType::Ref { type_id, .. } = wir_type {
                                        WirInstr::I32Eq(
                                            Box::new(WirInstr::StructGet {
                                                type_id,
                                                field_name: "discriminant".to_string(),
                                                expr: Box::new(scrut_get),
                                                result_ty: WirType::I32,
                                            }),
                                            Box::new(WirInstr::I32Const(case.index as i32)),
                                        )
                                    } else {
                                        WirInstr::I32Const(0)
                                    }
                                }
                            }
                        } else {
                            WirInstr::I32Const(0)
                        }
                    } else {
                        WirInstr::I32Const(0)
                    }
                } else {
                    WirInstr::I32Const(0)
                }
            }
            NirPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => {
                let scrut_get = || WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
                };
                let is_i64 = matches!(
                    self.type_table.get(scrut_type),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                if is_i64 {
                    let start_const = WirInstr::I64Const(*start as i64);
                    let end_const = WirInstr::I64Const(*end as i64);
                    let ge = if *is_unsigned {
                        WirInstr::I64GeU(Box::new(scrut_get()), Box::new(start_const))
                    } else {
                        WirInstr::I64GeS(Box::new(scrut_get()), Box::new(start_const))
                    };
                    let upper = if *inclusive {
                        if *is_unsigned {
                            WirInstr::I64LeU(Box::new(scrut_get()), Box::new(end_const))
                        } else {
                            WirInstr::I64LeS(Box::new(scrut_get()), Box::new(end_const))
                        }
                    } else if *is_unsigned {
                        WirInstr::I64LtU(Box::new(scrut_get()), Box::new(end_const))
                    } else {
                        WirInstr::I64LtS(Box::new(scrut_get()), Box::new(end_const))
                    };
                    WirInstr::I32And(Box::new(ge), Box::new(upper))
                } else {
                    let start_const = WirInstr::I32Const(*start as i32);
                    let end_const = WirInstr::I32Const(*end as i32);
                    let ge = if *is_unsigned {
                        WirInstr::I32GeU(Box::new(scrut_get()), Box::new(start_const))
                    } else {
                        WirInstr::I32GeS(Box::new(scrut_get()), Box::new(start_const))
                    };
                    let upper = if *inclusive {
                        if *is_unsigned {
                            WirInstr::I32LeU(Box::new(scrut_get()), Box::new(end_const))
                        } else {
                            WirInstr::I32LeS(Box::new(scrut_get()), Box::new(end_const))
                        }
                    } else if *is_unsigned {
                        WirInstr::I32LtU(Box::new(scrut_get()), Box::new(end_const))
                    } else {
                        WirInstr::I32LtS(Box::new(scrut_get()), Box::new(end_const))
                    };
                    WirInstr::I32And(Box::new(ge), Box::new(upper))
                }
            }
            NirPattern::Tuple(_, _) | NirPattern::Struct { .. } => {
                // Tuple/struct patterns: always irrefutable
                WirInstr::I32Const(1)
            }
            NirPattern::Or(alternatives) => {
                // Or pattern: combine conditions with logical OR
                let mut result = WirInstr::I32Const(0);
                for alt in alternatives {
                    let cond = self.translate_pattern_condition(alt, scrut_local, scrut_type);
                    result = WirInstr::I32Or(Box::new(result), Box::new(cond));
                }
                result
            }
            NirPattern::ConstantValue { .. } => {
                panic!(
                    "ConstantValue pattern should have been lowered to binding + guard before WIR translation"
                );
            }
        }
    }

    /// Generate a condition for a literal pattern.
    fn translate_literal_pattern_condition(
        &self,
        lit: &NirLiteralPattern,
        scrut_get: WirInstr,
        scrut_type: TypeId,
    ) -> WirInstr {
        let is_i64 = matches!(
            self.type_table.get(scrut_type),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        match lit {
            NirLiteralPattern::I128(val) => {
                if is_i64 {
                    WirInstr::I64Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I64Const(*val as i64)),
                    )
                } else {
                    WirInstr::I32Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I32Const(*val as i32)),
                    )
                }
            }
            NirLiteralPattern::U128(val) => {
                if is_i64 {
                    WirInstr::I64Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I64Const(*val as i64)),
                    )
                } else {
                    WirInstr::I32Eq(
                        Box::new(scrut_get),
                        Box::new(WirInstr::I32Const(*val as i32)),
                    )
                }
            }
            NirLiteralPattern::Bool(val) => WirInstr::I32Eq(
                Box::new(scrut_get),
                Box::new(WirInstr::I32Const(i32::from(*val))),
            ),
            NirLiteralPattern::Char(val) => WirInstr::I32Eq(
                Box::new(scrut_get),
                Box::new(WirInstr::I32Const(*val as i32)),
            ),
            NirLiteralPattern::String(_) | NirLiteralPattern::Null => {
                // String/null patterns: use ref.eq or ref.is_null
                if matches!(lit, NirLiteralPattern::Null) {
                    WirInstr::RefIsNull(Box::new(scrut_get))
                } else {
                    panic!("string literal patterns should be lowered before WIR translation")
                }
            }
        }
    }

    /// Emit pattern bindings (local.set for bound variables).
    ///
    /// For or-patterns, conditionally extracts bindings from whichever alternative matched.
    fn emit_pattern_bindings(
        &mut self,
        pattern: &NirPattern,
        scrut_local: &str,
        scrut_type: TypeId,
        instrs: &mut Vec<WirInstr>,
    ) {
        match pattern {
            NirPattern::Binding { local_index, .. } => {
                instrs.push(WirInstr::LocalSet {
                    name: self.local_name(*local_index),
                    value: Box::new(WirInstr::LocalGet {
                        name: scrut_local.to_string(),
                        result_ty: self.wir_type(scrut_type),
                    }),
                });
            }
            NirPattern::Variant {
                variant_name,
                bindings,
                enum_type,
                payload_type,
            } => {
                if bindings.is_empty() {
                    return;
                }

                // Look up the variant type to find the case WirTypeId
                let (var_name, var_module) = match self.type_table.get(*enum_type) {
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    } => (name.clone(), module_source.clone()),
                    ResolvedType::GenericInstance {
                        name,
                        module_source,
                        type_args,
                        ..
                    } => {
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| self.type_table.mangle_type_arg_for_generic(*t))
                            .collect();
                        (
                            crate::name::mangle_generic_name(name, &type_arg_names),
                            module_source.clone(),
                        )
                    }
                    _ => return,
                };
                let fq = format!("{var_module}//{var_name}");

                let case_fq = format!("{fq}::{variant_name}");

                // Try to get the case type for ref.cast + struct.get
                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq).cloned() {
                    // Count bindings that actually need the cast result. A
                    // `Wildcard` or literal-shaped sub-pattern doesn't read
                    // any payload field, so it doesn't consume the cast.
                    let consumers = bindings
                        .iter()
                        .filter(|b| {
                            !matches!(
                                b,
                                NirPattern::Wildcard
                                    | NirPattern::Literal(_)
                                    | NirPattern::Enum { .. }
                                    | NirPattern::ConstantValue { .. }
                                    | NirPattern::Range { .. }
                            )
                        })
                        .count();
                    // For a single consumer, inline the `ref.cast` into the
                    // `struct.get`'s receiver: avoids the temp `__cast_N`
                    // local plus its `LocalSet` / `LocalGet` pair.
                    //
                    // For two or more consumers, keep the temp: each
                    // consumer would otherwise repeat the runtime `ref.cast`
                    // type check, which is more expensive than a
                    // `local.get`.
                    let cast_local = if consumers >= 2 {
                        self.local_counter += 1;
                        let cast_local = format!("__cast_{}", self.local_counter);
                        instrs.push(WirInstr::DeclareLocal {
                            name: cast_local.clone(),
                            ty: WirType::Ref {
                                type_id: case_type_id.clone(),
                                nullable: false,
                            },
                        });
                        instrs.push(WirInstr::LocalSet {
                            name: cast_local.clone(),
                            value: Box::new(WirInstr::RefCast {
                                type_id: case_type_id.clone(),
                                nullable: false,
                                expr: Box::new(WirInstr::LocalGet {
                                    name: scrut_local.to_string(),
                                    result_ty: self.wir_type(scrut_type),
                                }),
                            }),
                        });
                        Some(cast_local)
                    } else {
                        None
                    };
                    // Build the WIR instruction that produces the cast
                    // result for a `struct.get` receiver. `None` here means
                    // inline: a fresh `ref.cast` per use. Cloned at every
                    // use (only one use when `cast_local` is `None`).
                    let cast_source = |this: &Self| -> WirInstr {
                        if let Some(ref name) = cast_local {
                            WirInstr::LocalGet {
                                name: name.clone(),
                                result_ty: WirType::Ref {
                                    type_id: case_type_id.clone(),
                                    nullable: false,
                                },
                            }
                        } else {
                            WirInstr::RefCast {
                                type_id: case_type_id.clone(),
                                nullable: false,
                                expr: Box::new(WirInstr::LocalGet {
                                    name: scrut_local.to_string(),
                                    result_ty: this.wir_type(scrut_type),
                                }),
                            }
                        }
                    };
                    // Extract each payload binding via struct.get from the cast source
                    for (i, binding) in bindings.iter().enumerate() {
                        let payload_field_name = format!("payload_{i}");
                        let payload_result_ty =
                            self.struct_field_wir_type(&case_type_id, &payload_field_name);
                        let payload_get = WirInstr::StructGet {
                            type_id: case_type_id.clone(),
                            field_name: payload_field_name,
                            expr: Box::new(cast_source(self)),
                            result_ty: payload_result_ty,
                        };
                        if let NirPattern::Binding {
                            local_index,
                            type_id,
                            ..
                        } = binding
                        {
                            // Check the local's actual type (which may have been
                            // promoted to Box<T> by the address-taken boxing pass)
                            // rather than the pattern binding's original type_id.
                            let local_type_id =
                                if (*local_index as usize) < self.tir_func.locals.len() {
                                    self.tir_func.locals[*local_index as usize].type_id
                                } else {
                                    *type_id
                                };
                            let binding_wir =
                                self.ctx.type_id_to_wir_type(self.type_table, local_type_id);
                            let payload_field_wir =
                                self.get_case_payload_wir_type(&case_type_id, i);
                            self.emit_pattern_binding_set(
                                *local_index,
                                &binding_wir,
                                payload_field_wir.as_ref(),
                                payload_get,
                                instrs,
                            );
                        } else if !matches!(
                            binding,
                            NirPattern::Wildcard
                                | NirPattern::Literal(_)
                                | NirPattern::Enum { .. }
                                | NirPattern::ConstantValue { .. }
                                | NirPattern::Range { .. }
                        ) {
                            // Compound sub-pattern (Tuple, Struct, Variant, Or):
                            // extract the payload into a temp local and recurse into
                            // emit_pattern_bindings to handle arbitrarily nested
                            // destructuring.
                            let payload_tid = *payload_type;
                            let payload_wir =
                                self.ctx.type_id_to_wir_type(self.type_table, payload_tid);
                            self.local_counter += 1;
                            let temp_name = format!("__variant_payload_{}", self.local_counter);
                            instrs.push(WirInstr::DeclareLocal {
                                name: temp_name.clone(),
                                ty: payload_wir,
                            });
                            instrs.push(WirInstr::LocalSet {
                                name: temp_name.clone(),
                                value: Box::new(payload_get),
                            });
                            self.emit_pattern_bindings(binding, &temp_name, payload_tid, instrs);
                        }
                    }
                } else {
                    // Fallback: just copy the scrutinee (won't be type-correct for payload)
                    for binding in bindings {
                        if let NirPattern::Binding {
                            local_index,
                            type_id,
                            ..
                        } = binding
                        {
                            let wir = self.ctx.type_id_to_wir_type(self.type_table, *type_id);
                            // Skip unit-typed bindings (no Wasm local exists for unit)
                            if !matches!(wir, WirType::Unit) {
                                instrs.push(WirInstr::LocalSet {
                                    name: self.local_name(*local_index),
                                    value: Box::new(WirInstr::LocalGet {
                                        name: scrut_local.to_string(),
                                        result_ty: self.wir_type(scrut_type),
                                    }),
                                });
                            }
                        }
                    }
                }
            }
            NirPattern::Wildcard
            | NirPattern::Literal(_)
            | NirPattern::Enum { .. }
            | NirPattern::ConstantValue { .. }
            | NirPattern::Range { .. } => {
                // No bindings needed
            }
            NirPattern::Tuple(sub_patterns, _) => {
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                if let WirType::Ref { ref type_id, .. } = wir_type {
                    let element_types = self.type_table.as_tuple(scrut_type).unwrap_or_default();
                    for (i, sub_pattern) in sub_patterns.iter().enumerate() {
                        let field_name_str = format!("{i}");
                        let field_result_ty = self.struct_field_wir_type(type_id, &field_name_str);
                        let field_get = WirInstr::StructGet {
                            type_id: type_id.clone(),
                            field_name: field_name_str,
                            expr: Box::new(WirInstr::LocalGet {
                                name: scrut_local.to_string(),
                                result_ty: wir_type.clone(),
                            }),
                            result_ty: field_result_ty.clone(),
                        };
                        match sub_pattern {
                            NirPattern::Binding { local_index, .. } => {
                                let local_type_id =
                                    if (*local_index as usize) < self.tir_func.locals.len() {
                                        self.tir_func.locals[*local_index as usize].type_id
                                    } else {
                                        element_types.get(i).copied().unwrap_or(TypeTable::UNKNOWN)
                                    };
                                let binding_wir =
                                    self.ctx.type_id_to_wir_type(self.type_table, local_type_id);
                                self.emit_pattern_binding_set(
                                    *local_index,
                                    &binding_wir,
                                    Some(&field_result_ty),
                                    field_get,
                                    instrs,
                                );
                            }
                            NirPattern::Wildcard => {}
                            _ => {
                                // Nested pattern: store in temp and recurse
                                self.local_counter += 1;
                                let temp_name = format!("__tuple_elem_{}", self.local_counter);
                                let elem_type =
                                    element_types.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                                let elem_wir_type =
                                    self.ctx.type_id_to_wir_type(self.type_table, elem_type);
                                instrs.push(WirInstr::DeclareLocal {
                                    name: temp_name.clone(),
                                    ty: elem_wir_type,
                                });
                                instrs.push(WirInstr::LocalSet {
                                    name: temp_name.clone(),
                                    value: Box::new(field_get),
                                });
                                self.emit_pattern_bindings(
                                    sub_pattern,
                                    &temp_name,
                                    elem_type,
                                    instrs,
                                );
                            }
                        }
                    }
                }
            }
            NirPattern::Struct { fields, .. } => {
                // Emit field bindings for struct patterns in match arms
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, scrut_type);
                if let WirType::Ref { ref type_id, .. } = wir_type {
                    for field in fields {
                        let field_result_ty =
                            self.struct_field_wir_type(type_id, &field.field_name);
                        let field_get = WirInstr::StructGet {
                            type_id: type_id.clone(),
                            field_name: field.field_name.clone(),
                            expr: Box::new(WirInstr::LocalGet {
                                name: scrut_local.to_string(),
                                result_ty: wir_type.clone(),
                            }),
                            result_ty: field_result_ty,
                        };
                        match &field.pattern {
                            NirPattern::Binding { local_index, .. } => {
                                instrs.push(WirInstr::LocalSet {
                                    name: self.local_name(*local_index),
                                    value: Box::new(field_get),
                                });
                            }
                            NirPattern::Wildcard => {}
                            _ => {
                                // For nested patterns, store in a temp and recurse
                                self.local_counter += 1;
                                let temp_name = format!("__struct_field_{}", self.local_counter);
                                let field_type =
                                    self.resolve_struct_field_type(scrut_type, &field.field_name);
                                let field_wir_type =
                                    self.ctx.type_id_to_wir_type(self.type_table, field_type);
                                instrs.push(WirInstr::DeclareLocal {
                                    name: temp_name.clone(),
                                    ty: field_wir_type,
                                });
                                instrs.push(WirInstr::LocalSet {
                                    name: temp_name.clone(),
                                    value: Box::new(field_get),
                                });
                                self.emit_pattern_bindings(
                                    &field.pattern,
                                    &temp_name,
                                    field_type,
                                    instrs,
                                );
                            }
                        }
                    }
                }
            }
            NirPattern::Or(alternatives) => {
                // Or patterns: emit bindings for each alternative, guarded by its condition.
                // For alternatives with only wildcards (no real bindings), skip entirely.
                let has_any_bindings = alternatives.iter().any(pattern_has_bindings);
                if !has_any_bindings {
                    return;
                }
                // Emit conditional binding extraction: check each alternative and
                // emit bindings from the one that matches.
                // Build a nested if-else chain from the inside out.
                let mut result: Option<WirInstr> = None;
                for alt in alternatives.iter().rev() {
                    let cond = self.translate_pattern_condition(alt, scrut_local, scrut_type);
                    let mut body = Vec::new();
                    self.emit_pattern_bindings(alt, scrut_local, scrut_type, &mut body);
                    let else_body = result.map(|r| vec![r]);
                    result = Some(WirInstr::If {
                        condition: Box::new(cond),
                        result: None,
                        then_body: body,
                        else_body,
                    });
                }
                if let Some(if_instr) = result {
                    instrs.push(if_instr);
                }
            }
        }
    }

    /// Store the result of `source` into the local for a pattern
    /// `Binding`, wrapping or narrowing as needed so the `LocalSet` is
    /// well-typed under wasm GC's exact-type rules.
    ///
    /// Three coercions land here, applied in priority order:
    ///
    /// 1. **Box wrap.** When the binding's local is a `Ref` to a
    ///    different struct than the source produces (the
    ///    address-taken boxing pass promoted a `T` local to `Box<T>`,
    ///    or the elaborator typed a variant-generic site as
    ///    `Box<primitive>`), wrap the source in `StructNew { box_tid,
    ///    fields: [source] }` so the source value lands in the Box's
    ///    payload field. Also covers the primitive-into-Box case
    ///    (`i32` / `i64` / `f32` / `f64` / `v128`).
    /// 2. **Nullability narrow.** When the binding is `Ref { nullable:
    ///    false }` but the source produces `Ref { nullable: true }`
    ///    (e.g. variant `payload_0` declared nullable for the
    ///    `Option<&T> = &T | null` boxing optimisation), wrap with
    ///    `RefAsNonNull`.
    /// 3. **Unit binding.** Pattern bindings of unit type don't have a
    ///    Wasm local; skip the `LocalSet` entirely.
    ///
    /// `source_wir = None` means the source's WIR type isn't known
    /// (only `get_case_payload_wir_type`'s missing-struct fallback
    /// produces this today); in that case fall through to the raw
    /// `LocalSet`.
    fn emit_pattern_binding_set(
        &self,
        local_index: u32,
        binding_wir: &WirType,
        source_wir: Option<&WirType>,
        source: WirInstr,
        instrs: &mut Vec<WirInstr>,
    ) {
        let needs_boxing = match (binding_wir, source_wir) {
            (WirType::Ref { type_id: bt, .. }, Some(WirType::Ref { type_id: st, .. })) => bt != st,
            (WirType::Ref { .. }, Some(WirType::AbstractRef { .. })) => false,
            (WirType::Ref { .. }, Some(_)) => true,
            _ => false,
        };
        let value = if needs_boxing {
            if let WirType::Ref {
                type_id: box_tid, ..
            } = binding_wir
            {
                WirInstr::StructNew {
                    type_id: box_tid.clone(),
                    fields: vec![source],
                }
            } else {
                source
            }
        } else if matches!(
            binding_wir,
            WirType::Ref {
                nullable: false,
                ..
            }
        ) && matches!(source_wir, Some(WirType::Ref { nullable: true, .. }))
        {
            WirInstr::RefAsNonNull(Box::new(source))
        } else {
            source
        };
        if !matches!(binding_wir, WirType::Unit) {
            instrs.push(WirInstr::LocalSet {
                name: self.local_name(local_index),
                value: Box::new(value),
            });
        }
    }

    /// Look up a variant case struct's payload field type, extracting the inner
    /// ref type ID. For `payload_i` of a tuple type, this returns the `WirTypeId`
    /// of the tuple struct.
    fn get_case_payload_wir_type(
        &self,
        case_type_id: &crate::wir::WirTypeId,
        payload_index: usize,
    ) -> Option<WirType> {
        let type_def = self.ctx.types.get(case_type_id.index() as usize)?;
        if let crate::wir::WirTypeDef::Struct(s) = type_def {
            let field = s.fields.get(payload_index + 1)?;
            return Some(field.ty.clone());
        }
        None
    }

    /// Resolve the `TypeId` of a struct field by name.
    fn resolve_struct_field_type(&self, struct_type: TypeId, field_name: &str) -> TypeId {
        if let ResolvedType::Struct {
            name,
            module_source,
            ..
        } = self.type_table.get(struct_type)
        {
            for s in &self.ctx.package.structs {
                if s.module_source == *module_source && s.name == *name {
                    for f in &s.fields {
                        if f.name == field_name {
                            return f.type_id;
                        }
                    }
                }
            }
        }
        TypeTable::UNKNOWN
    }

    /// Translate variant construction: `Shape::Circle(5.0)`
    pub(super) fn translate_variant_construct(
        &mut self,
        variant_type: TypeId,
        case_index: u32,
        case_name: &str,
        payload: Option<&NirExpr>,
        result_type: TypeId,
    ) -> WirInstr {
        // Get the variant name and module source
        let (variant_name, variant_module_source) = match self.type_table.get(variant_type) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_arg_for_generic(*t))
                    .collect();
                (
                    crate::name::mangle_generic_name(name, &type_arg_names),
                    module_source.clone(),
                )
            }
            other => panic!(
                "[WIR] translate_variant_construct: expected Variant/GenericInstance, got {other:?} (variant_type={variant_type:?})"
            ),
        };

        let fq = format!("{variant_module_source}//{variant_name}");

        // Look up case-specific struct type
        let case_fq = format!("{fq}::{case_name}");

        if let Some(case_type_id) = self.ctx.type_map.get(&case_fq).cloned() {
            // Build struct.new for the case type: (tag, payload?)
            let mut fields = vec![WirInstr::I32Const(case_index as i32)];
            if let Some(payload_expr) = payload {
                fields.push(self.translate_expr(payload_expr));
            }
            self.struct_new(case_type_id, fields)
        } else {
            // Fallback: try the base variant type
            let wir_type = self.ctx.type_id_to_wir_type(self.type_table, result_type);
            let WirType::Ref { type_id, .. } = wir_type else {
                panic!(
                    "[WIR] translate_variant_construct: case type {case_fq} not registered, and result type {result_type:?} is not a Ref ({wir_type:?})"
                );
            };
            let mut fields = vec![WirInstr::I32Const(case_index as i32)];
            if let Some(payload_expr) = payload {
                fields.push(self.translate_expr(payload_expr));
            }
            self.struct_new(type_id, fields)
        }
    }

    /// Build a variant case value directly from WIR instructions (no TIR needed).
    ///
    /// Used by canonical method synthesis to construct `Option::Some/None` and similar
    /// variant values without going through TIR expression translation.
    pub(super) fn build_variant_case_wir(
        &self,
        variant_type_id: TypeId,
        case_index: u32,
        case_name: &str,
        payload: Option<WirInstr>,
    ) -> WirInstr {
        let (variant_name, variant_module_source) = match self.type_table.get(variant_type_id) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_arg_for_generic(*t))
                    .collect();
                (
                    crate::name::mangle_generic_name(name, &type_arg_names),
                    module_source.clone(),
                )
            }
            other => panic!(
                "[WIR] build_variant_case_wir: expected Variant/GenericInstance, got {other:?} (variant_type_id={variant_type_id:?})"
            ),
        };

        let fq = format!("{variant_module_source}//{variant_name}");
        let case_fq = format!("{fq}::{case_name}");

        if let Some(case_type_id) = self.ctx.type_map.get(&case_fq).cloned() {
            let mut fields = vec![WirInstr::I32Const(case_index as i32)];
            if let Some(payload_instr) = payload {
                fields.push(payload_instr);
            }
            self.struct_new(case_type_id, fields)
        } else {
            let wir_type = self
                .ctx
                .type_id_to_wir_type(self.type_table, variant_type_id);
            let WirType::Ref { type_id, .. } = wir_type else {
                panic!(
                    "[WIR] build_variant_case_wir: case type {case_fq} not registered, and variant type {variant_type_id:?} is not a Ref ({wir_type:?})"
                );
            };
            let mut fields = vec![WirInstr::I32Const(case_index as i32)];
            if let Some(payload_instr) = payload {
                fields.push(payload_instr);
            }
            self.struct_new(type_id, fields)
        }
    }

    /// Translate variant test: check if variant is of a specific case.
    pub(super) fn translate_variant_test(&mut self, inner: &NirExpr, case_index: u32) -> WirInstr {
        let val = self.translate_expr(inner);

        // Look up variant type info
        let (var_name, var_module) = match self.type_table.get(inner.type_id) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_arg_for_generic(*t))
                    .collect();
                (
                    crate::name::mangle_generic_name(name, &type_arg_names),
                    module_source.clone(),
                )
            }
            _ => {
                // Non-variant: compare discriminant directly
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    return WirInstr::I32Eq(
                        Box::new(WirInstr::StructGet {
                            type_id,
                            field_name: "discriminant".to_string(),
                            expr: Box::new(val),
                            result_ty: WirType::I32,
                        }),
                        Box::new(WirInstr::I32Const(case_index as i32)),
                    );
                }
                return WirInstr::I32Const(0);
            }
        };

        let fq = format!("{var_module}//{var_name}");

        // Check if this case has a payload
        if let Some(variant_type_id) = self.ctx.type_map.get(&fq)
            && let crate::wir::WirTypeDef::Variant(vt) =
                &self.ctx.types[variant_type_id.index() as usize]
            && let Some(case) = vt.cases.get(case_index as usize)
        {
            if case.payload.is_empty() {
                // Unit variant: check discriminant
                let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    return WirInstr::I32Eq(
                        Box::new(WirInstr::StructGet {
                            type_id,
                            field_name: "discriminant".to_string(),
                            expr: Box::new(val),
                            result_ty: WirType::I32,
                        }),
                        Box::new(WirInstr::I32Const(case_index as i32)),
                    );
                }
            } else {
                // Payload variant: use ref.test
                let case_fq = format!("{fq}::{}", case.name);
                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq) {
                    return WirInstr::RefTest {
                        type_id: case_type_id.clone(),
                        nullable: false,
                        expr: Box::new(val),
                    };
                }
            }
        }

        // Fallback: compare discriminant
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, inner.type_id);
        if let WirType::Ref { type_id, .. } = wir_type {
            WirInstr::I32Eq(
                Box::new(WirInstr::StructGet {
                    type_id,
                    field_name: "discriminant".to_string(),
                    expr: Box::new(val),
                    result_ty: WirType::I32,
                }),
                Box::new(WirInstr::I32Const(case_index as i32)),
            )
        } else {
            WirInstr::I32Const(0)
        }
    }

    /// Translate variant payload extraction.
    pub(super) fn translate_variant_payload(
        &mut self,
        inner: &NirExpr,
        case_index: u32,
    ) -> WirInstr {
        let val = self.translate_expr(inner);

        // Look up variant type info
        let (var_name, var_module) = match self.type_table.get(inner.type_id) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
                ..
            } => {
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| self.type_table.mangle_type_arg_for_generic(*t))
                    .collect();
                (
                    crate::name::mangle_generic_name(name, &type_arg_names),
                    module_source.clone(),
                )
            }
            _ => return val,
        };

        let fq = format!("{var_module}//{var_name}");

        // Look up variant type info
        if let Some(variant_type_id) = self.ctx.type_map.get(&fq)
            && let crate::wir::WirTypeDef::Variant(vt) =
                &self.ctx.types[variant_type_id.index() as usize]
        {
            // ref.cast to the case struct, then struct.get the payload field
            if let Some(case) = vt.cases.get(case_index as usize) {
                let case_fq = format!("{fq}::{}", case.name);
                if let Some(case_type_id) = self.ctx.type_map.get(&case_fq) {
                    let cast = WirInstr::RefCast {
                        type_id: case_type_id.clone(),
                        nullable: false,
                        expr: Box::new(val),
                    };
                    let payload_result_ty = self.struct_field_wir_type(case_type_id, "payload_0");
                    let get = WirInstr::StructGet {
                        type_id: case_type_id.clone(),
                        field_name: "payload_0".to_string(),
                        expr: Box::new(cast),
                        result_ty: payload_result_ty.clone(),
                    };
                    // The variant case's `payload_0` field is declared
                    // nullable for the Option<&T> = &T | null boxing
                    // optimisation, but every `Some(_)` construction site
                    // wraps its value with `RefAsNonNull`, so the extracted
                    // value is invariantly non-null at runtime. Narrow the
                    // WIR type so downstream call-sites and struct writes
                    // see it as non-null.
                    if matches!(payload_result_ty, WirType::Ref { nullable: true, .. }) {
                        return WirInstr::RefAsNonNull(Box::new(get));
                    }
                    return get;
                }
            }
        }

        // Fallback
        val
    }
}

fn pattern_has_bindings(pattern: &NirPattern) -> bool {
    match pattern {
        NirPattern::Binding { .. } => true,
        NirPattern::Wildcard
        | NirPattern::Literal(_)
        | NirPattern::Enum { .. }
        | NirPattern::ConstantValue { .. }
        | NirPattern::Range { .. } => false,
        NirPattern::Variant { bindings, .. } => bindings.iter().any(pattern_has_bindings),
        NirPattern::Tuple(subs, _) => subs.iter().any(pattern_has_bindings),
        NirPattern::Struct { fields, .. } => {
            fields.iter().any(|f| pattern_has_bindings(&f.pattern))
        }
        NirPattern::Or(alts) => alts.iter().any(pattern_has_bindings),
    }
}
