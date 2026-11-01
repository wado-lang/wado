use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::call_args::CallArgs;
use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::Visibility;
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::logger::{Bail, ErrorSink};
use crate::module_source::ModuleSource;
use crate::name::{
    FunctionId, MODULE_INIT_FUNCTION, MODULES_INIT_FLAG, MODULES_INIT_FUNCTION,
    global_init_function, global_init_target,
};
use crate::synthesis::common::known_cold_path_stmt;
use crate::tir::{
    FunctionRef, GlobalInit, LocalFrame, TirBlock, TirExpr, TirExprKind, TirFunction, TirGlobal,
    TirStmt, TirStmtKind, is_constant_initializer,
};
use crate::tir_visitor::{TirRefVisitor, shift_locals};
use crate::token::Span;
use crate::type_table::TypeTable;

/// A global, by the module declaring it and its name: two modules may each
/// declare one of the same name.
type GlobalKey = (ModuleSource, String);

/// One global initializer, taken out of the `$init$` function reify put it in.
/// It lives only in `functions` until here, so no pass between reify and this
/// one can walk the functions and miss an initializer.
struct LazyInit {
    global: String,
    module_source: ModuleSource,
    /// What the global is assigned, in the frame `frame` describes.
    value: TirExpr,
    frame: LocalFrame,
}

/// The value a `$init$` function returns. Reify writes the one return, and
/// every pass since rewrote expressions within it.
fn returned_value(body: TirBlock) -> TirExpr {
    let mut stmts = body.stmts;
    assert_eq!(stmts.len(), 1, "a global initializer is a single statement");
    let Some(TirStmt {
        kind: TirStmtKind::Return { value: Some(value) },
        ..
    }) = stmts.pop()
    else {
        panic!("a global initializer returns its value");
    };
    value
}

/// Take each global initializer out of its own function and into the module's
/// `$initialize_module`, in dependency order. Must run before `boxing`, the
/// extracted code being able to contain the `&primitive` and closure
/// expressions boxing rewrites; the top-level aggregator calling each
/// `$initialize_module` is built later by [`build_initialize_modules`].
pub fn extract(flat: &mut FlatPackage, errors: &dyn ErrorSink) -> Result<(), Bail> {
    // Reify classified these, and nothing since could have changed the answer:
    // the typed IR folds no constant, and turns no literal into code.
    {
        let type_table = flat.type_table.borrow();
        for global in &flat.globals {
            assert!(
                global.init.is_deferred()
                    || is_constant_initializer(global.init.slot_expr(), &type_table),
                "a Direct global stays a Wasm constant: {}",
                global.name
            );
        }
    }

    let mut inits: Vec<LazyInit> = Vec::new();
    flat.functions.retain(|func_rc| {
        let mut func = func_rc.borrow_mut();
        let Some(global) = global_init_target(&func.name).map(str::to_string) else {
            return true;
        };
        inits.push(LazyInit {
            global,
            module_source: func.module_source.clone(),
            value: returned_value(func.body.take().expect("$init$ carries a body")),
            frame: func.take_frame(),
        });
        false
    });
    // In declaration order, which the topological sort keeps wherever no
    // dependency decides. The functions follow the order monomorphization
    // reached them in.
    let declared: IndexMap<(&ModuleSource, &str), usize> = flat
        .globals
        .iter()
        .enumerate()
        .map(|(i, global)| ((&global.module_source, global.name.as_str()), i))
        .collect();
    inits.sort_by_key(|init| declared[&(&init.module_source, init.global.as_str())]);
    let refs = global_refs_by_function(&flat.functions);
    for (module_source, global) in refs.values().flatten() {
        assert!(
            !flat.pruned.contains(&FunctionId::free(
                module_source,
                &global_init_function(global)
            )),
            "`{global}` is used, but its initializer was pruned as unreached"
        );
    }
    if inits.is_empty() {
        return Ok(());
    }

    // One sort over every module, so a cycle through a module boundary is
    // found like any other.
    let deps = init_dependencies(&inits, &refs);
    let order = topological_order(&deps)
        .map_err(|in_degree| report_init_cycle(&inits, &deps, &in_degree, errors))?;
    let modules = module_order(&inits, &deps, errors)?;

    let mut by_module: IndexMap<ModuleSource, Vec<LazyInit>> =
        modules.into_iter().map(|ms| (ms, Vec::new())).collect();
    let mut taken: Vec<Option<LazyInit>> = inits.into_iter().map(Some).collect();
    for i in order {
        let init = taken[i]
            .take()
            .expect("the order names each initializer once");
        by_module[&init.module_source].push(init);
    }
    // `build_initialize_modules` calls them in the order they are pushed here.
    let span = Span::new(0, 0, 1, 1);
    for (module_source, sorted_inits) in by_module {
        let init_func = build_module_init_function(module_source, sorted_inits, span);
        flat.functions.push(Rc::new(RefCell::new(init_func)));
    }
    Ok(())
}

/// Assign every initializer to its global, in the order given, under one frame.
fn build_module_init_function(
    module_source: ModuleSource,
    sorted_inits: Vec<LazyInit>,
    span: Span,
) -> TirFunction {
    let mut init_stmts: Vec<TirStmt> = Vec::new();
    let mut merged = LocalFrame::default();

    for init in sorted_inits {
        let LazyInit {
            global,
            module_source,
            mut value,
            frame,
        } = init;
        merged.absorb(frame, |offset| shift_locals(&mut value, offset));
        init_stmts.push(set_global(module_source, global, value, span));
    }

    TirFunction::synthesized(
        module_source,
        MODULE_INIT_FUNCTION.to_string(),
        TypeTable::UNIT,
        TirBlock {
            stmts: init_stmts,
            span,
        },
        merged,
        span,
    )
}

fn set_global(module_source: ModuleSource, name: String, value: TirExpr, span: Span) -> TirStmt {
    let global_set = TirExpr::new(
        TirExprKind::GlobalVarSet {
            module_source,
            name,
            value: Box::new(value),
        },
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(global_set), span)
}

/// The globals a function body reads or writes, and what it calls, in one walk.
#[derive(Default)]
struct BodyRefs {
    globals: IndexSet<GlobalKey>,
    callees: IndexSet<FunctionId>,
}

impl TirRefVisitor for BodyRefs {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // A write is as much a dependency as a read: the global's own
            // initializer, run later, would replace what was stored.
            TirExprKind::GlobalVarGet {
                name,
                module_source,
            }
            | TirExprKind::GlobalVarSet {
                name,
                module_source,
                ..
            } => {
                self.globals.insert((module_source.clone(), name.clone()));
            }
            TirExprKind::Call { func, .. } => {
                self.callees.insert(func.function_id());
            }
            // A function named as a value reaches a call this walk cannot see —
            // `apply(reader)` calls `reader` through a parameter, and a closure
            // body is a function of its own. Counting the mention keeps what it
            // reads on the mentioning side of the graph.
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => {
                self.callees.insert(FunctionId::free(module_source, name));
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Every global each function reads or writes, closed over the call graph.
///
/// An initializer's dependency is not always written in it: a call reads
/// globals the caller never names, and ordering on the references the caller
/// names alone leaves such a global still holding its placeholder when the
/// caller runs. Computed as a least fixpoint, so a recursive cycle contributes
/// each member's own references and terminates.
fn global_refs_by_function(
    functions: &[Rc<RefCell<TirFunction>>],
) -> IndexMap<FunctionId, IndexSet<GlobalKey>> {
    let mut refs: IndexMap<FunctionId, IndexSet<GlobalKey>> = IndexMap::default();
    let mut callees: IndexMap<FunctionId, IndexSet<FunctionId>> = IndexMap::default();
    for func_rc in functions {
        let func = func_rc.borrow();
        let Some(body) = func.body.as_ref() else {
            continue;
        };
        let mut scan = BodyRefs::default();
        scan.visit_block(body);
        let key = FunctionId::free(&func.module_source, &func.name);
        refs.entry(key.clone()).or_default().extend(scan.globals);
        callees.entry(key).or_default().extend(scan.callees);
    }

    loop {
        let mut grown: Vec<(FunctionId, IndexSet<GlobalKey>)> = Vec::new();
        for (name, called) in &callees {
            let known = &refs[name];
            let fresh: IndexSet<GlobalKey> = called
                .iter()
                .filter_map(|callee| refs.get(callee))
                .flatten()
                .filter(|global| !known.contains(*global))
                .cloned()
                .collect();
            if !fresh.is_empty() {
                grown.push((name.clone(), fresh));
            }
        }
        if grown.is_empty() {
            return refs;
        }
        for (name, fresh) in grown {
            refs[&name].extend(fresh);
        }
    }
}

/// What each initializer depends on, as indices into `inits`.
///
/// An initializer depends on every global it refers to, and every global a
/// function it calls or names refers to, on any path. The rule is
/// path-insensitive, as Go's is: a reference only some executions take orders the
/// initializer the same as one they all take, and a cycle among them is an
/// error even where no execution would close it.
fn init_dependencies(
    inits: &[LazyInit],
    refs_by_function: &IndexMap<FunctionId, IndexSet<GlobalKey>>,
) -> Vec<IndexSet<usize>> {
    let key_to_idx: IndexMap<GlobalKey, usize> = inits
        .iter()
        .enumerate()
        .map(|(i, init)| ((init.module_source.clone(), init.global.clone()), i))
        .collect();
    inits
        .iter()
        .map(|init| {
            let mut scan = BodyRefs::default();
            scan.visit_expr(&init.value);
            let via_calls = scan
                .callees
                .iter()
                .filter_map(|callee| refs_by_function.get(callee))
                .flatten();
            scan.globals
                .iter()
                .chain(via_calls)
                .filter_map(|key| key_to_idx.get(key).copied())
                .collect()
        })
        .collect()
}

/// Kahn's algorithm: each node after every one in its `deps`, ties in index
/// order. On a cycle, the in-degrees left, nonzero on every node it could not
/// place.
fn topological_order(deps: &[IndexSet<usize>]) -> Result<Vec<usize>, Vec<usize>> {
    let mut in_degree: Vec<usize> = deps.iter().map(IndexSet::len).collect();
    let mut queue: VecDeque<usize> = (0..deps.len()).filter(|&i| in_degree[i] == 0).collect();
    let mut sorted: Vec<usize> = Vec::with_capacity(deps.len());
    while let Some(node) = queue.pop_front() {
        sorted.push(node);
        for (dependent, dep_set) in deps.iter().enumerate() {
            if dep_set.contains(&node) {
                in_degree[dependent] -= 1;
                if in_degree[dependent] == 0 {
                    queue.push_back(dependent);
                }
            }
        }
    }
    if sorted.len() < deps.len() {
        return Err(in_degree);
    }
    Ok(sorted)
}

fn report_init_cycle(
    inits: &[LazyInit],
    deps: &[IndexSet<usize>],
    in_degree: &[usize],
    errors: &dyn ErrorSink,
) -> Bail {
    let cycle = blocking_cycle(deps, in_degree);
    let names: Vec<&str> = cycle.iter().map(|&i| inits[i].global.as_str()).collect();
    let LazyInit {
        module_source,
        value,
        ..
    } = &inits[cycle[0]];
    errors.fatal_in(
        module_source,
        Diagnostic {
            severity: Severity::Error,
            code: Code::CircularDependency,
            message: if let [name] = names[..] {
                format!(
                    "the initializer of `{name}` may use `{name}` before it has been given \
                     a value."
                )
            } else {
                format!(
                    "global initializers form a cycle: {}. Each may use a global another has \
                     not given a value yet, so none can go first.",
                    names.join(", ")
                )
            },
            span: Some(DiagnosticSpan::from_span(&value.span, None)),
        },
    )
}

/// One cycle among the nodes the sort left unplaced, in dependency order,
/// leading with the lowest index. Those include nodes that merely wait on a
/// cycle, so it follows unplaced dependencies until one repeats.
fn blocking_cycle(deps: &[IndexSet<usize>], in_degree: &[usize]) -> Vec<usize> {
    let mut path: Vec<usize> = Vec::new();
    let mut node = in_degree
        .iter()
        .position(|&d| d > 0)
        .expect("a node is unplaced");
    while !path.contains(&node) {
        path.push(node);
        node = *deps[node]
            .iter()
            .find(|&&dep| in_degree[dep] > 0)
            .expect("an unplaced node waits on another unplaced one");
    }
    let mut cycle = path.split_off(path.iter().position(|&n| n == node).expect("node repeats"));
    let lowest = cycle.iter().min().copied().expect("a cycle has a member");
    let first = cycle
        .iter()
        .position(|&n| n == lowest)
        .expect("lowest is a member");
    cycle.rotate_left(first);
    cycle
}

/// The order to call the modules' `$initialize_module` in, each after the
/// modules whose globals its initializers depend on.
///
/// A module's initializers run together, so two modules whose initializers
/// depend on each other's have no order, even where the initializers alone
/// would: `A1` needing `B1` and `B2` needing `A2` puts each module first.
fn module_order(
    inits: &[LazyInit],
    deps: &[IndexSet<usize>],
    errors: &dyn ErrorSink,
) -> Result<Vec<ModuleSource>, Bail> {
    let modules: IndexSet<&ModuleSource> = inits.iter().map(|init| &init.module_source).collect();
    let module_of = |i: usize| -> usize {
        modules
            .get_index_of(&inits[i].module_source)
            .expect("every initializer's module is listed")
    };
    // For each module dependency, one initializer edge that makes it.
    let mut witness: IndexMap<(usize, usize), (usize, usize)> = IndexMap::default();
    let mut module_deps: Vec<IndexSet<usize>> = vec![IndexSet::default(); modules.len()];
    for (i, dep_set) in deps.iter().enumerate() {
        for &dep in dep_set {
            let [from, to] = [module_of(i), module_of(dep)];
            if from != to {
                module_deps[from].insert(to);
                witness.entry((from, to)).or_insert((i, dep));
            }
        }
    }
    match topological_order(&module_deps) {
        Ok(order) => Ok(order.into_iter().map(|m| modules[m].clone()).collect()),
        Err(in_degree) => {
            let cycle = blocking_cycle(&module_deps, &in_degree);
            let edges: Vec<(usize, usize)> = cycle
                .iter()
                .zip(cycle.iter().cycle().skip(1))
                .map(|(&from, &to)| witness[&(from, to)])
                .collect();
            let first = &inits[edges[0].0];
            let edges: Vec<String> = edges
                .into_iter()
                .map(|(i, dep)| format!("`{}` needs `{}`", inits[i].global, inits[dep].global))
                .collect();
            Err(errors.fatal_in(
                &first.module_source,
                Diagnostic {
                    severity: Severity::Error,
                    code: Code::CircularDependency,
                    message: format!(
                        "the global initializers of these modules depend on each other: {}. \
                         A module's globals are initialized together, so none of these \
                         modules can go first.",
                        edges.join(", ")
                    ),
                    span: Some(DiagnosticSpan::from_span(&first.value.span, None)),
                },
            ))
        }
    }
}

/// Build the top-level aggregator calling every module's
/// `$initialize_module`, in the order [`extract`] pushed them, which is their
/// dependency order. Must run after [`extract`] has created them.
pub fn build_initialize_modules(flat: &mut FlatPackage) {
    let entry_source = flat.entry_module_source.clone();

    let mut modules_with_init: Vec<ModuleSource> = Vec::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if func.name == MODULE_INIT_FUNCTION {
            assert!(
                !modules_with_init.contains(&func.module_source),
                "extract builds one initializer per module"
            );
            modules_with_init.push(func.module_source.clone());
        }
    }

    if modules_with_init.is_empty() {
        return;
    }

    let span = Span::new(0, 0, 1, 1);

    flat.globals.push(TirGlobal {
        name: MODULES_INIT_FLAG.to_string(),
        ty: TypeTable::BOOL,
        init: GlobalInit::Direct(TirExpr::new(
            TirExprKind::BoolLiteral(false),
            TypeTable::BOOL,
            span,
        )),
        param: None,
        wado_mutable: true,
        visibility: Visibility::Private,
        module_source: entry_source.clone(),
        span,
    });

    let mut init_stmts: Vec<TirStmt> = Vec::new();

    let flag_check = TirExpr::new(
        TirExprKind::GlobalVarGet {
            module_source: entry_source.clone(),
            name: MODULES_INIT_FLAG.to_string(),
        },
        TypeTable::BOOL,
        span,
    );
    let early_return_stmt = TirStmt::new(TirStmtKind::Return { value: None }, span);
    let early_return_block = TirBlock {
        stmts: vec![early_return_stmt],
        span,
    };
    let if_already_init = TirStmt::new(
        TirStmtKind::If {
            condition: flag_check,
            then_block: early_return_block,
            else_block: None,
        },
        span,
    );
    init_stmts.push(if_already_init);

    // The fall-through below the guard runs once per program: mark it cold so
    // the guard is hinted likely-taken (`hint_guard_fall_through`), including
    // the copies inlined into each export entry, and the inliner excludes the
    // one-shot init calls from its cost estimate.
    init_stmts.push(known_cold_path_stmt(span));

    for module_source in &modules_with_init {
        let call = TirExpr::new(
            TirExprKind::Call {
                func: Box::new(FunctionRef {
                    module_source: module_source.clone(),
                    name: MODULE_INIT_FUNCTION.to_string(),
                    template: None,
                    monomorph_info: None,
                    method_info: None,
                }),
                type_args: Vec::new(),
                args: CallArgs::free(Vec::new()),
            },
            TypeTable::UNIT,
            span,
        );
        init_stmts.push(TirStmt::new(TirStmtKind::Expr(call), span));
    }

    let initialized = TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span);
    init_stmts.push(set_global(
        entry_source.clone(),
        MODULES_INIT_FLAG.to_string(),
        initialized,
        span,
    ));

    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    let init_modules_func = TirFunction::synthesized(
        entry_source.clone(),
        MODULES_INIT_FUNCTION.to_string(),
        TypeTable::UNIT,
        init_body,
        LocalFrame::default(),
        span,
    );
    flat.functions
        .push(Rc::new(RefCell::new(init_modules_func)));

    let init_call = TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: entry_source.clone(),
                name: MODULES_INIT_FUNCTION.to_string(),
                template: None,
                monomorph_info: None,
                method_info: None,
            }),
            type_args: Vec::new(),
            args: CallArgs::free(Vec::new()),
        },
        TypeTable::UNIT,
        span,
    );
    let init_call_stmt = TirStmt::new(TirStmtKind::Expr(init_call), span);

    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        if func.module_source != entry_source {
            continue;
        }
        if func.is_export
            && let Some(ref mut body) = func.body
        {
            body.stmts.insert(0, init_call_stmt.clone());
        }
    }
}
