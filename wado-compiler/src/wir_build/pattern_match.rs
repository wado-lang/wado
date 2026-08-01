//! Pattern matching translation — lowers `match`, `if let`, `while let`,
//! `matches`, and `switch` expressions to WIR control flow, including
//! variant construction, variant testing, and payload extraction.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::module_source::ModuleSource;
use crate::nir::NirLiteralPattern;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::wir::{WirInstr, WirType, WirTypeId};

use super::calls::{MULTIVALUE_I64_BUILTINS, MULTIVALUE_I64_RESULTS};
use super::translate::{FunctionTranslator, LabelEntry, declare_and_set_local};
use crate::nir_arena::{ArmData, BlockId, Body, ExprKind, Operand, PatId, PatKind};

/// Build `if condition { then_body } else { else_body }`, collapsing the
/// boolean-materialization idiom `if C { 1 } else { 0 }` to `C`.
///
/// Every condition match lowering feeds to an `If` already yields 0/1
/// (`ref.test`, comparisons), so dropping the redundant select is value-exact.
fn bool_if(
    condition: WirInstr,
    result: Option<WirType>,
    then_body: Vec<WirInstr>,
    else_body: Vec<WirInstr>,
) -> WirInstr {
    if matches!(then_body.as_slice(), [WirInstr::I32Const(1)])
        && matches!(else_body.as_slice(), [WirInstr::I32Const(0)])
    {
        return condition;
    }
    WirInstr::If {
        condition: Box::new(condition),
        result,
        then_body,
        else_body: Some(else_body),
    }
}

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
        scrutinee: Operand,
        min_value: i64,
        arms: &[BlockId],
        default: BlockId,
        result_type: TypeId,
    ) -> WirInstr {
        let arena = self.body;
        let has_result = result_type != TypeTable::UNIT && result_type != TypeTable::NEVER;
        let result_wir_type = if has_result {
            Some(self.ctx.type_id_to_wir_type(self.type_table, result_type))
        } else {
            None
        };

        // Translate scrutinee and adjust for min_value
        let scrut = self.translate_operand(scrutinee);
        let is_i64 = matches!(
            self.type_table.get(self.operand_type_id(scrutinee)),
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
            self.translate_stmts_as_value(&arena.blocks[default].stmts)
        } else {
            self.translate_stmts(&arena.blocks[default].stmts)
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
                    self.translate_stmts_as_value(&arena.blocks[*arm].stmts)
                } else {
                    self.translate_stmts(&arena.blocks[*arm].stmts)
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

    /// Bind `let [a, b] = builtin::i64_mul_wide_u(…)` straight into the
    /// binding locals: the Wasm instruction pushes its two results on the
    /// stack, so `MultiValueLocalBind` pops them into the bindings with no
    /// tuple struct in between (a `Wildcard` slot drops its result). The
    /// tuple struct the expression-position lowering builds
    /// (`calls::wrap_multivalue_i64`) would otherwise have to be recovered
    /// by a WIR pass.
    ///
    /// The pattern must name every result: one `local.set` per pushed value,
    /// or the operand stack is left unbalanced. A rest pattern (`[a, ..]`)
    /// does not, and neither does a shorter tuple — both fall through to the
    /// tuple-struct path.
    fn try_bind_multivalue_builtin(&mut self, pattern: PatId, value: Operand) -> Option<WirInstr> {
        let PatKind::Tuple(patterns, has_rest) = &self.body.pats[pattern].kind else {
            return None;
        };
        if *has_rest || patterns.len() != MULTIVALUE_I64_RESULTS {
            return None;
        }
        let expr = value.as_expr()?;
        let ExprKind::Call { func_id, args, .. } = &self.body.exprs[expr].kind else {
            return None;
        };
        let (func_id, args) = (*func_id, args.clone());
        let func = self.callee_descriptor(func_id);
        let builtin_name = func
            .builtin_name()
            .or_else(|| func.monomorphized_builtin_name())?;
        if !MULTIVALUE_I64_BUILTINS.contains(&builtin_name.as_str()) {
            return None;
        }

        let bindings: Vec<Option<u32>> = patterns
            .iter()
            .map(|p| match &self.body.pats[*p].kind {
                PatKind::Binding { local_index, .. } => Some(*local_index),
                _ => None,
            })
            .collect();
        let locals: Vec<Option<String>> = bindings
            .iter()
            .map(|b| b.map(|local_index| self.local_name(local_index)))
            .collect();

        let instr = self.translate_multivalue_i64_builtin(&builtin_name, &args);
        Some(WirInstr::MultiValueLocalBind {
            instr: Box::new(instr),
            locals,
        })
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
    /// `Binding` (a single multivalue result) is also accepted. Any other shape
    /// means pattern lowering stopped rewriting one it used to rewrite; emitting
    /// nothing would drop the destructure and leave its bindings unassigned, so
    /// it panics instead.
    pub(super) fn translate_let_pattern(&mut self, pattern: PatId, value: Operand) -> WirInstr {
        if let Some(instr) = self.try_bind_multivalue_builtin(pattern, value) {
            return instr;
        }
        let value_instr = self.translate_operand(value);
        let value_ty = self.operand_type_id(value);
        let arena = self.body;

        match &arena.pats[pattern].kind {
            PatKind::Tuple(patterns, _) => {
                let wir_type = self.wir_type(value_ty);
                let type_id = self.ref_type_id(value_ty);
                let mut instrs = Vec::new();

                let temp_name = format!("__let_pattern_{}", self.match_counter);
                self.match_counter += 1;
                instrs.extend(declare_and_set_local(
                    temp_name.clone(),
                    wir_type.clone(),
                    value_instr,
                ));

                for (i, sub_pattern) in patterns.iter().enumerate() {
                    if let PatKind::Binding { local_index, .. } = &arena.pats[*sub_pattern].kind {
                        let local_name = self.local_name(*local_index);
                        let field_name_str = format!("{i}");
                        let field_result_ty = self.struct_field_wir_type(&type_id, &field_name_str);
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
                }

                WirInstr::Seq(instrs)
            }
            PatKind::Binding { local_index, .. } => WirInstr::LocalSet {
                name: self.local_name(*local_index),
                value: Box::new(value_instr),
            },
            other => panic!(
                "[WIR] pattern lowering left a `LetDestructure` this translator cannot bind: {other:?}"
            ),
        }
    }

    /// Translate match expression as nested if-else chain.
    pub(super) fn translate_match(
        &mut self,
        scrutinee: Operand,
        arms: &[ArmData],
        result_type: TypeId,
    ) -> WirInstr {
        let has_result = result_type != TypeTable::UNIT && result_type != TypeTable::NEVER;
        let result_wir_type = if has_result {
            Some(self.ctx.type_id_to_wir_type(self.type_table, result_type))
        } else {
            None
        };

        // Store scrutinee in a local to avoid re-evaluation
        let scrut = self.translate_operand(scrutinee);
        let match_id = self.match_counter;
        self.match_counter += 1;
        let scrut_local_name = format!("__match_scrut_{match_id}");
        let scrut_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, self.operand_type_id(scrutinee));

        // Precompute, per source-order arm, whether it will be lowered as
        // irrefutable (body only, no surrounding `If`). Both the `if_depths`
        // depth counter below and the emission loop that follows consume this
        // slice, so the two views cannot disagree.
        let emitted_as_irrefutable =
            self.compute_emitted_as_irrefutable(self.operand_type_id(scrutinee), arms);

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
            for (idx, _arm) in arms.iter().enumerate() {
                // Every non-irrefutable arm — guarded or not — emits exactly one
                // wrapping `If`. Guarded arms fold their pattern test and guard
                // into a single short-circuiting condition (see below), so they
                // add no extra nesting.
                if !emitted_as_irrefutable[idx] {
                    depth += 1;
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
                    arm.pattern,
                    &scrut_local_name,
                    self.operand_type_id(scrutinee),
                    &mut bindings,
                );
                let body = if has_result {
                    match arm.body {
                        Operand::Value(v) => self.extract_value(v),
                        Operand::Expr(e) => self.translate_expr_as_value(e),
                    }
                } else {
                    let instr = self.translate_operand(arm.body);
                    // If the arm body produces a non-unit value (e.g. after inlining
                    // transforms a Block into a bare call), drop it to avoid leaving
                    // values on the Wasm stack. Guard with `produces_stack_value()` to
                    // avoid emitting `drop` after instructions that produce no value
                    // (e.g. `Block{result: None}` from LabeledBlock fusion).
                    if self.operand_type_id(arm.body) != TypeTable::UNIT
                        && self.operand_type_id(arm.body) != TypeTable::NEVER
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
                arm.pattern,
                &scrut_local_name,
                self.operand_type_id(scrutinee),
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
                // Guarded arm. Fold the pattern test and the guard into a single
                // short-circuiting condition so the fall-through subtree
                // (`result`) is placed at exactly one tree depth.
                //
                // A nested two-`If` form (outer pattern test, inner guard test)
                // would clone `result` into both the inner guard-`else` and the
                // outer pattern-`else` — copies that sit at depths differing by
                // one. Break depths are baked in when an arm body is translated,
                // so the shallower copy ends up with a stale (too-large) `Br`
                // depth, producing invalid core Wasm (issue #1418). Collapsing to
                // one `If` keeps `result` at a single depth and also avoids the
                // 2^N clone explosion for many guarded arms.
                let guard_expr = self.translate_operand(*guard);
                let pattern_is_trivially_true = matches!(&condition, WirInstr::I32Const(1));
                let folded_condition = if pattern_is_trivially_true {
                    // Pattern always matches: bindings are safe to emit
                    // unconditionally, so the condition is just `bindings; guard`.
                    if bindings.is_empty() {
                        guard_expr
                    } else {
                        let mut seq = bindings.clone();
                        seq.push(guard_expr);
                        WirInstr::Seq(seq)
                    }
                } else {
                    // Refutable pattern: short-circuit as
                    // `if pattern { bindings; guard } else { false }` so the
                    // bindings (e.g. `ref.cast`) run only after the pattern
                    // matches, never against the wrong variant.
                    let mut guarded_then = bindings.clone();
                    guarded_then.push(guard_expr);
                    bool_if(
                        condition,
                        Some(WirType::I32),
                        guarded_then,
                        vec![WirInstr::I32Const(0)],
                    )
                };
                // Bindings already ran inside the condition, so the arm body is
                // emitted alone.
                result = bool_if(
                    folded_condition,
                    result_wir_type.clone(),
                    vec![body.clone()],
                    vec![result],
                );
            } else {
                result = bool_if(
                    condition,
                    result_wir_type.clone(),
                    body_instrs,
                    vec![result],
                );
            }
        }

        let mut seq = declare_and_set_local(scrut_local_name, scrut_wir_type, scrut).to_vec();
        seq.push(result);
        WirInstr::Seq(seq)
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
    fn compute_emitted_as_irrefutable(&self, scrut_type: TypeId, arms: &[ArmData]) -> Vec<bool> {
        let mut out: Vec<bool> = arms
            .iter()
            .map(|arm| {
                matches!(
                    &self.body.pats[arm.pattern].kind,
                    PatKind::Wildcard
                        | PatKind::Binding { .. }
                        | PatKind::Struct { .. }
                        | PatKind::Tuple(_, _)
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
    /// Accepts `PatKind::Variant`, `PatKind::Enum`, and one level of
    /// `PatKind::Or` whose alternatives are themselves `Variant`/`Enum`
    /// patterns. Anything else (wildcards, literals, ranges, guards, nested
    /// Ors with non-Variant/Enum alternatives) bails out.
    fn match_is_exhaustive(&self, scrut_type: TypeId, arms: &[ArmData]) -> bool {
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
            if !self.arm_pattern_covers_cases(arm.pattern, &index_of, &mut seen, &mut covered) {
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
        pattern: PatId,
        index_of: &CaseIndexer,
        seen: &mut [bool],
        covered: &mut usize,
    ) -> bool {
        match &self.body.pats[pattern].kind {
            PatKind::Variant { variant_name, .. } => {
                let Some(i) = index_of.by_name(variant_name) else {
                    return false;
                };
                self.record_case(i, seen, covered)
            }
            PatKind::Enum { case_index, .. } => {
                self.record_case(*case_index as usize, seen, covered)
            }
            PatKind::Or(alts) => alts
                .clone()
                .iter()
                .all(|alt| self.arm_pattern_covers_cases(*alt, index_of, seen, covered)),
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
                let mangled =
                    super::types::generic_instance_name(self.type_table, name, type_args);
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
        let fq = crate::name::wir_type_key(module_source, variant_name);
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

    /// Key of the WIR variant type a scrutinee lowers to. A variant arrives
    /// either as `ResolvedType::Variant` or, once monomorphized, as the
    /// `GenericInstance` spelling; registration covers both.
    #[track_caller]
    fn variant_type_key(&self, type_id: TypeId) -> String {
        match self.type_table.get(type_id) {
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => crate::name::wir_type_key(module_source, name),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                let mangled =
                    super::types::generic_instance_name(self.type_table, name, type_args);
                crate::name::wir_type_key(module_source, &mangled)
            }
            other => panic!("[WIR] expected a variant type, got {other:?}"),
        }
    }

    /// The registered WIR variant a scrutinee lowers to, with its type key.
    ///
    /// Type checking already proved the scrutinee is this variant. Degrading on
    /// a miss — testing a discriminant nothing wrote, or projecting a payload
    /// out of the base type — is a miscompile the validator rarely catches.
    #[track_caller]
    fn variant_def(&self, type_id: TypeId) -> (String, &crate::wir::WirVariantType) {
        let key = self.variant_type_key(type_id);
        let Some(wir_type_id) = self.ctx.type_map.get(&key) else {
            panic!("[WIR] variant `{key}` is not registered");
        };
        let crate::wir::WirTypeDef::Variant(vt) = &self.ctx.types[wir_type_id.index() as usize]
        else {
            panic!("[WIR] `{key}` is registered as a non-variant WIR type");
        };
        (key, vt)
    }

    /// The WIR struct type of a variant's payload-carrying case.
    ///
    /// Only a payload case has a subtype of its own; a unit case shares the base
    /// variant struct and is told apart by its discriminant.
    #[track_caller]
    fn variant_case_type_id(&self, variant_key: &str, case_name: &str) -> WirTypeId {
        let case_key = crate::name::wir_variant_case_key(variant_key, case_name);
        let Some(case_type_id) = self.ctx.type_map.get(&case_key) else {
            panic!("[WIR] payload case `{case_key}` is not registered");
        };
        case_type_id.clone()
    }

    /// Read a variant's `discriminant` field off `expr`.
    fn variant_discriminant(&self, variant_type_id: TypeId, expr: WirInstr) -> WirInstr {
        WirInstr::StructGet {
            type_id: self.ref_type_id(variant_type_id),
            field_name: crate::name::VARIANT_DISCRIMINANT_FIELD.to_string(),
            expr: Box::new(expr),
            result_ty: WirType::I32,
        }
    }

    /// Generate a condition expression for a pattern.
    /// Returns an i32 (0 or 1) indicating whether the pattern matches.
    fn translate_pattern_condition(
        &self,
        pattern: PatId,
        scrut_local: &str,
        scrut_type: TypeId,
    ) -> WirInstr {
        match &self.body.pats[pattern].kind {
            PatKind::Wildcard | PatKind::Binding { .. } => {
                WirInstr::I32Const(1) // always matches
            }
            PatKind::Literal(lit) => {
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
                };
                self.translate_literal_pattern_condition(lit, scrut_get, scrut_type)
            }
            PatKind::Enum { case_index, .. } => {
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
            PatKind::Variant { variant_name, .. } => {
                let scrut_get = WirInstr::LocalGet {
                    name: scrut_local.to_string(),
                    result_ty: self.wir_type(scrut_type),
                };

                let (variant_key, vt) = self.variant_def(scrut_type);
                let Some(case) = vt.cases.iter().find(|c| c.name == *variant_name) else {
                    panic!("[WIR] variant `{variant_key}` has no case `{variant_name}`");
                };
                let case_index = case.index as i32;
                if case.payload.is_empty() {
                    WirInstr::I32Eq(
                        Box::new(self.variant_discriminant(scrut_type, scrut_get)),
                        Box::new(WirInstr::I32Const(case_index)),
                    )
                } else {
                    WirInstr::RefTest {
                        type_id: self.variant_case_type_id(&variant_key, variant_name),
                        nullable: false,
                        expr: Box::new(scrut_get),
                    }
                }
            }
            PatKind::Range {
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
            PatKind::Tuple(_, _) | PatKind::Struct { .. } => {
                // Tuple/struct patterns: always irrefutable
                WirInstr::I32Const(1)
            }
            PatKind::Or(alternatives) => {
                // Or pattern: combine conditions with logical OR
                let alternatives = alternatives.clone();
                let mut result = WirInstr::I32Const(0);
                for alt in alternatives {
                    let cond = self.translate_pattern_condition(alt, scrut_local, scrut_type);
                    result = WirInstr::I32Or(Box::new(result), Box::new(cond));
                }
                result
            }
            PatKind::ConstantValue { .. } => {
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
        pattern: PatId,
        scrut_local: &str,
        scrut_type: TypeId,
        instrs: &mut Vec<WirInstr>,
    ) {
        let arena = self.body;
        match &arena.pats[pattern].kind {
            PatKind::Binding { local_index, .. } => {
                instrs.push(WirInstr::LocalSet {
                    name: self.local_name(*local_index),
                    value: Box::new(WirInstr::LocalGet {
                        name: scrut_local.to_string(),
                        result_ty: self.wir_type(scrut_type),
                    }),
                });
            }
            PatKind::Variant {
                variant_name,
                bindings,
                enum_type,
                payload_type,
            } => {
                if bindings.is_empty() {
                    return;
                }

                let (variant_key, vt) = self.variant_def(*enum_type);
                let Some(case) = vt.cases.iter().find(|c| c.name == *variant_name) else {
                    panic!("[WIR] variant `{variant_key}` has no case `{variant_name}`");
                };
                // A unit case carries no payload struct, so its bindings can only
                // be unit-typed — and unit has no Wasm local to bind.
                let case_has_payload = !case.payload.is_empty();
                if case_has_payload {
                    let case_type_id = self.variant_case_type_id(&variant_key, variant_name);
                    // Count bindings that actually need the cast result. A
                    // `Wildcard` or literal-shaped sub-pattern doesn't read
                    // any payload field, so it doesn't consume the cast.
                    let consumers = bindings
                        .iter()
                        .filter(|b| {
                            !matches!(
                                &arena.pats[**b].kind,
                                PatKind::Wildcard
                                    | PatKind::Literal(_)
                                    | PatKind::Enum { .. }
                                    | PatKind::ConstantValue { .. }
                                    | PatKind::Range { .. }
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
                        instrs.extend(declare_and_set_local(
                            cast_local.clone(),
                            WirType::Ref {
                                type_id: case_type_id.clone(),
                                nullable: false,
                            },
                            WirInstr::RefCast {
                                type_id: case_type_id.clone(),
                                nullable: false,
                                expr: Box::new(WirInstr::LocalGet {
                                    name: scrut_local.to_string(),
                                    result_ty: self.wir_type(scrut_type),
                                }),
                            },
                        ));
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
                        if let PatKind::Binding {
                            local_index,
                            type_id,
                            ..
                        } = &arena.pats[*binding].kind
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
                                Some(&payload_field_wir),
                                payload_get,
                                instrs,
                            );
                        } else if !matches!(
                            &arena.pats[*binding].kind,
                            PatKind::Wildcard
                                | PatKind::Literal(_)
                                | PatKind::Enum { .. }
                                | PatKind::ConstantValue { .. }
                                | PatKind::Range { .. }
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
                            instrs.extend(declare_and_set_local(
                                temp_name.clone(),
                                payload_wir,
                                payload_get,
                            ));
                            self.emit_pattern_bindings(*binding, &temp_name, payload_tid, instrs);
                        }
                    }
                } else {
                    for binding in bindings {
                        let PatKind::Binding { type_id, .. } = &arena.pats[*binding].kind else {
                            continue;
                        };
                        assert!(
                            matches!(self.wir_type(*type_id), WirType::Unit),
                            "[WIR] case `{variant_key}::{variant_name}` carries no payload, \
                             so binding it to a non-unit local has nothing to read"
                        );
                    }
                }
            }
            PatKind::Wildcard
            | PatKind::Literal(_)
            | PatKind::Enum { .. }
            | PatKind::ConstantValue { .. }
            | PatKind::Range { .. } => {
                // No bindings needed
            }
            PatKind::Tuple(sub_patterns, _) => {
                let wir_type = self.wir_type(scrut_type);
                let type_id = &self.ref_type_id(scrut_type);
                let element_types = self
                    .type_table
                    .as_tuple(scrut_type)
                    .unwrap_or_else(|| panic!("[WIR] tuple pattern on non-tuple {scrut_type:?}"));
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
                    match &arena.pats[*sub_pattern].kind {
                        PatKind::Binding { local_index, .. } => {
                            let local_type_id =
                                if (*local_index as usize) < self.tir_func.locals.len() {
                                    self.tir_func.locals[*local_index as usize].type_id
                                } else {
                                    tuple_element_type(&element_types, i)
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
                        PatKind::Wildcard => {}
                        _ => {
                            self.local_counter += 1;
                            let temp_name = format!("__tuple_elem_{}", self.local_counter);
                            let elem_type = tuple_element_type(&element_types, i);
                            let elem_wir_type =
                                self.ctx.type_id_to_wir_type(self.type_table, elem_type);
                            instrs.extend(declare_and_set_local(
                                temp_name.clone(),
                                elem_wir_type,
                                field_get,
                            ));
                            self.emit_pattern_bindings(
                                *sub_pattern,
                                &temp_name,
                                elem_type,
                                instrs,
                            );
                        }
                    }
                }
            }
            PatKind::Struct { fields, .. } => {
                // Emit field bindings for struct patterns in match arms
                let wir_type = self.wir_type(scrut_type);
                let type_id = &self.ref_type_id(scrut_type);
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
                    match &arena.pats[field.pattern].kind {
                        PatKind::Binding { local_index, .. } => {
                            instrs.push(WirInstr::LocalSet {
                                name: self.local_name(*local_index),
                                value: Box::new(field_get),
                            });
                        }
                        PatKind::Wildcard => {}
                        _ => {
                            self.local_counter += 1;
                            let temp_name = format!("__struct_field_{}", self.local_counter);
                            let field_type =
                                self.resolve_struct_field_type(scrut_type, &field.field_name);
                            let field_wir_type =
                                self.ctx.type_id_to_wir_type(self.type_table, field_type);
                            instrs.extend(declare_and_set_local(
                                temp_name.clone(),
                                field_wir_type,
                                field_get,
                            ));
                            self.emit_pattern_bindings(
                                field.pattern,
                                &temp_name,
                                field_type,
                                instrs,
                            );
                        }
                    }
                }
            }
            PatKind::Or(alternatives) => {
                // Or patterns: emit bindings for each alternative, guarded by its condition.
                // For alternatives with only wildcards (no real bindings), skip entirely.
                let alternatives = alternatives.clone();
                let has_any_bindings = alternatives
                    .iter()
                    .any(|alt| pattern_has_bindings(arena, *alt));
                if !has_any_bindings {
                    return;
                }
                // Emit conditional binding extraction: check each alternative and
                // emit bindings from the one that matches.
                // Build a nested if-else chain from the inside out.
                let mut result: Option<WirInstr> = None;
                for alt in alternatives.iter().rev() {
                    let cond = self.translate_pattern_condition(*alt, scrut_local, scrut_type);
                    let mut body = Vec::new();
                    self.emit_pattern_bindings(*alt, scrut_local, scrut_type, &mut body);
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
        let needs_boxing = super::translate::ref_binding_needs_boxing(binding_wir, source_wir);
        let value = if needs_boxing {
            let WirType::Ref {
                type_id: box_tid, ..
            } = binding_wir
            else {
                unreachable!("ref_binding_needs_boxing only reports boxing for a Ref binding")
            };
            WirInstr::StructNew {
                type_id: box_tid.clone(),
                fields: vec![source],
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
    ///
    /// Field 0 is the discriminant, so payload `i` sits at `i + 1`. Reporting
    /// "unknown" instead would silently disable the boxing decision in
    /// [`Self::emit_pattern_binding_set`].
    #[track_caller]
    fn get_case_payload_wir_type(
        &self,
        case_type_id: &crate::wir::WirTypeId,
        payload_index: usize,
    ) -> WirType {
        let type_def = &self.ctx.types[case_type_id.index() as usize];
        let crate::wir::WirTypeDef::Struct(s) = type_def else {
            panic!("[WIR] variant case type {case_type_id:?} is not registered as a struct");
        };
        let Some(field) = s.fields.get(payload_index + 1) else {
            panic!(
                "[WIR] variant case `{}` has no payload field {payload_index}",
                s.name.fq
            );
        };
        field.ty.clone()
    }

    /// Resolve the `TypeId` of a struct field by name. Type checking already
    /// proved the pattern names a field this struct has.
    #[track_caller]
    fn resolve_struct_field_type(&self, struct_type: TypeId, field_name: &str) -> TypeId {
        let ResolvedType::Struct {
            decl_name,
            module_source,
            type_args,
        } = self.type_table.get(struct_type)
        else {
            panic!("[WIR] struct pattern on non-struct {struct_type:?}");
        };
        let name = self.type_table.struct_rendered_name(decl_name, type_args);
        self.ctx
            .package
            .structs
            .iter()
            .filter(|s| s.module_source == *module_source && s.name == name)
            .flat_map(|s| &s.fields)
            .find(|f| f.name == field_name)
            .unwrap_or_else(|| panic!("[WIR] struct `{name}` has no field `{field_name}`"))
            .type_id
    }

    /// Translate variant construction: `Shape::Circle(5.0)`
    pub(super) fn translate_variant_construct(
        &mut self,
        variant_type: TypeId,
        case_index: u32,
        case_name: &str,
        payload: Option<Operand>,
        result_type: TypeId,
    ) -> WirInstr {
        let (variant_key, vt) = self.variant_def(variant_type);
        let Some(case) = vt.cases.get(case_index as usize) else {
            panic!("[WIR] variant `{variant_key}` has no case at index {case_index}");
        };
        let struct_type_id = if case.payload.is_empty() {
            self.ref_type_id(result_type)
        } else {
            self.variant_case_type_id(&variant_key, case_name)
        };

        let mut fields = vec![WirInstr::I32Const(case_index as i32)];
        if let Some(payload_expr) = payload {
            fields.push(self.translate_operand(payload_expr));
        }
        self.struct_new(struct_type_id, fields)
    }

    /// Translate variant test: check if variant is of a specific case.
    pub(super) fn translate_variant_test(&mut self, inner: Operand, case_index: u32) -> WirInstr {
        let val = self.translate_operand(inner);
        let inner_ty = self.operand_type_id(inner);

        let (variant_key, vt) = self.variant_def(inner_ty);
        let Some(case) = vt.cases.get(case_index as usize) else {
            panic!("[WIR] variant `{variant_key}` has no case at index {case_index}");
        };
        if case.payload.is_empty() {
            WirInstr::I32Eq(
                Box::new(self.variant_discriminant(inner_ty, val)),
                Box::new(WirInstr::I32Const(case_index as i32)),
            )
        } else {
            let case_name = case.name.clone();
            WirInstr::RefTest {
                type_id: self.variant_case_type_id(&variant_key, &case_name),
                nullable: false,
                expr: Box::new(val),
            }
        }
    }

    /// Translate variant payload extraction.
    pub(super) fn translate_variant_payload(
        &mut self,
        inner: Operand,
        case_index: u32,
    ) -> WirInstr {
        let val = self.translate_operand(inner);
        let inner_ty = self.operand_type_id(inner);

        // Every step below must resolve. Falling back to the scrutinee would
        // extract the variant where its payload belongs — a miscompile the Wasm
        // validator only catches when the two happen to differ in shape, and
        // silently wrong output when they do not.
        let (variant_key, vt) = self.variant_def(inner_ty);
        let Some(case) = vt.cases.get(case_index as usize) else {
            panic!("[WIR] variant `{variant_key}` has no case at index {case_index}");
        };
        let case_name = case.name.clone();
        let case_type_id = self.variant_case_type_id(&variant_key, &case_name);

        // ref.cast to the case struct, then struct.get the payload field.
        let cast = WirInstr::RefCast {
            type_id: case_type_id.clone(),
            nullable: false,
            expr: Box::new(val),
        };
        let payload_result_ty = self.struct_field_wir_type(&case_type_id, &crate::name::variant_payload_field(0));
        let get = WirInstr::StructGet {
            type_id: case_type_id,
            field_name: crate::name::variant_payload_field(0),
            expr: Box::new(cast),
            result_ty: payload_result_ty.clone(),
        };
        // The variant case's `payload_0` field is declared nullable for the
        // `Option<&T>` = `&T | null` boxing optimisation, but every `Some(_)`
        // construction site wraps its value with `RefAsNonNull`, so the
        // extracted value is invariantly non-null at runtime. Narrow the WIR
        // type so downstream call-sites and struct writes see it as non-null.
        if matches!(payload_result_ty, WirType::Ref { nullable: true, .. }) {
            return WirInstr::RefAsNonNull(Box::new(get));
        }
        get
    }
}

fn pattern_has_bindings(body: &Body, pattern: PatId) -> bool {
    match &body.pats[pattern].kind {
        PatKind::Binding { .. } => true,
        PatKind::Wildcard
        | PatKind::Literal(_)
        | PatKind::Enum { .. }
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => false,
        PatKind::Variant { bindings, .. } => {
            bindings.iter().any(|p| pattern_has_bindings(body, *p))
        }
        PatKind::Tuple(subs, _) => subs.iter().any(|p| pattern_has_bindings(body, *p)),
        PatKind::Struct { fields, .. } => {
            fields.iter().any(|f| pattern_has_bindings(body, f.pattern))
        }
        PatKind::Or(alts) => alts.iter().any(|p| pattern_has_bindings(body, *p)),
    }
}

/// The declared type of a tuple's `i`-th element. Arity is fixed by type
/// checking, so an out-of-range index is a lowering bug.
#[track_caller]
fn tuple_element_type(element_types: &[TypeId], index: usize) -> TypeId {
    *element_types.get(index).unwrap_or_else(|| {
        panic!(
            "[WIR] tuple pattern binds element {index} of a {}-element tuple",
            element_types.len()
        )
    })
}
