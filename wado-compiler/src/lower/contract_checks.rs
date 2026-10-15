//! Check each call's contract at the call, before the plan.
//!
//! A function `F` writing `contract` clauses reifies as two: its twin
//! `F$unchecked`, holding the body, and `F` itself, holding only the clauses,
//! each as `if $contract_clause() { … }`, and a call to the twin. Here each
//! call to `F` becomes a copy of the clauses, reading the call's arguments,
//! and a call to the twin, so a failed clause reports the call's position as
//! `$contract_site`. A call `unchecked` marks already names the twin, unless
//! `-f contract-checks` turned it back (see [`check_marked_calls`]). `F` itself
//! is left to the calls that reach it as a value, which check at the clause.

use crate::compiler_item::CompilerItems;
use crate::defs::DefId;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::place::{is_source_place, source_place_subscripts_mut};
use crate::lower::translate::{convert_function_ref, tir_function_key};
use crate::name::{
    CONTRACT_CLAUSE_INTRINSIC, CONTRACT_SITE_INTRINSIC, FunctionId, LocalMethodName, RefKind,
    minted_name,
};
use crate::tir::{
    CallArg, LetStorage, TemplateId, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal,
    TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor, shift_locals};

/// Under `-f contract-checks`, point every call `unchecked` marks back at its
/// checked function, so that it is checked as an unmarked call is. Runs ahead
/// of monomorphization, which then instantiates the checked function the call
/// reaches.
pub fn check_marked_calls(flat: &FlatPackage) {
    if !flat.codegen_flags.contract_checks {
        return;
    }
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        let own = func.def_id;
        if let Some(body) = func.body.as_mut() {
            MarkedCalls { own }.visit_block(body);
        }
    }
}

struct MarkedCalls {
    /// The declaration the function holding the calls was reified from: its
    /// own call to its twin is the split, not a mark.
    own: Option<DefId>,
}

impl TirMutVisitor for MarkedCalls {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Call { func, .. } = &mut expr.kind {
            // A call through a bound learns its template from its method name
            // only at monomorphization.
            let marked = match func.template.as_ref().and_then(TemplateId::checked) {
                Some(checked) => checked.def() != self.own,
                None => func
                    .method_info
                    .as_ref()
                    .is_some_and(LocalMethodName::is_unchecked),
            };
            if marked {
                func.retarget_to_checked();
            }
        }
        self.walk_expr(expr);
    }
}

/// Copy each function's `contract` clauses to every call naming it.
pub fn lower(flat: &FlatPackage) {
    let contracts = collect_contracts(flat);
    let type_table = flat.type_table.borrow();
    let items = type_table.compiler_items();
    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        let Some(mut body) = func.body.take() else {
            continue;
        };
        let site_module = func.module_source.to_string();
        let mut expander = Expander {
            contracts: &contracts,
            type_table: &type_table,
            items,
            func: &mut func,
            site: None,
            site_module,
            expanding: Vec::new(),
        };
        expander.visit_block(&mut body);
        Unwrap.visit_block(&mut body);
        func.body = Some(body);
    }
}

/// What a call to a function writing `contract` clauses copies.
struct Contract {
    /// The clauses, as the function's body opens with them.
    clauses: Vec<TirStmt>,
    /// The call to the twin that follows them; none for a trait's clause
    /// holder, which is clauses alone.
    forward: Option<TirExpr>,
    params: Vec<TypeId>,
    locals: Vec<TirLocal>,
    address_taken: IndexSet<u32>,
}

fn collect_contracts(flat: &FlatPackage) -> IndexMap<FunctionId, Contract> {
    let mut contracts = IndexMap::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let Some(body) = &func.body else {
            continue;
        };
        let clauses = body
            .stmts
            .iter()
            .take_while(|s| is_contract_clause(s))
            .count();
        // A closure numbers its locals from its own zero, so its captures
        // name a parameter by index, which no argument can stand in for.
        if clauses == 0 || body.stmts[..clauses].iter().any(holds_closure) {
            continue;
        }
        let forward = match body.stmts.get(clauses..) {
            Some([]) => None,
            Some(
                [
                    TirStmt {
                        kind: TirStmtKind::Expr(call) | TirStmtKind::Return { value: Some(call) },
                        ..
                    },
                ],
            ) => Some(call.clone()),
            rest => unreachable!(
                "`{}` is its `contract` clauses and a call to its twin, not {rest:?}",
                func.name
            ),
        };
        contracts.insert(
            tir_function_key(&func),
            Contract {
                clauses: body.stmts[..clauses].to_vec(),
                forward,
                params: func.params.iter().map(|p| p.type_id).collect(),
                locals: func.locals.clone(),
                address_taken: func.address_taken_locals.clone(),
            },
        );
    }
    contracts
}

/// `expr`, leaving a unit in its place.
fn take(expr: &mut TirExpr) -> TirExpr {
    let span = expr.span;
    std::mem::replace(expr, TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span))
}

fn holds_closure(stmt: &TirStmt) -> bool {
    struct Find(bool);
    impl TirRefVisitor for Find {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if matches!(expr.kind, TirExprKind::Closure { .. }) {
                self.0 = true;
            } else {
                self.walk_expr(expr);
            }
        }
    }
    let mut find = Find(false);
    find.visit_stmt(stmt);
    find.0
}

pub fn is_contract_clause(stmt: &TirStmt) -> bool {
    matches!(
        &stmt.kind,
        TirStmtKind::If { condition, else_block: None, .. } if is_intrinsic_call(condition, CONTRACT_CLAUSE_INTRINSIC)
    )
}

fn is_intrinsic_call(expr: &TirExpr, intrinsic: &str) -> bool {
    matches!(&expr.kind, TirExprKind::Call { func, .. } if func.is_builtin_named(intrinsic))
}

struct Expander<'a> {
    contracts: &'a IndexMap<FunctionId, Contract>,
    type_table: &'a TypeTable,
    items: &'a CompilerItems,
    func: &'a mut TirFunction,
    /// The position a clause copied here reports: the call's, where the copy
    /// reaches another call through a clause of its own.
    site: Option<String>,
    site_module: String,
    /// The functions whose clauses are being copied, outermost first. A clause
    /// calling its own function back is left to call it.
    expanding: Vec<FunctionId>,
}

impl TirMutVisitor for Expander<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // A closure's body is a frame of its own, whose calls keep reaching
        // the checked function.
        if matches!(expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
        let TirExprKind::Call { func, .. } = &expr.kind else {
            return;
        };
        let key = convert_function_ref(func).function_id();
        if self.contracts.contains_key(&key) && !self.expanding.contains(&key) {
            self.expand(expr, key);
        }
    }
}

impl Expander<'_> {
    /// Rewrite `call` to its callee's clauses over its arguments, then the
    /// call to the twin.
    fn expand(&mut self, call: &mut TirExpr, key: FunctionId) {
        let contracts = self.contracts;
        let contract = &contracts[&key];
        let span = call.span;
        let site = self
            .site
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.site_module, span.line));
        let TirExprKind::Call { args, .. } = &mut call.kind else {
            unreachable!("only a call expands");
        };
        let mut stmts = Vec::new();
        let substitutes: Vec<TirExpr> = args
            .iter_mut()
            .zip(&contract.params)
            .map(|(arg, &param_type)| self.bind_ahead(arg, param_type, &mut stmts))
            .collect();

        let offset = self.func.locals.len() as u32;
        self.func.locals.extend(contract.locals.iter().cloned());
        self.func.local_count = self.func.locals.len() as u32;
        self.func
            .address_taken_locals
            .extend(contract.address_taken.iter().map(|index| index + offset));
        let mut clauses = TirExpr::new(
            TirExprKind::Block(TirBlock::new(contract.clauses.clone(), span)),
            TypeTable::UNIT,
            span,
        );
        shift_locals(&mut clauses, offset);
        Substitute {
            offset,
            substitutes: &substitutes,
            site: &site,
        }
        .visit_expr(&mut clauses);
        let TirExprKind::Block(TirBlock { stmts: copied, .. }) = clauses.kind else {
            unreachable!("built as a block above");
        };

        let outer_site = self.site.replace(site);
        self.expanding.push(key);
        let mut copied = TirBlock::new(copied, span);
        self.visit_block(&mut copied);
        self.expanding.pop();
        self.site = outer_site;
        stmts.extend(copied.stmts);

        let value = match &contract.forward {
            Some(forward) => {
                let TirExprKind::Call {
                    func, type_args, ..
                } = &forward.kind
                else {
                    unreachable!("a function forwards its call to its twin");
                };
                let TirExprKind::Call { args, .. } =
                    std::mem::replace(&mut call.kind, TirExprKind::Unit)
                else {
                    unreachable!("only a call expands");
                };
                TirExpr::new(
                    TirExprKind::Call {
                        func: func.clone(),
                        type_args: type_args.clone(),
                        args,
                    },
                    call.type_id,
                    span,
                )
            }
            None => TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
        };
        stmts.push(TirStmt::new(TirStmtKind::Expr(value), span));
        call.kind = TirExprKind::Block(TirBlock::new(stmts, span));
    }

    /// What the clauses read for the parameter `arg` passes: a place stays in
    /// the call for a `&mut` parameter to write, with its subscripts bound
    /// ahead; any other value is bound ahead and read from there.
    fn bind_ahead(
        &mut self,
        arg: &mut CallArg,
        param_type: TypeId,
        stmts: &mut Vec<TirStmt>,
    ) -> TirExpr {
        let borrowed_place = matches!(
            &arg.expr.kind,
            TirExprKind::Unary { op: TirUnaryOp::Ref | TirUnaryOp::MutRef, expr } if is_source_place(expr, self.items)
        );
        let place = if borrowed_place {
            let TirExprKind::Unary { expr, .. } = &mut arg.expr.kind else {
                unreachable!("matched above");
            };
            &mut **expr
        } else if is_source_place(&arg.expr, self.items) {
            &mut arg.expr
        } else {
            let value = take(&mut arg.expr);
            arg.expr = self.bind(value, stmts);
            return arg.expr.clone();
        };
        let items = self.items;
        for subscript in source_place_subscripts_mut(place, items) {
            if !matches!(subscript.kind, TirExprKind::Local { .. }) {
                let value = take(subscript);
                *subscript = self.bind(value, stmts);
            }
        }
        // A receiver is passed as its place where the parameter borrows it.
        let borrow = RefKind::from_resolved(self.type_table.get(param_type));
        match borrow {
            Some(kind)
                if RefKind::from_resolved(self.type_table.get(arg.expr.type_id)).is_none() =>
            {
                let op = match kind {
                    RefKind::Shared => TirUnaryOp::Ref,
                    RefKind::Mut => TirUnaryOp::MutRef,
                };
                TirExpr::new(
                    TirExprKind::Unary {
                        op,
                        expr: Box::new(arg.expr.clone()),
                    },
                    param_type,
                    arg.expr.span,
                )
            }
            _ => arg.expr.clone(),
        }
    }

    /// Bind `value` to a new local of the function, and answer the read of it.
    fn bind(&mut self, value: TirExpr, stmts: &mut Vec<TirStmt>) -> TirExpr {
        let index = self.func.locals.len() as u32;
        let name = minted_name("contract_arg", index);
        let (type_id, span) = (value.type_id, value.span);
        self.func.locals.push(TirLocal {
            name: name.clone(),
            type_id,
            is_mut: false,
            span,
        });
        self.func.local_count = self.func.locals.len() as u32;
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.clone(),
                local_index: index,
                is_mut: false,
                type_id,
                value,
                storage: LetStorage::Planned,
            },
            span,
        ));
        TirExpr::new(TirExprKind::Local { index, name }, type_id, span)
    }
}

/// Read each parameter of a copied clause as the call's argument, and report
/// the call's position.
struct Substitute<'a> {
    /// Where the callee's locals, its parameters first, start among the
    /// caller's.
    offset: u32,
    substitutes: &'a [TirExpr],
    site: &'a str,
}

impl TirMutVisitor for Substitute<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &expr.kind {
            TirExprKind::Local { index, .. }
                if (self.offset..self.offset + self.substitutes.len() as u32).contains(index) =>
            {
                *expr = self.substitutes[(index - self.offset) as usize].clone();
            }
            _ if is_intrinsic_call(expr, CONTRACT_SITE_INTRINSIC) => {
                expr.kind = TirExprKind::StringLiteral(self.site.to_string());
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Unwrap every `if $contract_clause() { … }` left, and answer each
/// `$contract_site(clause)` left with the clause's own position.
struct Unwrap;

impl TirMutVisitor for Unwrap {
    fn visit_block(&mut self, block: &mut TirBlock) {
        block.stmts = std::mem::take(&mut block.stmts)
            .into_iter()
            .flat_map(|stmt| {
                if !is_contract_clause(&stmt) {
                    return vec![stmt];
                }
                let TirStmtKind::If { then_block, .. } = stmt.kind else {
                    unreachable!("a clause is an `if`");
                };
                then_block.stmts
            })
            .collect();
        self.walk_block(block);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if is_intrinsic_call(expr, CONTRACT_SITE_INTRINSIC) {
            let TirExprKind::Call { args, .. } = &mut expr.kind else {
                unreachable!("an intrinsic is called");
            };
            let clause_site = args
                .iter_mut()
                .next()
                .expect("`$contract_site` takes the clause's position");
            *expr = take(&mut clause_site.expr);
            return;
        }
        assert!(
            !is_intrinsic_call(expr, CONTRACT_CLAUSE_INTRINSIC),
            "`{CONTRACT_CLAUSE_INTRINSIC}()` outside the clause it heads"
        );
        self.walk_expr(expr);
    }
}
