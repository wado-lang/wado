//! Check each call's contract at the call, before the plan.
//!
//! A function `F` writing `contract` clauses reifies as two: its twin
//! `F$unchecked`, holding the body, and `F` itself, holding only the clauses,
//! each as `if $contract_clause() { … }`, and a call to the twin. Here each
//! call to `F` becomes a call to the twin carrying a copy of the clauses,
//! reading the call's arguments, so a failed clause reports the call's position as
//! `$contract_site`. A call `unchecked` marks already names the twin, unless
//! `-f contract-checks` turned it back (see [`check_marked_calls`]). `F` itself
//! is left to the calls that reach it as a value, which check at the clause.

use crate::compiler_item::CompilerItems;
use crate::defs::DefId;
use crate::elaborator::trait_env::TraitEnv;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::place::{
    is_source_place, place_root, source_place_subscripts_mut,
};
use crate::lower::translate::{convert_function_ref, tir_function_key};
use crate::name::{
    CONTRACT_CLAUSE_INTRINSIC, CONTRACT_SITE_INTRINSIC, FunctionId, LocalMethodName, RefKind,
    minted_name,
};
use crate::tir::{
    CallArg, LetStorage, TemplateId, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal,
    TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable, assign_stmt,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor, remap_locals};

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
        let mut expander = Expander {
            contracts: &contracts,
            type_table: &type_table,
            items,
            trait_env: &flat.trait_env,
            func: &mut func,
            site: None,
            expanding: Vec::new(),
            bound: IndexSet::default(),
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
    /// The parameters the clauses write. Each is the callee's own copy, so an
    /// argument for one is always bound, never read from the caller's place.
    written: IndexSet<u32>,
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
        let clauses = &body.stmts[..clauses];
        contracts.insert(
            tir_function_key(&func),
            Contract {
                clauses: clauses.to_vec(),
                forward,
                params: func.params.iter().map(|p| p.type_id).collect(),
                written: written_params(clauses, func.params.len() as u32),
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

/// The parameters `clauses` assign or borrow mutably.
fn written_params(clauses: &[TirStmt], params: u32) -> IndexSet<u32> {
    struct Writes {
        params: u32,
        written: IndexSet<u32>,
    }
    impl TirRefVisitor for Writes {
        fn visit_expr(&mut self, expr: &TirExpr) {
            let place = match &expr.kind {
                TirExprKind::Assign { target, .. } => Some(&**target),
                TirExprKind::Unary {
                    op: TirUnaryOp::MutRef,
                    expr,
                } => Some(&**expr),
                _ => None,
            };
            if let Some(root) = place.and_then(place_root)
                && root < self.params
            {
                self.written.insert(root);
            }
            self.walk_expr(expr);
        }
    }
    let mut writes = Writes {
        params,
        written: IndexSet::default(),
    };
    for clause in clauses {
        writes.visit_stmt(clause);
    }
    writes.written
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
    trait_env: &'a TraitEnv,
    func: &'a mut TirFunction,
    /// The position a clause copied here reports: the call's, where the copy
    /// reaches another call through a clause of its own.
    site: Option<String>,
    /// The functions whose clauses are being copied, outermost first. A clause
    /// calling its own function back is left to call it.
    expanding: Vec<FunctionId>,
    /// The locals [`Self::bind`] made.
    bound: IndexSet<u32>,
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
    /// Rewrite `call` to a call to the twin whose last argument checks the
    /// callee's clauses once every argument is evaluated, so the call stays
    /// the place an index call names. Where the last parameter borrows, or no
    /// twin follows a trait's clauses, the clauses and then the call form a
    /// block instead, its arguments bound ahead.
    fn expand(&mut self, call: &mut TirExpr, key: FunctionId) {
        let contracts = self.contracts;
        let contract = &contracts[&key];
        let span = call.span;
        let site = self.site.clone().unwrap_or_else(|| {
            let module = self
                .trait_env
                .module_of_span(span, &self.func.module_source);
            format!("{module}:{}", span.line)
        });
        let TirExprKind::Call { args, .. } = &mut call.kind else {
            unreachable!("only a call expands");
        };
        let in_last_arg = contract.forward.is_some()
            && contract
                .params
                .last()
                .is_some_and(|&param| RefKind::from_resolved(self.type_table.get(param)).is_none());
        // The last argument's own bindings are `let`s in its block, which the
        // clauses after them share; only an earlier argument's are assigned.
        let mut stmts = Vec::new();
        let last_index = args.len().saturating_sub(1);
        let substitutes: Vec<TirExpr> = args
            .iter_mut()
            .zip(&contract.params)
            .enumerate()
            .map(|(i, (arg, &param_type))| {
                if !in_last_arg {
                    return self.bind_passed(arg, param_type, &mut stmts);
                }
                let sink = if i < last_index {
                    Sink::InPlace
                } else {
                    Sink::Ahead(&mut stmts)
                };
                if contract.written.contains(&(i as u32)) {
                    return self.bind(&mut arg.expr, sink);
                }
                self.bind_ahead(arg, param_type, sink)
            })
            .collect();

        // The callee's own locals follow the caller's; its parameters, which
        // the arguments stand in for, are numbered past them only to be
        // substituted away.
        let params = contract.params.len() as u32;
        let base = self.func.locals.len() as u32;
        let own = contract.locals.len() as u32 - params;
        let renumber = |index: u32| {
            if index < params {
                base + own + index
            } else {
                base + index - params
            }
        };
        self.func
            .locals
            .extend(contract.locals[params as usize..].iter().cloned());
        self.func.local_count = self.func.locals.len() as u32;
        for &index in &contract.address_taken {
            if index >= params {
                self.func.address_taken_locals.insert(renumber(index));
            } else if let TirExprKind::Local { index: arg, .. } = substitutes[index as usize].kind {
                self.func.address_taken_locals.insert(arg);
            }
        }
        let mut clauses = TirExpr::new(
            TirExprKind::Block(TirBlock::new(contract.clauses.clone(), span)),
            TypeTable::UNIT,
            span,
        );
        remap_locals(&mut clauses, renumber);
        Substitute {
            offset: base + own,
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

        let Some(forward) = &contract.forward else {
            stmts.extend(copied.stmts);
            stmts.push(TirStmt::new(
                TirStmtKind::Expr(TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span)),
                span,
            ));
            call.kind = TirExprKind::Block(TirBlock::new(stmts, span));
            return;
        };
        let TirExprKind::Call {
            func: twin,
            type_args,
            ..
        } = &forward.kind
        else {
            unreachable!("a function forwards its call to its twin");
        };
        let TirExprKind::Call {
            func,
            type_args: call_type_args,
            args,
        } = &mut call.kind
        else {
            unreachable!("only a call expands");
        };
        func.clone_from(twin);
        call_type_args.clone_from(type_args);
        if !in_last_arg {
            let type_id = call.type_id;
            let forwarded = take(call);
            stmts.extend(copied.stmts);
            stmts.push(TirStmt::new(TirStmtKind::Expr(forwarded), span));
            *call = TirExpr::new(
                TirExprKind::Block(TirBlock::new(stmts, span)),
                type_id,
                span,
            );
            return;
        }
        let last = args
            .iter_mut()
            .last()
            .expect("the clauses ride the last argument");
        let value = take(&mut last.expr);
        let (type_id, value_span) = (value.type_id, value.span);
        stmts.extend(copied.stmts);
        stmts.push(TirStmt::new(TirStmtKind::Expr(value), value_span));
        last.expr = TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, value_span)),
            type_id,
            value_span,
        );
    }

    /// What the clauses read for the parameter `arg` passes, evaluating it
    /// once: a place a reference parameter takes stays in the call, with its
    /// subscripts bound, as the callee reads through it only once it runs; any
    /// other value is bound and read from there. A place read by value stays
    /// only as the last argument: nothing runs between it and the clauses to
    /// change what it holds.
    fn bind_ahead(&mut self, arg: &mut CallArg, param_type: TypeId, mut sink: Sink<'_>) -> TirExpr {
        let later_args_run = matches!(sink, Sink::InPlace);
        let borrowed_place = matches!(
            &arg.expr.kind,
            TirExprKind::Unary { op: TirUnaryOp::Ref | TirUnaryOp::MutRef, expr } if is_source_place(expr, self.items)
        );
        let place = if borrowed_place {
            let TirExprKind::Unary { expr, .. } = &mut arg.expr.kind else {
                unreachable!("matched above");
            };
            &mut **expr
        } else if is_source_place(&arg.expr, self.items)
            && (RefKind::from_resolved(self.type_table.get(param_type)).is_some()
                || !later_args_run)
        {
            &mut arg.expr
        } else {
            return self.bind(&mut arg.expr, sink.reborrow());
        };
        let items = self.items;
        for subscript in source_place_subscripts_mut(place, items) {
            if later_args_run || !matches!(subscript.kind, TirExprKind::Local { .. }) {
                self.bind(subscript, sink.reborrow());
            }
        }
        let read = self.read_of(&arg.expr);
        match self.receiver_borrow(&read, param_type) {
            Some(op) => borrow_of(read, op, param_type),
            None => read,
        }
    }

    /// Bind the value `arg` passes as a `let` in `stmts`, a borrow included,
    /// and pass the bound local instead: the call follows every `let`, so
    /// anything it evaluated itself would run after the later arguments.
    fn bind_passed(
        &mut self,
        arg: &mut CallArg,
        param_type: TypeId,
        stmts: &mut Vec<TirStmt>,
    ) -> TirExpr {
        if let Some(op) = self.receiver_borrow(&arg.expr, param_type) {
            arg.expr = borrow_of(take(&mut arg.expr), op, param_type);
        }
        self.bind(&mut arg.expr, Sink::Ahead(stmts))
    }

    /// The borrow a receiver passed as its place takes, where the parameter
    /// borrows it.
    fn receiver_borrow(&self, arg: &TirExpr, param_type: TypeId) -> Option<TirUnaryOp> {
        let kind = RefKind::from_resolved(self.type_table.get(param_type))?;
        if RefKind::from_resolved(self.type_table.get(arg.type_id)).is_some() {
            return None;
        }
        Some(match kind {
            RefKind::Shared => TirUnaryOp::Ref,
            RefKind::Mut => TirUnaryOp::MutRef,
        })
    }

    /// Bind what `slot` evaluates to a new local of the function, where `sink`
    /// says, and answer the read of it. In place it is assigned where it is
    /// evaluated, as a power-assert capture is: a `let` there would scope the
    /// local to that argument, out of reach of the clauses after it.
    fn bind(&mut self, slot: &mut TirExpr, sink: Sink<'_>) -> TirExpr {
        let index = self.func.locals.len() as u32;
        // Each binding joins `bound`, so its size is the serial.
        let name = minted_name("contract_arg", self.bound.len());
        let (type_id, span) = (slot.type_id, slot.span);
        let in_place = matches!(sink, Sink::InPlace);
        self.func.locals.push(TirLocal {
            name: name.clone(),
            type_id,
            is_mut: in_place,
            span,
        });
        self.func.local_count = self.func.locals.len() as u32;
        self.bound.insert(index);
        let read = TirExpr::new(
            TirExprKind::Local {
                index,
                name: name.clone(),
            },
            type_id,
            span,
        );
        let value = take(slot);
        *slot = match sink {
            Sink::Ahead(stmts) => {
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name,
                        local_index: index,
                        is_mut: false,
                        type_id,
                        value,
                        storage: LetStorage::Planned,
                    },
                    span,
                ));
                read.clone()
            }
            Sink::InPlace => TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![
                        assign_stmt(read.clone(), value, span),
                        TirStmt::new(TirStmtKind::Expr(read.clone()), span),
                    ],
                    span,
                )),
                type_id,
                span,
            ),
        };
        read
    }

    /// Whether `stmts` is a binding [`Self::bind`] made in place.
    fn binds(&self, stmts: &[TirStmt]) -> bool {
        let [assign, read] = stmts else {
            return false;
        };
        let (
            TirStmtKind::Expr(TirExpr {
                kind: TirExprKind::Assign { target, .. },
                ..
            }),
            TirStmtKind::Expr(TirExpr {
                kind: TirExprKind::Local { index, .. },
                ..
            }),
        ) = (&assign.kind, &read.kind)
        else {
            return false;
        };
        matches!(target.kind, TirExprKind::Local { index: assigned, .. } if assigned == *index)
            && self.bound.contains(index)
    }

    /// `expr` read again: each binding made in place in it is its local.
    fn read_of(&self, expr: &TirExpr) -> TirExpr {
        struct Reread<'a>(&'a Expander<'a>);
        impl TirMutVisitor for Reread<'_> {
            fn visit_expr(&mut self, expr: &mut TirExpr) {
                if let TirExprKind::Block(block) = &mut expr.kind
                    && self.0.binds(&block.stmts)
                {
                    let read = block.stmts.pop().expect("a binding ends in its read");
                    let TirStmtKind::Expr(read) = read.kind else {
                        unreachable!("matched by `binds`");
                    };
                    *expr = read;
                    return;
                }
                self.walk_expr(expr);
            }
        }
        let mut read = expr.clone();
        Reread(self).visit_expr(&mut read);
        read
    }
}

/// Where [`Expander::bind`] puts a binding: ahead of the call, or where the
/// value is evaluated.
enum Sink<'s> {
    Ahead(&'s mut Vec<TirStmt>),
    InPlace,
}

impl Sink<'_> {
    fn reborrow(&mut self) -> Sink<'_> {
        match self {
            Sink::Ahead(stmts) => Sink::Ahead(stmts),
            Sink::InPlace => Sink::InPlace,
        }
    }
}

fn borrow_of(place: TirExpr, op: TirUnaryOp, type_id: TypeId) -> TirExpr {
    let span = place.span;
    TirExpr::new(
        TirExprKind::Unary {
            op,
            expr: Box::new(place),
        },
        type_id,
        span,
    )
}

/// Read each parameter of a copied clause as the call's argument, and report
/// the call's position.
struct Substitute<'a> {
    /// Where the callee's parameters are numbered among the caller's locals.
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
